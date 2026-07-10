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
//! `version` is managed by vard and is not settable either. `edit` is the one
//! freeform escape hatch — it validates the whole document rather than one key —
//! so it can touch anything, at the cost of the identity guarantees the verbs
//! give.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use toml_edit::DocumentMut;

use crate::cli::{ColorWhen, ConfigCommand, OutputFormat};
use crate::command::{CmdError, CmdResult, OutCtx, emit_action, finish};
use crate::config::Config;
use crate::config_edit::{self, ConfigLock};
use crate::output::record::{Record, RecordField};
use crate::paths;

/// The tables whose scalar keys `get`/`set`/`unset` may address.
const SETTABLE_TABLES: &[&str] = &["daemon", "defaults", "ai", "update"];

/// Production entry point for `vard config <subcommand>`.
pub(crate) fn run(cmd: ConfigCommand, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    let out = OutCtx::resolve(color, format);
    match cmd {
        // `get` distinguishes found (0) from not-set (1, silent) from an
        // operational error (2, with a message), the way `git config` does.
        ConfigCommand::Get(args) => match cmd_get(&out, &args.key) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => finish(Err(e)),
        },
        ConfigCommand::Set(args) => finish(cmd_set(&out, &args.key, &args.value)),
        ConfigCommand::Unset(args) => finish(cmd_unset(&out, &args.key)),
        ConfigCommand::Edit => finish(cmd_edit(&out)),
        ConfigCommand::Path => finish(cmd_path(&out)),
    }
}

/// Rejects keys outside the settable surface: a `watch.*` key points at the
/// `vard watch` verbs, `version` is not settable, any other top-level table is
/// not addressable, and a bare table name (no dotted field) is not a scalar.
fn classify_key(key: &str) -> Result<(), CmdError> {
    let head = key.split('.').next().unwrap_or_default();
    if head == "watch" {
        return Err(CmdError::err(
            "watch settings are not edited with `vard config`; use `vard watch add|remove|pause|resume`",
        ));
    }
    if head == "version" {
        return Err(CmdError::err(
            "`version` is managed by vard and is not settable",
        ));
    }
    if !SETTABLE_TABLES.contains(&head) {
        return Err(CmdError::err(format!(
            "config key {key:?} is not settable; address a scalar in [daemon], [defaults], \
             [ai], or [update]"
        )));
    }
    if !key.contains('.') {
        return Err(CmdError::err(format!(
            "config key {key:?} names a table, not a scalar; use a dotted key like `{head}.<field>`"
        )));
    }
    Ok(())
}

// --- get -------------------------------------------------------------------

/// Prints the value a key is set to. Returns `Ok(true)` when found (and printed),
/// `Ok(false)` when the key is not set (empty stdout, exit 1), or an error.
fn cmd_get(out: &OutCtx, key: &str) -> Result<bool, CmdError> {
    classify_key(key)?;
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let Some(doc) = config_edit::load_document(&config_file)? else {
        // No config file at all ⇒ nothing is set.
        return Ok(false);
    };
    match config_edit::get_dotted(&doc, key) {
        Some(Ok(value)) => {
            // Records/human: the bare value (scripting ergonomics). JSON: the
            // {key, value} object. `emit_action` renders each form for us.
            let record = Record {
                header: None,
                fields: vec![
                    RecordField::str("key", key),
                    RecordField::str("value", value.clone()),
                ],
            };
            emit_action(out, &value, &record)?;
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
    classify_key(key)?;
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let _lock = ConfigLock::acquire(&config_file)?;
    let mut doc =
        config_edit::load_document(&config_file)?.unwrap_or_else(config_edit::new_document);
    let pre_edit_invalid = config_edit::document_validity(&doc.to_string()).is_err();
    config_edit::set_dotted(&mut doc, key, raw).map_err(CmdError::err)?;
    let warning = config_edit::commit_document(&doc, &config_file, pre_edit_invalid)?;

    let human = format!("set {key} = {raw}");
    let record = Record {
        header: None,
        fields: vec![RecordField::str("key", key), RecordField::str("value", raw)],
    };
    emit_action(out, &human, &record)?;
    warning.map_or(Ok(()), Err)
}

// --- unset -----------------------------------------------------------------

fn cmd_unset(out: &OutCtx, key: &str) -> CmdResult {
    classify_key(key)?;
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let _lock = ConfigLock::acquire(&config_file)?;
    let Some(mut doc) = config_edit::load_document(&config_file)? else {
        return Err(CmdError::err(format!(
            "no config file at {}; nothing to unset",
            config_file.display()
        )));
    };
    let pre_edit_invalid = config_edit::document_validity(&doc.to_string()).is_err();
    if !config_edit::unset_dotted(&mut doc, key) {
        return Err(CmdError::err(format!("config key {key:?} is not set")));
    }
    let warning = config_edit::commit_document(&doc, &config_file, pre_edit_invalid)?;

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
    let path = Config::default_path().map_err(|e| CmdError::err(e.to_string()))?;
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

/// The injectable core of `edit`: seed a temp file from the current config,
/// hand it to `launch`, then validate and atomically install the result under
/// the config lock. On a rejection (invalid TOML, or an edit that would turn a
/// valid config invalid) the temp file is left in place and its path is reported
/// so the work is not lost.
fn edit_with(
    out: &OutCtx,
    config_file: &Path,
    launch: impl FnOnce(&Path) -> Result<(), CmdError>,
) -> CmdResult {
    // Seed the temp with the current config, or a fresh version-seeded document
    // when there is none yet.
    let seed = match fs::read_to_string(config_file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            config_edit::new_document().to_string()
        }
        Err(e) => {
            return Err(CmdError::err(format!(
                "reading {}: {e}",
                config_file.display()
            )));
        }
    };

    let dir = config_file.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| CmdError::err(format!("{}: {e}", dir.display())))?;
    let tmp = dir.join(format!(".config-edit-{}.toml", std::process::id()));
    fs::write(&tmp, seed.as_bytes())
        .map_err(|e| CmdError::err(format!("writing {}: {e}", tmp.display())))?;

    launch(&tmp)?;

    let edited = fs::read_to_string(&tmp)
        .map_err(|e| CmdError::err(format!("reading {}: {e}", tmp.display())))?;

    // Serialize the read→validate→install against concurrent writers.
    let _lock = ConfigLock::acquire(config_file)?;
    // Whether the config on disk was already invalid decides how strictly the
    // result is judged (a repair of a broken config is allowed to stay broken).
    let pre_edit_invalid = match fs::read_to_string(config_file) {
        Ok(text) => config_edit::document_validity(&text).is_err(),
        Err(_) => true,
    };

    let doc: DocumentMut = match edited.parse() {
        Ok(doc) => doc,
        Err(e) => {
            return Err(preserved(
                &tmp,
                format!("edited config is not valid TOML: {e}"),
            ));
        }
    };
    match config_edit::commit_document(&doc, config_file, pre_edit_invalid) {
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

/// Builds the rejection error that preserves the edit at `tmp`.
fn preserved(tmp: &Path, message: String) -> CmdError {
    CmdError::err(format!(
        "{message}; your edit is preserved at {}",
        tmp.display()
    ))
}

/// Resolves the editor command: `$EDITOR`, then `$VISUAL`, each ignored when
/// empty. An error names both when neither is set.
fn resolve_editor() -> Result<String, CmdError> {
    for var in ["EDITOR", "VISUAL"] {
        if let Some(value) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
            return Ok(value);
        }
    }
    Err(CmdError::err(
        "no editor configured; set $EDITOR (or $VISUAL) to edit the config",
    ))
}

/// Launches `editor` on `file`, waiting for it to exit. The command may carry
/// arguments (e.g. `code --wait`), split on whitespace, with `file` appended.
fn launch_editor(editor: &str, file: &Path) -> Result<(), CmdError> {
    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| CmdError::err("the configured editor command is empty"))?;
    let status = Command::new(program)
        .args(parts)
        .arg(file)
        .status()
        .map_err(|e| CmdError::err(format!("launching editor {program:?}: {e}")))?;
    if !status.success() {
        return Err(CmdError::err(format!(
            "editor {program:?} exited without success; the config was not changed"
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

    #[test]
    fn classify_rejects_watch_keys_with_a_pointer() {
        let err = classify_key("watch.0.name").unwrap_err();
        assert!(
            err.message().contains("vard watch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn classify_rejects_version() {
        assert!(
            classify_key("version")
                .unwrap_err()
                .message()
                .contains("not settable")
        );
    }

    #[test]
    fn classify_rejects_unknown_table() {
        assert!(classify_key("bogus.key").is_err());
    }

    #[test]
    fn classify_rejects_a_bare_table_name() {
        let err = classify_key("daemon").unwrap_err();
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
            assert!(classify_key(key).is_ok(), "should accept {key}");
        }
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
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".config-edit-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp scratch left behind: {leftovers:?}"
        );
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
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".config-edit-"))
            .collect();
        assert_eq!(leftovers.len(), 1, "the edit must be preserved");
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
}
