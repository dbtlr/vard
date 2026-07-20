//! Atomic binary swap via `rename(2)`.

use std::fs;
use std::path::Path;

/// Replaces `dest` with `new_binary` atomically, within a single filesystem.
/// The caller places `new_binary` next to `dest` (see
/// [`sibling_temp_path`](super::download::sibling_temp_path)) so the rename
/// never crosses a filesystem boundary. The running process keeps executing from
/// the old inode until it exits — expected and safe.
pub(crate) fn swap(new_binary: &Path, dest: &Path) -> Result<(), String> {
    fs::rename(new_binary, dest).map_err(|e| {
        format!(
            "replacing {} with {}: {e}",
            dest.display(),
            new_binary.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_replaces_dest_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("vard");
        let new = tmp.path().join(".vard-self-update-new");
        fs::write(&dest, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        swap(&new, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(!new.exists(), "the staged binary was moved, not copied");
    }
}
