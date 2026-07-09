//! The snapshot engine: the coordinator that turns watcher and scheduler
//! signals into version-control snapshots, one watch at a time.
//!
//! [`Engine`] is the embeddable SDK entry point (the spec's §2a contract). A
//! host builds it from validated [`WatchSpec`] values, subscribes to its
//! [`EventBus`](crate::EventBus), and starts it:
//!
//! ```no_run
//! use std::time::Duration;
//! use vard_core::{Engine, Event, TriggerMode, WatchSpec};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let engine = Engine::builder()
//!     .watch(
//!         WatchSpec::builder("vault", "/home/u/vault")
//!             .trigger(TriggerMode::Both)
//!             .interval(Duration::from_secs(15 * 60))
//!             .quiesce(Duration::from_secs(10))
//!             .sync(false)
//!             .build()?,
//!     )
//!     .build()?;
//!
//! let mut events = engine.subscribe(); // same bus the hooks use
//! engine.start().await?;
//!
//! while let Ok(ev) = events.recv().await {
//!     match ev {
//!         Event::SnapshotCompleted { watch, snapshot, .. } => { let _ = (watch, snapshot); }
//!         Event::SyncConflict { watch, .. } => { let _ = watch; }
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # One worker per watch
//!
//! [`start`](Engine::start) arms the [`Watcher`] and [`Scheduler`] once for the
//! whole engine (each exposes a single multiplexed receiver) and spawns exactly
//! one **worker** task per watch. Two dispatcher tasks fan the shared
//! [`WatcherSignal`]/[`SchedulerSignal`] streams out to the right worker by
//! watch name. Watches therefore run concurrently, while every operation within
//! a watch is strictly serialized — a worker is a single task, so it is doing at
//! most one thing at a time.
//!
//! # The per-watch worker
//!
//! A worker keeps a single **coalesced pending flag**, not a queue: many
//! triggers collapse into one due snapshot, and the flag records only the
//! most-intentional [`Trigger`] that contributed (manual beats the protective
//! pre-restore/pre-sync triggers, which beat filesystem events, which beat the
//! interval timer — see [`trigger_priority`]). When a snapshot comes due the
//! worker:
//!
//! 1. Acquires the watch's [`MuteGuard`](crate::MuteGuard) so vard's own writes
//!    do not feed back as fresh activity (self-suppression), and holds it across
//!    the whole operation.
//! 2. Re-checks [`is_safe_state`](VcsBackend::is_safe_state). An unsafe repo
//!    pauses the watch and arms a bounded re-poll that auto-resumes it once the
//!    repo returns to safe — this works even for an `events`-only watch that
//!    will receive no further signals, because the re-poll is a timer, not a
//!    signal.
//! 3. On a safe repo, calls [`snapshot`](VcsBackend::snapshot), retrying a
//!    contended index lock with exponential backoff before requeueing (it never
//!    deletes a foreign lock), and emits [`Event::SnapshotCompleted`] /
//!    [`Event::SnapshotFailed`] as appropriate.
//! 4. Runs a **post-op dirtiness re-check**: because the sweep is a total
//!    `add -A`, a clean tree yields nothing on a second pass, so re-snapshotting
//!    converges — but a real write that landed during the muted window is caught
//!    and snapshotted as a follow-up, never lost.
//!
//! Trouble from either signal source ([`WatcherSignal::Trouble`],
//! [`SchedulerSignal::Trouble`]) moves the watch to
//! [`WatchState::Attention`](crate::WatchState) and is surfaced on the bus, so
//! nothing dies silently.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::config::{TriggerMode, WatchSpec};
use crate::event::{Event, EventBus, EventReceiver, Trigger, WatchState};
use crate::scheduler::{ScheduleHandle, Scheduler, SchedulerRx, SchedulerSignal};
use crate::vcs::git::GitBackend;
use crate::vcs::{SafeState, SnapshotOutcome, SnapshotRequest, UnsafeReason, VcsBackend, VcsError};
use crate::watcher::{MuteGuard, WatchHandle, Watcher, WatcherRx, WatcherSignal};

/// Default number of attempts made against a contended index lock before the
/// snapshot is requeued (spec §3: 5 attempts over ~30 s).
pub const DEFAULT_LOCK_RETRY_ATTEMPTS: u32 = 5;

/// Default base delay for lock-retry exponential backoff. With
/// [`DEFAULT_LOCK_RETRY_ATTEMPTS`] the backoff runs `2s, 4s, 8s, 16s` between
/// the five attempts — ~30 s total, matching spec §3.
pub const DEFAULT_LOCK_RETRY_BASE: Duration = Duration::from_secs(2);

/// Default cadence at which a watch paused for an unsafe repository re-polls
/// [`is_safe_state`](VcsBackend::is_safe_state) to auto-resume.
pub const DEFAULT_UNSAFE_REPOLL_INTERVAL: Duration = Duration::from_secs(30);

/// Default cap on consecutive unsafe re-polls before a paused watch stops
/// polling and waits for fresh activity. At [`DEFAULT_UNSAFE_REPOLL_INTERVAL`]
/// this is four hours of background polling — bounded, so an abandoned unsafe
/// repository does not poll forever, yet long enough that a genuine mid-op
/// pause always auto-resumes. The counter resets whenever new activity arrives.
pub const DEFAULT_UNSAFE_REPOLL_MAX_ATTEMPTS: u32 = 480;

/// Tunable timing policy for a worker's retry and re-poll loops.
///
/// The defaults are the `DEFAULT_*` constants in this module; the
/// [`EngineBuilder`] exposes setters so a host (or a deterministic test under
/// paused time) can override them.
#[derive(Clone, Copy, Debug)]
struct EngineConfig {
    lock_retry_attempts: u32,
    lock_retry_base: Duration,
    unsafe_repoll_interval: Duration,
    unsafe_repoll_max_attempts: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            lock_retry_attempts: DEFAULT_LOCK_RETRY_ATTEMPTS,
            lock_retry_base: DEFAULT_LOCK_RETRY_BASE,
            unsafe_repoll_interval: DEFAULT_UNSAFE_REPOLL_INTERVAL,
            unsafe_repoll_max_attempts: DEFAULT_UNSAFE_REPOLL_MAX_ATTEMPTS,
        }
    }
}

/// A shared, thread-safe version-control backend for one watch.
///
/// The trait is synchronous and its methods block (they shell out to `git`), so
/// the worker calls them from [`spawn_blocking`](tokio::task::spawn_blocking);
/// that requires the backend be `Send + Sync + 'static`, which an [`Arc`] of a
/// `dyn` backend satisfies. Sharing (rather than owning) lets the same value be
/// cloned into each blocking call while the worker keeps serializing them.
pub type SharedBackend = Arc<dyn VcsBackend + Send + Sync>;

/// Why a snapshot is due: the winning [`Trigger`] and any user-supplied text.
///
/// A worker keeps at most one of these pending at a time; [`coalesce`] merges a
/// new trigger into the current one by [`trigger_priority`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct Provenance {
    trigger: Trigger,
    user_text: Option<String>,
}

impl Provenance {
    /// A filesystem-activity provenance (from [`WatcherSignal::Activity`]).
    fn event() -> Self {
        Self {
            trigger: Trigger::Event,
            user_text: None,
        }
    }

    /// An interval-timer provenance (from [`SchedulerSignal::Tick`]).
    fn interval() -> Self {
        Self {
            trigger: Trigger::Interval,
            user_text: None,
        }
    }
}

/// Priority of a trigger when several coalesce: the most *intentional* wins.
///
/// `manual` (4) beats the protective `pre-restore`/`pre-sync` (3), which beat a
/// filesystem `event` (2), which beats the background `interval` (1). This is
/// the rule behind "multiple ticks and activity collapse into one pending
/// snapshot, tagged with the most deliberate reason".
fn trigger_priority(trigger: Trigger) -> u8 {
    match trigger {
        Trigger::Manual => 4,
        Trigger::PreRestore | Trigger::PreSync => 3,
        Trigger::Event => 2,
        Trigger::Interval => 1,
    }
}

/// Merges an incoming provenance into the currently pending one, keeping the
/// higher-priority trigger (and its user text). Ties keep the existing one, so
/// the earliest deliberate intent is preserved.
fn coalesce(existing: Option<Provenance>, incoming: Provenance) -> Provenance {
    match existing {
        None => incoming,
        Some(current) => {
            if trigger_priority(incoming.trigger) > trigger_priority(current.trigger) {
                incoming
            } else {
                current
            }
        }
    }
}

/// One item routed to a worker: a snapshot trigger or a trouble report.
#[derive(Clone, Debug)]
enum WatchInput {
    /// A snapshot is due for the given reason.
    Trigger(Provenance),
    /// A signal source reported trouble; the detail is surfaced on the bus.
    Trouble {
        /// Human-readable description of the condition.
        detail: String,
    },
}

/// The source of self-suppression mutes for a worker.
///
/// A watch that arms the filesystem watcher mutes it around vard's own writes;
/// an `interval`-only watch has no watcher and nothing to suppress.
enum MuteSource {
    /// The watch's live filesystem handle; muting it drops self-inflicted
    /// events. Owning the handle also keeps the watch armed.
    Watch(WatchHandle),
    /// No watcher armed: nothing to mute.
    Silent,
    /// Test double: increments a shared counter instead of a real watch, so a
    /// test can observe that the worker is muted across its operation.
    #[cfg(test)]
    Counter(Arc<std::sync::atomic::AtomicUsize>),
}

impl MuteSource {
    /// Acquires a self-suppression mute, held until the returned guard drops.
    fn acquire(&self) -> MuteHold {
        match self {
            MuteSource::Watch(handle) => MuteHold {
                _guard: Some(handle.mute()),
                #[cfg(test)]
                counter: None,
            },
            MuteSource::Silent => MuteHold {
                _guard: None,
                #[cfg(test)]
                counter: None,
            },
            #[cfg(test)]
            MuteSource::Counter(counter) => {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                MuteHold {
                    _guard: None,
                    counter: Some(Arc::clone(counter)),
                }
            }
        }
    }
}

/// RAII hold produced by [`MuteSource::acquire`]. Dropping it releases the mute.
struct MuteHold {
    // A real `MuteGuard` releases its watch when this field drops; no manual
    // work needed for the non-test paths.
    _guard: Option<MuteGuard>,
    #[cfg(test)]
    counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

#[cfg(test)]
impl Drop for MuteHold {
    fn drop(&mut self) {
        if let Some(counter) = &self.counter {
            counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// The outcome of one attempt to bring the tree to a committed snapshot.
enum PassResult {
    /// A snapshot was committed.
    Committed(SnapshotOutcome),
    /// The sweep found nothing to commit (clean tree): a no-op.
    Clean,
    /// The repository became unsafe between the guard check and the commit.
    Unsafe(UnsafeReason),
    /// The index lock stayed contended through every retry: requeue and retry
    /// on the next trigger.
    StillLocked,
    /// The backend failed for some other reason.
    Failed(VcsError),
}

/// One per-watch worker: the serialized snapshot loop for a single watch.
struct Worker {
    name: String,
    backend: SharedBackend,
    mute: MuteSource,
    // Kept only to hold the interval schedule armed for this worker's lifetime.
    _schedule: Option<ScheduleHandle>,
    bus: EventBus,
    cfg: EngineConfig,

    /// The coalesced due snapshot, if any.
    pending: Option<Provenance>,
    /// The last reported lifecycle state (drives change-only event emission).
    state: WatchState,
    /// Whether the watch is paused because the repository is unsafe to commit.
    unsafe_paused: bool,
    /// Consecutive unsafe re-polls since the last activity (bounds polling).
    repoll_attempts: u32,
    /// Whether the re-poll budget is spent; the worker then waits for activity.
    repoll_exhausted: bool,
}

impl Worker {
    /// Runs the worker until its input channel closes (every signal source for
    /// this watch was dropped, i.e. the engine is shutting down).
    ///
    /// The loop blocks for the next input, except while paused for an unsafe
    /// repository, when it also wakes on the re-poll deadline so the watch can
    /// auto-resume without any further signal.
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<WatchInput>) {
        loop {
            let waiting_on_repoll = self.unsafe_paused && !self.repoll_exhausted;
            let input = if waiting_on_repoll {
                match timeout(self.cfg.unsafe_repoll_interval, rx.recv()).await {
                    Ok(received) => received,
                    Err(_elapsed) => {
                        self.repoll().await;
                        continue;
                    }
                }
            } else {
                rx.recv().await
            };

            let Some(input) = input else { break };
            self.apply(input);
            // Drain everything else already queued so a burst that arrived while
            // a snapshot was in flight collapses into one follow-up.
            while let Ok(more) = rx.try_recv() {
                self.apply(more);
            }

            if self.pending.is_some() && !self.unsafe_paused {
                self.run_pass().await;
            }
        }
    }

    /// Folds one input into the worker's state.
    fn apply(&mut self, input: WatchInput) {
        match input {
            WatchInput::Trigger(prov) => {
                // Fresh activity gives a paused watch a new chance to resume.
                if self.unsafe_paused {
                    self.repoll_attempts = 0;
                    self.repoll_exhausted = false;
                }
                self.pending = Some(coalesce(self.pending.take(), prov));
            }
            WatchInput::Trouble { detail } => {
                self.set_state(WatchState::Attention, Some(detail));
            }
        }
    }

    /// Drives the pending snapshot to a committed (or converged) state, holding
    /// the self-suppression mute for the whole sequence.
    async fn run_pass(&mut self) {
        let _mute = self.mute.acquire();

        while let Some(prov) = self.pending.take() {
            match check_safe(Arc::clone(&self.backend)).await {
                Ok(SafeState::Safe) => {}
                Ok(SafeState::Unsafe(reason)) => {
                    self.pending = Some(prov);
                    self.enter_unsafe(reason);
                    return;
                }
                Err(err) => {
                    // The safe-state probe itself failed; do not commit into an
                    // unknown state. Surface it and give up the pass.
                    self.emit_failed(prov.trigger, &err);
                    return;
                }
            }

            match snapshot_with_retry(Arc::clone(&self.backend), self.cfg, &prov).await {
                PassResult::Committed(outcome) => {
                    self.emit_completed(prov.trigger, &outcome);
                    // Post-op dirtiness re-check: sweep again under the same
                    // mute. A clean tree returns `Clean` and the loop converges;
                    // a write that landed during the muted window is caught here
                    // and snapshotted as a follow-up.
                    self.pending = Some(Provenance::event());
                }
                PassResult::Clean => {
                    // Nothing to commit: converged.
                }
                PassResult::Unsafe(reason) => {
                    self.pending = Some(prov);
                    self.enter_unsafe(reason);
                    return;
                }
                PassResult::StillLocked => {
                    // Never delete a foreign lock: requeue and try again on the
                    // next trigger.
                    self.pending = Some(coalesce(self.pending.take(), prov));
                    return;
                }
                PassResult::Failed(err) => {
                    self.emit_failed(prov.trigger, &err);
                    // Do not requeue a hard failure; converge this pass.
                }
            }
        }
    }

    /// Re-checks the repository while paused, resuming the watch when it becomes
    /// safe. Bounds the number of consecutive polls.
    async fn repoll(&mut self) {
        self.repoll_attempts += 1;
        match check_safe(Arc::clone(&self.backend)).await {
            Ok(SafeState::Safe) => {
                self.unsafe_paused = false;
                self.repoll_attempts = 0;
                self.repoll_exhausted = false;
                self.set_state(WatchState::Ok, Some("repository returned to a safe state".into()));
                if self.pending.is_some() {
                    self.run_pass().await;
                }
            }
            Ok(SafeState::Unsafe(_)) | Err(_) => {
                if self.repoll_attempts >= self.cfg.unsafe_repoll_max_attempts {
                    self.repoll_exhausted = true;
                }
            }
        }
    }

    /// Enters the unsafe-paused state, arming the re-poll loop.
    fn enter_unsafe(&mut self, reason: UnsafeReason) {
        self.unsafe_paused = true;
        self.repoll_attempts = 0;
        self.repoll_exhausted = false;
        self.set_state(WatchState::Paused, Some(reason.to_string()));
    }

    /// Emits [`Event::SnapshotCompleted`] for a committed snapshot.
    fn emit_completed(&self, trigger: Trigger, outcome: &SnapshotOutcome) {
        self.bus.emit(Event::SnapshotCompleted {
            watch: self.name.clone(),
            snapshot: outcome.id.to_string(),
            files_changed: outcome.summary.total(),
            trigger,
        });
    }

    /// Emits [`Event::SnapshotFailed`] for a failed attempt.
    fn emit_failed(&self, trigger: Trigger, err: &VcsError) {
        self.bus.emit(Event::SnapshotFailed {
            watch: self.name.clone(),
            trigger,
            error: err.to_string(),
        });
    }

    /// Transitions the watch's reported state, emitting
    /// [`Event::WatchStateChanged`] only on an actual change.
    fn set_state(&mut self, to: WatchState, reason: Option<String>) {
        if self.state == to {
            return;
        }
        let from = self.state;
        self.state = to;
        self.bus.emit(Event::WatchStateChanged {
            watch: self.name.clone(),
            from,
            to,
            reason,
        });
    }
}

/// Runs [`is_safe_state`](VcsBackend::is_safe_state) off the async runtime.
///
/// Takes the backend by value (a cheap [`Arc`] clone) rather than borrowing the
/// worker, so the returned future stays `Send` — the worker holds a
/// [`WatchHandle`], which is `Send` but not `Sync`.
async fn check_safe(backend: SharedBackend) -> Result<SafeState, VcsError> {
    tokio::task::spawn_blocking(move || backend.is_safe_state())
        .await
        .expect("is_safe_state task panicked")
}

/// Runs [`snapshot`](VcsBackend::snapshot) off the async runtime. See
/// [`check_safe`] for why it takes the backend by value.
async fn call_snapshot(
    backend: SharedBackend,
    req: SnapshotRequest,
) -> Result<Option<SnapshotOutcome>, VcsError> {
    tokio::task::spawn_blocking(move || backend.snapshot(&req))
        .await
        .expect("snapshot task panicked")
}

/// Calls [`snapshot`](VcsBackend::snapshot), retrying a contended lock with
/// exponential backoff up to [`EngineConfig::lock_retry_attempts`]. A free
/// function (not a method) so the future stays `Send` — see [`check_safe`].
async fn snapshot_with_retry(
    backend: SharedBackend,
    cfg: EngineConfig,
    prov: &Provenance,
) -> PassResult {
    let req = SnapshotRequest {
        trigger: prov.trigger,
        user_text: prov.user_text.clone(),
        extra_trailers: Vec::new(),
    };

    for attempt in 1..=cfg.lock_retry_attempts {
        match call_snapshot(Arc::clone(&backend), req.clone()).await {
            Ok(Some(outcome)) => return PassResult::Committed(outcome),
            Ok(None) => return PassResult::Clean,
            Err(VcsError::UnsafeState(reason)) => return PassResult::Unsafe(reason),
            Err(VcsError::LockContended { .. }) => {
                if attempt < cfg.lock_retry_attempts {
                    tokio::time::sleep(backoff(cfg, attempt)).await;
                    continue;
                }
                return PassResult::StillLocked;
            }
            Err(other) => return PassResult::Failed(other),
        }
    }
    // `lock_retry_attempts` is always >= 1, so the loop always returns.
    PassResult::StillLocked
}

/// The backoff before the retry that follows `attempt` (1-based):
/// `base * 2^(attempt - 1)`.
fn backoff(cfg: EngineConfig, attempt: u32) -> Duration {
    cfg.lock_retry_base * 2u32.saturating_pow(attempt - 1)
}

/// The embeddable snapshot engine.
///
/// Build one with [`Engine::builder`], [`subscribe`](Engine::subscribe) to its
/// event stream, and [`start`](Engine::start) it. See the [module docs](self)
/// for the architecture and the §2a SDK example.
pub struct Engine {
    bus: EventBus,
    watches: Vec<ConfiguredWatch>,
    cfg: EngineConfig,
}

/// One watch paired with the backend it snapshots through.
struct ConfiguredWatch {
    spec: WatchSpec,
    backend: SharedBackend,
}

impl Engine {
    /// Starts building an engine.
    pub fn builder() -> EngineBuilder {
        EngineBuilder::new()
    }

    /// Subscribes to the engine's event bus.
    ///
    /// Safe to call before or after [`start`](Engine::start); a subscriber sees
    /// every event emitted after it subscribes (see [`EventBus`]).
    pub fn subscribe(&self) -> EventReceiver {
        self.bus.subscribe()
    }

    /// Arms every watch and spawns its worker, then returns.
    ///
    /// Consumes the engine: its watches, handles, and bus move into the spawned
    /// tasks, which run until the runtime stops. The returned future resolves
    /// once all watches are armed and [`Event::DaemonStarted`] has been emitted;
    /// the workers then run in the background. A host keeps the process alive by
    /// holding a subscriber (or its own runtime) — this call does not block.
    ///
    /// # Errors
    ///
    /// Fails if any watch cannot arm its filesystem watcher
    /// ([`EngineError::Watcher`]) or interval schedule
    /// ([`EngineError::Scheduler`]). Watchers are armed before any worker is
    /// spawned, so a failure leaves nothing running.
    ///
    /// # Runtime
    ///
    /// Must be called from within a Tokio runtime.
    pub async fn start(self) -> Result<(), EngineError> {
        let Engine { bus, watches, cfg } = self;

        let (watcher, watcher_rx) = Watcher::new();
        let (scheduler, scheduler_rx) = Scheduler::new();

        let mut prepared: Vec<(Worker, mpsc::UnboundedReceiver<WatchInput>)> = Vec::new();
        let mut watcher_routes: HashMap<String, mpsc::UnboundedSender<WatchInput>> = HashMap::new();
        let mut scheduler_routes: HashMap<String, mpsc::UnboundedSender<WatchInput>> =
            HashMap::new();

        for cw in watches {
            let name = cw.spec.name().to_string();
            let (tx, rx) = mpsc::unbounded_channel();
            let mode = cw.spec.trigger();

            let mute = if matches!(mode, TriggerMode::Events | TriggerMode::Both) {
                let handle = watcher.arm(&cw.spec).map_err(EngineError::Watcher)?;
                watcher_routes.insert(name.clone(), tx.clone());
                MuteSource::Watch(handle)
            } else {
                MuteSource::Silent
            };

            let schedule = if matches!(mode, TriggerMode::Interval | TriggerMode::Both) {
                let handle = scheduler
                    .arm(name.clone(), cw.spec.interval())
                    .map_err(EngineError::Scheduler)?;
                scheduler_routes.insert(name.clone(), tx.clone());
                Some(handle)
            } else {
                None
            };

            let worker = Worker {
                name,
                backend: cw.backend,
                mute,
                _schedule: schedule,
                bus: bus.clone(),
                cfg,
                pending: None,
                state: WatchState::Ok,
                unsafe_paused: false,
                repoll_attempts: 0,
                repoll_exhausted: false,
            };
            prepared.push((worker, rx));
        }

        for (worker, rx) in prepared {
            tokio::spawn(worker.run(rx));
        }
        tokio::spawn(dispatch_watcher(watcher_rx, watcher_routes));
        tokio::spawn(dispatch_scheduler(scheduler_rx, scheduler_routes));

        bus.emit(Event::DaemonStarted);
        Ok(())
    }
}

/// Fans the shared watcher stream out to per-watch workers by name.
async fn dispatch_watcher(
    mut rx: WatcherRx,
    routes: HashMap<String, mpsc::UnboundedSender<WatchInput>>,
) {
    while let Some(signal) = rx.recv().await {
        let (watch, input) = match signal {
            WatcherSignal::Activity { watch, .. } => {
                (watch, WatchInput::Trigger(Provenance::event()))
            }
            WatcherSignal::Trouble { watch, detail } => (watch, WatchInput::Trouble { detail }),
        };
        if let Some(tx) = routes.get(&watch) {
            let _ = tx.send(input);
        }
    }
}

/// Fans the shared scheduler stream out to per-watch workers by name.
async fn dispatch_scheduler(
    mut rx: SchedulerRx,
    routes: HashMap<String, mpsc::UnboundedSender<WatchInput>>,
) {
    while let Some(signal) = rx.recv().await {
        let (watch, input) = match signal {
            SchedulerSignal::Tick { watch } => {
                (watch, WatchInput::Trigger(Provenance::interval()))
            }
            SchedulerSignal::Trouble { watch, detail } => (watch, WatchInput::Trouble { detail }),
        };
        if let Some(tx) = routes.get(&watch) {
            let _ = tx.send(input);
        }
    }
}

/// A watch queued into an [`EngineBuilder`], with how its backend is obtained.
enum PendingWatch {
    /// Build a [`GitBackend`] from the spec at [`build`](EngineBuilder::build).
    Git(WatchSpec),
    /// Use the caller-supplied backend.
    Backend(WatchSpec, SharedBackend),
}

impl PendingWatch {
    /// The watch's stable name.
    fn name(&self) -> &str {
        match self {
            PendingWatch::Git(spec) | PendingWatch::Backend(spec, _) => spec.name(),
        }
    }
}

/// A builder for [`Engine`].
///
/// Add watches with [`watch`](EngineBuilder::watch) (each backed by a git
/// repository at the watch path) or [`watch_with_backend`](EngineBuilder::watch_with_backend)
/// (any [`VcsBackend`], for embedders and tests). The retry and re-poll timing
/// setters exist mainly for deterministic tests; their defaults follow spec §3.
pub struct EngineBuilder {
    watches: Vec<PendingWatch>,
    capacity: usize,
    cfg: EngineConfig,
}

impl EngineBuilder {
    /// A builder with no watches and default event-bus capacity and timings.
    fn new() -> Self {
        Self {
            watches: Vec::new(),
            capacity: crate::event::DEFAULT_CAPACITY,
            cfg: EngineConfig::default(),
        }
    }

    /// Adds a watch backed by the git repository rooted at its path.
    ///
    /// The backend is opened at [`build`](Self::build): the branch comes from
    /// [`WatchSpec::branch`], or is adopted from the repository's current branch
    /// when unset (spec §3 branch policy).
    pub fn watch(mut self, spec: WatchSpec) -> Self {
        self.watches.push(PendingWatch::Git(spec));
        self
    }

    /// Adds a watch snapshotted through a caller-supplied backend.
    ///
    /// Lets an embedder plug in an alternate [`VcsBackend`] and lets tests drive
    /// the engine with a fake. The backend must be `Send + Sync` because the
    /// worker calls it from a blocking task (see [`SharedBackend`]).
    pub fn watch_with_backend(mut self, spec: WatchSpec, backend: SharedBackend) -> Self {
        self.watches.push(PendingWatch::Backend(spec, backend));
        self
    }

    /// Sets the per-subscriber event-bus capacity (default
    /// [`DEFAULT_CAPACITY`](crate::DEFAULT_CAPACITY)).
    pub fn event_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Sets how many attempts a contended index lock gets before requeueing.
    pub fn lock_retry_attempts(mut self, attempts: u32) -> Self {
        self.cfg.lock_retry_attempts = attempts;
        self
    }

    /// Sets the base delay for lock-retry exponential backoff.
    pub fn lock_retry_base(mut self, base: Duration) -> Self {
        self.cfg.lock_retry_base = base;
        self
    }

    /// Sets how often a watch paused for an unsafe repository re-polls to resume.
    pub fn unsafe_repoll_interval(mut self, interval: Duration) -> Self {
        self.cfg.unsafe_repoll_interval = interval;
        self
    }

    /// Sets the cap on consecutive unsafe re-polls before waiting for activity.
    pub fn unsafe_repoll_max_attempts(mut self, attempts: u32) -> Self {
        self.cfg.unsafe_repoll_max_attempts = attempts;
        self
    }

    /// Validates the watches and builds the engine.
    ///
    /// # Errors
    ///
    /// - [`EngineError::DuplicateWatch`] if two watches share a name (names are
    ///   the routing key and must be unique).
    /// - [`EngineError::Backend`] if a [`watch`](Self::watch) repository cannot
    ///   be opened.
    pub fn build(self) -> Result<Engine, EngineError> {
        let mut seen: HashMap<&str, ()> = HashMap::new();
        for pending in &self.watches {
            if seen.insert(pending.name(), ()).is_some() {
                return Err(EngineError::DuplicateWatch {
                    name: pending.name().to_string(),
                });
            }
        }

        let mut watches = Vec::with_capacity(self.watches.len());
        for pending in self.watches {
            let cw = match pending {
                PendingWatch::Backend(spec, backend) => ConfiguredWatch { spec, backend },
                PendingWatch::Git(spec) => {
                    let backend = open_git_backend(&spec).map_err(|source| EngineError::Backend {
                        watch: spec.name().to_string(),
                        source,
                    })?;
                    ConfiguredWatch {
                        spec,
                        backend: Arc::new(backend),
                    }
                }
            };
            watches.push(cw);
        }

        Ok(Engine {
            bus: EventBus::new(self.capacity),
            watches,
            cfg: self.cfg,
        })
    }
}

/// Opens the git backend for a [`watch`](EngineBuilder::watch) spec, applying
/// the branch policy: use the configured branch, else adopt the repository's
/// current branch.
fn open_git_backend(spec: &WatchSpec) -> Result<GitBackend, VcsError> {
    let branch = match spec.branch() {
        Some(branch) => branch.to_string(),
        None => GitBackend::detect(spec.path())?
            .ok_or(VcsError::NotARepo)?
            .branch()
            .to_string(),
    };
    GitBackend::open(spec.path(), &branch, spec.remote())
}

/// Everything that can go wrong building or starting an [`Engine`].
#[derive(Debug)]
#[non_exhaustive]
pub enum EngineError {
    /// Two watches shared a name; names are the routing key and must be unique.
    DuplicateWatch {
        /// The duplicated watch name.
        name: String,
    },
    /// A [`watch`](EngineBuilder::watch) repository could not be opened.
    Backend {
        /// The watch whose backend failed to open.
        watch: String,
        /// The underlying VCS error.
        source: VcsError,
    },
    /// A watch's filesystem watcher could not be armed.
    Watcher(crate::watcher::WatcherError),
    /// A watch's interval schedule could not be armed.
    Scheduler(crate::scheduler::SchedulerError),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::DuplicateWatch { name } => {
                write!(f, "duplicate watch name {name:?}; watch names must be unique")
            }
            EngineError::Backend { watch, source } => {
                write!(f, "watch {watch:?}: could not open backend: {source}")
            }
            EngineError::Watcher(e) => write!(f, "could not arm watcher: {e}"),
            EngineError::Scheduler(e) => write!(f, "could not arm scheduler: {e}"),
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            EngineError::Backend { source, .. } => Some(source),
            EngineError::Watcher(e) => Some(e),
            EngineError::Scheduler(e) => Some(e),
            EngineError::DuplicateWatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::event::{EventReceiver, TryRecvError};
    use crate::vcs::{ChangeSummary, SnapshotId};

    use super::*;

    /// A scripted result for one [`VcsBackend::snapshot`] call.
    #[derive(Clone)]
    enum Scripted {
        /// Return a committed snapshot with this many changed files.
        Commit(usize),
        /// Return a clean (no-op) sweep.
        Clean,
        /// Return a contended lock.
        Lock,
        /// Return a hard command failure.
        Fail,
    }

    /// A deterministic in-memory [`VcsBackend`] for driving worker scenarios.
    struct FakeBackend {
        inner: Mutex<FakeInner>,
    }

    struct FakeInner {
        safe: SafeState,
        snapshots: VecDeque<Scripted>,
        snapshot_calls: usize,
        safe_calls: usize,
        /// The 1-based snapshot call that should block on `gate_rx`.
        gate_on_call: Option<usize>,
        gate_rx: Option<std::sync::mpsc::Receiver<()>>,
    }

    impl FakeBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(FakeInner {
                    safe: SafeState::Safe,
                    snapshots: VecDeque::new(),
                    snapshot_calls: 0,
                    safe_calls: 0,
                    gate_on_call: None,
                    gate_rx: None,
                }),
            })
        }

        fn script(&self, results: impl IntoIterator<Item = Scripted>) {
            self.inner.lock().unwrap().snapshots.extend(results);
        }

        fn set_safe(&self, safe: SafeState) {
            self.inner.lock().unwrap().safe = safe;
        }

        /// Arms a blocking gate on the given 1-based snapshot call, returning the
        /// sender that releases it.
        fn gate(&self, call: usize) -> std::sync::mpsc::Sender<()> {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut inner = self.inner.lock().unwrap();
            inner.gate_on_call = Some(call);
            inner.gate_rx = Some(rx);
            tx
        }

        fn snapshot_calls(&self) -> usize {
            self.inner.lock().unwrap().snapshot_calls
        }

        fn safe_calls(&self) -> usize {
            self.inner.lock().unwrap().safe_calls
        }
    }

    impl VcsBackend for FakeBackend {
        fn is_safe_state(&self) -> Result<SafeState, VcsError> {
            let mut inner = self.inner.lock().unwrap();
            inner.safe_calls += 1;
            Ok(inner.safe.clone())
        }

        fn snapshot(&self, _req: &SnapshotRequest) -> Result<Option<SnapshotOutcome>, VcsError> {
            let (result, gate) = {
                let mut inner = self.inner.lock().unwrap();
                inner.snapshot_calls += 1;
                let call = inner.snapshot_calls;
                let gate = if inner.gate_on_call == Some(call) {
                    inner.gate_rx.take()
                } else {
                    None
                };
                let result = inner.snapshots.pop_front().unwrap_or(Scripted::Clean);
                (result, gate)
            };
            // Block outside the lock so the test can inspect/mutate meanwhile.
            if let Some(rx) = gate {
                let _ = rx.recv();
            }
            match result {
                Scripted::Commit(files) => Ok(Some(SnapshotOutcome {
                    id: SnapshotId::new("deadbeef"),
                    summary: ChangeSummary {
                        changed: files,
                        added: 0,
                        deleted: 0,
                        notable: Vec::new(),
                    },
                })),
                Scripted::Clean => Ok(None),
                Scripted::Lock => Err(VcsError::LockContended {
                    op: "commit".into(),
                }),
                Scripted::Fail => Err(VcsError::CommandFailed {
                    op: "commit".into(),
                    status: Some(1),
                    stderr: "boom".into(),
                }),
            }
        }

        fn log(
            &self,
            _filter: &crate::vcs::LogFilter,
        ) -> Result<Vec<crate::vcs::Snapshot>, VcsError> {
            Ok(Vec::new())
        }

        fn diff(
            &self,
            _from: &crate::vcs::VcsRef,
            _to: Option<&crate::vcs::VcsRef>,
        ) -> Result<String, VcsError> {
            Ok(String::new())
        }

        fn restore(&self, _target: &crate::vcs::RestoreTarget) -> Result<(), VcsError> {
            unimplemented!("restore is out of scope for the snapshot engine")
        }

        fn fetch(&self) -> Result<crate::vcs::RemoteState, VcsError> {
            unimplemented!("fetch is out of scope for the snapshot engine")
        }

        fn reconcile(&self) -> Result<crate::vcs::ReconcileOutcome, VcsError> {
            unimplemented!("reconcile is out of scope for the snapshot engine")
        }

        fn push(&self) -> Result<crate::vcs::PushOutcome, VcsError> {
            unimplemented!("push is out of scope for the snapshot engine")
        }
    }

    /// Spawns a worker driven directly by an injected input channel and a fake
    /// backend, bypassing the real watcher/scheduler for determinism. Returns
    /// the input sender, an event subscriber, and the shared mute counter.
    fn spawn_worker(
        backend: Arc<FakeBackend>,
        cfg: EngineConfig,
    ) -> (
        mpsc::UnboundedSender<WatchInput>,
        EventReceiver,
        Arc<AtomicUsize>,
    ) {
        let bus = EventBus::default();
        let events = bus.subscribe();
        let counter = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = Worker {
            name: "w".to_string(),
            backend: backend as SharedBackend,
            mute: MuteSource::Counter(Arc::clone(&counter)),
            _schedule: None,
            bus,
            cfg,
            pending: None,
            state: WatchState::Ok,
            unsafe_paused: false,
            repoll_attempts: 0,
            repoll_exhausted: false,
        };
        tokio::spawn(worker.run(rx));
        (tx, events, counter)
    }

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            lock_retry_attempts: 5,
            lock_retry_base: Duration::from_secs(2),
            unsafe_repoll_interval: Duration::from_secs(30),
            unsafe_repoll_max_attempts: 480,
        }
    }

    /// Yields enough to let spawned tasks (and their blocking calls) progress
    /// without advancing the paused clock.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// Settles until the backend has seen at least `n` snapshot calls, so a
    /// count assertion does not race the converging re-check's blocking call.
    async fn wait_snapshot_calls(backend: &FakeBackend, n: usize) {
        for _ in 0..500 {
            if backend.snapshot_calls() >= n {
                return;
            }
            settle().await;
        }
        panic!(
            "snapshot_calls never reached {n} (was {})",
            backend.snapshot_calls()
        );
    }

    /// Advances the paused clock in bounded steps until `events` yields a value,
    /// letting the worker's blocking calls and backoff/re-poll sleeps progress.
    async fn advance_until_event(events: &mut EventReceiver, step: Duration) -> Event {
        for _ in 0..500 {
            settle().await;
            match events.try_recv() {
                Ok(ev) => return ev,
                Err(TryRecvError::Empty) => {}
                Err(other) => panic!("event channel error: {other:?}"),
            }
            tokio::time::advance(step).await;
        }
        panic!("no event arrived within the step budget");
    }

    // --- pure coalescing / priority ------------------------------------------

    #[test]
    fn provenance_priority_orders_most_intentional_first() {
        assert!(trigger_priority(Trigger::Manual) > trigger_priority(Trigger::PreRestore));
        assert!(trigger_priority(Trigger::PreRestore) > trigger_priority(Trigger::Event));
        assert!(trigger_priority(Trigger::PreSync) > trigger_priority(Trigger::Event));
        assert!(trigger_priority(Trigger::Event) > trigger_priority(Trigger::Interval));
        // pre-restore and pre-sync share a rank.
        assert_eq!(
            trigger_priority(Trigger::PreRestore),
            trigger_priority(Trigger::PreSync)
        );
    }

    #[test]
    fn coalesce_keeps_the_higher_priority_trigger_and_its_text() {
        let interval = Provenance::interval();
        let event = Provenance::event();
        let manual = Provenance {
            trigger: Trigger::Manual,
            user_text: Some("checkpoint".into()),
        };

        // A higher-priority incoming wins, carrying its text.
        let merged = coalesce(Some(interval.clone()), manual.clone());
        assert_eq!(merged, manual);

        // A lower-priority incoming loses to the pending one.
        let merged = coalesce(Some(manual.clone()), interval.clone());
        assert_eq!(merged, manual);

        // Event beats a pending interval regardless of arrival order.
        assert_eq!(coalesce(Some(interval.clone()), event.clone()), event);
        assert_eq!(coalesce(Some(event.clone()), interval), event);

        // Nothing pending: incoming becomes pending as-is.
        assert_eq!(coalesce(None, event.clone()), event);
    }

    // --- worker behavior (paused time) ---------------------------------------

    #[tokio::test(start_paused = true)]
    async fn single_trigger_produces_one_snapshot() {
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(3)]); // then Clean by default on re-check
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::interval())).unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::SnapshotCompleted {
                files_changed,
                trigger,
                ..
            } => {
                assert_eq!(files_changed, 3);
                assert_eq!(trigger, Trigger::Interval);
            }
            other => panic!("expected SnapshotCompleted, got {other:?}"),
        }
        wait_snapshot_calls(&backend, 2).await;
        assert!(events.try_recv().is_err(), "exactly one snapshot");
        // One commit plus one converging re-check.
        assert_eq!(backend.snapshot_calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn clean_interval_tick_snapshots_nothing() {
        let backend = FakeBackend::new();
        backend.script([Scripted::Clean]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::interval())).unwrap();
        wait_snapshot_calls(&backend, 1).await;
        settle().await;
        // No event: an interval on a clean tree is a no-op.
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(backend.snapshot_calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn triggers_arriving_mid_snapshot_coalesce_into_one_followup() {
        let backend = FakeBackend::new();
        // Call 1 (gated) commits, call 2 (re-check) clean, call 3 (the single
        // coalesced follow-up) commits, call 4 (re-check) clean.
        backend.script([
            Scripted::Commit(1),
            Scripted::Clean,
            Scripted::Commit(2),
            Scripted::Clean,
        ]);
        let release = backend.gate(1);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        // First trigger enters the gated snapshot.
        tx.send(WatchInput::Trigger(Provenance::interval())).unwrap();
        settle().await;
        // A burst of ten more triggers queues while the snapshot is in flight.
        for _ in 0..10 {
            tx.send(WatchInput::Trigger(Provenance::interval())).unwrap();
        }
        settle().await;

        release.send(()).unwrap();

        // Exactly two commits: the original and one coalesced follow-up.
        let first = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(first, Event::SnapshotCompleted { files_changed: 1, .. }));
        let second = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(second, Event::SnapshotCompleted { files_changed: 2, .. }));

        wait_snapshot_calls(&backend, 4).await;
        assert!(
            events.try_recv().is_err(),
            "ten queued triggers must collapse into one follow-up, not ten"
        );
        assert_eq!(
            backend.snapshot_calls(),
            4,
            "two commits and two converging re-checks, regardless of trigger count"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn post_op_recheck_catches_a_write_during_the_muted_window() {
        let backend = FakeBackend::new();
        // The re-check (call 2) finds a change that landed during the mute.
        backend.script([Scripted::Commit(1), Scripted::Commit(1), Scripted::Clean]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let first = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(first, Event::SnapshotCompleted { .. }));
        let second = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(second, Event::SnapshotCompleted { .. }),
            "a write during the muted window must not be lost"
        );
        settle().await;
        assert!(events.try_recv().is_err(), "then it converges");
    }

    #[tokio::test(start_paused = true)]
    async fn worker_is_muted_across_the_operation_and_released_after() {
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let release = backend.gate(1);
        let (tx, mut events, counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // While the gated snapshot is in flight, the watch is muted.
        let mut saw_muted = false;
        for _ in 0..200 {
            settle().await;
            if counter.load(Ordering::SeqCst) == 1 {
                saw_muted = true;
                break;
            }
        }
        assert!(saw_muted, "the worker must be muted during its own snapshot");

        release.send(()).unwrap();
        let _ = advance_until_event(&mut events, Duration::from_secs(1)).await;

        // Once the pass converges the mute is released.
        let mut released = false;
        for _ in 0..200 {
            settle().await;
            if counter.load(Ordering::SeqCst) == 0 {
                released = true;
                break;
            }
        }
        assert!(released, "the mute must be released after the operation");
    }

    #[tokio::test(start_paused = true)]
    async fn contended_lock_is_retried_then_succeeds() {
        let backend = FakeBackend::new();
        // Two contended locks, then a commit; then Clean on the re-check.
        backend.script([Scripted::Lock, Scripted::Lock, Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::interval())).unwrap();

        // If the retry loop were removed, the first lock would requeue and no
        // SnapshotCompleted would ever arrive — this asserts the retry.
        let ev = advance_until_event(&mut events, Duration::from_secs(4)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "a lock that clears must be retried to success"
        );
        assert!(
            backend.snapshot_calls() >= 3,
            "the two locked attempts plus the successful one must all have run"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn permanently_locked_backend_gives_up_the_pass_without_deleting() {
        let backend = FakeBackend::new();
        backend.script([
            Scripted::Lock,
            Scripted::Lock,
            Scripted::Lock,
            Scripted::Lock,
            Scripted::Lock,
        ]);
        let cfg = test_cfg();
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), cfg);

        tx.send(WatchInput::Trigger(Provenance::interval())).unwrap();

        // Drive through all the backoffs; nothing ever completes.
        for _ in 0..200 {
            settle().await;
            tokio::time::advance(Duration::from_secs(16)).await;
        }
        settle().await;
        assert!(
            matches!(events.try_recv(), Err(TryRecvError::Empty)),
            "a permanently locked repo must not report a snapshot"
        );
        // Exactly the configured number of attempts, then the pass gives up.
        assert_eq!(backend.snapshot_calls(), cfg.lock_retry_attempts as usize);

        // No further attempts without a new trigger (the snapshot was requeued,
        // not retried in a loop, and no lock was ever deleted).
        for _ in 0..50 {
            settle().await;
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        assert_eq!(backend.snapshot_calls(), cfg.lock_retry_attempts as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_failure_is_reported() {
        let backend = FakeBackend::new();
        backend.script([Scripted::Fail]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::SnapshotFailed { trigger, .. } => assert_eq!(trigger, Trigger::Event),
            other => panic!("expected SnapshotFailed, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unsafe_repo_pauses_then_auto_resumes_without_further_signal() {
        let backend = FakeBackend::new();
        backend.set_safe(SafeState::Unsafe(UnsafeReason::MergeInProgress));
        backend.script([Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        // A single event trigger (an events-only watch gets nothing more).
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // The watch pauses on the unsafe repo.
        let paused = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match paused {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::Paused),
            other => panic!("expected a pause, got {other:?}"),
        }
        settle().await;
        assert!(
            matches!(events.try_recv(), Err(TryRecvError::Empty)),
            "no snapshot into an unsafe repo"
        );

        // The repo returns to safe; only the bounded re-poll timer can notice —
        // there is no further signal for this events-only watch.
        backend.set_safe(SafeState::Safe);
        let resumed = advance_until_event(&mut events, Duration::from_secs(30)).await;
        match resumed {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::Ok),
            other => panic!("expected a resume, got {other:?}"),
        }
        let snap = advance_until_event(&mut events, Duration::from_secs(30)).await;
        assert!(
            matches!(snap, Event::SnapshotCompleted { .. }),
            "the requeued snapshot runs once the repo is safe again"
        );
        assert!(backend.safe_calls() >= 2, "the re-poll must have re-checked");
    }

    #[tokio::test(start_paused = true)]
    async fn trouble_moves_the_watch_to_attention() {
        let backend = FakeBackend::new();
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trouble {
            detail: "inotify queue overflowed".into(),
        })
        .unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::WatchStateChanged { to, reason, .. } => {
                assert_eq!(to, WatchState::Attention);
                assert_eq!(reason.as_deref(), Some("inotify queue overflowed"));
            }
            other => panic!("expected an attention transition, got {other:?}"),
        }
    }

    // --- builder validation --------------------------------------------------

    #[test]
    fn build_rejects_duplicate_watch_names() {
        let spec = WatchSpec::builder("dup", "/tmp/a").build().unwrap();
        let spec2 = WatchSpec::builder("dup", "/tmp/b").build().unwrap();
        let backend = FakeBackend::new() as SharedBackend;
        let result = Engine::builder()
            .watch_with_backend(spec, Arc::clone(&backend))
            .watch_with_backend(spec2, backend)
            .build();
        match result {
            Err(EngineError::DuplicateWatch { name }) => assert_eq!(name, "dup"),
            other => panic!("expected DuplicateWatch, got {:?}", other.map(|_| ())),
        }
    }
}
