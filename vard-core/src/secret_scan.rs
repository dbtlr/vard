//! Pure secret detection: the scanner behind vard's secret quarantine (VRD-22).
//!
//! # The contract
//!
//! This module owns *detection*, not policy. It answers one question — "does
//! this path or this content look like a leaked credential?" — and answers it
//! without touching the filesystem, holding no state beyond a compiled matcher,
//! and taking no snapshot-pipeline decisions. Quarantine policy (what to do with
//! a hit, where it surfaces in the health file) is a host concern re-derived
//! every snapshot pass from a [`SecretScanner`] injected by the daemon; that
//! wiring lands in later VRD-22 checkpoints, not here.
//!
//! A scanner is compiled once per watch from two inputs: whether scanning is
//! enabled at all, and the watch's *extra* filename patterns. Compilation folds
//! [`excludes::SECRET_PATTERNS`](crate::excludes::SECRET_PATTERNS) — the shared
//! catalog of secret filename shapes, the single source of truth — together with
//! those extras into one gitignore matcher. The catalog is never duplicated
//! here.
//!
//! Detection has three layers, each deliberately conservative to keep false
//! positives rare (a false positive quarantines a file the user wanted):
//!
//! 1. **Filename patterns** ([`SecretScanner::scan_path`]) — a path whose name
//!    matches a secret filename shape (`.env`, `id_rsa`, `*.pem`, plus the
//!    watch's extras). Works without reading the file.
//! 2. **Token prefixes** ([`SecretScanner::scan_content`]) — high-precision,
//!    case-sensitive credential markers (AWS `AKIA…`, GitHub `ghp_…`, PEM
//!    private-key headers, …) each requiring enough trailing token body to be a
//!    real credential rather than the prefix appearing as a word.
//! 3. **Entropy** ([`SecretScanner::scan_content`]) — a strict last resort: a
//!    long base64-class run with high Shannon entropy *and* mixed character
//!    classes, skipping pure-hex runs (git hashes) and lockfile manifests.
//!
//! The content layers skip binary files (a NUL byte in the first 8 KiB) and cap
//! the scanned window at [`MAX_SCAN_BYTES`] so a huge file costs bounded work.
//!
//! There is no `regex` dependency: every check is plain byte/str scanning, and
//! the filename layer reuses the `ignore` gitignore matcher the watcher already
//! depends on. A thin filesystem convenience ([`SecretScanner::scan_file`]) is
//! offered separately for hosts, but the core functions stay pure.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::excludes::SECRET_PATTERNS;

/// Largest content window the content heuristics scan, in bytes (1 MiB).
///
/// Content larger than this is not skipped: only the first `MAX_SCAN_BYTES` are
/// scanned, so a credential in the head of a large file is still caught while
/// the work stays bounded.
pub const MAX_SCAN_BYTES: usize = 1024 * 1024;

/// How many leading bytes the binary sniff inspects for a NUL (8 KiB). A NUL in
/// this window marks the content binary and skips content scanning entirely.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Minimum length of a base64-class run for the entropy heuristic to consider
/// it a candidate.
const ENTROPY_MIN_RUN: usize = 40;

/// Minimum Shannon entropy (bits per character) for an entropy candidate to be
/// flagged.
const ENTROPY_MIN_BITS: f64 = 4.5;

/// Character classes a token rule requires of the body following its prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharClass {
    /// `[A-Z0-9]` — uppercase letters and digits (AWS access-key IDs).
    UpperDigit,
    /// `[A-Za-z0-9]` — alphanumerics (GitHub personal tokens).
    Alnum,
    /// `[A-Za-z0-9_-]` — alphanumerics plus `_` and `-` (most token bodies).
    Token,
}

impl CharClass {
    #[inline]
    fn accepts(self, b: u8) -> bool {
        match self {
            CharClass::UpperDigit => b.is_ascii_uppercase() || b.is_ascii_digit(),
            CharClass::Alnum => b.is_ascii_alphanumeric(),
            CharClass::Token => b.is_ascii_alphanumeric() || b == b'_' || b == b'-',
        }
    }
}

/// One credential-token family: a set of case-sensitive prefixes and the amount
/// and class of token body that must follow one of them to count as a match.
struct TokenRule {
    /// Human-readable family name, surfaced in [`SecretReason::TokenPrefix`].
    family: &'static str,
    /// Case-sensitive byte prefixes; any one of them can anchor a match.
    prefixes: &'static [&'static str],
    /// Minimum number of [`class`](Self::class) bytes that must follow the
    /// prefix for it to be a credential rather than the prefix as a word.
    min_body: usize,
    /// The character class the trailing body must consist of.
    class: CharClass,
}

/// The credential-token families, one row per family. Add a family by adding a
/// row; add a spelling to an existing family by extending its `prefixes`.
///
/// PEM private-key headers are detected separately (see [`scan_pem`]) because
/// they are a line shape, not a prefix-plus-body token.
const TOKEN_RULES: &[TokenRule] = &[
    TokenRule {
        family: "AWS access key",
        prefixes: &["AKIA"],
        min_body: 16,
        class: CharClass::UpperDigit,
    },
    TokenRule {
        family: "GitHub token",
        prefixes: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        min_body: 36,
        class: CharClass::Alnum,
    },
    TokenRule {
        family: "GitHub fine-grained token",
        prefixes: &["github_pat_"],
        min_body: 22,
        class: CharClass::Token,
    },
    TokenRule {
        family: "GitLab token",
        prefixes: &["glpat-"],
        min_body: 20,
        class: CharClass::Token,
    },
    TokenRule {
        family: "Slack token",
        prefixes: &["xoxb-", "xoxp-", "xoxa-", "xoxs-"],
        min_body: 10,
        class: CharClass::Token,
    },
    TokenRule {
        family: "Anthropic API key",
        prefixes: &["sk-ant-"],
        min_body: 20,
        class: CharClass::Token,
    },
    TokenRule {
        family: "OpenAI API key",
        prefixes: &["sk-proj-"],
        min_body: 20,
        class: CharClass::Token,
    },
    TokenRule {
        family: "Google API key",
        prefixes: &["AIza"],
        min_body: 35,
        class: CharClass::Token,
    },
];

/// Basenames whose entropy runs are ignored: lockfiles and manifests carry long
/// high-entropy integrity hashes that are not secrets. The skip is scoped to the
/// *entropy* heuristic only — token prefixes and filename patterns still apply.
const LOCKFILE_NAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "go.sum",
    "composer.lock",
    "Gemfile.lock",
    "flake.lock",
];

/// Why a path or its content was flagged as a likely secret.
///
/// Carries enough to explain the hit in a log line or a health summary without
/// echoing the secret material itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretReason {
    /// The path's name matched a secret filename pattern (from the built-in
    /// catalog or the watch's extras). Carries the pattern that matched.
    FilenamePattern {
        /// The gitignore pattern that matched, e.g. `id_rsa*`.
        pattern: String,
    },
    /// The content contained a known credential token. Carries the family name.
    TokenPrefix {
        /// The credential family, e.g. `AWS access key` or `PEM private key`.
        family: &'static str,
    },
    /// The content contained a long, high-entropy, mixed-class base64 run.
    Entropy,
}

impl fmt::Display for SecretReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretReason::FilenamePattern { pattern } => {
                write!(f, "filename matches secret pattern {pattern:?}")
            }
            SecretReason::TokenPrefix { family } => write!(f, "contains {family}"),
            SecretReason::Entropy => f.write_str("contains a high-entropy secret-like string"),
        }
    }
}

/// A detected likely-secret: the relative path that triggered it and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretMatch {
    /// The watch-relative path that was flagged.
    pub path: PathBuf,
    /// Why it was flagged.
    pub reason: SecretReason,
}

impl fmt::Display for SecretMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.reason)
    }
}

/// A compiled per-watch secret scanner: a filename matcher plus the enabled
/// flag. Build one with [`SecretScanner::compile`]; a disabled scanner returns
/// `None` from every method.
#[derive(Debug)]
pub struct SecretScanner {
    enabled: bool,
    filenames: Gitignore,
}

impl SecretScanner {
    /// Compiles a scanner from whether scanning is `enabled` and the watch's
    /// `extra_patterns` (additional secret filename shapes on top of the
    /// built-in [`crate::excludes::SECRET_PATTERNS`] catalog).
    ///
    /// The patterns are gitignore dialect. An extra pattern that is not valid
    /// gitignore syntax is a [`SecretScanError`] naming the pattern; the
    /// built-in catalog is always valid.
    pub fn compile(enabled: bool, extra_patterns: &[String]) -> Result<Self, SecretScanError> {
        // Rooted at "/" — the matcher is fed watch-relative paths, and the root
        // only matters for stripping an absolute-path prefix, which never
        // happens here. The patterns themselves carry all the semantics.
        let mut builder = GitignoreBuilder::new("/");
        for pattern in SECRET_PATTERNS {
            builder
                .add_line(None, pattern)
                .map_err(|e| SecretScanError {
                    pattern: (*pattern).to_string(),
                    reason: e.to_string(),
                })?;
        }
        for pattern in extra_patterns {
            builder
                .add_line(None, pattern)
                .map_err(|e| SecretScanError {
                    pattern: pattern.clone(),
                    reason: e.to_string(),
                })?;
        }
        let filenames = builder.build().map_err(|e| SecretScanError {
            pattern: extra_patterns.join(", "),
            reason: e.to_string(),
        })?;
        Ok(Self { enabled, filenames })
    }

    /// Whether this scanner is enabled. A disabled scanner never flags anything.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Checks `rel_path`'s name against the secret filename patterns. Returns a
    /// [`SecretMatch`] naming the pattern that matched, or `None`. Needs no file
    /// content, so it catches a secret file even when its bytes are unavailable.
    pub fn scan_path(&self, rel_path: &Path) -> Option<SecretMatch> {
        if !self.enabled {
            return None;
        }
        match self.filenames.matched(rel_path, false) {
            Match::Ignore(glob) => Some(SecretMatch {
                path: rel_path.to_path_buf(),
                reason: SecretReason::FilenamePattern {
                    pattern: glob.original().to_string(),
                },
            }),
            _ => None,
        }
    }

    /// Runs the content heuristics over `content` (bytes of the file at
    /// `rel_path`): binary skip, size cap, then token prefixes and, unless the
    /// path is a lockfile, the entropy heuristic. Returns the first hit or
    /// `None`.
    ///
    /// This does *not* re-run [`scan_path`](Self::scan_path); a caller that
    /// wants both layers calls both (see [`scan_file`](Self::scan_file)).
    pub fn scan_content(&self, rel_path: &Path, content: &[u8]) -> Option<SecretMatch> {
        if !self.enabled {
            return None;
        }
        // Binary skip: a NUL in the first 8 KiB means binary; don't scan.
        let sniff = &content[..content.len().min(BINARY_SNIFF_BYTES)];
        if sniff.contains(&0) {
            return None;
        }
        // Size cap: scan only the first MAX_SCAN_BYTES of a large file.
        let window = &content[..content.len().min(MAX_SCAN_BYTES)];

        if let Some(family) = scan_tokens(window) {
            return Some(SecretMatch {
                path: rel_path.to_path_buf(),
                reason: SecretReason::TokenPrefix { family },
            });
        }
        if !is_lockfile(rel_path) && scan_entropy(window) {
            return Some(SecretMatch {
                path: rel_path.to_path_buf(),
                reason: SecretReason::Entropy,
            });
        }
        None
    }

    /// Filesystem convenience: check `rel_path`'s name, then read the file at
    /// `abs_path` (capped at [`MAX_SCAN_BYTES`]) and run the content heuristics.
    ///
    /// Kept separate from the pure core so the detection logic stays I/O-free.
    /// A read error yields `Ok(None)` for the content layer — the filename layer
    /// still applies — because an unreadable file is not evidence of a secret;
    /// callers that need to distinguish read failures should read themselves and
    /// call [`scan_path`](Self::scan_path)/[`scan_content`](Self::scan_content).
    pub fn scan_file(&self, rel_path: &Path, abs_path: &Path) -> Option<SecretMatch> {
        if !self.enabled {
            return None;
        }
        if let Some(hit) = self.scan_path(rel_path) {
            return Some(hit);
        }
        let content = read_capped(abs_path)?;
        self.scan_content(rel_path, &content)
    }
}

/// Reads up to [`MAX_SCAN_BYTES`] + 1 bytes of `path` (the extra byte only
/// distinguishes "exactly at the cap" from "over it"; the scan window is still
/// capped). Returns `None` on any I/O error.
fn read_capped(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_SCAN_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

/// Scans `content` for any [`TOKEN_RULES`] prefix (or a PEM header) with enough
/// trailing body, returning the matched family name.
fn scan_tokens(content: &[u8]) -> Option<&'static str> {
    for rule in TOKEN_RULES {
        for prefix in rule.prefixes {
            if let Some(family) = find_token(content, prefix.as_bytes(), rule) {
                return Some(family);
            }
        }
    }
    scan_pem(content)
}

/// Returns `rule.family` if `prefix` occurs in `content` at a token boundary and
/// is followed by at least `rule.min_body` bytes of `rule.class`.
fn find_token(content: &[u8], prefix: &[u8], rule: &TokenRule) -> Option<&'static str> {
    let mut from = 0;
    while let Some(rel) = find_sub(&content[from..], prefix) {
        let start = from + rel;
        // Leading boundary: the prefix must not sit inside a larger token, so
        // the preceding byte cannot itself be a token-class char. This rejects
        // an incidental prefix embedded in a longer word.
        let boundary = start == 0 || !CharClass::Token.accepts(content[start - 1]);
        if boundary {
            let body = &content[start + prefix.len()..];
            let body_len = body.iter().take_while(|&&b| rule.class.accepts(b)).count();
            if body_len >= rule.min_body {
                return Some(rule.family);
            }
        }
        from = start + 1;
    }
    None
}

/// Detects a PEM private-key header: a single line containing both `-----BEGIN `
/// and ` PRIVATE KEY-----` (covers `RSA`, `OPENSSH`, `EC`, and bare variants).
/// A `CERTIFICATE` line lacks the private-key marker and is not flagged.
fn scan_pem(content: &[u8]) -> Option<&'static str> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const KEY: &[u8] = b" PRIVATE KEY-----";
    for line in content.split(|&b| b == b'\n') {
        if let Some(begin) = find_sub(line, BEGIN)
            && find_sub(&line[begin + BEGIN.len()..], KEY).is_some()
        {
            return Some("PEM private key");
        }
    }
    None
}

/// Byte-substring search (`needle` in `haystack`). An empty needle matches at 0.
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether any maximal base64-class run in `content` is at least
/// [`ENTROPY_MIN_RUN`] long, not pure hex, mixed-class, and above the entropy
/// threshold.
fn scan_entropy(content: &[u8]) -> bool {
    let mut i = 0;
    let n = content.len();
    while i < n {
        if is_base64_class(content[i]) {
            let start = i;
            while i < n && is_base64_class(content[i]) {
                i += 1;
            }
            let run = &content[start..i];
            if run.len() >= ENTROPY_MIN_RUN && run_looks_secret(run) {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

/// Whether a base64-class run passes the strict entropy gate: not pure hex,
/// contains upper + lower + digit, and Shannon entropy ≥ [`ENTROPY_MIN_BITS`].
fn run_looks_secret(run: &[u8]) -> bool {
    // Pure hex (git/sha hashes) is never a secret candidate, regardless of
    // length — checked before the class mix so uppercase hex is skipped too.
    if run.iter().all(u8::is_ascii_hexdigit) {
        return false;
    }
    let has_upper = run.iter().any(u8::is_ascii_uppercase);
    let has_lower = run.iter().any(u8::is_ascii_lowercase);
    let has_digit = run.iter().any(u8::is_ascii_digit);
    if !(has_upper && has_lower && has_digit) {
        return false;
    }
    shannon_entropy(run) >= ENTROPY_MIN_BITS
}

/// Shannon entropy of `data` in bits per byte.
fn shannon_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let len = data.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Whether `b` is a base64-class character (`[A-Za-z0-9+/=_-]`, covering
/// standard and URL-safe base64 plus padding).
#[inline]
fn is_base64_class(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// Whether `rel_path`'s basename is a lockfile/manifest whose entropy runs are
/// integrity hashes rather than secrets.
fn is_lockfile(rel_path: &Path) -> bool {
    let Some(name) = rel_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // The `*.lock` glob covers Cargo.lock, yarn.lock, composer.lock, etc.; the
    // named list catches the non-`.lock` spellings (package-lock.json, go.sum).
    name.ends_with(".lock") || LOCKFILE_NAMES.contains(&name)
}

/// An invalid extra secret filename pattern supplied at [`SecretScanner::compile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretScanError {
    /// The offending pattern.
    pub pattern: String,
    /// Why it failed to compile as gitignore syntax.
    pub reason: String,
}

impl fmt::Display for SecretScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid secret pattern {:?}: {}",
            self.pattern, self.reason
        )
    }
}

impl Error for SecretScanError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner() -> SecretScanner {
        SecretScanner::compile(true, &[]).unwrap()
    }

    fn content_reason(name: &str, content: &[u8]) -> Option<SecretReason> {
        scanner()
            .scan_content(Path::new(name), content)
            .map(|m| m.reason)
    }

    // --- token prefixes: one positive per family + boundary negatives -------

    #[test]
    fn aws_access_key_is_detected() {
        let content = b"aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n";
        assert_eq!(
            content_reason("creds.txt", content),
            Some(SecretReason::TokenPrefix {
                family: "AWS access key"
            })
        );
    }

    #[test]
    fn github_classic_token_is_detected() {
        // ghp_ + 36 alnum body.
        let token = format!("token: ghp_{}\n", "a1B2c3D4e5".repeat(4)); // 40 alnum body
        assert_eq!(
            content_reason("ci.yml", token.as_bytes()),
            Some(SecretReason::TokenPrefix {
                family: "GitHub token"
            })
        );
    }

    #[test]
    fn github_fine_grained_token_is_detected() {
        let token = "github_pat_A1b2C3d4E5f6G7h8I9j0K1\n"; // 22-char body
        assert_eq!(
            content_reason("env", token.as_bytes()),
            Some(SecretReason::TokenPrefix {
                family: "GitHub fine-grained token"
            })
        );
    }

    #[test]
    fn gitlab_token_is_detected() {
        let token = concat!("gl", "pat-xY3zAb9Qwrite20chars\n"); // 20-char body, split so the source blob never contains a token-shaped literal
        assert_eq!(
            content_reason("env", token.as_bytes()),
            Some(SecretReason::TokenPrefix {
                family: "GitLab token"
            })
        );
    }

    #[test]
    fn slack_token_is_detected() {
        let content = b"SLACK=xoxb-1234567890-abcdef\n";
        assert_eq!(
            content_reason("env", content),
            Some(SecretReason::TokenPrefix {
                family: "Slack token"
            })
        );
    }

    #[test]
    fn anthropic_key_is_detected() {
        let content = b"ANTHROPIC_API_KEY=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv\n";
        assert_eq!(
            content_reason("env", content),
            Some(SecretReason::TokenPrefix {
                family: "Anthropic API key"
            })
        );
    }

    #[test]
    fn openai_project_key_is_detected() {
        let content = b"OPENAI_API_KEY=sk-proj-AbCdEfGhIjKlMnOpQrStUv\n";
        assert_eq!(
            content_reason("env", content),
            Some(SecretReason::TokenPrefix {
                family: "OpenAI API key"
            })
        );
    }

    #[test]
    fn google_api_key_is_detected() {
        // AIza + 35 token chars.
        let content = b"key=AIzaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r\n";
        assert_eq!(
            content_reason("env", content),
            Some(SecretReason::TokenPrefix {
                family: "Google API key"
            })
        );
    }

    #[test]
    fn bare_prefix_without_body_is_not_a_token() {
        // Literal prefixes as prose, no credential body following.
        assert_eq!(
            content_reason("notes.md", b"The AKIA prefix marks AWS keys.\n"),
            None
        );
        assert_eq!(
            content_reason("notes.md", b"keys start with sk-ant- normally\n"),
            None
        );
        assert_eq!(
            content_reason("notes.md", b"a ghp_ token looks like this\n"),
            None
        );
    }

    #[test]
    fn prefix_embedded_in_longer_word_is_not_a_token() {
        // "xAKIA…" — the AKIA is not at a token boundary, so it must not match
        // even though a long uppercase body follows.
        assert_eq!(content_reason("x", b"ZZZAKIAIOSFODNN7EXAMPLE\n"), None);
    }

    // --- PEM ----------------------------------------------------------------

    #[test]
    fn pem_rsa_private_key_header_is_detected() {
        let content = b"-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----\n";
        assert_eq!(
            content_reason("id_rsa.txt", content),
            Some(SecretReason::TokenPrefix {
                family: "PEM private key"
            })
        );
    }

    #[test]
    fn pem_openssh_private_key_header_is_detected() {
        let content = b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnN...\n";
        assert_eq!(
            content_reason("key.txt", content),
            Some(SecretReason::TokenPrefix {
                family: "PEM private key"
            })
        );
    }

    #[test]
    fn pem_certificate_header_is_not_a_private_key() {
        let content = b"-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----\n";
        assert_eq!(content_reason("cert.pem.txt", content), None);
    }

    // --- entropy ------------------------------------------------------------

    #[test]
    fn high_entropy_mixed_base64_is_detected() {
        // 64 chars, mixed upper/lower/digit, high entropy.
        let content = b"session=Zk3Lp9Qw2Rt8Xv1Nb6Yc4Md7Hf0Jg5As2Dl8Ke3Pi9Uo1Wr4Tz7Qy6Bn0Xm\n";
        assert_eq!(
            content_reason("app.log", content),
            Some(SecretReason::Entropy)
        );
    }

    #[test]
    fn git_sha_hex_is_not_entropy() {
        // 40-char lowercase hex — pure hex, skipped regardless of length.
        let content = b"commit 356a192b7913b04c54574d18c28d46e6395428ab now\n";
        assert_eq!(content_reason("log.txt", content), None);
    }

    #[test]
    fn lockfile_integrity_hash_is_skipped_for_entropy() {
        // A yarn.lock integrity line: high-entropy base64, but the file is a
        // lockfile so the entropy heuristic ignores it.
        let content =
            b"  resolved \"https://r/x\"\n  integrity sha512-Zk3Lp9Qw2Rt8Xv1Nb6Yc4Md7Hf0Jg5As2Dl8Ke3Pi9Uo1Wr4Tz==\n";
        assert_eq!(content_reason("yarn.lock", content), None);
        // The very same content in a non-lockfile IS flagged, proving the skip
        // is name-scoped, not content-scoped.
        assert_eq!(
            content_reason("notes.txt", content),
            Some(SecretReason::Entropy)
        );
    }

    #[test]
    fn ordinary_prose_is_not_entropy() {
        let content = b"The quick brown fox jumps over the lazy dog again and again today.\n";
        assert_eq!(content_reason("readme.md", content), None);
    }

    #[test]
    fn all_lowercase_base64_run_fails_mixed_class() {
        // 50 lowercase letters: long enough, but no uppercase or digit.
        let content = b"data=abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwx\n";
        assert_eq!(content_reason("x.txt", content), None);
    }

    // --- binary skip & size cap ---------------------------------------------

    #[test]
    fn binary_content_with_nul_is_skipped() {
        // A token that WOULD match, but a NUL in the first 8 KiB marks binary.
        let mut content = b"AKIAIOSFODNN7EXAMPLE".to_vec();
        content.insert(0, 0); // NUL up front
        assert_eq!(content_reason("blob.bin", &content), None);
    }

    #[test]
    fn nul_after_sniff_window_does_not_skip() {
        // A NUL past the 8 KiB sniff window does not mark the file binary, so a
        // token before it is still caught.
        let mut content = b"AKIAIOSFODNN7EXAMPLE\n".to_vec();
        content.resize(BINARY_SNIFF_BYTES + 100, b'x');
        content.push(0);
        assert_eq!(
            content_reason("big.txt", &content),
            Some(SecretReason::TokenPrefix {
                family: "AWS access key"
            })
        );
    }

    #[test]
    fn secret_past_the_size_cap_is_not_scanned() {
        // Fill past MAX_SCAN_BYTES with safe bytes, then put a token beyond the
        // cap: it is outside the scanned window, so it is not flagged.
        let mut content = vec![b'x'; MAX_SCAN_BYTES];
        content.extend_from_slice(b"\nAKIAIOSFODNN7EXAMPLE\n");
        assert_eq!(content_reason("huge.txt", &content), None);
    }

    #[test]
    fn secret_within_the_size_cap_is_scanned() {
        // The same token just inside the cap is caught, proving the file is
        // scanned (capped), not skipped wholesale.
        let mut content = b"AKIAIOSFODNN7EXAMPLE\n".to_vec();
        content.resize(MAX_SCAN_BYTES + 5000, b'x');
        assert_eq!(
            content_reason("huge.txt", &content),
            Some(SecretReason::TokenPrefix {
                family: "AWS access key"
            })
        );
    }

    // --- filename patterns --------------------------------------------------

    #[test]
    fn builtin_catalog_filename_is_matched() {
        let hit = scanner().scan_path(Path::new("id_rsa")).unwrap();
        match hit.reason {
            SecretReason::FilenamePattern { .. } => {}
            other => panic!("expected FilenamePattern, got {other:?}"),
        }
        // A nested .env is caught too (non-anchored pattern).
        assert!(scanner().scan_path(Path::new("sub/dir/.env")).is_some());
    }

    #[test]
    fn extra_pattern_is_matched() {
        let scanner = SecretScanner::compile(true, &["*.secret".to_string()]).unwrap();
        let hit = scanner.scan_path(Path::new("prod.secret")).unwrap();
        assert_eq!(
            hit.reason,
            SecretReason::FilenamePattern {
                pattern: "*.secret".to_string()
            }
        );
        // A file matching neither catalog nor extra is not flagged.
        assert!(scanner.scan_path(Path::new("main.rs")).is_none());
    }

    #[test]
    fn invalid_extra_pattern_is_a_compile_error() {
        // A dangling backslash is invalid glob syntax and must surface as a
        // compile error naming the offending pattern.
        let err = SecretScanner::compile(true, &["bad\\".to_string()]).unwrap_err();
        assert_eq!(err.pattern, "bad\\");
        assert!(err.to_string().contains("bad\\"), "got: {err}");
    }

    #[test]
    fn disabled_scanner_flags_nothing() {
        let scanner = SecretScanner::compile(false, &[]).unwrap();
        assert!(scanner.scan_path(Path::new("id_rsa")).is_none());
        assert!(
            scanner
                .scan_content(Path::new("creds"), b"AKIAIOSFODNN7EXAMPLE\n")
                .is_none()
        );
        assert!(!scanner.is_enabled());
    }
}
