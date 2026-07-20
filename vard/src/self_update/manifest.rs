//! cargo-dist `dist-manifest.json` parsing and fetch.
//!
//! The update manifest *is* the artifact cargo-dist already publishes on every
//! GitHub release (ADR 0017): TLS + the sha256 checksums inside it are the trust
//! root, with no bespoke signed manifest. Only the fields consumed here are
//! modeled; serde ignores everything else, so a manifest from a newer cargo-dist
//! still parses.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use serde::Deserialize;

/// One retry with this backoff on a transient (transport) manifest fetch error.
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// The parsed subset of `dist-manifest.json`.
#[derive(Debug, Deserialize)]
pub(crate) struct DistManifest {
    /// The release tag the manifest announces (`v0.1.0`-style).
    pub announcement_tag: String,
    /// Every published artifact, keyed by asset name.
    pub artifacts: BTreeMap<String, Artifact>,
}

impl DistManifest {
    /// The announced version with any leading `v` stripped (`v0.1.0` → `0.1.0`),
    /// so it compares directly against `CARGO_PKG_VERSION`.
    pub(crate) fn announcement_version(&self) -> &str {
        self.announcement_tag
            .strip_prefix('v')
            .unwrap_or(&self.announcement_tag)
    }
}

/// One artifact entry: enough to match a target triple to its downloadable
/// tarball and the sha256 to verify it against.
#[derive(Debug, Deserialize)]
pub(crate) struct Artifact {
    /// The asset filename (`vard-<triple>.tar.xz`).
    pub name: String,
    /// The artifact kind; the per-target binary tarballs are `executable-zip`.
    pub kind: String,
    /// The target triples this artifact serves.
    #[serde(default)]
    pub target_triples: Vec<String>,
    /// The artifact's checksums, keyed by algorithm (`sha256`).
    #[serde(default)]
    pub checksums: BTreeMap<String, String>,
}

/// Fetches and parses a `dist-manifest.json` from `url`.
///
/// One retry with [`RETRY_BACKOFF`] on a transport error; a 4xx/5xx is a hard
/// error carrying the status (so a 404 on a pinned tag is recognizable).
pub(crate) fn fetch(url: &str) -> Result<DistManifest, String> {
    let body = fetch_body(url)?;
    serde_json::from_slice::<DistManifest>(&body)
        .map_err(|e| format!("parsing dist-manifest.json from {url}: {e}"))
}

fn fetch_body(url: &str) -> Result<Vec<u8>, String> {
    let mut last_err: Option<String> = None;
    for attempt in 0..2 {
        match super::http::agent().get(url).call() {
            Ok(response) => {
                let mut buf = Vec::new();
                response
                    .into_reader()
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("reading the manifest body from {url}: {e}"))?;
                return Ok(buf);
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(format!("fetching {url} returned HTTP {code}"));
            }
            Err(ureq::Error::Transport(t)) => {
                last_err = Some(format!("fetching {url}: {t}"));
                if attempt == 0 {
                    std::thread::sleep(RETRY_BACKOFF);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| format!("fetching {url} failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MANIFEST: &str = r#"{
        "dist_version": "0.32.0",
        "announcement_tag": "v0.2.0",
        "announcement_title": "Version 0.2.0",
        "artifacts": {
            "vard-aarch64-apple-darwin.tar.xz": {
                "name": "vard-aarch64-apple-darwin.tar.xz",
                "kind": "executable-zip",
                "target_triples": ["aarch64-apple-darwin"],
                "checksums": { "sha256": "abc123def456" }
            },
            "vard-aarch64-apple-darwin.tar.xz.sha256": {
                "name": "vard-aarch64-apple-darwin.tar.xz.sha256",
                "kind": "checksum"
            }
        }
    }"#;

    #[test]
    fn parses_manifest_and_strips_v_prefix() {
        let m: DistManifest = serde_json::from_str(SAMPLE_MANIFEST).unwrap();
        assert_eq!(m.announcement_tag, "v0.2.0");
        assert_eq!(m.announcement_version(), "0.2.0");
        assert_eq!(m.artifacts.len(), 2);
    }

    #[test]
    fn fetch_returns_parsed_manifest() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/dist-manifest.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(SAMPLE_MANIFEST)
            .create();
        let url = format!("{}/dist-manifest.json", server.url());
        let manifest = fetch(&url).unwrap();
        assert_eq!(manifest.announcement_tag, "v0.2.0");
        mock.assert();
    }

    #[test]
    fn fetch_surfaces_404_status() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/missing.json")
            .with_status(404)
            .create();
        let url = format!("{}/missing.json", server.url());
        let err = fetch(&url).unwrap_err();
        assert!(err.contains("HTTP 404"), "got: {err}");
    }

    #[test]
    fn fetch_errors_on_malformed_body() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/bad.json")
            .with_status(200)
            .with_body("{ not json")
            .create();
        let url = format!("{}/bad.json", server.url());
        let err = fetch(&url).unwrap_err();
        assert!(err.contains("parsing"), "got: {err}");
    }
}
