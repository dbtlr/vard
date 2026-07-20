//! The cargo-dist install receipt — the gate for `vard self-update`.
//!
//! The shell installer cargo-dist ships writes a receipt next to vard's own
//! config, at `<config-dir>/vard/vard-receipt.json` (the same directory as
//! `config.toml`). Its presence is what proves this binary was installed by the
//! official installer and can therefore be swapped in place; a `cargo install`,
//! a Homebrew install, or a source build has no receipt and is directed back to
//! its own tooling instead of having its binary replaced underneath it.
//!
//! Only the fields worth reading are modeled — the receipt exists to *gate*, not
//! to drive the update (the target triple comes from the compile-time
//! [`resolve::TARGET_TRIPLE`](super::resolve::TARGET_TRIPLE) and the current
//! version from `CARGO_PKG_VERSION`). Unknown fields are ignored, so a receipt
//! written by a newer cargo-dist still parses.

use std::path::Path;

use serde::Deserialize;

/// The subset of cargo-dist's `vard-receipt.json` this cares about. Every field
/// is optional: the receipt only has to *exist and parse* to open the gate, and
/// the shape varies across cargo-dist versions. The fields are modeled to
/// document the shape and to let tests assert on it; the gate itself reads none
/// of them (presence-and-parse is the whole contract).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct Receipt {
    /// The install "app name" cargo-dist recorded (`vard`).
    #[serde(default)]
    pub app_name: Option<String>,
    /// The version the receipt was written for.
    #[serde(default)]
    pub version: Option<String>,
    /// Where the installer placed the binary.
    #[serde(default)]
    pub install_prefix: Option<String>,
}

/// Loads and parses the install receipt at `path`.
///
/// `Ok(None)` when no receipt is present (this binary was not installed by the
/// official installer); `Ok(Some(_))` when it is present and parses; `Err` when
/// it is present but malformed. The caller blocks self-update on anything other
/// than `Ok(Some(_))`.
pub(crate) fn load(path: &Path) -> Result<Option<Receipt>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "reading the install receipt {}: {e}",
                path.display()
            ));
        }
    };
    serde_json::from_slice::<Receipt>(&bytes)
        .map(Some)
        .map_err(|e| format!("parsing the install receipt {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative receipt as the v0.1.0 shell installer writes it: the
    /// app name is `vard`, plus a version and an install prefix.
    const SAMPLE_RECEIPT: &str = r#"{
        "binaries": ["vard"],
        "install_prefix": "/home/user/.cargo",
        "binary_aliases": {},
        "cargo_dist_version": "0.32.0",
        "install_layout": "cargo-home",
        "modify_path": true,
        "provider": { "source": "cargo-dist", "version": "0.32.0" },
        "source": {
            "app_name": "vard",
            "name": "vard",
            "owner": "dbtlr",
            "release_type": "github"
        },
        "app_name": "vard",
        "version": "0.1.0"
    }"#;

    #[test]
    fn load_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("vard-receipt.json");
        assert!(load(&missing).unwrap().is_none());
    }

    #[test]
    fn load_some_when_present_and_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vard-receipt.json");
        std::fs::write(&path, SAMPLE_RECEIPT).unwrap();
        let receipt = load(&path).unwrap().expect("a present receipt loads");
        assert_eq!(receipt.app_name.as_deref(), Some("vard"));
        assert_eq!(receipt.version.as_deref(), Some("0.1.0"));
        assert_eq!(receipt.install_prefix.as_deref(), Some("/home/user/.cargo"));
    }

    #[test]
    fn load_err_when_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vard-receipt.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_err());
    }
}
