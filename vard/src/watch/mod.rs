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
// `pub(crate)` so the shared `<name|path>` identity/selector logic is reachable
// from future top-level commands (VRD-16/17) as `crate::watch::select`.
pub(crate) mod select;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use toml_edit::DocumentMut;
use vard_core::{GitBackend, TriggerMode, WatchSpec};

use crate::cli::{ColorWhen, OutputFormat, WatchAddArgs, WatchCommand, WatchRemoveArgs};
use crate::config::{Config, ConfigError, ResolvedWatch, WatchConfig};
use crate::config_edit::{self, ConfigLock, WatchEntry};
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
    /// Relink an existing watch (matched by name) to the new path — the re-add
    /// / moved-directory path. The document is relocated by name at mutation
    /// time, so no index is carried across the two parses.
    Relink,
}

/// The repository decision resolved *before* the config lock is taken, so an
/// interactive `git init` prompt (a human wait) never spans the blocking flock
/// and wedges concurrent `vard watch` writers.
enum RepoPlan {
    /// The path is already a git repository; its backend is carried forward.
    Existing(GitBackend),
    /// The path is not a repository, but an init was authorized (`--init`, or an
    /// interactive "yes"). The init itself runs later, under the lock.
    Init,
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

    // The config is UTF-8 (TOML), so a non-UTF-8 path cannot be stored without
    // a lossy corruption that could never be matched again. Reject it honestly.
    let path_str = canonical
        .to_str()
        .ok_or_else(|| {
            CmdError::err(format!(
                "{} is not valid UTF-8; vard's config is UTF-8 and cannot store this path",
                canonical.display()
            ))
        })?
        .to_string();

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

    // Resolve the repository decision — including any interactive `git init`
    // prompt — *before* acquiring the lock, so a human wait never blocks other
    // `vard watch` writers. The init itself is deferred until after the
    // under-lock conflict re-check.
    let repo_plan = plan_repo(&canonical, args.init)?;

    // Serialize the whole read→plan→mutate→write cycle against concurrent
    // `vard watch` writers, so lost updates and stale relocations cannot race.
    let _lock = ConfigLock::acquire(&paths.config_file)?;

    // Decide append-vs-relink against the current config, and load the editable
    // document, *before* any git init or exclude side effects — so a rejected
    // add (a name/path conflict, or a config the editor cannot safely mutate)
    // leaves no git init or exclude block behind.
    let config = load_config(&paths.config_file)?;
    let registration = plan_registration(config.as_ref(), &name, &canonical)?;
    let relinked = matches!(registration, Registration::Relink);
    let mut doc =
        config_edit::load_document(&paths.config_file)?.unwrap_or_else(config_edit::new_document);
    // The config's validity *before* this edit decides how strictly the result
    // is judged (see [`commit_document`]).
    let pre_edit_invalid = document_validity(&doc.to_string()).is_err();

    // Realize the repository plan under the lock: perform any authorized init,
    // then seed vard's default excludes into the repo's resolved exclude file
    // (which is correct even for a worktree or submodule, where `.git` is a
    // file).
    let initialized = init_and_seed_excludes(&canonical, repo_plan, args.branch.as_deref())?;

    let entry = WatchEntry {
        name: name.clone(),
        path: path_str,
        branch: args.branch.clone(),
        remote: args.remote.clone(),
        trigger: trigger.map(str::to_string),
        interval: args.interval.clone(),
        quiesce: args.quiesce.clone(),
        no_sync: args.no_sync,
    };

    match registration {
        Registration::Append => config_edit::append_watch(&mut doc, &entry),
        // Relink relocates by name inside this document; if the watch vanished
        // between planning and now, fall back to appending rather than panic.
        Registration::Relink => {
            if !config_edit::update_watch(&mut doc, &entry) {
                config_edit::append_watch(&mut doc, &entry);
            }
        }
    }
    // Validate the exact bytes to be written before committing them, so the CLI
    // can never take a valid config to invalid and wedge the daemon's reloads.
    commit_document(&doc, &paths.config_file, pre_edit_invalid)?;

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

/// Resolves the repository decision for `path` *without holding the config
/// lock*: an existing repo is carried forward, a missing one is authorized for
/// init only via `--init` or an interactive "yes", and a declined or
/// non-interactive miss is refused. Any human prompt happens here, before the
/// lock, so an unanswered prompt cannot wedge concurrent writers.
fn plan_repo(path: &Path, init_flag: bool) -> Result<RepoPlan, CmdError> {
    match GitBackend::detect(path) {
        Ok(Some(backend)) => Ok(RepoPlan::Existing(backend)),
        Ok(None) => {
            let approved = if init_flag {
                true
            } else if io::stdin().is_terminal() && io::stderr().is_terminal() {
                // Gate interactivity on the stream we actually prompt on
                // (stderr), so `2>/dev/null` yields a clean error, not a
                // silent, invisible hang waiting on input.
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
            Ok(RepoPlan::Init)
        }
        Err(e) => Err(CmdError::err(format!("checking {}: {e}", path.display()))),
    }
}

/// Realizes a [`RepoPlan`] under the config lock: performs a planned `git init`
/// (idempotent, so a repo appearing between plan and now is harmless), then
/// seeds vard's default excludes into the repo's resolved exclude file. Returns
/// whether an init happened.
fn init_and_seed_excludes(
    path: &Path,
    plan: RepoPlan,
    branch: Option<&str>,
) -> Result<bool, CmdError> {
    let (initialized, backend) = match plan {
        RepoPlan::Existing(backend) => (false, backend),
        RepoPlan::Init => {
            let backend = GitBackend::init(path, branch)
                .map_err(|e| CmdError::err(format!("git init {}: {e}", path.display())))?;
            (true, backend)
        }
    };
    let exclude_path = backend
        .info_exclude_path()
        .map_err(|e| CmdError::err(format!("resolving git excludes: {e}")))?;
    excludes::ensure(&exclude_path)
        .map_err(|e| CmdError::err(format!("writing git excludes: {e}")))?;
    Ok(initialized)
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
/// rejecting the conflicting cases. Path identity uses the shared spec-§12 rule
/// in [`select`], the same one the `<name|path>` selectors apply.
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
    let by_path = config.watches.iter().position(|w| {
        // `canonical` is already canonicalized, so it is its own canonical form.
        select::config_path_identifies(&w.path, home.as_deref(), canonical, Some(canonical))
    });

    match (by_name, by_path) {
        // Same watch (re-add same name + path, or relink same name to a new
        // path): update it in place.
        (Some(n), Some(p)) if n == p => Ok(Registration::Relink),
        (Some(_), None) => Ok(Registration::Relink),
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
    let _lock = ConfigLock::acquire(&paths.config_file)?;
    let config = require_config(paths, "remove")?;
    let index =
        select::select_watch(&config, &args.target).map_err(|e| CmdError::err(e.to_string()))?;
    let name = config.watches[index].name.clone();
    let path_display = config.watches[index].path.display().to_string();

    let mut doc = config_edit::load_document(&paths.config_file)?
        .ok_or_else(|| CmdError::err("config file vanished while removing".to_string()))?;
    let pre_edit_invalid = document_validity(&doc.to_string()).is_err();
    if !config_edit::remove_watch(&mut doc, &name) {
        return Err(CmdError::err(format!(
            "watch {name:?} vanished from the config before it could be removed"
        )));
    }
    commit_document(&doc, &paths.config_file, pre_edit_invalid)?;

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
    let Some(config) = load_config(&paths.config_file)? else {
        return emit_list(out, &[]);
    };
    // `list` is the one read-only diagnostic: it must render even when the
    // config fails full validation (a duplicate name, a bad inherited default),
    // so a broken config is exactly what you can still inspect. On success it
    // shows effective values; on a validation failure it renders leniently from
    // the raw watches and exits 1 (attention) with a warning, never 2.
    match config.resolve_all() {
        Ok(watches) => {
            let records: Vec<Record> = watches.iter().map(watch_record).collect();
            emit_list(out, &records)
        }
        Err(e) => {
            let records: Vec<Record> = config.watches.iter().map(raw_watch_record).collect();
            emit_list(out, &records)?;
            Err(CmdError::attention(format!(
                "listed watches as written, but the config is not fully valid: {e}"
            )))
        }
    }
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

/// Builds a *lenient* display record straight from a raw `[[watch]]` table,
/// used only when full resolution fails so the diagnostic `list` still renders.
/// Unset optional fields show absent (`—` / `null`) rather than resolved
/// defaults — this is the "as written" view, not the effective one.
fn raw_watch_record(w: &WatchConfig) -> Record {
    Record {
        header: None,
        fields: vec![
            RecordField::str("name", &w.name),
            RecordField::str("path", w.path.to_string_lossy()),
            RecordField::opt("branch", w.branch.clone()),
            RecordField::opt("remote", w.remote.clone()),
            RecordField::opt("trigger", w.trigger.clone()),
            RecordField::opt("interval", w.interval.map(record::format_duration)),
            // Boolean-or-null, matching the resolved path's `sync` type — a
            // machine consumer's parse must not depend on config validity.
            RecordField::opt_bool("sync", w.sync),
            RecordField::bool("paused", w.paused).highlighted(w.paused),
        ],
    }
}

// --- pause / resume --------------------------------------------------------

fn cmd_set_paused(paths: &WatchPaths, out: &OutCtx, target: &str, paused: bool) -> CmdResult {
    let _lock = ConfigLock::acquire(&paths.config_file)?;
    let config = require_config(paths, if paused { "pause" } else { "resume" })?;
    let index = select::select_watch(&config, target).map_err(|e| CmdError::err(e.to_string()))?;
    let name = config.watches[index].name.clone();
    let was = config.watches[index].paused;

    let mut doc = config_edit::load_document(&paths.config_file)?
        .ok_or_else(|| CmdError::err("config file vanished while updating".to_string()))?;
    let pre_edit_invalid = document_validity(&doc.to_string()).is_err();
    if !config_edit::set_paused(&mut doc, &name, paused) {
        return Err(CmdError::err(format!(
            "watch {name:?} vanished from the config before it could be updated"
        )));
    }
    commit_document(&doc, &paths.config_file, pre_edit_invalid)?;

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

/// Re-parses a serialized document through the read layer and resolves every
/// watch, returning the first validation error. This mirrors the daemon's
/// validate-before-swap discipline and subsumes per-field gaps (defaults
/// inheritance, duplicate names/paths) that the pre-mutation checks do not see.
/// Paused watches are validated too (via `resolve_all`).
fn document_validity(text: &str) -> Result<(), ConfigError> {
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
///   (attention): the config was already broken, so blocking an unrelated
///   pause/remove would only trap the user — the natural repair path (e.g.
///   `remove`-ing one of a pair of duplicate paths) must be allowed to proceed.
fn commit_document(doc: &DocumentMut, config_file: &Path, pre_edit_invalid: bool) -> CmdResult {
    let text = doc.to_string();
    match document_validity(&text) {
        Ok(()) => {
            config_edit::write_atomic(config_file, doc)?;
            Ok(())
        }
        Err(e) if pre_edit_invalid => {
            // The edit did not introduce the breakage, so honor it — but flag
            // that the config is still not fully valid.
            config_edit::write_atomic(config_file, doc)?;
            Err(CmdError::attention(format!(
                "wrote {}, but the config is still not fully valid: {e}",
                config_file.display()
            )))
        }
        Err(e) => Err(CmdError::err(format!(
            "refusing to write {}: the edit would make a valid config invalid: {e}",
            config_file.display()
        ))),
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
