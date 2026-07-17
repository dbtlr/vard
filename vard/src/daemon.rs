//! The `vard run` daemon shell: the long-lived supervisor that turns the file
//! config into a running [`Engine`] and keeps it healthy.
//!
//! # Responsibilities
//!
//! 1. Acquire the single-instance [`InstanceLock`] so only one daemon owns a
//!    state directory (contention exits with code 2).
//! 2. Load and resolve the file [`Config`] into watch specs; a missing or
//!    watch-less config is a startup error (nothing to do).
//! 3. Recover any stale git index locks left by a previous crash (per-watch
//!    [`Journal`] recovery), then build and start the engine.
//! 4. Supervise it: log every bus [`Event`] (the per-watch journal is bracketed
//!    structurally by the engine worker's op-lock guard, VRD-37 — no longer from
//!    the bus here), reload on SIGHUP or a config-file edit, rebuild on a dead
//!    signal source with exponential backoff, drain snapshot/sync request files,
//!    and shut down cleanly on SIGINT/SIGTERM.
//!
//! # The request-file contract (ADR 0004, spec §11)
//!
//! The CLI (VRD-15/16) and the daemon do not share memory; they rendezvous
//! through files under [`paths::request_dir`]. The schema is owned by
//! [`crate::request`] (one serde type both sides share); each file is a small
//! TOML document:
//!
//! ```toml
//! kind = "snapshot"          # or "sync"
//! watch = "vault"            # optional; omitted means "every watch"
//! requested_at = 1752000000  # unix seconds; a request too old is discarded
//! ```
//!
//! The daemon polls the directory, and for each file:
//!
//! - `kind = "snapshot"` injects a manual `Trigger::Manual` snapshot via
//!   [`EngineHandle::trigger`](vard_core::EngineHandle::trigger) — for the named
//!   watch, or every watch when `watch` is omitted. An unknown watch is logged
//!   and skipped.
//! - `kind = "sync"` requests a manual sync cycle via
//!   [`EngineHandle::request_sync`](vard_core::EngineHandle::request_sync) — for
//!   the named watch, or every watch when `watch` is omitted. A watch that
//!   cannot sync accepts the request and does nothing; an unknown watch is
//!   logged and skipped.
//!
//! Every consumed file is deleted — including a malformed one, which is logged
//! and removed so a single poison file cannot wedge the queue.
//!
//! Writers MUST create a request atomically: write the payload to a temporary
//! name in the same directory (a leading-dot or `.tmp` name) and `rename(2)` it
//! to its final `*.toml` name — atomic on POSIX. The daemon consumes only
//! *settled* names (plain `*.toml`, not hidden, no temp suffix) and leaves
//! everything else in place, so a request mid-write is never half-read or
//! deleted.
//!
//! # Config and request watching: polling, not a notifier
//!
//! Both the config file and the request directory are watched by polling on a
//! fixed interval (default every [`DEFAULT_POLL_INTERVAL`]), not by arming a
//! `vard-core` [`Watcher`](vard_core::Watcher) on them. Polling is simpler and
//! avoids a feedback loop on the request directory (the daemon itself deletes
//! files there, which a notifier would report back as activity); a couple of
//! seconds of latency on a control-plane path is immaterial. Config change
//! detection keys off a content fingerprint rather than mtime: on filesystems
//! with 1-second mtime granularity, two back-to-back CLI writes can land in the
//! same tick and be indistinguishable by timestamp alone (VRD-35). The
//! fingerprint is debounced (see [`ConfigDebounce`]) so an editor writing
//! several times in a row still collapses into a single rebuild.
//!
//! # Testability
//!
//! The IO loop is factored around an explicit [`DaemonPaths`] (config file, lock
//! file, request dir, journal dir) rather than reaching into XDG, mirroring the
//! `*_at`/`in_dir` cores elsewhere in the crate, and it takes its shutdown/reload
//! signals as [`Notify`] handles rather than raw OS signals. Tests drive
//! [`run_daemon`] against a tempdir with a cancellation `Notify` standing in for
//! SIGTERM; production wires the real paths and signal handlers in [`run`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::Notify;
use tokio::time::Instant as TokioInstant;
use tracing::{debug, error, info, warn};

use vard_core::{
    Engine, EngineError, EngineHandle, Event, EventReceiver, RecvError, TroubleKind, WatchSpec,
    WatchState,
};

use crate::config::{Config, ConfigError, LogLevel};
use crate::health;
use crate::hooks::{self, HooksConfig, HooksRunnerHandle, RunnerSnapshot};
use crate::instance::{InstanceLock, LockError, LockRole};
use crate::journal::{self, Journal, JournalOpGate, RecoveryReport, SweepOpts};
use crate::paths::{self, HomeNotFound};
use crate::request::{self, Request, RequestKind};

/// One supervised watch's durable identity: its stable config `name` (how the
/// engine and request files refer to it), its repository `path` (used to drive
/// recovery), and — computed **once** at construction — its canonical
/// `identity` path and `journal_key` filename. Caching the latter two is what
/// keeps a bus event's `begin`/`complete` addressing the same journal file even
/// if the directory is removed mid-operation, and spares a canonicalize syscall
/// per event and per reload membership check (see [`journal::identity_path`]).
///
/// # Cached key vs. fresh canonicalize (retargeted symlinks) — F9
///
/// The op-lock and journal keys are canonicalized **once** here and cached for
/// the life of this identity (one engine generation). That is deliberate — a key
/// that stayed stable is exactly what lets a `begin` and its `complete` address
/// one file even after the directory vanishes. The trade-off: if a watch's path
/// is a symlink and it is **retargeted to a different repository** while the
/// daemon runs, this identity keeps keying by the *old* canonical target until
/// the daemon is **restarted** (or the engine generation is rebuilt). A CLI
/// operation, which canonicalizes freshly, would key by the *new* target, so the
/// two would take *different* op locks and not exclude each other across that
/// window. Strict cross-process exclusion after a symlink retarget therefore
/// requires a daemon restart. In practice `vard watch add` stores the already-
/// canonical path and repositories are not moved under a live daemon, so the two
/// forms coincide; this note records the residual for the symlink-retarget case.
#[derive(Debug, Clone)]
struct WatchIdentity {
    name: String,
    path: PathBuf,
    /// The canonical-or-textual identity path, recorded in `begin`'s `path=`.
    identity: PathBuf,
    /// The journal filename key, cached from [`identity`](Self::identity).
    journal_key: String,
    /// The operation-lock filename key, cached from [`identity`](Self::identity)
    /// beside `journal_key` (VRD-37). Same prefix, `.lock` suffix.
    lock_key: String,
}

impl WatchIdentity {
    /// Builds an identity for a watch rooted at `path`, resolving its canonical
    /// identity and its journal + op-lock keys once.
    fn new(name: String, path: PathBuf) -> WatchIdentity {
        let identity = journal::identity_path(&path);
        let journal_key = journal::journal_file_name_for_identity(&identity);
        let lock_key = journal::lock_file_name_for_identity(&identity);
        WatchIdentity {
            name,
            path,
            identity,
            journal_key,
            lock_key,
        }
    }

    /// This watch's journal for the given directory, built from the cached
    /// identity and key so no canonicalization happens per event.
    fn journal(&self, journal_dir: &Path) -> Journal {
        Journal::for_identity_in_dir(journal_dir, &self.identity, &self.journal_key)
    }

    /// This watch's operation gate (op lock + journal) for the given directory,
    /// injected into the engine so every commit is bracketed under the op lock.
    fn op_gate(&self, journal_dir: &Path) -> JournalOpGate {
        JournalOpGate::new(
            journal_dir,
            &self.identity,
            &self.journal_key,
            &self.lock_key,
        )
    }

    /// The set of journal keys across a slice of identities — the "which watches
    /// own a journal" membership used by the reload recovery filter and the
    /// drain-on-remove survivor check.
    fn key_set(watches: &[WatchIdentity]) -> std::collections::HashSet<String> {
        watches.iter().map(|w| w.journal_key.clone()).collect()
    }
}

/// The supervised watch identities for one engine generation (name for request
/// fan-out, path/keys for the op gate and drain-on-remove recovery). Rebuilt
/// atomically on each engine swap. Derefs to the identity slice so the iterating
/// callers (request fan-out, membership sets) read it as a plain
/// `&[WatchIdentity]`.
///
/// It no longer carries a name→identity index: the journal used to be looked up
/// by a bus event's watch *name* here (VRD-37 moved that into the engine worker's
/// op-lock gate), so nothing needs O(1) name resolution any more.
struct WatchSet {
    watches: Vec<WatchIdentity>,
}

impl WatchSet {
    fn new(watches: Vec<WatchIdentity>) -> WatchSet {
        WatchSet { watches }
    }
}

impl std::ops::Deref for WatchSet {
    type Target = [WatchIdentity];
    fn deref(&self) -> &[WatchIdentity] {
        &self.watches
    }
}

/// How often the supervisor polls the config file's content and drains the
/// request directory. A control-plane cadence: fast enough that a manual
/// snapshot request feels responsive, slow enough to be negligible overhead.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Base delay before the first rebuild after a watch's signal source dies. The
/// backoff doubles from here on repeated quick failures (see
/// [`SourceDiedBackoff`]).
const SOURCE_DIED_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Cap on the source-died rebuild backoff: a persistently broken engine retries
/// at most this often rather than hammering.
const SOURCE_DIED_BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// How long the engine must run without a fresh source-died failure before the
/// backoff resets to [`SOURCE_DIED_BACKOFF_BASE`]. A failure after this long is
/// treated as a new incident, not a continuation of the previous storm.
const SOURCE_DIED_BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

/// How long the daemon waits out a *transient CLI* lock holder before refusing
/// to start. A CLI `snapshot` taking the lock in-process finishes in well under
/// this; a duplicate *daemon* holder is refused immediately, not waited on.
const CLI_HOLDER_WAIT: Duration = Duration::from_secs(3);

/// The concrete filesystem locations the daemon reads and writes, resolved once
/// at startup. Held explicitly (rather than re-derived from XDG on each use) so
/// tests can inject a tempdir; [`from_xdg`](Self::from_xdg) is the production
/// resolver.
#[derive(Debug, Clone)]
pub(crate) struct DaemonPaths {
    /// `config.toml`, loaded and reloaded into watch specs.
    pub config_file: PathBuf,
    /// The single-instance `flock` target.
    pub lock_file: PathBuf,
    /// The request-file queue drained each poll.
    pub request_dir: PathBuf,
    /// The directory holding per-watch operation journals.
    pub journal_dir: PathBuf,
    /// The parent directory under which each syncing watch gets its out-of-tree
    /// reconcile scratch worktree (`<reconcile_dir>/<journal-key>`).
    pub reconcile_dir: PathBuf,
    /// The health file rewritten on every watch state change and cleared on
    /// clean shutdown; `vard notify` reads it.
    pub health_file: PathBuf,
}

impl DaemonPaths {
    /// Resolves every daemon path from the XDG base directories.
    pub(crate) fn from_xdg() -> Result<DaemonPaths, HomeNotFound> {
        Ok(DaemonPaths {
            config_file: paths::config_file()?,
            lock_file: paths::lock_file()?,
            request_dir: paths::request_dir()?,
            journal_dir: paths::journal_dir()?,
            reconcile_dir: paths::reconcile_dir()?,
            health_file: paths::health_file()?,
        })
    }

    /// The out-of-tree reconcile scratch path for the watch rooted at
    /// `repo_path`: `<reconcile_dir>/<journal-key>`, keyed by the same repository
    /// identity as the watch's journal so recovery can address it.
    fn scratch_for(&self, repo_path: &Path) -> PathBuf {
        self.reconcile_dir
            .join(journal::journal_file_name(repo_path))
    }
}

/// Production entry point for `vard run`. Resolves paths, takes the
/// single-instance lock, validates the config, initializes logging, then runs
/// the async supervisor to completion. Returns the process exit code: `0` on a
/// clean shutdown, `2` on any startup error (lock contention, missing or invalid
/// config), matching spec §11's "0 healthy, 2 error" convention.
pub(crate) fn run() -> ExitCode {
    let paths = match DaemonPaths::from_xdg() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("vard: {err}");
            return ExitCode::from(2);
        }
    };

    // Hold the lock for the daemon's whole lifetime; contention exits 2. A
    // duplicate daemon is refused outright; a transient CLI holder is waited
    // out briefly, and its message names it as a passing command (not a rival
    // daemon) so the diagnostic is honest.
    let lock = match InstanceLock::acquire_for_daemon(&paths.lock_file, CLI_HOLDER_WAIT) {
        Ok(lock) => lock,
        Err(LockError::Held {
            holder,
            role: Some(LockRole::Daemon),
            ..
        }) => {
            eprintln!(
                "vard: another vard daemon ({}) already owns this state directory; \
                 only one daemon may run per directory",
                holder_desc(holder)
            );
            return ExitCode::from(2);
        }
        Err(LockError::Held { holder, .. }) => {
            eprintln!(
                "vard: a transient vard command ({}) is holding the instance lock; \
                 it should release it shortly — retry `vard run`",
                holder_desc(holder)
            );
            return ExitCode::from(2);
        }
        Err(err) => {
            eprintln!("vard: {err}");
            return ExitCode::from(2);
        }
    };

    // Validate the config up front so a missing/empty/invalid file exits 2 with
    // a clear message, before any runtime is spun up.
    let config = match load_startup_config(&paths) {
        Ok(config) => config,
        Err(code) => return code,
    };

    init_tracing(config.daemon.log_level);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("vard: could not start async runtime: {err}");
            return ExitCode::from(2);
        }
    };

    let result = runtime.block_on(async move {
        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        install_signal_handlers(Arc::clone(&shutdown), Arc::clone(&reload));
        run_daemon(paths, config, shutdown, reload, DEFAULT_POLL_INTERVAL).await
    });

    // Wind the runtime down, then release the instance lock explicitly so its
    // ordering after shutdown is documented rather than incidental.
    drop(runtime);
    drop(lock);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vard: {err}");
            ExitCode::from(2)
        }
    }
}

/// Renders a lock holder's PID for a diagnostic, or a placeholder when it could
/// not be read.
fn holder_desc(holder: Option<u32>) -> String {
    holder.map_or_else(|| "PID unknown".to_string(), |pid| format!("PID {pid}"))
}

/// Loads and validates the config for startup, mapping every failure to a clear
/// stderr message and exit code 2. A missing file and a config that *defines*
/// no watches are both "nothing to do" errors that point at `vard watch add`.
///
/// A config that defines watches but has them *all paused* is not an error: the
/// daemon starts idle (supervising nothing) and a `vard watch resume`
/// hot-reloads it back to work. The zero-defined check therefore keys off
/// [`Config::resolve_all`] (every defined watch, paused or not), not
/// [`Config::resolve`] (active watches only). `resolve_all` also validates, so a
/// genuinely invalid config still exits 2.
fn load_startup_config(paths: &DaemonPaths) -> Result<Config, ExitCode> {
    let config = match Config::load(&paths.config_file) {
        Ok(config) => config,
        Err(ConfigError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "vard: no config file at {}; there is nothing to watch yet. \
                 Add a watch with `vard watch add`.",
                paths.config_file.display()
            );
            return Err(ExitCode::from(2));
        }
        Err(err) => {
            eprintln!("vard: {err}");
            return Err(ExitCode::from(2));
        }
    };

    match config.resolve_all() {
        Ok(defined) if defined.is_empty() => {
            eprintln!(
                "vard: config at {} defines no watches; add one with `vard watch add`.",
                paths.config_file.display()
            );
            Err(ExitCode::from(2))
        }
        Ok(_) => Ok(config),
        Err(err) => {
            eprintln!("vard: {err}");
            Err(ExitCode::from(2))
        }
    }
}

/// Initializes stderr logging at the config's level. Best-effort: a second call
/// in one process (e.g. across tests) is a no-op rather than a panic. The level
/// is fixed for the process lifetime — a `log_level` change in the config takes
/// effect on the next restart, not on reload, since the daemon does not arm a
/// reloadable filter layer.
fn init_tracing(level: LogLevel) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(log_level_to_tracing(level))
        .with_writer(std::io::stderr)
        .try_init();
}

/// Maps the config's [`LogLevel`] to a [`tracing::Level`].
fn log_level_to_tracing(level: LogLevel) -> tracing::Level {
    match level {
        LogLevel::Error => tracing::Level::ERROR,
        LogLevel::Warn => tracing::Level::WARN,
        LogLevel::Info => tracing::Level::INFO,
        LogLevel::Debug => tracing::Level::DEBUG,
        LogLevel::Trace => tracing::Level::TRACE,
    }
}

/// Exit codes for a hard exit forced by a second termination signal: the
/// conventional `128 + signo` for the signal that forced it.
const SECOND_SIGINT_EXIT_CODE: i32 = 130;
const SECOND_SIGTERM_EXIT_CODE: i32 = 143;

/// Spawns background tasks that translate OS signals into the supervisor's
/// [`Notify`] handles: SIGINT/SIGTERM request shutdown, SIGHUP requests a
/// reload. Each `notify_one` collapses into a single wakeup, which is exactly
/// the debounce we want for a burst of SIGHUPs.
///
/// The termination handler keeps listening after the first signal: a *second*
/// SIGINT/SIGTERM during a slow graceful drain hard-exits the process
/// immediately (the conventional `128 + signo` code) so a user can always
/// escalate a hung shutdown — tokio's installed handler would otherwise
/// swallow the repeat and leave no way out short of SIGKILL.
fn install_signal_handlers(shutdown: Arc<Notify>, reload: Arc<Notify>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(sig) => sig,
            Err(err) => {
                error!(error = %err, "could not install SIGINT handler");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(err) => {
                error!(error = %err, "could not install SIGTERM handler");
                return;
            }
        };
        let mut already_asked = false;
        loop {
            let (received, exit_code) = tokio::select! {
                sig = sigint.recv() => (sig, SECOND_SIGINT_EXIT_CODE),
                sig = sigterm.recv() => (sig, SECOND_SIGTERM_EXIT_CODE),
            };
            if received.is_none() {
                // The signal stream closed; nothing more to translate.
                return;
            }
            if already_asked {
                eprintln!("vard: received a second termination signal; exiting immediately");
                std::process::exit(exit_code);
            }
            already_asked = true;
            shutdown.notify_one();
        }
    });

    tokio::spawn(async move {
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(sig) => sig,
            Err(err) => {
                error!(error = %err, "could not install SIGHUP handler");
                return;
            }
        };
        while sighup.recv().await.is_some() {
            reload.notify_one();
        }
    });
}

/// The daemon's startup errors, surfaced to [`run`] as an exit-2 message. Bad
/// edits are caught here (config load/resolve) before the engine is built.
#[derive(Debug)]
#[non_exhaustive]
enum StartupError {
    /// The config file could not be loaded or resolved.
    Config(ConfigError),
    /// The config resolved to no watches — nothing to run.
    NoWatches,
    /// The engine could not be built or started.
    Engine(EngineError),
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupError::Config(err) => write!(f, "{err}"),
            StartupError::NoWatches => f.write_str("config defines no watches"),
            StartupError::Engine(err) => write!(f, "could not start engine: {err}"),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StartupError::Config(err) => Some(err),
            StartupError::Engine(err) => Some(err),
            StartupError::NoWatches => None,
        }
    }
}

/// Runs the daemon: recovers stale locks, builds the initial engine, and
/// supervises it until `shutdown` fires. Takes the already-loaded startup
/// [`Config`] ([`run`] loads it once, for validation and the log level) rather
/// than re-reading the file; reloads always re-read from disk. Factored out of
/// [`run`] so tests can drive it against injected paths with a cancellation
/// `Notify` in place of a real SIGTERM. `reload` stands in for SIGHUP.
async fn run_daemon(
    paths: DaemonPaths,
    config: Config,
    shutdown: Arc<Notify>,
    reload: Arc<Notify>,
    poll_interval: Duration,
) -> Result<(), StartupError> {
    // `resolve_all` gives every *defined* watch (paused or not) and validates
    // them; `defined.is_empty()` is the true "nothing configured" condition.
    // The active specs (paused filtered out) may be empty when every watch is
    // paused — a legitimate idle daemon that a later resume hot-reloads.
    let defined = config.resolve_all().map_err(StartupError::Config)?;
    if defined.is_empty() {
        return Err(StartupError::NoWatches);
    }
    let specs: Vec<WatchSpec> = inject_scratch_dirs(
        &paths,
        defined
            .iter()
            .filter(|w| !w.paused)
            .map(|w| w.spec.clone())
            .collect(),
    );
    if specs.is_empty() {
        info!("all configured watches are paused; starting idle (a resume will reload)");
    }

    // Remove any crash-leftover health file up front (after lock acquisition,
    // before recovery/engine build). A stale document must not be trusted during
    // the startup window: until the engine is built and the first fresh document
    // is written, `vard notify` sees the lock held with no readable health and
    // reports an honest "starting or stopping" line rather than a stale problem
    // set (or a false all-clear).
    health::clear(&paths.health_file);

    // Migrate any legacy name-keyed journals to their path keys and sweep old
    // orphans, then recover stale locks — migration first, so a pre-upgrade
    // journal is under its path key before recovery looks for it there. Recovery
    // runs over every *defined* watch, paused included: a watch that crashed while
    // active and was then paused still owns a provably-ours stale lock wedging the
    // user's own git, and recovery is read-only unless the lock proves ours and
    // stale — so covering paused watches is safe and never leaves such a lock.
    reconcile_journals(&paths, &defined);
    recover_stale_locks(&paths, defined.iter().map(|w| &w.spec));

    let hooks_config = build_hooks_config(&config, &defined);
    let (handle, events, watches, skipped, hooks) =
        build_started_engine_from_specs(&paths.journal_dir, specs, hooks_config, None)
            .await
            .map_err(StartupError::Engine)?;

    supervise(
        paths,
        handle,
        events,
        watches,
        skipped,
        hooks,
        shutdown,
        reload,
        poll_interval,
    )
    .await;
    Ok(())
}

/// Runs the journal-directory reconciliation (legacy-name migration + orphan
/// GC) for the currently configured watches, logging anything it did. Paused
/// watches are included — a paused watch still owns its journal — so `defined`
/// (every resolved watch) is the right input, not the active specs. Never fails
/// the daemon: reconciliation folds all trouble into its report.
fn reconcile_journals(paths: &DaemonPaths, defined: &[crate::config::ResolvedWatch]) {
    let owners: Vec<(&str, &Path)> = defined
        .iter()
        .map(|rw| (rw.spec.name(), rw.spec.path()))
        .collect();
    let report = journal::reconcile_journals(&paths.journal_dir, &owners, SweepOpts::new());
    if report.is_noop() {
        return;
    }
    for (from, to) in &report.migrated {
        info!(from = %from.display(), to = %to.display(), "migrated a legacy journal to its path key");
    }
    for (journal_path, rep) in &report.recovered {
        // The orphan sweep recovered a since-removed watch's stale lock from the
        // path encoded in its own journal. A removed lock is warn-worthy; a
        // foreign or unsettled one is info.
        match rep {
            RecoveryReport::LockRemoved { .. } => {
                warn!(journal = %journal_path.display(), report = %rep, "recovered a stale git lock from an orphan journal");
            }
            _ => info!(journal = %journal_path.display(), report = %rep, "orphan journal recovery"),
        }
    }
    for path in &report.gc_deleted {
        info!(journal = %path.display(), "swept a clean orphan journal past its retention window");
    }
    for path in &report.retained {
        // A dangling orphan the sweep could not settle (legacy record with no
        // encoded path, a lock still too fresh, or an I/O failure): retained as
        // live evidence and surfaced so an operator can clean it up by hand.
        warn!(journal = %path.display(), "retained a dangling orphan journal; recovery could not settle it — manual cleanup may be needed");
    }
    for path in &report.deferred {
        // A dangling orphan whose holder is still running (e.g. a watch removed
        // during an in-flight snapshot on this very daemon): untouched and expected
        // to settle on the holder's own drain, so it is informational, not a
        // manual-cleanup warning.
        info!(journal = %path.display(), "deferred a dangling orphan journal; its holder is still running and will settle it");
    }
    for trouble in &report.trouble {
        warn!(detail = %trouble, "journal reconciliation hiccup");
    }
}

/// Runs journal recovery for the given watches, cleaning a provably stale git
/// index lock left by a previous crash. Callers pass every *defined* watch
/// (paused included), not just the active ones: a paused watch that crashed while
/// active still owns a provably-ours stale lock, and recovery is read-only unless
/// the lock proves ours and stale — so covering a paused watch never touches a
/// live or foreign lock. Every non-`Clean` outcome is logged; nothing here can
/// fail the daemon (recovery folds all trouble into its report).
fn recover_stale_locks<'a>(paths: &DaemonPaths, specs: impl IntoIterator<Item = &'a WatchSpec>) {
    for spec in specs {
        let journal = Journal::for_repo_in_dir(&paths.journal_dir, spec.path());
        // A syncing watch may have crashed mid-sync, leaving a scratch worktree
        // and an un-applied advance target. Recover with a settler that cleans
        // those up — but ONLY through the journal's provably-ours-and-dead gates.
        // A non-sync watch (or a repo we cannot open) takes the plain path.
        match sync_settler(paths, spec) {
            Some(settler) => {
                let report = journal.recover_with_settler(spec.path(), &settler);
                journal::log_recovery(&report, spec.name(), "startup");
            }
            None => {
                journal.recover_and_log(spec.path(), spec.name(), "startup");
            }
        }
    }
}

/// Host-injects the out-of-tree reconcile scratch directory into every
/// sync-enabled spec, so vard-core can run the sync cycle (it resolves no paths
/// itself). The path is [`DaemonPaths::scratch_for`] — the *same* derivation
/// [`recover_stale_locks`] prunes via [`GitSyncSettler`], so the engine
/// reconciles in exactly the directory recovery cleans.
///
/// Scratch injection depends ONLY on `sync = true`; it does **not** probe the
/// remote (findings 4/5). The remote gate is LIVE inside the engine's sync cycle
/// ([`VcsBackend::has_remote`](vard_core::VcsBackend::has_remote) at cycle
/// start), so a remote added after the daemon started is picked up on the next
/// request with no restart, and a request on a remote-less watch is answered
/// honestly (an `Event::SyncSkipped` the daemon logs) rather than silently
/// dropped. A non-sync spec is returned unchanged.
fn inject_scratch_dirs(paths: &DaemonPaths, specs: Vec<WatchSpec>) -> Vec<WatchSpec> {
    specs
        .into_iter()
        .map(|spec| {
            if !spec.sync() {
                return spec;
            }
            let scratch = paths.scratch_for(spec.path());
            spec.with_scratch_dir(scratch)
        })
        .collect()
}

/// Builds a [`SyncSettler`](journal::SyncSettler) for a syncing watch, or `None`
/// when the watch does not sync or its repository cannot be opened (recovery
/// then falls back to the plain lock-only path).
fn sync_settler(paths: &DaemonPaths, spec: &WatchSpec) -> Option<GitSyncSettler> {
    if !spec.sync() {
        return None;
    }
    let backend = vard_core::open_git_backend(spec).ok()?;
    Some(GitSyncSettler {
        backend,
        scratch: paths.scratch_for(spec.path()),
    })
}

/// The daemon's [`SyncSettler`](journal::SyncSettler): idempotently settles a
/// crashed sync's tree through a real [`GitBackend`](vard_core::GitBackend).
struct GitSyncSettler {
    backend: vard_core::GitBackend,
    scratch: PathBuf,
}

impl journal::SyncSettler for GitSyncSettler {
    fn settle(&self) -> Result<(), String> {
        use vard_core::VcsBackend;
        // Recovery is never surgery on the user's files: prune the vard-owned
        // scratch worktree (always safe, never the user's tree; a no-op when
        // absent) and nothing else. A crashed advance leaves a fully-committed
        // tree at worst mid-checkout, and the next sync cycle self-heals — its
        // dirty check + pre-sync snapshot + fresh reconcile land the work
        // properly. No advance is ever re-applied here.
        self.backend
            .prune_scratch(&self.scratch)
            .map_err(|e| e.to_string())
    }
}

/// Builds a git-backed engine from resolved specs, subscribes, and starts it.
/// Returns the handle, a fresh event subscriber, the ordered watch identities
/// (name for request fan-out, path for the op gate), and this generation's
/// skipped-watch health problems.
///
/// Each watch is armed with its per-watch [`JournalOpGate`] (op lock + journal
/// under `journal_dir`), so the engine's worker brackets every commit under the
/// op lock — this is where the daemon injects the structural one-writer-per-watch
/// invariant (VRD-37); the journal is no longer an event-bus subscriber.
///
/// Per-watch open isolation (VRD-41): every surviving spec's repository is
/// opened HERE, one at a time, before any watch reaches [`Engine::builder`] — a
/// spec whose repository cannot be opened (missing, corrupt, mid-rebase) is
/// `error!`-logged and dropped from this engine generation, never taking the
/// rest down with it. Only the openable specs feed `watches`, so a skipped
/// watch is not part of the supervised identity set either (request fan-out
/// then honestly warns "unknown watch" for it, same as any other unknown name).
/// A skipped watch is not merely a log line, though: this function is the
/// single source of truth for the skip decision, so it also synthesizes a
/// [`health::HealthProblem`] for it right here (`state = "attention"`,
/// `kind = "unopenable"` — see [`health::unopenable_problem`]) and returns the
/// whole set for the caller to fold into every health-document write. Nothing
/// downstream re-probes the repository to learn this. The already-open
/// backends are injected via `watch_with_backend_and_gate`, so `build` cannot
/// re-open (and re-fail) what this function already vetted — the same pattern
/// `cmd/sync.rs` uses for the CLI sync path. A reload re-resolves every spec
/// from scratch, so a repository fixed on disk is picked back up on the next
/// poll. If every spec fails to open, the engine simply supervises zero
/// watches; [`Engine::start`] is fine with that (no workers armed,
/// `DaemonStarted` still emitted) — the same idle shape the daemon already runs
/// for an all-paused config.
async fn build_started_engine_from_specs(
    journal_dir: &Path,
    specs: Vec<WatchSpec>,
    hooks: Option<HooksConfig>,
    hooks_carry: Option<hooks::Carryover>,
) -> Result<
    (
        EngineHandle,
        EventReceiver,
        WatchSet,
        Vec<health::HealthProblem>,
        Option<HooksRunnerHandle>,
    ),
    EngineError,
> {
    let specs = dedup_aliased_specs(specs);
    // One instant for the whole build: every watch skipped in this generation
    // entered its `unopenable` problem "now", at the engine-build moment.
    let build_moment = health::now_secs();
    let mut opened = Vec::with_capacity(specs.len());
    let mut skipped = Vec::new();
    for spec in specs {
        match vard_core::open_git_backend(&spec) {
            Ok(backend) => opened.push((spec, backend)),
            Err(e) => {
                error!(
                    skipped = spec.name(),
                    repo = %spec.path().display(),
                    error = %e,
                    "watch's repository cannot be opened; skipping it for this engine \
                     (fix the repository and it is picked up on the next reload)"
                );
                skipped.push(health::unopenable_problem(
                    spec.name(),
                    &e.to_string(),
                    build_moment,
                ));
            }
        }
    }
    let watches = WatchSet::new(
        opened
            .iter()
            .map(|(spec, _)| WatchIdentity::new(spec.name().to_string(), spec.path().to_path_buf()))
            .collect(),
    );
    let mut builder = Engine::builder();
    // `opened` and `watches` are in the same (surviving) order, so zip pairs each
    // spec with its identity to build the injected gate.
    for ((spec, backend), identity) in opened.into_iter().zip(watches.iter()) {
        let gate: vard_core::SharedGate = Arc::new(identity.op_gate(journal_dir));
        builder = builder.watch_with_backend_and_gate(spec, Arc::new(backend), gate);
    }
    let engine = builder.build()?;
    let events = engine.subscribe();
    // The hooks runner takes its OWN subscription, distinct from the supervisor's
    // `events`, and does so *before* start so it observes `daemon.started`.
    let hook_events = hooks.as_ref().map(|_| engine.subscribe());
    let handle = engine.start().await?;
    let runner = match (hooks, hook_events) {
        (Some(config), Some(rx)) => Some(hooks::spawn(rx, config, hooks_carry)),
        _ => None,
    };
    Ok((handle, events, watches, skipped, runner))
}

/// Assembles the hooks runner's arming from a loaded [`Config`] and its resolved
/// watches, or `None` when no hooks are configured anywhere (global or on any
/// active watch) — in which case the daemon spawns no runner. Only active
/// (non-paused) watches contribute: a paused watch runs no engine worker and so
/// emits no events. The global `[hooks]` timeout and rate limit come from
/// `[defaults]` (there is no daemon-level override), matching how a watch with no
/// explicit values resolves them.
fn build_hooks_config(
    config: &Config,
    defined: &[crate::config::ResolvedWatch],
) -> Option<HooksConfig> {
    // `resolve_global_hooks` was already validated during `resolve_all`; an error
    // here would have failed startup/reload before this point, so treat a stray
    // failure as "no global hooks" rather than propagating.
    let global = config.resolve_global_hooks().unwrap_or_default();
    let global_timeout = config
        .defaults
        .hook_timeout
        .unwrap_or(crate::config::DEFAULT_HOOK_TIMEOUT);
    let global_rate_limit = config
        .defaults
        .hook_rate_limit
        .unwrap_or(crate::config::DEFAULT_HOOK_RATE_LIMIT);
    let watches = defined
        .iter()
        .filter(|w| !w.paused)
        .map(|w| hooks::WatchHooks {
            name: w.spec.name().to_string(),
            path: w.spec.path().to_path_buf(),
            hooks: w.hooks.clone(),
            timeout: w.hook_timeout,
            rate_limit: w.hook_rate_limit,
        });
    HooksConfig::build(global, global_timeout, global_rate_limit, watches)
}

/// Drops any active watch whose journal key collides with an earlier one's —
/// the canonical-path aliasing that [`Config::resolve_all`](crate::config::Config::resolve_all)'s
/// textual duplicate-path check cannot catch (two config paths that differ as
/// text but canonicalize to one repo: a symlink, a `..` segment, case folding on
/// a case-insensitive filesystem). Two such watches would share one journal
/// file, so a crash on one could clean or condemn the other's lock.
///
/// First in config order wins; a later colliding watch is **skipped** with a
/// loud warn naming both watches and the shared repository. It is deliberately
/// not a hard error: a hand-edited alias must not take every other watch down
/// with it. This is the check `config.rs` defers to "the daemon at registration
/// time"; it runs on both the initial build and every reload, since both funnel
/// through here.
fn dedup_aliased_specs(specs: Vec<WatchSpec>) -> Vec<WatchSpec> {
    // Detect aliases with the shared first-wins rule so the daemon's skip and the
    // `status` / `watch list` markers cannot drift on which watch is supervised.
    let aliases = journal::alias_winners(specs.iter().map(|s| (s.name(), s.path())));
    let mut kept = Vec::with_capacity(specs.len());
    for (spec, alias) in specs.into_iter().zip(aliases) {
        if let Some(winner) = alias {
            warn!(
                skipped = spec.name(),
                kept = %winner,
                repo = %spec.path().display(),
                "two watches resolve to the same canonical repository; skipping the later one \
                 (they would share one operation journal)"
            );
            continue;
        }
        kept.push(spec);
    }
    kept
}

/// Loads the current config from disk and starts a fresh engine from it, for a
/// reload or a source-died rebuild. Returns `None` (leaving the caller's current
/// engine untouched) on any error, so a bad edit never takes a healthy daemon
/// down.
///
/// `known` are the identities already supervised before this rebuild. Recovery
/// runs for any watch whose journal key is *not* among them (initial startup
/// recovered the rest), so a watch introduced by a reload — a fresh add, a
/// *rename* (path unchanged, so its journal is the same file), or a *relink*
/// (same name, new path, so a new journal key) — gets the same stale-lock
/// cleaning a startup watch does, before its engine arms. Keying membership by
/// journal path rather than by name is what makes the rename/relink cases work:
/// a rename keeps its journal (no orphan), a relink recovers the new path.
async fn build_started_engine(
    paths: &DaemonPaths,
    known: &[WatchIdentity],
    hooks_carry: Option<hooks::Carryover>,
) -> Option<(
    EngineHandle,
    EventReceiver,
    WatchSet,
    Vec<health::HealthProblem>,
    Option<HooksRunnerHandle>,
)> {
    let config = match Config::load(&paths.config_file) {
        Ok(config) => config,
        Err(err) => {
            error!(error = %err, "reload: could not load config; keeping current engine");
            return None;
        }
    };
    // Resolve every defined watch (this also validates). On any error — the
    // never-reload guard's real purpose — keep the current engine so a bad edit
    // cannot take a healthy daemon down.
    let defined = match config.resolve_all() {
        Ok(defined) => defined,
        Err(err) => {
            error!(error = %err, "reload: could not resolve config; keeping current engine");
            return None;
        }
    };
    // A successfully-loaded config that defines *zero* watches is treated as a
    // suspicious edit, not a request to idle: keep the current engine. But a
    // config that defines watches which are all *paused* is a legitimate idle
    // state — build a zero-active engine so a later resume reloads it back.
    if defined.is_empty() {
        error!("reload: config defines no watches; keeping current engine");
        return None;
    }
    // Reconcile journals against the new config first (migrate legacy names,
    // sweep old orphans — a watch just removed becomes a fresh orphan for a
    // future sweep), so recovery below looks under the right path keys.
    reconcile_journals(paths, &defined);

    let specs: Vec<WatchSpec> = inject_scratch_dirs(
        paths,
        defined
            .iter()
            .filter(|w| !w.paused)
            .map(|w| w.spec.clone())
            .collect(),
    );
    if specs.is_empty() {
        info!("reload: all watches are paused; running idle");
    }

    // Recover stale locks for every *defined* watch (paused included, same as
    // startup) that this reload newly introduces — one whose journal key was not
    // already supervised. A watch that crashed-then-paused across a reload still
    // owns a provably-ours stale lock; recovery is read-only unless the lock
    // proves ours and stale, so covering paused watches never wedges anything.
    let known_keys = WatchIdentity::key_set(known);
    recover_stale_locks(
        paths,
        defined
            .iter()
            .map(|w| &w.spec)
            .filter(|spec| !known_keys.contains(&journal::journal_file_name(spec.path()))),
    );

    let hooks_config = build_hooks_config(&config, &defined);
    match build_started_engine_from_specs(&paths.journal_dir, specs, hooks_config, hooks_carry)
        .await
    {
        Ok(started) => Some(started),
        Err(err) => {
            error!(error = %err, "reload: could not start new engine; keeping current engine");
            None
        }
    }
}

/// The result of a drain-and-rebuild attempt: the (possibly unchanged) engine
/// plus what happened during the swap.
struct RebuildOutcome {
    handle: EngineHandle,
    events: EventReceiver,
    watches: WatchSet,
    /// The current engine generation's skipped-watch health problems (VRD-41):
    /// the fresh set on a successful rebuild, or the old generation's set
    /// unchanged when the rebuild kept the old engine.
    skipped: Vec<health::HealthProblem>,
    /// The hooks runner armed against the current engine's bus (VRD-21): a fresh
    /// runner on a successful rebuild, or the old one retained when the rebuild
    /// kept the old engine. `None` when no hooks are configured.
    hooks: Option<HooksRunnerHandle>,
    /// Whether a fresh engine actually replaced the old one.
    rebuilt: bool,
    /// Whether the new engine reported failure (a dead signal source or a
    /// closed bus) while the old one drained. The caller schedules a
    /// backed-off rebuild, exactly as it would for the same event observed
    /// live — the pump consumed it, so it will not reappear on the receiver.
    failure_seen: bool,
}

/// Builds a fresh engine and, on success, drains the old one before swapping to
/// it; on any failure it keeps the old engine. The new engine is started before
/// the old is drained, so there is a brief window where both are armed — and
/// under VRD-37 that window is now safe **structurally**: a surviving/renamed
/// watch keys the same op lock across the swap, so while the old engine's worker
/// still holds it mid-op the new engine's worker's `gate.begin` returns busy and
/// requeues rather than racing. No cross-engine double-journal or double-commit
/// is possible, and the old `superseded` key-set that used to paper over the
/// reload window is gone. Either way a valid `(handle, events)` exists at all
/// times, honoring "keep the old engine on any reload error".
///
/// While the old engine drains (up to its shutdown budget), the *new* engine's
/// bus is pumped concurrently through the same log/health path the supervisor
/// uses: without this, a busy new engine could overflow its subscriber buffer
/// during a slow drain and drop health-relevant events.
async fn try_rebuild(
    paths: &DaemonPaths,
    old_handle: EngineHandle,
    mut old_events: EventReceiver,
    old_watches: WatchSet,
    old_skipped: Vec<health::HealthProblem>,
    old_hooks: Option<HooksRunnerHandle>,
) -> RebuildOutcome {
    // Capture the old runner's coalescing + failure + suppression state so the new
    // runner resumes it: a pending trailing event, a failure streak, and the
    // debounced `daemon.started` all survive the swap (VRD-21). Cloned, not drained
    // — on a failed rebuild `old_hooks` is returned unchanged as the live runner,
    // so it must keep its own state intact.
    let hooks_carry = old_hooks.as_ref().map(HooksRunnerHandle::carryover);
    match build_started_engine(paths, &old_watches, hooks_carry).await {
        Some((handle, mut events, watches, skipped, hooks)) => {
            // The new runner is already armed against the new engine's bus (resuming
            // the carried state); abort the old one now so it does not react to the
            // old engine's drain. In particular the old engine's `daemon.stopped` is
            // deliberately suppressed on a reload: only a true shutdown fires
            // `daemon.stopped`, while `daemon.started` is re-emitted by every new
            // generation and rate-limited by the carried loop guard. That asymmetry
            // (started re-fires, bounded; stopped does not) is an accepted trade.
            drop(old_hooks);
            let mut failure_seen = false;
            {
                let mut shutdown = std::pin::pin!(old_handle.shutdown());
                // Pump the new engine's receiver until the old engine's drain
                // completes. If the new bus closes (it should not — the engine
                // just started), stop pumping and just await the drain, rather
                // than busy-looping on `Closed`.
                let mut pumping = true;
                loop {
                    if !pumping {
                        (&mut shutdown).await;
                        break;
                    }
                    tokio::select! {
                        _ = &mut shutdown => break,
                        received = events.recv() => {
                            let closed = matches!(received, Err(RecvError::Closed));
                            if matches!(handle_bus(received), Action::EngineFailure) {
                                failure_seen = true;
                            }
                            if closed {
                                pumping = false;
                            }
                        }
                    }
                }
            }
            // The old engine has fully shut down. Drain its buffered event tail so
            // the log is complete; nothing here journals any more (the op-lock
            // guard closed each bracket when the worker's pass finished, or left it
            // dangling on a crash for recovery). The old engine's own passes have
            // by here released their op locks, so a surviving watch's new worker
            // can proceed.
            drain_remaining_events(&mut old_events);
            // Drain-on-remove: a watch present before this reload but gone now
            // (removed or relinked to a new path) has, by here, settled its
            // in-flight bracket via the shutdown join above. Run recovery
            // against its repo so any stale git lock we left is proven ours and
            // cleaned — never leaving a removed watch wedged on a lock only its
            // journal could vouch for.
            drain_removed_watches(paths, &old_watches, &watches);
            // Regenerate health from the NEW engine's truth. A reload that
            // removed or renamed a watch leaves no recovery event behind, but
            // regeneration from `watch_states()` simply omits the vanished watch
            // — so a stale problem cannot linger, and a still-blocked watch the
            // new engine re-probed at startup is reflected at once (no rebuild
            // amnesia).
            write_health(&handle, &skipped, hooks.as_ref(), paths);
            RebuildOutcome {
                handle,
                events,
                watches,
                skipped,
                hooks,
                rebuilt: true,
                failure_seen,
            }
        }
        None => RebuildOutcome {
            handle: old_handle,
            events: old_events,
            watches: old_watches,
            skipped: old_skipped,
            hooks: old_hooks,
            rebuilt: false,
            failure_seen: false,
        },
    }
}

/// Runs stale-lock recovery for every watch that a reload dropped — present in
/// `before` but not `after` (compared by journal path key). The old engine's
/// shutdown join has already closed each dropped watch's in-flight bracket, so
/// recovery sees either a clean journal (nothing to do) or a dangling record
/// whose lock it proves ours and cleans. This is the daemon half of
/// drain-on-remove; the no-daemon half runs synchronously in `vard watch
/// remove`. Every outcome is logged through the shared
/// [`recover_and_log`](Journal::recover_and_log) so its levels match every other
/// drain/recover site; nothing here can fail the daemon.
///
/// A watch that was merely *paused* (still defined, so still supervised as a
/// resolved watch but filtered out of the active engine specs) is included in
/// `after` only if it stays active, so pausing a watch does make it look
/// "removed" here for one drain. That is deliberate and safe: the paused watch
/// took no in-flight operation, so recovery finds its journal clean and does
/// nothing, while its journal file is retained (a paused watch still owns it and
/// the orphan sweep never touches a configured key).
///
/// A watch that turned temporarily *unopenable* (VRD-41 per-watch isolation —
/// still configured and active, but this generation's `build_started_engine`
/// skipped it) rides this same path: it was in `before`'s supervised set but is
/// absent from `after`, so it reads as "removed" even though it is not. That is
/// safe for the same reason as a pause: the settler-less recovery here is
/// idempotent and touches only a provably-ours, provably-stale lock, so the
/// watch re-arms cleanly (no lock left wedged) the moment its repository is
/// repaired and a later reload reopens it.
fn drain_removed_watches(paths: &DaemonPaths, before: &[WatchIdentity], after: &[WatchIdentity]) {
    let surviving = WatchIdentity::key_set(after);
    for removed in before
        .iter()
        .filter(|w| !surviving.contains(&w.journal_key))
    {
        let journal = removed.journal(&paths.journal_dir);
        journal.recover_and_log(&removed.path, &removed.name, "drain-on-remove");
    }
}

/// Logs every event still buffered on a receiver whose engine has fully shut
/// down. `EngineHandle::shutdown` joins every task before emitting the final
/// `DaemonStopped`, so once it returns the buffer holds the complete tail of the
/// stream; draining it here keeps the log complete (and lets a lagging
/// health-relevant event be observed) rather than dropping the tail.
///
/// It **no longer journals** (VRD-37): each commit's journal bracket is opened
/// and closed by the engine worker's op-lock guard, not by the supervisor's view
/// of the bus, so a bracket is settled the moment the worker's pass finishes —
/// there is nothing left for a drain to close.
fn drain_remaining_events(events: &mut EventReceiver) {
    use vard_core::TryRecvError;
    loop {
        match events.try_recv() {
            Ok(event) => log_event(&event),
            Err(TryRecvError::Lagged(skipped)) => {
                warn!(skipped, "event bus lagged during shutdown drain");
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
        }
    }
}

/// What one turn of the supervisor loop decided to do next. Kept separate from
/// the `select!` so the engine swap happens outside the branch that borrows the
/// event receiver.
enum Action {
    /// Nothing to do; keep looping.
    Continue,
    /// Rebuild from the current config (a config edit or SIGHUP).
    Reload,
    /// The scheduled source-died backoff elapsed; rebuild now.
    BackoffRebuild,
    /// A watch's signal source died (or the bus closed); schedule a backed-off
    /// rebuild.
    EngineFailure,
    /// Shut down gracefully.
    Shutdown,
}

/// The supervisor loop: selects over the event bus, the shutdown/reload signals,
/// the poll timer, and the source-died backoff timer, applying each turn's
/// [`Action`] outside the select so the engine can be swapped without borrow
/// conflicts.
#[allow(clippy::too_many_arguments)]
async fn supervise(
    paths: DaemonPaths,
    mut handle: EngineHandle,
    mut events: EventReceiver,
    mut watches: WatchSet,
    mut skipped: Vec<health::HealthProblem>,
    mut hooks: Option<HooksRunnerHandle>,
    shutdown: Arc<Notify>,
    reload: Arc<Notify>,
    poll_interval: Duration,
) {
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut config_debounce = ConfigDebounce::new(config_fingerprint(&paths.config_file));
    let mut backoff = SourceDiedBackoff::new();
    // When set, a source-died rebuild is due at this deadline.
    let mut rebuild_at: Option<TokioInstant> = None;

    // The health heartbeat: rewrite the document every ~60s even when nothing
    // changed, so `written_at` stays fresh (notify uses its age to detect a
    // wedged daemon) and any health-relevant event a lagging subscriber dropped
    // is reconciled from the engine's own truth on the next tick.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(health::HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Write a fresh health document on startup, regenerated from the engine's
    // truth (the initial state probe has already flagged any blocked repo), so
    // it supersedes the crash-leftover file that was cleared before build.
    write_health(&handle, &skipped, hooks.as_ref(), &paths);

    loop {
        // Coalesce a burst of health-relevant events into a single write per loop
        // turn (marked here, written once below), rather than a write per event.
        let mut health_dirty = false;
        let action = tokio::select! {
            _ = shutdown.notified() => Action::Shutdown,
            _ = reload.notified() => {
                info!("reload requested (SIGHUP)");
                Action::Reload
            }
            _ = heartbeat.tick() => {
                // Refresh written_at (and self-heal any missed transition, plus
                // reconcile the hooks runner's suppression/failure projection).
                write_health(&handle, &skipped, hooks.as_ref(), &paths);
                Action::Continue
            }
            _ = poll.tick() => {
                // Check the config BEFORE draining requests, and hold requests
                // while a config change is in flight, so a request is always
                // answered against the *current* config rather than a pre-reload
                // engine. This closes a whole race class — any request that
                // follows a config write within a tick (most visibly `vard watch
                // sync`, which flips `sync` off→on and then queues a confirming
                // sync). VRD-35's content fingerprint guarantees the config
                // change that preceded a request is observed no later than the
                // request itself, and a single atomic CLI write settles one poll
                // later (the debounce needs two stable samples), so deferring the
                // drain to the reload below (see the `Reload` arm) loses nothing.
                if config_debounce.poll(config_fingerprint(&paths.config_file)) {
                    info!("config file changed; reloading");
                    Action::Reload
                } else if config_debounce.change_pending() {
                    // A change was seen once and is settling; its reload lands on
                    // the next poll and drains any deferred request against the
                    // rebuilt engine. Hold requests until then.
                    Action::Continue
                } else {
                    process_requests(&paths, &handle, &watches);
                    Action::Continue
                }
            }
            _ = tokio::time::sleep_until(rebuild_at.unwrap_or_else(TokioInstant::now)),
                if rebuild_at.is_some() =>
            {
                Action::BackoffRebuild
            }
            received = events.recv() => {
                health_dirty = received.as_ref().map(health_relevant).unwrap_or(false);
                handle_bus(received)
            }
        };

        // A watch transition changed the projected health picture: regenerate.
        // (Rebuild/reload paths write their own health from the new engine.)
        if health_dirty {
            write_health(&handle, &skipped, hooks.as_ref(), &paths);
        }

        match action {
            Action::Continue => {}
            Action::Shutdown => break,
            Action::Reload => {
                let outcome = try_rebuild(&paths, handle, events, watches, skipped, hooks).await;
                handle = outcome.handle;
                events = outcome.events;
                watches = outcome.watches;
                skipped = outcome.skipped;
                hooks = outcome.hooks;
                if outcome.rebuilt {
                    // A full rebuild supersedes any pending source-died retry.
                    rebuild_at = None;
                }
                if outcome.failure_seen {
                    schedule_rebuild(&mut backoff, &mut rebuild_at, "engine failed during swap");
                }
                // Drain any request the poll tick deferred while this config
                // change settled — so a control-plane request that accompanied a
                // config edit (e.g. `vard watch sync` enabling sync) is answered
                // against the current config, not the pre-reload engine. On a
                // successful rebuild that is the new engine and watch set; when
                // the reload FAILED (an invalid config), `try_rebuild` retained
                // the last-good engine and watch set, so the request is still
                // consumed here and answered by that last-good config. A request
                // naming a watch this reload just removed fails honestly (logged
                // and skipped), which is correct.
                //
                // BUT `try_rebuild` is awaited, and an external write can land a
                // NEWER config (plus its own request) during that window. The
                // engine we just built is then already stale, so draining here
                // would answer the newer request against it — the same
                // silent-drop class, through the rebuild-duration window. Re-read
                // the fingerprint and drain only when the on-disk config still
                // matches what this reload settled on; if it moved again, leave
                // the request queued for the next poll's pending-hold + reload
                // (held, not lost — stale-expiry still backstops).
                if config_fingerprint(&paths.config_file) == config_debounce.settled() {
                    process_requests(&paths, &handle, &watches);
                }
            }
            Action::BackoffRebuild => {
                rebuild_at = None;
                let outcome = try_rebuild(&paths, handle, events, watches, skipped, hooks).await;
                handle = outcome.handle;
                events = outcome.events;
                watches = outcome.watches;
                skipped = outcome.skipped;
                hooks = outcome.hooks;
                if !outcome.rebuilt {
                    // The rebuild failed; keep retrying on a growing backoff so a
                    // persistently broken engine is not hammered.
                    schedule_rebuild(
                        &mut backoff,
                        &mut rebuild_at,
                        "source-died rebuild failed; retrying",
                    );
                } else if outcome.failure_seen {
                    schedule_rebuild(&mut backoff, &mut rebuild_at, "engine failed during swap");
                }
            }
            Action::EngineFailure => {
                schedule_rebuild(&mut backoff, &mut rebuild_at, "watch signal source died");
            }
        }
    }

    info!("shutting down");
    handle.shutdown().await;
    // The shutdown join finishes any in-flight pass — closing (or, on a crash,
    // leaving dangling for recovery) each op-lock journal bracket — so this drain
    // only needs to log the trailing events (including the final DaemonStopped).
    drain_remaining_events(&mut events);
    // Clean shutdown: remove the health file BEFORE the caller releases the
    // instance lock, so `vard notify` sees either a running daemon with a live
    // document or no daemon at all — never a stale problem set presented as
    // current. The window between this clear and the lock release reads as the
    // honest "starting or stopping" line.
    health::clear(&paths.health_file);
}

/// Arms (or keeps) the source-died rebuild timer: advances the backoff and sets
/// the deadline unless one is already pending, logging why. Shared by every
/// path that discovers engine failure — a live bus event, a failure observed
/// while pumping during a swap, or a failed backoff rebuild.
fn schedule_rebuild(
    backoff: &mut SourceDiedBackoff,
    rebuild_at: &mut Option<TokioInstant>,
    why: &str,
) {
    if rebuild_at.is_none() {
        let delay = backoff.on_failure(Instant::now());
        warn!(?delay, reason = why, "scheduling engine rebuild");
        *rebuild_at = Some(TokioInstant::now() + delay);
    }
}

/// Handles one item from the event bus: logs it and reports whether it signals
/// engine failure (a dead signal source or a closed bus) that warrants a rebuild.
///
/// The journal is **no longer** driven from here (VRD-37): each commit is
/// bracketed structurally by the engine worker's op-lock guard, so the supervisor
/// only logs the bus and projects health — it never writes the journal, and the
/// old reload-window `superseded` key-set is gone with it.
///
/// It does *not* write the health file: the caller regenerates health from
/// [`EngineHandle::watch_states`] once per loop turn when a health-relevant
/// event arrived (see [`health_relevant`]), so a burst coalesces into one write.
fn handle_bus(received: Result<Event, RecvError>) -> Action {
    match received {
        Ok(event) => {
            log_event(&event);
            if is_source_died(&event) {
                Action::EngineFailure
            } else {
                Action::Continue
            }
        }
        Err(RecvError::Lagged(skipped)) => {
            warn!(skipped, "event bus lagged; some events were dropped");
            Action::Continue
        }
        Err(RecvError::Closed) => {
            warn!("event bus closed unexpectedly; scheduling engine rebuild");
            Action::EngineFailure
        }
    }
}

/// Whether an event can change the projected health picture — only a watch
/// lifecycle transition can. Snapshot/sync/lifecycle chatter does not, so it
/// never triggers a health rewrite; the periodic heartbeat covers anything a
/// dropped transition might have missed.
fn health_relevant(event: &Event) -> bool {
    matches!(event, Event::WatchStateChanged { .. })
}

/// Whether an event reports a watch's signal source dying — the one condition a
/// supervisor rebuilds for (the watch is otherwise permanently silent).
fn is_source_died(event: &Event) -> bool {
    matches!(
        event,
        Event::WatchStateChanged {
            trouble: Some(TroubleKind::SourceDied),
            ..
        }
    )
}

/// Logs an event via tracing: `info` for snapshots and lifecycle, `warn` for
/// failures and attention. Uses [`Event::name`] as a stable field so log
/// consumers can key off the catalog string.
fn log_event(event: &Event) {
    let name = event.name();
    match event {
        Event::SnapshotStarted { watch, trigger } => {
            // The pre-commit signal is high-rate (per sweep); keep it at debug.
            debug!(event = name, %watch, %trigger, "snapshot starting");
        }
        Event::SnapshotCompleted {
            watch,
            snapshot,
            files_changed,
            trigger,
        } => {
            info!(event = name, %watch, %snapshot, files_changed, %trigger, "snapshot completed");
        }
        Event::SnapshotFailed {
            watch,
            trigger,
            error,
        } => {
            warn!(event = name, %watch, %trigger, %error, "snapshot failed");
        }
        Event::SnapshotSkipped {
            watch,
            trigger,
            reason,
        } => {
            // The no-commit closer of the started bracket; high-rate for clean
            // sweeps (one follows every commit's re-check), so debug like the
            // opener.
            debug!(event = name, %watch, %trigger, %reason, "snapshot skipped");
        }
        Event::WatchStateChanged {
            watch,
            from,
            to,
            reason,
            trouble,
        } => {
            let reason = reason.as_deref().unwrap_or("");
            if trouble.is_some() || matches!(to, WatchState::Attention) {
                warn!(event = name, %watch, %from, %to, ?trouble, reason, "watch needs attention");
            } else {
                info!(event = name, %watch, %from, %to, reason, "watch state changed");
            }
        }
        Event::SyncPushed {
            watch,
            new_ref,
            commits,
        } => info!(event = name, %watch, %new_ref, commits, "pushed to remote"),
        Event::SyncPulled {
            watch,
            prev_ref,
            new_ref,
        } => info!(event = name, %watch, %prev_ref, %new_ref, "pulled from remote"),
        Event::SyncConflict { watch } => warn!(event = name, %watch, "sync conflict"),
        Event::SyncResolved { watch, resolver } => {
            info!(event = name, %watch, %resolver, "sync conflict resolved");
        }
        Event::SyncFailed { watch, error } => warn!(event = name, %watch, %error, "sync failed"),
        Event::SyncSkipped { watch, reason } => {
            info!(event = name, %watch, %reason, "sync skipped")
        }
        Event::RestoreCompleted {
            watch,
            restored_to,
            prev_ref,
        } => info!(event = name, %watch, %restored_to, %prev_ref, "restore completed"),
        Event::DaemonStarted => info!(event = name, "daemon started"),
        Event::DaemonStopped => info!(event = name, "daemon stopped"),
        Event::UpdateAvailable { version } => info!(event = name, %version, "update available"),
        // `Event` is non_exhaustive; a new variant logs at info until handled.
        _ => info!(event = name, "event"),
    }
}

/// Regenerates the health file from the engine's current per-watch truth
/// ([`EngineHandle::watch_states`]) plus the current engine generation's
/// `skipped` watches (VRD-41 — those whose repository could not be opened and
/// so never reached the engine at all) and writes it atomically. The document
/// is a pure projection — never patched incrementally — so every write
/// reflects exactly what is true right now: a recovered watch drops out, a
/// renamed watch's stale entry cannot linger, a still-blocked watch a restart
/// re-probed is present from the first write, and a skipped watch is never
/// silently absent (which would read as `ok`).
///
/// The current engine generation's hooks runner (VRD-21), when hooks are armed,
/// is projected into the same document: its persistently-failing hooks become
/// `hook-failing` problems and its per-watch suppression counters become
/// telemetry. The projection is read from the runner's in-memory snapshot on
/// every write, so a recovered hook or a reset counter simply drops out — no
/// accumulator lives in the file.
///
/// A write failure is warned, never fatal: a health-file hiccup must not crash
/// the daemon or interrupt snapshotting, and `vard notify` degrades to the
/// daemon-not-running / starting / stale path on its own.
fn write_health(
    handle: &EngineHandle,
    skipped: &[health::HealthProblem],
    hooks: Option<&HooksRunnerHandle>,
    paths: &DaemonPaths,
) {
    let now = health::now_secs();
    let (hook_problems, suppressions) = hooks
        .map(|h| project_hook_health(&h.snapshot(), now))
        .unwrap_or_default();
    let doc = health::doc_with_hooks(
        &handle.watch_states(),
        skipped,
        hook_problems,
        suppressions,
        now,
    );
    if let Err(err) = health::write(&paths.health_file, &doc) {
        warn!(error = %err, "could not write health file");
    }
}

/// Projects a hooks-runner [`RunnerSnapshot`] into the health vocabulary: each
/// persistently-failing hook (already at or beyond the failure threshold, and
/// pre-sorted by the snapshot) becomes a `hook-failing` [`health::HealthProblem`]
/// stamped at `now`, and each nonzero per-watch suppression counter becomes a
/// [`health::HookSuppression`]. Suppression counters are sorted by watch name so
/// the projection is deterministic (the runner keeps them in a `HashMap`); a
/// zero counter is dropped rather than written.
fn project_hook_health(
    snapshot: &RunnerSnapshot,
    now: u64,
) -> (Vec<health::HealthProblem>, Vec<health::HookSuppression>) {
    let problems = snapshot
        .failing
        .iter()
        .map(|f| {
            health::hook_failing_problem(
                f.watch.as_deref(),
                &f.event,
                &f.command,
                f.consecutive,
                &f.last_error,
                now,
            )
        })
        .collect();
    let mut suppressions: Vec<health::HookSuppression> = snapshot
        .suppressed_by_watch
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(watch, count)| health::HookSuppression {
            watch: watch.clone(),
            count: *count,
        })
        .collect();
    suppressions.sort_by(|a, b| a.watch.cmp(&b.watch));
    (problems, suppressions)
}

/// Drains the request directory into the engine (see [`drain_request_dir`] for
/// the consumption rules), discarding any request that has aged past the
/// staleness window before it reaches the engine.
fn process_requests(paths: &DaemonPaths, handle: &EngineHandle, watches: &[WatchIdentity]) {
    let now = SystemTime::now();
    drain_request_dir(&paths.request_dir, |request| {
        if request_is_stale(&request, now) {
            warn!(
                ?request,
                age_secs = request.age(now).as_secs(),
                "discarding a request older than the staleness window"
            );
            return;
        }
        apply_request(request, handle, watches);
    });
}

/// Whether a drained request is too old to act on: a machine that slept for
/// hours must not wake to a burst of stale manual snapshots (see
/// [`request::STALE_AFTER`]).
fn request_is_stale(request: &Request, now: SystemTime) -> bool {
    request.age(now) > request::STALE_AFTER
}

/// Whether `name` is a *settled* request file the daemon may consume: a plain
/// `*.toml` name that is not hidden (no leading dot). Writers create requests
/// by writing to a temp or dot name in the same directory and `rename(2)`-ing
/// to the final `*.toml` name (atomic on POSIX; see the [module docs](self)),
/// so a name matching this predicate is always a complete file. Temp suffixes
/// (`.tmp`, `.partial`, anything else) fail the `*.toml` requirement.
fn is_settled_request_name(name: &str) -> bool {
    name.ends_with(".toml") && !name.starts_with('.')
}

/// Scans `dir` and consumes every *settled* request file: each is parsed,
/// handed to `on_request` when valid, and then deleted — a consumed request or
/// a settled poison file alike (poison is logged and removed so it cannot
/// wedge the queue). Unsettled names — dotfiles, temp suffixes, anything not
/// `*.toml` — are a writer mid-flight per the contract and are ignored *and
/// left in place*, never read or deleted. A missing directory is not an error
/// (the CLI creates it lazily). Factored from [`process_requests`] so tests can
/// drive it with a collecting closure instead of a live engine.
fn drain_request_dir(dir: &Path, mut on_request: impl FnMut(Request)) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            warn!(dir = %dir.display(), error = %err, "could not read request dir");
            return;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_settled_request_name(name) {
            // A writer mid-flight (temp/dot name) or an unrelated file: not
            // ours to touch.
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match request::parse(&text) {
                Ok(request) => on_request(request),
                Err(err) => {
                    warn!(file = %path.display(), error = %err, "malformed request file; dropping");
                }
            },
            Err(err) => {
                warn!(file = %path.display(), error = %err, "could not read request file; dropping");
            }
        }
        // Remove the settled file — a consumed request or a poison one alike.
        if let Err(err) = std::fs::remove_file(&path) {
            warn!(file = %path.display(), error = %err, "could not delete request file");
        }
    }
}

/// Applies one parsed request: a snapshot injects a manual trigger and a sync a
/// manual sync request — for the named watch, or every watch when unnamed. A
/// watch that cannot sync accepts the request and does nothing (see
/// [`EngineHandle::request_sync`]).
fn apply_request(request: Request, handle: &EngineHandle, watches: &[WatchIdentity]) {
    match request.kind {
        RequestKind::Snapshot => match request.watch {
            Some(watch) => {
                if handle.trigger(&watch) {
                    info!(%watch, "manual snapshot requested");
                } else {
                    warn!(%watch, "snapshot requested for an unknown watch; ignoring");
                }
            }
            None => {
                for watch in watches {
                    handle.trigger(&watch.name);
                }
                info!(
                    count = watches.len(),
                    "manual snapshot requested for all watches"
                );
            }
        },
        RequestKind::Sync => match request.watch {
            Some(watch) => {
                if handle.request_sync(&watch) {
                    info!(%watch, "manual sync requested");
                } else {
                    warn!(%watch, "sync requested for an unknown watch; ignoring");
                }
            }
            None => {
                for watch in watches {
                    handle.request_sync(&watch.name);
                }
                info!(
                    count = watches.len(),
                    "manual sync requested for all watches"
                );
            }
        },
    }
}

/// The config file's change-detection key: its byte length plus the FNV-1a
/// hash of its content. The length is a free strict reduction of the (already
/// tiny) hash-collision risk, since most real edits change the file's size.
type ConfigFingerprint = (u64, u64);

/// Reads a file's content and returns its [`ConfigFingerprint`], or `None` if
/// the file is missing or unreadable — the same posture the old mtime check
/// had (an empty file is `Some`, distinct from a missing one). The config file
/// is tiny, so a read-and-hash on every poll tick is cheap enough to be the
/// whole "did it change?" check; mtime alone can't be, since two back-to-back
/// CLI writes can land in the same 1-second mtime tick and be indistinguishable
/// by timestamp (VRD-35).
fn config_fingerprint(path: &Path) -> Option<ConfigFingerprint> {
    std::fs::read(path)
        .ok()
        .map(|bytes| (bytes.len() as u64, journal::fnv1a(&bytes)))
}

/// A one-cycle debounce over the config file's content fingerprint: it reports
/// a change only once the fingerprint has held steady at a new value across two
/// consecutive polls, so a burst of writes (an editor saving repeatedly)
/// collapses into a single settled signal rather than a rebuild per write.
struct ConfigDebounce {
    /// The last fingerprint we reported as settled.
    stable: Option<ConfigFingerprint>,
    /// A new fingerprint seen once, awaiting a second confirming poll.
    pending: Option<ConfigFingerprint>,
}

impl ConfigDebounce {
    /// Starts from an initial (settled) fingerprint.
    fn new(initial: Option<ConfigFingerprint>) -> ConfigDebounce {
        ConfigDebounce {
            stable: initial,
            pending: None,
        }
    }

    /// Feeds the current fingerprint; returns `true` exactly when a settled
    /// change is detected (a new value seen on two consecutive polls).
    fn poll(&mut self, current: Option<ConfigFingerprint>) -> bool {
        if current == self.stable {
            // Back to (or still at) the settled value: cancel any pending change.
            self.pending = None;
            false
        } else if self.pending == current {
            // The same new value on a second poll: settle it.
            self.stable = current;
            self.pending = None;
            true
        } else {
            // First sighting of a new value: wait one more poll to confirm.
            self.pending = current;
            false
        }
    }

    /// Whether a new fingerprint has been seen once and is awaiting its
    /// confirming poll. The supervisor holds request draining while this is
    /// true, so a request that followed a config write is answered only after
    /// the change settles and the reload rebuilds the engine.
    fn change_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The fingerprint currently settled (the last confirmed change, or the
    /// initial value). After an *awaited* rebuild the supervisor compares this
    /// to the on-disk fingerprint before draining a deferred request: if the
    /// config changed again while `try_rebuild` was in flight, the just-built
    /// engine is already stale, so draining is skipped and the request is left
    /// queued for the next poll's pending-hold and reload.
    fn settled(&self) -> Option<ConfigFingerprint> {
        self.stable
    }
}

/// Exponential backoff for source-died rebuilds: doubles from a base toward a
/// cap on repeated quick failures, resetting to the base after a healthy period.
struct SourceDiedBackoff {
    base: Duration,
    cap: Duration,
    reset_after: Duration,
    /// The delay the next failure will use.
    next: Duration,
    /// When the previous failure occurred, to detect a healthy gap.
    last_failure: Option<Instant>,
}

impl SourceDiedBackoff {
    /// A backoff with the module's default base/cap/reset constants.
    fn new() -> SourceDiedBackoff {
        SourceDiedBackoff {
            base: SOURCE_DIED_BACKOFF_BASE,
            cap: SOURCE_DIED_BACKOFF_CAP,
            reset_after: SOURCE_DIED_BACKOFF_RESET_AFTER,
            next: SOURCE_DIED_BACKOFF_BASE,
            last_failure: None,
        }
    }

    /// Advances the backoff for a failure at `now`, returning the delay to wait
    /// before rebuilding. Resets to the base when the previous failure was more
    /// than `reset_after` ago (the engine had recovered), otherwise doubles
    /// toward the cap.
    fn on_failure(&mut self, now: Instant) -> Duration {
        let within_window = self
            .last_failure
            .is_some_and(|prev| now.duration_since(prev) < self.reset_after);
        self.next = if within_window {
            (self.next * 2).min(self.cap)
        } else {
            self.base
        };
        self.last_failure = Some(now);
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- GitSyncSettler (crash-recovery settlement) ---------------------------

    /// Opens a `GitSyncSettler` over `repo` on `main`, scratch under a subdir.
    fn settler_for(repo: &Path) -> GitSyncSettler {
        GitSyncSettler {
            backend: vard_core::GitBackend::open(repo, "main", "origin").unwrap(),
            scratch: repo.join(".vard-scratch"),
        }
    }

    #[test]
    fn git_sync_settler_prunes_the_scratch_and_never_touches_user_files() {
        use journal::SyncSettler;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-m", "one"]);

        // A leftover scratch worktree from a crashed reconcile, at the very path
        // the settler prunes.
        let scratch = repo.join(".vard-scratch");
        git_ok(
            &repo,
            &["worktree", "add", "--detach", scratch.to_str().unwrap()],
        );
        assert!(scratch.exists(), "scratch worktree planted");

        // An uncommitted edit to a tracked file that recovery must NOT touch —
        // recovery is never surgery on the user's files; it only prunes scratch.
        std::fs::write(repo.join("a.txt"), "unsaved local work\n").unwrap();
        let commits_before = commit_count(&repo);

        settler_for(&repo).settle().unwrap();

        assert!(!scratch.exists(), "the scratch worktree is pruned");
        assert_eq!(
            commit_count(&repo),
            commits_before,
            "no advance is ever re-applied by recovery"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "unsaved local work\n",
            "the uncommitted edit survives untouched"
        );
    }

    // --- request-file staleness ----------------------------------------------

    #[test]
    fn a_request_within_the_window_is_not_stale() {
        let now = SystemTime::now();
        let req = Request::snapshot(Some("w".to_string()));
        assert!(!request_is_stale(&req, now));
    }

    #[test]
    fn a_request_past_the_window_is_stale() {
        let now = SystemTime::UNIX_EPOCH + request::STALE_AFTER + Duration::from_secs(1_000);
        // Stamped at the epoch: well past the staleness window relative to now.
        let req = Request {
            kind: RequestKind::Snapshot,
            watch: None,
            requested_at: 0,
        };
        assert!(request_is_stale(&req, now));
    }

    // --- request-file consumption --------------------------------------------

    #[test]
    fn only_plain_toml_names_are_settled() {
        assert!(is_settled_request_name("req.toml"));
        assert!(is_settled_request_name("snapshot-1234.toml"));
        // Hidden files are a writer's staging name, never consumed.
        assert!(!is_settled_request_name(".req.toml"));
        assert!(!is_settled_request_name(".hidden"));
        // Temp suffixes and non-toml names are not settled.
        assert!(!is_settled_request_name("req.toml.tmp"));
        assert!(!is_settled_request_name("req.toml.partial"));
        assert!(!is_settled_request_name("req.tmp"));
        assert!(!is_settled_request_name("notes.txt"));
        assert!(!is_settled_request_name("toml"));
    }

    #[test]
    fn drain_ignores_unsettled_files_and_consumes_settled_ones() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| dir.path().join(name);
        // A settled, valid request.
        std::fs::write(
            file("ok.toml"),
            "kind = \"snapshot\"\nwatch = \"vault\"\nrequested_at = 1752000000\n",
        )
        .unwrap();
        // A settled poison file: consumed (deleted) but produces no request.
        std::fs::write(file("poison.toml"), "not toml = = =").unwrap();
        // Unsettled names: a writer mid-flight, must be left untouched.
        std::fs::write(file(".staged.toml"), "kind = \"snapshot\"\n").unwrap();
        std::fs::write(file("mid.toml.tmp"), "kind = \"snapshot\"\n").unwrap();
        std::fs::write(file("part.partial"), "kind = \"snapshot\"\n").unwrap();

        let mut seen: Vec<Request> = Vec::new();
        drain_request_dir(dir.path(), |request| seen.push(request));

        assert_eq!(
            seen,
            vec![Request {
                kind: RequestKind::Snapshot,
                watch: Some("vault".to_string()),
                requested_at: 1_752_000_000,
            }],
            "exactly the one valid settled request is delivered"
        );
        assert!(!file("ok.toml").exists(), "a consumed request is deleted");
        assert!(
            !file("poison.toml").exists(),
            "a settled poison file is deleted so it cannot wedge the queue"
        );
        assert!(
            file(".staged.toml").exists(),
            "a dotfile is a writer mid-flight and must be left in place"
        );
        assert!(
            file("mid.toml.tmp").exists(),
            "a temp-suffixed file must be left in place"
        );
        assert!(
            file("part.partial").exists(),
            "a partial file must be left in place"
        );
    }

    // --- log level mapping ---------------------------------------------------

    #[test]
    fn log_level_maps_to_tracing_level() {
        assert_eq!(log_level_to_tracing(LogLevel::Error), tracing::Level::ERROR);
        assert_eq!(log_level_to_tracing(LogLevel::Warn), tracing::Level::WARN);
        assert_eq!(log_level_to_tracing(LogLevel::Info), tracing::Level::INFO);
        assert_eq!(log_level_to_tracing(LogLevel::Debug), tracing::Level::DEBUG);
        assert_eq!(log_level_to_tracing(LogLevel::Trace), tracing::Level::TRACE);
    }

    // --- config debounce ------------------------------------------------------

    #[test]
    fn config_debounce_needs_two_stable_polls_to_fire() {
        let h0: ConfigFingerprint = (0, 0);
        let h1: ConfigFingerprint = (0, 1);
        let mut d = ConfigDebounce::new(Some(h0));

        // No change: never fires.
        assert!(!d.poll(Some(h0)));
        // First sighting of the new value: still debouncing.
        assert!(!d.poll(Some(h1)));
        // Second, confirming poll at the same value: fires once.
        assert!(d.poll(Some(h1)));
        // Settled; no re-fire without a further change.
        assert!(!d.poll(Some(h1)));
    }

    #[test]
    fn config_debounce_cancels_a_reverted_change() {
        let h0: ConfigFingerprint = (0, 0);
        let h1: ConfigFingerprint = (0, 1);
        let mut d = ConfigDebounce::new(Some(h0));

        // A flicker to h1 then back to h0 before confirming: no fire.
        assert!(!d.poll(Some(h1)));
        assert!(!d.poll(Some(h0)));
        assert!(!d.poll(Some(h0)));
    }

    #[test]
    fn config_debounce_reports_a_pending_change_until_it_settles() {
        // The supervisor holds request draining while a change is pending (so a
        // request that followed a config write is answered only after the reload
        // lands). The flag must set on first sighting and clear once the change
        // settles or reverts.
        let h0: ConfigFingerprint = (0, 0);
        let h1: ConfigFingerprint = (0, 1);
        let h2: ConfigFingerprint = (0, 2);
        let mut d = ConfigDebounce::new(Some(h0));

        assert!(!d.change_pending(), "settled at start: nothing pending");
        // First sighting of a new value: a change is now pending.
        assert!(!d.poll(Some(h1)));
        assert!(d.change_pending(), "a first-seen change is pending");
        // The confirming poll settles it: no longer pending.
        assert!(d.poll(Some(h1)));
        assert!(!d.change_pending(), "a settled change is no longer pending");

        // A flicker that reverts before confirming clears pending too.
        assert!(!d.poll(Some(h2)));
        assert!(d.change_pending());
        assert!(!d.poll(Some(h1))); // back to the settled value h1
        assert!(!d.change_pending(), "a reverted change clears pending");
    }

    #[test]
    fn settled_tracks_the_confirmed_fingerprint_for_the_post_rebuild_recheck() {
        // The Reload arm drains a deferred request only when the on-disk config
        // still matches what the reload settled on. This exercises that seam: a
        // real config file drives the fingerprints, and `settled()` tracks the
        // confirmed value — a NEWER write (the in-flight-rebuild window) differs
        // from it, so the recheck (`config_fingerprint(disk) == settled()`) is
        // false and the drain is skipped.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "version = 1\n").unwrap();
        let fp0 = config_fingerprint(&cfg);
        let mut d = ConfigDebounce::new(fp0);
        assert_eq!(
            d.settled(),
            fp0,
            "settled starts at the initial fingerprint"
        );

        // A change settles after its confirming poll; settled then tracks it.
        std::fs::write(&cfg, "version = 1\n# edit A\n").unwrap();
        let fp1 = config_fingerprint(&cfg);
        assert!(!d.poll(fp1), "first sighting: pending, not settled");
        assert!(d.poll(fp1), "second sighting: settles");
        assert_eq!(d.settled(), fp1, "settled now tracks the confirmed change");
        assert_eq!(
            config_fingerprint(&cfg),
            d.settled(),
            "unchanged disk matches settled: the reload arm would drain"
        );

        // A newer write during the rebuild window no longer matches settled, so
        // the recheck skips the drain (the request stays queued).
        std::fs::write(&cfg, "version = 1\n# edit B (newer)\n").unwrap();
        assert_ne!(
            config_fingerprint(&cfg),
            d.settled(),
            "a newer write differs from settled: the reload arm would skip the drain"
        );
    }

    // --- config fingerprint (VRD-35) -------------------------------------------

    #[test]
    fn config_fingerprint_ignores_mtime_and_keys_off_content() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, b"version = 1\n").unwrap();
        let unchanged = config_fingerprint(&config);

        // Bump mtime with no content change: the fingerprint — and therefore
        // the debounce fed by it — must not see a change. This is the
        // behavior improvement over a pure mtime check: a hash-equal file
        // never triggers a reload no matter how its mtime moves.
        set_mtime(&config, SystemTime::now() + Duration::from_secs(120));
        assert_eq!(
            config_fingerprint(&config),
            unchanged,
            "identical content must fingerprint identically regardless of mtime"
        );

        let mut debounce = ConfigDebounce::new(unchanged);
        assert!(!debounce.poll(config_fingerprint(&config)));
        assert!(!debounce.poll(config_fingerprint(&config)));
    }

    #[test]
    fn config_fingerprint_distinguishes_missing_empty_and_length() {
        let dir = tempfile::tempdir().unwrap();

        // Missing file: `None`, matching the old mtime check's posture.
        let missing = dir.path().join("nope.toml");
        assert_eq!(config_fingerprint(&missing), None);

        // Empty file: `Some`, distinct from missing.
        let empty = dir.path().join("empty.toml");
        std::fs::write(&empty, b"").unwrap();
        let empty_fp = config_fingerprint(&empty).expect("empty file fingerprints as Some");
        assert_eq!(empty_fp.0, 0, "empty file has length 0");

        // The length component alone separates same-hash-risk inputs of
        // different sizes.
        let short = dir.path().join("short.toml");
        std::fs::write(&short, b"a").unwrap();
        assert_eq!(config_fingerprint(&short).unwrap().0, 1);
        assert_ne!(config_fingerprint(&short), Some(empty_fp));
    }

    // --- source-died backoff -------------------------------------------------

    #[test]
    fn backoff_starts_at_base_and_doubles_within_the_window() {
        let mut b = SourceDiedBackoff::new();
        let t = Instant::now();
        assert_eq!(b.on_failure(t), SOURCE_DIED_BACKOFF_BASE);
        // A quick second failure doubles.
        assert_eq!(
            b.on_failure(t + Duration::from_secs(1)),
            SOURCE_DIED_BACKOFF_BASE * 2
        );
        assert_eq!(
            b.on_failure(t + Duration::from_secs(2)),
            SOURCE_DIED_BACKOFF_BASE * 4
        );
    }

    #[test]
    fn backoff_resets_after_a_healthy_gap() {
        let mut b = SourceDiedBackoff::new();
        let t = Instant::now();
        b.on_failure(t);
        b.on_failure(t + Duration::from_secs(1)); // now at base*2
        // A failure well past the reset window starts over at the base.
        let healthy = t + SOURCE_DIED_BACKOFF_RESET_AFTER + Duration::from_secs(5);
        assert_eq!(b.on_failure(healthy), SOURCE_DIED_BACKOFF_BASE);
    }

    #[test]
    fn backoff_is_capped() {
        let mut b = SourceDiedBackoff::new();
        let mut t = Instant::now();
        let mut last = Duration::ZERO;
        for _ in 0..100 {
            last = b.on_failure(t);
            t += Duration::from_secs(1);
        }
        assert_eq!(
            last, SOURCE_DIED_BACKOFF_CAP,
            "backoff must saturate at the cap"
        );
    }

    // --- end-to-end smoke ----------------------------------------------------

    /// Runs a raw git command in `dir`, asserting success.
    fn git_ok(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Number of commits on HEAD.
    fn commit_count(repo: &Path) -> usize {
        let out = std::process::Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("failed to spawn git");
        if !out.status.success() {
            return 0;
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// The full commit message of HEAD.
    fn head_message(repo: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["log", "-1", "--format=%B"])
            .current_dir(repo)
            .output()
            .expect("failed to spawn git");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Initializes a git repo ready to commit into, with one root commit.
    fn init_repo(repo: &Path) {
        std::fs::create_dir_all(repo).unwrap();
        git_ok(repo, &["init", "-b", "main"]);
        git_ok(repo, &["config", "user.email", "vard-test@example.com"]);
        git_ok(repo, &["config", "user.name", "Vard Test"]);
        git_ok(repo, &["config", "commit.gpgsign", "false"]);
        git_ok(repo, &["commit", "--allow-empty", "-m", "root"]);
        assert_eq!(commit_count(repo), 1);
    }

    /// Forces a file's mtime to an exact value, standing in for a coarse
    /// (1-second granularity) filesystem where two quick writes can land in
    /// the same tick (VRD-35).
    fn set_mtime(path: &Path, time: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    /// Polls until `repo` has at least `at_least` commits, occasionally
    /// re-touching `touch` so an events watch fires even if the first write raced
    /// the watcher arming. The re-touch is sparse (every ~3s, never faster than
    /// the quiesce window) so the watch's quiescence can settle in the quiet gaps
    /// rather than being reset on every poll. `touch` is `None` for a watch
    /// driven by an explicit trigger (interval/manual). Generous budget for CI
    /// jitter.
    async fn wait_for_commits(repo: &Path, at_least: usize, touch: Option<&Path>) {
        for i in 0..400 {
            if commit_count(repo) >= at_least {
                return;
            }
            if let (Some(path), true) = (touch, i % 30 == 0) {
                let _ = std::fs::write(path, format!("poke {i}"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "repo never reached {at_least} commits (stuck at {})",
            commit_count(repo)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_snapshots_writes_and_manual_requests_then_shuts_down() {
        let root = tempfile::tempdir().unwrap();
        // Two watches on two repos, so the event and manual paths never race:
        // `auto` is events-driven (a write snapshots itself); `manual` is
        // interval-only with a huge interval (no filesystem watcher, no auto
        // snapshot), so only an injected request commits it.
        let auto = root.path().join("auto");
        let manual = root.path().join("manual");
        init_repo(&auto);
        init_repo(&manual);

        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        std::fs::write(
            &config_file,
            format!(
                "version = 1\n\n\
                 [[watch]]\nname = \"auto\"\npath = {auto:?}\ntrigger = \"events\"\nquiesce = \"500ms\"\n\n\
                 [[watch]]\nname = \"manual\"\npath = {manual:?}\ntrigger = \"interval\"\ninterval = \"24h\"\n",
            ),
        )
        .unwrap();

        let paths = DaemonPaths {
            config_file,
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths.clone(),
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        // A write to the events watch snapshots itself through the whole
        // pipeline. Re-touching removes any watcher-arm race.
        let auto_note = auto.join("note.md");
        std::fs::write(&auto_note, b"first note").unwrap();
        wait_for_commits(&auto, 2, Some(&auto_note)).await;
        assert!(
            head_message(&auto).contains("Vard-Trigger: event"),
            "the automatic snapshot must be event-triggered, got:\n{}",
            head_message(&auto)
        );

        // Make the interval-only watch dirty (no watcher, so nothing auto-fires)
        // and drop a manual snapshot request for it: only the request commits it.
        std::fs::write(manual.join("note.md"), b"manual note").unwrap();
        // A fresh manual request for the interval-only watch, stamped now so it
        // clears the daemon's staleness gate.
        request::write(
            &paths.request_dir,
            &request::Request::snapshot(Some("manual".to_string())),
        )
        .unwrap();
        wait_for_commits(&manual, 2, None).await;
        assert!(
            head_message(&manual).contains("Vard-Trigger: manual"),
            "the requested snapshot must be manual-triggered, got:\n{}",
            head_message(&manual)
        );

        // The consumed request file is deleted: no settled `*.toml` remains.
        let settled_remaining = || {
            std::fs::read_dir(&paths.request_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| e.file_name().to_str().is_some_and(is_settled_request_name))
                        .count()
                })
                .unwrap_or(0)
        };
        for _ in 0..50 {
            if settled_remaining() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            settled_remaining(),
            0,
            "a consumed request file must be deleted"
        );

        // A SIGTERM-equivalent shutdown drives a clean exit.
        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok, got {result:?}");

        // After a clean shutdown no journal holds a dangling begin: every
        // snapshot.started bracket was closed by its outcome, so each watch's
        // journal is compacted to empty (or was never written). A dangling
        // begin here would degrade recovery's foreign-lock protection on the
        // next start — the exact defect the skipped outcome exists to prevent.
        for (watch, repo) in [("auto", &auto), ("manual", &manual)] {
            let journal_path = Journal::for_repo_in_dir(&paths.journal_dir, repo);
            let len = std::fs::metadata(journal_path.path())
                .map(|meta| meta.len())
                .unwrap_or(0);
            assert_eq!(
                len, 0,
                "watch {watch:?} journal must hold no dangling begin after a clean shutdown"
            );
        }
    }

    /// A bare repository usable as a file remote.
    fn bare_origin(path: &Path) -> PathBuf {
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(path)
            .output()
            .expect("spawn git");
        path.to_path_buf()
    }

    /// A working repo with `origin` set, a base commit, and `main` pushed.
    fn synced_repo(repo: &Path, origin: &Path) {
        std::fs::create_dir_all(repo).unwrap();
        git_ok(repo, &["init", "-b", "main"]);
        git_ok(repo, &["config", "user.email", "vard-test@example.com"]);
        git_ok(repo, &["config", "user.name", "Vard Test"]);
        git_ok(repo, &["config", "commit.gpgsign", "false"]);
        git_ok(repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git_ok(repo, &["add", "-A"]);
        git_ok(repo, &["commit", "-m", "base"]);
        git_ok(repo, &["push", "-u", "origin", "main"]);
    }

    /// Commits reachable from the bare remote's `main`.
    fn remote_commit_count(origin: &Path) -> usize {
        let out = std::process::Command::new("git")
            .args(["rev-list", "--count", "refs/heads/main"])
            .current_dir(origin)
            .output()
            .expect("failed to spawn git");
        if !out.status.success() {
            return 0;
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_consumes_a_sync_request_and_pushes_to_the_remote() {
        // A sync-enabled watch on an interval-only trigger (no watcher, no auto
        // fire): the ONLY thing that drives a sync is the dropped request file.
        // Proves the daemon injects the reconcile scratch dir (so the cycle can
        // run at all) and routes a `kind = "sync"` request to `request_sync`.
        let root = tempfile::tempdir().unwrap();
        let origin = bare_origin(&root.path().join("origin.git"));
        let notes = root.path().join("notes");
        synced_repo(&notes, &origin);
        // Uncommitted local work: the sync's pre-sync snapshot commits it, then
        // the cycle pushes it — moving the remote from 1 commit to 2.
        std::fs::write(notes.join("draft.txt"), "local work\n").unwrap();

        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        std::fs::write(
            &config_file,
            format!(
                "version = 1\n\n\
                 [[watch]]\nname = \"notes\"\npath = {notes:?}\nsync = true\n\
                 branch = \"main\"\nremote = \"origin\"\ntrigger = \"interval\"\ninterval = \"24h\"\n",
            ),
        )
        .unwrap();

        let paths = DaemonPaths {
            config_file,
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths.clone(),
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        assert_eq!(remote_commit_count(&origin), 1, "remote starts at the base");
        request::write(
            &paths.request_dir,
            &request::Request::sync(Some("notes".to_string())),
        )
        .unwrap();

        // The consumed request drives the cycle, which pushes the pre-sync commit.
        let mut pushed = false;
        for _ in 0..400 {
            if remote_commit_count(&origin) >= 2 {
                pushed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(pushed, "the sync request must push the pre-sync commit");
        assert!(
            head_message(&notes).contains("Vard-Trigger: pre-sync"),
            "the pushed commit must be the pre-sync snapshot, got:\n{}",
            head_message(&notes)
        );

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok, got {result:?}");

        let journal_path = Journal::for_repo_in_dir(&paths.journal_dir, &notes);
        let len = std::fs::metadata(journal_path.path())
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert_eq!(len, 0, "a clean sync leaves no dangling journal record");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_added_after_daemon_start_is_picked_up_on_the_next_sync() {
        // Findings 4/5: the remote gate is LIVE in the sync cycle, not a
        // startup-only probe. A sync-enabled watch whose repo has no remote
        // *configured* when the daemon starts still gets its scratch injected;
        // once the remote is added (no daemon restart) a queued sync runs and
        // pushes to it.
        let root = tempfile::tempdir().unwrap();
        let origin = bare_origin(&root.path().join("origin.git"));
        let notes = root.path().join("notes");
        // A normal synced repo (origin has `main`), but the remote is then
        // REMOVED so the watch starts with no configured remote.
        synced_repo(&notes, &origin);
        git_ok(&notes, &["remote", "remove", "origin"]);

        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        std::fs::write(
            &config_file,
            format!(
                "version = 1\n\n\
                 [[watch]]\nname = \"notes\"\npath = {notes:?}\nsync = true\n\
                 branch = \"main\"\nremote = \"origin\"\ntrigger = \"interval\"\ninterval = \"24h\"\n",
            ),
        )
        .unwrap();

        let paths = DaemonPaths {
            config_file,
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths.clone(),
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        // The remote is added AFTER the daemon started (and some local work made).
        git_ok(
            &notes,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::write(notes.join("draft.txt"), "local work\n").unwrap();
        request::write(
            &paths.request_dir,
            &request::Request::sync(Some("notes".to_string())),
        )
        .unwrap();

        // The live gate now sees the remote, so the queued sync runs and pushes.
        // The remote starts at the base (1 commit); the drained sync pushes the
        // pre-sync commit, moving it to 2.
        let mut pushed = false;
        for _ in 0..400 {
            if remote_commit_count(&origin) >= 2 {
                pushed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            pushed,
            "a remote added after daemon start must be picked up with no restart"
        );

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok, got {result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sync_request_after_a_config_write_runs_against_the_reloaded_spec() {
        // Regression (VRD-40 review, finding 1): the `vard watch sync` daemon
        // path. The CLI writes the config (flipping `sync` off→on) and THEN drops
        // a sync request, both within one poll window. The daemon must answer
        // that request against the reloaded, sync-ENABLED spec — never the
        // pre-reload engine that still sees `sync = false` (which accepts the
        // request and does nothing, silently dropping the confirmation). Proven
        // by a real push to the remote.
        let root = tempfile::tempdir().unwrap();
        let origin = bare_origin(&root.path().join("origin.git"));
        let notes = root.path().join("notes");
        synced_repo(&notes, &origin);
        // Uncommitted local work the eventual sync snapshots and pushes.
        std::fs::write(notes.join("draft.txt"), "local work\n").unwrap();

        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        // Interval-only, 24h: nothing but the request ever drives a sync.
        let watch_block = |sync: bool| {
            format!(
                "version = 1\n\n\
                 [[watch]]\nname = \"notes\"\npath = {notes:?}\nsync = {sync}\n\
                 branch = \"main\"\nremote = \"origin\"\ntrigger = \"interval\"\ninterval = \"24h\"\n",
            )
        };
        // Starts with syncing OFF.
        std::fs::write(&config_file, watch_block(false)).unwrap();

        let paths = DaemonPaths {
            config_file: config_file.clone(),
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();
        let health_file = paths.health_file.clone();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths.clone(),
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        // Wait until the daemon captured its initial (sync = false) fingerprint,
        // so the edit below is guaranteed to register as a change.
        for i in 0..200 {
            if health_file.exists() {
                break;
            }
            assert!(i < 199, "daemon never reached its supervise loop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Mimic `vard watch sync notes`: flip sync on, THEN queue the request.
        std::fs::write(&config_file, watch_block(true)).unwrap();
        request::write(
            &paths.request_dir,
            &request::Request::sync(Some("notes".to_string())),
        )
        .unwrap();

        // The request, held until the reload lands, runs against the sync-enabled
        // spec and pushes the pre-sync snapshot — origin moves from 1 commit to 2.
        let mut pushed = false;
        for _ in 0..400 {
            if remote_commit_count(&origin) >= 2 {
                pushed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            pushed,
            "a sync request that followed the sync-enabling config write must run \
             against the reloaded spec and push, not be dropped by the pre-reload engine"
        );

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok, got {result:?}");
    }

    use crate::journal::test_support::{plant_crashed, retry_until};

    fn id(name: &str, path: &Path) -> WatchIdentity {
        WatchIdentity::new(name.to_string(), path.to_path_buf())
    }

    #[test]
    fn drain_removed_watches_cleans_a_dropped_watch_and_spares_survivors() {
        // A reload drops one watch and renames another (same path). The dropped
        // watch's stale lock is drained; the renamed one — surviving by path key
        // — is left for the running engine, proving a rename never orphans.
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let paths = DaemonPaths {
            config_file: state.join("config.toml"),
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        let removed_repo = root.path().join("removed");
        let kept_repo = root.path().join("kept");
        let (_, removed_lock) = plant_crashed(&paths.journal_dir, &removed_repo);
        let (_, kept_lock) = plant_crashed(&paths.journal_dir, &kept_repo);

        let before = vec![id("removed", &removed_repo), id("kept", &kept_repo)];
        // "kept" survives under a new NAME but the same PATH — a rename.
        let after = vec![id("kept-renamed", &kept_repo)];

        drain_removed_watches(&paths, &before, &after);

        assert!(
            !removed_lock.exists(),
            "the dropped watch's stale lock must be drained"
        );
        assert!(
            kept_lock.exists(),
            "a renamed watch (same path key) is not a removal and must not be drained"
        );
    }

    #[test]
    fn cached_journal_key_is_stable_after_the_directory_is_removed() {
        // An identity built while the directory exists keys off its canonical
        // path. If the directory is then removed, the cached key must not flip to
        // the textual fallback — recomputing it would truncate the wrong file on
        // `complete`. The cache is what guarantees stability.
        // Reach the repo through a symlinked component so canonical and textual
        // forms provably differ on every platform (macOS tempdirs get this for
        // free via /var -> /private/var; Linux tempdir paths have no symlink,
        // so the hazard must be constructed explicitly).
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("repo")).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("link")).unwrap();
        let repo = root.path().join("link").join("repo");
        let identity = id("w", &repo);
        let key_before = identity.journal_key.clone();

        std::fs::remove_dir_all(&real).unwrap();
        // Recomputing from scratch now takes the textual fallback and differs...
        assert_ne!(
            key_before,
            journal::journal_file_name(&repo),
            "canonical vs textual key differ once the dir is gone (the hazard)"
        );
        // ...but the cached identity is unchanged, so begin/complete agree.
        assert_eq!(identity.journal_key, key_before, "the cached key is stable");
    }

    #[test]
    fn op_lock_makes_a_reload_overlap_structurally_safe() {
        // VRD-37 replaces the old superseded-key skip with a structural invariant.
        // The reload window: the OLD engine's worker holds a watch's op lock
        // mid-op while the NEW engine's worker (same journal key across the swap)
        // tries to snapshot the same watch. The op lock makes the overlap safe —
        // the new worker's gate returns busy, so it requeues rather than opening a
        // second journal bracket or racing a second commit onto one repo.
        use vard_core::{OpGate, OpGuard};
        let root = tempfile::tempdir().unwrap();
        let journal_dir = root.path().join("journal");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let identity = id("w", &repo);

        // Old worker admits an operation: op lock held, `begin` written.
        let old_gate = identity.op_gate(&journal_dir);
        let old_guard: Box<dyn OpGuard> = old_gate
            .begin("snapshot")
            .expect("op-lock I/O")
            .expect("the op lock is initially free");
        let journal = identity.journal(&journal_dir);
        assert!(!journal.is_clean(), "the old worker's begin is recorded");

        // New worker (a fresh gate for the same watch) is refused: busy.
        let new_gate = identity.op_gate(&journal_dir);
        assert!(
            new_gate.begin("snapshot").expect("op-lock I/O").is_none(),
            "the new engine's worker must see the op lock busy and requeue, never \
             double-journal or double-commit onto the same watch"
        );

        // The old worker completes: journal compacted, lock released.
        old_guard.complete();
        assert!(journal.is_clean(), "complete compacts the shared journal");

        // Only now can the new worker proceed. Retry briefly: a *sibling* test's
        // `Command::spawn` forks this process, and between the fork and the
        // child's exec (which closes O_CLOEXEC fds) the child transiently shares
        // our just-released lock fd, holding the flock a microsecond longer. This
        // window is a pure test artifact (production never reacquires like this);
        // instance.rs documents the same race.
        let mut new_guard: Option<Box<dyn OpGuard>> = None;
        retry_until(|| match new_gate.begin("snapshot").expect("op-lock I/O") {
            Some(guard) => {
                new_guard = Some(guard);
                true
            }
            None => false,
        });
        new_guard
            .expect("with the old op done, the new worker must be able to proceed")
            .complete();
    }

    #[test]
    fn aliased_specs_skip_the_later_colliding_watch_deterministically() {
        // Two specs whose paths canonicalize to one repository (a directory and
        // a symlink to it) share a journal key. The dedup keeps the first in
        // config order and drops the second, deterministically.
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let first = WatchSpec::builder("first", &real).build().unwrap();
        let second = WatchSpec::builder("second", &link).build().unwrap();
        let kept = dedup_aliased_specs(vec![first, second]);
        assert_eq!(kept.len(), 1, "the aliased later watch is skipped");
        assert_eq!(kept[0].name(), "first", "first in config order wins");

        // Reversing config order flips the winner — the rule is positional.
        let first = WatchSpec::builder("first", &real).build().unwrap();
        let second = WatchSpec::builder("second", &link).build().unwrap();
        let kept = dedup_aliased_specs(vec![second, first]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name(), "second");
    }

    // --- per-watch open isolation (VRD-41) ------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unopenable_repo_is_skipped_while_its_healthy_sibling_still_starts() {
        // One spec points at a plain directory (never `git init`-ed, so
        // `open_git_backend` fails with `NotARepo`); the other is a real repo.
        // Before VRD-41, `Engine::build` opened every watch itself and one bad
        // repository failed the whole build — snapshot protection for the
        // healthy watch too. Now the bad watch is vetted and dropped HERE, so
        // the engine still builds and starts, supervising only the survivor.
        let root = tempfile::tempdir().unwrap();
        let broken = root.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        let healthy = root.path().join("healthy");
        init_repo(&healthy);

        let journal_dir = root.path().join("journal");
        let specs = vec![
            WatchSpec::builder("broken", &broken).build().unwrap(),
            WatchSpec::builder("healthy", &healthy).build().unwrap(),
        ];

        let (handle, _events, watches, skipped, _hooks) =
            build_started_engine_from_specs(&journal_dir, specs, None, None)
                .await
                .expect("one unopenable repo must not fail the whole engine build");

        assert_eq!(
            watches.len(),
            1,
            "only the healthy watch survives into the supervised identity set"
        );
        assert_eq!(watches[0].name, "healthy");

        // The skipped watch is not just logged and dropped — it comes back as a
        // health problem, so `vard status`/`notify` inherit honesty rather than
        // reading a missing watch as a false "ok" (the honesty gap this closes).
        assert_eq!(
            skipped.len(),
            1,
            "exactly one health problem for the one skipped watch"
        );
        assert_eq!(skipped[0].watch, "broken");
        assert_eq!(skipped[0].state, "attention");
        assert_eq!(skipped[0].kind, "unopenable");
        assert!(
            !skipped[0].summary.is_empty(),
            "the summary carries the open error, not left blank"
        );

        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_repo_unopenable_still_builds_and_starts_a_zero_watch_engine() {
        // The whole-engine-failure regression this ticket closes: a single spec
        // whose repository cannot be opened must no longer bubble up as
        // `Err(EngineError::Backend { .. })` from `Engine::build`. It is skipped
        // and logged, and the engine proceeds supervising nothing — the same
        // idle shape an all-paused config already produces (`Engine::start`
        // arms no workers and still emits `DaemonStarted` for zero watches).
        let root = tempfile::tempdir().unwrap();
        let broken = root.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();

        let journal_dir = root.path().join("journal");
        let specs = vec![WatchSpec::builder("broken", &broken).build().unwrap()];

        let (handle, _events, watches, skipped, _hooks) =
            build_started_engine_from_specs(&journal_dir, specs, None, None)
                .await
                .expect("an engine with zero survivors must still build and start, not fail whole");

        assert!(
            watches.is_empty(),
            "the sole watch failed to open and is not supervised"
        );
        assert_eq!(
            skipped.len(),
            1,
            "the sole skipped watch still surfaces as a health problem"
        );
        assert_eq!(skipped[0].watch, "broken");

        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_paused_startup_runs_idle_then_shuts_down_cleanly() {
        // A config that defines a watch but has it paused is not "no watches":
        // the daemon must start idle (supervising nothing) rather than exit,
        // so a later resume can hot-reload it back.
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo);
        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        std::fs::write(
            &config_file,
            format!("version = 1\n\n[[watch]]\nname = \"w\"\npath = {repo:?}\npaused = true\n"),
        )
        .unwrap();

        let paths = DaemonPaths {
            config_file,
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths,
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        // Give it a moment to reach its idle supervise loop, then confirm it is
        // still running (a wrong "no watches" exit would have finished it).
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !daemon.is_finished(),
            "an all-paused daemon must stay running idle, not exit"
        );

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("idle daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(
            result.is_ok(),
            "clean idle shutdown returns Ok, got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_hook_fires_on_snapshot_and_re_arms_across_a_reload() {
        // A watch-scoped `snapshot_completed` hook runs its shell command when the
        // watch commits, and after a config reload the runner is re-armed against
        // the new engine's bus with the reload's new command. Markers live outside
        // the watched repo so firing a hook cannot itself trigger a snapshot.
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo);
        let before = root.path().join("before-marker");
        let after = root.path().join("after-marker");

        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        let config_text = |marker: &Path| {
            format!(
                "version = 1\n\n\
                 [[watch]]\nname = \"w\"\npath = {repo:?}\ntrigger = \"events\"\nquiesce = \"300ms\"\n\n\
                 [watch.hooks]\nsnapshot_completed = {cmd:?}\n",
                cmd = format!("touch {}", marker.display()),
            )
        };
        std::fs::write(&config_file, config_text(&before)).unwrap();

        let paths = DaemonPaths {
            config_file: config_file.clone(),
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths.clone(),
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(100),
        ));

        // A write commits a snapshot, which fires the pre-reload hook.
        let note = repo.join("note.md");
        std::fs::write(&note, b"one").unwrap();
        wait_for_commits(&repo, 2, Some(&note)).await;
        wait_for_marker(&before).await;

        // Rewrite the config with a new hook command; the content-fingerprint
        // poll detects it and reloads, re-arming the runner on the new engine.
        // Let the debounce settle and the engine rebuild land before triggering
        // the next snapshot, so it is unambiguously the reloaded hook that fires.
        std::fs::write(&config_file, config_text(&after)).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // A fresh write after the reload commits again and must fire the *new*
        // hook. Re-touch to clear any watcher-arm race across the rebuild.
        std::fs::write(&note, b"two").unwrap();
        wait_for_commits(&repo, 3, Some(&note)).await;
        wait_for_marker(&after).await;

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok, got {result:?}");
    }

    #[test]
    fn project_hook_health_maps_failures_and_sorts_suppression() {
        use crate::hooks::FailingHook;
        let mut suppressed = std::collections::HashMap::new();
        suppressed.insert("zebra".to_string(), 3u64);
        suppressed.insert("apple".to_string(), 7u64);
        suppressed.insert("silent".to_string(), 0u64); // a zero count is dropped
        let snapshot = RunnerSnapshot {
            suppressed_by_watch: suppressed,
            failing: vec![
                FailingHook {
                    watch: Some("notes".to_string()),
                    event: "snapshot.completed".to_string(),
                    command: "apply".to_string(),
                    consecutive: 4,
                    last_error: "exited with status 1".to_string(),
                },
                FailingHook {
                    watch: None,
                    event: "daemon.started".to_string(),
                    command: "up".to_string(),
                    consecutive: 3,
                    last_error: "timed out".to_string(),
                },
            ],
        };
        let (problems, suppressions) = project_hook_health(&snapshot, 500);

        // Every failing hook becomes a hook-failing / attention problem at `now`.
        assert_eq!(problems.len(), 2);
        assert!(
            problems
                .iter()
                .all(|p| p.kind == "hook-failing" && p.state == "attention" && p.since == 500)
        );
        // The watch-scoped hook keeps its watch; the global one carries the empty
        // marker.
        assert!(problems.iter().any(|p| p.watch == "notes"));
        assert!(problems.iter().any(|p| p.watch.is_empty()));

        // Suppression: the zero count is dropped and the rest are sorted by watch
        // (the runner keeps them in a nondeterministic HashMap).
        let names: Vec<_> = suppressions.iter().map(|s| s.watch.clone()).collect();
        assert_eq!(names, vec!["apple", "zebra"]);
        assert_eq!(suppressions[0].count, 7);
    }

    /// Polls until `marker` exists (a hook side effect) or a generous budget
    /// elapses. Real time; the hook itself is a sub-millisecond `touch`.
    async fn wait_for_marker(marker: &Path) {
        for _ in 0..200 {
            if marker.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("hook marker {} never appeared", marker.display());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn startup_recovers_a_paused_watchs_stale_lock() {
        // A watch that crashed while active and was then paused still owns a
        // provably-ours stale index.lock wedging the user's own git. Startup
        // recovery must cover every DEFINED watch, paused included — not just the
        // active engine specs — so the lock is cleaned even though the paused watch
        // never enters the (idle) engine.
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo);
        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        std::fs::write(
            &config_file,
            format!("version = 1\n\n[[watch]]\nname = \"w\"\npath = {repo:?}\npaused = true\n"),
        )
        .unwrap();

        let paths = DaemonPaths {
            config_file,
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();

        // Plant a crash residue (dead-pid path-keyed journal + aged lock) for the
        // paused repo.
        let (_repo, lock) = plant_crashed(&paths.journal_dir, &repo);
        assert!(lock.exists(), "precondition: the stale lock is present");

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths,
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        // Give startup time to run recovery, then confirm the paused watch's stale
        // lock was cleaned.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !lock.exists(),
            "a paused watch's provably-ours stale lock must be recovered at startup"
        );

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("idle daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(
            result.is_ok(),
            "clean idle shutdown returns Ok, got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reloading_to_all_paused_keeps_the_daemon_idle() {
        // Pausing the last active watch is a legitimate reload to an idle
        // engine, not a config that should be rejected: the daemon must stay up
        // and shut down cleanly rather than dying on a "no watches" reload.
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo);
        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        let active = format!(
            "version = 1\n\n[[watch]]\nname = \"w\"\npath = {repo:?}\ntrigger = \"events\"\nquiesce = \"500ms\"\n",
        );
        std::fs::write(&config_file, &active).unwrap();

        let paths = DaemonPaths {
            config_file: config_file.clone(),
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths.clone(),
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        // The active watch snapshots a write, proving it is live before we pause.
        let note = repo.join("note.md");
        std::fs::write(&note, b"first").unwrap();
        wait_for_commits(&repo, 2, Some(&note)).await;

        // Pause the last watch and reload: the daemon rebuilds to an idle engine.
        std::fs::write(
            &config_file,
            format!("version = 1\n\n[[watch]]\nname = \"w\"\npath = {repo:?}\ntrigger = \"events\"\nquiesce = \"500ms\"\npaused = true\n"),
        )
        .unwrap();
        reload.notify_one();

        // Give the reload time to swap in the idle engine, then confirm the
        // daemon is still alive (a "no watches" reject would have kept the old
        // engine; a wrong exit would have finished the task).
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !daemon.is_finished(),
            "daemon must stay running after pausing its last watch"
        );

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok, got {result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_detects_a_second_same_tick_config_edit_by_content() {
        // Found in VRD-15's review, fixed as VRD-35: an mtime-only debounce
        // can settle on a value and then silently ignore a later edit that
        // happens to land on the exact same (coarse, 1-second) mtime. Pin two
        // content-different writes to the identical mtime and confirm the
        // second is still detected — proof the poll's "did it change?"
        // predicate keys off content.
        //
        // Detection of edit B is observed DETERMINISTICALLY, never by a blind
        // sleep: edit B both pauses `w` and activates a sentinel watch `s`
        // that no earlier engine ever had active, so a commit landing in `s`'s
        // repo (poll-with-deadline via `wait_for_commits`) can only come from
        // the engine built from edit B's content. Drain-and-rebuild joins the
        // old engine before starting the new one, and the new engine excludes
        // the re-paused `w` structurally, so once `s` commits, nothing is
        // supervising `w` and the no-new-commit assertion cannot race.
        let root = tempfile::tempdir().unwrap();
        let w_repo = root.path().join("w-repo");
        let s_repo = root.path().join("s-repo");
        init_repo(&w_repo);
        init_repo(&s_repo);
        let state = root.path().join("state");
        let config_file = root.path().join("config.toml");
        let watch_toml = |name: &str, repo: &Path, paused: bool| {
            format!(
                "[[watch]]\nname = \"{name}\"\npath = {repo:?}\ntrigger = \"events\"\nquiesce = \"200ms\"\npaused = {paused}\n"
            )
        };
        let all_paused = format!(
            "version = 1\n\n{}\n{}",
            watch_toml("w", &w_repo, true),
            watch_toml("s", &s_repo, true)
        );
        std::fs::write(&config_file, &all_paused).unwrap();

        let paths = DaemonPaths {
            config_file: config_file.clone(),
            lock_file: state.join("vard.lock"),
            request_dir: state.join("requests"),
            journal_dir: state.join("journal"),
            reconcile_dir: state.join("reconcile"),
            health_file: state.join("health"),
        };
        std::fs::create_dir_all(&paths.request_dir).unwrap();
        let health_file = paths.health_file.clone();

        let shutdown = Arc::new(Notify::new());
        let reload = Arc::new(Notify::new());
        let config = Config::load(&paths.config_file).unwrap();
        let daemon = tokio::spawn(run_daemon(
            paths,
            config,
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Duration::from_millis(150),
        ));

        // Wait (with deadline) for the supervise loop's startup health write:
        // it happens after the initial config fingerprint is captured, so once
        // the file exists, edit A below is guaranteed to register as a change.
        for i in 0..200 {
            if health_file.exists() {
                break;
            }
            assert!(i < 199, "daemon never reached its supervise loop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Edit A activates `w` (sentinel stays paused). Pin its mtime to a
        // fixed instant.
        let same_tick = SystemTime::now();
        let edit_a = format!(
            "version = 1\n\n{}\n{}",
            watch_toml("w", &w_repo, false),
            watch_toml("s", &s_repo, true)
        );
        std::fs::write(&config_file, &edit_a).unwrap();
        set_mtime(&config_file, same_tick);

        // Edit A's reload is proven complete when `w` snapshots a write.
        let w_note = w_repo.join("note.md");
        std::fs::write(&w_note, b"first").unwrap();
        wait_for_commits(&w_repo, 2, Some(&w_note)).await;

        // Edit B re-pauses `w` and activates the sentinel `s`: different
        // content, but pinned to the SAME mtime as edit A — simulating two
        // back-to-back CLI writes landing in one mtime tick on a coarse
        // filesystem.
        let edit_b = format!(
            "version = 1\n\n{}\n{}",
            watch_toml("w", &w_repo, true),
            watch_toml("s", &s_repo, false)
        );
        std::fs::write(&config_file, &edit_b).unwrap();
        set_mtime(&config_file, same_tick);

        // The deterministic reload-complete signal: only the engine built from
        // edit B's content has `s` active, so this commit proves edit B was
        // detected despite the unchanged mtime. An mtime-only debounce would
        // hang here (and the harness would panic at its deadline).
        let s_note = s_repo.join("note.md");
        std::fs::write(&s_note, b"sentinel").unwrap();
        wait_for_commits(&s_repo, 2, Some(&s_note)).await;

        // The same reload structurally dropped `w` from the engine, so a
        // further write must not produce a commit. The grace period only
        // strengthens the check (a wrongly-live watch would need its 200ms
        // quiesce to commit); the reload itself was already proven above, so
        // this cannot flake into a false failure.
        let before = commit_count(&w_repo);
        std::fs::write(&w_note, b"second").unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            commit_count(&w_repo),
            before,
            "edit B (same mtime as edit A, different content) must still be \
             detected and re-pause the watch"
        );

        shutdown.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon must exit promptly on shutdown")
            .expect("daemon task must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok, got {result:?}");
    }
}
