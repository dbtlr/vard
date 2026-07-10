//! Comment-preserving, atomic mutation of `config.toml`.
//!
//! The [read layer](crate::config) parses `config.toml` into a typed, validated
//! [`Config`]. This module is its write counterpart: the
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
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value, value};

use crate::command::CmdError;
use crate::config::{Config, ConfigError, SUPPORTED_VERSION};

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

/// Reads and parses `path` into an editable document, preserving all
/// formatting, and verifies the `watch` key (if present) is in a shape the
/// comment-preserving editor can safely mutate.
///
/// Returns `Ok(None)` when the file does not exist (the caller starts from
/// [`new_document`]); `Ok(Some(doc))` when it parses; an error when the file
/// exists but cannot be read or is not valid TOML; and
/// [`EditError::WatchNotArrayOfTables`] when `watch` is present but not an
/// array-of-`[[watch]]`-tables — an inline `watch = [{...}]` (or `watch = []`)
/// the read layer tolerates but the editor cannot restructure without risking
/// the user's formatting. Refusing is safer than coercing (which would drop
/// every inline watch) or blindly indexing (which would panic on a stale
/// index).
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
    if let Some(item) = doc.get(WATCH_KEY)
        && !item.is_array_of_tables()
    {
        return Err(EditError::WatchNotArrayOfTables {
            path: path.to_path_buf(),
        });
    }
    Ok(Some(doc))
}

/// A fresh document seeded with `version = <SUPPORTED_VERSION>`, for the first
/// `vard watch add` in a config-less environment.
pub(crate) fn new_document() -> DocumentMut {
    let mut doc = DocumentMut::new();
    doc["version"] = value(SUPPORTED_VERSION);
    doc
}

/// The `[[watch]]` array-of-tables, created empty if absent.
///
/// Never coerces: [`load_document`] has already rejected a non-array `watch`
/// key, and [`new_document`] has no `watch` key at all, so `or_insert_with`
/// either finds an existing array-of-tables or creates a fresh empty one. The
/// `expect` documents that precondition; it cannot fire for a document produced
/// by this module's constructors.
fn watch_tables_mut(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    let item = doc
        .entry(WATCH_KEY)
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    item.as_array_of_tables_mut()
        .expect("load_document guarantees `watch` is an array-of-tables or absent")
}

/// The index of the `[[watch]]` named `name` (compared case-insensitively, as
/// the read layer compares names), relocated *inside the given document* so an
/// index computed from a different parse can never be used against it.
fn watch_index(tables: &ArrayOfTables, name: &str) -> Option<usize> {
    tables.iter().position(|table| {
        table
            .get("name")
            .and_then(Item::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
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

/// Relinks the watch named `entry.name` to a new path and updates any
/// explicitly-set optional fields, leaving unmentioned fields (and the whole
/// rest of the file) untouched. This is the moved-directory / re-add path.
///
/// Returns `false` when no watch by that name is present in the document (it
/// vanished between planning and mutating) so the caller can fall back to an
/// append rather than panic on a stale index.
pub(crate) fn update_watch(doc: &mut DocumentMut, entry: &WatchEntry) -> bool {
    let tables = watch_tables_mut(doc);
    let Some(index) = watch_index(tables, &entry.name) else {
        return false;
    };
    let table = tables
        .get_mut(index)
        .expect("index just located in this document");
    table["path"] = value(entry.path.clone());
    apply_optional_fields(table, entry);
    true
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

/// Removes the `[[watch]]` named `name`, relocating it inside the document.
/// Returns `false` when no watch by that name is present (already gone).
pub(crate) fn remove_watch(doc: &mut DocumentMut, name: &str) -> bool {
    let tables = watch_tables_mut(doc);
    let Some(index) = watch_index(tables, name) else {
        return false;
    };
    tables.remove(index);
    true
}

/// Sets or clears the `paused` flag on the watch named `name`, relocating it
/// inside the document. Pausing writes `paused = true`; resuming removes the key
/// entirely so a resumed watch is byte-for-byte a never-paused one — the file
/// stays minimal. Returns `false` when no watch by that name is present.
pub(crate) fn set_paused(doc: &mut DocumentMut, name: &str, paused: bool) -> bool {
    let tables = watch_tables_mut(doc);
    let Some(index) = watch_index(tables, name) else {
        return false;
    };
    let table = tables
        .get_mut(index)
        .expect("index just located in this document");
    if paused {
        table["paused"] = value(true);
    } else {
        table.remove("paused");
    }
    true
}

// --- generic dotted scalar keys (`vard config get/set/unset`) --------------

/// Reads the scalar value addressed by a dotted `key` (e.g. `daemon.log_level`),
/// as a display string. `None` when the path does not resolve, and
/// `Some(Err(..))` — a present-but-not-scalar answer — when the key names a
/// table or array rather than a settable scalar. Only the value the file
/// actually holds is returned; inherited defaults are not materialized here.
pub(crate) fn get_dotted(doc: &DocumentMut, key: &str) -> Option<Result<String, KeyNotScalar>> {
    let item = item_at(doc, key)?;
    Some(match item.as_value().and_then(scalar_to_string) {
        Some(text) => Ok(text),
        None => Err(KeyNotScalar),
    })
}

/// The addressed key exists but names a table or array, not a scalar value.
pub(crate) struct KeyNotScalar;

/// Sets the scalar `key` to `raw`, creating intermediate tables as needed and
/// preserving the rest of the document. The value's TOML type is inferred from
/// `raw` (see [`infer_value`]); correctness of the *typed* result is the
/// caller's to check via [`commit_document`]. Returns an error only when a path
/// segment already exists as a non-table (so it cannot be descended into).
pub(crate) fn set_dotted(doc: &mut DocumentMut, key: &str, raw: &str) -> Result<(), String> {
    let segments: Vec<&str> = key.split('.').collect();
    let (last, parents) = segments
        .split_last()
        .expect("split on a non-empty key yields at least one segment");
    let mut table: &mut Table = doc.as_table_mut();
    for seg in parents {
        let entry = table
            .entry(seg)
            .or_insert_with(|| Item::Table(Table::new()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| format!("config key {seg:?} is not a table"))?;
    }
    table[last] = value(infer_value(raw));
    Ok(())
}

/// Removes the scalar `key`, leaving any now-empty parent table in place (the
/// file stays otherwise byte-for-byte unchanged). Returns `false` when the key
/// was not present.
pub(crate) fn unset_dotted(doc: &mut DocumentMut, key: &str) -> bool {
    let segments: Vec<&str> = key.split('.').collect();
    let (last, parents) = segments
        .split_last()
        .expect("split on a non-empty key yields at least one segment");
    let mut table: &mut Table = doc.as_table_mut();
    for seg in parents {
        match table.get_mut(seg).and_then(Item::as_table_mut) {
            Some(next) => table = next,
            None => return false,
        }
    }
    table.remove(last).is_some()
}

/// Resolves a dotted path to the item it addresses, descending table-like nodes.
fn item_at<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    let mut segments = key.split('.');
    let mut item = doc.as_table().get(segments.next()?)?;
    for seg in segments {
        item = item.as_table_like()?.get(seg)?;
    }
    Some(item)
}

/// Renders a scalar [`Value`] to its display string (a bare string keeps its
/// contents, other scalars their TOML spelling). `None` for arrays and inline
/// tables, which are not settable scalars.
fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.value().clone()),
        Value::Integer(i) => Some(i.value().to_string()),
        Value::Float(f) => Some(f.value().to_string()),
        Value::Boolean(b) => Some(b.value().to_string()),
        Value::Datetime(d) => Some(d.value().to_string()),
        Value::Array(_) | Value::InlineTable(_) => None,
    }
}

/// Infers a TOML scalar from a command-line string: `true`/`false` a boolean, a
/// bare integer a number, everything else a string. The typed result is only a
/// candidate — schema validation ([`document_validity`]) decides whether it is
/// acceptable for the key, so this never needs a per-key type table.
fn infer_value(raw: &str) -> Value {
    if raw == "true" {
        Value::from(true)
    } else if raw == "false" {
        Value::from(false)
    } else if let Ok(n) = raw.parse::<i64>() {
        Value::from(n)
    } else {
        Value::from(raw)
    }
}

// --- validate-before-write (shared by `watch` and `config`) ----------------

impl From<EditError> for CmdError {
    fn from(e: EditError) -> Self {
        CmdError::err(e.to_string())
    }
}

/// Re-parses a serialized document through the read layer and resolves every
/// watch, returning the first validation error. Mirrors the daemon's
/// validate-before-swap discipline and subsumes per-field gaps (defaults
/// inheritance, duplicate names/paths). Paused watches are validated too (via
/// `resolve_all`).
pub(crate) fn document_validity(text: &str) -> Result<(), ConfigError> {
    Config::from_toml_str(text)?.resolve_all()?;
    Ok(())
}

/// Validates the exact bytes a mutation is about to write, then writes them —
/// applying the "never break a working config" invariant relative to the
/// config's validity *before* the edit (`pre_edit_invalid`):
///
/// * post-edit valid → write and succeed (a clean edit, or a repair that made an
///   invalid config valid again — either way, silently).
/// * pre-edit valid, post-edit invalid → refuse (exit 2): the CLI must never
///   turn a working config into a broken one that would wedge the daemon.
/// * pre-edit invalid, post-edit invalid → write anyway, warn, and exit 1
///   (attention): the config was already broken, so blocking an unrelated edit
///   would only trap the user — the natural repair path must be allowed.
///
/// Returns `Ok(None)` when written clean and `Ok(Some(attention))` when written
/// with the still-invalid warning — the caller must finish its post-write work
/// and then surface the carried attention, so a write that landed is never
/// reported as if it hadn't.
pub(crate) fn commit_document(
    doc: &DocumentMut,
    config_file: &Path,
    pre_edit_invalid: bool,
) -> Result<Option<CmdError>, CmdError> {
    let text = doc.to_string();
    match document_validity(&text) {
        Ok(()) => {
            write_atomic(config_file, doc)?;
            Ok(None)
        }
        Err(e) if pre_edit_invalid => {
            write_atomic(config_file, doc)?;
            Ok(Some(CmdError::attention(format!(
                "wrote {}, but the config is still not fully valid: {e}",
                config_file.display()
            ))))
        }
        Err(e) => Err(CmdError::err(format!(
            "refusing to write {}: the edit would make a valid config invalid: {e}",
            config_file.display()
        ))),
    }
}

/// Serializes `doc` and installs it at `path` through the shared durable
/// atomic-write recipe ([`atomic::write`](crate::atomic::write)): temp file,
/// `fsync`, `rename(2)`, directory `fsync`. The daemon, which watches this
/// file, therefore never observes a partial write, and a crash immediately
/// after cannot leave a truncated or lost config — the source of truth for
/// every watch. A symlinked `path` is resolved first so the link is preserved.
pub(crate) fn write_atomic(path: &Path, doc: &DocumentMut) -> Result<(), EditError> {
    crate::atomic::write(path, doc.to_string().as_bytes()).map_err(|source| EditError::Io {
        path: path.to_path_buf(),
        source,
    })
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
    /// The config's `watch` key is present but not an array-of-`[[watch]]`
    /// tables (an inline `watch = [{...}]` or `watch = []`). The
    /// comment-preserving editor cannot safely restructure it, so the mutation
    /// is refused rather than risking the user's formatting or losing watches.
    WatchNotArrayOfTables {
        /// The config path.
        path: PathBuf,
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
            EditError::WatchNotArrayOfTables { path } => write!(
                f,
                "config {} stores watches as an inline array; rewrite them as [[watch]] \
                 tables before using `vard watch` to edit it",
                path.display()
            ),
        }
    }
}

impl std::error::Error for EditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EditError::Io { source, .. } => Some(source),
            EditError::Parse { .. } | EditError::WatchNotArrayOfTables { .. } => None,
        }
    }
}

/// An exclusive advisory lock over `config.lock`, held for a whole
/// read→plan→mutate→rename cycle so concurrent `vard watch` invocations
/// serialize instead of racing.
///
/// It adapts the `flock` machinery the daemon uses for its single-instance lock
/// ([`crate::instance`]) — the kernel releases the lock when the descriptor
/// closes, so a crashed CLI never leaves a stale lock — but takes a *blocking*
/// exclusive lock rather than a non-blocking one: a second writer waits its turn
/// rather than failing. Combined with by-name relocation and pre-write
/// revalidation, this closes the lost-update and stale-index races between
/// concurrent mutators. The lock file is left on disk deliberately (removing it
/// would race a concurrent acquirer that already opened it).
pub(crate) struct ConfigLock {
    /// Held open purely to keep the `flock`; the drop that closes it releases
    /// the lock.
    _file: File,
}

impl ConfigLock {
    /// Acquires the writer lock for the config at `config_path`, blocking until
    /// any concurrent `vard watch` writer releases it. The lock file is
    /// `config.lock` beside the config (its directory is stable across
    /// invocations, so all writers contend on the same file).
    pub(crate) fn acquire(config_path: &Path) -> Result<ConfigLock, EditError> {
        let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        let lock_path = dir.join("config.lock");
        let io_err = |source| EditError::Io {
            path: lock_path.clone(),
            source,
        };
        fs::create_dir_all(dir).map_err(io_err)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io_err)?;
        // Blocking exclusive lock: a concurrent writer waits rather than failing.
        flock(&file, FlockOperation::LockExclusive).map_err(|errno| io_err(errno.into()))?;
        Ok(ConfigLock { _file: file })
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
        assert!(set_paused(&mut doc, "w", true));
        assert!(doc.to_string().contains("paused = true"));
        assert!(set_paused(&mut doc, "w", false));
        assert_eq!(
            doc.to_string(),
            original,
            "resuming must remove the paused key, restoring the original bytes"
        );
    }

    #[test]
    fn set_paused_relocates_by_name_not_index() {
        // The name is matched case-insensitively and located within the
        // document, so a case difference or reordering never mis-targets.
        let original = "version = 1\n\n[[watch]]\nname = \"a\"\npath = \"/a\"\n\n[[watch]]\nname = \"B\"\npath = \"/b\"\n";
        let mut doc = original.parse::<DocumentMut>().unwrap();
        assert!(set_paused(&mut doc, "b", true));
        let text = doc.to_string();
        // Only the second watch (matched case-insensitively) gained the flag.
        let b_block = text.split("name = \"B\"").nth(1).unwrap();
        assert!(b_block.contains("paused = true"), "got: {text}");
        // The first watch's block (everything before the B table) is untouched.
        let a_block = text.split("name = \"B\"").next().unwrap();
        assert!(!a_block.contains("paused"), "got: {text}");
    }

    #[test]
    fn set_paused_returns_false_for_a_vanished_watch() {
        let mut doc = "version = 1\n\n[[watch]]\nname = \"w\"\npath = \"/p\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(!set_paused(&mut doc, "ghost", true));
    }

    #[test]
    fn remove_watch_drops_only_that_table() {
        let original = "version = 1\n\n[[watch]]\nname = \"a\"\npath = \"/a\"\n\n[[watch]]\nname = \"b\"\npath = \"/b\"\n";
        let mut doc = original.parse::<DocumentMut>().unwrap();
        assert!(remove_watch(&mut doc, "a"));
        let text = doc.to_string();
        assert!(!text.contains("name = \"a\""), "got: {text}");
        assert!(text.contains("name = \"b\""), "got: {text}");
    }

    #[test]
    fn remove_watch_returns_false_for_a_vanished_watch() {
        let mut doc = "version = 1\n\n[[watch]]\nname = \"a\"\npath = \"/a\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(!remove_watch(&mut doc, "ghost"));
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
        assert!(update_watch(&mut doc, &entry));
        let text = doc.to_string();
        assert!(text.contains("path = \"/new\""), "got: {text}");
        assert!(text.contains("branch = \"b\""), "got: {text}");
        // An unmentioned existing field is preserved.
        assert!(text.contains("remote = \"keep\""), "got: {text}");
    }

    #[test]
    fn update_watch_returns_false_when_name_absent() {
        let mut doc = new_document();
        let entry = sample_entry("ghost", "/new");
        assert!(!update_watch(&mut doc, &entry), "no such watch to relink");
    }

    #[test]
    fn load_document_rejects_inline_watch_array() {
        // The read layer tolerates `watch = [{...}]`, but the editor must refuse
        // it rather than coerce (dropping every watch) or index into a
        // structure it did not parse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "version = 1\nwatch = [{ name = \"w\", path = \"/p\" }]\n",
        )
        .unwrap();
        assert!(matches!(
            load_document(&path),
            Err(EditError::WatchNotArrayOfTables { .. })
        ));
    }

    #[test]
    fn config_lock_serializes_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("vard").join("config.toml");
        {
            let _held = ConfigLock::acquire(&config).unwrap();
            assert!(config.parent().unwrap().join("config.lock").exists());
        } // released here
        // A fresh acquire after release succeeds.
        let _again = ConfigLock::acquire(&config).unwrap();
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
    fn set_dotted_creates_a_table_and_preserves_comments() {
        let original = "version = 1\n\n# keep me\n[defaults]\ninterval = \"15m\"  # inline\n";
        let mut doc = original.parse::<DocumentMut>().unwrap();
        set_dotted(&mut doc, "daemon.log_level", "debug").unwrap();
        let text = doc.to_string();
        assert!(text.contains("# keep me"), "comment lost: {text}");
        assert!(text.contains("# inline"), "inline comment lost: {text}");
        assert!(text.contains("log_level = \"debug\""), "got: {text}");
    }

    #[test]
    fn set_dotted_infers_bool_and_integer_and_string() {
        let mut doc = new_document();
        set_dotted(&mut doc, "defaults.sync", "true").unwrap();
        set_dotted(&mut doc, "daemon.log_retention_days", "30").unwrap();
        set_dotted(&mut doc, "defaults.interval", "15m").unwrap();
        let text = doc.to_string();
        assert!(text.contains("sync = true"), "bool not inferred: {text}");
        assert!(
            text.contains("log_retention_days = 30"),
            "int not inferred: {text}"
        );
        assert!(
            text.contains("interval = \"15m\""),
            "string not inferred: {text}"
        );
    }

    #[test]
    fn get_dotted_reads_scalars_and_reports_missing_and_non_scalar() {
        let doc = "version = 1\n\n[daemon]\nlog_level = \"info\"\nlog_retention_days = 14\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            get_dotted(&doc, "daemon.log_level")
                .unwrap()
                .ok()
                .as_deref(),
            Some("info")
        );
        assert_eq!(
            get_dotted(&doc, "daemon.log_retention_days")
                .unwrap()
                .ok()
                .as_deref(),
            Some("14")
        );
        // A missing key is None.
        assert!(get_dotted(&doc, "defaults.interval").is_none());
        // The bare table is present but not a scalar.
        assert!(get_dotted(&doc, "daemon").unwrap().is_err());
    }

    #[test]
    fn unset_dotted_removes_a_key_and_reports_a_missing_one() {
        let mut doc = "version = 1\n\n[defaults]\ninterval = \"15m\"\nquiesce = \"10s\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(unset_dotted(&mut doc, "defaults.interval"));
        let text = doc.to_string();
        assert!(!text.contains("interval"), "got: {text}");
        assert!(text.contains("quiesce = \"10s\""), "sibling kept: {text}");
        // Removing an absent key reports false.
        assert!(!unset_dotted(&mut doc, "defaults.nope"));
        assert!(!unset_dotted(&mut doc, "ai.model"));
    }

    #[test]
    fn unset_dotted_may_leave_an_empty_table() {
        let mut doc = "version = 1\n\n[ai]\nmodel = \"claude\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(unset_dotted(&mut doc, "ai.model"));
        // The now-empty [ai] table remains — simple and harmless.
        let text = doc.to_string();
        assert!(text.contains("[ai]"), "empty table left in place: {text}");
        assert!(!text.contains("model"), "got: {text}");
    }

    #[test]
    fn document_validity_flags_a_missing_version() {
        assert!(document_validity("version = 1\n").is_ok());
        assert!(document_validity("[daemon]\nlog_level = \"info\"\n").is_err());
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
