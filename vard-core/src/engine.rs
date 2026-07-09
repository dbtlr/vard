//! The snapshot engine: the coordinator that turns watcher and scheduler
//! signals into version-control snapshots, one watch at a time.
//!
//! [`Engine`] is the embeddable SDK entry point (the spec's §2a contract). A
//! host builds it from validated [`WatchSpec`] values, subscribes to its
//! [`EventBus`], and starts it:
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
//! interval timer — most-intentional wins). When a snapshot comes due the
//! worker:
//!
//! 1. Acquires the watch's [`MuteGuard`] so vard's own writes
//!    do not feed back as fresh activity (self-suppression), and holds it across
//!    the whole operation.
//! 2. Re-checks [`is_safe_state`](VcsBackend::is_safe_state). An unsafe repo
//!    pauses the watch and arms a bounded retry that auto-resumes it once the
//!    repo returns to safe — this works even for an `events`-only watch that
//!    will receive no further signals, because the retry is a timer, not a
//!    signal.
//! 3. On a safe repo, calls [`snapshot`](VcsBackend::snapshot), retrying a
//!    contended index lock with exponential backoff before requeueing (it never
//!    deletes a foreign lock), and emits [`Event::SnapshotCompleted`] /
//!    [`Event::SnapshotFailed`] as appropriate.
//! 4. Runs a **post-op dirtiness re-check**: because the sweep is a total
//!    `add -A`, a clean tree yields nothing on a second pass, so re-snapshotting
//!    converges — but a real write that landed during the muted window is caught
//!    and snapshotted as a follow-up, never lost. The re-check is bounded to one
//!    sweep under the mute so a misbehaving backend cannot livelock it.
//!
//! # Converging a stuck pending change
//!
//! Whenever a pass holds a pending change it could not snapshot — for **any**
//! reason: an unsafe repo, a failed safe-state probe, or a hard snapshot
//! failure — the worker preserves the change and arms a single bounded retry
//! timer that re-attempts it until it converges, with no further external
//! signal. The failure is surfaced once (on entry), not once per tick, and the
//! retry budget is bounded so a permanently broken repository stops retrying and
//! waits for fresh activity. A contended lock is the sole exception: it requeues
//! and waits for the next trigger rather than self-driving, so an externally
//! held lock is never hammered.
//!
//! Trouble from either signal source ([`WatcherSignal::Trouble`],
//! [`SchedulerSignal::Trouble`]), and a panicked backend call, move the watch to
//! [`WatchState::Attention`](crate::WatchState) and are surfaced on the bus, so
//! nothing dies silently.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinError;
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

/// Default cadence at which a worker holding an un-snapshotted pending change
/// re-attempts it — re-polling [`is_safe_state`](VcsBackend::is_safe_state) and
/// re-running the snapshot — so it converges without any further external
/// signal. Covers an unsafe repository, a failed safe-state probe, and a hard
/// snapshot failure alike.
pub const DEFAULT_UNSAFE_REPOLL_INTERVAL: Duration = Duration::from_secs(30);

/// Default cap on consecutive self-driving retries before a stuck worker stops
/// retrying and waits for fresh activity. At [`DEFAULT_UNSAFE_REPOLL_INTERVAL`]
/// this is four hours of background retrying — bounded, so a permanently broken
/// repository does not retry forever, yet long enough that a genuine mid-op
/// pause or transient failure always converges. The counter resets whenever new
/// activity arrives.
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
    /// The blocking backend call panicked; the detail is the join error text.
    Panicked(String),
}

/// Why a worker is holding an un-snapshotted pending change and retrying it on
/// the bounded timer rather than waiting for an external signal.
///
/// The single generalized retry loop converges any stuck pending change without
/// a future trigger; the kind only decides how the condition is surfaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryKind {
    /// The repository is unsafe to commit: reported as
    /// [`WatchState::Paused`](crate::WatchState) and resolved with an `Ok`
    /// transition once it returns to safe.
    UnsafePause,
    /// A safe-state probe or snapshot attempt failed:
    /// [`Event::SnapshotFailed`] was emitted once on entry and the change is
    /// retried silently (no per-tick failure storm).
    Failure,
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
    /// Set while the worker holds a pending change it could not snapshot and is
    /// converging it on the bounded retry timer, without any external signal.
    /// The kind records how the blocking condition is surfaced.
    retry: Option<RetryKind>,
    /// Consecutive retry ticks since the last activity (bounds self-driving
    /// retry).
    retry_attempts: u32,
    /// Whether the retry budget is spent; the worker then waits for activity.
    retry_exhausted: bool,
}

impl Worker {
    /// Runs the worker until its input channel closes (every signal source for
    /// this watch was dropped, i.e. the engine is shutting down).
    ///
    /// The loop blocks for the next input, except while a pending change is
    /// being retried, when it also wakes on the retry deadline so the change
    /// converges without any further signal.
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<WatchInput>) {
        loop {
            let waiting_on_retry = self.retry.is_some() && !self.retry_exhausted;
            let input = if waiting_on_retry {
                match timeout(self.cfg.unsafe_repoll_interval, rx.recv()).await {
                    Ok(received) => received,
                    Err(_elapsed) => {
                        self.retry_tick().await;
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

            // While retrying, the bounded timer (not a fresh input) drives the
            // next attempt, so we do not hammer a still-broken repository.
            if self.pending.is_some() && self.retry.is_none() {
                self.run_pass().await;
            }
        }
    }

    /// Folds one input into the worker's state.
    fn apply(&mut self, input: WatchInput) {
        match input {
            WatchInput::Trigger(prov) => {
                // Fresh activity gives a retrying watch a new chance to converge.
                if self.retry.is_some() {
                    self.retry_attempts = 0;
                    self.retry_exhausted = false;
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
    ///
    /// Any way the pass fails to snapshot the pending change — an unsafe repo, a
    /// failed safe-state probe, or a hard snapshot failure — preserves the
    /// pending change and arms the bounded retry timer, so the change converges
    /// without any external signal (see [`retry_tick`](Self::retry_tick)). A
    /// contended lock is the one exception: it requeues and waits for the next
    /// trigger, never self-driving, so an externally held lock is not hammered.
    async fn run_pass(&mut self) {
        let _mute = self.mute.acquire();

        // Bounds the post-op dirtiness re-check to a single converging sweep
        // under the mute: a backend that never returns `Clean` must not livelock
        // this loop while holding the mute.
        let mut committed_in_pass = false;

        while let Some(prov) = self.pending.take() {
            match check_safe(Arc::clone(&self.backend)).await {
                Ok(Ok(SafeState::Safe)) => self.on_safe(),
                Ok(Ok(SafeState::Unsafe(reason))) => {
                    self.pending = Some(prov);
                    self.enter_unsafe(reason);
                    return;
                }
                Ok(Err(err)) => {
                    // The safe-state probe itself failed; do not commit into an
                    // unknown state. Preserve the pending change and converge it
                    // on the bounded retry timer instead of dropping it.
                    self.pending = Some(prov.clone());
                    self.enter_failure(prov.trigger, &err);
                    return;
                }
                Err(join) => {
                    // The probe task panicked: surface it, move to Attention,
                    // and stay alive to process later inputs.
                    self.on_backend_panic(prov.trigger, &join.to_string());
                    return;
                }
            }

            match snapshot_with_retry(Arc::clone(&self.backend), self.cfg, &prov).await {
                PassResult::Committed(outcome) => {
                    self.emit_completed(prov.trigger, &outcome);
                    // A commit means any prior retry has converged.
                    self.clear_retry();
                    if committed_in_pass {
                        // A second commit under the mute is a genuine
                        // mute-window write. Requeue it (carrying this pass's
                        // provenance) and return so the mute is released and the
                        // outer loop handles it, rather than looping unbounded
                        // under the mute.
                        self.pending = Some(prov);
                        return;
                    }
                    committed_in_pass = true;
                    // Post-op dirtiness re-check: sweep once more under the same
                    // mute, carrying this pass's provenance. A clean tree returns
                    // `Clean` and the loop converges; a write that landed during
                    // the muted window is caught and snapshotted as a follow-up.
                    self.pending = Some(prov);
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
                    // Preserve the pending change and converge it on the bounded
                    // retry timer instead of dropping a hard failure.
                    self.pending = Some(prov.clone());
                    self.enter_failure(prov.trigger, &err);
                    return;
                }
                PassResult::Panicked(detail) => {
                    self.on_backend_panic(prov.trigger, &detail);
                    return;
                }
            }
        }
    }

    /// One bounded retry tick for a stuck pending change: re-attempt the pass so
    /// an unsafe pause, failed probe, or hard failure converges without any
    /// external signal. Exhausts the budget once the cap is reached, after which
    /// the worker waits for fresh activity.
    async fn retry_tick(&mut self) {
        self.retry_attempts += 1;
        if self.pending.is_some() {
            self.run_pass().await;
        }
        // Still stuck after this attempt and out of budget: stop self-driving
        // and wait for activity, so a permanently broken repo does not retry
        // forever. Fresh activity resets the budget (see [`apply`](Self::apply)).
        if self.retry.is_some() && self.retry_attempts >= self.cfg.unsafe_repoll_max_attempts {
            self.retry_exhausted = true;
        }
    }

    /// Handles a safe repository at the top of a pass: resolves an unsafe pause
    /// with an `Ok` transition and clears the retry. A failure retry is left
    /// intact — a clean probe does not mean the snapshot will succeed — and is
    /// cleared only when a snapshot actually commits.
    fn on_safe(&mut self) {
        if self.retry == Some(RetryKind::UnsafePause) {
            self.set_state(
                WatchState::Ok,
                Some("repository returned to a safe state".into()),
            );
            self.clear_retry();
        }
    }

    /// Clears any active retry state (a snapshot committed, or an unsafe pause
    /// resolved).
    fn clear_retry(&mut self) {
        self.retry = None;
        self.retry_attempts = 0;
        self.retry_exhausted = false;
    }

    /// Enters (or stays in) the unsafe-paused retry, arming the bounded timer.
    /// The `Paused` transition and the budget reset happen only on the first
    /// entry, so a re-poll that still finds the repo unsafe neither re-emits nor
    /// refills the budget.
    fn enter_unsafe(&mut self, reason: UnsafeReason) {
        if self.retry.is_none() {
            self.retry_attempts = 0;
            self.retry_exhausted = false;
        }
        self.retry = Some(RetryKind::UnsafePause);
        self.set_state(WatchState::Paused, Some(reason.to_string()));
    }

    /// Enters (or stays in) the failure retry after a probe or snapshot failed.
    /// Emits [`Event::SnapshotFailed`] and resets the budget only on the first
    /// entry, so a retry loop surfaces the failure once — never once per tick.
    fn enter_failure(&mut self, trigger: Trigger, err: &VcsError) {
        if self.retry.is_none() {
            self.emit_failed(trigger, err);
            self.retry_attempts = 0;
            self.retry_exhausted = false;
        }
        self.retry = Some(RetryKind::Failure);
    }

    /// Surfaces a panicked backend call: emits [`Event::SnapshotFailed`], moves
    /// the watch to [`WatchState::Attention`], and returns so the worker stays
    /// alive to process later inputs (a detached panic must not kill the watch).
    fn on_backend_panic(&mut self, trigger: Trigger, detail: &str) {
        let msg = format!("backend task panicked: {detail}");
        self.emit_failed_error(trigger, msg.clone());
        self.set_state(WatchState::Attention, Some(msg));
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
        self.emit_failed_error(trigger, err.to_string());
    }

    /// Emits [`Event::SnapshotFailed`] with a raw error message (for conditions
    /// that are not a [`VcsError`], such as a panicked backend task).
    fn emit_failed_error(&self, trigger: Trigger, error: String) {
        self.bus.emit(Event::SnapshotFailed {
            watch: self.name.clone(),
            trigger,
            error,
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
///
/// A backend panic is returned as the outer [`JoinError`] rather than
/// propagated, so the caller can surface it instead of aborting the detached
/// worker task (which would kill the watch and leave its channel unread).
async fn check_safe(backend: SharedBackend) -> Result<Result<SafeState, VcsError>, JoinError> {
    tokio::task::spawn_blocking(move || backend.is_safe_state()).await
}

/// Runs [`snapshot`](VcsBackend::snapshot) off the async runtime. See
/// [`check_safe`] for why it takes the backend by value and returns the
/// [`JoinError`] rather than propagating a panic.
async fn call_snapshot(
    backend: SharedBackend,
    req: SnapshotRequest,
) -> Result<Result<Option<SnapshotOutcome>, VcsError>, JoinError> {
    tokio::task::spawn_blocking(move || backend.snapshot(&req)).await
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
            Ok(Ok(Some(outcome))) => return PassResult::Committed(outcome),
            Ok(Ok(None)) => return PassResult::Clean,
            Ok(Err(VcsError::UnsafeState(reason))) => return PassResult::Unsafe(reason),
            Ok(Err(VcsError::LockContended { .. })) => {
                if attempt < cfg.lock_retry_attempts {
                    tokio::time::sleep(backoff(cfg, attempt)).await;
                    continue;
                }
                return PassResult::StillLocked;
            }
            Ok(Err(other)) => return PassResult::Failed(other),
            Err(join) => return PassResult::Panicked(join.to_string()),
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
                retry: None,
                retry_attempts: 0,
                retry_exhausted: false,
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
            SchedulerSignal::Tick { watch } => (watch, WatchInput::Trigger(Provenance::interval())),
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
                    let backend =
                        open_git_backend(&spec).map_err(|source| EngineError::Backend {
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
                write!(
                    f,
                    "duplicate watch name {name:?}; watch names must be unique"
                )
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
        /// Panic inside the blocking snapshot call.
        Panic,
    }

    /// A deterministic in-memory [`VcsBackend`] for driving worker scenarios.
    struct FakeBackend {
        inner: Mutex<FakeInner>,
    }

    struct FakeInner {
        safe: SafeState,
        /// Scripted safe-state probe results, consumed before falling back to
        /// `safe`. Lets a test make [`is_safe_state`] fail or flip over calls.
        safe_results: VecDeque<Result<SafeState, VcsError>>,
        snapshots: VecDeque<Scripted>,
        /// When set, every snapshot commits — models a backend that never
        /// reports a clean tree (used to prove the post-op re-check is bounded).
        always_commit: bool,
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
                    safe_results: VecDeque::new(),
                    snapshots: VecDeque::new(),
                    always_commit: false,
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

        /// Scripts a sequence of [`is_safe_state`] results consumed in order;
        /// once exhausted the probe falls back to the fixed `safe` state.
        fn script_safe(&self, results: impl IntoIterator<Item = Result<SafeState, VcsError>>) {
            self.inner.lock().unwrap().safe_results.extend(results);
        }

        fn set_safe(&self, safe: SafeState) {
            self.inner.lock().unwrap().safe = safe;
        }

        /// Makes every snapshot commit, so the post-op re-check never converges
        /// on its own — used to prove the re-check is bounded under the mute.
        fn set_always_commit(&self) {
            self.inner.lock().unwrap().always_commit = true;
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
            match inner.safe_results.pop_front() {
                Some(scripted) => scripted,
                None => Ok(inner.safe.clone()),
            }
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
                let result = if inner.always_commit {
                    Scripted::Commit(1)
                } else {
                    inner.snapshots.pop_front().unwrap_or(Scripted::Clean)
                };
                (result, gate)
            };
            // Block outside the lock so the test can inspect/mutate meanwhile.
            if let Some(rx) = gate {
                let _ = rx.recv();
            }
            match result {
                Scripted::Panic => panic!("scripted backend panic"),
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
            retry: None,
            retry_attempts: 0,
            retry_exhausted: false,
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
    ///
    /// The cooperative yields drive the current-thread runtime, but a worker's
    /// backend calls run on a separate `spawn_blocking` thread. A brief real
    /// sleep releases the OS core so that pool makes progress: without it a
    /// busy-spinning poll loop can starve its own blocking threads under
    /// parallel test load, which made the count/wait assertions flaky.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        std::thread::sleep(Duration::from_micros(100));
        tokio::task::yield_now().await;
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

        tx.send(WatchInput::Trigger(Provenance::interval()))
            .unwrap();

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

        tx.send(WatchInput::Trigger(Provenance::interval()))
            .unwrap();
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
        tx.send(WatchInput::Trigger(Provenance::interval()))
            .unwrap();
        settle().await;
        // A burst of ten more triggers queues while the snapshot is in flight.
        for _ in 0..10 {
            tx.send(WatchInput::Trigger(Provenance::interval()))
                .unwrap();
        }
        settle().await;

        release.send(()).unwrap();

        // Exactly two commits: the original and one coalesced follow-up.
        let first = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(
            first,
            Event::SnapshotCompleted {
                files_changed: 1,
                ..
            }
        ));
        let second = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(
            second,
            Event::SnapshotCompleted {
                files_changed: 2,
                ..
            }
        ));

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
        assert!(
            saw_muted,
            "the worker must be muted during its own snapshot"
        );

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

        tx.send(WatchInput::Trigger(Provenance::interval()))
            .unwrap();

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

        tx.send(WatchInput::Trigger(Provenance::interval()))
            .unwrap();

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

    /// Counts [`Event::SnapshotFailed`] drained so far without blocking.
    fn drain_failures(events: &mut EventReceiver) -> usize {
        let mut n = 0;
        loop {
            match events.try_recv() {
                Ok(Event::SnapshotFailed { .. }) => n += 1,
                Ok(_) => {}
                Err(_) => return n,
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_failure_is_preserved_and_retried_reporting_once() {
        // A hard failure is surfaced once, the pending change is preserved, and
        // it converges on the bounded retry timer with no further trigger — and
        // the retry does not storm the bus with a failure per tick.
        let backend = FakeBackend::new();
        backend.script([Scripted::Fail, Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // The failure is reported once, tagged with its trigger.
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::SnapshotFailed { trigger, .. } => assert_eq!(trigger, Trigger::Event),
            other => panic!("expected SnapshotFailed, got {other:?}"),
        }

        // With no new trigger, the retry timer re-attempts and the change lands.
        let ev = advance_until_event(&mut events, Duration::from_secs(30)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the preserved change must converge via the retry timer, got {ev:?}"
        );

        // Exactly one failure was emitted across the whole sequence.
        settle().await;
        assert_eq!(
            drain_failures(&mut events),
            0,
            "no further SnapshotFailed after convergence (no per-tick storm)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn safe_probe_error_is_preserved_and_retried_until_safe() {
        // The safe-state probe fails once, then succeeds. The pending change
        // must not be dropped: it converges on the retry timer with no trigger.
        let backend = FakeBackend::new();
        backend.script_safe([
            Err(VcsError::CommandFailed {
                op: "status".into(),
                status: Some(1),
                stderr: "boom".into(),
            }),
            Ok(SafeState::Safe),
        ]);
        backend.script([Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(ev, Event::SnapshotFailed { .. }),
            "a failed safe-state probe is surfaced, got {ev:?}"
        );

        let ev = advance_until_event(&mut events, Duration::from_secs(30)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the preserved change snapshots once the probe returns safe, got {ev:?}"
        );
        settle().await;
        assert_eq!(
            drain_failures(&mut events),
            0,
            "the probe error is reported once, not per retry tick"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_budget_exhausts_then_resumes_on_fresh_activity() {
        // A permanently unsafe repo retries only up to the bound, then stops and
        // waits for activity; a later trigger (with the repo now safe) resumes.
        let backend = FakeBackend::new();
        backend.set_safe(SafeState::Unsafe(UnsafeReason::MergeInProgress));
        backend.script([Scripted::Commit(1)]);
        let mut cfg = test_cfg();
        cfg.unsafe_repoll_max_attempts = 2;
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), cfg);

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // Pauses on the unsafe repo.
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(
            ev,
            Event::WatchStateChanged {
                to: WatchState::Paused,
                ..
            }
        ));

        // Drive well past the bounded retries; polling stops at the cap.
        for _ in 0..50 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        settle().await;
        let calls_after_exhaustion = backend.safe_calls();
        for _ in 0..50 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        settle().await;
        assert_eq!(
            backend.safe_calls(),
            calls_after_exhaustion,
            "an exhausted retry budget stops polling until fresh activity"
        );

        // Fresh activity (repo now safe) resets the budget and resumes.
        backend.set_safe(SafeState::Safe);
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(30)).await;
        assert!(
            matches!(
                ev,
                Event::WatchStateChanged {
                    to: WatchState::Ok,
                    ..
                }
            ),
            "fresh activity re-arms the retry and the watch resumes, got {ev:?}"
        );
        let ev = advance_until_event(&mut events, Duration::from_secs(30)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the preserved change snapshots after resuming, got {ev:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backend_panic_is_surfaced_and_the_worker_survives() {
        // A panicking backend call must not kill the detached worker: it is
        // surfaced (SnapshotFailed + Attention) and a later trigger still works.
        let backend = FakeBackend::new();
        backend.script([Scripted::Panic, Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // The panic is surfaced as a failure and an attention transition.
        let mut saw_failed = false;
        let mut saw_attention = false;
        for _ in 0..2 {
            match advance_until_event(&mut events, Duration::from_secs(1)).await {
                Event::SnapshotFailed { .. } => saw_failed = true,
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    ..
                } => saw_attention = true,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(
            saw_failed,
            "a backend panic must be surfaced as SnapshotFailed"
        );
        assert!(
            saw_attention,
            "a backend panic must move the watch to Attention"
        );

        // The worker is still alive: a second, non-panicking trigger snapshots.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the worker must keep processing inputs after a backend panic, got {ev:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn always_committing_backend_does_not_livelock_under_the_mute() {
        // A backend that never reports a clean tree must not livelock the post-op
        // re-check while holding the mute: the re-check is bounded to one sweep,
        // so a pass makes exactly two snapshot calls and releases the mute.
        let backend = FakeBackend::new();
        backend.set_always_commit();
        let (tx, mut events, counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // Two commits: the original plus one bounded re-check.
        let _ = advance_until_event(&mut events, Duration::from_secs(1)).await;
        let _ = advance_until_event(&mut events, Duration::from_secs(1)).await;
        wait_snapshot_calls(&backend, 2).await;

        // The pass does not loop unbounded under the mute: calls stop at two and
        // the mute is released.
        for _ in 0..50 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        assert_eq!(
            backend.snapshot_calls(),
            2,
            "the post-op re-check must be bounded to one sweep per pass"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "the mute must be released after the bounded pass"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mute_window_followup_keeps_the_pass_provenance() {
        // A change captured in a Manual pass's mute window must be tagged with
        // the pass's winning provenance (Manual), not hardcoded to Event.
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1), Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance {
            trigger: Trigger::Manual,
            user_text: Some("checkpoint".into()),
        }))
        .unwrap();

        let first = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match first {
            Event::SnapshotCompleted { trigger, .. } => assert_eq!(trigger, Trigger::Manual),
            other => panic!("expected SnapshotCompleted, got {other:?}"),
        }
        let second = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match second {
            Event::SnapshotCompleted { trigger, .. } => assert_eq!(
                trigger,
                Trigger::Manual,
                "the mute-window follow-up must keep the pass's provenance, not become Event"
            ),
            other => panic!("expected a follow-up SnapshotCompleted, got {other:?}"),
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
        assert!(
            backend.safe_calls() >= 2,
            "the re-poll must have re-checked"
        );
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
