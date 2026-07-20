//! Compile-time target-triple detection, asset selection, and the action a run
//! resolves to.

#[cfg(test)]
use super::manifest::Artifact;
use super::manifest::DistManifest;

/// The compile-time target triple for the running binary, or `None` when vard
/// was built for a target cargo-dist does not publish a release artifact for
/// (developer builds, unusual targets). The four arms mirror the release triples
/// in `dist-workspace.toml`.
pub(crate) const TARGET_TRIPLE: Option<&str> =
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        Some("x86_64-unknown-linux-musl")
    } else {
        None
    };

/// Finds the release artifact matching `triple`, returning its `(name, sha256)`.
/// Only `executable-zip` artifacts (the per-target binary tarballs) are
/// considered; checksum sidecar entries are skipped.
pub(crate) fn select_asset<'a>(
    manifest: &'a DistManifest,
    triple: &str,
) -> Option<(&'a str, &'a str)> {
    manifest.artifacts.values().find_map(|art| {
        if art.kind == "executable-zip" && art.target_triples.iter().any(|t| t == triple) {
            let sha = art.checksums.get("sha256")?;
            Some((art.name.as_str(), sha.as_str()))
        } else {
            None
        }
    })
}

/// What a run resolved to do, once the target version is known against the
/// current one — the four cross of dry-run × same-version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// A dry run that would apply an update.
    WouldUpdate,
    /// A dry run that would change nothing (already on the target version).
    WouldNoOp,
    /// A real run that applied an update.
    Updated,
    /// A real run that changed nothing (already on the target version).
    NoOp,
}

impl Action {
    /// The stable snake_case token stored in the `action` field of the machine
    /// output.
    pub(crate) fn token(self) -> &'static str {
        match self {
            Action::WouldUpdate => "would_update",
            Action::WouldNoOp => "would_no_op",
            Action::Updated => "updated",
            Action::NoOp => "no_op",
        }
    }
}

/// Resolves the [`Action`] from whether this is a dry run and whether the target
/// version equals the current one.
pub(crate) fn determine_action(
    dry_run: bool,
    target_version: &str,
    current_version: &str,
) -> Action {
    match (dry_run, target_version == current_version) {
        (true, false) => Action::WouldUpdate,
        (true, true) => Action::WouldNoOp,
        (false, false) => Action::Updated,
        (false, true) => Action::NoOp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn artifact(name: &str, kind: &str, triple: &str, sha: &str) -> (String, Artifact) {
        let mut checksums = BTreeMap::new();
        checksums.insert("sha256".to_string(), sha.to_string());
        (
            name.to_string(),
            Artifact {
                name: name.to_string(),
                kind: kind.to_string(),
                target_triples: vec![triple.to_string()],
                checksums,
            },
        )
    }

    fn manifest_with(artifacts: Vec<(String, Artifact)>) -> DistManifest {
        DistManifest {
            announcement_tag: "v0.2.0".to_string(),
            artifacts: artifacts.into_iter().collect(),
        }
    }

    #[test]
    fn select_asset_finds_matching_triple() {
        let m = manifest_with(vec![
            artifact("a", "executable-zip", "aarch64-apple-darwin", "AA"),
            artifact("b", "executable-zip", "x86_64-apple-darwin", "BB"),
        ]);
        assert_eq!(select_asset(&m, "x86_64-apple-darwin"), Some(("b", "BB")));
    }

    #[test]
    fn select_asset_ignores_checksum_kind() {
        let m = manifest_with(vec![artifact(
            "a.sha256",
            "checksum",
            "aarch64-apple-darwin",
            "ZZ",
        )]);
        assert_eq!(select_asset(&m, "aarch64-apple-darwin"), None);
    }

    #[test]
    fn select_asset_none_for_unknown_triple() {
        let m = manifest_with(vec![artifact(
            "a",
            "executable-zip",
            "aarch64-apple-darwin",
            "AA",
        )]);
        assert_eq!(select_asset(&m, "some-other-triple"), None);
    }

    #[test]
    fn action_truth_table() {
        assert_eq!(
            determine_action(true, "0.2.0", "0.1.0"),
            Action::WouldUpdate
        );
        assert_eq!(determine_action(true, "0.1.0", "0.1.0"), Action::WouldNoOp);
        assert_eq!(determine_action(false, "0.2.0", "0.1.0"), Action::Updated);
        assert_eq!(determine_action(false, "0.1.0", "0.1.0"), Action::NoOp);
    }

    #[test]
    fn action_tokens_are_stable() {
        assert_eq!(Action::WouldUpdate.token(), "would_update");
        assert_eq!(Action::WouldNoOp.token(), "would_no_op");
        assert_eq!(Action::Updated.token(), "updated");
        assert_eq!(Action::NoOp.token(), "no_op");
    }
}
