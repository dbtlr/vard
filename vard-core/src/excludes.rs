//! The shared catalog of default exclude patterns.
//!
//! vard keeps well-known secret shapes, build output, and OS cruft out of the
//! snapshots it takes. Two consumers need the *same* list: the CLI seeds each
//! watched repository's private `.git/info/exclude` with it (`vard watch add`),
//! and the daemon-side secret quarantine (VRD-22) matches against it. The
//! patterns therefore live here in the correctness crate — one source of truth
//! — rather than being duplicated per host.
//!
//! The patterns are in gitignore dialect. Rendering them into a file (comment
//! headers, managed-block markers) is a host presentation concern and stays in
//! the CLI; this module owns only the pattern data.

/// Secret-file patterns: credentials, private keys, and environment files that
/// must never be snapshotted. Listed first by convention so they are visually
/// prominent wherever the catalog is rendered.
pub const SECRET_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "id_rsa",
    "id_rsa*",
    "id_dsa*",
    "id_ecdsa*",
    "id_ed25519*",
    ".netrc",
    ".aws/credentials",
    "gcloud-credentials.json",
];

/// Dependency, build, and cache directories that bloat snapshots without
/// carrying durable state.
pub const BUILD_CACHE_PATTERNS: &[&str] = &[
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    ".cache/",
    "__pycache__/",
    ".venv/",
    "venv/",
];

/// OS and editor cruft that has no place in a snapshot.
pub const OS_CRUFT_PATTERNS: &[&str] = &[".DS_Store", "Thumbs.db"];
