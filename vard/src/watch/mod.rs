//! The `vard watch` command set: add, remove, list, pause, resume.
//!
//! These are the first commands that *mutate* vard's configuration. They edit
//! `config.toml` in place through the comment-preserving
//! [`config_edit`] layer and commit each change atomically,
//! so the running daemon — which watches the file — reloads a clean, whole
//! config every time.
//!
//! # Identity (spec §12)
//!
//! A watch is keyed by its canonicalized path (symlinks resolved) and its
//! stable name. `add` stores the canonical path; the `<name|path>` selector on
//! `remove`/`pause`/`resume` resolves either (see [`select`]). Re-adding an
//! existing name at a new path relinks that watch — the moved-directory
//! recovery.
//!
//! # Output
//!
//! `list` renders through the global `--format`: human records on a TTY, JSON /
//! JSONL when piped. The mutating commands print a one-line confirmation in the
//! records form and a single result object in the machine forms, so a script can
//! consume `add`/`remove`/`pause`/`resume` as readily as `list`.

mod excludes;
mod select;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use vard_core::{GitBackend, TriggerMode, WatchSpec};

use crate::cli::{ColorWhen, OutputFormat, WatchAddArgs, WatchCommand, WatchRemoveArgs};
use crate::config::{Config, ConfigError, ResolvedWatch, expand_tilde};
use crate::config_edit::{self, WatchEntry};
use crate::journal::Journal;
use crate::output::format;
use crate::output::palette::{self, Palette};
use crate::output::record::{self, Record, RecordField};
use crate::paths::{self, HomeNotFound};

/// The filesystem locations the watch commands read and write, resolved once.
/// Injected in tests so nothing touches the real HOME.
struct WatchPaths {
    /// `config.toml`, mutated in place.
    config_file: PathBuf,
    /// Per-watch operation journals, dropped by `remove --purge`.
    journal_dir: PathBuf,
}

impl WatchPaths {
    fn from_xdg() -> Result<WatchPaths, HomeNotFound> {
        Ok(WatchPaths {
            config_file: paths::config_file()?,
            journal_dir: paths::journal_dir()?,
        })
    }
}

/// Resolved output settings shared by every command's emitter.
struct OutCtx {
    format: OutputFormat,
    palette: Palette,
    term_width: usize,
}

/// A command failure carrying the message to print and the process exit code
/// (2 for an error, 1 for "attention needed" such as a declined `git init`).
struct CmdError {
    message: String,
    code: u8,
}

impl CmdError {
    /// An error (exit code 2).
    fn err(message: impl Into<String>) -> Self {
        CmdError {
            message: message.into(),
            code: 2,
        }
    }

    /// An "attention needed" outcome (exit code 1): the command did not fail,
    /// but it also did not complete — e.g. the user declined to initialize a
    /// repository.
    fn attention(message: impl Into<String>) -> Self {
        CmdError {
            message: message.into(),
            code: 1,
        }
    }
}

impl From<config_edit::EditError> for CmdError {
    fn from(e: config_edit::EditError) -> Self {
        CmdError::err(e.to_string())
    }
}

type CmdResult = Result<(), CmdError>;

/// Production entry point for `vard watch <subcommand>`. Resolves paths and
/// output settings, dispatches, and maps the result to an exit code.
pub(crate) fn run(cmd: WatchCommand, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    let paths = match WatchPaths::from_xdg() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("vard: {err}");
            return ExitCode::from(2);
        }
    };

    let is_tty = io::stdout().is_terminal();
    let out = OutCtx {
        format: format::resolve(format, is_tty),
        palette: palette::resolve_with_tty(color, is_tty),
        term_width: term_width(),
    };

    let result = match cmd {
        WatchCommand::Add(args) => cmd_add(&paths, &out, args),
        WatchCommand::Remove(args) => cmd_remove(&paths, &out, args),
        WatchCommand::List => cmd_list(&paths, &out),
        WatchCommand::Pause(args) => cmd_set_paused(&paths, &out, &args.target, true),
        WatchCommand::Resume(args) => cmd_set_paused(&paths, &out, &args.target, false),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vard: {}", err.message);
            ExitCode::from(err.code)
        }
    }
}

// --- add -------------------------------------------------------------------

/// How a `watch add` maps onto the existing config.
enum Registration {
    /// Append a new `[[watch]]`.
    Append,
    /// Relink the watch at this index to the new path (re-add / moved dir).
    Relink(usize),
}

fn cmd_add(paths: &WatchPaths, out: &OutCtx, args: WatchAddArgs) -> CmdResult {
    // Canonicalize the path (which requires it to exist); this is the watch's
    // identity. A non-existent directory is a clear, early error.
    let canonical = std::fs::canonicalize(&args.path)
        .map_err(|e| CmdError::err(format!("{}: {e}", args.path.display())))?;
    if !canonical.is_dir() {
        return Err(CmdError::err(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }

    // Name: explicit, or the directory's own final component.
    let name = match &args.name {
        Some(n) => n.clone(),
        None => canonical
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .ok_or_else(|| {
                CmdError::err(format!(
                    "cannot derive a watch name from {}; pass --name",
                    canonical.display()
                ))
            })?,
    };

    // Validate everything vard-core owns (name charset, durations, trigger)
    // before touching the filesystem or config, so a bad flag fails cleanly.
    let interval = opt_duration(args.interval.as_deref())?;
    let quiesce = opt_duration(args.quiesce.as_deref())?;
    let trigger = args.trigger.map(|t| t.as_str());
    validate_watch(
        &name,
        &canonical,
        trigger,
        interval,
        quiesce,
        args.no_sync,
        args.remote.as_deref(),
        args.branch.as_deref(),
    )?;

    // Ensure the path is a git repository (offering / performing init), then
    // seed vard's default excludes.
    let initialized = ensure_repo(&canonical, args.init, args.branch.as_deref())?;
    excludes::ensure(&canonical)
        .map_err(|e| CmdError::err(format!("writing git excludes: {e}")))?;

    // Decide append-vs-relink against the current config, then apply it to the
    // editable document and commit atomically.
    let config = load_config(&paths.config_file)?;
    let registration = plan_registration(config.as_ref(), &name, &canonical)?;
    let relinked = matches!(registration, Registration::Relink(_));

    let entry = WatchEntry {
        name: name.clone(),
        path: canonical.to_string_lossy().into_owned(),
        branch: args.branch.clone(),
        remote: args.remote.clone(),
        trigger: trigger.map(str::to_string),
        interval: args.interval.clone(),
        quiesce: args.quiesce.clone(),
        no_sync: args.no_sync,
    };

    let mut doc =
        config_edit::load_document(&paths.config_file)?.unwrap_or_else(config_edit::new_document);
    match registration {
        Registration::Append => config_edit::append_watch(&mut doc, &entry),
        Registration::Relink(index) => config_edit::update_watch(&mut doc, index, &entry),
    }
    config_edit::write_atomic(&paths.config_file, &doc)?;

    let verb = if relinked { "relinked" } else { "added" };
    let human = format!("{verb} watch {name} → {}", canonical.display());
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("name", &name),
            RecordField::str("path", canonical.to_string_lossy()),
            RecordField::bool("initialized", initialized),
            RecordField::bool("relinked", relinked),
        ],
    };
    emit_action(out, &human, &record)
}

/// Ensures `path` is a git repository rooted there, initializing one when it is
/// not and doing so is authorized. Returns whether an init happened.
fn ensure_repo(path: &Path, init_flag: bool, branch: Option<&str>) -> Result<bool, CmdError> {
    match GitBackend::detect(path) {
        Ok(Some(_)) => Ok(false),
        Ok(None) => {
            let approved = if init_flag {
                true
            } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
                prompt_init(path)?
            } else {
                return Err(CmdError::err(format!(
                    "{} is not a git repository; re-run with --init to initialize one, \
                     or run `git init` there first",
                    path.display()
                )));
            };
            if !approved {
                return Err(CmdError::attention(format!(
                    "{} is not a git repository; nothing was added",
                    path.display()
                )));
            }
            GitBackend::init(path, branch)
                .map_err(|e| CmdError::err(format!("git init {}: {e}", path.display())))?;
            Ok(true)
        }
        Err(e) => Err(CmdError::err(format!("checking {}: {e}", path.display()))),
    }
}

/// Asks the user, on a terminal, whether to initialize a repository. The prompt
/// goes to stderr so stdout stays clean for machine consumers; default is no.
fn prompt_init(path: &Path) -> Result<bool, CmdError> {
    eprint!(
        "{} is not a git repository. Initialize one? [y/N] ",
        path.display()
    );
    io::stderr()
        .flush()
        .map_err(|e| CmdError::err(format!("prompting: {e}")))?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| CmdError::err(format!("reading response: {e}")))?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Validates the watch against vard-core's invariants by building (and
/// discarding) a [`WatchSpec`]. This surfaces a bad name, duration, or trigger
/// with the same message the daemon would give, before anything is written.
#[allow(clippy::too_many_arguments)]
fn validate_watch(
    name: &str,
    path: &Path,
    trigger: Option<&str>,
    interval: Option<Duration>,
    quiesce: Option<Duration>,
    no_sync: bool,
    remote: Option<&str>,
    branch: Option<&str>,
) -> Result<(), CmdError> {
    let mut builder = WatchSpec::builder(name, path);
    if let Some(t) = trigger {
        let mode = t
            .parse::<TriggerMode>()
            .map_err(|e| CmdError::err(e.to_string()))?;
        builder = builder.trigger(mode);
    }
    if let Some(iv) = interval {
        builder = builder.interval(iv);
    }
    if let Some(q) = quiesce {
        builder = builder.quiesce(q);
    }
    builder = builder.sync(!no_sync);
    if let Some(r) = remote {
        builder = builder.remote(r);
    }
    if let Some(b) = branch {
        builder = builder.branch(b);
    }
    builder
        .build()
        .map(|_| ())
        .map_err(|e| CmdError::err(format!("invalid watch: {e}")))
}

/// Decides whether an add appends a new watch or relinks an existing one,
/// rejecting the conflicting cases.
fn plan_registration(
    config: Option<&Config>,
    name: &str,
    canonical: &Path,
) -> Result<Registration, CmdError> {
    let Some(config) = config else {
        return Ok(Registration::Append);
    };
    let home = home_dir();
    let by_name = config
        .watches
        .iter()
        .position(|w| w.name.eq_ignore_ascii_case(name));
    let by_path = config
        .watches
        .iter()
        .position(|w| config_path_is(&w.path, home.as_deref(), canonical));

    match (by_name, by_path) {
        // Same watch (re-add same name + path, or relink same name to a new
        // path): update it in place.
        (Some(n), Some(p)) if n == p => Ok(Registration::Relink(n)),
        (Some(n), None) => Ok(Registration::Relink(n)),
        // The name exists on one watch and the path on another — relinking would
        // collide. Refuse rather than silently clobber.
        (Some(n), Some(p)) => Err(CmdError::err(format!(
            "name {:?} belongs to one watch and {} is already watched by {:?}; \
             remove one first",
            config.watches[n].name,
            canonical.display(),
            config.watches[p].name
        ))),
        // The path is watched under a different name.
        (None, Some(p)) => Err(CmdError::err(format!(
            "{} is already watched by {:?}; remove it first or re-add under that name",
            canonical.display(),
            config.watches[p].name
        ))),
        (None, None) => Ok(Registration::Append),
    }
}

// --- remove ----------------------------------------------------------------

fn cmd_remove(paths: &WatchPaths, out: &OutCtx, args: WatchRemoveArgs) -> CmdResult {
    let config = require_config(paths, "remove")?;
    let index =
        select::select_watch(&config, &args.target).map_err(|e| CmdError::err(e.to_string()))?;
    let name = config.watches[index].name.clone();
    let path_display = config.watches[index].path.display().to_string();

    let mut doc = config_edit::load_document(&paths.config_file)?
        .ok_or_else(|| CmdError::err("config file vanished while removing".to_string()))?;
    config_edit::remove_watch(&mut doc, index);
    config_edit::write_atomic(&paths.config_file, &doc)?;

    // --purge drops vard's own per-watch metadata; the repository is never
    // touched, purge or not.
    if args.purge {
        purge_metadata(paths, &name)?;
    }

    let human = if args.purge {
        format!("removed watch {name} and purged its metadata")
    } else {
        format!("removed watch {name}")
    };
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("name", &name),
            RecordField::str("path", path_display),
            RecordField::bool("purged", args.purge),
        ],
    };
    emit_action(out, &human, &record)
}

/// Drops vard's per-watch metadata (its operation journal) for `name`. Absent
/// metadata is not an error — purge is idempotent.
fn purge_metadata(paths: &WatchPaths, name: &str) -> CmdResult {
    let journal = Journal::in_dir(&paths.journal_dir, name);
    match std::fs::remove_file(journal.path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CmdError::err(format!("purging metadata for {name:?}: {e}"))),
    }
}

// --- list ------------------------------------------------------------------

fn cmd_list(paths: &WatchPaths, out: &OutCtx) -> CmdResult {
    let watches = match load_config(&paths.config_file)? {
        Some(config) => config
            .resolve_all()
            .map_err(|e: ConfigError| CmdError::err(e.to_string()))?,
        None => Vec::new(),
    };
    let records: Vec<Record> = watches.iter().map(watch_record).collect();
    emit_list(out, &records)
}

/// Builds the display record for one resolved watch (effective values plus its
/// paused flag). Name is a field, not a header, so the machine forms carry it.
fn watch_record(rw: &ResolvedWatch) -> Record {
    let spec = &rw.spec;
    Record {
        header: None,
        fields: vec![
            RecordField::str("name", spec.name()),
            RecordField::str("path", spec.path().to_string_lossy()),
            RecordField::opt("branch", spec.branch()),
            RecordField::str("remote", spec.remote()),
            RecordField::str("trigger", spec.trigger().to_string()),
            RecordField::str("interval", record::format_duration(spec.interval())),
            RecordField::bool("sync", spec.sync()),
            RecordField::bool("paused", rw.paused).highlighted(rw.paused),
        ],
    }
}

// --- pause / resume --------------------------------------------------------

fn cmd_set_paused(paths: &WatchPaths, out: &OutCtx, target: &str, paused: bool) -> CmdResult {
    let config = require_config(paths, if paused { "pause" } else { "resume" })?;
    let index = select::select_watch(&config, target).map_err(|e| CmdError::err(e.to_string()))?;
    let name = config.watches[index].name.clone();
    let was = config.watches[index].paused;

    let mut doc = config_edit::load_document(&paths.config_file)?
        .ok_or_else(|| CmdError::err("config file vanished while updating".to_string()))?;
    config_edit::set_paused(&mut doc, index, paused);
    config_edit::write_atomic(&paths.config_file, &doc)?;

    let human = if was == paused {
        format!(
            "watch {name} was already {}",
            if paused { "paused" } else { "active" }
        )
    } else if paused {
        format!("paused watch {name}")
    } else {
        format!("resumed watch {name}")
    };
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("name", &name),
            RecordField::bool("paused", paused),
        ],
    };
    emit_action(out, &human, &record)
}

// --- shared helpers --------------------------------------------------------

/// Loads and validates the config, or `None` when the file does not exist.
fn load_config(config_file: &Path) -> Result<Option<Config>, CmdError> {
    match Config::load(config_file) {
        Ok(config) => Ok(Some(config)),
        Err(ConfigError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CmdError::err(err.to_string())),
    }
}

/// Like [`load_config`], but a missing file is an error naming the failed
/// operation — you cannot remove or pause a watch that was never added.
fn require_config(paths: &WatchPaths, op: &str) -> Result<Config, CmdError> {
    load_config(&paths.config_file)?.ok_or_else(|| {
        CmdError::err(format!(
            "no config file at {}; nothing to {op}",
            paths.config_file.display()
        ))
    })
}

/// Whether a watch's configured path (tilde-expanded) identifies the same
/// directory as `target` (already canonical).
fn config_path_is(config_path: &Path, home: Option<&Path>, target: &Path) -> bool {
    let expanded = expand_tilde(config_path, home).unwrap_or_else(|| config_path.to_path_buf());
    match std::fs::canonicalize(&expanded) {
        Ok(canon) => canon == target,
        Err(_) => expanded == target,
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn opt_duration(raw: Option<&str>) -> Result<Option<Duration>, CmdError> {
    // `vard_core::parse_duration`'s error already names the offending value, so
    // it is surfaced verbatim rather than re-wrapped.
    raw.map(|v| vard_core::parse_duration(v).map_err(|e| CmdError::err(e.to_string())))
        .transpose()
}

fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

/// Emits a list of records in the resolved format.
fn emit_list(out: &OutCtx, records: &[Record]) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => {
            record::render_records(&mut w, &out.palette, records, "watches", out.term_width)
        }
        OutputFormat::Json => record::render_json(&mut w, records),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, records),
    };
    finish_write(res)
}

/// Emits a single command result: a human line in the records form, or a single
/// JSON object in the machine forms.
fn emit_action(out: &OutCtx, human: &str, record: &Record) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => writeln!(w, "{human}"),
        OutputFormat::Json | OutputFormat::Jsonl => {
            record::write_json_object(&mut w, record).and_then(|()| w.write_all(b"\n"))
        }
    };
    finish_write(res)
}

/// Folds a write result into a [`CmdResult`], treating a broken pipe (the reader
/// went away, e.g. `| head`) as success rather than an error.
fn finish_write(res: io::Result<()>) -> CmdResult {
    match res {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(CmdError::err(format!("writing output: {e}"))),
    }
}
