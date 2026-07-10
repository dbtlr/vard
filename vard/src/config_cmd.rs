//! `vard config get|set|unset|edit|path` — read and edit the TOML config.
//!
//! These commands address scalar keys in the `[daemon]`, `[defaults]`, `[ai]`,
//! and `[update]` tables by their dotted names, plus a freeform `edit` and a
//! `path` locator. They build on the same comment-preserving, validate-before-
//! write machinery the [`watch`](crate::watch) verbs use ([`config_edit`]): every
//! mutation is applied to a [`toml_edit`] document, validated by round-tripping
//! through the read layer, and committed atomically under the [`ConfigLock`], so
//! the running daemon — which watches the file — never sees a half-written or
//! broken config.
//!
//! # Key surface
//!
//! The set of watched directories is deliberately *not* editable here: a
//! `watch.*` key is refused with a pointer to the `vard watch` verbs, which
//! understand watch identity (canonical paths, relinking). The top-level
//! `version` is managed by vard and is not settable (though it is readable).
//! `edit` is the one freeform escape hatch — it validates the whole document
//! rather than one key — so it can touch anything, at the cost of the identity
//! guarantees the verbs give.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};

use toml_edit::DocumentMut;

use crate::cli::{ColorWhen, ConfigCommand, OutputFormat};
use crate::command::{CmdError, CmdResult, OutCtx, emit_action, finish};
use crate::config_edit::{self, ConfigLock, ScalarValue};
use crate::output::record::{Record, RecordField};
use crate::paths;

/// The tables whose scalar keys `get`/`set`/`unset` may address.
const SETTABLE_TABLES: &[&str] = &["daemon", "defaults", "ai", "update"];

/// Whether a key is being read (`get`) or written (`set`/`unset`). A few keys
/// answer differently: `version` is readable but not settable, and the pointer
/// for a `watch.*` key points at inspection (`vard watch list`) or mutation
/// (`vard watch add|…`) accordingly.
#[derive(Clone, Copy)]
enum KeyMode {
    Read,
    Write,
}

/// Production entry point for `vard config <subcommand>`.
pub(crate) fn run(cmd: ConfigCommand, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    match cmd {
        // `get` and `path` are single-value surfaces: absent an explicit
        // `--format` they emit the bare value regardless of destination (the
        // TEXT response type). See `output::format` module docs.
        //
        // `get` distinguishes found (0) from not-set (1, silent) from an
        // operational error (2, with a message), the way `git config` does — the
        // git-config contract this command deliberately mirrors.
        ConfigCommand::Get(args) => {
            let out = OutCtx::resolve_single_value(color, format);
            match cmd_get(&out, &args.key) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::from(1),
                Err(e) => finish(Err(e)),
            }
        }
        ConfigCommand::Path => finish(cmd_path(&OutCtx::resolve_single_value(color, format))),
        ConfigCommand::Set(args) => finish(cmd_set(
            &OutCtx::resolve(color, format),
            &args.key,
            &args.value,
        )),
        ConfigCommand::Unset(args) => finish(cmd_unset(&OutCtx::resolve(color, format), &args.key)),
        ConfigCommand::Edit => finish(cmd_edit(&OutCtx::resolve(color, format))),
    }
}

/// The human-readable settable-table list (`[daemon], [defaults], [ai], or
/// [update]`), built from [`SETTABLE_TABLES`] so the error text and the const
/// can never drift.
fn settable_tables_phrase() -> String {
    let bracketed: Vec<String> = SETTABLE_TABLES.iter().map(|t| format!("[{t}]")).collect();
    match bracketed.split_last() {
        Some((last, head)) if !head.is_empty() => format!("{}, or {last}", head.join(", ")),
        _ => bracketed.join(", "),
    }
}

/// Classifies a dotted key against the settable surface. A `watch.*` key points
/// at the `vard watch` verbs (inspection for a read, mutation for a write);
/// `version` is readable but not settable; any other top-level table is not
/// addressable; and a bare table name (no dotted field) is not a scalar.
fn classify_key(key: &str, mode: KeyMode) -> Result<(), CmdError> {
    let head = key.split('.').next().unwrap_or_default();
    if head == "watch" {
        return Err(CmdError::err(match mode {
            KeyMode::Read => {
                "watch settings are inspected with `vard watch list`, not `vard config get`"
            }
            KeyMode::Write => {
                "watch settings are not edited with `vard config`; use `vard watch add|remove|pause|resume`"
            }
        }));
    }
    if head == "version" {
        return match mode {
            // The managed version is readable like any other top-level scalar…
            KeyMode::Read => Ok(()),
            // …but never settable by hand.
            KeyMode::Write => Err(CmdError::err(
                "`version` is managed by vard and is not settable",
            )),
        };
    }
    if !SETTABLE_TABLES.contains(&head) {
        return Err(CmdError::err(format!(
            "config key {key:?} is not settable; address a scalar in {}",
            settable_tables_phrase()
        )));
    }
    if !key.contains('.') {
        return Err(CmdError::err(format!(
            "config key {key:?} names a table, not a scalar; use a dotted key like `{head}.<field>`"
        )));
    }
    Ok(())
}

/// Builds the `value` record field carrying the typed scalar, so the machine
/// forms emit a JSON boolean/number/string rather than a stringified everything.
fn value_field(value: &ScalarValue) -> RecordField {
    match value {
        ScalarValue::Bool(b) => RecordField::bool("value", *b),
        ScalarValue::Int(i) => RecordField::opt_int("value", Some(*i)),
        ScalarValue::Str(s) => RecordField::str("value", s),
    }
}

// --- get -------------------------------------------------------------------

/// Prints the value a key is set to. Returns `Ok(true)` when found (and printed),
/// `Ok(false)` when the key is not set (empty stdout, exit 1), or an error.
fn cmd_get(out: &OutCtx, key: &str) -> Result<bool, CmdError> {
    classify_key(key, KeyMode::Read)?;
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let Some(doc) = config_edit::load_document(&config_file)? else {
        // No config file at all ⇒ nothing is set.
        return Ok(false);
    };
    match config_edit::get_dotted(&doc, key) {
        Some(Ok(value)) => {
            // Records/human: the bare value (scripting ergonomics). JSON: the
            // {key, value} object with a *typed* value cell. `emit_action`
            // renders each form for us.
            let record = Record {
                header: None,
                fields: vec![RecordField::str("key", key), value_field(&value)],
            };
            emit_action(out, &value.display(), &record)?;
            Ok(true)
        }
        Some(Err(_not_scalar)) => Err(CmdError::err(format!(
            "config key {key:?} names a table or array, not a scalar; name a specific field"
        ))),
        None => Ok(false),
    }
}

// --- set -------------------------------------------------------------------

fn cmd_set(out: &OutCtx, key: &str, raw: &str) -> CmdResult {
    classify_key(key, KeyMode::Write)?;
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let _lock = ConfigLock::acquire(&config_file)?;
    let (base, pre_edit) = match config_edit::load_document_with_text(&config_file)? {
        Some((doc, text)) => (doc, Some(text)),
        None => (config_edit::new_document(), None),
    };

    // Build both candidate documents in memory, then decide the value's type
    // *once* — from validity, not write side effects — and commit exactly once.
    // The inferred candidate types `raw` per TOML inference (a duration like
    // `defaults.interval 3600` infers as an integer); the string candidate forces
    // the same edit to a string, for a field that accepts "3600" but not 3600.
    // When inference already yields a string the two are identical, so the string
    // candidate is skipped (`None`). Which one is committed — and whether that is
    // a clean write, a refusal, or a repair-with-warning — is
    // [`select_set_candidate`]'s decision, realized by [`commit_document`].
    let mut inferred = base.clone();
    config_edit::set_dotted(&mut inferred, key, raw).map_err(CmdError::err)?;
    let string_candidate = if config_edit::infers_string(raw) {
        None
    } else {
        let mut string_doc = base;
        config_edit::set_dotted_string(&mut string_doc, key, raw).map_err(CmdError::err)?;
        Some(string_doc)
    };
    let doc = config_edit::select_set_candidate(pre_edit.as_deref(), inferred, string_candidate);

    let warning = config_edit::commit_document(&doc, &config_file, pre_edit.as_deref())?;

    // Report the value as actually stored (post-inference / post-selection), typed.
    let stored = config_edit::get_dotted(&doc, key).and_then(Result::ok);
    let (display, cell) = match stored {
        Some(value) => (value.display(), value_field(&value)),
        None => (raw.to_string(), RecordField::str("value", raw)),
    };
    let human = format!("set {key} = {display}");
    let record = Record {
        header: None,
        fields: vec![RecordField::str("key", key), cell],
    };
    emit_action(out, &human, &record)?;
    warning.map_or(Ok(()), Err)
}

// --- unset -----------------------------------------------------------------

fn cmd_unset(out: &OutCtx, key: &str) -> CmdResult {
    classify_key(key, KeyMode::Write)?;
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let _lock = ConfigLock::acquire(&config_file)?;
    let Some((mut doc, pre_edit)) = config_edit::load_document_with_text(&config_file)? else {
        return Err(CmdError::err(format!(
            "no config file at {}; nothing to unset",
            config_file.display()
        )));
    };
    if !config_edit::unset_dotted(&mut doc, key) {
        return Err(CmdError::err(format!("config key {key:?} is not set")));
    }
    let warning = config_edit::commit_document(&doc, &config_file, Some(&pre_edit))?;

    let human = format!("unset {key}");
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("key", key),
            RecordField::bool("unset", true),
        ],
    };
    emit_action(out, &human, &record)?;
    warning.map_or(Ok(()), Err)
}

// --- path ------------------------------------------------------------------

fn cmd_path(out: &OutCtx) -> CmdResult {
    let path = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let human = path.display().to_string();
    let record = Record {
        header: None,
        fields: vec![RecordField::str("path", path.to_string_lossy())],
    };
    emit_action(out, &human, &record)
}

// --- edit ------------------------------------------------------------------

fn cmd_edit(out: &OutCtx) -> CmdResult {
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let editor = resolve_editor()?;
    edit_with(out, &config_file, |tmp| launch_editor(&editor, tmp))
}

/// The injectable core of `edit`: seed a temp file from the current config, hand
/// it to `launch`, then — under the config lock — verify the config did not
/// change while editing, validate, and atomically install the result.
///
/// Temp-file hygiene: the scratch file is created `0600`. A launch failure (the
/// user never edited) removes it; a temp read failure removes it best-effort. But
/// once the user *has* edited, every later failure (lock, concurrent change,
/// invalid TOML, or a valid→invalid result) preserves the temp and names its
/// path, so the work is never lost.
fn edit_with(
    out: &OutCtx,
    config_file: &Path,
    launch: impl FnOnce(&Path) -> Result<(), CmdError>,
) -> CmdResult {
    // The baseline the edit starts from: the exact on-disk text, or `None` when
    // no config file exists yet.
    let seed_baseline = match fs::read_to_string(config_file) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(CmdError::err(format!(
                "reading {}: {e}",
                config_file.display()
            )));
        }
    };
    // The bytes actually handed to the editor: the baseline, or a fresh
    // version-seeded document when there is none. This is also the pre-edit text
    // whose validity decides how strictly the result is judged (a missing file
    // seeds a *valid* document, so a first-time edit that saves an invalid config
    // is refused, not silently written).
    let seeded = seed_baseline
        .clone()
        .unwrap_or_else(|| config_edit::new_document().to_string());

    let dir = config_file.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| CmdError::err(format!("{}: {e}", dir.display())))?;
    let tmp = dir.join(format!(".config-edit-{}.toml", std::process::id()));
    write_private(&tmp, seeded.as_bytes())
        .map_err(|e| CmdError::err(format!("writing {}: {e}", tmp.display())))?;

    // A launch failure means the user made no edits — the temp is pure scratch,
    // so remove it.
    if let Err(e) = launch(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // A temp read failure loses nothing the user could recover *from the temp*
    // (we cannot read it), so remove it best-effort.
    let edited = match fs::read_to_string(&tmp) {
        Ok(text) => text,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(CmdError::err(format!("reading {}: {e}", tmp.display())));
        }
    };

    // From here the user HAS edited: preserve the temp on every failure.
    let _lock = ConfigLock::acquire(config_file).map_err(|e| preserved(&tmp, e.to_string()))?;

    // Optimistic concurrency: the config must be byte-for-byte what we seeded
    // from, or a concurrent writer landed a change our whole-document overwrite
    // would silently clobber. Refuse and preserve the edit.
    let current = match fs::read_to_string(config_file) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(preserved(
                &tmp,
                format!("reading {}: {e}", config_file.display()),
            ));
        }
    };
    if current != seed_baseline {
        return Err(preserved(
            &tmp,
            "the config changed on disk while you were editing, so your edit was not applied"
                .to_string(),
        ));
    }

    let doc: DocumentMut = match edited.parse() {
        Ok(doc) => doc,
        Err(e) => {
            return Err(preserved(
                &tmp,
                format!("edited config is not valid TOML: {e}"),
            ));
        }
    };
    match config_edit::commit_document(&doc, config_file, Some(&seeded)) {
        Ok(warning) => {
            // The write landed; the temp scratch is no longer needed.
            let _ = fs::remove_file(&tmp);
            let human = format!("updated {}", config_file.display());
            let record = Record {
                header: None,
                fields: vec![
                    RecordField::str("path", config_file.to_string_lossy()),
                    RecordField::bool("updated", true),
                ],
            };
            emit_action(out, &human, &record)?;
            warning.map_or(Ok(()), Err)
        }
        // Rejected valid→invalid: nothing was written, so keep the temp file and
        // point the user at it.
        Err(e) => Err(preserved(&tmp, e.message().to_string())),
    }
}

/// Writes `contents` to `path`, creating it `0600` on unix so an in-flight edit
/// (which may hold secrets a user is about to add) is not world-readable.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(contents)
}

/// Builds the rejection error that preserves the edit at `tmp`.
fn preserved(tmp: &Path, message: String) -> CmdError {
    CmdError::err(format!(
        "{message}; your edit is preserved at {}",
        tmp.display()
    ))
}

/// Resolves the editor command: `$VISUAL`, then `$EDITOR` (the git/historical
/// precedence), each ignored when empty. An error names both when neither is set.
fn resolve_editor() -> Result<String, CmdError> {
    for var in ["VISUAL", "EDITOR"] {
        if let Some(value) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
            return Ok(value);
        }
    }
    Err(CmdError::err(
        "no editor configured; set $VISUAL (or $EDITOR) to edit the config",
    ))
}

/// Launches the configured editor on `file` via the shell — like `git` does —
/// so an editor string carrying flags (`code --wait`) or a space-containing path
/// both work without hand-rolled tokenizing. The file is passed as `"$1"`, not
/// interpolated into the script, so its path is never re-parsed by the shell.
fn launch_editor(editor: &str, file: &Path) -> Result<(), CmdError> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("vard-editor")
        .arg(file)
        .status()
        .map_err(|e| CmdError::err(format!("launching editor {editor:?}: {e}")))?;
    if !status.success() {
        return Err(CmdError::err(format!(
            "editor {editor:?} exited without success; the config was not changed"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputFormat;

    fn out() -> OutCtx {
        // JSON so any emitted line is a single compact object during tests.
        OutCtx::resolve(ColorWhen::Never, Some(OutputFormat::Json))
    }

    fn leftovers(dir: &Path) -> Vec<std::fs::DirEntry> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".config-edit-"))
            .collect()
    }

    #[test]
    fn classify_rejects_watch_keys_with_a_pointer() {
        let err = classify_key("watch.0.name", KeyMode::Write).unwrap_err();
        assert!(
            err.message().contains("vard watch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn classify_read_of_a_watch_key_points_at_watch_list() {
        // A read points at inspection (`vard watch list`), not the mutation verbs.
        let err = classify_key("watch.0.name", KeyMode::Read).unwrap_err();
        assert!(
            err.message().contains("vard watch list"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn classify_rejects_setting_version_but_allows_reading_it() {
        assert!(
            classify_key("version", KeyMode::Write)
                .unwrap_err()
                .message()
                .contains("not settable")
        );
        // `config get version` must be permitted through classification.
        assert!(classify_key("version", KeyMode::Read).is_ok());
    }

    #[test]
    fn classify_rejects_unknown_table() {
        assert!(classify_key("bogus.key", KeyMode::Write).is_err());
    }

    #[test]
    fn classify_rejects_a_bare_table_name() {
        let err = classify_key("daemon", KeyMode::Write).unwrap_err();
        assert!(err.message().contains("scalar"), "got: {}", err.message());
    }

    #[test]
    fn classify_accepts_settable_scalars() {
        for key in [
            "daemon.log_level",
            "defaults.interval",
            "ai.model",
            "update.channel",
        ] {
            assert!(
                classify_key(key, KeyMode::Write).is_ok(),
                "should accept {key}"
            );
        }
    }

    #[test]
    fn settable_tables_phrase_is_built_from_the_const() {
        assert_eq!(
            settable_tables_phrase(),
            "[daemon], [defaults], [ai], or [update]"
        );
    }

    #[test]
    fn edit_installs_a_valid_result() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "version = 1\n\n[daemon]\nlog_level = \"info\"\n").unwrap();

        let res = edit_with(&out(), &config, |tmp| {
            fs::write(tmp, "version = 1\n\n[daemon]\nlog_level = \"debug\"\n").unwrap();
            Ok(())
        });
        assert!(res.is_ok(), "a valid edit must install");
        let written = fs::read_to_string(&config).unwrap();
        assert!(written.contains("log_level = \"debug\""), "got: {written}");
        // The temp scratch is cleaned up on success.
        assert!(leftovers(dir.path()).is_empty(), "temp scratch left behind");
    }

    #[test]
    fn edit_rejects_invalid_toml_and_preserves_the_temp() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let original = "version = 1\n\n[daemon]\nlog_level = \"info\"\n";
        fs::write(&config, original).unwrap();

        let err = edit_with(&out(), &config, |tmp| {
            fs::write(tmp, "this = = not toml").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(
            err.message().contains("preserved at"),
            "got: {}",
            err.message()
        );
        // The config on disk is untouched.
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        // The temp scratch survives so the edit is recoverable.
        assert_eq!(leftovers(dir.path()).len(), 1, "the edit must be preserved");
    }

    #[test]
    fn edit_refuses_a_valid_to_invalid_edit() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let original = "version = 1\n\n[daemon]\nlog_level = \"info\"\n";
        fs::write(&config, original).unwrap();

        // Valid TOML, but drops the required version ⇒ schema-invalid.
        let err = edit_with(&out(), &config, |tmp| {
            fs::write(tmp, "[daemon]\nlog_level = \"debug\"\n").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(
            err.message().contains("preserved at"),
            "got: {}",
            err.message()
        );
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            original,
            "a valid→invalid edit must not land"
        );
    }

    #[test]
    fn edit_of_a_missing_config_saving_invalid_is_refused() {
        // F7: a first-time edit (no config file) seeds a *valid* version-only
        // document, so saving a schema-invalid result is a valid→invalid edit and
        // must be refused — not written with a warning.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let err = edit_with(&out(), &config, |tmp| {
            // Valid TOML but no version ⇒ schema-invalid.
            fs::write(tmp, "[daemon]\nlog_level = \"debug\"\n").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(
            err.message().contains("preserved at"),
            "got: {}",
            err.message()
        );
        assert!(!config.exists(), "no config must have been written");
        assert_eq!(leftovers(dir.path()).len(), 1, "the edit must be preserved");
    }

    #[test]
    fn edit_of_a_missing_config_saving_valid_installs() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let res = edit_with(&out(), &config, |tmp| {
            fs::write(tmp, "version = 1\n\n[daemon]\nlog_level = \"debug\"\n").unwrap();
            Ok(())
        });
        assert!(res.is_ok(), "a valid first-time edit must install");
        assert!(
            fs::read_to_string(&config)
                .unwrap()
                .contains("log_level = \"debug\""),
        );
    }

    #[test]
    fn edit_refuses_when_the_config_changed_during_editing() {
        // F1: a concurrent writer lands a change between seed and commit. The
        // whole-document overwrite must not silently clobber it.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let original = "version = 1\n\n[daemon]\nlog_level = \"info\"\n";
        fs::write(&config, original).unwrap();

        let err = edit_with(&out(), &config, |tmp| {
            // The user's edit…
            fs::write(tmp, "version = 1\n\n[daemon]\nlog_level = \"debug\"\n").unwrap();
            // …but a concurrent writer changed the config underneath.
            fs::write(&config, "version = 1\n\n[daemon]\nlog_level = \"trace\"\n").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(
            err.message().contains("changed on disk"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("preserved at"),
            "must preserve the edit: {}",
            err.message()
        );
        // The concurrent write stands; the edit did not clobber it.
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "version = 1\n\n[daemon]\nlog_level = \"trace\"\n"
        );
        assert_eq!(leftovers(dir.path()).len(), 1, "the edit must be preserved");
    }

    #[test]
    fn edit_launch_failure_removes_the_temp() {
        // F8: the user never edited (the editor failed to launch), so the scratch
        // temp is removed rather than left behind.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "version = 1\n").unwrap();

        let err = edit_with(&out(), &config, |_tmp| {
            Err(CmdError::err("editor exploded"))
        })
        .unwrap_err();
        assert_eq!(err.message(), "editor exploded");
        assert!(
            leftovers(dir.path()).is_empty(),
            "a launch failure must remove the scratch temp"
        );
    }

    #[cfg(unix)]
    #[test]
    fn edit_temp_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "version = 1\n").unwrap();

        // Inspect the temp's mode while the editor "runs".
        let mode = std::cell::Cell::new(0u32);
        let _ = edit_with(&out(), &config, |tmp| {
            mode.set(fs::metadata(tmp).unwrap().permissions().mode() & 0o777);
            fs::write(tmp, "version = 1\n").unwrap();
            Ok(())
        });
        assert_eq!(mode.get(), 0o600, "temp must be created 0600");
    }
}
