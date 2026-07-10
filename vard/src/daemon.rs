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
//! 4. Supervise it: log every bus [`Event`], bracket each commit window in the
//!    per-watch journal (`begin` on `snapshot.started`, `complete` on the
//!    `snapshot.completed`/`snapshot.failed`/`snapshot.skipped` outcome the
//!    engine guarantees follows it — see [`journal_event`]), reload on SIGHUP
//!    or a config-file edit, rebuild on a dead signal source with exponential
//!    backoff, drain snapshot/sync request files, and shut down cleanly on
//!    SIGINT/SIGTERM.
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
//! - `kind = "sync"` is accepted but not yet implemented (VRD-19); it is logged
//!   and dropped.
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
//! # Config and request watching: mtime polling, not a notifier
//!
//! Both the config file and the request directory are watched by a lightweight
//! mtime poll (default every [`DEFAULT_POLL_INTERVAL`]), not by arming a
//! `vard-core` [`Watcher`](vard_core::Watcher) on them. Polling is simpler and
//! avoids a feedback loop on the request directory (the daemon itself deletes
//! files there, which a notifier would report back as activity); a couple of
//! seconds of latency on a control-plane path is immaterial. Config edits are
//! debounced (see [`MtimeDebounce`]) so an editor writing several times in a row
//! collapses into a single rebuild.
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
use crate::health::{self, HealthTracker};
use crate::instance::{InstanceLock, LockError, LockRole};
use crate::journal::{Journal, RecoveryOpts, RecoveryReport};
use crate::paths::{self, HomeNotFound};
use crate::request::{self, Request, RequestKind};

/// How often the supervisor polls the config file's mtime and drains the
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
            health_file: paths::health_file()?,
        })
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
    let specs: Vec<WatchSpec> = defined
        .iter()
        .filter(|w| !w.paused)
        .map(|w| w.spec.clone())
        .collect();
    if specs.is_empty() {
        info!("all configured watches are paused; starting idle (a resume will reload)");
    }

    recover_stale_locks(&paths, &specs);

    let (handle, events, watch_names) = build_started_engine_from_specs(specs)
        .await
        .map_err(StartupError::Engine)?;

    supervise(
        paths,
        handle,
        events,
        watch_names,
        shutdown,
        reload,
        poll_interval,
    )
    .await;
    Ok(())
}

/// Runs journal recovery for the given watches, cleaning a provably stale git
/// index lock left by a previous crash. Every non-`Clean` outcome is logged;
/// nothing here can fail the daemon (recovery folds all trouble into its
/// report).
fn recover_stale_locks<'a>(paths: &DaemonPaths, specs: impl IntoIterator<Item = &'a WatchSpec>) {
    for spec in specs {
        let journal = Journal::in_dir(&paths.journal_dir, spec.name());
        let report = journal.recover(spec.path(), RecoveryOpts::new());
        match report {
            RecoveryReport::Clean => {}
            RecoveryReport::LockRemoved { .. } => {
                warn!(watch = spec.name(), report = %report, "recovered a stale git lock");
            }
            // A foreign lock is currently wedging a watched repo — operator
            // significant, even though it is not ours to remove.
            RecoveryReport::LockNotOurs { .. } | RecoveryReport::HolderAlive { .. } => {
                warn!(watch = spec.name(), report = %report, "journal recovery");
            }
            other => {
                info!(watch = spec.name(), report = %other, "journal recovery");
            }
        }
    }
}

/// Builds a git-backed engine from resolved specs, subscribes, and starts it.
/// Returns the handle, a fresh event subscriber, and the ordered watch names
/// (used to fan a `watch`-less snapshot request out to every watch).
async fn build_started_engine_from_specs(
    specs: Vec<WatchSpec>,
) -> Result<(EngineHandle, EventReceiver, Vec<String>), EngineError> {
    let watch_names: Vec<String> = specs.iter().map(|spec| spec.name().to_string()).collect();
    let mut builder = Engine::builder();
    for spec in specs {
        builder = builder.watch(spec);
    }
    let engine = builder.build()?;
    let events = engine.subscribe();
    let handle = engine.start().await?;
    Ok((handle, events, watch_names))
}

/// Loads the current config from disk and starts a fresh engine from it, for a
/// reload or a source-died rebuild. Returns `None` (leaving the caller's current
/// engine untouched) on any error, so a bad edit never takes a healthy daemon
/// down.
///
/// `known_watches` are the names already supervised before this rebuild:
/// journal recovery runs for any watch *not* in it (initial startup recovered
/// the rest), so a watch introduced by a reload gets the same stale-lock
/// cleaning a startup watch does — before its engine arms. (Recovering a
/// journal orphaned by a watch *rename* — the journal is keyed by the old
/// name — is a separate tracked task, not attempted here.)
async fn build_started_engine(
    paths: &DaemonPaths,
    known_watches: &[String],
) -> Option<(EngineHandle, EventReceiver, Vec<String>)> {
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
    let specs: Vec<WatchSpec> = defined
        .iter()
        .filter(|w| !w.paused)
        .map(|w| w.spec.clone())
        .collect();
    if specs.is_empty() {
        info!("reload: all watches are paused; running idle");
    }

    recover_stale_locks(
        paths,
        specs
            .iter()
            .filter(|spec| !known_watches.iter().any(|known| known == spec.name())),
    );

    match build_started_engine_from_specs(specs).await {
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
    watch_names: Vec<String>,
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
/// the old is drained, so there is a brief window where both are armed — benign,
/// since concurrent git commits serialize on the index lock — but it guarantees
/// a valid `(handle, events)` at all times, honoring "keep the old engine on any
/// reload error".
///
/// While the old engine drains (up to its shutdown budget), the *new* engine's
/// bus is pumped concurrently through the same log/journal path the supervisor
/// uses: without this, a busy new engine could overflow its subscriber buffer
/// during a slow drain, dropping events and leaving journal brackets
/// unbalanced.
async fn try_rebuild(
    paths: &DaemonPaths,
    old_handle: EngineHandle,
    mut old_events: EventReceiver,
    old_names: Vec<String>,
    health: &mut HealthTracker,
) -> RebuildOutcome {
    match build_started_engine(paths, &old_names).await {
        Some((handle, mut events, names)) => {
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
                            if matches!(handle_bus(received, paths, health), Action::EngineFailure) {
                                failure_seen = true;
                            }
                            if closed {
                                pumping = false;
                            }
                        }
                    }
                }
            }
            drain_remaining_events(paths, &mut old_events, health);
            // Reconcile the health tracker to the new active watch set: a reload
            // that removed or renamed a watch leaves no recovery event behind,
            // so prune its stale problem here rather than report a watch that no
            // longer exists forever.
            if health.retain_active(&names) {
                write_health(health, paths);
            }
            RebuildOutcome {
                handle,
                events,
                watch_names: names,
                rebuilt: true,
                failure_seen,
            }
        }
        None => RebuildOutcome {
            handle: old_handle,
            events: old_events,
            watch_names: old_names,
            rebuilt: false,
            failure_seen: false,
        },
    }
}

/// Processes (logs + journals) every event still buffered on a receiver whose
/// engine has fully shut down. `EngineHandle::shutdown` joins every task before
/// emitting the final `DaemonStopped`, so once it returns the buffer holds the
/// complete tail of the stream — draining it here guarantees each journal
/// `begin` written earlier meets the outcome that completes it, instead of
/// dangling because the supervisor stopped reading mid-bracket.
fn drain_remaining_events(
    paths: &DaemonPaths,
    events: &mut EventReceiver,
    health: &mut HealthTracker,
) {
    use vard_core::TryRecvError;
    loop {
        match events.try_recv() {
            Ok(event) => {
                log_event(&event);
                journal_event(paths, &event);
                apply_health(health, paths, &event);
            }
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
    mut watch_names: Vec<String>,
    shutdown: Arc<Notify>,
    reload: Arc<Notify>,
    poll_interval: Duration,
) {
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut config_mtime = MtimeDebounce::new(file_mtime(&paths.config_file));
    let mut backoff = SourceDiedBackoff::new();
    // When set, a source-died rebuild is due at this deadline.
    let mut rebuild_at: Option<TokioInstant> = None;

    // Write a fresh health file on startup: a clean, empty problem set that
    // supersedes any stale file a previous crash may have left behind.
    let mut health = HealthTracker::new();
    write_health(&health, &paths);

    loop {
        let action = tokio::select! {
            _ = shutdown.notified() => Action::Shutdown,
            _ = reload.notified() => {
                info!("reload requested (SIGHUP)");
                Action::Reload
            }
            _ = poll.tick() => {
                process_requests(&paths, &handle, &watch_names);
                if config_mtime.poll(file_mtime(&paths.config_file)) {
                    info!("config file changed; reloading");
                    Action::Reload
                } else {
                    Action::Continue
                }
            }
            _ = tokio::time::sleep_until(rebuild_at.unwrap_or_else(TokioInstant::now)),
                if rebuild_at.is_some() =>
            {
                Action::BackoffRebuild
            }
            received = events.recv() => handle_bus(received, &paths, &mut health),
        };

        match action {
            Action::Continue => {}
            Action::Shutdown => break,
            Action::Reload => {
                let outcome = try_rebuild(&paths, handle, events, watch_names, &mut health).await;
                handle = outcome.handle;
                events = outcome.events;
                watch_names = outcome.watch_names;
                if outcome.rebuilt {
                    // A full rebuild supersedes any pending source-died retry.
                    rebuild_at = None;
                }
                if outcome.failure_seen {
                    schedule_rebuild(&mut backoff, &mut rebuild_at, "engine failed during swap");
                }
            }
            Action::BackoffRebuild => {
                rebuild_at = None;
                let outcome = try_rebuild(&paths, handle, events, watch_names, &mut health).await;
                handle = outcome.handle;
                events = outcome.events;
                watch_names = outcome.watch_names;
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
    // The drain finishes any in-flight pass, so trailing events (including the
    // final DaemonStopped) are logged and their journal brackets closed.
    drain_remaining_events(&paths, &mut events, &mut health);
    // Clean shutdown: remove the health file so `vard notify` reports the
    // daemon-not-running line rather than treating a stale problem set as
    // current.
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

/// Handles one item from the event bus: logs it, journals the commit window if
/// applicable, updates the health file if the event changed a watch's state,
/// and reports whether it signals engine failure (a dead signal source or a
/// closed bus) that warrants a rebuild.
fn handle_bus(
    received: Result<Event, RecvError>,
    paths: &DaemonPaths,
    health: &mut HealthTracker,
) -> Action {
    match received {
        Ok(event) => {
            log_event(&event);
            journal_event(paths, &event);
            apply_health(health, paths, &event);
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
        Event::SyncPushed { watch } => info!(event = name, %watch, "pushed to remote"),
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

/// Brackets a snapshot's commit window in the per-watch journal: `begin` on the
/// pre-commit [`Event::SnapshotStarted`], `complete` on the outcome that closes
/// its bracket — [`Event::SnapshotCompleted`], [`Event::SnapshotFailed`], or
/// [`Event::SnapshotSkipped`] (the engine guarantees exactly one of the three
/// follows every started). Completing on skipped outcomes is what keeps the
/// journal empty after a no-op sweep, so recovery's "no dangling begin means
/// any git lock is foreign — never touch it" protection stays meaningful.
/// Journal errors are logged and never propagated — a journaling hiccup must
/// not crash the daemon or interrupt snapshotting.
fn journal_event(paths: &DaemonPaths, event: &Event) {
    match event {
        Event::SnapshotStarted { watch, .. } => {
            let journal = Journal::in_dir(&paths.journal_dir, watch);
            if let Err(err) = journal.begin("snapshot") {
                warn!(%watch, error = %err, "journal begin failed");
            }
        }
        Event::SnapshotCompleted { watch, .. }
        | Event::SnapshotFailed { watch, .. }
        | Event::SnapshotSkipped { watch, .. } => {
            let journal = Journal::in_dir(&paths.journal_dir, watch);
            if let Err(err) = journal.complete() {
                warn!(%watch, error = %err, "journal complete failed");
            }
        }
        _ => {}
    }
}

/// Folds one event into the health tracker and, when it changed the problem
/// set (only [`Event::WatchStateChanged`] can), rewrites the health file so it
/// always reflects *current* problem state — including a watch recovering to
/// `Ok`, which clears its entry. A write failure is warned, never fatal: a
/// health-file hiccup must not crash the daemon or interrupt snapshotting, and
/// `vard notify` degrades to the daemon-not-running/stale path on its own.
fn apply_health(health: &mut HealthTracker, paths: &DaemonPaths, event: &Event) {
    if health.observe(event, health::now_secs()) {
        write_health(health, paths);
    }
}

/// Writes the tracker's current problem set to the health file (atomically),
/// warning on failure. Shared by the per-event update and the startup
/// fresh-write.
fn write_health(health: &HealthTracker, paths: &DaemonPaths) {
    let doc = health.doc(health::now_secs());
    if let Err(err) = health::write(&paths.health_file, &doc) {
        warn!(error = %err, "could not write health file");
    }
}

/// Drains the request directory into the engine (see [`drain_request_dir`] for
/// the consumption rules), discarding any request that has aged past the
/// staleness window before it reaches the engine.
fn process_requests(paths: &DaemonPaths, handle: &EngineHandle, watch_names: &[String]) {
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
        apply_request(request, handle, watch_names);
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

/// Applies one parsed request: a snapshot injects a manual trigger (for the
/// named watch, or every watch when unnamed); a sync is logged and dropped
/// pending VRD-19.
fn apply_request(request: Request, handle: &EngineHandle, watch_names: &[String]) {
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
                for watch in watch_names {
                    handle.trigger(watch);
                }
                info!(
                    count = watch_names.len(),
                    "manual snapshot requested for all watches"
                );
            }
        },
        RequestKind::Sync => {
            info!("sync requested but not yet implemented (VRD-19); dropping");
        }
    }
}

/// Reads a file's modification time, or `None` if it is missing or unreadable.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// A one-cycle debounce over a file's mtime: it reports a change only once the
/// mtime has held steady at a new value across two consecutive polls, so a burst
/// of writes (an editor saving repeatedly) collapses into a single settled
/// signal rather than a rebuild per write.
struct MtimeDebounce {
    /// The last mtime we reported as settled.
    stable: Option<SystemTime>,
    /// A new mtime seen once, awaiting a second confirming poll.
    pending: Option<SystemTime>,
}

impl MtimeDebounce {
    /// Starts from an initial (settled) mtime.
    fn new(initial: Option<SystemTime>) -> MtimeDebounce {
        MtimeDebounce {
            stable: initial,
            pending: None,
        }
    }

    /// Feeds the current mtime; returns `true` exactly when a settled change is
    /// detected (a new value seen on two consecutive polls).
    fn poll(&mut self, current: Option<SystemTime>) -> bool {
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

    // --- mtime debounce ------------------------------------------------------

    #[test]
    fn mtime_debounce_needs_two_stable_polls_to_fire() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(1);
        let mut d = MtimeDebounce::new(Some(t0));

        // No change: never fires.
        assert!(!d.poll(Some(t0)));
        // First sighting of the new value: still debouncing.
        assert!(!d.poll(Some(t1)));
        // Second, confirming poll at the same value: fires once.
        assert!(d.poll(Some(t1)));
        // Settled; no re-fire without a further change.
        assert!(!d.poll(Some(t1)));
    }

    #[test]
    fn mtime_debounce_cancels_a_reverted_change() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(1);
        let mut d = MtimeDebounce::new(Some(t0));

        // A flicker to t1 then back to t0 before confirming: no fire.
        assert!(!d.poll(Some(t1)));
        assert!(!d.poll(Some(t0)));
        assert!(!d.poll(Some(t0)));
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
        for watch in ["auto", "manual"] {
            let journal_path = Journal::in_dir(&paths.journal_dir, watch);
            let len = std::fs::metadata(journal_path.path())
                .map(|meta| meta.len())
                .unwrap_or(0);
            assert_eq!(
                len, 0,
                "watch {watch:?} journal must hold no dangling begin after a clean shutdown"
            );
        }
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
}
