//! Comment-preserving, atomic mutation of `config.toml`.
//!
//! The [read layer](crate::config) parses `config.toml` into a typed, validated
//! [`Config`](crate::config::Config). This module is its write counterpart: the
//! `vard watch add/remove/pause/resume` commands edit the file *in place*
//! through [`toml_edit`], so a user's comments, key ordering, and formatting
//! survive a mutation untouched — only the bytes that must change, change.
//!
//! # The atomic-write contract
//!
//! The running daemon watches `config.toml` for edits (mtime polling; see the
//! [`daemon`](crate::daemon) module docs) and reloads on change. A half-written
//! file must therefore never be observable. Every mutation is committed the same
//! way the daemon's own request files are: serialize to a temporary file in the
//! *same directory*, then [`rename(2)`] it into place — atomic on POSIX, so the
//! daemon sees either the old file or the new one, never a torn write.
//!
//! [`rename(2)`]: https://man7.org/linux/man-pages/man2/rename.2.html
//!
//! # Injected paths
//!
//! Like the read layer, every entry point takes an explicit config-file path so
//! tests operate entirely inside a tempdir; the thin XDG wrapper lives in
//! [`paths`](crate::paths).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::config::SUPPORTED_VERSION;

/// The TOML key under which watches are stored (`[[watch]]`). Matches the read
/// layer's `#[serde(rename = "watch")]`.
const WATCH_KEY: &str = "watch";

/// The fields a `vard watch add` may write into a `[[watch]]` table. Only the
/// fields the user explicitly set are `Some`; everything else is left to inherit
/// from `[defaults]` and the core constants, keeping the file minimal.
#[derive(Debug, Default, Clone)]
pub(crate) struct WatchEntry {
    /// The watch's stable name (required).
    pub name: String,
    /// The canonicalized path the watch covers (required).
    pub path: String,
    /// Explicit `branch`, when `--branch` was given.
    pub branch: Option<String>,
    /// Explicit `remote`, when `--remote` was given.
    pub remote: Option<String>,
    /// Explicit `trigger`, when `--trigger` was given.
    pub trigger: Option<String>,
    /// Explicit `interval` humantime string, when `--interval` was given.
    pub interval: Option<String>,
    /// Explicit `quiesce` humantime string, when `--quiesce` was given.
    pub quiesce: Option<String>,
    /// Whether `--no-sync` was given: writes `sync = false`. `false` leaves the
    /// key unset so the watch inherits the default.
    pub no_sync: bool,
}

/// Reads and parses `path` into an editable document, preserving all formatting.
///
/// Returns `Ok(None)` when the file does not exist (the caller starts from
/// [`new_document`]); `Ok(Some(doc))` when it parses; and an error when the file
/// exists but cannot be read or is not valid TOML.
pub(crate) fn load_document(path: &Path) -> Result<Option<DocumentMut>, EditError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(EditError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let doc = text.parse::<DocumentMut>().map_err(|e| EditError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(Some(doc))
}

/// A fresh document seeded with `version = <SUPPORTED_VERSION>`, for the first
/// `vard watch add` in a config-less environment.
pub(crate) fn new_document() -> DocumentMut {
    let mut doc = DocumentMut::new();
    doc["version"] = value(SUPPORTED_VERSION);
    doc
}

/// The `[[watch]]` array-of-tables, created empty if absent. Coerces a
/// conflicting non-array `watch` key into an array-of-tables — the read layer
/// would already have rejected such a file, so this only fires on a document the
/// caller built itself.
fn watch_tables_mut(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    let item = doc
        .entry(WATCH_KEY)
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    if !item.is_array_of_tables() {
        *item = Item::ArrayOfTables(ArrayOfTables::new());
    }
    item.as_array_of_tables_mut()
        .expect("watch key coerced to an array-of-tables above")
}

/// Appends a new `[[watch]]` table built from `entry`. Only the fields the user
/// set are written, so inheritance from `[defaults]` stays intact.
pub(crate) fn append_watch(doc: &mut DocumentMut, entry: &WatchEntry) {
    let mut table = Table::new();
    table["name"] = value(entry.name.clone());
    table["path"] = value(entry.path.clone());
    apply_optional_fields(&mut table, entry);
    watch_tables_mut(doc).push(table);
}

/// Relinks the watch at `index` to a new path and updates any explicitly-set
/// optional fields, leaving unmentioned fields (and the whole rest of the file)
/// untouched. This is the moved-directory / re-add path.
pub(crate) fn update_watch(doc: &mut DocumentMut, index: usize, entry: &WatchEntry) {
    let table = watch_tables_mut(doc)
        .get_mut(index)
        .expect("caller resolved a valid watch index");
    table["path"] = value(entry.path.clone());
    apply_optional_fields(table, entry);
}

/// Writes the explicitly-set optional fields of `entry` into `table`. A field
/// left `None` is not touched — on re-add this preserves whatever the user had.
fn apply_optional_fields(table: &mut Table, entry: &WatchEntry) {
    if let Some(branch) = &entry.branch {
        table["branch"] = value(branch.clone());
    }
    if let Some(remote) = &entry.remote {
        table["remote"] = value(remote.clone());
    }
    if let Some(trigger) = &entry.trigger {
        table["trigger"] = value(trigger.clone());
    }
    if let Some(interval) = &entry.interval {
        table["interval"] = value(interval.clone());
    }
    if let Some(quiesce) = &entry.quiesce {
        table["quiesce"] = value(quiesce.clone());
    }
    if entry.no_sync {
        table["sync"] = value(false);
    }
}

/// Removes the `[[watch]]` at `index`.
pub(crate) fn remove_watch(doc: &mut DocumentMut, index: usize) {
    watch_tables_mut(doc).remove(index);
}

/// Sets or clears the `paused` flag on the watch at `index`. Pausing writes
/// `paused = true`; resuming removes the key entirely so a resumed watch is
/// byte-for-byte a never-paused one — the file stays minimal.
pub(crate) fn set_paused(doc: &mut DocumentMut, index: usize, paused: bool) {
    let table = watch_tables_mut(doc)
        .get_mut(index)
        .expect("caller resolved a valid watch index");
    if paused {
        table["paused"] = value(true);
    } else {
        table.remove("paused");
    }
}

/// Serializes `doc` and installs it at `path` atomically: write to a temporary
/// file in the same directory, then `rename(2)` it into place. The daemon, which
/// watches this file, therefore never observes a partial write.
pub(crate) fn write_atomic(path: &Path, doc: &DocumentMut) -> Result<(), EditError> {
    let io_err = |source| EditError::Io {
        path: path.to_path_buf(),
        source,
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(io_err)?;

    // Temp name in the same directory so the rename is same-filesystem (hence
    // atomic). The leading dot and pid keep concurrent writers from colliding.
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));

    let text = doc.to_string();
    if let Err(source) = fs::write(&tmp, text.as_bytes()) {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(source));
    }
    if let Err(source) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(source));
    }
    Ok(())
}

/// Everything that can go wrong reading or writing the config for a mutation.
#[derive(Debug)]
pub(crate) enum EditError {
    /// An I/O error reading or writing the config file.
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The existing config file is not valid TOML, so it cannot be edited
    /// without risking data loss.
    Parse {
        /// The config path.
        path: PathBuf,
        /// The parser's message.
        message: String,
    },
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::Io { path, source } => {
                write!(f, "editing config {}: {source}", path.display())
            }
            EditError::Parse { path, message } => {
                write!(f, "config {} is not valid TOML: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for EditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EditError::Io { source, .. } => Some(source),
            EditError::Parse { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(name: &str, path: &str) -> WatchEntry {
        WatchEntry {
            name: name.to_string(),
            path: path.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn append_to_new_document_seeds_version() {
        let mut doc = new_document();
        append_watch(&mut doc, &sample_entry("notes", "/home/u/notes"));
        let text = doc.to_string();
        assert!(text.contains("version = 1"), "got: {text}");
        assert!(text.contains("[[watch]]"), "got: {text}");
        assert!(text.contains("name = \"notes\""), "got: {text}");
        assert!(text.contains("path = \"/home/u/notes\""), "got: {text}");
    }

    #[test]
    fn append_writes_only_set_optional_fields() {
        let mut doc = new_document();
        let entry = WatchEntry {
            name: "proj".to_string(),
            path: "/p".to_string(),
            branch: Some("backup".to_string()),
            no_sync: true,
            ..Default::default()
        };
        append_watch(&mut doc, &entry);
        let text = doc.to_string();
        assert!(text.contains("branch = \"backup\""), "got: {text}");
        assert!(text.contains("sync = false"), "got: {text}");
        // Fields left unset must not appear.
        assert!(!text.contains("remote"), "got: {text}");
        assert!(!text.contains("trigger"), "got: {text}");
        assert!(!text.contains("interval"), "got: {text}");
    }

    #[test]
    fn editing_preserves_comments_and_formatting() {
        let original = r#"version = 1

# my important defaults
[defaults]
trigger = "both"  # inline comment kept

[[watch]]
name = "notes"
path = "/home/u/notes"
"#;
        let mut doc = original.parse::<DocumentMut>().unwrap();
        append_watch(&mut doc, &sample_entry("project", "/home/u/project"));
        let text = doc.to_string();
        assert!(text.contains("# my important defaults"), "got: {text}");
        assert!(text.contains("# inline comment kept"), "got: {text}");
        assert!(text.contains("name = \"project\""), "got: {text}");
        // The original watch is still present.
        assert!(text.contains("name = \"notes\""), "got: {text}");
    }

    #[test]
    fn set_paused_then_resume_round_trips_to_original() {
        let original = "version = 1\n\n[[watch]]\nname = \"w\"\npath = \"/p\"\n";
        let mut doc = original.parse::<DocumentMut>().unwrap();
        set_paused(&mut doc, 0, true);
        assert!(doc.to_string().contains("paused = true"));
        set_paused(&mut doc, 0, false);
        assert_eq!(
            doc.to_string(),
            original,
            "resuming must remove the paused key, restoring the original bytes"
        );
    }

    #[test]
    fn remove_watch_drops_only_that_table() {
        let original = "version = 1\n\n[[watch]]\nname = \"a\"\npath = \"/a\"\n\n[[watch]]\nname = \"b\"\npath = \"/b\"\n";
        let mut doc = original.parse::<DocumentMut>().unwrap();
        remove_watch(&mut doc, 0);
        let text = doc.to_string();
        assert!(!text.contains("name = \"a\""), "got: {text}");
        assert!(text.contains("name = \"b\""), "got: {text}");
    }

    #[test]
    fn update_watch_relinks_path_and_updates_set_fields() {
        let original =
            "version = 1\n\n[[watch]]\nname = \"w\"\npath = \"/old\"\nremote = \"keep\"\n";
        let mut doc = original.parse::<DocumentMut>().unwrap();
        let entry = WatchEntry {
            name: "w".to_string(),
            path: "/new".to_string(),
            branch: Some("b".to_string()),
            ..Default::default()
        };
        update_watch(&mut doc, 0, &entry);
        let text = doc.to_string();
        assert!(text.contains("path = \"/new\""), "got: {text}");
        assert!(text.contains("branch = \"b\""), "got: {text}");
        // An unmentioned existing field is preserved.
        assert!(text.contains("remote = \"keep\""), "got: {text}");
    }

    #[test]
    fn write_atomic_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut doc = new_document();
        append_watch(&mut doc, &sample_entry("notes", "/home/u/notes"));
        write_atomic(&path, &doc).unwrap();

        let back = fs::read_to_string(&path).unwrap();
        assert_eq!(back, doc.to_string());
        // No temp files linger.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn load_document_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        assert!(load_document(&path).unwrap().is_none());
    }

    #[test]
    fn load_document_invalid_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is = = not toml").unwrap();
        assert!(matches!(load_document(&path), Err(EditError::Parse { .. })));
    }
}
