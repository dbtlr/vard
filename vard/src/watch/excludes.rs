//! vard's default git excludes, written into a watched repo's
//! `.git/info/exclude`.
//!
//! When `vard watch add` registers a repository, it seeds that repo's
//! *private* exclude file — `.git/info/exclude`, never the user's tracked
//! `.gitignore` — with a curated set of patterns: build output and caches that
//! would bloat snapshots, OS cruft, and well-known secret shapes that must never
//! be committed. The patterns live inside a marked block so the write is
//! idempotent: a re-add rewrites only vard's block and leaves every line a user
//! added above or below it untouched.

use std::fs;
use std::io;
use std::path::Path;

/// Opening marker of vard's managed block. A line equal to this (trimmed) marks
/// where the block begins.
const BLOCK_BEGIN: &str = "# >>> vard managed excludes >>>";
/// Closing marker of vard's managed block.
const BLOCK_END: &str = "# <<< vard managed excludes <<<";

/// The patterns vard writes, in gitignore dialect. Comment lines document the
/// groups; blank-line-free so the rendered block stays compact. Secrets come
/// first so they are visually prominent.
const DEFAULT_EXCLUDES: &[&str] = &[
    "# Secrets — never snapshot credentials, keys, or environment files.",
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
    "# Dependency, build, and cache directories.",
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    ".cache/",
    "__pycache__/",
    ".venv/",
    "venv/",
    "# OS and editor cruft.",
    ".DS_Store",
    "Thumbs.db",
];

/// What [`ensure`] did to the exclude file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExcludeOutcome {
    /// The managed block was created or refreshed.
    Written,
    /// The file already carried an identical managed block; nothing changed.
    Unchanged,
}

/// Renders vard's managed exclude block (markers included), always ending in a
/// newline.
fn render_block() -> String {
    let mut s = String::new();
    s.push_str(BLOCK_BEGIN);
    s.push('\n');
    s.push_str(
        "# Managed by `vard watch add`; edit above or below this block to keep your rules.\n",
    );
    for line in DEFAULT_EXCLUDES {
        s.push_str(line);
        s.push('\n');
    }
    s.push_str(BLOCK_END);
    s.push('\n');
    s
}

/// Ensures `<repo_path>/.git/info/exclude` carries vard's managed block,
/// idempotently.
///
/// If the file has no managed block, the block is appended (after the user's
/// existing content, separated by a blank line). If it already has one, that
/// block is replaced in place — content outside the markers is preserved
/// verbatim. A re-add with the same defaults is a no-op ([`ExcludeOutcome::Unchanged`]).
pub(crate) fn ensure(repo_path: &Path) -> io::Result<ExcludeOutcome> {
    let info_dir = repo_path.join(".git").join("info");
    fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");

    let existing = match fs::read_to_string(&exclude_path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    let updated = splice_block(&existing, &render_block());
    if updated == existing {
        return Ok(ExcludeOutcome::Unchanged);
    }
    fs::write(&exclude_path, updated)?;
    Ok(ExcludeOutcome::Written)
}

/// Splices `block` into `existing`, replacing any current managed block or
/// appending a fresh one. Pure and line-oriented, so it is exhaustively
/// testable without touching a filesystem.
fn splice_block(existing: &str, block: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let begin = lines.iter().position(|l| l.trim() == BLOCK_BEGIN);
    let end = lines.iter().position(|l| l.trim() == BLOCK_END);

    if let (Some(b), Some(e)) = (begin, end)
        && b <= e
    {
        // Replace the existing block in place, preserving lines around it.
        let mut out = String::new();
        for line in &lines[..b] {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(block);
        for line in &lines[e + 1..] {
            out.push_str(line);
            out.push('\n');
        }
        return out;
    }

    // No managed block: append, keeping the user's content and a blank spacer.
    let mut out = existing.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(block);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_block_to_empty_file() {
        let result = splice_block("", &render_block());
        assert!(result.contains(BLOCK_BEGIN));
        assert!(result.contains(BLOCK_END));
        assert!(result.contains("node_modules/"));
        assert!(result.contains(".env"));
        assert!(result.contains("*.pem"));
    }

    #[test]
    fn appends_after_existing_user_content() {
        let existing = "# my rules\nsecret.txt\n";
        let result = splice_block(existing, &render_block());
        assert!(result.starts_with("# my rules\nsecret.txt\n\n"));
        assert!(result.contains(BLOCK_BEGIN));
        assert!(result.contains("secret.txt"));
    }

    #[test]
    fn re_splicing_is_idempotent() {
        let once = splice_block("", &render_block());
        let twice = splice_block(&once, &render_block());
        assert_eq!(
            once, twice,
            "splicing an already-managed file must be a no-op"
        );
    }

    #[test]
    fn replaces_block_in_place_preserving_surrounding_lines() {
        let existing =
            format!("keep-before.txt\n{BLOCK_BEGIN}\nstale-line\n{BLOCK_END}\nkeep-after.txt\n");
        let result = splice_block(&existing, &render_block());
        assert!(result.contains("keep-before.txt"));
        assert!(result.contains("keep-after.txt"));
        assert!(!result.contains("stale-line"), "old block must be gone");
        assert!(result.contains("node_modules/"));
    }

    #[test]
    fn ensure_writes_then_reports_unchanged_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(ensure(repo).unwrap(), ExcludeOutcome::Written);
        let content = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(content.contains("target/"));

        assert_eq!(
            ensure(repo).unwrap(),
            ExcludeOutcome::Unchanged,
            "a second ensure with identical defaults must not rewrite"
        );
        // No duplication of the block on the second pass.
        let content2 = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(content2.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn ensure_preserves_preexisting_exclude_lines() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let info = repo.join(".git/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("exclude"), "# git's default\n*.tmp\n").unwrap();

        ensure(repo).unwrap();
        let content = fs::read_to_string(info.join("exclude")).unwrap();
        assert!(content.contains("*.tmp"), "user's line must survive");
        assert!(content.contains(BLOCK_BEGIN));
    }
}
