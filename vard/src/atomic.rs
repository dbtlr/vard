//! One durable atomic-write primitive, shared by every file the daemon may
//! observe mid-write: `config.toml` (via [`config_edit`](crate::config_edit))
//! and the request-file queue (via [`request`](crate::request)).
//!
//! # The recipe
//!
//! Write the bytes to a temporary file in the *same directory*, `fsync` it,
//! `rename(2)` it into place (atomic on POSIX), then `fsync` the parent
//! directory so the rename itself is durable. A reader therefore sees either
//! the old file or the whole new one, never a torn write, and a crash
//! immediately after cannot leave a truncated or lost file.
//!
//! When the target is a symlink it is resolved first, so the rename replaces
//! the real file the link points at, leaving the link itself intact; a
//! not-yet-existing target resolves to itself.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Atomically and durably installs `bytes` at `path` (see the [module
/// docs](self) for the recipe). Returns the underlying I/O error on failure,
/// having removed any temporary file it created.
pub(crate) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Resolve through a symlink so we replace the real target (preserving the
    // link); a missing file has no link to preserve and resolves to itself.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    // Temp name in the same directory so the rename is same-filesystem (hence
    // atomic). The leading dot plus pid keeps concurrent writers from colliding
    // and keeps the temp out of any `*.toml` glob a reader scans.
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "atomic".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));

    if let Err(source) = write_and_sync(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(source);
    }
    if let Err(source) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(source);
    }
    // fsync the directory so the rename itself is durable. Best-effort: some
    // filesystems reject an fsync on a directory, which does not make the write
    // any less committed than the file fsync already made it.
    if let Ok(dir_file) = File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Whether `name` matches the temporary-file scheme [`write()`](fn@write) uses:
/// `.{final-name}.tmp-{pid}` — a leading dot, an embedded `.tmp-`, and an
/// all-digit pid suffix. A crashed atomic write is the only thing that strands
/// such a name, so a directory scanner (e.g. `vard doctor`) can single these out
/// as its own safe-to-delete leftovers without touching files vard never wrote.
pub(crate) fn is_temp_name(name: &str) -> bool {
    name.starts_with('.')
        && name.rsplit_once(".tmp-").is_some_and(|(prefix, pid)| {
            !prefix.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Writes `bytes` to `path` and `fsync`s the file before returning, so its
/// contents are on stable storage before the caller renames it into place.
fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.toml");
        write(&path, b"hello\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn overwrites_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.toml");
        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn recognizes_only_its_own_temp_scheme() {
        // Exactly the names `write` produces: `.{final}.tmp-{pid}`.
        assert!(is_temp_name(".req-99-1700000000.toml.tmp-4242"));
        assert!(is_temp_name(".config.toml.tmp-1"));
        // Not our scheme: no leading dot, no `.tmp-`, or a non-numeric pid.
        assert!(!is_temp_name("req-1.toml"));
        assert!(!is_temp_name(".req-1.toml"));
        assert!(!is_temp_name("req.toml.tmp-1")); // missing the leading dot
        assert!(!is_temp_name(".req.toml.tmp-")); // empty pid
        assert!(!is_temp_name(".req.toml.tmp-abc")); // non-digit pid
        assert!(!is_temp_name(".tmp-123")); // no final-name prefix
    }
}
