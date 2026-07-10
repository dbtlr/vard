//! vard's default git excludes, written into a watched repo's private exclude
//! file (`info/exclude`).
//!
//! When `vard watch add` registers a repository, it seeds that repo's
//! *private* exclude file — `info/exclude`, never the user's tracked
//! `.gitignore` — with a curated set of patterns: build output and caches that
//! would bloat snapshots, OS cruft, and well-known secret shapes that must never
//! be committed. The pattern *catalog* is shared with the correctness crate
//! ([`vard_core::excludes`]) so the future daemon-side quarantine consumes the
//! same list; this module owns only how they render into a file.
//!
//! The patterns live inside a marked block so the write is idempotent: a re-add
//! rewrites only vard's block and leaves every line a user added above or below
//! it untouched, and it preserves the file's existing line endings (a CRLF
//! exclude file stays CRLF).

use std::fs;
use std::io;
use std::path::Path;

use vard_core::excludes::{BUILD_CACHE_PATTERNS, OS_CRUFT_PATTERNS, SECRET_PATTERNS};

/// Opening marker of vard's managed block. A line equal to this (trimmed) marks
/// where the block begins.
const BLOCK_BEGIN: &str = "# >>> vard managed excludes >>>";
/// Closing marker of vard's managed block.
const BLOCK_END: &str = "# <<< vard managed excludes <<<";

/// What [`ensure`] did to the exclude file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExcludeOutcome {
    /// The managed block was created or refreshed.
    Written,
    /// The file already carried an identical managed block; nothing changed.
    Unchanged,
}

/// Renders vard's managed exclude block (markers included) with `\n` line
/// endings, always ending in a newline. [`splice_block`] rewrites the endings
/// to match the target file. The patterns come from the shared
/// [`vard_core::excludes`] catalog; the comment headers are presentation.
fn render_block() -> String {
    let mut s = String::new();
    s.push_str(BLOCK_BEGIN);
    s.push('\n');
    s.push_str(
        "# Managed by `vard watch add`; edit above or below this block to keep your rules.\n",
    );
    let groups: &[(&str, &[&str])] = &[
        (
            "# Secrets — never snapshot credentials, keys, or environment files.",
            SECRET_PATTERNS,
        ),
        (
            "# Dependency, build, and cache directories.",
            BUILD_CACHE_PATTERNS,
        ),
        ("# OS and editor cruft.", OS_CRUFT_PATTERNS),
    ];
    for (heading, patterns) in groups {
        s.push_str(heading);
        s.push('\n');
        for pattern in *patterns {
            s.push_str(pattern);
            s.push('\n');
        }
    }
    s.push_str(BLOCK_END);
    s.push('\n');
    s
}

/// Ensures the repository's `info/exclude` file (resolved by the caller via the
/// git backend, so it is correct for worktrees and submodules) carries vard's
/// managed block, idempotently.
///
/// If the file has no managed block, the block is appended (after the user's
/// existing content, separated by a blank line). If it already has one, that
/// block is replaced in place — content outside the markers is preserved
/// verbatim, and the file's dominant line ending is preserved. A re-add with
/// the same defaults is a no-op ([`ExcludeOutcome::Unchanged`]), including on a
/// CRLF file.
pub(crate) fn ensure(exclude_path: &Path) -> io::Result<ExcludeOutcome> {
    if let Some(parent) = exclude_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let existing = match fs::read_to_string(exclude_path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    let updated = splice_block(&existing, &render_block());
    if updated == existing {
        return Ok(ExcludeOutcome::Unchanged);
    }
    fs::write(exclude_path, updated)?;
    Ok(ExcludeOutcome::Written)
}

/// Splices `block` into `existing`, replacing any current managed block or
/// appending a fresh one. Pure and line-oriented, so it is exhaustively
/// testable without touching a filesystem.
///
/// Line endings are preserved: the block is re-lined to the file's dominant
/// ending (CRLF if the file predominantly uses it, else LF), and lines outside
/// the block keep their original endings verbatim. Marker detection trims a
/// trailing `\r` so a CRLF file's markers still match. This makes a re-add on a
/// CRLF file byte-identical rather than silently rewriting it to LF.
fn splice_block(existing: &str, block: &str) -> String {
    let ending = if is_crlf_dominant(existing) {
        "\r\n"
    } else {
        "\n"
    };
    let block = relined(block, ending);

    // `split_inclusive` keeps each line's own terminator, so lines outside the
    // block round-trip unchanged.
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    let is_marker = |line: &str, marker: &str| line.trim_end_matches(['\r', '\n']).trim() == marker;
    let begin = lines.iter().position(|l| is_marker(l, BLOCK_BEGIN));
    let end = lines.iter().position(|l| is_marker(l, BLOCK_END));

    if let (Some(b), Some(e)) = (begin, end)
        && b <= e
    {
        // Replace the existing block in place, preserving lines around it.
        let mut out = String::new();
        for line in &lines[..b] {
            out.push_str(line);
        }
        out.push_str(&block);
        for line in &lines[e + 1..] {
            out.push_str(line);
        }
        return out;
    }

    // No managed block: append, keeping the user's content and a blank spacer.
    let mut out = existing.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push_str(ending);
        }
        out.push_str(ending);
    }
    out.push_str(&block);
    out
}

/// Whether `text` predominantly uses CRLF line endings. An empty or LF-only
/// file is not CRLF; ties (equal counts) favor CRLF so a mixed file that a
/// CRLF editor produced stays CRLF.
fn is_crlf_dominant(text: &str) -> bool {
    let total = text.matches('\n').count();
    if total == 0 {
        return false;
    }
    let crlf = text.matches("\r\n").count();
    crlf * 2 >= total
}

/// Rewrites `block`'s `\n` line endings to `ending`. A no-op when `ending` is
/// already `\n`. `block` never contains a bare `\r`, so this cannot double up.
fn relined(block: &str, ending: &str) -> String {
    if ending == "\n" {
        block.to_string()
    } else {
        block.replace('\n', ending)
    }
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
        let exclude = dir.path().join(".git/info/exclude");

        assert_eq!(ensure(&exclude).unwrap(), ExcludeOutcome::Written);
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.contains("target/"));

        assert_eq!(
            ensure(&exclude).unwrap(),
            ExcludeOutcome::Unchanged,
            "a second ensure with identical defaults must not rewrite"
        );
        // No duplication of the block on the second pass.
        let content2 = fs::read_to_string(&exclude).unwrap();
        assert_eq!(content2.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn ensure_preserves_preexisting_exclude_lines() {
        let dir = tempfile::tempdir().unwrap();
        let exclude = dir.path().join(".git/info/exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, "# git's default\n*.tmp\n").unwrap();

        ensure(&exclude).unwrap();
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.contains("*.tmp"), "user's line must survive");
        assert!(content.contains(BLOCK_BEGIN));
    }

    #[test]
    fn ensure_is_byte_idempotent_on_a_crlf_file() {
        let dir = tempfile::tempdir().unwrap();
        let exclude = dir.path().join(".git/info/exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        // A CRLF file, as a Windows editor would produce.
        fs::write(&exclude, "# my rules\r\n*.tmp\r\n").unwrap();

        assert_eq!(ensure(&exclude).unwrap(), ExcludeOutcome::Written);
        let once = fs::read(&exclude).unwrap();
        // The managed block must have been written with CRLF endings, not LF.
        let text = String::from_utf8(once.clone()).unwrap();
        assert!(text.contains("\r\n# >>> vard managed excludes >>>\r\n"));
        assert!(
            !text.contains("\n\n"),
            "no bare-LF blank line on a CRLF file"
        );
        // The user's CRLF lines survive verbatim.
        assert!(text.contains("# my rules\r\n*.tmp\r\n"));

        // A re-add is a true no-op: byte-identical, reported Unchanged.
        assert_eq!(ensure(&exclude).unwrap(), ExcludeOutcome::Unchanged);
        let twice = fs::read(&exclude).unwrap();
        assert_eq!(once, twice, "re-add on a CRLF file must be byte-identical");
    }
}
