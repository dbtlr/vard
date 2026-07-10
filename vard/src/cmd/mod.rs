//! The top-level snapshot/history commands: `snapshot`, `log`, `diff`,
//! `restore` (spec §11).
//!
//! These operate on a watch's version-control repository directly, on demand —
//! the manual counterparts to the daemon's automatic snapshotting. They share
//! one discipline with the daemon so the two never corrupt each other's state:
//!
//! * **Backends** are opened through [`vard_core::open_git_backend`], the same
//!   branch policy the engine applies, so a command commits to (and restores
//!   from) exactly the branch the daemon uses.
//! * **In-process snapshots** ([`snapshot`] and [`restore`]'s protective
//!   snapshot) bracket the backend call in the per-watch operation
//!   [`Journal`] — `begin` before, `complete` after
//!   every outcome — mirroring the daemon's event bracket so a crash
//!   mid-operation leaves a recoverable journal record.
//! * **Dispatch** for `snapshot` uses the single-instance flock as a race-free
//!   discriminator: if the CLI can take the lock, no daemon is running and it
//!   snapshots in-process while holding the lock; if the lock is held, a daemon
//!   owns the repositories and the CLI hands it a request file instead (see
//!   [`crate::daemon`]).
//!
//! `restore` cannot take the instance lock (a running daemon holds it), so it
//! proceeds without it and documents the interaction honestly — see
//! [`restore`].

pub(crate) mod diff;
pub(crate) mod log;
pub(crate) mod restore;
pub(crate) mod snapshot;

// Time rendering and the `--at`/`--since` grammar live in vard-core (VRD-18
// renders the same RFC 3339); re-exported here so the commands reach it as
// `super::timefmt`, unchanged by the move.
pub(crate) use vard_core::timefmt;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use vard_core::{GitBackend, VcsBackend, VcsError, WatchSpec};

use crate::cli::{ColorWhen, OutputFormat};
use crate::config::{Config, ConfigError, ResolvedWatch};
use crate::journal::Journal;
use crate::output::format;
use crate::output::pager::{should_page, spawn_pager_or_passthrough};
use crate::output::palette::{self, Palette};
use crate::output::record::{self, Record};
use crate::paths::{self, HomeNotFound};
use crate::watch::select;

/// A command failure carrying the message to print and the process exit code
/// (2 for an error, 1 for "attention needed" such as an unsafe repository).
pub(crate) struct CmdError {
    message: String,
    code: u8,
}

impl CmdError {
    /// An error (exit code 2).
    pub(crate) fn err(message: impl Into<String>) -> Self {
        CmdError {
            message: message.into(),
            code: 2,
        }
    }

    /// An "attention needed" outcome (exit code 1): the command did not fail,
    /// but it also did not fully complete — e.g. a repository was not in a safe
    /// state, or a git lock was contended.
    pub(crate) fn attention(message: impl Into<String>) -> Self {
        CmdError {
            message: message.into(),
            code: 1,
        }
    }

    /// The higher-severity of two exit codes (2 beats 1 beats 0), for
    /// aggregating per-watch outcomes.
    fn worse(a: u8, b: u8) -> u8 {
        a.max(b)
    }

    /// The error's human-readable message.
    fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) type CmdResult = Result<(), CmdError>;

/// The filesystem locations the commands read, resolved once. Kept together so
/// tests (and the dispatch code) thread one value.
struct CmdPaths {
    config_file: PathBuf,
    journal_dir: PathBuf,
    request_dir: PathBuf,
    lock_file: PathBuf,
}

impl CmdPaths {
    fn from_xdg() -> Result<CmdPaths, HomeNotFound> {
        Ok(CmdPaths {
            config_file: paths::config_file()?,
            journal_dir: paths::journal_dir()?,
            request_dir: paths::request_dir()?,
            lock_file: paths::lock_file()?,
        })
    }
}

/// Resolved output settings shared by every command's emitter.
struct OutCtx {
    /// The effective format after resolving the global `--format` against the
    /// destination.
    format: OutputFormat,
    /// The raw `--format` flag, before destination resolution. `diff` needs it
    /// to tell an explicit `--format json` (rejected) from the piped default.
    raw_format: Option<OutputFormat>,
    palette: Palette,
    term_width: usize,
    term_height: usize,
    is_tty: bool,
}

impl OutCtx {
    fn resolve(color: ColorWhen, format_flag: Option<OutputFormat>) -> OutCtx {
        let is_tty = io::stdout().is_terminal();
        let (term_width, term_height) = terminal_size::terminal_size()
            .map(|(w, h)| (w.0 as usize, h.0 as usize))
            .unwrap_or((80, 24));
        OutCtx {
            format: format::resolve(format_flag, is_tty),
            raw_format: format_flag,
            palette: palette::resolve_with_tty(color, is_tty),
            term_width,
            term_height,
            is_tty,
        }
    }
}

/// Loads and validates the config, erroring when the file is missing (you
/// cannot snapshot, log, diff, or restore a watch that was never added).
fn load_config(config_file: &std::path::Path) -> Result<Config, CmdError> {
    match Config::load(config_file) {
        Ok(config) => Ok(config),
        Err(ConfigError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Err(CmdError::err(format!(
                "no config file at {}; add a watch first with `vard watch add`",
                config_file.display()
            )))
        }
        Err(err) => Err(CmdError::err(err.to_string())),
    }
}

/// Resolves every watch (paused included), erroring if the config does not
/// fully validate. Order matches `config.watches`, so a [`select`] index lines
/// up with the returned vector.
fn resolve_all(config: &Config) -> Result<Vec<ResolvedWatch>, CmdError> {
    config
        .resolve_all()
        .map_err(|e| CmdError::err(e.to_string()))
}

/// Resolves a required `<name|path>` selector to its [`ResolvedWatch`].
fn select_one(config: &Config, target: &str) -> Result<ResolvedWatch, CmdError> {
    let index = select::select_watch(config, target).map_err(|e| CmdError::err(e.to_string()))?;
    let mut resolved = resolve_all(config)?;
    // `select_watch` indexes `config.watches`; `resolve_all` preserves that
    // order, so the same index selects the resolved watch.
    Ok(resolved.swap_remove(index))
}

/// Opens the git backend for a watch through the engine's shared branch policy,
/// mapping the failure to a command error.
fn open_backend(spec: &WatchSpec) -> Result<GitBackend, CmdError> {
    vard_core::open_git_backend(spec)
        .map_err(|e| CmdError::err(format!("opening watch {:?}: {e}", spec.name())))
}

/// Takes one in-process snapshot for `watch_name`, bracketing the backend call
/// in the per-watch operation journal exactly as the daemon's event handler
/// does: `begin` before, `complete` after — on success, no-op, AND failure —
/// so a crash mid-commit leaves a recoverable dangling record and a clean exit
/// leaves none. Journal I/O trouble is warned, never fatal (matching the
/// daemon), so a journaling hiccup cannot block a manual snapshot.
///
/// Returns the backend's own result untouched; the caller maps it to exit
/// semantics.
fn journaled_snapshot(
    journal_dir: &std::path::Path,
    watch_name: &str,
    backend: &GitBackend,
    req: &vard_core::SnapshotRequest,
) -> Result<Option<vard_core::SnapshotOutcome>, VcsError> {
    let journal = Journal::in_dir(journal_dir, watch_name);
    if let Err(err) = journal.begin("snapshot") {
        eprintln!("vard: journal begin for {watch_name:?}: {err}");
    }
    let result = backend.snapshot(req);
    if let Err(err) = journal.complete() {
        eprintln!("vard: journal complete for {watch_name:?}: {err}");
    }
    result
}

/// Emits a list of records in the resolved format under a collective noun.
fn emit_records(out: &OutCtx, records: &[Record], noun: &str) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => {
            record::render_records(&mut w, &out.palette, records, noun, out.term_width)
        }
        OutputFormat::Json => record::render_json(&mut w, records),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, records),
    };
    finish_write(res)
}

/// Emits a single command result: a human line in the records form, or a
/// single JSON object in the machine forms.
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

/// Emits raw bytes to stdout, paging through the resolved pager when they
/// overflow a terminal, and passing them through untouched when piped. Used for
/// the raw unified diff, which bypasses record shaping entirely.
fn emit_raw_paged(out: &OutCtx, buf: &[u8], context: &str) -> CmdResult {
    let line_count = buf.iter().filter(|b| **b == b'\n').count();
    let res = if should_page(
        line_count,
        /* no_pager */ false,
        out.is_tty,
        out.term_height,
    ) {
        let mut stderr = io::stderr();
        let mut stdout = io::stdout().lock();
        spawn_pager_or_passthrough(buf, &mut stdout, &mut stderr, context)
    } else {
        io::stdout().lock().write_all(buf)
    };
    finish_write(res)
}

/// Folds a write result into a [`CmdResult`], treating a broken pipe (the
/// reader went away, e.g. `| head`) as success rather than an error.
fn finish_write(res: io::Result<()>) -> CmdResult {
    match res {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(CmdError::err(format!("writing output: {e}"))),
    }
}

/// Maps a [`CmdResult`] to a process exit code, printing any error to stderr.
fn finish(result: CmdResult) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vard: {}", err.message);
            std::process::ExitCode::from(err.code)
        }
    }
}
