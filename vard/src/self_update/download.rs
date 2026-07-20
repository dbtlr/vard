//! Download the release tarball, verify its sha256, and extract the `vard`
//! binary. The sha256 is verified against the manifest checksum **before** any
//! extraction, so a tampered or truncated download never reaches the tar/xz
//! decoders.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// One retry with this backoff on a transient (transport) download error.
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// The binary's own name as cargo-dist ships it inside the release archive,
/// sourced from the bin target name so a future rename cannot silently desync
/// the extractor from the archive's contents. Falls back to the literal for
/// build contexts where `CARGO_BIN_NAME` is not set.
const BIN_NAME: &str = match option_env!("CARGO_BIN_NAME") {
    Some(name) => name,
    None => "vard",
};

/// Downloads `url` into `dest`, streaming the body to disk rather than buffering
/// the whole tarball in memory.
pub(crate) fn download_to(url: &str, dest: &Path) -> Result<(), String> {
    let mut last_err: Option<String> = None;
    for attempt in 0..2 {
        match super::http::agent().get(url).call() {
            Ok(response) => {
                let mut reader = response.into_reader();
                let mut file = fs::File::create(dest)
                    .map_err(|e| format!("creating {}: {e}", dest.display()))?;
                std::io::copy(&mut reader, &mut file)
                    .map_err(|e| format!("streaming the download to {}: {e}", dest.display()))?;
                file.flush()
                    .map_err(|e| format!("flushing {}: {e}", dest.display()))?;
                return Ok(());
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(format!("downloading {url} returned HTTP {code}"));
            }
            Err(ureq::Error::Transport(t)) => {
                last_err = Some(format!("downloading {url}: {t}"));
                if attempt == 0 {
                    std::thread::sleep(RETRY_BACKOFF);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| format!("downloading {url} failed")))
}

/// Verifies that the sha256 of `path` equals `expected` (lowercase hex).
pub(crate) fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex_lower(&hasher.finalize());
    if got != expected {
        return Err(format!(
            "sha256 mismatch for {}: expected {expected}, got {got}",
            path.display()
        ));
    }
    Ok(())
}

/// Extracts the `vard` binary from a cargo-dist `.tar.xz` archive to `dest`,
/// setting it executable (0o755). Errors when the archive contains no file whose
/// basename is the bin target name.
pub(crate) fn extract_binary(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(archive)
        .map_err(|e| format!("opening archive {}: {e}", archive.display()))?;
    let xz = xz2::read::XzDecoder::new(file);
    let mut tar = tar::Archive::new(xz);
    for entry in tar.entries().map_err(|e| format!("reading archive: {e}"))? {
        let mut entry = entry.map_err(|e| format!("reading archive entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("reading entry path: {e}"))?;
        if path.file_name().and_then(|s| s.to_str()) == Some(BIN_NAME) {
            let mut out =
                fs::File::create(dest).map_err(|e| format!("creating {}: {e}", dest.display()))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| format!("writing {}: {e}", dest.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("setting mode on {}: {e}", dest.display()))?;
            }
            return Ok(());
        }
    }
    Err(format!(
        "archive {} did not contain a {BIN_NAME} binary",
        archive.display()
    ))
}

/// A temp path adjacent to `install_path`, hidden and prefixed so it is
/// recognizable. Same-directory placement guarantees the swap's `rename(2)`
/// stays within one filesystem and is therefore atomic.
pub(crate) fn sibling_temp_path(install_path: &Path, suffix: &str) -> PathBuf {
    let parent = install_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = install_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(BIN_NAME);
    parent.join(format!(".{stem}-self-update-{suffix}"))
}

/// Lowercase hex encoding of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_name_is_vard() {
        // The archive entry the extractor matches must be the shipped binary
        // name; a bin-target rename must update the release path alongside.
        assert_eq!(BIN_NAME, "vard");
    }

    #[test]
    fn hex_lower_encodes_bytes() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }

    #[test]
    fn download_writes_body_to_destination() {
        let mut server = mockito::Server::new();
        let body = b"hello world";
        let _m = server
            .mock("GET", "/vard.tar.xz")
            .with_status(200)
            .with_body(body)
            .create();
        let url = format!("{}/vard.tar.xz", server.url());
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("vard.tar.xz");
        download_to(&url, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn download_surfaces_404() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", "/gone").with_status(404).create();
        let url = format!("{}/gone", server.url());
        let tmp = tempfile::tempdir().unwrap();
        let err = download_to(&url, &tmp.path().join("out")).unwrap_err();
        assert!(err.contains("HTTP 404"), "got: {err}");
    }

    #[test]
    fn verify_sha256_ok_when_match() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("blob");
        fs::write(&file, b"hello world").unwrap();
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        verify_sha256(&file, expected).unwrap();
    }

    #[test]
    fn verify_sha256_err_on_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("blob");
        fs::write(&file, b"hello world").unwrap();
        let err = verify_sha256(&file, "deadbeef").unwrap_err();
        assert!(err.contains("sha256 mismatch"), "got: {err}");
    }

    /// Builds a `.tar.xz` containing one `<entry>` file plus a noise file.
    fn tarball_with_entry(archive_path: &Path, entry: &str) {
        let xz_writer = xz2::write::XzEncoder::new(fs::File::create(archive_path).unwrap(), 6);
        let mut builder = tar::Builder::new(xz_writer);

        let mut header = tar::Header::new_gnu();
        header.set_path(entry).unwrap();
        header.set_size(11);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &b"fake binary"[..]).unwrap();

        let mut header = tar::Header::new_gnu();
        header.set_path("noise/README.md").unwrap();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &b"noise"[..]).unwrap();

        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn extract_binary_pulls_vard_from_tarball() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("release.tar.xz");
        // The real cargo-dist layout: `vard-<target>/vard`.
        tarball_with_entry(&archive, "vard-aarch64-apple-darwin/vard");
        let dest = tmp.path().join("vard.new");
        extract_binary(&archive, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"fake binary");
    }

    #[test]
    fn extract_binary_errors_when_no_vard_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("stale.tar.xz");
        tarball_with_entry(&archive, "other-tool/other");
        let dest = tmp.path().join("vard.new");
        let err = extract_binary(&archive, &dest).unwrap_err();
        assert!(err.contains("did not contain"), "got: {err}");
    }

    #[test]
    fn sibling_temp_path_is_hidden_and_adjacent() {
        let install = Path::new("/opt/homebrew/bin/vard");
        let temp = sibling_temp_path(install, "0.2.0.bin");
        assert_eq!(temp.parent().unwrap(), Path::new("/opt/homebrew/bin"));
        assert_eq!(
            temp.file_name().unwrap().to_str().unwrap(),
            ".vard-self-update-0.2.0.bin"
        );
    }
}
