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

use std::io;
use std::path::PathBuf;

use vard_core::{GitBackend, VcsBackend, VcsError, WatchSpec};

use crate::config::{Config, ConfigError, ResolvedWatch};
use crate::journal::Journal;
use crate::paths::{self, HomeNotFound};
use crate::watch::select;

// The command-outcome layer (error/exit-code, output resolution, emitters) is
// shared with the `watch` commands; re-exported so the submodules reach it as
// `super::…`, unchanged by the extraction.
pub(crate) use crate::command::{
    CmdError, CmdResult, OutCtx, emit_action, emit_raw_paged, emit_records, finish,
};

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

/// Runs `body` bracketed in the per-watch operation journal: `begin(op)`
/// before, `complete` after — on every outcome, success or failure — exactly as
/// the daemon's event handler brackets a commit window. A crash mid-operation
/// therefore leaves one recoverable dangling record and a clean exit leaves
/// none. Journal I/O trouble is warned, never fatal (matching the daemon), so a
/// journaling hiccup cannot block a manual operation.
///
/// The one journal-bracket helper the CLI paths share (`snapshot`'s in-process
/// commit, `restore`'s protective-snapshot-plus-checkout). Only ever called
/// while this process holds the instance lock — the journal's single-writer
/// invariant.
fn journaled<T>(
    journal_dir: &std::path::Path,
    watch_name: &str,
    op: &str,
    body: impl FnOnce() -> T,
) -> T {
    let journal = Journal::in_dir(journal_dir, watch_name);
    if let Err(err) = journal.begin(op) {
        eprintln!("vard: journal begin for {watch_name:?}: {err}");
    }
    let result = body();
    if let Err(err) = journal.complete() {
        eprintln!("vard: journal complete for {watch_name:?}: {err}");
    }
    result
}

/// Takes one in-process snapshot for `watch_name`, bracketed in the per-watch
/// operation journal via [`journaled`]. Returns the backend's own result
/// untouched; the caller maps it to exit semantics.
fn journaled_snapshot(
    journal_dir: &std::path::Path,
    watch_name: &str,
    backend: &GitBackend,
    req: &vard_core::SnapshotRequest,
) -> Result<Option<vard_core::SnapshotOutcome>, VcsError> {
    journaled(journal_dir, watch_name, "snapshot", || {
        backend.snapshot(req)
    })
}
