//! `vard self-update` — refresh the running `vard` binary from its GitHub
//! release (VRD-25, phase 1).
//!
//! The flow, modeled on norn's proven self-update client and shaped by ADR 0017:
//! gate on the cargo-dist install receipt, fetch cargo-dist's own
//! `dist-manifest.json` (latest, or a pinned tag), resolve the target version
//! and this platform's artifact, download and verify its sha256 **before** any
//! extraction, stage the new binary as a sibling of the install path, and
//! replace it with one atomic `rename(2)`. `--version` pins any version
//! (downgrades allowed) and *is* the rollback; `--dry-run` resolves and prints
//! the plan without changing anything.
//!
//! Trust root is TLS + the manifest's sha256 — no signed manifest (ADR 0017).
//! This phase does not restart the daemon or verify a post-update heartbeat
//! (phase 2): a swap tells the user to restart the daemon and stops there.

mod download;
mod manifest;
mod receipt;
mod render;
mod resolve;
mod swap;

use std::path::PathBuf;
use std::process::ExitCode;

use crate::cli::{ColorWhen, OutputFormat, SelfUpdateArgs};
use crate::command::{self, CmdError, CmdResult, OutCtx};
use crate::paths;

use self::resolve::Action;

/// The production releases endpoint. Injectable via [`RunConfig::releases_url`]
/// so tests point it at a mock server and never touch the real network.
const RELEASES_URL: &str = "https://github.com/dbtlr/vard/releases";

/// The resolved plan (and, after a real run, the applied outcome) for one
/// `vard self-update` invocation. Rendered by [`render`]; the asset fields are
/// present only when an update is in play.
#[derive(Debug)]
pub(crate) struct SelfUpdateReport {
    /// Whether the true latest release differs from the current version.
    pub(crate) update_available: bool,
    /// The running binary's version (`CARGO_PKG_VERSION`).
    pub(crate) current_version: String,
    /// The true latest published version (even when a lower version is pinned).
    pub(crate) latest_version: String,
    /// The version this run targets — the pin, or the latest.
    pub(crate) target_version: String,
    /// The compile-time target triple the artifact was selected for.
    pub(crate) target_triple: String,
    /// The path of the binary that was (or would be) replaced.
    pub(crate) install_path: String,
    /// The resolved artifact URL, when an update is in play.
    pub(crate) asset_url: Option<String>,
    /// The artifact's manifest sha256, when an update is in play.
    pub(crate) asset_sha256: Option<String>,
    /// Whether this was a dry run.
    pub(crate) dry_run: bool,
    /// What the run resolved to do.
    pub(crate) action: Action,
}

/// Everything one run needs, resolved once. Production values come from
/// [`from_env`](RunConfig::from_env); tests construct it directly, injecting a
/// tempdir receipt, a tempfile install path, a mock releases URL, and a fixed
/// triple — so no test touches the real network or the real home directory.
pub(crate) struct RunConfig {
    /// Resolve and print the plan without downloading or swapping anything.
    pub(crate) dry_run: bool,
    /// The pinned target version, if `--version` was given (downgrades allowed).
    pub(crate) pinned_version: Option<String>,
    /// The install-receipt path (the gate). Production resolves it through
    /// [`paths::self_update_receipt`]; tests inject a tempdir path.
    pub(crate) receipt_path: PathBuf,
    /// The running binary's path — the swap target. Production is
    /// `current_exe()`; tests inject a tempfile.
    pub(crate) install_path: PathBuf,
    /// The releases URL prefix. Production is [`RELEASES_URL`]; tests inject a
    /// mock server URL.
    pub(crate) releases_url: String,
    /// The compile-time target triple; `None` on a target with no release
    /// artifact.
    pub(crate) target_triple: Option<String>,
    /// The current binary's version.
    pub(crate) current_version: String,
}

impl RunConfig {
    /// Resolves the production configuration for one invocation.
    fn from_env(args: &SelfUpdateArgs) -> Result<RunConfig, CmdError> {
        let receipt_path =
            paths::self_update_receipt().map_err(|e| CmdError::err(e.to_string()))?;
        let install_path = std::env::current_exe()
            .map_err(|e| CmdError::err(format!("resolving the running vard binary: {e}")))?;
        Ok(RunConfig {
            dry_run: args.dry_run,
            pinned_version: args.version.clone(),
            receipt_path,
            install_path,
            releases_url: RELEASES_URL.to_string(),
            target_triple: resolve::TARGET_TRIPLE.map(str::to_string),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }
}

/// Entry point for `vard self-update`.
pub(crate) fn run(
    args: SelfUpdateArgs,
    color: ColorWhen,
    format: Option<OutputFormat>,
) -> ExitCode {
    let out = OutCtx::resolve(color, format);
    command::finish(run_inner(&out, &args))
}

fn run_inner(out: &OutCtx, args: &SelfUpdateArgs) -> CmdResult {
    let cfg = RunConfig::from_env(args)?;
    let report = perform(&cfg)?;
    render::render(out, &report)
}

/// Runs the update flow and returns the outcome report, applying the swap on a
/// real (non-dry) run. Precondition blocks — no receipt, no artifact for this
/// target — are the attention class (exit 1): the updater ran but will not
/// proceed, and the user resolves it elsewhere. Operational failures — a
/// nonexistent pinned tag, a network, checksum, extraction, or swap error — are
/// the error class (exit 2).
fn perform(cfg: &RunConfig) -> Result<SelfUpdateReport, CmdError> {
    // 1. Receipt gate. Absent or unreadable blocks with a pointer back to the
    //    installer / the user's original package manager.
    match receipt::load(&cfg.receipt_path) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(CmdError::attention(block_message(false))),
        Err(_) => return Err(CmdError::attention(block_message(true))),
    }

    // 2. Target triple. A build with no release artifact cannot self-update.
    let triple = cfg
        .target_triple
        .as_deref()
        .ok_or_else(|| CmdError::attention(unknown_target_message()))?;

    // 3+4. Fetch the manifest(s). Pinned mode also fetches the true latest so the
    //      report's `latest_version` is honest; a 404 on the pinned tag means it
    //      does not exist on GitHub — a bad-input error, not a transient fail.
    let (manifest, latest_version) = if let Some(pin) = cfg.pinned_version.as_deref() {
        let pinned_url = format!("{}/download/v{pin}/dist-manifest.json", cfg.releases_url);
        let pinned = manifest::fetch(&pinned_url).map_err(|e| {
            if e.contains("HTTP 404") {
                CmdError::err(format!(
                    "release v{pin} does not exist on GitHub (no dist-manifest.json at the pinned tag)"
                ))
            } else {
                CmdError::err(e)
            }
        })?;
        let latest_url = format!("{}/latest/download/dist-manifest.json", cfg.releases_url);
        let latest = manifest::fetch(&latest_url).map_err(CmdError::err)?;
        let latest_version = latest.announcement_version().to_string();
        (pinned, latest_version)
    } else {
        let latest_url = format!("{}/latest/download/dist-manifest.json", cfg.releases_url);
        let latest = manifest::fetch(&latest_url).map_err(CmdError::err)?;
        let latest_version = latest.announcement_version().to_string();
        (latest, latest_version)
    };

    let target_version = cfg
        .pinned_version
        .clone()
        .unwrap_or_else(|| latest_version.clone());
    let same_version = target_version == cfg.current_version;
    let action = resolve::determine_action(cfg.dry_run, &target_version, &cfg.current_version);

    // 5. Select this platform's artifact. Only needed when moving versions; a
    //    no-op never touches it, so an up-to-date run on a niche build still
    //    reports cleanly.
    let (asset_url, asset_sha256) = if same_version {
        (None, None)
    } else {
        match resolve::select_asset(&manifest, triple) {
            Some((name, sha)) => (
                Some(format!(
                    "{}/download/v{target_version}/{name}",
                    cfg.releases_url
                )),
                Some(sha.to_string()),
            ),
            None => {
                return Err(CmdError::attention(format!(
                    "release v{target_version} has no artifact for your target ({triple}); \
                     update via the package manager you originally used instead"
                )));
            }
        }
    };

    let report = SelfUpdateReport {
        update_available: latest_version != cfg.current_version,
        current_version: cfg.current_version.clone(),
        latest_version,
        target_version: target_version.clone(),
        target_triple: triple.to_string(),
        install_path: cfg.install_path.display().to_string(),
        asset_url: asset_url.clone(),
        asset_sha256: asset_sha256.clone(),
        dry_run: cfg.dry_run,
        action,
    };

    // 6. Apply, on a real run that changes the version. Download → verify sha256
    //    (BEFORE extraction) → extract → atomic swap. On any failure the current
    //    binary is untouched: nothing is swapped until the staged binary exists
    //    and its archive verified.
    if action == Action::Updated {
        let url = asset_url.as_ref().expect("Updated implies an asset URL");
        let sha = asset_sha256
            .as_ref()
            .expect("Updated implies an asset sha256");
        apply(cfg, &target_version, url, sha)?;
    }

    Ok(report)
}

/// Downloads, verifies, extracts, and atomically swaps in the new binary. The
/// staged archive and binary are siblings of the install path (same filesystem)
/// and are cleaned up on failure so a checksum mismatch leaves nothing behind.
fn apply(cfg: &RunConfig, target_version: &str, url: &str, sha: &str) -> Result<(), CmdError> {
    let archive =
        download::sibling_temp_path(&cfg.install_path, &format!("{target_version}.tar.xz"));
    download::download_to(url, &archive).map_err(CmdError::err)?;

    if let Err(e) = download::verify_sha256(&archive, sha) {
        let _ = std::fs::remove_file(&archive);
        return Err(CmdError::err(e));
    }

    let staged = download::sibling_temp_path(&cfg.install_path, &format!("{target_version}.bin"));
    if let Err(e) = download::extract_binary(&archive, &staged) {
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_file(&staged);
        return Err(CmdError::err(e));
    }
    let _ = std::fs::remove_file(&archive);

    if let Err(e) = swap::swap(&staged, &cfg.install_path) {
        let _ = std::fs::remove_file(&staged);
        return Err(CmdError::err(e));
    }
    Ok(())
}

/// The receipt-gate block message: distinct lead for an absent vs. an unreadable
/// receipt, both pointing back at the installer or the user's package manager.
fn block_message(unparseable: bool) -> String {
    let lead = if unparseable {
        "vard self-update found an install receipt it could not read."
    } else {
        "vard self-update only works for installs from the official vard installer, and this \
         binary has no install receipt."
    };
    format!(
        "{lead}\nTo update, either:\n  \
         • re-run the installer:\n      \
         curl --proto '=https' --tlsv1.2 -LsSf \
         https://github.com/dbtlr/vard/releases/latest/download/vard-installer.sh | sh\n  \
         • or update via the package manager you originally used (cargo, Homebrew, etc.)"
    )
}

/// The message for a build whose target has no published release artifact.
fn unknown_target_message() -> String {
    "vard was built for a target the official release does not publish a binary for, so \
     self-update cannot swap it — update via the package manager you originally used instead"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// Lowercase-hex sha256 of `bytes`.
    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Builds an in-memory `.tar.xz` whose single binary entry is `vard`, with
    /// the given `contents`, mirroring cargo-dist's `vard-<triple>/vard` layout.
    fn tarball_bytes(contents: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let xz = xz2::write::XzEncoder::new(&mut buf, 6);
            let mut builder = tar::Builder::new(xz);
            let mut header = tar::Header::new_gnu();
            header.set_path("vard-aarch64-apple-darwin/vard").unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, contents).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    /// A manifest body announcing `tag` with one artifact for
    /// `aarch64-apple-darwin` carrying `sha`.
    fn manifest_body(tag: &str, sha: &str) -> String {
        format!(
            r#"{{
                "dist_version": "0.32.0",
                "announcement_tag": "{tag}",
                "announcement_title": "{tag}",
                "artifacts": {{
                    "vard-aarch64-apple-darwin.tar.xz": {{
                        "name": "vard-aarch64-apple-darwin.tar.xz",
                        "kind": "executable-zip",
                        "target_triples": ["aarch64-apple-darwin"],
                        "checksums": {{ "sha256": "{sha}" }}
                    }}
                }}
            }}"#
        )
    }

    /// Writes a minimal valid receipt into `dir` and returns its path.
    fn write_receipt(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("vard-receipt.json");
        std::fs::write(
            &path,
            r#"{"app_name":"vard","version":"0.1.0","install_prefix":"/x"}"#,
        )
        .unwrap();
        path
    }

    fn base_config(tmp: &std::path::Path, url: String) -> RunConfig {
        RunConfig {
            dry_run: false,
            pinned_version: None,
            receipt_path: write_receipt(tmp),
            install_path: tmp.join("vard"),
            releases_url: url,
            target_triple: Some("aarch64-apple-darwin".to_string()),
            current_version: "0.1.0".to_string(),
        }
    }

    #[test]
    fn happy_path_applies_the_update_and_replaces_the_binary() {
        let binary = b"the new vard binary bytes";
        let tarball = tarball_bytes(binary);
        let sha = sha256_hex(&tarball);

        let mut server = mockito::Server::new();
        let _m1 = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(manifest_body("v0.2.0", &sha))
            .create();
        let _m2 = server
            .mock("GET", "/download/v0.2.0/vard-aarch64-apple-darwin.tar.xz")
            .with_status(200)
            .with_body(tarball)
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path(), server.url());
        std::fs::write(&cfg.install_path, b"OLD BINARY").unwrap();
        cfg.current_version = "0.1.0".to_string();

        let report = perform(&cfg).unwrap();
        assert_eq!(report.action, Action::Updated);
        assert_eq!(report.target_version, "0.2.0");
        assert!(report.update_available);
        assert_eq!(std::fs::read(&cfg.install_path).unwrap(), binary);
        // No staging leftovers.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("self-update"))
            .collect();
        assert!(leftovers.is_empty(), "staging leftovers: {leftovers:?}");
    }

    #[test]
    fn up_to_date_is_a_clean_no_op() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(manifest_body("v0.1.0", "unused"))
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path(), server.url());
        std::fs::write(&cfg.install_path, b"CURRENT").unwrap();
        cfg.current_version = "0.1.0".to_string();

        let report = perform(&cfg).unwrap();
        assert_eq!(report.action, Action::NoOp);
        assert!(!report.update_available);
        assert!(report.asset_url.is_none());
        // The binary is untouched.
        assert_eq!(std::fs::read(&cfg.install_path).unwrap(), b"CURRENT");
    }

    #[test]
    fn pinned_downgrade_targets_the_lower_version() {
        let binary = b"the v0.1.0 binary";
        let tarball = tarball_bytes(binary);
        let sha = sha256_hex(&tarball);

        let mut server = mockito::Server::new();
        let _pinned = server
            .mock("GET", "/download/v0.1.0/dist-manifest.json")
            .with_status(200)
            .with_body(manifest_body("v0.1.0", &sha))
            .create();
        let _latest = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(manifest_body("v0.2.0", "unused"))
            .create();
        let _tar = server
            .mock("GET", "/download/v0.1.0/vard-aarch64-apple-darwin.tar.xz")
            .with_status(200)
            .with_body(tarball)
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path(), server.url());
        std::fs::write(&cfg.install_path, b"v0.2.0 binary").unwrap();
        cfg.current_version = "0.2.0".to_string();
        cfg.pinned_version = Some("0.1.0".to_string());

        let report = perform(&cfg).unwrap();
        assert_eq!(report.action, Action::Updated);
        assert_eq!(report.target_version, "0.1.0");
        assert_eq!(
            report.latest_version, "0.2.0",
            "the true latest is still reported"
        );
        // The pin is a downgrade FROM the latest, so no update is "available"
        // (latest == current), yet the downgrade still applies.
        assert!(
            !report.update_available,
            "already on the latest; the pin is a downgrade"
        );
        assert_eq!(std::fs::read(&cfg.install_path).unwrap(), binary);
    }

    #[test]
    fn checksum_mismatch_fails_before_swap_and_leaves_the_binary_untouched() {
        let tarball = tarball_bytes(b"tampered");
        // Advertise a sha that does not match the served body.
        let wrong_sha = sha256_hex(b"a different artifact entirely");

        let mut server = mockito::Server::new();
        let _m1 = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(manifest_body("v0.2.0", &wrong_sha))
            .create();
        let _m2 = server
            .mock("GET", "/download/v0.2.0/vard-aarch64-apple-darwin.tar.xz")
            .with_status(200)
            .with_body(tarball)
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path(), server.url());
        std::fs::write(&cfg.install_path, b"ORIGINAL").unwrap();
        cfg.current_version = "0.1.0".to_string();

        let err = perform(&cfg).unwrap_err();
        assert_eq!(
            err.code(),
            2,
            "an integrity failure is an operational error"
        );
        assert!(
            err.message().contains("sha256 mismatch"),
            "got: {}",
            err.message()
        );
        // The current binary is byte-for-byte untouched, and nothing is staged.
        assert_eq!(std::fs::read(&cfg.install_path).unwrap(), b"ORIGINAL");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("self-update"))
            .collect();
        assert!(leftovers.is_empty(), "staging leftovers: {leftovers:?}");
    }

    #[test]
    fn missing_receipt_blocks_with_attention_and_no_network() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = RunConfig {
            dry_run: true,
            pinned_version: None,
            // Points at a path with no receipt; no mock server needed — the gate
            // is checked before any fetch.
            receipt_path: tmp.path().join("vard-receipt.json"),
            install_path: tmp.path().join("vard"),
            releases_url: "http://127.0.0.1:1/unused".to_string(),
            target_triple: Some("aarch64-apple-darwin".to_string()),
            current_version: "0.1.0".to_string(),
        };
        let err = perform(&cfg).unwrap_err();
        assert_eq!(err.code(), 1, "a receipt block is the attention class");
        assert!(
            err.message().contains("install receipt"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("installer"),
            "points at the installer: {}",
            err.message()
        );
    }

    #[test]
    fn unknown_triple_blocks_with_attention() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = RunConfig {
            dry_run: true,
            pinned_version: None,
            receipt_path: write_receipt(tmp.path()),
            install_path: tmp.path().join("vard"),
            releases_url: "http://127.0.0.1:1/unused".to_string(),
            target_triple: None,
            current_version: "0.1.0".to_string(),
        };
        let err = perform(&cfg).unwrap_err();
        assert_eq!(err.code(), 1);
        assert!(err.message().contains("target"), "got: {}", err.message());
    }

    #[test]
    fn pinned_missing_tag_is_an_operational_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/download/v9.9.9/dist-manifest.json")
            .with_status(404)
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path(), server.url());
        cfg.dry_run = true;
        cfg.pinned_version = Some("9.9.9".to_string());

        let err = perform(&cfg).unwrap_err();
        assert_eq!(err.code(), 2);
        assert!(
            err.message().contains("does not exist"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn dry_run_resolves_the_plan_without_downloading() {
        let mut server = mockito::Server::new();
        // Only the manifest is mocked; a download attempt would 501 (no mock).
        let _m = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(manifest_body("v0.2.0", "sha-not-fetched"))
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = base_config(tmp.path(), server.url());
        cfg.dry_run = true;
        std::fs::write(&cfg.install_path, b"OLD").unwrap();

        let report = perform(&cfg).unwrap();
        assert_eq!(report.action, Action::WouldUpdate);
        assert_eq!(report.target_version, "0.2.0");
        assert!(report.asset_url.is_some());
        // Nothing downloaded, binary untouched.
        assert_eq!(std::fs::read(&cfg.install_path).unwrap(), b"OLD");
    }
}
