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
//! * **In-process mutations** ([`snapshot`] and [`restore`]'s protective
//!   snapshot + checkout) run under the watch's per-watch **operation lock**
//!   via [`with_op_gate`] — acquire the op lock, `begin`, run, `complete` — the
//!   same structural one-writer-per-watch invariant the engine worker holds
//!   (VRD-37). This serializes a CLI mutation against a running daemon's worker
//!   on that watch and leaves a recoverable journal record either way, so a crash
//!   mid-operation is recoverable whether or not a daemon runs.
//! * **Dispatch** for `snapshot` uses the single-instance flock as a race-free
//!   discriminator of *who should do the work*: if the CLI can take the instance
//!   lock, no daemon is running and it snapshots in-process; if the lock is held,
//!   a daemon owns the repositories and the CLI hands it a request file instead
//!   (see [`crate::daemon`]). This is separate from the op lock, which is *who
//!   may mutate*.
//!
//! `restore` consults the instance lock for the same dispatch decision but no
//! longer depends on holding it to journal — the op lock covers the daemon-running
//! case too. See [`restore`].

pub(crate) mod diff;
pub(crate) mod log;
pub(crate) mod restore;
pub(crate) mod snapshot;
pub(crate) mod sync;

// Time rendering and the `--at`/`--since` grammar live in vard-core (VRD-18
// renders the same RFC 3339); re-exported here so the commands reach it as
// `super::timefmt`, unchanged by the move.
pub(crate) use vard_core::timefmt;

use std::io;
use std::path::PathBuf;

use std::time::Duration;

use vard_core::{GitBackend, VcsBackend, VcsError, WatchSpec};

use crate::config::{Config, ConfigError, ResolvedWatch};
use crate::journal::JournalOpGate;
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

/// How long a CLI operation waits for a watch's per-watch **operation lock**
/// before conceding another operation holds it. When no daemon runs (the common
/// in-process case) the lock is free — the instance lock already serialized any
/// peer CLI — so this only bites when a daemon's worker is mid-commit on the same
/// watch (a `restore` under a running daemon), where a brief wait then a
/// "retry" is the honest answer.
const OP_GATE_BUDGET: Duration = Duration::from_secs(10);

/// The result of running a mutating CLI operation under a watch's op-lock +
/// journal bracket (VRD-37): the op lock makes one-writer-per-watch structural,
/// so a CLI operation that cannot get it must not mutate.
enum Gated<T> {
    /// The op lock was acquired; the body ran under a `begin`→`complete` bracket.
    Ran(T),
    /// Another operation held the op lock for the whole budget; nothing ran.
    Busy,
    /// The op gate could not be evaluated (op-lock or `begin`-write I/O trouble)
    /// while a **daemon coexists**, so we could not prove exclusion against its
    /// worker. Fail closed: nothing ran. Only produced under
    /// [`OpGateActor::DaemonCoexists`] — a sole CLI actor warns and proceeds
    /// instead (git's own `index.lock` still serializes).
    LockFailed(String),
}

/// Whether the CLI can prove itself the sole vard actor for this operation,
/// which decides how an op-gate *error* is handled (F3).
#[derive(Clone, Copy)]
enum OpGateActor {
    /// The CLI holds the instance lock (or no daemon runs): it is provably the
    /// only vard process that could mutate this watch. An op-gate I/O error is
    /// non-fatal — git's own `index.lock` still serializes — so warn and proceed
    /// (matching the pre-VRD-37 "a journaling hiccup is non-fatal" behavior).
    Sole,
    /// A daemon owns the repositories (the CLI did not take the instance lock).
    /// An op-gate error means we cannot prove exclusion against the daemon's
    /// worker, so fail closed ([`Gated::LockFailed`]) — nothing is changed.
    DaemonCoexists,
}

/// Runs `body` under the watch's operation gate: acquire the op lock (a bounded
/// blocking wait), write the journal `begin`, run `body`, then `complete`
/// (compact + release) on every outcome. A crash mid-operation therefore leaves
/// one recoverable dangling record; a clean exit leaves none. Op-lock contention
/// past [`OP_GATE_BUDGET`] returns [`Gated::Busy`] so the caller reports "retry"
/// rather than mutating without the lock.
///
/// This is the single op-gate bracket helper the CLI paths share (`snapshot`'s
/// in-process commit, `restore`'s protective-snapshot-plus-checkout). It holds
/// the per-watch op lock — the structural single-writer invariant — independently
/// of the instance lock, so it protects the `restore`-under-a-daemon path too
/// (the daemon's worker contends on the same op lock). The gate is keyed by the
/// watch's repository path, so `watch_name` only words a diagnostic.
///
/// `actor` decides how an op-gate *error* is handled (F3): a
/// [sole CLI actor](OpGateActor::Sole) warns and proceeds, but under a
/// [coexisting daemon](OpGateActor::DaemonCoexists) an error fails closed
/// ([`Gated::LockFailed`]) rather than mutate a watch the daemon's worker might
/// be touching.
fn with_op_gate<T>(
    journal_dir: &std::path::Path,
    repo_path: &std::path::Path,
    watch_name: &str,
    op: &str,
    actor: OpGateActor,
    body: impl FnOnce() -> T,
) -> Gated<T> {
    let gate = JournalOpGate::for_repo_in_dir(journal_dir, repo_path);
    match gate.begin_blocking(op, OP_GATE_BUDGET) {
        Ok(Some(guard)) => {
            let result = body();
            guard.complete();
            Gated::Ran(result)
        }
        Ok(None) => Gated::Busy,
        Err(err) => match actor {
            // Sole vard actor: git's own index.lock still serializes, so warn and
            // run WITHOUT the bracket (only the recovery record is missing).
            OpGateActor::Sole => {
                eprintln!("vard: op gate for {watch_name:?}: {err}");
                Gated::Ran(body())
            }
            // A daemon coexists: we cannot prove exclusion against its worker, so
            // fail closed — nothing runs.
            OpGateActor::DaemonCoexists => Gated::LockFailed(err),
        },
    }
}

/// Takes one in-process snapshot for the watch rooted at `repo_path` under the
/// op-lock + journal bracket via [`with_op_gate`]. Returns the backend's own
/// result untouched inside [`Gated`]; the caller maps it (and the busy case) to
/// exit semantics.
fn journaled_snapshot(
    journal_dir: &std::path::Path,
    repo_path: &std::path::Path,
    watch_name: &str,
    backend: &GitBackend,
    req: &vard_core::SnapshotRequest,
) -> Gated<Result<Option<vard_core::SnapshotOutcome>, VcsError>> {
    // In-process `snapshot` runs only while holding the instance lock (no daemon),
    // so the CLI is the sole vard actor — an op-gate error is non-fatal.
    with_op_gate(
        journal_dir,
        repo_path,
        watch_name,
        "snapshot",
        OpGateActor::Sole,
        || backend.snapshot(req),
    )
}
