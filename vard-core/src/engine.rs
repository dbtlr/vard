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
//! let handle = engine.start().await?;
//!
//! while let Ok(ev) = events.recv().await {
//!     match ev {
//!         Event::SnapshotCompleted { watch, snapshot, .. } => { let _ = (watch, snapshot); }
//!         Event::SyncConflict { watch, .. } => { let _ = watch; }
//!         Event::DaemonStopped => break,
//!         _ => {}
//!     }
//! }
//!
//! handle.shutdown().await; // drain in-flight passes, then Event::DaemonStopped
//! # Ok(())
//! # }
//! ```
//!
//! # One worker per watch
//!
//! [`start`](Engine::start) arms the [`Watcher`] and two [`Scheduler`]s once for
//! the whole engine — one for the snapshot interval, one for the pull-driven
//! sync interval, each exposing a single multiplexed receiver — and spawns
//! exactly one **worker** task per watch. Three dispatcher tasks fan the shared
//! [`WatcherSignal`]/[`SchedulerSignal`] streams out to the right worker by
//! watch name (the watcher, the snapshot scheduler, and the sync scheduler,
//! each on its own channel). Watches therefore run concurrently, while every
//! operation within a watch is strictly serialized — a worker is a single task,
//! so it is doing at most one thing at a time.
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
//! nothing dies silently. Whether that `Attention` clears itself once a later
//! pass succeeds, or stays until an operator resolves it, is decided per
//! [`crate::TroubleKind`] — see [`crate::TroubleKind::latches`].
//!
//! # The sync cycle
//!
//! A syncing watch reconciles with its remote on the *same serialized worker*,
//! so a sync never overlaps a snapshot on that watch. A cycle is requested via
//! [`EngineHandle::request_sync`] (manual) or automatically — after a successful
//! snapshot (the post-snapshot enqueue), on the pull-driven sync-interval
//! cadence (a jittered [`Scheduler`] tick), and on the failure-backoff retry;
//! [`EngineHandle::request_auto_sync`] is the external entry point for the same
//! automatic path. It runs under two hard invariants.
//!
//! **Lock/network separation.** The network-facing steps — [`fetch`](VcsBackend::fetch)
//! and [`push`](VcsBackend::push) — run **outside** the per-watch op lock, each
//! timeout-bounded ([`DEFAULT_SYNC_NETWORK_TIMEOUT`]), so a hung endpoint
//! can never block a worker while holding the lock. Between them, one **locked
//! window** holds the op lock and one journal bracket (`begin("sync")` →
//! `complete`) with **zero network I/O** inside: a pre-sync snapshot
//! ([`Trigger::PreSync`], a no-op on a clean tree), then — only when the fetch
//! found remote commits to integrate — the out-of-tree
//! [`reconcile`](VcsBackend::reconcile) and the single [`advance`](VcsBackend::advance)
//! that makes the reconciled tip live. With nothing new remotely (including a
//! never-pushed branch, whose upstream ref does not exist yet) the window is
//! just the pre-sync snapshot and the cycle proceeds straight to the push.
//! The cycle's **pre-flight** — the fetch, then the dirty check (in that order,
//! so an edit saved during the fetch is never missed by the early exits) — is
//! lock-free; the op gate is engaged **only when locked work is needed** (a
//! dirty tree to snapshot, or remote commits to integrate). A clean up-to-date
//! watch, and a clean ahead-only push, never touch the gate — a foreign op-lock
//! holder cannot fail work that needs no lock. A busy gate defers on a short
//! cadence with the pre-flight cached: while the holder wedges, the paced
//! retries probe only the gate (zero network I/O and zero subprocesses), and
//! the cache's freshness is judged once, at gate acquisition — still fresh
//! proceeds on it, stale costs exactly one fresh pre-flight. The wait
//! deliberately does NOT re-evaluate whether the work still exists (a user who
//! hand-resolves mid-wait re-runs after the bounded wait's honest "did not
//! run"): a wedged gate is an abnormal state, and rare-edge honesty beats
//! rare-edge cleverness that could report a false success against a stale
//! tracking ref. A stale rebase target is never a correctness
//! problem: a remote that moved past the cache surfaces as a non-fast-forward
//! push, which re-runs the cycle with a fresh fetch.
//! The op guard is coupled to that blocking
//! git work (it moves into `spawn_blocking` and is closed there), so an async
//! abort can never separate lock-release from git-completion — the same
//! discipline as the snapshot path.
//!
//! **No dirty tree, ever.** The reconcile rebases in a vard-owned scratch
//! worktree ([`WatchSpec::scratch_dir`], host-injected — vard-core resolves no
//! paths), never touching the user's tree; the tree only ever moves between
//! fully-committed states, and only via the single `advance` under the lock (the
//! pre-sync snapshot having committed everything first). Snapshot-local-first
//! means no sync step can destroy the only copy of anything. The advance's tree
//! rewrite is performed under the self-suppression [`MuteGuard`], and it lands a
//! fully-committed tree, so it can neither feed back as fresh activity nor leave
//! anything for a follow-up pass to commit.
//!
//! A [`ReconcileOutcome::Conflict`] latches the watch [`WatchState::Conflicted`]
//! (auto-sync then stops for it; local snapshotting continues); a network/auth
//! failure enters [`WatchState::SyncError`] with an exponential backoff
//! (capped at [`DEFAULT_SYNC_BACKOFF_CAP`], reset on success) re-driven by the
//! worker's own timer. A lost fast-forward race ([`PushOutcome::NonFastForward`])
//! re-runs the cycle in place up to [`SYNC_MAX_ATTEMPTS`] before degrading to a
//! backoff.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::{Instant, timeout};

use crate::config::{TriggerMode, WatchSpec};
use crate::event::{Event, EventBus, EventReceiver, SkipReason, Trigger, TroubleKind, WatchState};
use crate::gate::{OpGuard, SharedGate, default_gate};
use crate::scheduler::{ScheduleHandle, Scheduler, SchedulerRx, SchedulerSignal};
use crate::secret_scan::{SecretMatch, SecretScanError, SecretScanner};
use crate::vcs::git::GitBackend;
use crate::vcs::{
    AdvanceOutcome, LogFilter, PushOutcome, ReconcileOutcome, SafeState, SnapshotId,
    SnapshotOutcome, SnapshotReport, SnapshotRequest, UnsafeReason, VcsBackend, VcsError,
};
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

/// Default budget [`EngineHandle::shutdown`] gives the workers to drain any
/// in-flight pass before it escalates to aborting them. A pass shells out to
/// `git` (a commit on a large tree, a lock-retry backoff), so the window is
/// generous; a worker still running when it elapses is aborted and shutdown
/// completes regardless (see [`EngineHandle::shutdown`]).
pub const DEFAULT_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cadence for the op-gate-busy self-retry (F7). Much shorter than
/// [`DEFAULT_UNSAFE_REPOLL_INTERVAL`] because the contended lock is *our own*
/// per-watch op lock — held only across a peer's commit window and freed
/// quickly — so re-attempting soon converges an event-only watch that would
/// otherwise wait for a fresh trigger, without hammering a foreign lock.
pub const DEFAULT_GATE_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Default wall-clock bound on each network-facing sync step
/// ([`fetch`](VcsBackend::fetch) and [`push`](VcsBackend::push)). On expiry the
/// backend kills the git child (and its process group) and returns
/// [`VcsError::Timeout`], which the sync cycle treats as a network failure:
/// [`WatchState::SyncError`] with exponential backoff. Sixty seconds
/// comfortably covers a healthy fetch/push while bounding a hung endpoint.
pub const DEFAULT_SYNC_NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// Base delay of the sync failure backoff. On the first
/// [`WatchState::SyncError`] the watch re-attempts after this long, then doubles
/// each consecutive failure up to [`DEFAULT_SYNC_BACKOFF_CAP`]. This is its own
/// escalating schedule, deliberately separate from the snapshot retry cadences
/// (the long unsafe/failure re-poll and the short gate-busy retry): a network
/// outage is retried on a network-appropriate ramp, not the snapshot ramp.
pub const DEFAULT_SYNC_BACKOFF_BASE: Duration = Duration::from_secs(60);

/// Cap on the sync failure backoff: consecutive failures ramp
/// `1m, 2m, 4m, …` and then hold here. A persistently unreachable remote is
/// retried at most once an hour, never abandoned (unlike the bounded snapshot
/// retry budget) — sync has no local-data-loss stake, so it simply keeps trying
/// on a slow cadence until the remote returns.
pub const DEFAULT_SYNC_BACKOFF_CAP: Duration = Duration::from_secs(60 * 60);

/// How many times one sync cycle re-runs `fetch → reconcile → advance → push`
/// when the push loses a fast-forward race
/// ([`PushOutcome::NonFastForward`]) before it stops converging in-cycle and
/// degrades to a [`WatchState::SyncError`] backoff. Three total attempts bounds
/// a pathologically contended remote without giving up on the common
/// single-race case.
pub const SYNC_MAX_ATTEMPTS: u32 = 3;

/// How many gate-busy deferrals the shutdown drain services before it stops
/// waiting and terminates the request as [`SyncOutcome::NotRun`]. The drain
/// exists to converge a request the engine itself deferred (a transient
/// gate-busy, a clobber retry); a gate held by a *foreign or peer* process may
/// never free, and a CLI must fail fast with an honest "did not run" rather
/// than hang for the whole shutdown drain budget. Three paced tries cover the
/// transient our-own-window case without stretching the stuck case past ~2 s.
const DRAIN_GATE_BUSY_MAX_ATTEMPTS: u32 = 3;

/// How long a [`CachedFetch`] stays fresh, as a multiple of the gate-busy
/// cadence ([`EngineConfig::gate_busy_retry_interval`]): a gate-busy deferral's
/// paced retries reuse the pre-flight dirty+fetch result instead of re-fetching
/// on every attempt, and this bounds how stale that reuse can get (8 × the
/// 500 ms default = 4 s). A cycle that proceeds on a cached fetch and turns out
/// stale is still correct — a moved remote surfaces as a non-fast-forward push,
/// which re-runs the cycle with a fresh fetch.
const SYNC_FETCH_CACHE_TTL_CADENCES: u32 = 8;

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
    shutdown_drain_timeout: Duration,
    /// The (shorter) cadence for the op-gate-busy self-retry; see
    /// [`RetryKind::GateBusy`] and [`DEFAULT_GATE_BUSY_RETRY_INTERVAL`].
    gate_busy_retry_interval: Duration,
    /// Per-step timeout for the sync cycle's network ops (fetch/push); see
    /// [`DEFAULT_SYNC_NETWORK_TIMEOUT`].
    sync_network_timeout: Duration,
    /// Base delay of the sync failure backoff; see [`DEFAULT_SYNC_BACKOFF_BASE`].
    sync_backoff_base: Duration,
    /// Cap on the sync failure backoff; see [`DEFAULT_SYNC_BACKOFF_CAP`].
    sync_backoff_cap: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            lock_retry_attempts: DEFAULT_LOCK_RETRY_ATTEMPTS,
            lock_retry_base: DEFAULT_LOCK_RETRY_BASE,
            unsafe_repoll_interval: DEFAULT_UNSAFE_REPOLL_INTERVAL,
            unsafe_repoll_max_attempts: DEFAULT_UNSAFE_REPOLL_MAX_ATTEMPTS,
            shutdown_drain_timeout: DEFAULT_SHUTDOWN_DRAIN_TIMEOUT,
            gate_busy_retry_interval: DEFAULT_GATE_BUSY_RETRY_INTERVAL,
            sync_network_timeout: DEFAULT_SYNC_NETWORK_TIMEOUT,
            sync_backoff_base: DEFAULT_SYNC_BACKOFF_BASE,
            sync_backoff_cap: DEFAULT_SYNC_BACKOFF_CAP,
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

/// A point-in-time snapshot of one watch's lifecycle truth, returned by
/// [`EngineHandle::watch_states`].
///
/// This is the *queryable* projection of engine state a host renders into
/// health or status output, rather than reconstructing state from the
/// [`Event`] stream — which is lossy, since a slow subscriber can miss a
/// [`Event::WatchStateChanged`]. A `watch_states` call always reflects the
/// engine's own current truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchStatus {
    /// The watch's stable name.
    pub name: String,
    /// The watch's current lifecycle state.
    pub state: WatchState,
    /// The [`TroubleKind`] of the transition that put the watch in `state`, when
    /// that transition was caused by trouble; `None` otherwise (a healthy `Ok`,
    /// an unsafe-repo auto-pause, ...).
    pub trouble: Option<TroubleKind>,
    /// The human-readable reason recorded for the current state, when one was
    /// given.
    pub reason: Option<String>,
    /// When the watch entered `state`.
    ///
    /// **Engine-local and not persisted.** It is stamped `SystemTime::now()` on
    /// each transition and lives only in memory, so a daemon restart or engine
    /// rebuild resets it: a watch that has genuinely been blocked for hours
    /// reads as freshly entered right after a restart. A host that needs
    /// restart-stable "since" must persist its own timestamp.
    pub entered_at: SystemTime,
}

/// The per-worker mutable status cell the [`EngineHandle`] reads through
/// [`watch_states`](EngineHandle::watch_states). A worker owns one `Arc` clone
/// and mirrors every state transition into it under the lock; the handle holds
/// the other clone and reads it. Cheap: touched only on an actual transition
/// (which the engine already emits sparingly) and on an explicit query.
#[derive(Debug)]
struct SharedStatus {
    state: WatchState,
    trouble: Option<TroubleKind>,
    reason: Option<String>,
    entered_at: SystemTime,
}

impl SharedStatus {
    /// A freshly-started worker's status: healthy, entered now.
    fn new() -> SharedStatus {
        SharedStatus {
            state: WatchState::Ok,
            trouble: None,
            reason: None,
            entered_at: SystemTime::now(),
        }
    }
}

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

    /// A manual provenance, injected by [`EngineHandle::trigger`] when a host
    /// (the daemon draining a request file, spec §11) asks for a snapshot now.
    fn manual() -> Self {
        Self {
            trigger: Trigger::Manual,
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

/// One item routed to a worker: a snapshot trigger, a sync request, or a
/// trouble report.
///
/// Not `Clone`/`Debug`: a [`RequestSync`](WatchInput::RequestSync) carries a
/// one-shot completion sender, which is neither. Each input is constructed once
/// and moved into a worker channel.
enum WatchInput {
    /// A snapshot is due for the given reason.
    Trigger(Provenance),
    /// A sync cycle is requested. `manual` distinguishes an explicit user/CLI
    /// request (which attempts even a [`Conflicted`](WatchState::Conflicted)
    /// watch, to try resolving it) from an automatic one (interval/push-driven,
    /// suppressed while a conflict latches — "auto-sync stops for that watch").
    RequestSync {
        /// Whether this is an explicit manual request.
        manual: bool,
        /// An optional completion acknowledgement: the worker sends the cycle's
        /// terminal [`SyncOutcome`] here when the requested cycle finishes, so an
        /// in-process caller (the `vard sync` CLI) learns the real result instead
        /// of inferring it from event silence. The sender is dropped without a
        /// value if the worker shuts down before the cycle completes, which the
        /// caller reports honestly as "did not run". `None` for the daemon's
        /// fire-and-forget path.
        ack: Option<oneshot::Sender<SyncOutcome>>,
    },
    /// A signal source reported trouble; the kind and detail are both
    /// surfaced on the bus.
    Trouble {
        /// Distinguishes the signal source dying from every other cause; see
        /// [`TroubleKind`].
        kind: TroubleKind,
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
///
/// The two success arms each carry the pass's quarantined secrets (VRD-22),
/// empty when nothing was withheld. The failure arms carry none: a pass that
/// failed to snapshot asserts its own trouble and never reaches the quarantine
/// reflection, so quarantine yields to the more urgent failure/pause (it
/// re-derives once snapshots resume).
enum PassResult {
    /// A snapshot was committed, and any secrets this pass withheld.
    Committed(SnapshotOutcome, Vec<SecretMatch>),
    /// The sweep committed nothing (a clean tree, or a tree whose only
    /// newly-added changes were quarantined), and any secrets this pass withheld.
    Clean(Vec<SecretMatch>),
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
    /// The op gate was busy (another writer holds this watch's op lock): the
    /// change is un-snapshotted, so a short bounded self-retry is armed to
    /// converge it once the lock frees, without any external trigger (F7). No
    /// state change and no failure event — gate contention is transient, not a
    /// fault. Uses a shorter cadence than the other kinds
    /// ([`EngineConfig::gate_busy_retry_interval`]): our own op lock frees
    /// quickly, unlike a foreign index lock (whose hammering concern keeps the
    /// [`StillLocked`](PassResult::StillLocked) path trigger-driven).
    GateBusy,
}

/// The terminal outcome of a sync cycle, delivered on a request's completion
/// acknowledgement (see [`EngineHandle::request_sync_ack`]).
///
/// This is the source of truth an in-process caller reports from, replacing any
/// inference from event silence: a cycle that did not converge (a busy op gate,
/// a shutdown mid-cycle) drops the sender without a value rather than sending a
/// misleading `UpToDate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The fetch found nothing to pull, the tree was clean, and nothing was
    /// pushed.
    UpToDate,
    /// The cycle moved history: `pushed` counts commits sent to the remote (when
    /// any were), and `pulled` records whether remote commits were integrated.
    Moved {
        /// Commits pushed to the remote, when the push sent any.
        pushed: Option<usize>,
        /// Whether remote commits were pulled in.
        pulled: bool,
    },
    /// A reconcile conflict latched the watch [`Conflicted`](WatchState::Conflicted).
    Conflict,
    /// A network, gate, or reconcile step failed; the message is ready to
    /// surface.
    Failed(String),
    /// The watch cannot sync (sync disabled or no scratch directory); the request
    /// was a no-op.
    Disabled,
    /// The watch is sync-enabled but its repository has no configured remote, so
    /// the live remote gate ([`VcsBackend::has_remote`]) skipped the cycle: an
    /// honest no-op with no state change, no [`SyncError`](WatchState::SyncError),
    /// and no backoff. A remote added later is picked up on the next request.
    /// (The rendered reason is [`sync_no_remote_reason`].)
    NoRemote,
    /// The request never ran: the op gate stayed busy through the shutdown
    /// drain's bounded retries (a foreign or peer holder that did not free in
    /// time). Honest "did not run" with the reason ready to surface — never a
    /// false success, and no failure latch (the gate holder, not the sync, owns
    /// the condition).
    NotRun(String),
}

/// The single wording for "sync-enabled, but the repository does not define the
/// configured remote", naming the remote so the user knows exactly what to add
/// (a repo with only `origin` but `remote = "backup"` configured must be told
/// about `"backup"`). Used by the engine's [`Event::SyncSkipped`] reason and by
/// every host row/refusal for [`SyncOutcome::NoRemote`], so the wording cannot
/// drift between surfaces.
pub fn sync_no_remote_reason(remote: &str) -> String {
    format!("no remote {remote:?} in the repository; add it first")
}

/// What woke a worker's [`run`](Worker::run) loop for one turn. Every variant
/// falls through to the same snapshot/sync dispatch afterwards.
enum Wake {
    /// The input channel closed: the worker should exit.
    Closed,
    /// An input arrived (already applied, with the queued burst drained).
    Input,
    /// The snapshot retry cadence elapsed: run one retry tick.
    SnapshotRetry,
    /// The sync retry deadline came due: fire the pending sync.
    SyncTimer,
}

/// A coalesced pending sync request — the per-watch request-lifecycle record.
///
/// One invariant governs it: **every accepted request terminates in exactly one
/// terminal [`SyncOutcome`] delivered to ALL of its waiters, and every
/// non-terminal deferral leaves an armed wake ([`sync_defer`](Worker::sync_defer))
/// that survives until the request terminates or the drain budget kills it.**
/// Coalescing a second request onto a pending one APPENDS its ack (no waiter is
/// ever dropped, finding 9) and keeps `manual` sticky; the single terminal
/// delivery ([`terminate_sync`](Worker::terminate_sync)) completes every ack.
struct PendingSync {
    /// Whether the most-intentional pending request is manual (an explicit
    /// `vard sync`). Manual bypasses the failure backoff and the live-snapshot
    /// gate; coalescing keeps it sticky (manual wins on intent).
    manual: bool,
    /// Every outstanding waiter's completion acknowledgement. Empty for the
    /// daemon's fire-and-forget path; termination delivers the same outcome to
    /// each.
    acks: Vec<oneshot::Sender<SyncOutcome>>,
    /// Consecutive Abandoned/WouldClobber re-arm attempts for THIS request,
    /// capped at [`SYNC_MAX_ATTEMPTS`] so a continuously-clobbered advance
    /// terminates as a failure instead of looping on the short cadence forever
    /// (finding 6).
    clobber_attempts: u32,
    /// The pre-flight (dirty check + fetch) result a gate-busy deferral cached,
    /// so the Cadence retries reuse it instead of re-fetching on every paced
    /// attempt. Invalidated by fresh local activity ([`Worker::apply`]'s
    /// `Trigger` arm), consumed at most once per cycle attempt (a lost
    /// fast-forward race re-fetches), and expired past
    /// [`SYNC_FETCH_CACHE_TTL_CADENCES`] cadences.
    cached_fetch: Option<CachedFetch>,
}

impl PendingSync {
    /// An automatic (non-manual) request with no waiters — the shape a
    /// post-snapshot enqueue or a backoff/timer re-attempt arms.
    fn auto() -> PendingSync {
        PendingSync {
            manual: false,
            acks: Vec::new(),
            clobber_attempts: 0,
            cached_fetch: None,
        }
    }
}

/// The lock-free pre-flight of one sync cycle — the [`is_dirty`](VcsBackend::is_dirty)
/// answer and the [`fetch`](VcsBackend::fetch) result — cached across gate-busy
/// [`Cadence`](SyncDeferral::Cadence) deferrals so a busy gate costs ONE fetch,
/// not one per paced retry. `at` is the ORIGINAL fetch time (carried through
/// re-arms), so the TTL measures true staleness.
#[derive(Clone, Copy, Debug)]
struct CachedFetch {
    /// Whether the work tree was dirty at pre-flight.
    dirty: bool,
    /// The fetch's remote state at pre-flight.
    remote: crate::vcs::RemoteState,
    /// When the fetch actually ran.
    at: Instant,
}

/// Why (and until when) the pending sync is deferred. The two variants carry
/// the same shape but mean different things, and a MANUAL request is allowed to
/// bypass exactly one of them:
///
/// - [`Backoff`](Self::Backoff) — the exponential failure ramp after a failed
///   cycle. A manual request bypasses it: a user's explicit sync is fresh
///   activity and must not wait out a network-failure ramp (its own attempt
///   surfaces any real problem).
/// - [`Cadence`](Self::Cadence) — the short pacing between gate-busy/clobber
///   re-attempts. EVERYONE respects it, manual included: it exists to keep a
///   wake storm (an editor save-storm triggering snapshot passes) from
///   re-running a full network cycle on every wake and burning the clobber cap
///   within milliseconds.
///
/// Encoding the distinction in the type (rather than one overloaded deadline
/// field) is what makes the bypass rule checkable at the dispatch site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncDeferral {
    /// The exponential failure backoff ([`Worker::sync_backoff`]).
    Backoff(Instant),
    /// The short gate-busy/clobber pacing
    /// ([`EngineConfig::gate_busy_retry_interval`]).
    Cadence(Instant),
}

impl SyncDeferral {
    /// The wake deadline, independent of kind (what the run loop's timer uses).
    fn deadline(self) -> Instant {
        match self {
            SyncDeferral::Backoff(at) | SyncDeferral::Cadence(at) => at,
        }
    }
}

/// The standing sync condition on a worker's separate latch axis: the sync
/// [`WatchState`] (only ever [`Conflicted`](WatchState::Conflicted) or
/// [`SyncError`](WatchState::SyncError)) and the reason recorded with it.
struct SyncLatch {
    state: WatchState,
    reason: Option<String>,
}

/// One per-watch worker: the serialized snapshot loop for a single watch.
struct Worker {
    name: String,
    backend: SharedBackend,
    /// The per-watch operation gate: every commit is admitted (op lock + journal
    /// `begin`) through this before the backend runs and closed through the
    /// returned guard, making one-writer-per-watch a structural invariant. The
    /// standalone default is a no-op ([`default_gate`]); the daemon/CLI inject an
    /// op-lock-backed gate.
    gate: SharedGate,
    mute: MuteSource,
    /// The per-watch secret scanner (VRD-22), compiled once at engine build from
    /// the watch's [`WatchSpec`], injected into every pass's [`SnapshotRequest`]
    /// so newly-added secrets are withheld from the commit. `None` disables
    /// scanning entirely (byte-identical to a plain sweep); a *disabled* scanner
    /// (`secret_scan = false`) is still `Some` but flags nothing.
    scanner: Option<Arc<SecretScanner>>,
    // Kept only to hold the snapshot-interval schedule armed for this worker's
    // lifetime.
    _schedule: Option<ScheduleHandle>,
    // Kept only to hold the pull-driven sync-interval schedule armed for this
    // worker's lifetime. `None` when the watch does not sync, or its
    // `sync_interval` is zero (the timer is disabled).
    _sync_schedule: Option<ScheduleHandle>,
    bus: EventBus,
    cfg: EngineConfig,

    /// The coalesced due snapshot, if any.
    pending: Option<Provenance>,
    /// The last **displayed** state (drives change-only event emission and is
    /// what the shared cell mirrors). It is the projection of the two independent
    /// axes below — `local_state` and `sync_latch` — computed by
    /// [`project`](Self::project): the sync latch surfaces only while the local
    /// state is `Ok`, so a local fault (Paused/Attention) is never hidden by a
    /// standing sync condition and re-surfaces the latch when it clears.
    state: WatchState,
    /// The trouble reported with the displayed `state` — part of the change-only
    /// dedup key: an Attention→Attention transition with a DIFFERENT trouble
    /// (e.g. snapshots-failing → source-died) is a real change subscribers depend
    /// on (the daemon's dead-source rebuild matches the SourceDied event).
    trouble: Option<TroubleKind>,
    /// The watch's **local** lifecycle state — `Ok`, `Paused`, or `Attention` —
    /// owned by the snapshot/trouble path ([`set_state`](Self::set_state)). Never
    /// a sync state; the sync condition rides the separate `sync_latch` axis.
    local_state: WatchState,
    /// The trouble reported with `local_state` (the local latching contract lives
    /// here — a latching [`SourceDied`](TroubleKind::SourceDied) refuses a later
    /// non-latching local transition).
    local_trouble: Option<TroubleKind>,
    /// The reason recorded for the current `local_state`.
    local_reason: Option<String>,
    /// The standing sync condition, if any: [`Conflicted`](WatchState::Conflicted)
    /// (latches until a resolving cycle succeeds) or
    /// [`SyncError`](WatchState::SyncError) (self-clears on the next successful
    /// cycle). A separate axis from the local state so a local-protection failure
    /// is never masked by it; it is displayed only while `local_state` is `Ok`.
    sync_latch: Option<SyncLatch>,
    /// The shared status cell the [`EngineHandle`] reads: mirrors the displayed
    /// `state` (and its trouble/reason/entered_at) on every transition so a host
    /// can query per-watch truth without reconstructing it from the event stream.
    status: Arc<StdMutex<SharedStatus>>,
    /// Set while the worker holds a pending change it could not snapshot and is
    /// converging it on the bounded retry timer, without any external signal.
    /// The kind records how the blocking condition is surfaced.
    retry: Option<RetryKind>,
    /// Consecutive retry ticks since the last activity (bounds self-driving
    /// retry).
    retry_attempts: u32,
    /// Whether the retry budget is spent; the worker then waits for activity.
    retry_exhausted: bool,

    /// Whether this watch may actually sync: [`WatchSpec::sync`] is `true` **and**
    /// a [`scratch_dir`](WatchSpec::scratch_dir) was injected. When `false`, every
    /// sync request is an immediate no-op (see [`run_sync_cycle`](Self::run_sync_cycle)).
    sync_enabled: bool,
    /// The out-of-tree reconcile directory for this watch's [`reconcile`](VcsBackend::reconcile).
    /// `None` disables sync (mirrored in [`sync_enabled`](Self::sync_enabled)).
    scratch_dir: Option<PathBuf>,
    /// The watch's configured remote NAME ([`WatchSpec::remote`]), used to name
    /// the missing remote in [`sync_no_remote_reason`] so the skip reason tells
    /// the user exactly which remote to add.
    remote: String,
    /// The per-watch pending sync request, if any (see [`PendingSync`]): its
    /// origin (manual vs auto), every outstanding waiter's ack, and the
    /// clobber-retry counter. `None` once the request terminates.
    sync_pending: Option<PendingSync>,
    /// Consecutive sync-cycle failures, driving the exponential backoff
    /// ([`sync_backoff`](Self::sync_backoff)); reset to zero on any success.
    sync_failures: u32,
    /// When set, why (and until when) the pending sync is deferred — a failure
    /// [`Backoff`](SyncDeferral::Backoff) a manual request may bypass, or the
    /// short [`Cadence`](SyncDeferral::Cadence) pacing everyone respects. The
    /// run loop wakes on its deadline exactly as it wakes on the snapshot retry
    /// timer.
    sync_defer: Option<SyncDeferral>,
}

impl Worker {
    /// Runs the worker until its input channel closes (every signal source for
    /// this watch was dropped, i.e. the engine is shutting down).
    ///
    /// The loop blocks for the next input, except while a timer is armed — the
    /// snapshot retry timer (a stuck pending change) or the sync retry timer (a
    /// backed-off or gate-deferred sync) — when it also wakes on the nearest
    /// deadline so the operation converges without any further signal. Whatever
    /// wakes it (a fresh input, a snapshot-retry tick, or a sync timer), the loop
    /// then **always** reaches a single snapshot-pass check and a single sync
    /// dispatch ([`dispatch_sync`](Self::dispatch_sync)), so a sync that became
    /// runnable while the worker was busy is never stranded. A snapshot pass and
    /// a sync cycle both run on this one task, so every operation on a watch stays
    /// strictly serial.
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<WatchInput>) {
        self.probe_initial_state().await;
        loop {
            match self.wait_for_work(&mut rx).await {
                Wake::Closed => break,
                Wake::Input => {}
                Wake::SnapshotRetry => self.retry_tick().await,
                Wake::SyncTimer => self.fire_sync_timer(),
            }

            // A pending snapshot runs unless a snapshot retry is armed (its
            // bounded timer, not a fresh input, drives the next attempt so a
            // still-broken repository is not hammered). `retry_tick` already ran
            // the pass on a `SnapshotRetry` wake.
            if self.pending.is_some() && self.retry.is_none() {
                self.run_pass().await;
            }

            // Single sync dispatch, always reached: it decides on its own whether
            // a pending sync may run now (no live snapshot retry, past any backoff
            // deadline) and arms a wake timer when it must defer.
            self.dispatch_sync().await;
        }

        // The input channel closed (shutdown). A sync request with an outstanding
        // waiter may still be mid-retry on the short gate-busy/clobber cadence:
        // keep servicing ONLY that until it terminates, so a request the engine
        // armed a retry for never drops its ack to a false "did not run"
        // (finding 3). The outer shutdown drain budget bounds this — on budget
        // kill the worker is aborted and the ack drops honestly.
        self.drain_pending_sync().await;
    }

    /// Channel-close drain (finding 3): service a still-pending sync request that
    /// has an outstanding waiter to its terminal outcome, so a retry the engine
    /// armed (a transient gate-busy or WouldClobber) is never abandoned mid-flight
    /// with its ack unresolved. Only a request WITH acks obligates the drain — a
    /// fire-and-forget auto request is dropped on shutdown.
    ///
    /// Every drain path is bounded: WouldClobber has its own cap
    /// ([`SYNC_MAX_ATTEMPTS`]), and a persistently busy op gate (a foreign/peer
    /// holder that never frees) gets [`DRAIN_GATE_BUSY_MAX_ATTEMPTS`] paced tries
    /// before the request terminates honestly as [`SyncOutcome::NotRun`] — a
    /// stuck lock must fail the CLI fast, not hang it for the whole outer drain
    /// budget. Gate-busy attempts cost no network I/O (the cycle probes the gate
    /// before its fetch). Snapshots are not serviced — the channel is closed, no
    /// new trigger can arrive.
    async fn drain_pending_sync(&mut self) {
        let mut gate_busy_attempts = 0u32;
        while self
            .sync_pending
            .as_ref()
            .is_some_and(|p| !p.acks.is_empty())
        {
            if let Some(defer) = self.sync_defer {
                tokio::time::sleep(defer.deadline().saturating_duration_since(Instant::now()))
                    .await;
            }
            // Force the pending request to run now, past dispatch's snapshot/backoff
            // guards (there is no snapshot activity during drain).
            self.sync_defer = None;
            let clobbers_before = self.sync_pending.as_ref().map(|p| p.clobber_attempts);
            self.run_sync_cycle().await;
            // A request still pending with an UNCHANGED clobber count deferred on
            // the gate (busy): count it against the drain's own small budget.
            let clobbers_after = self.sync_pending.as_ref().map(|p| p.clobber_attempts);
            if self.sync_pending.is_some() && clobbers_after == clobbers_before {
                gate_busy_attempts += 1;
                if gate_busy_attempts >= DRAIN_GATE_BUSY_MAX_ATTEMPTS
                    && let Some(pending) = self.sync_pending.take()
                {
                    self.sync_defer = None;
                    self.terminate_sync(
                        pending.acks,
                        SyncOutcome::NotRun(
                            "another operation holds the repository; the sync did not run"
                                .to_string(),
                        ),
                    );
                }
            } else {
                gate_busy_attempts = 0;
            }
        }
    }

    /// Waits for the next thing to do, applying an input (and draining the queued
    /// burst) itself when one arrives. Wakes on the nearest of the snapshot retry
    /// cadence and the sync retry deadline, reporting which fired so the caller
    /// can always fall through to the snapshot/sync dispatch afterwards.
    async fn wait_for_work(&mut self, rx: &mut mpsc::UnboundedReceiver<WatchInput>) -> Wake {
        // Snapshot retry cadence (a gate-busy retry frees quickly, so it uses the
        // shorter cadence; every other kind uses the unsafe/failure re-poll).
        let snapshot_interval = if self.retry.is_some() && !self.retry_exhausted {
            Some(match self.retry {
                Some(RetryKind::GateBusy) => self.cfg.gate_busy_retry_interval,
                _ => self.cfg.unsafe_repoll_interval,
            })
        } else {
            None
        };

        // The nearest deadline across both timers, if any.
        let now = Instant::now();
        let mut deadline = self.sync_defer.map(SyncDeferral::deadline);
        if let Some(interval) = snapshot_interval {
            let snap_deadline = now + interval;
            deadline = Some(deadline.map_or(snap_deadline, |d| d.min(snap_deadline)));
        }

        let received = match deadline {
            None => rx.recv().await,
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                match timeout(remaining, rx.recv()).await {
                    Ok(received) => received,
                    Err(_elapsed) => {
                        // A timer fired. Prefer the sync timer when it is the one
                        // that came due; otherwise it is the snapshot retry tick.
                        if self
                            .sync_defer
                            .is_some_and(|defer| Instant::now() >= defer.deadline())
                        {
                            return Wake::SyncTimer;
                        }
                        return Wake::SnapshotRetry;
                    }
                }
            }
        };

        let Some(input) = received else {
            return Wake::Closed;
        };
        self.apply(input);
        // Drain everything else already queued so a burst that arrived while an
        // operation was in flight collapses into one follow-up.
        while let Ok(more) = rx.try_recv() {
            self.apply(more);
        }
        Wake::Input
    }

    /// The sync retry deadline came due: clear it and ensure a request is pending
    /// (as an automatic re-attempt, preserving a manual intent that survived).
    /// The subsequent [`dispatch_sync`](Self::dispatch_sync) runs it.
    fn fire_sync_timer(&mut self) {
        self.sync_defer = None;
        // Ensure a request is pending as an automatic re-attempt. A deferred
        // record (a gate-busy/clobber re-arm still holding its acks and attempt
        // count) survives untouched; a pure backoff wake (record already
        // terminated) arms a fresh auto one.
        self.sync_pending.get_or_insert_with(PendingSync::auto);
    }

    /// The single "may a sync run now?" decision, reached on every loop turn.
    ///
    /// Runs the pending cycle iff there is one, it is not blocked by a *live*
    /// (armed, un-exhausted) snapshot retry, and any deferral deadline has
    /// passed. A pending **manual** request bypasses the live-snapshot-retry
    /// block — an explicit user sync is honored even while snapshots are
    /// retrying, and its own pre-sync snapshot surfaces any real problem. When it
    /// defers it leaves `sync_defer` armed, so the run loop wakes and fires the
    /// sync without any external activity.
    ///
    /// The two deferral kinds are treated differently by origin (the point of
    /// [`SyncDeferral`]): a manual request bypasses a failure
    /// [`Backoff`](SyncDeferral::Backoff) (finding 2 — a user's explicit sync is
    /// fresh activity, same rationale as the manual-resets-exhausted-budget
    /// rule), but EVERY origin respects the short
    /// [`Cadence`](SyncDeferral::Cadence) — otherwise each worker wake during an
    /// editor save-storm would re-run a full network cycle for a deferred manual
    /// request and burn the clobber cap within milliseconds.
    async fn dispatch_sync(&mut self) {
        let Some(manual) = self.sync_pending.as_ref().map(|p| p.manual) else {
            return;
        };
        let live_snapshot_retry = self.retry.is_some() && !self.retry_exhausted;
        if live_snapshot_retry && !manual {
            // An automatic sync waits until local snapshots are healthy again; the
            // pending request is retained and the snapshot retry timer will bring
            // the loop back here once the retry resolves.
            return;
        }
        let now = Instant::now();
        match self.sync_defer {
            // The short pacing binds everyone, manual included.
            Some(SyncDeferral::Cadence(at)) if now < at => return,
            // The failure backoff binds automatic requests only.
            Some(SyncDeferral::Backoff(at)) if !manual && now < at => return,
            _ => {}
        }
        self.run_sync_cycle().await;
    }

    /// Probes the repository's safe state once at worker start and enters the
    /// blocked (unsafe-pause) state immediately if it is unsafe.
    ///
    /// Without this, a daemon restart or engine rebuild would start every watch
    /// at `Ok` and only discover a genuinely blocked repository on the next
    /// trigger — so a restart mid-merge would *amnesia* the block back to healthy
    /// until something happened to write. Entering the state up front keeps the
    /// queryable projection ([`watch_states`](EngineHandle::watch_states)) honest
    /// from the first instant.
    ///
    /// No retry timer is armed and no pending change exists yet: the first real
    /// trigger drives the normal safe re-check that resumes the watch. A probe
    /// error (or panic) at startup is left as `Ok` — no snapshot was attempted,
    /// so there is nothing to fail yet; the first real pass surfaces any genuine
    /// problem.
    async fn probe_initial_state(&mut self) {
        if let Ok(Ok(SafeState::Unsafe(reason))) = check_safe(Arc::clone(&self.backend)).await {
            self.set_state(WatchState::Paused, None, Some(reason.to_string()));
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
                // ...and invalidates a deferred sync's cached pre-flight: the
                // activity changes the dirty answer the cache captured.
                if let Some(pending) = &mut self.sync_pending {
                    pending.cached_fetch = None;
                }
                self.pending = Some(coalesce(self.pending.take(), prov));
            }
            WatchInput::RequestSync { manual, ack } => {
                // A manual request is fresh activity: give a snapshot-retry watch
                // whose budget was spent a new convergence chance (consistent with
                // a `Trigger` reset), so an exhausted snapshot retry does not keep
                // the repository's pre-sync snapshot from landing.
                if manual && self.retry.is_some() && self.retry_exhausted {
                    self.retry_attempts = 0;
                    self.retry_exhausted = false;
                }
                // Coalesce onto any already-pending request: a manual upgrades a
                // pending automatic one (manual wins on intent), and the inbound
                // ack is APPENDED rather than replacing — no earlier waiter is
                // ever dropped (finding 9), and termination completes every ack in
                // the record with the same terminal outcome.
                let pending = self.sync_pending.get_or_insert_with(PendingSync::auto);
                pending.manual |= manual;
                if let Some(ack) = ack {
                    pending.acks.push(ack);
                }
                // A manual request is fresh user activity, so it gets a fresh
                // start on the FAILURE axes: clear a standing failure-backoff
                // deadline (finding 2) and reset a partially-burned clobber
                // budget it coalesced onto (finding 5) — the same rationale as
                // the manual-resets-exhausted-snapshot-budget rule. The short
                // Cadence deferral is deliberately NOT cleared: it is pacing,
                // not a failure, and manual respects it (see `dispatch_sync`).
                if manual {
                    if matches!(self.sync_defer, Some(SyncDeferral::Backoff(_))) {
                        self.sync_defer = None;
                    }
                    pending.clobber_attempts = 0;
                }
            }
            WatchInput::Trouble { kind, detail } => {
                self.set_state(WatchState::Attention, Some(kind), Some(detail));
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

        // Emit `snapshot.quarantined` at most once per run_pass invocation: the
        // post-op re-sweep re-derives the same withheld secrets, but the user
        // sees one logical trigger, so the bus event (and any hook it fires)
        // must not double-fire. The Attention *state* is set idempotently below
        // regardless.
        let mut quarantine_emitted = false;

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

            // The repository is safe and a backend commit is imminent. Admit the
            // operation through the gate FIRST: this acquires the watch's op lock
            // and writes the journal `begin`, making this worker the sole writer
            // for the commit window. `gate.begin` is a non-blocking op-lock attempt
            // plus a small record write, so it runs directly here — synchronous, no
            // `.await` — which also keeps the non-`Sync` `self` from being held
            // across the blocking git work below.
            let guard = match self.gate.begin("snapshot") {
                Ok(Some(guard)) => guard,
                Ok(None) => {
                    // Another holder owns the watch's op lock (a second engine
                    // mid-reload, a CLI restore). Requeue like a contended index
                    // lock — WITHOUT opening a bracket (no `SnapshotStarted`) — and
                    // arm a short bounded self-retry so an event-only watch still
                    // converges once the lock frees, without a fresh trigger (F7).
                    self.pending = Some(coalesce(self.pending.take(), prov));
                    self.enter_gate_busy();
                    return;
                }
                Err(err) => {
                    // The gate itself could not be evaluated (op-lock or journal
                    // I/O trouble), including a fail-closed `begin`-write failure.
                    // Preserve the change and converge it on the bounded retry
                    // timer, surfacing the failure once — the same discipline as a
                    // failed safe-state probe. No bracket was opened.
                    self.pending = Some(prov.clone());
                    self.enter_failure_msg(prov.trigger, format!("operation gate failed: {err}"));
                    return;
                }
            };

            // The commit window is open. Announce it before any git write, so a
            // bus subscriber can bracket it. INVARIANT: every arm below closes this
            // bracket with exactly one outcome event — `SnapshotCompleted`,
            // `SnapshotFailed`, or `SnapshotSkipped` — so a subscriber's record
            // never dangles (see `Event::SnapshotStarted`).
            self.emit_started(prov.trigger);

            // Run the snapshot with the op guard COUPLED to the blocking git work
            // (F2): the guard moves into the blocking scope and is closed there, so
            // an async abort cannot separate lock-release from git-completion. This
            // await borrows no `self` (owned backend/cfg, the local `prov`).
            match run_snapshot_under_guard(
                Arc::clone(&self.backend),
                self.cfg,
                self.scanner.clone(),
                &prov,
                guard,
            )
            .await
            {
                PassResult::Committed(outcome, quarantined) => {
                    self.emit_completed(prov.trigger, &outcome);
                    // A commit means any prior retry has converged.
                    self.clear_retry();
                    // A successful commit clears a snapshots-failing (or blocked)
                    // state — unless this same pass withheld secrets, in which
                    // case the watch rests in Attention/SecretsQuarantined
                    // instead of Ok (VRD-22). One or the other, never a flap.
                    self.reflect_pass(&quarantined, &mut quarantine_emitted);
                    // New local history is worth propagating: fire an automatic
                    // sync so the commit reaches the remote without waiting for a
                    // timer. A no-op unless the watch syncs, and self-suppressed
                    // while Conflicted/latched (see `run_sync_cycle`).
                    self.enqueue_auto_sync();
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
                PassResult::Clean(quarantined) => {
                    // Nothing to commit: the pending change resolved (it was
                    // committed or reverted elsewhere), OR its only newly-added
                    // content was quarantined. Close the started bracket, then
                    // clear any retry so a prior failure episode does not keep
                    // ticking.
                    self.emit_skipped(prov.trigger, SkipReason::Clean);
                    self.clear_retry();
                    // Skip-to-clean clears a snapshots-failing state — unless this
                    // pass withheld secrets, in which case the watch rests in
                    // Attention/SecretsQuarantined. A quarantine-only pass (the
                    // secret is the sole change) commits nothing but must still
                    // assert the trouble (VRD-22).
                    self.reflect_pass(&quarantined, &mut quarantine_emitted);
                }
                PassResult::Unsafe(reason) => {
                    // The repo turned unsafe between the probe and the commit:
                    // no commit was made, so the bracket closes as skipped (the
                    // pause itself travels as `WatchStateChanged`). git wrote
                    // nothing, so the journal bracket closes cleanly.
                    self.emit_skipped(prov.trigger, SkipReason::Unsafe);
                    self.pending = Some(prov);
                    self.enter_unsafe(reason);
                    return;
                }
                PassResult::StillLocked => {
                    // Never delete a foreign lock: requeue and try again on the
                    // next trigger. Nothing was committed, so close the bracket.
                    self.emit_skipped(prov.trigger, SkipReason::LockContended);
                    self.pending = Some(coalesce(self.pending.take(), prov));
                    return;
                }
                PassResult::Failed(err) => {
                    // Preserve the pending change and converge it on the bounded
                    // retry timer instead of dropping a hard failure.
                    // `enter_failure` surfaces `SnapshotFailed` only on the
                    // transition into the failure retry; a repeat inside the
                    // episode closes this pass's bracket as skipped instead
                    // (checked here, before `enter_failure` mutates the retry
                    // state), so the bracket invariant holds without a per-tick
                    // failure storm. A hard git failure cleans up its own lock, so
                    // the journal bracket closes cleanly.
                    self.pending = Some(prov.clone());
                    if self.retry == Some(RetryKind::Failure) {
                        self.emit_skipped(prov.trigger, SkipReason::RetryStillFailing);
                    }
                    self.enter_failure(prov.trigger, &err);
                    return;
                }
                PassResult::Panicked(detail) => {
                    // The blocking commit unwound inside the gated scope: the guard
                    // was already dropped there WITHOUT completing (release-only),
                    // so the journal `begin` stays dangling as recovery evidence
                    // for any abandoned git lock (see the gate module docs). The
                    // `SnapshotStarted` bracket is closed by the `SnapshotFailed`
                    // that `on_backend_panic` emits.
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
    /// with an `Ok` transition. It does not itself clear the retry or touch the
    /// budget — the resolution of the pending change ([`clear_retry`](Self::clear_retry)
    /// on `Committed`/`Clean`) does that. Leaving the budget alone here is what
    /// keeps a repository flapping unsafe↔safe on one bounded episode instead of
    /// refilling the budget every safe edge.
    fn on_safe(&mut self) {
        if self.retry == Some(RetryKind::UnsafePause) {
            self.set_state(
                WatchState::Ok,
                None,
                Some("repository returned to a safe state".into()),
            );
        }
    }

    /// Clears the whole retry state. Called exactly when the pending change
    /// resolves — it committed, the tree came back clean, or it was abandoned by
    /// a backend panic — which is the single place the retry lifecycle ends.
    fn clear_retry(&mut self) {
        self.retry = None;
        self.retry_attempts = 0;
        self.retry_exhausted = false;
    }

    /// Arms (or keeps) the bounded retry for `kind`, the single seam every
    /// `enter_*` retry entry point flows through.
    ///
    /// The attempt budget is measured in *ticks*, and the two retry cadences
    /// count time very differently — the short [`GateBusy`](RetryKind::GateBusy)
    /// cadence ([`gate_busy_retry_interval`](EngineConfig::gate_busy_retry_interval))
    /// versus the long unsafe/failure cadence
    /// ([`unsafe_repoll_interval`](EngineConfig::unsafe_repoll_interval)). So a
    /// change of *cadence class* (GateBusy ↔ the unsafe/failure kinds) invalidates
    /// the accumulated count and starts a **fresh** budget: a spent Failure/Unsafe
    /// episode must not starve a following GateBusy window (R2), and — the mirror
    /// case that makes evidence-pending converge — a short GateBusy episode's tiny
    /// budget must not starve a following long-cadence retry. A transition *within*
    /// a class (unsafe ↔ failure, both long-cadence) keeps the count, so one
    /// flapping episode stays bounded on a single budget rather than refilling on
    /// every edge. The count is otherwise reset only by fresh activity
    /// ([`apply`](Self::apply)) or by [`clear_retry`](Self::clear_retry) when the
    /// change resolves.
    fn arm_retry(&mut self, kind: RetryKind) {
        let was_gate_busy = self.retry == Some(RetryKind::GateBusy);
        let now_gate_busy = kind == RetryKind::GateBusy;
        if was_gate_busy != now_gate_busy {
            self.retry_attempts = 0;
            self.retry_exhausted = false;
        }
        self.retry = Some(kind);
    }

    /// Enters (or stays in) the unsafe-paused retry, arming the bounded timer via
    /// [`arm_retry`](Self::arm_retry): the budget carries across the whole stuck
    /// episode (including an unsafe ↔ failure flap, same cadence class) and is
    /// reset only on a cadence-class change, by fresh activity
    /// ([`apply`](Self::apply)), or by [`clear_retry`](Self::clear_retry).
    /// `set_state` is idempotent, so a re-poll that still finds the repo unsafe
    /// does not re-emit `Paused`.
    fn enter_unsafe(&mut self, reason: UnsafeReason) {
        self.arm_retry(RetryKind::UnsafePause);
        self.set_state(WatchState::Paused, None, Some(reason.to_string()));
    }

    /// Arms the short bounded op-gate-busy self-retry (F7). Unlike
    /// [`enter_unsafe`](Self::enter_unsafe)/[`enter_failure`](Self::enter_failure)
    /// it makes **no** state transition and emits **no** event: op-gate
    /// contention is transient (a peer's commit window on our own op lock), not a
    /// fault, so the watch stays whatever it was. It only arms the retry timer so
    /// the preserved change converges once the lock frees, without a fresh
    /// trigger. Arms through [`arm_retry`](Self::arm_retry), which starts a FRESH
    /// budget because GateBusy is a different cadence class from the unsafe/failure
    /// kinds: a spent Failure/Unsafe episode must not starve this short-cadence
    /// window (R2). Within a GateBusy episode the count carries as usual (reset
    /// only by fresh activity in [`apply`](Self::apply) or [`clear_retry`](Self::clear_retry)).
    fn enter_gate_busy(&mut self) {
        self.arm_retry(RetryKind::GateBusy);
    }

    /// Enters (or stays in) the failure retry after a probe or snapshot failed.
    /// Emits [`Event::SnapshotFailed`] on any transition into the failure retry
    /// — including from an unsafe pause — but not while already in it, so a
    /// genuine new failure is surfaced once and per-tick retries do not storm.
    /// Arms through [`arm_retry`](Self::arm_retry) (same cadence class as
    /// [`enter_unsafe`](Self::enter_unsafe), so an unsafe ↔ failure flap keeps one
    /// budget; a GateBusy → failure transition starts a fresh one).
    ///
    /// It also moves the watch to [`WatchState::Attention`] with
    /// [`TroubleKind::SnapshotsFailing`], carrying the error as the reason, so
    /// the failing snapshot is a *queryable state* (see
    /// [`watch_states`](EngineHandle::watch_states)) — not merely a one-off
    /// event a health projection could miss. `set_state` is idempotent, so a
    /// repeat failure inside the episode does not re-emit
    /// [`Event::WatchStateChanged`].
    fn enter_failure(&mut self, trigger: Trigger, err: &VcsError) {
        self.enter_failure_msg(trigger, err.to_string());
    }

    /// [`enter_failure`](Self::enter_failure) for a failure whose message is not
    /// a [`VcsError`] — a gate-evaluation failure (op-lock/journal I/O trouble)
    /// that has no backend error to carry. Same once-per-episode surfacing and
    /// snapshots-failing state.
    fn enter_failure_msg(&mut self, trigger: Trigger, msg: String) {
        if self.retry != Some(RetryKind::Failure) {
            self.emit_failed_error(trigger, msg.clone());
        }
        self.arm_retry(RetryKind::Failure);
        self.set_state(
            WatchState::Attention,
            Some(TroubleKind::SnapshotsFailing),
            Some(msg),
        );
    }

    /// Returns the watch to [`WatchState::Ok`] after a pass proves it healthy — a
    /// committed snapshot or a skip-to-clean. Idempotent via `set_state`, so a
    /// watch already `Ok` (the common case) emits nothing.
    ///
    /// A watch parked in `Attention` on a **latching** ([`TroubleKind::latches`])
    /// trouble kind is left alone: `set_state` itself refuses the transition
    /// (see its docs), since a successful pass proving the *mechanism*
    /// recovered proves nothing about a latching kind's own condition. Every
    /// other case — healthy already, `Paused`, or `Attention` on a
    /// self-clearing kind — recovers to `Ok` here.
    fn recover_to_ok(&mut self) {
        self.set_state(
            WatchState::Ok,
            None,
            Some("snapshots are succeeding".into()),
        );
    }

    /// Reflects a completed (committed or clean) pass's quarantine result into
    /// watch state (VRD-22). With nothing withheld, the pass proves the watch
    /// healthy and it returns to `Ok` ([`recover_to_ok`](Self::recover_to_ok)).
    /// With secrets withheld, the watch rests in
    /// [`WatchState::Attention`]/[`TroubleKind::SecretsQuarantined`] carrying
    /// this pass's count, and — at most once per `run_pass`, guarded by
    /// `emitted` — emits [`Event::SnapshotQuarantined`].
    ///
    /// This is the single branch between "healthy again" and "still withholding"
    /// so the two never flap: a quarantining pass never calls `recover_to_ok`
    /// (which would emit an `Ok` transition only to be overwritten by the
    /// Attention one). `set_state` is idempotent, so the post-op re-sweep's
    /// identical result re-affirms the state silently. Reached only from the
    /// success arms, so a failing/paused pass's own (more urgent) trouble wins —
    /// quarantine re-derives once snapshots resume.
    fn reflect_pass(&mut self, quarantined: &[SecretMatch], emitted: &mut bool) {
        if quarantined.is_empty() {
            self.recover_to_ok();
            return;
        }
        let count = quarantined.len();
        if !*emitted {
            self.emit_quarantined(count);
            *emitted = true;
        }
        let reason = match count {
            1 => "1 newly-added file withheld as a likely secret".to_string(),
            n => format!("{n} newly-added files withheld as likely secrets"),
        };
        self.set_state(
            WatchState::Attention,
            Some(TroubleKind::SecretsQuarantined { count }),
            Some(reason),
        );
    }

    /// Emits [`Event::SnapshotQuarantined`] for a pass that withheld `count`
    /// files. Count-only: never the paths or any secret bytes.
    fn emit_quarantined(&self, count: usize) {
        self.bus.emit(Event::SnapshotQuarantined {
            watch: self.name.clone(),
            count,
        });
    }

    /// Surfaces a panicked backend call: emits [`Event::SnapshotFailed`], moves
    /// the watch to [`WatchState::Attention`], and clears the retry (the pending
    /// change was already dropped by the panicking pass), so the worker stays
    /// alive to process later inputs without a zombie retry burning the budget.
    fn on_backend_panic(&mut self, trigger: Trigger, detail: &str) {
        let msg = format!("backend task panicked: {detail}");
        self.emit_failed_error(trigger, msg.clone());
        // A panicked backend call is not the watcher/scheduler signal source
        // dying — the worker itself survives it (see the module docs) — so
        // it is `Degraded`, not `SourceDied`.
        self.set_state(
            WatchState::Attention,
            Some(TroubleKind::Degraded),
            Some(msg),
        );
        self.clear_retry();
    }

    /// Emits [`Event::SnapshotStarted`] just before a backend commit is
    /// attempted (see [`run_pass`](Self::run_pass)).
    fn emit_started(&self, trigger: Trigger) {
        self.bus.emit(Event::SnapshotStarted {
            watch: self.name.clone(),
            trigger,
        });
    }

    /// Emits [`Event::SnapshotSkipped`]: the no-commit outcome that closes a
    /// [`SnapshotStarted`](Event::SnapshotStarted) bracket when neither a
    /// completion nor a (newly surfaced) failure applies.
    fn emit_skipped(&self, trigger: Trigger, reason: SkipReason) {
        self.bus.emit(Event::SnapshotSkipped {
            watch: self.name.clone(),
            trigger,
            reason,
        });
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

    /// Emits [`Event::SnapshotFailed`] with a raw error message (for conditions
    /// that are not a [`VcsError`], such as a panicked backend task).
    fn emit_failed_error(&self, trigger: Trigger, error: String) {
        self.bus.emit(Event::SnapshotFailed {
            watch: self.name.clone(),
            trigger,
            error,
        });
    }

    /// Records a **local** lifecycle transition (`Ok`/`Paused`/`Attention`) on
    /// the local-state axis, then re-projects the displayed state
    /// ([`refresh_display`](Self::refresh_display)). `trouble` is `Some` only for
    /// a transition caused by trouble (a `Trouble` signal or a backend panic);
    /// every other transition (a resolved-safe `Ok`, an unsafe-repo `Paused`)
    /// passes `None`.
    ///
    /// The **local latching contract** lives here: once the current local trouble
    /// latches ([`TroubleKind::latches`]), a non-latching local transition is
    /// refused, no matter which caller drives it — an unsafe pause, a fresh
    /// failure, a panic, or a snapshot recovery — so a latching condition
    /// (currently only [`SourceDied`](TroubleKind::SourceDied)) is never silently
    /// clobbered. The exception is an incoming **latching trouble**, which always
    /// records, so the daemon can still rebuild a watch whose source died.
    ///
    /// Unlike the old design, this never blocks on the *sync* condition: the sync
    /// latch is a separate axis ([`sync_latch`](Self::sync_latch)) and only
    /// overlays the display while the local state is `Ok`. A snapshot commit's
    /// [`recover_to_ok`](Self::recover_to_ok) therefore always returns the local
    /// state to `Ok`, and a standing conflict/sync-error simply re-surfaces in the
    /// projection — a successful commit still says nothing about remote
    /// reachability or an unresolved conflict.
    fn set_state(&mut self, to: WatchState, trouble: Option<TroubleKind>, reason: Option<String>) {
        if self.local_state == to && self.local_trouble == trouble {
            self.local_reason = reason;
            self.refresh_display();
            return;
        }
        // A latching trouble (SourceDied) always records; every other local
        // transition yields to a latched local trouble.
        let incoming_latches = trouble.is_some_and(TroubleKind::latches);
        if !incoming_latches && self.local_trouble.is_some_and(TroubleKind::latches) {
            return;
        }
        self.local_state = to;
        self.local_trouble = trouble;
        self.local_reason = reason;
        self.refresh_display();
    }

    /// Projects the displayed `(state, trouble, reason)` from the two axes: the
    /// sync latch surfaces only while the local state is `Ok`, so a local fault is
    /// never hidden and the latch re-appears the moment local trouble clears.
    fn project(&self) -> (WatchState, Option<TroubleKind>, Option<String>) {
        if self.local_state == WatchState::Ok
            && let Some(latch) = &self.sync_latch
        {
            return (latch.state, None, latch.reason.clone());
        }
        (
            self.local_state,
            self.local_trouble,
            self.local_reason.clone(),
        )
    }

    /// Recomputes the displayed state from the local-state and sync-latch axes and
    /// commits it: emits [`Event::WatchStateChanged`] only on an actual
    /// `(state, trouble)` change, and otherwise refreshes only the shared cell's
    /// reason (so queries stay accurate without bumping `entered_at` or emitting).
    /// The single choke point every axis change funnels through, so
    /// `watch.state_changed` events and the health projection stay consistent.
    fn refresh_display(&mut self) {
        let (to, trouble, reason) = self.project();
        if self.state == to && self.trouble == trouble {
            let mut shared = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            if shared.reason != reason {
                shared.reason = reason;
            }
            return;
        }
        let from = self.state;
        self.state = to;
        self.trouble = trouble;
        // Mirror the transition into the shared cell the handle reads, stamping a
        // fresh `entered_at`. Held only for this update; recover a poisoned lock
        // rather than propagate a panic into an unrelated worker.
        {
            let mut shared = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            shared.state = to;
            shared.trouble = trouble;
            shared.reason = reason.clone();
            shared.entered_at = SystemTime::now();
        }
        self.bus.emit(Event::WatchStateChanged {
            watch: self.name.clone(),
            from,
            to,
            reason,
            trouble,
        });
    }

    /// Sets (or clears) the sync latch axis and re-projects the display. Clearing
    /// (`None`) is what a successful cycle does — it only touches the sync axis,
    /// never the local state.
    fn set_sync_latch(&mut self, latch: Option<SyncLatch>) {
        self.sync_latch = latch;
        self.refresh_display();
    }

    /// Drives one full sync cycle for a coalesced request, on the worker's
    /// serialized loop so it never overlaps a snapshot on the same watch.
    ///
    /// The network (fetch, push) runs **outside** the op lock and the
    /// reconcile+advance run **inside** it, exactly as the module's
    /// lock/network separation requires. `fetch → reconcile → advance → push`
    /// re-runs in-cycle up to [`SYNC_MAX_ATTEMPTS`] when the push loses a
    /// fast-forward race; on a conflict the watch latches
    /// [`Conflicted`](WatchState::Conflicted); on a network failure it enters
    /// [`SyncError`](WatchState::SyncError) with exponential backoff.
    /// Enqueues an **automatic** sync request for this watch (the post-snapshot
    /// trigger). A no-op on a watch that cannot sync. Coalesces with any pending
    /// request without downgrading a manual one to automatic — manual wins on
    /// intent, exactly as [`apply`](Self::apply) coalesces an inbound request.
    /// The [`run`](Self::run) loop runs the enqueued cycle once the pass returns.
    fn enqueue_auto_sync(&mut self) {
        if !self.sync_enabled {
            return;
        }
        // Coalesce into any pending request, preserving its origin, waiters, and
        // clobber count (an automatic enqueue adds none of its own). A no-op on
        // an existing record; arms a fresh auto one when none is pending.
        self.sync_pending.get_or_insert_with(PendingSync::auto);
    }

    async fn run_sync_cycle(&mut self) {
        let Some(mut pending) = self.sync_pending.take() else {
            return;
        };
        // A watch that cannot sync (sync disabled, or no scratch directory to
        // reconcile in) drops the request — every waiter is told `Disabled`.
        if !self.sync_enabled {
            self.terminate_sync(pending.acks, SyncOutcome::Disabled);
            return;
        }
        // The remote gate is LIVE and lives HERE, not at scratch injection
        // (findings 4/5): probe the configured remote at cycle start (a cheap,
        // non-network config lookup). A missing remote is an honest no-op — every
        // waiter is told `NoRemote`, one event is emitted, and NOTHING latches
        // (no SyncError, no backoff, no state change) — so a remote added after
        // the daemon started is picked up on the next request with no restart,
        // and a remote-less watch never storms doomed fetches.
        //
        // A probe ERROR is a different animal: `git config` failing to read is a
        // real repository/environment fault, not a missing remote. Masking it as
        // `NoRemote` would render the watch "disabled" and hide the fault, so it
        // terminates as a failure (SyncFailed + SyncError latch + backoff),
        // exactly like any other broken step.
        match sync_has_remote(Arc::clone(&self.backend)).await {
            Ok(true) => {}
            Ok(false) => {
                self.emit_sync_skipped(&sync_no_remote_reason(&self.remote));
                self.terminate_sync(pending.acks, SyncOutcome::NoRemote);
                return;
            }
            Err(error) => {
                self.enter_sync_error(error.clone());
                self.terminate_sync(pending.acks, SyncOutcome::Failed(error));
                return;
            }
        }
        // Auto-sync stops while a conflict latches: only an explicit manual
        // request re-attempts, to pick up a resolution the user just made. (A
        // manual request always carries the ack, so an auto-suppressed request
        // never strands a waiter.)
        if matches!(&self.sync_latch, Some(l) if l.state == WatchState::Conflicted)
            && !pending.manual
        {
            self.terminate_sync(pending.acks, SyncOutcome::Conflict);
            return;
        }
        let Some(scratch) = self.scratch_dir.clone() else {
            self.terminate_sync(pending.acks, SyncOutcome::Disabled);
            return;
        };

        // The pre-flight cache from a prior gate-busy deferral: consumed by the
        // FIRST attempt only (a lost fast-forward race must re-fetch against the
        // moved remote). Freshness is NOT judged here — the TTL is consulted at
        // gate ACQUISITION inside the cycle, so the busy-wait itself never costs
        // network I/O no matter how long it lasts (see `sync_once`).
        let mut cached = pending.cached_fetch.take();

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.sync_once(&scratch, cached.take()).await {
                SyncStep::NothingToDo => {
                    self.sync_succeeded();
                    self.terminate_sync(pending.acks, SyncOutcome::UpToDate);
                    return;
                }
                SyncStep::Done { pushed, pulled } => {
                    self.sync_succeeded();
                    let outcome = if pushed.is_some() || pulled {
                        SyncOutcome::Moved { pushed, pulled }
                    } else {
                        SyncOutcome::UpToDate
                    };
                    self.terminate_sync(pending.acks, outcome);
                    return;
                }
                SyncStep::Conflict => {
                    self.enter_conflict();
                    self.terminate_sync(pending.acks, SyncOutcome::Conflict);
                    return;
                }
                SyncStep::RaceLost => {
                    if attempt >= SYNC_MAX_ATTEMPTS {
                        let msg =
                            "push kept losing a fast-forward race to a moving remote".to_string();
                        self.enter_sync_error(msg.clone());
                        self.terminate_sync(pending.acks, SyncOutcome::Failed(msg));
                        return;
                    }
                    // Loop: re-fetch and reconcile against the newly-moved remote.
                }
                SyncStep::GateBusy(fetched) => {
                    // Another writer holds the op lock (a CLI restore, a second
                    // engine mid-reload). Preserve the request (with EVERY ack) and
                    // re-attempt on the short gate-busy cadence, no state change —
                    // contention is transient, not a sync fault, and NOT a clobber
                    // attempt (the cap counts only genuine WouldClobber refusals).
                    // The pre-flight result is cached on the record so the paced
                    // retries reuse it instead of re-fetching (its `at` carries the
                    // original fetch time, so the TTL measures true staleness).
                    pending.cached_fetch = Some(fetched);
                    self.sync_defer = Some(SyncDeferral::Cadence(
                        Instant::now() + self.cfg.gate_busy_retry_interval,
                    ));
                    self.sync_pending = Some(pending);
                    return;
                }
                SyncStep::Abandoned => {
                    // The advance refused to overwrite uncommitted/unmerged work,
                    // an untracked file, or a locally-gitignored file a remote
                    // change would clobber (WouldClobber). Retry on the short
                    // cadence so the next cycle's pre-sync snapshot commits the new
                    // work and reconciles it — but BOUNDED (finding 6): a
                    // continuously-rewritten path would otherwise loop forever. On
                    // cap exhaustion terminate as a real failure (SyncFailed +
                    // SyncError latch + normal exponential backoff).
                    pending.clobber_attempts += 1;
                    if pending.clobber_attempts >= SYNC_MAX_ATTEMPTS {
                        let msg = "sync kept refusing to overwrite local work that a remote \
                                   change would clobber"
                            .to_string();
                        self.enter_sync_error(msg.clone());
                        self.terminate_sync(pending.acks, SyncOutcome::Failed(msg));
                        return;
                    }
                    self.sync_defer = Some(SyncDeferral::Cadence(
                        Instant::now() + self.cfg.gate_busy_retry_interval,
                    ));
                    self.sync_pending = Some(pending);
                    return;
                }
                SyncStep::Failed(error) => {
                    self.enter_sync_error(error.clone());
                    self.terminate_sync(pending.acks, SyncOutcome::Failed(error));
                    return;
                }
            }
        }
    }

    /// The single terminal delivery point (the request-lifecycle choke): sends
    /// `outcome` to EVERY outstanding waiter of a request. A request with no
    /// waiters (the daemon's fire-and-forget path) terminates silently. Closed
    /// receivers (a caller that gave up) are ignored.
    fn terminate_sync(&self, acks: Vec<oneshot::Sender<SyncOutcome>>, outcome: SyncOutcome) {
        for tx in acks {
            let _ = tx.send(outcome.clone());
        }
    }

    /// One `pre-flight → [locked window] → push` pass. Emits
    /// [`Event::SyncPulled`]/[`Event::SyncPushed`] for what actually moved and
    /// returns the [`SyncStep`] the caller folds into state and retry decisions.
    ///
    /// Three properties hold simultaneously (each pinned by a test):
    ///
    /// - **The op gate is touched only when locked work is actually needed** (a
    ///   dirty tree to snapshot, or remote commits to integrate). The pre-flight
    ///   — fetch, then the dirty check — is lock-free, so a clean, up-to-date
    ///   watch reports `UpToDate` and a clean, ahead-only watch pushes, both
    ///   with the gate never probed: a foreign/peer op-lock holder cannot fail
    ///   work that needs no lock.
    /// - **Waiting on a busy gate costs zero network I/O.** A retry arriving
    ///   with a `cached` pre-flight probes the gate FIRST and, while it stays
    ///   busy, returns the cache untouched (no network, no subprocesses). The
    ///   freshness TTL is consulted only at gate ACQUISITION: still fresh → the
    ///   cycle proceeds on the cache; stale → exactly one fresh pre-flight,
    ///   then proceed.
    /// - **The early exits cannot miss an edit saved during the fetch.** The
    ///   dirty check runs AFTER the fetch returns, so its answer postdates the
    ///   whole network window; the locked window's own pre-sync snapshot remains
    ///   the authoritative capture on every locked path.
    async fn sync_once(
        &mut self,
        scratch: &std::path::Path,
        cached: Option<CachedFetch>,
    ) -> SyncStep {
        // 0. Waiting phase: a retry carrying a cached pre-flight exists only
        //    because locked work was needed and the gate was busy. Probe the
        //    gate BEFORE anything else — while it stays busy the retry is pure
        //    lock-probing (zero network AND zero subprocesses), no matter how
        //    long the holder wedges. DECISION: the wait does NOT re-evaluate
        //    whether the work still exists — a user who hand-resolves mid-wait
        //    still gets the bounded wait's honest "did not run" and re-runs.
        //    A wedged gate is an abnormal state; rare-edge honesty beats
        //    rare-edge cleverness (a local re-evaluation could report a false
        //    "up to date" against an arbitrarily stale tracking ref). Only once
        //    the gate looks free is the cache's freshness judged: a fresh cache
        //    short-circuits the pre-flight below; a stale one is dropped and
        //    replaced by exactly one fresh pre-flight.
        let cached = match cached {
            Some(c) => {
                if !self.gate.available() {
                    return SyncStep::GateBusy(c);
                }
                let ttl = self.cfg.gate_busy_retry_interval * SYNC_FETCH_CACHE_TTL_CADENCES;
                (c.at.elapsed() <= ttl).then_some(c)
            }
            None => None,
        };

        // 1. Pre-flight — LOCK-FREE (neither reads under the op lock). First a
        //    fast-fail dirty probe: a repository whose status cannot even be
        //    read (a corrupt index) must fail BEFORE the network fetch, not
        //    after paying for one on every backoff retry. Then the
        //    timeout-bounded fetch, then the AUTHORITATIVE dirty check — that
        //    order is load-bearing: the early exits below trust `!dirty`, so
        //    the deciding answer must postdate the fetch, and an edit saved
        //    while the fetch was in flight is seen, never skipped as a false
        //    "up to date".
        let (dirty, remote, fetched_at) =
            match cached {
                Some(c) => (c.dirty, c.remote, c.at),
                None => {
                    if let Err(error) = sync_is_dirty(Arc::clone(&self.backend)).await {
                        return SyncStep::Failed(error);
                    }
                    let remote =
                        match sync_fetch(Arc::clone(&self.backend), self.cfg.sync_network_timeout)
                            .await
                        {
                            Ok(remote) => remote,
                            Err(error) => return SyncStep::Failed(error),
                        };
                    let dirty = match sync_is_dirty(Arc::clone(&self.backend)).await {
                        Ok(dirty) => dirty,
                        Err(error) => return SyncStep::Failed(error),
                    };
                    (dirty, remote, Instant::now())
                }
            };

        // 2. Gate-free fast paths: with a clean tree and nothing to integrate
        //    there is no locked work at all. Nothing local and nothing remote is
        //    the whole job done; local commits with an unmoved remote push
        //    directly (the tree is untouched, so no lock and no journal bracket
        //    are needed). Only a FRESH pre-flight reaches these arms: a cached
        //    one exists precisely because locked work was needed.
        if !dirty && remote.behind == 0 {
            if remote.ahead == 0 {
                return SyncStep::NothingToDo;
            }
            // Count what this push actually sends at PUSH time when the backend
            // can (commits may have landed since the fetch); fall back to the
            // fetch-time count. The one exception is a branch deleted remotely
            // with a stale tracking ref surviving: the local read is then
            // untrustworthy, and the fetch-time count (the full history this
            // push recreates) is the truth. A never-pushed branch has no
            // tracking ref, so its push-time read is trustworthy as usual.
            let pushed = if remote.stale_tracking_ref {
                remote.ahead
            } else {
                sync_ahead_of_upstream(Arc::clone(&self.backend))
                    .await
                    .unwrap_or(remote.ahead)
            };
            let tip = sync_current_tip(Arc::clone(&self.backend)).await;
            return self.push_step(tip, pushed, false).await;
        }

        // 3. Locked work is needed (a dirty tree to snapshot, or remote commits
        //    to integrate): NOW probe the gate. A busy gate defers the cycle on
        //    the short cadence, carrying the pre-flight result; the paced
        //    retries then wait network-free (step 0). The probe is advisory: a
        //    holder arriving between it and the real `begin` below simply
        //    surfaces as another GateBusy defer there.
        let fetched = CachedFetch {
            dirty,
            remote,
            at: fetched_at,
        };
        if !self.gate.available() {
            return SyncStep::GateBusy(fetched);
        }

        // 4. Locked window — op lock + one journal bracket, ZERO network I/O:
        //    pre-sync snapshot → reconcile → advance, under the self-suppression
        //    mute so the advance's tree rewrite does not feed back as activity.
        //
        //    Reconcile+advance run ONLY when the fetch found remote commits to
        //    integrate (`behind > 0`). With nothing new remotely there is nothing
        //    to rebase onto or advance to, so the window is just the pre-sync
        //    snapshot — this skips pointless scratch-worktree work on the common
        //    push-only path AND makes the first push of a never-pushed branch
        //    work at all: its upstream tracking ref does not exist yet (fetch
        //    reports that as the normal not-moved/behind-0 state), and a rebase
        //    onto the nonexistent ref would error out the whole cycle.
        let guard = match self.gate.begin("sync") {
            Ok(Some(guard)) => guard,
            Ok(None) => return SyncStep::GateBusy(fetched),
            Err(err) => return SyncStep::Failed(format!("operation gate failed: {err}")),
        };
        let locked = {
            let _mute = self.mute.acquire();
            // `behind == 0`: nothing to integrate, run the push-only window.
            let scratch = (remote.behind > 0).then(|| scratch.to_path_buf());
            run_locked_window(
                Arc::clone(&self.backend),
                self.scanner.clone(),
                scratch,
                guard,
            )
            .await
        };
        let (tip, presync_committed, pulled_moved) = match locked {
            LockedResult::Reconciled {
                pulled,
                tip,
                presync_committed,
            } => {
                let pulled_moved = pulled.is_some();
                if let Some((prev, new)) = pulled {
                    self.emit_sync_pulled(prev, new);
                }
                (tip, presync_committed, pulled_moved)
            }
            LockedResult::Conflict => return SyncStep::Conflict,
            LockedResult::Abandoned => return SyncStep::Abandoned,
            LockedResult::Failed(error) => return SyncStep::Failed(error),
        };

        // 5. Push. What the remote receives is the commits the fetch found us
        //    ahead by PLUS the pre-sync snapshot commit this window just made
        //    (if any) — the latter was uncounted when `ahead` was captured at
        //    fetch time (before the pre-sync snapshot existed).
        let pushed = remote.ahead + usize::from(presync_committed);
        self.push_step(tip, pushed, pulled_moved).await
    }

    /// The push step shared by the gate-free fast path and the post-window path:
    /// OUTSIDE the op lock, timeout-bounded, emitting [`Event::SyncPushed`] for
    /// what the remote actually received.
    async fn push_step(&mut self, tip: String, pushed: usize, pulled_moved: bool) -> SyncStep {
        match sync_push(Arc::clone(&self.backend), self.cfg.sync_network_timeout).await {
            Ok(PushOutcome::Pushed) => {
                self.emit_sync_pushed(tip, pushed);
                SyncStep::Done {
                    pushed: Some(pushed),
                    pulled: pulled_moved,
                }
            }
            Ok(PushOutcome::UpToDate) => SyncStep::Done {
                pushed: None,
                pulled: pulled_moved,
            },
            Ok(PushOutcome::NonFastForward) => SyncStep::RaceLost,
            Err(error) => SyncStep::Failed(error),
        }
    }

    /// Latches the watch [`Conflicted`](WatchState::Conflicted) on the sync axis:
    /// a reconcile hit a conflict the user must resolve. Auto-sync stops (no
    /// backoff re-enqueue); only a manual request re-attempts. Emits
    /// [`Event::SyncConflict`]. The latch is displayed while the local state is
    /// `Ok`; a concurrent local fault takes visual precedence and the conflict
    /// re-surfaces when it clears.
    fn enter_conflict(&mut self) {
        self.bus.emit(Event::SyncConflict {
            watch: self.name.clone(),
        });
        self.sync_failures = 0;
        self.sync_defer = None;
        self.set_sync_latch(Some(SyncLatch {
            state: WatchState::Conflicted,
            reason: Some("a sync conflict needs resolution".into()),
        }));
    }

    /// Latches [`SyncError`](WatchState::SyncError) on the sync axis after a
    /// network/gate failure and schedules an exponential-backoff re-attempt.
    /// Emits [`Event::SyncFailed`]. Self-clearing: the next successful cycle
    /// clears the latch (see [`sync_succeeded`](Self::sync_succeeded)).
    ///
    /// **Conflicted takes precedence (finding 7).** A sync failure NEVER
    /// downgrades a standing [`Conflicted`](WatchState::Conflicted) latch: the
    /// failure is reported ([`Event::SyncFailed`] above, and the failing cycle's
    /// ack) but the conflict stays latched and keeps suppressing auto-sync — no
    /// `SyncError` latch, no backoff re-attempt armed against an unresolved
    /// conflict. So a network error while conflicted leaves the conflict standing
    /// and reported rather than flipping to a self-clearing sync-error that would
    /// lift the suppression and cycle a backoff.
    fn enter_sync_error(&mut self, error: String) {
        self.bus.emit(Event::SyncFailed {
            watch: self.name.clone(),
            error: error.clone(),
        });
        if matches!(&self.sync_latch, Some(l) if l.state == WatchState::Conflicted) {
            return;
        }
        self.sync_failures = self.sync_failures.saturating_add(1);
        self.sync_defer = Some(SyncDeferral::Backoff(Instant::now() + self.sync_backoff()));
        self.set_sync_latch(Some(SyncLatch {
            state: WatchState::SyncError,
            reason: Some(error),
        }));
    }

    /// Records a successful cycle: resets the failure backoff and clears the sync
    /// latch (a standing [`Conflicted`](WatchState::Conflicted)/[`SyncError`](WatchState::SyncError)).
    /// It never touches the local state, so an unrelated local `Attention`/`Paused`
    /// is left exactly as it was (the projection simply stops overlaying the
    /// cleared latch). A no-op when there was no latch.
    fn sync_succeeded(&mut self) {
        self.sync_failures = 0;
        self.sync_defer = None;
        self.set_sync_latch(None);
    }

    /// The backoff before the next sync re-attempt: `base * 2^(failures-1)`,
    /// capped at [`EngineConfig::sync_backoff_cap`]. `failures` is at least one
    /// here (this is only consulted after a failure).
    fn sync_backoff(&self) -> Duration {
        let shift = self.sync_failures.saturating_sub(1).min(20);
        let factor = 2u32.saturating_pow(shift);
        self.cfg
            .sync_backoff_base
            .checked_mul(factor)
            .unwrap_or(self.cfg.sync_backoff_cap)
            .min(self.cfg.sync_backoff_cap)
    }

    /// Emits [`Event::SyncPulled`] for an advance that integrated remote changes.
    fn emit_sync_pulled(&self, prev_ref: String, new_ref: String) {
        self.bus.emit(Event::SyncPulled {
            watch: self.name.clone(),
            prev_ref,
            new_ref,
        });
    }

    /// Emits [`Event::SyncPushed`] for commits the remote received.
    fn emit_sync_pushed(&self, new_ref: String, commits: usize) {
        self.bus.emit(Event::SyncPushed {
            watch: self.name.clone(),
            new_ref,
            commits,
        });
    }

    /// Emits [`Event::SyncSkipped`]: a sync request that reached a benign no-op
    /// (currently only a missing remote via the live gate), so the daemon logs it
    /// rather than silently dropping the request. No state change accompanies it.
    fn emit_sync_skipped(&self, reason: &str) {
        self.bus.emit(Event::SyncSkipped {
            watch: self.name.clone(),
            reason: reason.to_string(),
        });
    }
}

/// The outcome of one [`Worker::sync_once`] pass, folded into state and retry
/// decisions by [`Worker::run_sync_cycle`].
enum SyncStep {
    /// Fetch showed nothing to pull and nothing to push.
    NothingToDo,
    /// The cycle completed: `pushed` counts commits sent (when any were) and
    /// `pulled` records whether remote commits were integrated.
    Done {
        /// Commits pushed to the remote, when the push sent any.
        pushed: Option<usize>,
        /// Whether remote commits were pulled in this cycle.
        pulled: bool,
    },
    /// Reconcile hit a conflict: the watch latches `Conflicted`.
    Conflict,
    /// The push lost a fast-forward race: re-run the cycle (capped).
    RaceLost,
    /// The op gate was busy while locked work was needed: re-attempt on the
    /// short gate-busy cadence, carrying the pre-flight result so the paced
    /// retries reuse it instead of re-fetching.
    GateBusy(CachedFetch),
    /// The advance refused to overwrite uncommitted/unmerged work (or a raced
    /// branch commit): abandon this cycle and re-attempt, never a sync failure.
    Abandoned,
    /// A network, gate, or reconcile step failed (message ready to surface).
    Failed(String),
}

/// The outcome of the locked reconcile+advance window ([`run_locked_window`]).
enum LockedResult {
    /// Reconcile ran and (if it moved remote changes in) advanced. `pulled` is
    /// `Some((prev_ref, new_ref))` when the advance integrated remote commits,
    /// `None` when there was nothing upstream to integrate. `tip` is the branch
    /// tip after the window, for the push event.
    Reconciled {
        /// The prev/new refs when remote changes were pulled in, else `None`.
        pulled: Option<(String, String)>,
        /// The branch tip after the window (the ref a push would send).
        tip: String,
        /// Whether the pre-sync snapshot committed local work in this window.
        /// Counted into what the push sends (a pre-sync commit is a real commit
        /// the remote receives), so [`Event::SyncPushed`] does not undercount it.
        presync_committed: bool,
    },
    /// Reconcile hit a conflict; nothing advanced.
    Conflict,
    /// The advance refused to overwrite uncommitted/unmerged work (a local change
    /// the reconcile target would clobber, or a commit the user raced onto the
    /// branch): nothing was advanced and the tree is untouched.
    Abandoned,
    /// A step in the window failed (message ready to surface).
    Failed(String),
}

/// Runs [`fetch`](VcsBackend::fetch) off the async runtime, timeout-bounded.
/// Takes the backend by value (a cheap [`Arc`] clone) so the future stays
/// `Send`. A backend panic is surfaced as an error message rather than aborting
/// the worker task.
async fn sync_fetch(
    backend: SharedBackend,
    timeout: Duration,
) -> Result<crate::vcs::RemoteState, String> {
    match tokio::task::spawn_blocking(move || backend.fetch(timeout)).await {
        Ok(Ok(remote)) => Ok(remote),
        Ok(Err(err)) => Err(format!("fetch: {err}")),
        Err(join) => Err(format!("fetch task panicked: {join}")),
    }
}

/// Runs [`has_remote`](VcsBackend::has_remote) off the async runtime — the sync
/// cycle's live remote gate. `Ok(false)` (the configured remote is not defined)
/// is the honest no-op the cycle skips on without latching. A backend error (or
/// panic) is different: it maps to `Err`, which the cycle treats as a real
/// repository/environment fault — it latches [`SyncError`](WatchState::SyncError)
/// and surfaces [`SyncOutcome::Failed`], never masked as a missing remote. Takes
/// the backend by value (a cheap [`Arc`] clone) so the future stays `Send`.
async fn sync_has_remote(backend: SharedBackend) -> Result<bool, String> {
    match tokio::task::spawn_blocking(move || backend.has_remote()).await {
        Ok(Ok(has)) => Ok(has),
        Ok(Err(err)) => Err(format!("has_remote: {err}")),
        Err(join) => Err(format!("has_remote task panicked: {join}")),
    }
}

/// Resolves the current branch tip off the async runtime (for the gate-free
/// push path's [`Event::SyncPushed`] ref). **Best-effort, never an error**: a
/// backend failure, a panicked task, or an empty history all yield an empty
/// string — the same fallback the locked window uses for its tip — because the
/// tip here is event decoration, not a step the cycle's outcome depends on.
async fn sync_current_tip(backend: SharedBackend) -> String {
    tokio::task::spawn_blocking(move || current_tip(&backend).unwrap_or_default())
        .await
        .unwrap_or_default()
}

/// Runs [`ahead_of_upstream`](VcsBackend::ahead_of_upstream) off the async
/// runtime — the local-only read behind the gate-free push's at-push-time
/// commit count. **Best-effort**: a backend without the notion (the default
/// `None`), an error, or a panic all yield `None`, and the caller falls back
/// to the fetch-time count — the count is advisory, never a correctness gate.
async fn sync_ahead_of_upstream(backend: SharedBackend) -> Option<usize> {
    tokio::task::spawn_blocking(move || backend.ahead_of_upstream().ok().flatten())
        .await
        .ok()
        .flatten()
}

/// Runs [`is_dirty`](VcsBackend::is_dirty) off the async runtime, mapping a
/// backend error (or panic) to a message the sync cycle surfaces as a failure.
/// Takes the backend by value (a cheap [`Arc`] clone) so the future stays
/// `Send`.
async fn sync_is_dirty(backend: SharedBackend) -> Result<bool, String> {
    match tokio::task::spawn_blocking(move || backend.is_dirty()).await {
        Ok(Ok(dirty)) => Ok(dirty),
        Ok(Err(err)) => Err(format!("status: {err}")),
        Err(join) => Err(format!("status task panicked: {join}")),
    }
}

/// Runs [`push`](VcsBackend::push) off the async runtime, timeout-bounded.
async fn sync_push(backend: SharedBackend, timeout: Duration) -> Result<PushOutcome, String> {
    match tokio::task::spawn_blocking(move || backend.push(timeout)).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(err)) => Err(format!("push: {err}")),
        Err(join) => Err(format!("push task panicked: {join}")),
    }
}

/// Runs the locked window — pre-sync snapshot → (optionally) reconcile →
/// advance — off the async runtime with the op `guard` **coupled to the
/// blocking git work** exactly like the snapshot path: the guard moves into the
/// blocking scope and is [`complete`](OpGuard::complete)d there on every
/// non-panic outcome, so an async abort can never release the op lock
/// mid-write. On a panic the guard is dropped release-only, leaving the journal
/// `begin` as recovery evidence. This is the ONE place that guard/panic/journal
/// discipline lives for the sync window; both window shapes share it.
///
/// `scratch` selects the shape. `Some` runs the full window (there are remote
/// commits to integrate): pre-sync snapshot, then the out-of-tree reconcile in
/// the scratch worktree, then the advance. `None` is the **push-only** window,
/// used when the fetch found nothing new remotely (`behind == 0`) — including
/// the never-pushed branch, whose upstream tracking ref does not exist yet:
/// there is nothing to rebase onto or advance to, and rebasing onto the
/// nonexistent ref would error the whole cycle (breaking the first push of a
/// fresh repository). The push-only window is just the pre-sync snapshot
/// (`pulled` is `None`, `tip` is the branch tip after it) and skips the
/// scratch-worktree setup/teardown entirely.
///
/// Recovery is never surgery on the user's files: a crash mid-window leaves the
/// tree fully committed at worst mid-checkout, and the next sync cycle self-heals
/// (dirty check → pre-sync snapshot → fresh reconcile → advance). The advance
/// itself is non-destructive by construction (see [`VcsBackend::advance`]): it
/// carries the pre-reconcile tip and refuses ([`AdvanceOutcome::WouldClobber`])
/// rather than overwrite uncommitted work or a raced branch commit, which this
/// surfaces as [`LockedResult::Abandoned`]. A prior crashed cycle's leftover
/// scratch worktree is pruned first, since reconcile requires the scratch path
/// not to exist.
async fn run_locked_window(
    backend: SharedBackend,
    scanner: Option<Arc<SecretScanner>>,
    scratch: Option<PathBuf>,
    guard: Box<dyn OpGuard>,
) -> LockedResult {
    let joined = tokio::task::spawn_blocking(move || {
        // Snapshot local first, always: commit any pending local work before the
        // tree can be moved by advance. A no-op on a clean tree. The commit
        // carries a `Vard-Host` trailer so multi-machine history stays legible,
        // and the scanner so a secret is never swept in right before a push.
        let presync_committed = match backend.snapshot(&pre_sync_request(scanner)) {
            Ok(report) => report.committed.is_some(),
            Err(err) => {
                guard.complete();
                return LockedResult::Failed(format!("pre-sync snapshot: {err}"));
            }
        };
        // Push-only window: nothing to integrate, so the snapshot was the whole
        // job — report the tip it left for the push.
        let Some(scratch) = scratch else {
            let tip = current_tip(&backend).unwrap_or_default();
            guard.complete();
            return LockedResult::Reconciled {
                pulled: None,
                tip,
                presync_committed,
            };
        };
        // Clear any scratch worktree a prior crashed cycle left behind (reconcile
        // requires the path not to exist). A no-op when absent.
        let _ = backend.prune_scratch(&scratch);
        // The branch tip the reconcile is about to consume: passed to `advance`
        // as the expected tip so a commit the user races onto the branch during
        // the reconcile window is refused rather than stranded.
        let tip_before = current_tip(&backend).unwrap_or_default();
        let expected_tip = SnapshotId::new(tip_before.clone());
        match backend.reconcile(&scratch) {
            Ok(ReconcileOutcome::Rebased { new_head }) => {
                match backend.advance(&new_head, &expected_tip) {
                    Ok(AdvanceOutcome::Advanced) => {
                        guard.complete();
                        LockedResult::Reconciled {
                            pulled: Some((tip_before, new_head.to_string())),
                            tip: new_head.to_string(),
                            presync_committed,
                        }
                    }
                    Ok(AdvanceOutcome::WouldClobber) => {
                        // A concurrent local change or a raced branch commit made
                        // the advance unsafe. Do NOT overwrite. Clean up the
                        // scratch and abandon; the next cycle's pre-sync snapshot
                        // commits the new work and reconciles it properly.
                        let _ = backend.prune_scratch(&scratch);
                        guard.complete();
                        LockedResult::Abandoned
                    }
                    Err(err) => {
                        guard.complete();
                        LockedResult::Failed(format!("advance: {err}"))
                    }
                }
            }
            Ok(ReconcileOutcome::AlreadyUpToDate) => {
                guard.complete();
                LockedResult::Reconciled {
                    pulled: None,
                    tip: tip_before,
                    presync_committed,
                }
            }
            Ok(ReconcileOutcome::Conflict) => {
                guard.complete();
                LockedResult::Conflict
            }
            Err(err) => {
                guard.complete();
                LockedResult::Failed(format!("reconcile: {err}"))
            }
        }
    })
    .await;
    match joined {
        Ok(result) => result,
        Err(join) => LockedResult::Failed(format!("sync task panicked: {join}")),
    }
}

/// Builds the [`SnapshotRequest`] for the sync cycle's pre-sync snapshot: a
/// [`Trigger::PreSync`] commit tagged with a `Vard-Host: <hostname>` trailer.
/// The trailer records which machine took the snapshot, so a branch synced
/// across several hosts reads legibly (and a future tool can attribute commits).
///
/// The watch's `scanner` is threaded in so the pre-sync snapshot quarantines
/// secrets exactly as an ordinary pass does (VRD-22). This is load-bearing:
/// the pre-sync snapshot is the commit that immediately precedes a push, so
/// without scanning it a secret an ordinary pass had kept untracked would be
/// swept in here and pushed to the remote — the one place quarantine most needs
/// to hold. The sync path reads only whether it committed; the quarantine is
/// reflected into watch state by the ordinary snapshot pass, not here.
fn pre_sync_request(scanner: Option<Arc<SecretScanner>>) -> SnapshotRequest {
    SnapshotRequest {
        trigger: Trigger::PreSync,
        user_text: None,
        extra_trailers: vec![("Vard-Host".to_string(), host_name())],
        scanner,
    }
}

/// The local host's name for the `Vard-Host` trailer, resolved once and cached.
/// On unix it is read from the kernel via `uname(2)` (the same value `hostname`
/// prints); elsewhere it falls back to the `COMPUTERNAME`/`HOSTNAME` environment
/// variables. Any empty or unreadable result becomes `"unknown"`, so the trailer
/// is always present and well-formed.
fn host_name() -> String {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        #[cfg(unix)]
        {
            let uname = rustix::system::uname();
            match uname.nodename().to_str() {
                Ok(name) if !name.is_empty() => name.to_string(),
                _ => "unknown".to_string(),
            }
        }
        #[cfg(not(unix))]
        {
            std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .ok()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        }
    })
    .clone()
}

/// The branch tip's id via a one-entry `log`, or `None` on any error/empty
/// history. Cheap, non-network, and read-only — used to fill the sync events'
/// ref fields.
fn current_tip(backend: &SharedBackend) -> Option<String> {
    backend
        .log(&LogFilter {
            since: None,
            until: None,
            limit: Some(1),
        })
        .ok()
        .and_then(|entries| entries.into_iter().next())
        .map(|snap| snap.id.to_string())
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

/// Runs [`snapshot`](VcsBackend::snapshot) for an already-admitted operation,
/// retrying a contended index lock with exponential backoff up to
/// [`EngineConfig::lock_retry_attempts`], with the op `guard` **coupled to the
/// blocking git work** (F2).
///
/// Each attempt runs `backend.snapshot` in a `spawn_blocking` scope with the
/// guard moved IN and returned OUT with the result. That coupling is the whole
/// point: if this worker's async task is aborted mid-write (a shutdown drain
/// timeout), the detached blocking task carries the guard to completion and only
/// then drops it — so an async abort can never release the op lock while git is
/// still writing (and a new engine's `begin` can never truncate the evidence out
/// from under a live commit). A panic in the git call unwinds the scope, dropping
/// the guard *release-only* (no journal close) and leaving the dangling `begin`
/// as recovery evidence; it surfaces as [`PassResult::Panicked`].
///
/// The lock-retry backoff stays on the **async** timer between attempts: no git
/// is in flight during the sleep (a contended attempt already released git's own
/// `index.lock`), so the guard rests on the async side safely there, and the
/// sleep stays controllable under paused-time tests. A free function (not a
/// method) so the future stays `Send` — see [`check_safe`].
///
/// The guard is closed at a single decision point (Q1): [`complete`](OpGuard::complete)
/// on every non-panic outcome, a deliberate release-only drop on a panic.
async fn run_snapshot_under_guard(
    backend: SharedBackend,
    cfg: EngineConfig,
    scanner: Option<Arc<SecretScanner>>,
    prov: &Provenance,
    guard: Box<dyn OpGuard>,
) -> PassResult {
    let req = SnapshotRequest {
        trigger: prov.trigger,
        user_text: prov.user_text.clone(),
        extra_trailers: Vec::new(),
        scanner,
    };

    let mut guard = guard;
    let mut attempt = 1u32;
    let result = loop {
        let backend_for_call = Arc::clone(&backend);
        let req_for_call = req.clone();
        // Guard moves INTO the blocking scope for the git write and back OUT with
        // the result (see the doc above for why this coupling is load-bearing).
        let joined = tokio::task::spawn_blocking(move || {
            let outcome = backend_for_call.snapshot(&req_for_call);
            (guard, outcome)
        })
        .await;
        let (returned_guard, outcome) = match joined {
            Ok(pair) => pair,
            // The blocking git call panicked: the guard was already dropped
            // release-only during the unwind, preserving the dangling `begin`.
            Err(join) => return PassResult::Panicked(join.to_string()),
        };
        guard = returned_guard;
        match outcome {
            Ok(SnapshotReport {
                committed: Some(committed),
                quarantined,
            }) => break PassResult::Committed(committed, quarantined),
            Ok(SnapshotReport {
                committed: None,
                quarantined,
            }) => break PassResult::Clean(quarantined),
            Err(VcsError::UnsafeState(reason)) => break PassResult::Unsafe(reason),
            Err(VcsError::LockContended { .. }) => {
                if attempt < cfg.lock_retry_attempts {
                    tokio::time::sleep(backoff(cfg, attempt)).await;
                    attempt += 1;
                    continue;
                }
                break PassResult::StillLocked;
            }
            Err(other) => break PassResult::Failed(other),
        }
    };
    // Single close point (Q1): `complete` on every non-panic outcome — compacts
    // the journal and releases the op lock. The panic path returned early with
    // the guard already dropped release-only.
    guard.complete();
    result
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

/// One watch paired with the backend it snapshots through and the operation
/// gate that admits its mutations.
struct ConfiguredWatch {
    spec: WatchSpec,
    backend: SharedBackend,
    gate: SharedGate,
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

    /// Arms every watch, spawns its worker, and returns an [`EngineHandle`].
    ///
    /// Consumes the engine: its watches, handles, and bus move into the spawned
    /// tasks, which run in the background. The returned future resolves once all
    /// watches are armed and [`Event::DaemonStarted`] has been emitted. A host
    /// keeps the process alive by holding a subscriber (or its own runtime) —
    /// this call does not block.
    ///
    /// The returned [`EngineHandle`] owns the worker and dispatcher tasks. Call
    /// [`shutdown`](EngineHandle::shutdown) to wind the engine down gracefully;
    /// dropping the handle instead leaves the engine running detached (see
    /// [`EngineHandle`]).
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
    pub async fn start(self) -> Result<EngineHandle, EngineError> {
        let Engine { bus, watches, cfg } = self;

        let (watcher, watcher_rx) = Watcher::new();
        let (scheduler, scheduler_rx) = Scheduler::new();
        // A second, independent scheduler for the pull-driven sync interval: its
        // ticks are jittered and its stream routes to a separate dispatcher, so a
        // sync tick never needs a purpose label (see the `scheduler` module docs).
        let (sync_scheduler, sync_scheduler_rx) = Scheduler::new();

        let mut prepared: Vec<(Worker, mpsc::UnboundedReceiver<WatchInput>)> = Vec::new();
        let mut watcher_routes: HashMap<String, mpsc::UnboundedSender<WatchInput>> = HashMap::new();
        let mut scheduler_routes: HashMap<String, mpsc::UnboundedSender<WatchInput>> =
            HashMap::new();
        let mut sync_scheduler_routes: HashMap<String, mpsc::UnboundedSender<WatchInput>> =
            HashMap::new();
        // The handle keeps its own clone of every worker's input sender so it can
        // inject manual triggers ([`EngineHandle::trigger`]). Held per watch
        // regardless of trigger mode — even an interval-only watch accepts a
        // manual snapshot. See [`EngineHandle::shutdown`] for why these must be
        // dropped before the workers are drained.
        let mut handle_routes: HashMap<String, mpsc::UnboundedSender<WatchInput>> = HashMap::new();
        // Per-watch status cells, in configured order, shared with the handle so
        // it can project per-watch truth ([`EngineHandle::watch_states`]).
        let mut statuses: Vec<(String, Arc<StdMutex<SharedStatus>>)> = Vec::new();

        for cw in watches {
            let name = cw.spec.name().to_string();
            let (tx, rx) = mpsc::unbounded_channel();
            handle_routes.insert(name.clone(), tx.clone());
            let status = Arc::new(StdMutex::new(SharedStatus::new()));
            statuses.push((name.clone(), Arc::clone(&status)));
            let mode = cw.spec.trigger();

            let mute = if matches!(mode, TriggerMode::Events | TriggerMode::Both) {
                let handle = watcher.arm(&cw.spec).map_err(EngineError::Watcher)?;
                watcher_routes.insert(name.clone(), tx.clone());
                MuteSource::Watch(handle)
            } else {
                MuteSource::Silent
            };

            // Compile the per-watch secret scanner (VRD-22) — unconditionally,
            // regardless of trigger mode: an interval-only or manual watch
            // quarantines just as an events watch does. An invalid extra pattern
            // is a config fault surfaced like an invalid `exclude` pattern.
            let scanner = Arc::new(
                SecretScanner::compile(cw.spec.secret_scan(), cw.spec.secret_patterns()).map_err(
                    |source| EngineError::SecretScan {
                        watch: name.clone(),
                        source,
                    },
                )?,
            );

            let schedule = if matches!(mode, TriggerMode::Interval | TriggerMode::Both) {
                let handle = scheduler
                    .arm(name.clone(), cw.spec.interval())
                    .map_err(EngineError::Scheduler)?;
                scheduler_routes.insert(name.clone(), tx.clone());
                Some(handle)
            } else {
                None
            };

            // Sync runs only when the watch opts in AND a scratch directory was
            // injected for the out-of-tree reconcile (vard-core resolves none).
            let scratch_dir = cw.spec.scratch_dir().map(|p| p.to_path_buf());
            let sync_enabled = cw.spec.sync() && scratch_dir.is_some();
            let remote = cw.spec.remote().to_string();

            // Arm the pull-driven sync-interval schedule only for a watch that
            // actually syncs and whose `sync_interval` is nonzero (zero disables
            // the cadence timer). Its jittered ticks route to `dispatch_sync_
            // scheduler`, which delivers them as automatic sync requests. The
            // snapshot `trigger` mode does not gate this: even an `events`-only
            // watch syncs on its cadence — the mode governs snapshots, not sync.
            let sync_schedule = if sync_enabled && !cw.spec.sync_interval().is_zero() {
                let handle = sync_scheduler
                    .arm_jittered(name.clone(), cw.spec.sync_interval())
                    .map_err(EngineError::Scheduler)?;
                sync_scheduler_routes.insert(name.clone(), tx.clone());
                Some(handle)
            } else {
                None
            };

            let worker = Worker {
                name,
                backend: cw.backend,
                gate: cw.gate,
                scanner: Some(scanner),
                mute,
                _schedule: schedule,
                _sync_schedule: sync_schedule,
                bus: bus.clone(),
                cfg,
                pending: None,
                state: WatchState::Ok,
                trouble: None,
                local_state: WatchState::Ok,
                local_trouble: None,
                local_reason: None,
                sync_latch: None,
                status,
                retry: None,
                retry_attempts: 0,
                retry_exhausted: false,
                sync_enabled,
                scratch_dir,
                remote,
                sync_pending: None,
                sync_failures: 0,
                sync_defer: None,
            };
            prepared.push((worker, rx));
        }

        let workers: Vec<JoinHandle<()>> = prepared
            .into_iter()
            .map(|(worker, rx)| tokio::spawn(worker.run(rx)))
            .collect();
        let dispatchers = vec![
            tokio::spawn(dispatch_watcher(watcher_rx, watcher_routes)),
            tokio::spawn(dispatch_scheduler(scheduler_rx, scheduler_routes)),
            tokio::spawn(dispatch_sync_scheduler(
                sync_scheduler_rx,
                sync_scheduler_routes,
            )),
        ];

        bus.emit(Event::DaemonStarted);
        Ok(EngineHandle {
            bus,
            workers,
            dispatchers,
            routes: handle_routes,
            statuses,
            drain_timeout: cfg.shutdown_drain_timeout,
        })
    }
}

/// A live [`Engine`]'s teardown lever, returned by [`Engine::start`].
///
/// It owns the engine's worker and dispatcher tasks. Two lifecycles are
/// supported:
///
/// - **Graceful shutdown.** [`shutdown`](Self::shutdown) stops the dispatchers,
///   drains each worker's in-flight pass, tears the watch handles down, and
///   emits [`Event::DaemonStopped`] once every task has joined. It consumes the
///   handle, so it cannot be called twice.
/// - **Fire and forget.** Dropping the handle *without* calling
///   [`shutdown`](Self::shutdown) leaves the engine running detached — the
///   worker and dispatcher tasks keep running on the runtime, exactly as before
///   this type existed. An embedder that just wants the engine to run for the
///   life of the process can drop the handle and hold a subscriber instead.
pub struct EngineHandle {
    bus: EventBus,
    workers: Vec<JoinHandle<()>>,
    dispatchers: Vec<JoinHandle<()>>,
    /// A clone of every worker's input sender, keyed by watch name, so
    /// [`trigger`](Self::trigger) can inject a manual snapshot. These are
    /// additional senders on the same channels the dispatchers feed, so
    /// [`shutdown`](Self::shutdown) must drop this map before draining the
    /// workers or their channels never close.
    routes: HashMap<String, mpsc::UnboundedSender<WatchInput>>,
    /// Per-watch shared status cells, in configured order, read by
    /// [`watch_states`](Self::watch_states). Each is the other end of the `Arc`
    /// its worker mirrors state into.
    statuses: Vec<(String, Arc<StdMutex<SharedStatus>>)>,
    drain_timeout: Duration,
}

impl EngineHandle {
    /// A point-in-time snapshot of every watch's lifecycle truth, in configured
    /// order: its current [`WatchState`], the [`TroubleKind`] and reason of the
    /// transition that put it there, and when it entered that state.
    ///
    /// This is the projection a host renders into health or status output
    /// instead of reconstructing state from the [`Event`] stream (which is
    /// lossy — a slow subscriber can miss a [`Event::WatchStateChanged`]). The
    /// returned values are consistent with the engine's own truth at the moment
    /// of the call.
    ///
    /// Each [`WatchStatus::entered_at`] is engine-local and not persisted, so a
    /// restart resets it — see [`WatchStatus`].
    pub fn watch_states(&self) -> Vec<WatchStatus> {
        self.statuses
            .iter()
            .map(|(name, cell)| {
                let s = cell.lock().unwrap_or_else(PoisonError::into_inner);
                WatchStatus {
                    name: name.clone(),
                    state: s.state,
                    trouble: s.trouble,
                    reason: s.reason.clone(),
                    entered_at: s.entered_at,
                }
            })
            .collect()
    }

    /// Injects a manual snapshot trigger ([`Trigger::Manual`]) for the named
    /// watch, exactly as if a filesystem or interval signal had arrived — the
    /// worker coalesces it with any pending change (manual wins on priority) and
    /// snapshots on its serialized loop.
    ///
    /// This is the daemon's lever for a user-requested snapshot (spec §11: the
    /// CLI drops a request file, the daemon drains it and calls this).
    ///
    /// Returns `true` if the watch exists and the trigger was delivered to its
    /// worker; `false` if no watch by that name is configured (or, defensively,
    /// if its worker has already stopped). The delivery is fire-and-forget: a
    /// `true` return means the worker was handed the trigger, not that a commit
    /// landed — the outcome arrives later on the event bus.
    pub fn trigger(&self, watch: &str) -> bool {
        match self.routes.get(watch) {
            Some(tx) => tx.send(WatchInput::Trigger(Provenance::manual())).is_ok(),
            None => false,
        }
    }

    /// Requests a **manual** sync cycle for the named watch: fetch, reconcile
    /// out of tree, advance, and push, on the watch's serialized worker (so it
    /// never overlaps a snapshot). This is the explicit user/CLI verb — it
    /// attempts even a watch latched [`Conflicted`](WatchState::Conflicted), to
    /// pick up a resolution the user just made.
    ///
    /// A watch that cannot sync — [`WatchSpec::sync`] is `false`, or no
    /// [`scratch_dir`](WatchSpec::scratch_dir) was injected — accepts the
    /// request and does nothing.
    ///
    /// Returns `true` if the watch exists and the request was delivered; `false`
    /// if no watch by that name is configured (or its worker has stopped).
    /// Fire-and-forget: the outcome arrives later on the event bus
    /// ([`Event::SyncPushed`]/[`Event::SyncPulled`]/[`Event::SyncConflict`]/[`Event::SyncFailed`]).
    pub fn request_sync(&self, watch: &str) -> bool {
        self.deliver_sync(watch, true, None)
    }

    /// Requests a **manual** sync cycle and returns a completion acknowledgement:
    /// the [`oneshot::Receiver`] resolves with the cycle's terminal
    /// [`SyncOutcome`] when it finishes. This is the in-process path (the `vard
    /// sync` CLI) that must report the *real* result rather than infer it from
    /// event silence.
    ///
    /// Returns `None` if no watch by that name is configured (or its worker has
    /// stopped). If the worker shuts down before the requested cycle completes
    /// (a busy op gate that never freed, a drain that cut it short), the sender is
    /// dropped without a value and the receiver resolves to `Err`, which the
    /// caller reports honestly as "did not run" — never as success.
    pub fn request_sync_ack(&self, watch: &str) -> Option<oneshot::Receiver<SyncOutcome>> {
        let (tx, rx) = oneshot::channel();
        if self.deliver_sync(watch, true, Some(tx)) {
            Some(rx)
        } else {
            None
        }
    }

    /// Requests an **automatic** sync cycle for the named watch — the same path
    /// the post-snapshot enqueue and the pull-driven sync-interval timer drive.
    /// Identical to
    /// [`request_sync`](Self::request_sync) except it is **suppressed while the
    /// watch is [`Conflicted`](WatchState::Conflicted)** — auto-sync stops for a
    /// conflicted watch until a manual request resolves it.
    pub fn request_auto_sync(&self, watch: &str) -> bool {
        self.deliver_sync(watch, false, None)
    }

    fn deliver_sync(
        &self,
        watch: &str,
        manual: bool,
        ack: Option<oneshot::Sender<SyncOutcome>>,
    ) -> bool {
        match self.routes.get(watch) {
            Some(tx) => tx.send(WatchInput::RequestSync { manual, ack }).is_ok(),
            None => false,
        }
    }

    /// Winds the engine down gracefully, emitting [`Event::DaemonStopped`] once
    /// every task has joined.
    ///
    /// The teardown is a cancellation drain, in order:
    ///
    /// 0. **Drop the handle's own route senders.** The handle retains a sender
    ///    per worker for [`trigger`](Self::trigger); those are dropped first, so
    ///    that once the dispatchers' senders go too, each worker channel is truly
    ///    senderless and closes. (Skipping this would wedge the drain until the
    ///    timeout.)
    /// 1. **Stop the dispatchers.** All three dispatch tasks (watcher, snapshot
    ///    scheduler, sync scheduler) are aborted and joined, which drops every
    ///    per-watch route sender they held. No new trigger can reach a worker
    ///    after this point.
    /// 2. **Drain the workers.** With the dispatchers gone, each worker's input
    ///    channel has no senders left, so its run loop observes the close and
    ///    exits *after* finishing any pass already in flight — a snapshot mid-commit
    ///    is never abandoned. A worker parked on its retry timer simply drains and
    ///    exits. As each worker task ends it drops its [`WatchHandle`] and its
    ///    [`ScheduleHandle`]s, whose `Drop` impls disarm the notify backend and the
    ///    tick tasks.
    /// 3. **Emit [`Event::DaemonStopped`].** Every task has joined, so no further
    ///    event can be emitted after it.
    ///
    /// # Drain timeout
    ///
    /// The worker drain is bounded by the configured drain timeout (default
    /// [`DEFAULT_SHUTDOWN_DRAIN_TIMEOUT`], overridable with
    /// [`EngineBuilder::shutdown_drain_timeout`]). A worker still running a pass
    /// when the budget elapses is **aborted** rather than waited on forever;
    /// shutdown then still joins it and emits [`Event::DaemonStopped`]. An aborted
    /// pass may leave a `git` invocation running on its blocking thread, but no
    /// async task is leaked and the engine is fully wound down.
    ///
    /// # Runtime
    ///
    /// Must be called from within the same Tokio runtime the engine was started
    /// on.
    pub async fn shutdown(self) {
        let EngineHandle {
            bus,
            workers,
            dispatchers,
            routes,
            statuses,
            drain_timeout,
        } = self;
        // The status cells are pure readers' state; drop them with the handle.
        drop(statuses);

        // 0. Drop the handle's own route senders. The dispatchers hold their
        //    clones (dropped in step 1), but this map holds one more sender per
        //    worker for [`trigger`](Self::trigger); a worker's channel only
        //    closes once *every* sender is gone. Without this drop the drain in
        //    step 2 would block until the timeout aborts the workers.
        drop(routes);

        // 1. Stop the dispatchers first so no further trigger reaches a worker.
        //    Aborting drops the route senders they hold; joining guarantees
        //    those senders are gone before we wait on the workers, so each
        //    worker's input channel is already closing.
        for dispatcher in &dispatchers {
            dispatcher.abort();
        }
        for dispatcher in dispatchers {
            let _ = dispatcher.await;
        }

        // 2. Drain the workers. Each observes its now-senderless channel close
        //    and exits after finishing any in-flight pass, dropping its watch
        //    and schedule handles on the way out. Bound the wait: a worker still
        //    running when the budget elapses is aborted so shutdown cannot hang.
        let mut workers: Vec<Option<JoinHandle<()>>> = workers.into_iter().map(Some).collect();
        let deadline = Instant::now() + drain_timeout;
        for slot in workers.iter_mut() {
            let handle = slot.as_mut().expect("worker slots start populated");
            let remaining = deadline.saturating_duration_since(Instant::now());
            match timeout(remaining, handle).await {
                Ok(_joined) => *slot = None,
                // Budget spent: stop draining and abort whatever is left.
                Err(_elapsed) => break,
            }
        }
        for handle in workers.iter_mut().flatten() {
            handle.abort();
        }
        for mut slot in workers {
            if let Some(handle) = slot.take() {
                let _ = handle.await;
            }
        }

        // 3. Every task has joined, so this is provably the last event.
        bus.emit(Event::DaemonStopped);
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
            WatcherSignal::Trouble {
                watch,
                kind,
                detail,
            } => (watch, WatchInput::Trouble { kind, detail }),
        };
        if let Some(tx) = routes.get(&watch) {
            let _ = tx.send(input);
        }
    }
}

/// Fans the shared snapshot-scheduler stream out to per-watch workers by name.
async fn dispatch_scheduler(
    mut rx: SchedulerRx,
    routes: HashMap<String, mpsc::UnboundedSender<WatchInput>>,
) {
    while let Some(signal) = rx.recv().await {
        let (watch, input) = match signal {
            SchedulerSignal::Tick { watch } => (watch, WatchInput::Trigger(Provenance::interval())),
            SchedulerSignal::Trouble {
                watch,
                kind,
                detail,
            } => (watch, WatchInput::Trouble { kind, detail }),
        };
        if let Some(tx) = routes.get(&watch) {
            let _ = tx.send(input);
        }
    }
}

/// Fans the shared sync-scheduler stream out to per-watch workers by name.
///
/// A [`Tick`](SchedulerSignal::Tick) becomes an **automatic** sync request
/// ([`WatchInput::RequestSync`] with `manual: false`) — the worker's own gating
/// (suppressed while Conflicted, respects the sync-error backoff, a no-op when
/// the watch cannot sync) decides what it does. [`Trouble`](SchedulerSignal::Trouble)
/// is handled exactly as the snapshot scheduler's is: it surfaces the schedule's
/// death as a watch [`Trouble`](WatchInput::Trouble).
async fn dispatch_sync_scheduler(
    mut rx: SchedulerRx,
    routes: HashMap<String, mpsc::UnboundedSender<WatchInput>>,
) {
    while let Some(signal) = rx.recv().await {
        let (watch, input) = match signal {
            SchedulerSignal::Tick { watch } => (
                watch,
                WatchInput::RequestSync {
                    manual: false,
                    ack: None,
                },
            ),
            SchedulerSignal::Trouble {
                watch,
                kind,
                detail,
            } => (watch, WatchInput::Trouble { kind, detail }),
        };
        if let Some(tx) = routes.get(&watch) {
            let _ = tx.send(input);
        }
    }
}

/// A watch queued into an [`EngineBuilder`]: how its backend is obtained, plus
/// the operation gate that admits its mutations (the injected op-lock seam, or
/// the standalone [`default_gate`]).
struct PendingWatch {
    source: BackendSource,
    gate: SharedGate,
}

/// How a [`PendingWatch`]'s backend is obtained at [`build`](EngineBuilder::build).
enum BackendSource {
    /// Build a [`GitBackend`] from the spec.
    Git(WatchSpec),
    /// Use the caller-supplied backend.
    Backend(WatchSpec, SharedBackend),
}

impl PendingWatch {
    /// The watch's stable name.
    fn name(&self) -> &str {
        match &self.source {
            BackendSource::Git(spec) | BackendSource::Backend(spec, _) => spec.name(),
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
    ///
    /// **UNGATED.** This watch uses the standalone no-op gate: it enforces no
    /// cross-process one-writer-per-watch invariant, which is the right default
    /// for an embedding SDK host that is the sole writer of its repositories. The
    /// `vard` daemon must instead use [`watch_with_gate`](Self::watch_with_gate)
    /// to inject its per-watch op lock; running multiple ungated writers against
    /// one repository can corrupt it.
    pub fn watch(mut self, spec: WatchSpec) -> Self {
        self.watches.push(PendingWatch {
            source: BackendSource::Git(spec),
            gate: default_gate(),
        });
        self
    }

    /// Adds a git-backed watch whose mutations are admitted through an injected
    /// operation `gate` (the `vard` daemon's per-watch op lock + journal), rather
    /// than the standalone no-op default. Otherwise identical to
    /// [`watch`](Self::watch).
    pub fn watch_with_gate(mut self, spec: WatchSpec, gate: SharedGate) -> Self {
        self.watches.push(PendingWatch {
            source: BackendSource::Git(spec),
            gate,
        });
        self
    }

    /// Adds a watch snapshotted through a caller-supplied backend.
    ///
    /// Lets an embedder plug in an alternate [`VcsBackend`] and lets tests drive
    /// the engine with a fake. The backend must be `Send + Sync` because the
    /// worker calls it from a blocking task (see [`SharedBackend`]).
    ///
    /// **UNGATED.** Like [`watch`](Self::watch), this uses the standalone no-op
    /// gate — no cross-process one-writer-per-watch invariant. A host that needs
    /// the op lock (the daemon) must inject one via
    /// [`watch_with_gate`](Self::watch_with_gate).
    pub fn watch_with_backend(mut self, spec: WatchSpec, backend: SharedBackend) -> Self {
        self.watches.push(PendingWatch {
            source: BackendSource::Backend(spec, backend),
            gate: default_gate(),
        });
        self
    }

    /// Adds a watch with BOTH a caller-supplied backend and an injected
    /// operation gate — the combination a host uses when it has already opened
    /// (and vetted) the repository itself and must not have
    /// [`build`](Self::build) re-open it: `build` cannot then fail on an open
    /// the host performed, so one broken repository never aborts a multi-watch
    /// engine the host filtered per watch (the CLI's in-process sync path).
    pub fn watch_with_backend_and_gate(
        mut self,
        spec: WatchSpec,
        backend: SharedBackend,
        gate: SharedGate,
    ) -> Self {
        self.watches.push(PendingWatch {
            source: BackendSource::Backend(spec, backend),
            gate,
        });
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

    /// Sets the (shorter) cadence for the op-gate-busy self-retry. Exists mainly
    /// for deterministic tests.
    #[allow(dead_code)]
    pub fn gate_busy_retry_interval(mut self, interval: Duration) -> Self {
        self.cfg.gate_busy_retry_interval = interval;
        self
    }

    /// Sets how long [`EngineHandle::shutdown`] waits for in-flight passes to
    /// drain before aborting the workers (default
    /// [`DEFAULT_SHUTDOWN_DRAIN_TIMEOUT`]).
    pub fn shutdown_drain_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.shutdown_drain_timeout = timeout;
        self
    }

    /// Sets the per-step timeout for the sync cycle's network ops (default
    /// [`DEFAULT_SYNC_NETWORK_TIMEOUT`]).
    pub fn sync_network_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.sync_network_timeout = timeout;
        self
    }

    /// Sets the base delay of the sync failure backoff (default
    /// [`DEFAULT_SYNC_BACKOFF_BASE`]). Mainly for deterministic tests.
    pub fn sync_backoff_base(mut self, base: Duration) -> Self {
        self.cfg.sync_backoff_base = base;
        self
    }

    /// Sets the cap on the sync failure backoff (default
    /// [`DEFAULT_SYNC_BACKOFF_CAP`]). Mainly for deterministic tests.
    pub fn sync_backoff_cap(mut self, cap: Duration) -> Self {
        self.cfg.sync_backoff_cap = cap;
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
            let PendingWatch { source, gate } = pending;
            let cw = match source {
                BackendSource::Backend(spec, backend) => ConfiguredWatch {
                    spec,
                    backend,
                    gate,
                },
                BackendSource::Git(spec) => {
                    let backend =
                        open_git_backend(&spec).map_err(|source| EngineError::Backend {
                            watch: spec.name().to_string(),
                            source,
                        })?;
                    ConfiguredWatch {
                        spec,
                        backend: Arc::new(backend),
                        gate,
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
///
/// Public so a host (the `vard` binary's `snapshot`/`log`/`diff`/`restore`
/// commands) opens a watch's backend through the *same* branch policy the
/// engine uses.
///
/// For a watch with an **explicitly configured** [`branch`](WatchSpec::branch),
/// this guarantees the CLI operates on exactly the branch the daemon commits to.
/// For an adopt-current-branch watch (`branch` unset), the branch is whatever
/// `HEAD` names at open time — so the daemon (which opened at its startup) and a
/// later CLI invocation bind at *different* moments and could disagree if the
/// user switched branches in between. Configure a branch to pin the two together.
pub fn open_git_backend(spec: &WatchSpec) -> Result<GitBackend, VcsError> {
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
    /// A watch's secret scanner could not be compiled — one of its extra
    /// `secret_patterns` is not valid gitignore syntax (VRD-22). Surfaced at
    /// engine start exactly as an invalid watcher `exclude` pattern is
    /// ([`Watcher`](EngineError::Watcher) carrying
    /// [`InvalidExclude`](crate::watcher::WatcherError::InvalidExclude)): a
    /// configuration fault the daemon reports rather than starting the watch.
    SecretScan {
        /// The watch whose scanner failed to compile.
        watch: String,
        /// The underlying compile error naming the offending pattern.
        source: SecretScanError,
    },
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
            EngineError::SecretScan { watch, source } => {
                write!(f, "watch {watch:?}: {source}")
            }
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            EngineError::Backend { source, .. } => Some(source),
            EngineError::Watcher(e) => Some(e),
            EngineError::Scheduler(e) => Some(e),
            EngineError::SecretScan { source, .. } => Some(source),
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
    use crate::secret_scan::SecretReason;
    use crate::vcs::{ChangeSummary, SnapshotId};

    use super::*;

    /// A scripted result for one [`VcsBackend::snapshot`] call.
    #[derive(Clone)]
    enum Scripted {
        /// Return a committed snapshot with this many changed files.
        Commit(usize),
        /// Return a clean (no-op) sweep.
        Clean,
        /// Return a clean sweep that quarantined this many newly-added secrets:
        /// nothing committed, but the report lists the withheld matches (VRD-22).
        Quarantine(usize),
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
        /// Whether the work tree reports dirty ([`is_dirty`]); drives the sync
        /// short-circuit's "clean tree is the only nothing-to-do" check.
        dirty: bool,
        /// When set, [`is_dirty`](VcsBackend::is_dirty) errors — models a
        /// repository whose status cannot be read (a corrupt index).
        fail_is_dirty: bool,
        /// What [`has_remote`](VcsBackend::has_remote) reports; drives the sync
        /// cycle's live remote gate. Defaults to `true` (a usable remote).
        has_remote: bool,
        /// When set, [`has_remote`](VcsBackend::has_remote) errors — models a
        /// repository whose config cannot be read (a real fault, not a missing
        /// remote).
        fail_has_remote: bool,
        /// When set, [`fetch`](VcsBackend::fetch) flips `dirty` to this value as
        /// a side effect — models an edit saved WHILE the fetch was in flight,
        /// for pinning the pre-flight's fetch-then-dirty ordering (P4).
        dirty_after_fetch: Option<bool>,
        /// What [`ahead_of_upstream`](VcsBackend::ahead_of_upstream) reports
        /// (`None` = the trait default: the backend has no such notion, the
        /// push-time count falls back to the fetch-time one).
        ahead_now: Option<usize>,
        snapshots: VecDeque<Scripted>,
        /// When set, every snapshot commits — models a backend that never
        /// reports a clean tree (used to prove the post-op re-check is bounded).
        always_commit: bool,
        /// When set, every snapshot fails hard — models a repository whose
        /// snapshots never succeed (used to prove the retry budget is bounded).
        always_fail: bool,
        snapshot_calls: usize,
        safe_calls: usize,
        /// The 1-based snapshot call that should block on `gate_rx`.
        gate_on_call: Option<usize>,
        gate_rx: Option<std::sync::mpsc::Receiver<()>>,
        /// Scripted sync-primitive results, consumed in order; each falls back
        /// to a benign default when its script runs dry.
        fetch_results: VecDeque<Result<crate::vcs::RemoteState, VcsError>>,
        reconcile_results: VecDeque<Result<ReconcileOutcome, VcsError>>,
        advance_results: VecDeque<AdvanceOutcome>,
        push_results: VecDeque<Result<PushOutcome, VcsError>>,
        fetch_calls: usize,
        reconcile_calls: usize,
        advance_calls: usize,
        prune_calls: usize,
        push_calls: usize,
        /// When set, model git's real reconcile precondition on disk: `reconcile`
        /// fails (as `worktree add` does) if the scratch path already exists, and
        /// `prune_scratch` actually removes it. Off by default so the scriptable
        /// tests that never touch a real scratch path are unaffected.
        model_scratch_precondition: bool,
    }

    impl FakeBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(FakeInner {
                    safe: SafeState::Safe,
                    safe_results: VecDeque::new(),
                    dirty: false,
                    fail_is_dirty: false,
                    has_remote: true,
                    fail_has_remote: false,
                    dirty_after_fetch: None,
                    ahead_now: None,
                    snapshots: VecDeque::new(),
                    always_commit: false,
                    always_fail: false,
                    snapshot_calls: 0,
                    safe_calls: 0,
                    gate_on_call: None,
                    gate_rx: None,
                    fetch_results: VecDeque::new(),
                    reconcile_results: VecDeque::new(),
                    advance_results: VecDeque::new(),
                    push_results: VecDeque::new(),
                    fetch_calls: 0,
                    reconcile_calls: 0,
                    advance_calls: 0,
                    prune_calls: 0,
                    push_calls: 0,
                    model_scratch_precondition: false,
                }),
            })
        }

        /// Turns on on-disk modeling of git's scratch precondition: `reconcile`
        /// fails when the scratch path already exists (as `worktree add` does)
        /// and `prune_scratch` really removes it. For the leftover-scratch
        /// self-heal pin; the scriptable tests never touch a real path.
        fn model_scratch_precondition(&self) {
            self.inner.lock().unwrap().model_scratch_precondition = true;
        }

        /// Scripts the sync primitives' results, consumed in order (falling back
        /// to benign defaults once drained).
        fn script_fetch(
            &self,
            r: impl IntoIterator<Item = Result<crate::vcs::RemoteState, VcsError>>,
        ) {
            self.inner.lock().unwrap().fetch_results.extend(r);
        }
        fn script_reconcile(
            &self,
            r: impl IntoIterator<Item = Result<ReconcileOutcome, VcsError>>,
        ) {
            self.inner.lock().unwrap().reconcile_results.extend(r);
        }
        fn script_advance(&self, r: impl IntoIterator<Item = AdvanceOutcome>) {
            self.inner.lock().unwrap().advance_results.extend(r);
        }
        fn script_push(&self, r: impl IntoIterator<Item = Result<PushOutcome, VcsError>>) {
            self.inner.lock().unwrap().push_results.extend(r);
        }
        fn fetch_calls(&self) -> usize {
            self.inner.lock().unwrap().fetch_calls
        }
        fn reconcile_calls(&self) -> usize {
            self.inner.lock().unwrap().reconcile_calls
        }
        fn advance_calls(&self) -> usize {
            self.inner.lock().unwrap().advance_calls
        }
        fn push_calls(&self) -> usize {
            self.inner.lock().unwrap().push_calls
        }
        fn prune_calls(&self) -> usize {
            self.inner.lock().unwrap().prune_calls
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

        /// Marks the work tree dirty (or clean) for the sync short-circuit's
        /// [`is_dirty`](VcsBackend::is_dirty) probe.
        fn set_dirty(&self, dirty: bool) {
            self.inner.lock().unwrap().dirty = dirty;
        }

        /// Sets what the live remote gate ([`has_remote`](VcsBackend::has_remote))
        /// reports.
        fn set_has_remote(&self, has_remote: bool) {
            self.inner.lock().unwrap().has_remote = has_remote;
        }

        /// Makes the dirty probe error (an unreadable repository status).
        fn fail_is_dirty(&self) {
            self.inner.lock().unwrap().fail_is_dirty = true;
        }

        /// Makes the remote probe error (an unreadable repository config).
        fn fail_has_remote(&self) {
            self.inner.lock().unwrap().fail_has_remote = true;
        }

        /// Arms an edit that "lands during the fetch": the next fetch flips the
        /// dirty answer to `dirty` as a side effect, so a cycle that checked
        /// dirtiness BEFORE fetching would miss it.
        fn set_dirty_after_fetch(&self, dirty: bool) {
            self.inner.lock().unwrap().dirty_after_fetch = Some(dirty);
        }

        /// Scripts the local-only ahead read
        /// ([`ahead_of_upstream`](VcsBackend::ahead_of_upstream)) — this feeds
        /// ONLY the gate-free push's at-push-time commit count.
        fn set_ahead_now(&self, ahead: usize) {
            self.inner.lock().unwrap().ahead_now = Some(ahead);
        }

        /// Makes every snapshot commit, so the post-op re-check never converges
        /// on its own — used to prove the re-check is bounded under the mute.
        fn set_always_commit(&self) {
            self.inner.lock().unwrap().always_commit = true;
        }

        /// Makes every snapshot fail hard — used to prove the retry budget is
        /// bounded even while the repository flaps unsafe↔safe.
        fn set_always_fail(&self) {
            self.inner.lock().unwrap().always_fail = true;
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

        fn is_dirty(&self) -> Result<bool, VcsError> {
            let inner = self.inner.lock().unwrap();
            if inner.fail_is_dirty {
                return Err(VcsError::CommandFailed {
                    op: "status".into(),
                    status: Some(128),
                    stderr: "fatal: index file corrupt".into(),
                });
            }
            Ok(inner.dirty)
        }

        fn ahead_of_upstream(&self) -> Result<Option<usize>, VcsError> {
            Ok(self.inner.lock().unwrap().ahead_now)
        }

        fn has_remote(&self) -> Result<bool, VcsError> {
            let inner = self.inner.lock().unwrap();
            if inner.fail_has_remote {
                return Err(VcsError::CommandFailed {
                    op: "config".into(),
                    status: Some(128),
                    stderr: "fatal: unable to read config file".into(),
                });
            }
            Ok(inner.has_remote)
        }

        fn snapshot(&self, _req: &SnapshotRequest) -> Result<SnapshotReport, VcsError> {
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
                } else if inner.always_fail {
                    Scripted::Fail
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
                Scripted::Commit(files) => Ok(SnapshotReport {
                    committed: Some(SnapshotOutcome {
                        id: SnapshotId::new("deadbeef"),
                        summary: ChangeSummary {
                            changed: files,
                            added: 0,
                            deleted: 0,
                            notable: Vec::new(),
                        },
                    }),
                    quarantined: Vec::new(),
                }),
                Scripted::Clean => Ok(SnapshotReport::default()),
                Scripted::Quarantine(n) => Ok(SnapshotReport {
                    committed: None,
                    quarantined: (0..n)
                        .map(|i| SecretMatch {
                            path: PathBuf::from(format!(".env.{i}")),
                            reason: SecretReason::FilenamePattern {
                                pattern: ".env*".to_string(),
                            },
                        })
                        .collect(),
                }),
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

        fn tracked_files(&self) -> Result<Vec<PathBuf>, VcsError> {
            unimplemented!("tracked_files is out of scope for the snapshot engine")
        }

        fn diff(
            &self,
            _from: &crate::vcs::VcsRef,
            _to: Option<&crate::vcs::VcsRef>,
            _pathspec: Option<&std::path::Path>,
        ) -> Result<String, VcsError> {
            Ok(String::new())
        }

        fn verify_ref(&self, _rev: &crate::vcs::VcsRef) -> Result<bool, VcsError> {
            unimplemented!("verify_ref is out of scope for the snapshot engine")
        }

        fn path_exists_at(
            &self,
            _rev: &crate::vcs::VcsRef,
            _path: &std::path::Path,
        ) -> Result<bool, VcsError> {
            unimplemented!("path_exists_at is out of scope for the snapshot engine")
        }

        fn restore(&self, _target: &crate::vcs::RestoreTarget) -> Result<(), VcsError> {
            unimplemented!("restore is out of scope for the snapshot engine")
        }

        fn fetch(
            &self,
            _timeout: std::time::Duration,
        ) -> Result<crate::vcs::RemoteState, VcsError> {
            let mut inner = self.inner.lock().unwrap();
            inner.fetch_calls += 1;
            // An armed "edit during the fetch" lands now: any dirty check that
            // ran before this fetch has already read the stale answer.
            if let Some(dirty) = inner.dirty_after_fetch.take() {
                inner.dirty = dirty;
            }
            inner
                .fetch_results
                .pop_front()
                .unwrap_or(Ok(crate::vcs::RemoteState {
                    remote_moved: false,
                    ahead: 0,
                    behind: 0,
                    stale_tracking_ref: false,
                }))
        }

        fn reconcile(
            &self,
            scratch: &std::path::Path,
        ) -> Result<crate::vcs::ReconcileOutcome, VcsError> {
            let mut inner = self.inner.lock().unwrap();
            inner.reconcile_calls += 1;
            if inner.model_scratch_precondition && scratch.exists() {
                // Model git's real precondition: `worktree add` fails when the
                // scratch path already exists (a crashed prior cycle's leftover).
                return Err(VcsError::CommandFailed {
                    op: "worktree add".into(),
                    status: Some(128),
                    stderr: format!("fatal: '{}' already exists", scratch.display()),
                });
            }
            inner
                .reconcile_results
                .pop_front()
                .unwrap_or(Ok(ReconcileOutcome::AlreadyUpToDate))
        }

        fn advance(
            &self,
            _target: &crate::vcs::SnapshotId,
            _expected_tip: &crate::vcs::SnapshotId,
        ) -> Result<AdvanceOutcome, VcsError> {
            let mut inner = self.inner.lock().unwrap();
            inner.advance_calls += 1;
            Ok(inner
                .advance_results
                .pop_front()
                .unwrap_or(AdvanceOutcome::Advanced))
        }

        fn prune_scratch(&self, scratch: &std::path::Path) -> Result<(), VcsError> {
            let mut inner = self.inner.lock().unwrap();
            inner.prune_calls += 1;
            if inner.model_scratch_precondition && scratch.exists() {
                std::fs::remove_dir_all(scratch).map_err(VcsError::Io)?;
            }
            Ok(())
        }

        fn push(&self, _timeout: std::time::Duration) -> Result<crate::vcs::PushOutcome, VcsError> {
            let mut inner = self.inner.lock().unwrap();
            inner.push_calls += 1;
            inner
                .push_results
                .pop_front()
                .unwrap_or(Ok(PushOutcome::UpToDate))
        }
    }

    /// A scriptable [`OpGate`] for driving the worker's admission paths: it can
    /// admit (the default), report busy, or fail, and counts how many times it
    /// was asked to begin.
    struct FakeGate {
        admit: std::sync::atomic::AtomicBool,
        fail: std::sync::atomic::AtomicBool,
        begins: AtomicUsize,
        /// How many times [`available`](crate::gate::OpGate::available) was
        /// probed — lets a test assert a path never touched the gate at all.
        probes: AtomicUsize,
    }

    impl FakeGate {
        /// A gate that reports busy on every `begin` until [`admit`](Self::admit).
        fn busy() -> Arc<FakeGate> {
            Arc::new(FakeGate {
                admit: std::sync::atomic::AtomicBool::new(false),
                fail: std::sync::atomic::AtomicBool::new(false),
                begins: AtomicUsize::new(0),
                probes: AtomicUsize::new(0),
            })
        }

        /// A gate that returns an error on every `begin`.
        fn failing() -> Arc<FakeGate> {
            Arc::new(FakeGate {
                admit: std::sync::atomic::AtomicBool::new(false),
                fail: std::sync::atomic::AtomicBool::new(true),
                begins: AtomicUsize::new(0),
                probes: AtomicUsize::new(0),
            })
        }

        fn admit(&self) {
            self.admit.store(true, Ordering::SeqCst);
        }

        fn probes(&self) -> usize {
            self.probes.load(Ordering::SeqCst)
        }

        /// Flips the error mode: `true` returns `Err` from `begin`, `false`
        /// returns busy/admit per [`admit`](Self::admit). Lets a test drive a
        /// Failure episode and then a GateBusy one on one gate.
        fn set_fail(&self, fail: bool) {
            self.fail.store(fail, Ordering::SeqCst);
        }

        fn begins(&self) -> usize {
            self.begins.load(Ordering::SeqCst)
        }
    }

    impl crate::gate::OpGate for FakeGate {
        fn begin(
            &self,
            _op: &str,
        ) -> Result<Option<Box<dyn crate::gate::OpGuard>>, crate::gate::OpGateError> {
            self.begins.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(crate::gate::OpGateError::new("scripted gate failure"));
            }
            if self.admit.load(Ordering::SeqCst) {
                Ok(Some(Box::new(FakeGuard)))
            } else {
                Ok(None)
            }
        }

        fn available(&self) -> bool {
            // Honest probe: busy iff `begin` would report busy. A scripted
            // *failing* gate reports available (optimistic, per the trait docs)
            // so `begin` gets to surface its error.
            self.probes.fetch_add(1, Ordering::SeqCst);
            self.fail.load(Ordering::SeqCst) || self.admit.load(Ordering::SeqCst)
        }
    }

    struct FakeGuard;
    impl crate::gate::OpGuard for FakeGuard {
        fn complete(self: Box<Self>) {}
    }

    /// Spawns a worker driven directly by an injected input channel and a fake
    /// backend, bypassing the real watcher/scheduler for determinism. Returns
    /// the input sender, an event subscriber, and the shared mute counter. Uses
    /// the standalone no-op gate.
    fn spawn_worker(
        backend: Arc<FakeBackend>,
        cfg: EngineConfig,
    ) -> (
        mpsc::UnboundedSender<WatchInput>,
        EventReceiver,
        Arc<AtomicUsize>,
    ) {
        spawn_worker_with_gate(backend, cfg, default_gate())
    }

    /// [`spawn_worker`] with an injected operation gate, for the gate-busy /
    /// gate-error admission paths.
    fn spawn_worker_with_gate(
        backend: Arc<FakeBackend>,
        cfg: EngineConfig,
        gate: SharedGate,
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
            gate,
            scanner: None,
            mute: MuteSource::Counter(Arc::clone(&counter)),
            _schedule: None,
            _sync_schedule: None,
            bus,
            cfg,
            pending: None,
            state: WatchState::Ok,
            trouble: None,
            local_state: WatchState::Ok,
            local_trouble: None,
            local_reason: None,
            sync_latch: None,
            status: Arc::new(StdMutex::new(SharedStatus::new())),
            retry: None,
            retry_attempts: 0,
            retry_exhausted: false,
            sync_enabled: false,
            scratch_dir: None,
            remote: "origin".to_string(),
            sync_pending: None,
            sync_failures: 0,
            sync_defer: None,
        };
        tokio::spawn(worker.run(rx));
        (tx, events, counter)
    }

    /// Spawns a **sync-enabled** worker (an injected scratch dir; the no-op gate
    /// admits) for driving the sync cycle deterministically under paused time.
    fn spawn_sync_worker(
        backend: Arc<FakeBackend>,
        cfg: EngineConfig,
    ) -> (mpsc::UnboundedSender<WatchInput>, EventReceiver) {
        spawn_sync_worker_with_gate(backend, cfg, default_gate())
    }

    /// [`spawn_sync_worker`] with an explicit scratch path, for tests that
    /// model the scratch directory's real on-disk lifecycle.
    fn spawn_sync_worker_with_scratch(
        backend: Arc<FakeBackend>,
        cfg: EngineConfig,
        scratch: PathBuf,
    ) -> (mpsc::UnboundedSender<WatchInput>, EventReceiver) {
        let (tx, events) = spawn_sync_worker_inner(backend, cfg, default_gate(), scratch);
        (tx, events)
    }

    /// [`spawn_sync_worker`] with an injected operation gate, for the sync
    /// cycle's gate-busy paths (the pre-network probe, the bounded drain).
    fn spawn_sync_worker_with_gate(
        backend: Arc<FakeBackend>,
        cfg: EngineConfig,
        gate: SharedGate,
    ) -> (mpsc::UnboundedSender<WatchInput>, EventReceiver) {
        // The scriptable FakeBackend never touches the scratch path, so a fixed
        // mock path is fine here; on-disk lifecycle tests inject a tempdir via
        // `spawn_sync_worker_with_scratch`.
        spawn_sync_worker_inner(backend, cfg, gate, PathBuf::from("/tmp/vard-test-scratch"))
    }

    fn spawn_sync_worker_inner(
        backend: Arc<FakeBackend>,
        cfg: EngineConfig,
        gate: SharedGate,
        scratch: PathBuf,
    ) -> (mpsc::UnboundedSender<WatchInput>, EventReceiver) {
        let bus = EventBus::default();
        let events = bus.subscribe();
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = Worker {
            name: "w".to_string(),
            backend: backend as SharedBackend,
            gate,
            scanner: None,
            mute: MuteSource::Silent,
            _schedule: None,
            _sync_schedule: None,
            bus,
            cfg,
            pending: None,
            state: WatchState::Ok,
            trouble: None,
            local_state: WatchState::Ok,
            local_trouble: None,
            local_reason: None,
            sync_latch: None,
            status: Arc::new(StdMutex::new(SharedStatus::new())),
            retry: None,
            retry_attempts: 0,
            retry_exhausted: false,
            sync_enabled: true,
            scratch_dir: Some(scratch),
            remote: "origin".to_string(),
            sync_pending: None,
            sync_failures: 0,
            sync_defer: None,
        };
        tokio::spawn(worker.run(rx));
        (tx, events)
    }

    /// A scripted [`fetch`](VcsBackend::fetch) result. `remote_moved` follows
    /// `behind > 0` (a remote we are behind has moved).
    fn remote(ahead: usize, behind: usize) -> Result<crate::vcs::RemoteState, VcsError> {
        Ok(crate::vcs::RemoteState {
            remote_moved: behind > 0,
            ahead,
            behind,
            stale_tracking_ref: false,
        })
    }

    /// A scripted fetch result for a branch DELETED remotely while a stale
    /// local tracking ref survives: ahead by `ahead` (the full local history),
    /// with the flag set so the cycle must not trust local tracking-ref reads.
    fn remote_with_stale_tracking(ahead: usize) -> Result<crate::vcs::RemoteState, VcsError> {
        Ok(crate::vcs::RemoteState {
            remote_moved: false,
            ahead,
            behind: 0,
            stale_tracking_ref: true,
        })
    }

    // --- sync cycle (paused time, scripted backend) --------------------------

    #[tokio::test(start_paused = true)]
    async fn sync_disabled_watch_never_talks_to_the_remote() {
        // sync = false (or an unset scratch dir) => spawn_worker is sync-disabled:
        // a request is dropped silently with no fetch and no event.
        let backend = FakeBackend::new();
        let (tx, mut events, _c) = spawn_worker(Arc::clone(&backend), test_cfg());
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        for _ in 0..10 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        assert_eq!(backend.fetch_calls(), 0, "a disabled watch never fetches");
        assert!(no_more_outcomes(&mut events), "and emits nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn sync_push_only_reports_pushed_with_commit_count() {
        // Local ahead, remote not moved (first push / local-ahead): nothing to
        // integrate, so reconcile+advance are skipped and the push reports Pushed.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(2, 0)]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncPushed { commits, .. } => assert_eq!(commits, 2),
            other => panic!("expected SyncPushed, got {other:?}"),
        }
        assert_eq!(
            backend.reconcile_calls(),
            0,
            "nothing to integrate: reconcile is skipped"
        );
        assert_eq!(backend.advance_calls(), 0, "and nothing advances");
        assert_eq!(backend.push_calls(), 1);
        assert!(no_more_outcomes(&mut events), "state stays Ok throughout");
    }

    #[tokio::test(start_paused = true)]
    async fn a_never_pushed_branch_pushes_without_reconciling() {
        // The first-push onboarding path: the branch has never been pushed, so
        // its upstream tracking ref does not exist — fetch reports the normal
        // not-moved / behind-0 / ahead-N state. The cycle must SKIP
        // reconcile+advance (a rebase onto the nonexistent upstream would error
        // the whole cycle) and push directly, terminating Moved{pushed > 0}.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(3, 0)]);
        // Poison the reconcile script: if the cycle reached reconcile it would
        // fail loudly instead of silently passing.
        backend.script_reconcile([Err(VcsError::CommandFailed {
            op: "rebase".into(),
            status: Some(128),
            stderr: "fatal: invalid upstream 'origin/main'".into(),
        })]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncPushed { commits, .. } => assert_eq!(commits, 3),
            other => panic!("expected SyncPushed, got {other:?}"),
        }
        assert_eq!(
            ack_rx.await,
            Ok(SyncOutcome::Moved {
                pushed: Some(3),
                pulled: false
            })
        );
        assert_eq!(
            backend.reconcile_calls(),
            0,
            "a never-pushed branch must not rebase onto its nonexistent upstream"
        );
        assert!(no_more_outcomes(&mut events), "no failure latches");
    }

    #[tokio::test(start_paused = true)]
    async fn sync_pull_then_push_emits_pulled_before_pushed() {
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::Rebased {
            new_head: SnapshotId::new("newtip"),
        })]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncPulled { new_ref, .. } => assert_eq!(new_ref, "newtip"),
            other => panic!("expected SyncPulled first, got {other:?}"),
        }
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncPushed {
                new_ref, commits, ..
            } => {
                assert_eq!(new_ref, "newtip");
                assert_eq!(commits, 1);
            }
            other => panic!("expected SyncPushed second, got {other:?}"),
        }
        assert_eq!(
            backend.advance_calls(),
            1,
            "a rebase is made live by advance"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_leftover_scratch_worktree_from_a_crash_self_heals() {
        // A prior cycle crashed mid-reconcile and left the scratch worktree on
        // disk. `reconcile`'s real precondition (`worktree add`) fails on an
        // existing path, so the locked window MUST prune before reconciling —
        // this pins that ordering, daemon-less: the next cycle self-heals with
        // no settler involved.
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("reconcile-scratch");
        std::fs::create_dir_all(&scratch).unwrap(); // the crash leftover
        let backend = FakeBackend::new();
        backend.model_scratch_precondition();
        backend.script_fetch([remote(0, 1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::Rebased {
            new_head: SnapshotId::new("healedtip"),
        })]);
        backend.script_push([Ok(PushOutcome::UpToDate)]);
        let (tx, mut events) =
            spawn_sync_worker_with_scratch(Arc::clone(&backend), test_cfg(), scratch.clone());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncPulled { new_ref, .. } => assert_eq!(new_ref, "healedtip"),
            other => panic!("expected the cycle to self-heal and pull, got {other:?}"),
        }
        assert_eq!(backend.reconcile_calls(), 1, "reconcile ran exactly once");
        assert!(
            backend.prune_calls() >= 1,
            "the leftover was pruned before reconcile"
        );
        assert!(
            !scratch.exists(),
            "the leftover scratch directory is gone from disk"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sync_conflict_latches_suppresses_auto_sync_and_a_manual_clears_it() {
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::Conflict)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::SyncConflict { .. }
        ));
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::Conflicted),
            other => panic!("expected a transition to Conflicted, got {other:?}"),
        }

        // Auto-sync is suppressed while Conflicted: an automatic request never
        // reaches the network.
        let fetches = backend.fetch_calls();
        tx.send(WatchInput::RequestSync {
            manual: false,
            ack: None,
        })
        .unwrap();
        for _ in 0..10 {
            settle().await;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(
            backend.fetch_calls(),
            fetches,
            "an auto-sync must not run while a conflict latches"
        );

        // Snapshots STILL work while latched (local snapshotting continues).
        backend.script([Scripted::Commit(1)]);
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::SnapshotCompleted { .. }
        ));

        // After the user resolves, a manual cycle that finds nothing to do
        // clears the conflict back to Ok.
        backend.script_fetch([remote(0, 0)]);
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::Ok),
            other => panic!("expected a transition back to Ok, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sync_network_failure_enters_sync_error_then_backoff_success_clears_it() {
        let cfg = EngineConfig {
            sync_backoff_base: Duration::from_secs(1),
            ..test_cfg()
        };
        let backend = FakeBackend::new();
        backend.script_fetch([Err(VcsError::CommandFailed {
            op: "fetch".into(),
            status: Some(1),
            stderr: "could not read from remote".into(),
        })]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), cfg);

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_millis(200)).await,
            Event::SyncFailed { .. }
        ));
        match advance_until_event(&mut events, Duration::from_millis(200)).await {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::SyncError),
            other => panic!("expected SyncError, got {other:?}"),
        }

        // The backoff timer re-attempts without any fresh trigger; a success
        // clears the watch back to Ok.
        backend.script_fetch([remote(0, 0)]);
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::Ok),
            other => panic!("expected a self-driven recovery to Ok, got {other:?}"),
        }
        assert_eq!(backend.fetch_calls(), 2, "the backoff drove a second fetch");
    }

    /// Advances the paused clock until an event matching `pred` arrives,
    /// returning it and ignoring the rest (bounded so a missing event fails).
    async fn advance_until_matching(
        events: &mut EventReceiver,
        step: Duration,
        pred: impl Fn(&Event) -> bool,
    ) -> Event {
        for _ in 0..40 {
            let ev = advance_until_event(events, step).await;
            if pred(&ev) {
                return ev;
            }
        }
        panic!("no matching event arrived");
    }

    /// Settles until the backend has seen at least `n` fetch calls.
    async fn advance_until_fetch_calls(backend: &FakeBackend, n: usize, step: Duration) {
        for _ in 0..500 {
            settle().await;
            if backend.fetch_calls() >= n {
                return;
            }
            tokio::time::advance(step).await;
        }
        panic!(
            "fetch_calls never reached {n} (was {})",
            backend.fetch_calls()
        );
    }

    // --- Group C: sync latch is a separate axis from the local state ----------

    #[tokio::test(start_paused = true)]
    async fn conflict_latch_is_hidden_by_a_local_pause_and_resurfaces() {
        // A conflicted watch whose repo then goes unsafe shows Paused (snapshots
        // stopped); when the repo is safe again the Conflicted latch re-surfaces.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::Conflict)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(
                e,
                Event::WatchStateChanged {
                    to: WatchState::Conflicted,
                    ..
                }
            )
        })
        .await;

        // The repo turns unsafe; a snapshot pass pauses the watch. The displayed
        // state moves Conflicted -> Paused (the local fault takes precedence).
        backend.set_safe(SafeState::Unsafe(UnsafeReason::MergeInProgress));
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        match advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::WatchStateChanged { .. })
        })
        .await
        {
            Event::WatchStateChanged { from, to, .. } => {
                assert_eq!(from, WatchState::Conflicted, "was displaying the conflict");
                assert_eq!(to, WatchState::Paused, "the local pause is now displayed");
            }
            other => panic!("expected a state change, got {other:?}"),
        }

        // The repo becomes safe again: the unsafe re-poll timer recovers the local
        // state to Ok, and the still-standing conflict re-surfaces.
        backend.set_safe(SafeState::Safe);
        match advance_until_matching(&mut events, Duration::from_secs(31), |e| {
            matches!(e, Event::WatchStateChanged { .. })
        })
        .await
        {
            Event::WatchStateChanged { from, to, .. } => {
                assert_eq!(from, WatchState::Paused);
                assert_eq!(to, WatchState::Conflicted, "the conflict re-surfaced");
            }
            other => panic!("expected the conflict to re-surface, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sync_nothing_to_do_leaves_an_unrelated_attention_untouched() {
        // A sync that finds nothing to do clears only the (absent) sync latch; a
        // standing non-latching local Attention is left exactly as it was.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(0, 0)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trouble {
            kind: TroubleKind::Degraded,
            detail: "unrelated".into(),
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::WatchStateChanged { to, trouble, .. } => {
                assert_eq!(to, WatchState::Attention);
                assert_eq!(trouble, Some(TroubleKind::Degraded));
            }
            other => panic!("expected Attention, got {other:?}"),
        }

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_fetch_calls(&backend, 1, Duration::from_secs(1)).await;
        // Let any (erroneous) transition surface, then assert none did.
        for _ in 0..5 {
            settle().await;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert!(
            no_more_outcomes(&mut events),
            "a NothingToDo sync must not clobber the standing Attention"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn conflict_hidden_under_attention_resurfaces_then_a_success_clears_it() {
        // Conflict latched, then a local Attention hides it, then a good snapshot
        // clears the Attention (conflict re-surfaces), then a resolving sync
        // clears the latch to the true local state (Ok).
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 1), remote(0, 0)]);
        backend.script_reconcile([Ok(ReconcileOutcome::Conflict)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        // 1. Latch Conflicted.
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(
                e,
                Event::WatchStateChanged {
                    to: WatchState::Conflicted,
                    ..
                }
            )
        })
        .await;

        // 2. A local Attention hides the latch (local != Ok).
        tx.send(WatchInput::Trouble {
            kind: TroubleKind::Degraded,
            detail: "blip".into(),
        })
        .unwrap();
        match advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::WatchStateChanged { .. })
        })
        .await
        {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::Attention),
            other => panic!("expected Attention, got {other:?}"),
        }

        // 3. A committed snapshot clears the (self-clearing) Attention; the
        //    conflict re-surfaces. Auto-sync is suppressed while it stands.
        backend.script([Scripted::Commit(1), Scripted::Clean]);
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        match advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::WatchStateChanged { .. })
        })
        .await
        {
            Event::WatchStateChanged { to, .. } => {
                assert_eq!(to, WatchState::Conflicted, "conflict re-surfaced")
            }
            other => panic!("expected the conflict to re-surface, got {other:?}"),
        }

        // 4. A resolving manual sync clears the latch to the true local state (Ok).
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        match advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::WatchStateChanged { .. })
        })
        .await
        {
            Event::WatchStateChanged { to, .. } => assert_eq!(to, WatchState::Ok),
            other => panic!("expected a clear to Ok, got {other:?}"),
        }
    }

    // --- Group A: advance refusal abandons the cycle without a SyncError ------

    #[tokio::test(start_paused = true)]
    async fn advance_would_clobber_abandons_and_retries_without_sync_error() {
        // The advance refuses (WouldClobber); the cycle abandons — no SyncError —
        // and re-attempts, converging on the next cycle.
        let backend = FakeBackend::new();
        // behind > 0 on both fetches: reconcile+advance only run when the fetch
        // found remote commits to integrate.
        backend.script_fetch([remote(1, 1), remote(1, 1)]);
        backend.script_reconcile([
            Ok(ReconcileOutcome::Rebased {
                new_head: SnapshotId::new("cafef00d"),
            }),
            Ok(ReconcileOutcome::AlreadyUpToDate),
        ]);
        backend.script_advance([AdvanceOutcome::WouldClobber]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        // The first observable event is the SECOND cycle's push — the first cycle
        // abandoned silently and re-armed a short retry.
        let ev = advance_until_matching(&mut events, Duration::from_millis(600), |e| {
            !matches!(e, Event::SnapshotCompleted { .. })
        })
        .await;
        assert!(
            matches!(ev, Event::SyncPushed { .. }),
            "expected the retry to converge with a push, got {ev:?}"
        );
        assert_eq!(
            backend.advance_calls(),
            1,
            "only the first cycle reached advance (and was refused)"
        );
        assert_eq!(backend.fetch_calls(), 2, "the cycle was re-attempted");
    }

    // --- Group A: the sync-request lifecycle ----------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_manual_sync_bypasses_a_standing_failure_backoff() {
        // Finding 2: a failed sync arms a long SyncError backoff; a later MANUAL
        // request must run promptly, not wait the backoff out.
        let cfg = EngineConfig {
            sync_backoff_base: Duration::from_secs(3600),
            ..test_cfg()
        };
        let backend = FakeBackend::new();
        backend.script_fetch([
            Err(VcsError::CommandFailed {
                op: "fetch".into(),
                status: Some(1),
                stderr: "unreachable".into(),
            }),
            remote(0, 0),
        ]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), cfg);

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::SyncFailed { .. })
        })
        .await;
        assert_eq!(backend.fetch_calls(), 1);

        // A second manual request runs immediately, WITHOUT advancing the clock
        // anywhere near the 3600s backoff deadline.
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_fetch_calls(&backend, 2, Duration::from_millis(100)).await;
        assert_eq!(
            backend.fetch_calls(),
            2,
            "the manual sync ran promptly despite the standing backoff"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_clobber_terminates_in_sync_error_after_the_cap() {
        // Finding 6: a continuously-clobbered advance must not fetch-loop forever.
        // It re-attempts up to SYNC_MAX_ATTEMPTS, then terminates as a SyncError.
        let backend = FakeBackend::new();
        let n = SYNC_MAX_ATTEMPTS as usize;
        // behind > 0 so every attempt takes the reconcile+advance path.
        backend.script_fetch((0..n).map(|_| remote(1, 1)));
        backend.script_reconcile((0..n).map(|_| {
            Ok(ReconcileOutcome::Rebased {
                new_head: SnapshotId::new("cafef00d"),
            })
        }));
        backend.script_advance((0..n).map(|_| AdvanceOutcome::WouldClobber));
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();

        // The cap is reached: a SyncFailed and a transition to SyncError.
        assert!(matches!(
            advance_until_matching(&mut events, Duration::from_secs(1), |e| matches!(
                e,
                Event::SyncFailed { .. }
            ))
            .await,
            Event::SyncFailed { .. }
        ));
        assert_eq!(
            backend.fetch_calls(),
            n,
            "fetches are bounded by the clobber cap, not unbounded"
        );
        assert_eq!(
            backend.advance_calls(),
            n,
            "one refused advance per attempt"
        );
        // The waiter is told the terminal failure, not left hanging.
        assert!(matches!(ack_rx.await, Ok(SyncOutcome::Failed(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn coalesced_requests_both_receive_the_same_terminal_outcome() {
        // Finding 9: a second acked request coalescing onto a pending one must NOT
        // drop the older waiter — one cycle runs and BOTH acks resolve to it.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(0, 0)]); // clean + unmoved => NothingToDo
        let (tx, _events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        // Queue both before the worker is polled, so they coalesce into one record.
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(tx1),
        })
        .unwrap();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(tx2),
        })
        .unwrap();

        advance_until_fetch_calls(&backend, 1, Duration::from_millis(100)).await;
        settle().await;
        let (o1, o2) = (rx1.await, rx2.await);
        assert_eq!(
            o1,
            Ok(SyncOutcome::UpToDate),
            "the first waiter is answered"
        );
        assert_eq!(
            o2,
            Ok(SyncOutcome::UpToDate),
            "the coalesced second waiter is answered too"
        );
        assert_eq!(
            backend.fetch_calls(),
            1,
            "the two requests coalesced into a single cycle"
        );
    }

    // --- Group B: the live remote gate ----------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_watch_without_a_remote_skips_the_cycle_without_latching() {
        // Findings 4/5: the sync cycle probes has_remote at start. A missing remote
        // is an honest no-op — a SyncSkipped event, a NoRemote ack, no fetch, and
        // NO state change (no SyncError, no backoff).
        let backend = FakeBackend::new();
        backend.set_has_remote(false);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();

        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncSkipped { reason, .. } => {
                // The reason NAMES the missing remote (the worker's configured
                // remote name), so the user knows exactly what to add.
                assert!(reason.contains("no remote \"origin\""), "got: {reason}");
            }
            other => panic!("expected SyncSkipped, got {other:?}"),
        }
        assert_eq!(ack_rx.await, Ok(SyncOutcome::NoRemote));
        assert_eq!(
            backend.fetch_calls(),
            0,
            "a remote-less watch never fetches"
        );
        assert!(
            no_more_outcomes(&mut events),
            "no state change latches for a remote-less watch"
        );
    }

    // --- Group C: a sync failure never downgrades a Conflicted latch ----------

    #[tokio::test(start_paused = true)]
    async fn a_sync_failure_while_conflicted_leaves_the_conflict_standing() {
        // Finding 7: a network error during a manual sync of a Conflicted watch
        // must not flip the latch to SyncError (which would lift auto-sync
        // suppression and resume a backoff cycle). The conflict stays standing.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::Conflict)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        // 1. A first manual sync conflicts and latches Conflicted.
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_matching(
            &mut events,
            Duration::from_secs(1),
            |e| matches!(e, Event::WatchStateChanged { to, .. } if *to == WatchState::Conflicted),
        )
        .await;

        // 2. An offline manual sync now fails at fetch. It is reported (SyncFailed
        //    + a Failed ack) but the Conflicted latch is untouched.
        backend.script_fetch([Err(VcsError::CommandFailed {
            op: "fetch".into(),
            status: Some(1),
            stderr: "offline".into(),
        })]);
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        assert!(matches!(
            advance_until_matching(&mut events, Duration::from_secs(1), |e| matches!(
                e,
                Event::SyncFailed { .. }
            ))
            .await,
            Event::SyncFailed { .. }
        ));
        assert!(matches!(ack_rx.await, Ok(SyncOutcome::Failed(_))));

        // No transition away from Conflicted, and no auto-sync resumption: advance
        // well past any backoff and confirm nothing further fetches or re-latches.
        let fetches = backend.fetch_calls();
        for _ in 0..5 {
            settle().await;
            tokio::time::advance(Duration::from_secs(120)).await;
        }
        assert_eq!(
            backend.fetch_calls(),
            fetches,
            "a Conflicted watch does not resume a backoff cycle"
        );
        assert!(
            no_more_outcomes(&mut events),
            "the watch stays Conflicted; no SyncError transition"
        );
    }

    // --- Round 3: deferral kinds, gate probe, drain bound, budgets ------------

    #[tokio::test(start_paused = true)]
    async fn a_deferred_manual_sync_respects_the_short_cadence_during_a_save_storm() {
        // A manual request whose first cycle abandoned (WouldClobber) is paced by
        // the short Cadence deferral. A storm of snapshot triggers (editor saves)
        // wakes the worker repeatedly; each wake must NOT re-run the network
        // cycle before the cadence deadline — otherwise the storm burns the
        // clobber cap within milliseconds and latches a spurious SyncError.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 1), remote(1, 1)]);
        backend.script_reconcile([
            Ok(ReconcileOutcome::Rebased {
                new_head: SnapshotId::new("cafef00d"),
            }),
            Ok(ReconcileOutcome::AlreadyUpToDate),
        ]);
        backend.script_advance([AdvanceOutcome::WouldClobber]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        // First cycle runs, clobbers, and arms the 500ms cadence.
        advance_until_fetch_calls(&backend, 1, Duration::from_millis(50)).await;

        // The save-storm: many triggers, NO clock movement. Every snapshot pass
        // (clean tree) wakes the loop and reaches dispatch — which must hold the
        // cadence even for the pending MANUAL request.
        for _ in 0..5 {
            tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
            settle().await;
        }
        assert_eq!(
            backend.fetch_calls(),
            1,
            "no wake may re-run the cycle before the cadence deadline"
        );

        // The cadence elapses: exactly one paced retry converges the request.
        let ev = advance_until_matching(&mut events, Duration::from_millis(300), |e| {
            matches!(e, Event::SyncPushed { .. } | Event::SyncFailed { .. })
        })
        .await;
        assert!(
            matches!(ev, Event::SyncPushed { .. }),
            "the paced retry converged without burning the clobber cap, got {ev:?}"
        );
        assert_eq!(backend.fetch_calls(), 2);
        assert!(matches!(ack_rx.await, Ok(SyncOutcome::Moved { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn a_clean_up_to_date_watch_never_touches_a_busy_gate() {
        // The gate is engaged only when locked work is needed. A clean,
        // up-to-date watch under a (foreign) op-lock holder must terminate
        // UpToDate — the dirty check and fetch need no lock — never NotRun.
        let backend = FakeBackend::new(); // clean tree, fetch defaults to (0, 0)
        let gate = FakeGate::busy();
        let (tx, _events) = spawn_sync_worker_with_gate(
            Arc::clone(&backend),
            test_cfg(),
            Arc::clone(&gate) as SharedGate,
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        settle().await;

        assert_eq!(ack_rx.await, Ok(SyncOutcome::UpToDate));
        assert_eq!(gate.probes(), 0, "no locked work: the gate is never probed");
        assert_eq!(gate.begins(), 0, "and never begun");
        assert_eq!(backend.fetch_calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_clean_ahead_only_push_never_touches_the_gate() {
        // Push-only (clean tree, local commits, unmoved remote) commits nothing
        // and moves no tree, so it needs no lock: it must push even while a
        // foreign holder owns the op gate.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(2, 0)]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let gate = FakeGate::busy();
        let (tx, _events) = spawn_sync_worker_with_gate(
            Arc::clone(&backend),
            test_cfg(),
            Arc::clone(&gate) as SharedGate,
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        settle().await;

        assert_eq!(
            ack_rx.await,
            Ok(SyncOutcome::Moved {
                pushed: Some(2),
                pulled: false
            })
        );
        assert_eq!(
            gate.probes(),
            0,
            "the push-only path never touches the gate"
        );
        assert_eq!(gate.begins(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn gate_busy_retries_reuse_one_cached_fetch() {
        // Locked work needed (dirty tree) against a busy gate: the pre-flight
        // fetch runs ONCE, is cached on the pending record, and the paced
        // Cadence retries reuse it — no fetch-per-retry storm. Once the gate
        // frees, the cycle completes still on that one fetch.
        let backend = FakeBackend::new();
        backend.set_dirty(true);
        let gate = FakeGate::busy();
        let (tx, _events) = spawn_sync_worker_with_gate(
            Arc::clone(&backend),
            test_cfg(),
            Arc::clone(&gate) as SharedGate,
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        // The first attempt runs on the input wake: fetch once, probe, defer.
        settle_until("the first attempt probed the gate", || gate.probes() >= 1).await;
        // Two provable paced retries, keeping the total clock movement well
        // inside the 4 s cache TTL (settle_until moves NO clock, so overshoot
        // under load is impossible).
        for probes in 2..=3 {
            tokio::time::advance(Duration::from_millis(600)).await;
            settle_until("a paced retry probed the gate", || gate.probes() >= probes).await;
        }
        assert_eq!(
            backend.fetch_calls(),
            1,
            "paced gate-busy retries reuse the cached pre-flight fetch"
        );
        assert_eq!(gate.begins(), 0, "a busy probe never opens an admission");

        // The gate frees: the cycle completes on the STILL-FRESH cached fetch
        // (dirty tree, nothing to integrate → pre-sync snapshot window, then
        // push). Awaiting the ack lets paused time auto-advance to the cadence.
        gate.admit();
        let outcome = timeout(Duration::from_secs(30), ack_rx)
            .await
            .expect("the freed gate completes the cycle")
            .expect("the request terminates");
        assert_eq!(outcome, SyncOutcome::UpToDate);
        assert_eq!(
            backend.fetch_calls(),
            1,
            "completion itself reused the cached fetch"
        );
        assert_eq!(
            gate.begins(),
            1,
            "the freed gate admitted the locked window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_gate_costs_zero_network_no_matter_how_long_it_holds() {
        // P3: the busy-wait NEVER re-fetches, even far past the cache TTL — the
        // TTL is consulted only at gate ACQUISITION. A holder wedging for many
        // TTLs costs exactly the one pre-flight fetch; when the gate finally
        // frees, the stale cache is replaced by exactly ONE fresh pre-flight.
        let backend = FakeBackend::new();
        backend.set_dirty(true);
        let gate = FakeGate::busy();
        let (tx, _events) = spawn_sync_worker_with_gate(
            Arc::clone(&backend),
            test_cfg(),
            Arc::clone(&gate) as SharedGate,
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        settle_until("the first attempt probed the gate", || gate.probes() >= 1).await;
        // Wedge the gate through TEN provable paced retries: ≥ 9 × 600 ms of
        // clock — more than twice the 4 s cache TTL. settle_until moves no
        // clock, so the count of retries is exact regardless of test load.
        for probes in 2..=10 {
            tokio::time::advance(Duration::from_millis(600)).await;
            settle_until("a paced retry probed the gate", || gate.probes() >= probes).await;
        }
        assert_eq!(
            backend.fetch_calls(),
            1,
            "waiting on a wedged gate is pure lock-probing: never one fetch per TTL"
        );

        // The gate frees: the cache is stale at acquisition, so exactly one
        // fresh pre-flight runs and the cycle completes. Awaiting the ack lets
        // paused time auto-advance to the cadence deadline.
        gate.admit();
        let outcome = timeout(Duration::from_secs(60), ack_rx)
            .await
            .expect("the freed gate completes the cycle")
            .expect("the request terminates");
        assert_eq!(outcome, SyncOutcome::UpToDate);
        assert_eq!(
            backend.fetch_calls(),
            2,
            "a stale cache at acquisition costs exactly one fresh fetch"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_dirty_probe_fails_before_any_network_fetch() {
        // Finding 4 (round 6): a repository whose status cannot be read (a
        // corrupt index) must fail pre-network — never a full fetch on every
        // backoff retry before erroring.
        let backend = FakeBackend::new();
        backend.fail_is_dirty();
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        advance_until_matching(&mut events, Duration::from_millis(200), |e| {
            matches!(e, Event::SyncFailed { .. })
        })
        .await;
        assert!(matches!(ack_rx.await, Ok(SyncOutcome::Failed(_))));
        assert_eq!(
            backend.fetch_calls(),
            0,
            "a status that cannot be read fails before the network"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_edit_saved_during_the_fetch_is_not_missed_by_the_early_exit() {
        // P4: the pre-flight checks dirtiness AFTER the fetch returns. An edit
        // that lands while the fetch is in flight must be seen — a cycle that
        // read dirtiness first would early-exit "up to date" and silently skip
        // the new work.
        let backend = FakeBackend::new();
        // Clean at request time; the edit lands as a side effect OF the fetch.
        backend.set_dirty_after_fetch(true);
        backend.script([Scripted::Commit(1)]); // the pre-sync snapshot captures it
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();

        let ev = advance_until_matching(&mut events, Duration::from_millis(200), |e| {
            matches!(e, Event::SyncPushed { .. } | Event::SyncFailed { .. })
        })
        .await;
        assert!(
            matches!(ev, Event::SyncPushed { .. }),
            "the mid-fetch edit was captured and pushed, got {ev:?}"
        );
        assert!(
            matches!(ack_rx.await, Ok(SyncOutcome::Moved { .. })),
            "never a false 'up to date' for work saved during the fetch"
        );
        assert_eq!(
            backend.snapshot_calls(),
            1,
            "the locked window's pre-sync snapshot captured the edit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_gate_free_push_counts_commits_at_push_time() {
        // Finding 3: commits landing between the fetch and the push are pushed —
        // the reported count must include them when the backend can count at
        // push time (ahead_of_upstream), not the stale fetch-time `ahead`.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(2, 0)]);
        backend.set_ahead_now(3); // one more commit landed after the fetch
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_millis(200)).await {
            Event::SyncPushed { commits, .. } => {
                assert_eq!(commits, 3, "the count is resolved at push time");
            }
            other => panic!("expected SyncPushed, got {other:?}"),
        }
        assert_eq!(
            ack_rx.await,
            Ok(SyncOutcome::Moved {
                pushed: Some(3),
                pulled: false
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_tracking_ref_uses_the_fetch_time_count() {
        // Deleted-remote-branch scenario, zero-mutation variant: the fetch
        // reports the branch deleted with a stale tracking ref surviving
        // (stale_tracking_ref), so the push count is the fetch-time ahead
        // (full history — what the push recreates) and the stale local read
        // (poisoned here via ahead_of_upstream) is never consulted. A
        // never-pushed branch does NOT set the flag and keeps the push-time
        // count. No ref is mutated outside a locked window.
        let backend = FakeBackend::new();
        backend.script_fetch([remote_with_stale_tracking(2)]);
        // The stale tracking ref would claim ahead == 1; it must be ignored.
        backend.set_ahead_now(1);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_millis(200)).await {
            Event::SyncPushed { commits, .. } => {
                assert_eq!(commits, 2, "the fetch-time full-history count is used");
            }
            other => panic!("expected SyncPushed, got {other:?}"),
        }
        assert_eq!(
            ack_rx.await,
            Ok(SyncOutcome::Moved {
                pushed: Some(2),
                pulled: false
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_drain_bounds_gate_busy_attempts_and_reports_did_not_run() {
        // Shutdown drain against a gate that NEVER frees (a foreign/peer holder)
        // while locked work is genuinely needed (dirty tree): the drain services
        // a small bounded number of paced attempts, then terminates the ack
        // honestly as NotRun — it must not hang for the whole outer drain
        // budget, and the retries reuse one cached fetch.
        let backend = FakeBackend::new();
        backend.set_dirty(true);
        let gate = FakeGate::busy();
        let (tx, _events) = spawn_sync_worker_with_gate(
            Arc::clone(&backend),
            test_cfg(),
            Arc::clone(&gate) as SharedGate,
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();
        settle().await;
        // Close the channel: the worker moves to the drain with the request
        // still deferred on the busy gate.
        drop(tx);

        let outcome = timeout(Duration::from_secs(30), ack_rx)
            .await
            .expect("the bounded drain resolves the ack quickly")
            .expect("the drain terminates the request rather than dropping it");
        assert!(
            matches!(outcome, SyncOutcome::NotRun(_)),
            "a stuck gate terminates as an honest NotRun, got {outcome:?}"
        );
        assert_eq!(
            backend.fetch_calls(),
            1,
            "the drain's gate-busy retries reuse the one cached fetch"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_manual_request_coalescing_onto_a_burned_clobber_budget_gets_a_fresh_one() {
        // Two of the three clobber attempts are burned; a MANUAL request then
        // coalesces onto the pending record. Manual is fresh activity: the budget
        // resets, so the request survives two MORE clobbers and converges on the
        // fifth cycle — without the reset the third clobber would latch SyncError.
        let backend = FakeBackend::new();
        backend.script_fetch((0..5).map(|_| remote(1, 1)));
        backend.script_reconcile((0..5).map(|_| {
            Ok(ReconcileOutcome::Rebased {
                new_head: SnapshotId::new("cafef00d"),
            })
        }));
        backend.script_advance([
            AdvanceOutcome::WouldClobber,
            AdvanceOutcome::WouldClobber,
            AdvanceOutcome::WouldClobber,
            AdvanceOutcome::WouldClobber,
            AdvanceOutcome::Advanced,
        ]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack1_tx, ack1_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack1_tx),
        })
        .unwrap();
        // Burn two of the three clobber attempts.
        advance_until_fetch_calls(&backend, 2, Duration::from_millis(300)).await;

        // A second manual request coalesces onto the pending record: fresh budget.
        let (ack2_tx, ack2_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack2_tx),
        })
        .unwrap();

        // Two more clobbers land (a spent budget would terminate on the first of
        // them), then the fifth cycle converges.
        let ev = advance_until_matching(&mut events, Duration::from_millis(300), |e| {
            matches!(e, Event::SyncPushed { .. } | Event::SyncFailed { .. })
        })
        .await;
        assert!(
            matches!(ev, Event::SyncPushed { .. }),
            "the coalesced manual request got a fresh clobber budget, got {ev:?}"
        );
        assert_eq!(backend.advance_calls(), 5, "four refusals plus the success");
        assert!(matches!(ack1_rx.await, Ok(SyncOutcome::Moved { .. })));
        assert!(matches!(ack2_rx.await, Ok(SyncOutcome::Moved { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_remote_probe_is_a_real_failure_not_no_remote() {
        // `has_remote` erroring (an unreadable repository config) is a repository
        // fault, not a missing remote: the cycle terminates as Failed with the
        // SyncError latch — never masked as a benign NoRemote skip.
        let backend = FakeBackend::new();
        backend.fail_has_remote();
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: Some(ack_tx),
        })
        .unwrap();

        let ev = advance_until_matching(&mut events, Duration::from_millis(200), |e| {
            matches!(e, Event::SyncFailed { .. } | Event::SyncSkipped { .. })
        })
        .await;
        assert!(
            matches!(ev, Event::SyncFailed { .. }),
            "a probe error must surface as a failure, not a skip, got {ev:?}"
        );
        assert!(matches!(ack_rx.await, Ok(SyncOutcome::Failed(_))));
        assert_eq!(
            backend.fetch_calls(),
            0,
            "the cycle never reached the fetch"
        );
    }

    // --- Group D: centralized sync dispatch is always reached -----------------

    #[tokio::test(start_paused = true)]
    async fn queued_sync_runs_after_a_snapshot_retry_resolves_via_its_timer() {
        // A failing snapshot arms a retry; a queued (auto) sync defers. When the
        // retry resolves on its own timer, the pending sync still runs (F8).
        let backend = FakeBackend::new();
        backend.script([Scripted::Fail, Scripted::Commit(1), Scripted::Clean]);
        backend.script_fetch([remote(1, 0)]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::interval()))
            .unwrap();
        advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::SnapshotFailed { .. })
        })
        .await;

        // Queue an automatic sync while the snapshot retry is armed: it defers.
        tx.send(WatchInput::RequestSync {
            manual: false,
            ack: None,
        })
        .unwrap();

        // Advancing time fires the retry timer (the snapshot commits), and the
        // pending sync then runs — proven by the push it makes.
        let ev = advance_until_matching(&mut events, Duration::from_secs(31), |e| {
            matches!(e, Event::SyncPushed { .. } | Event::SyncFailed { .. })
        })
        .await;
        assert!(
            matches!(ev, Event::SyncPushed { .. }),
            "the queued sync ran once the retry resolved, got {ev:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_manual_sync_runs_even_with_the_snapshot_retry_budget_exhausted() {
        // A repository whose snapshots keep failing exhausts the retry budget; a
        // later manual sync must still run rather than be swallowed (F9).
        let cfg = EngineConfig {
            unsafe_repoll_max_attempts: 2,
            ..test_cfg()
        };
        let backend = FakeBackend::new();
        backend.set_always_fail();
        backend.script_fetch([remote(0, 0)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), cfg);

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::SnapshotFailed { .. })
        })
        .await;
        // Drive the retry to exhaustion (max 2 ticks at the 30s cadence).
        for _ in 0..4 {
            settle().await;
            tokio::time::advance(Duration::from_secs(31)).await;
        }
        assert_eq!(
            backend.fetch_calls(),
            0,
            "no sync ran while snapshots failed"
        );

        // A manual sync now runs despite the exhausted budget.
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_fetch_calls(&backend, 1, Duration::from_secs(1)).await;
        assert_eq!(backend.fetch_calls(), 1, "the manual sync ran");
    }

    #[tokio::test(start_paused = true)]
    async fn auto_syncs_during_an_editing_storm_run_only_at_the_backoff_deadline() {
        // While a SyncError backoff stands, a post-snapshot auto-sync must not
        // fire a doomed fetch after every save: snapshots stay fast and the
        // coalesced sync runs only when the backoff deadline arrives (F10).
        let cfg = EngineConfig {
            sync_backoff_base: Duration::from_secs(60),
            ..test_cfg()
        };
        let backend = FakeBackend::new();
        backend.script_fetch([
            Err(VcsError::CommandFailed {
                op: "fetch".into(),
                status: Some(1),
                stderr: "unreachable".into(),
            }),
            remote(0, 0),
        ]);
        backend.script([Scripted::Commit(1), Scripted::Clean]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), cfg);

        // 1. A sync fails → SyncError with a 60s backoff.
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::SyncFailed { .. })
        })
        .await;
        assert_eq!(backend.fetch_calls(), 1);

        // 2. A save lands (the editing storm): the snapshot is fast, and its
        //    post-snapshot auto-sync is deferred by the backoff — no fetch.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        advance_until_matching(&mut events, Duration::from_secs(1), |e| {
            matches!(e, Event::SnapshotCompleted { .. })
        })
        .await;
        settle().await;
        assert_eq!(
            backend.fetch_calls(),
            1,
            "the auto-sync did not fetch while backed off"
        );

        // 3. At the backoff deadline the coalesced sync runs (a second fetch).
        advance_until_fetch_calls(&backend, 2, Duration::from_secs(10)).await;
        assert_eq!(backend.fetch_calls(), 2, "the sync ran at the deadline");
    }

    #[tokio::test(start_paused = true)]
    async fn sync_nonfastforward_reloops_in_cycle_and_converges() {
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 0), remote(1, 0)]);
        backend.script_reconcile([
            Ok(ReconcileOutcome::AlreadyUpToDate),
            Ok(ReconcileOutcome::AlreadyUpToDate),
        ]);
        // First push loses the race; the re-fetch+re-push converges.
        backend.script_push([Ok(PushOutcome::NonFastForward), Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::SyncPushed { .. }
        ));
        assert_eq!(
            backend.push_calls(),
            2,
            "the race re-looped once and converged"
        );
        assert_eq!(backend.fetch_calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn sync_nonfastforward_past_the_cap_degrades_to_sync_error() {
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 0), remote(1, 0), remote(1, 0)]);
        backend.script_reconcile([
            Ok(ReconcileOutcome::AlreadyUpToDate),
            Ok(ReconcileOutcome::AlreadyUpToDate),
            Ok(ReconcileOutcome::AlreadyUpToDate),
        ]);
        backend.script_push([
            Ok(PushOutcome::NonFastForward),
            Ok(PushOutcome::NonFastForward),
            Ok(PushOutcome::NonFastForward),
        ]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::SyncFailed { .. }
        ));
        assert_eq!(
            backend.push_calls(),
            SYNC_MAX_ATTEMPTS as usize,
            "the in-cycle race re-loop is capped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sync_dirty_tree_with_unmoved_remote_snapshots_and_pushes() {
        // Regression: an unmoved remote (ahead == 0, behind == 0) is NOT
        // "nothing to do" when the tree has uncommitted edits. The cycle must
        // enter the locked window, commit the pre-sync snapshot, and push it.
        let backend = FakeBackend::new();
        backend.set_dirty(true);
        backend.script_fetch([remote(0, 0)]);
        // The locked window's pre-sync snapshot commits the dirty work.
        backend.script([Scripted::Commit(1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::AlreadyUpToDate)]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            // The pre-sync commit is the one commit pushed (ahead was 0).
            Event::SyncPushed { commits, .. } => assert_eq!(commits, 1),
            other => panic!("expected SyncPushed, got {other:?}"),
        }
        assert_eq!(backend.push_calls(), 1, "the dirty tree was pushed");
    }

    #[tokio::test(start_paused = true)]
    async fn sync_pushed_count_includes_the_pre_sync_snapshot_commit() {
        // With one commit already ahead AND uncommitted local work, the pushed
        // count is the fetch-time ahead (1) PLUS the pre-sync snapshot commit
        // (1) — the pre-sync commit must not be undercounted.
        let backend = FakeBackend::new();
        backend.set_dirty(true);
        backend.script_fetch([remote(1, 0)]);
        backend.script([Scripted::Commit(1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::AlreadyUpToDate)]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncPushed { commits, .. } => {
                assert_eq!(commits, 2, "one already-ahead commit plus the pre-sync one")
            }
            other => panic!("expected SyncPushed, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_snapshot_drives_an_automatic_sync() {
        // A committed snapshot on a sync-enabled watch fires an automatic sync,
        // so the new local history reaches the remote without waiting on a timer.
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        backend.script_fetch([remote(1, 0)]);
        backend.script_reconcile([Ok(ReconcileOutcome::AlreadyUpToDate)]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        // A plain filesystem trigger — no explicit sync request.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::SnapshotCompleted { .. }
        ));
        // The post-snapshot trigger runs a real cycle that pushes.
        match advance_until_event(&mut events, Duration::from_secs(1)).await {
            Event::SyncPushed { .. } => {}
            other => panic!("expected a post-snapshot SyncPushed, got {other:?}"),
        }
        assert_eq!(
            backend.fetch_calls(),
            1,
            "the snapshot drove one sync cycle"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_snapshot_on_a_conflicted_watch_does_not_auto_sync() {
        // While a conflict latches, a successful snapshot must NOT fire an
        // automatic sync — auto-sync is suppressed until a manual resolve.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 1)]);
        backend.script_reconcile([Ok(ReconcileOutcome::Conflict)]);
        let (tx, mut events) = spawn_sync_worker(Arc::clone(&backend), test_cfg());

        // Latch the conflict via an explicit sync.
        tx.send(WatchInput::RequestSync {
            manual: true,
            ack: None,
        })
        .unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::SyncConflict { .. }
        ));
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::WatchStateChanged {
                to: WatchState::Conflicted,
                ..
            }
        ));

        // A snapshot still commits while latched, but its auto-sync is suppressed.
        let fetches = backend.fetch_calls();
        backend.script([Scripted::Commit(1)]);
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        assert!(matches!(
            advance_until_event(&mut events, Duration::from_secs(1)).await,
            Event::SnapshotCompleted { .. }
        ));
        for _ in 0..10 {
            settle().await;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(
            backend.fetch_calls(),
            fetches,
            "a conflicted watch's snapshot must not auto-sync"
        );
    }

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            lock_retry_attempts: 5,
            lock_retry_base: Duration::from_secs(2),
            unsafe_repoll_interval: Duration::from_secs(30),
            unsafe_repoll_max_attempts: 480,
            shutdown_drain_timeout: Duration::from_secs(30),
            gate_busy_retry_interval: Duration::from_millis(500),
            sync_network_timeout: Duration::from_secs(60),
            sync_backoff_base: Duration::from_secs(60),
            sync_backoff_cap: Duration::from_secs(60 * 60),
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

    /// Settles (WITHOUT advancing the paused clock) until `cond` holds, so a
    /// test can prove an event happened at the current clock reading — however
    /// slowly the blocking pool runs under parallel test load — before it moves
    /// time again. The budget is generous REAL time (not an iteration count):
    /// a saturated runner that starves the blocking pool for hundreds of
    /// milliseconds must not flake the exact-count assertions built on this
    /// (the same deflaking posture as the instance-probe tests). Panics only if
    /// the condition never settles within the wall-clock budget.
    async fn settle_until(what: &str, cond: impl Fn() -> bool) {
        let start = std::time::Instant::now();
        while !cond() {
            if start.elapsed() > Duration::from_secs(30) {
                panic!("never settled within 30s of real time: {what}");
            }
            settle().await;
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
    ///
    /// Skips [`Event::SnapshotStarted`] and [`Event::SnapshotSkipped`]: the
    /// pre-commit signal and its no-commit closer bracket every backend sweep,
    /// and these outcome-oriented assertions care about what a snapshot *did*
    /// (committed, failed, changed state), not that one was attempted. The
    /// dedicated bracket tests read the raw stream instead.
    async fn advance_until_event(events: &mut EventReceiver, step: Duration) -> Event {
        for _ in 0..500 {
            settle().await;
            match events.try_recv() {
                Ok(Event::SnapshotStarted { .. }) | Ok(Event::SnapshotSkipped { .. }) => continue,
                Ok(ev) => return ev,
                Err(TryRecvError::Empty) => {}
                Err(other) => panic!("event channel error: {other:?}"),
            }
            tokio::time::advance(step).await;
        }
        panic!("no event arrived within the step budget");
    }

    /// Drains any buffered [`Event::SnapshotStarted`]/[`Event::SnapshotSkipped`]
    /// bracket events and returns whether no *effect* event (Completed / Failed
    /// / StateChanged / …) remains — i.e. passes may have announced attempts
    /// that concluded without a commit, but nothing else was reported. Replaces
    /// a bare `events.try_recv().is_err()` now that every sweep is bracketed.
    fn no_more_outcomes(events: &mut EventReceiver) -> bool {
        loop {
            match events.try_recv() {
                Ok(Event::SnapshotStarted { .. }) | Ok(Event::SnapshotSkipped { .. }) => continue,
                Ok(_) => return false,
                Err(_) => return true,
            }
        }
    }

    /// Advances the paused clock in bounded steps until the backend has seen at
    /// least `n` snapshot calls (for passes that make a call but emit no event,
    /// e.g. a converging `Clean` re-attempt driven by the retry timer).
    async fn advance_until_snapshot_calls(backend: &FakeBackend, n: usize, step: Duration) {
        for _ in 0..500 {
            settle().await;
            if backend.snapshot_calls() >= n {
                return;
            }
            tokio::time::advance(step).await;
        }
        panic!(
            "snapshot_calls never reached {n} (was {})",
            backend.snapshot_calls()
        );
    }

    /// Returns the next event WITHOUT advancing the paused clock, or `None` if
    /// none arrives while settling. A timer-delayed snapshot needs a clock
    /// advance to fire, so this distinguishes "processed immediately" from
    /// "waiting for the retry timer".
    async fn recv_no_advance(events: &mut EventReceiver) -> Option<Event> {
        for _ in 0..200 {
            settle().await;
            match events.try_recv() {
                Ok(Event::SnapshotStarted { .. }) | Ok(Event::SnapshotSkipped { .. }) => continue,
                Ok(ev) => return Some(ev),
                Err(TryRecvError::Empty) => {}
                Err(other) => panic!("event channel error: {other:?}"),
            }
        }
        None
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
        assert!(
            no_more_outcomes(&mut events),
            "exactly one snapshot outcome"
        );
        // One commit plus one converging re-check.
        assert_eq!(backend.snapshot_calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_started_precedes_completed_in_a_pass() {
        // The pre-commit signal is emitted before the outcome for the same pass
        // and carries the same trigger, so a subscriber can bracket the commit
        // window. Reads the raw stream (no Started filtering).
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]); // then Clean on the re-check
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let mut seen: Vec<Event> = Vec::new();
        for _ in 0..500 {
            settle().await;
            while let Ok(ev) = events.try_recv() {
                seen.push(ev);
            }
            if seen
                .iter()
                .any(|e| matches!(e, Event::SnapshotCompleted { .. }))
            {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        }

        let started = seen.iter().position(|e| {
            matches!(
                e,
                Event::SnapshotStarted {
                    trigger: Trigger::Event,
                    ..
                }
            )
        });
        let completed = seen
            .iter()
            .position(|e| matches!(e, Event::SnapshotCompleted { .. }));
        let started = started.expect("a SnapshotStarted must precede the commit");
        let completed = completed.expect("the commit must complete");
        assert!(
            started < completed,
            "Started must precede Completed, got {seen:?}"
        );
    }

    /// Collects raw events (no filtering) while advancing the paused clock,
    /// until `done` accepts the collected list. Panics if the budget is spent
    /// first.
    async fn collect_raw_until(
        events: &mut EventReceiver,
        step: Duration,
        done: impl Fn(&[Event]) -> bool,
    ) -> Vec<Event> {
        let mut seen: Vec<Event> = Vec::new();
        for _ in 0..500 {
            settle().await;
            while let Ok(ev) = events.try_recv() {
                seen.push(ev);
            }
            if done(&seen) {
                return seen;
            }
            tokio::time::advance(step).await;
        }
        panic!("expected events never arrived; got {seen:?}");
    }

    /// Asserts the started→outcome bracket invariant over a raw event stream
    /// from a single watch: scanning in order, every [`Event::SnapshotStarted`]
    /// is closed by exactly one Completed/Failed/Skipped before the next
    /// Started opens, no outcome arrives with no bracket open, and the stream
    /// does not end mid-bracket. Non-bracket events (state changes) may
    /// interleave freely.
    fn assert_brackets_balanced(events: &[Event]) {
        let mut open = false;
        for ev in events {
            match ev {
                Event::SnapshotStarted { .. } => {
                    assert!(
                        !open,
                        "a second Started before the previous bracket closed: {events:?}"
                    );
                    open = true;
                }
                Event::SnapshotCompleted { .. }
                | Event::SnapshotFailed { .. }
                | Event::SnapshotSkipped { .. } => {
                    assert!(open, "an outcome with no open Started bracket: {events:?}");
                    open = false;
                }
                _ => {}
            }
        }
        assert!(
            !open,
            "stream ended with an unclosed Started bracket: {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn clean_pass_closes_the_started_bracket_with_skipped() {
        // A pass whose sweep finds a clean tree commits nothing, but its
        // SnapshotStarted must still be closed — by SnapshotSkipped(Clean) with
        // the same trigger — so a journaling subscriber's begin record never
        // dangles after a no-op sweep.
        let backend = FakeBackend::new();
        backend.script([Scripted::Clean]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::interval()))
            .unwrap();

        let seen = collect_raw_until(&mut events, Duration::from_secs(1), |evs| {
            evs.iter()
                .any(|e| matches!(e, Event::SnapshotSkipped { .. }))
        })
        .await;

        assert_brackets_balanced(&seen);
        let started_count = seen
            .iter()
            .filter(|e| matches!(e, Event::SnapshotStarted { .. }))
            .count();
        assert_eq!(started_count, 1, "one sweep, one bracket: {seen:?}");
        match seen
            .iter()
            .find(|e| matches!(e, Event::SnapshotSkipped { .. }))
        {
            Some(Event::SnapshotSkipped {
                trigger, reason, ..
            }) => {
                assert_eq!(*trigger, Trigger::Interval);
                assert_eq!(*reason, SkipReason::Clean);
            }
            other => panic!("expected SnapshotSkipped, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lock_exhausted_pass_closes_the_started_bracket_with_skipped() {
        // A pass that gives up on a permanently contended lock (requeueing, not
        // deleting) commits nothing and reports no failure — but its
        // SnapshotStarted must still close, as SnapshotSkipped(LockContended).
        // All the in-bracket lock retries share the one bracket.
        let backend = FakeBackend::new();
        backend.script([
            Scripted::Lock,
            Scripted::Lock,
            Scripted::Lock,
            Scripted::Lock,
            Scripted::Lock,
        ]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // Step past the 2/4/8/16s lock backoffs.
        let seen = collect_raw_until(&mut events, Duration::from_secs(16), |evs| {
            evs.iter()
                .any(|e| matches!(e, Event::SnapshotSkipped { .. }))
        })
        .await;

        assert_brackets_balanced(&seen);
        let started_count = seen
            .iter()
            .filter(|e| matches!(e, Event::SnapshotStarted { .. }))
            .count();
        assert_eq!(
            started_count, 1,
            "the lock retries stay within one bracket: {seen:?}"
        );
        match seen
            .iter()
            .find(|e| matches!(e, Event::SnapshotSkipped { .. }))
        {
            Some(Event::SnapshotSkipped {
                trigger, reason, ..
            }) => {
                assert_eq!(*trigger, Trigger::Event);
                assert_eq!(*reason, SkipReason::LockContended);
            }
            other => panic!("expected SnapshotSkipped, got {other:?}"),
        }
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, Event::SnapshotFailed { .. })),
            "a contended lock is skipped, not failed: {seen:?}"
        );
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
        // No effect event: an interval on a clean tree commits nothing (the
        // sweep's Started/Skipped bracket is filtered by the helper).
        assert!(
            no_more_outcomes(&mut events),
            "an interval on a clean tree is a no-op"
        );
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
            no_more_outcomes(&mut events),
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
        assert!(no_more_outcomes(&mut events), "then it converges");
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
            no_more_outcomes(&mut events),
            "a permanently locked repo must not report a snapshot outcome"
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
    async fn gate_busy_requeues_without_snapshotting_then_proceeds_when_free() {
        // A busy gate (another holder owns the watch's op lock) must requeue like
        // a contended index lock: no backend call, and — crucially — no event
        // bracket opened (nothing to close). Once the gate frees, a fresh trigger
        // drives the preserved change to a commit.
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let gate = FakeGate::busy();
        let (tx, mut events, _counter) = spawn_worker_with_gate(
            Arc::clone(&backend),
            test_cfg(),
            Arc::clone(&gate) as SharedGate,
        );

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // Let the worker run: the gate refuses, so nothing commits and no outcome
        // (or Started bracket) is emitted.
        for _ in 0..20 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        settle().await;
        assert_eq!(
            backend.snapshot_calls(),
            0,
            "a busy gate must block the backend commit entirely"
        );
        assert!(
            gate.begins() >= 1,
            "the worker must have consulted the gate"
        );
        let mut seen: Vec<Event> = Vec::new();
        while let Ok(ev) = events.try_recv() {
            seen.push(ev);
        }
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, Event::SnapshotStarted { .. })),
            "a busy gate opens no bracket: {seen:?}"
        );
        assert_brackets_balanced(&seen);

        // The gate frees; a fresh trigger drives the preserved change to a commit.
        gate.admit();
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "once the gate frees, the requeued change commits, got {ev:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gate_busy_self_retries_and_converges_without_a_fresh_trigger() {
        // F7: an event-only watch that hit a busy gate must converge once the gate
        // frees, driven by the short bounded self-retry timer — NOT waiting for a
        // fresh filesystem trigger (which an event-only watch may never get). This
        // closes the reload-teardown corner where a preserved change could strand.
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let gate = FakeGate::busy();
        let (tx, mut events, _counter) = spawn_worker_with_gate(
            Arc::clone(&backend),
            test_cfg(),
            Arc::clone(&gate) as SharedGate,
        );

        // One trigger; the gate refuses, so the change is preserved and a short
        // self-retry is armed. No fresh trigger is sent after this.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        for _ in 0..10 {
            settle().await;
            tokio::time::advance(Duration::from_millis(500)).await;
        }
        assert_eq!(
            backend.snapshot_calls(),
            0,
            "a busy gate blocks the commit while it is refused"
        );
        assert!(
            gate.begins() >= 2,
            "the self-retry must re-attempt the gate on its own timer, got {} begins",
            gate.begins()
        );

        // The gate frees. WITHOUT any new trigger, the self-retry timer alone must
        // drive the preserved change to a commit.
        gate.admit();
        let ev = advance_until_event(&mut events, Duration::from_millis(500)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the gate-busy self-retry must converge with no fresh trigger, got {ev:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gate_error_is_surfaced_and_retried_like_a_failed_probe() {
        // A gate that cannot even be evaluated (op-lock/journal I/O trouble)
        // preserves the pending change and surfaces one SnapshotFailed +
        // snapshots-failing state, exactly like a failed safe-state probe — never
        // a silent unbracketed mutation.
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let gate = FakeGate::failing();
        let (tx, mut events, _counter) =
            spawn_worker_with_gate(Arc::clone(&backend), test_cfg(), gate as SharedGate);

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(ev, Event::SnapshotFailed { .. }),
            "a gate-evaluation failure is surfaced, got {ev:?}"
        );
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(
                ev,
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    trouble: Some(TroubleKind::SnapshotsFailing),
                    ..
                }
            ),
            "a gate failure surfaces as snapshots-failing, got {ev:?}"
        );
        assert_eq!(
            backend.snapshot_calls(),
            0,
            "a gate that never admits must never let the backend mutate"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gate_busy_window_gets_a_fresh_budget_after_a_spent_failure_episode() {
        // R2: a Failure/Unsafe episode that spends its bounded budget must NOT
        // carry that count into a following GateBusy window — a different cadence
        // class. With the bug, `enter_gate_busy` inherited the spent budget, the
        // short self-retry timer never fired again, and the preserved change
        // stranded until a fresh trigger. The cadence-class transition now starts a
        // fresh budget, so the change still converges on the self-retry alone.
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let gate = FakeGate::failing();
        let mut cfg = test_cfg();
        // One tick spends the (long-cadence) Failure budget: at the Failure→GateBusy
        // transition the buggy code would carry attempts==max and mark GateBusy
        // exhausted at once.
        cfg.unsafe_repoll_max_attempts = 1;
        let (tx, mut events, _counter) =
            spawn_worker_with_gate(Arc::clone(&backend), cfg, Arc::clone(&gate) as SharedGate);

        // Trigger: the failing gate arms the Failure retry (attempts still 0). The
        // failure is emitted synchronously on this first pass, so no clock advance
        // is needed — keeping the budget un-ticked before the transition.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = recv_no_advance(&mut events)
            .await
            .expect("the failing gate surfaces one failure");
        assert!(
            matches!(ev, Event::SnapshotFailed { .. }),
            "the failing gate arms the Failure retry, got {ev:?}"
        );
        // `enter_failure` also emits the snapshots-failing state change; drain it so
        // the convergence assertion below cannot mistake it for progress.
        let ev = recv_no_advance(&mut events).await;
        assert!(
            matches!(
                ev,
                Some(Event::WatchStateChanged {
                    to: WatchState::Attention,
                    ..
                })
            ),
            "the failure surfaces snapshots-failing, got {ev:?}"
        );

        // The failure clears but the op lock is now held by a peer: flip the gate
        // to busy, then let one 30 s Failure tick fire — its pass sees a BUSY gate
        // and transitions Failure → GateBusy. No fresh trigger is sent.
        gate.set_fail(false); // begin now returns Ok(None) = busy
        settle().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        settle().await;
        assert_eq!(
            backend.snapshot_calls(),
            0,
            "still no commit: the gate is busy"
        );

        // The gate frees. WITHOUT any fresh trigger, only the short GateBusy
        // self-retry can drive convergence — which it can only do if that
        // transition gave it a fresh budget rather than the spent Failure one.
        gate.admit();
        let ev = advance_until_event(&mut events, Duration::from_millis(500)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the gate-busy self-retry must converge on a fresh budget after a spent \
             failure episode, with no new trigger, got {ev:?}"
        );
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
        // ...and it moves the watch into the snapshots-failing state.
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(
                ev,
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    trouble: Some(TroubleKind::SnapshotsFailing),
                    ..
                }
            ),
            "a hard failure surfaces as a snapshots-failing state, got {ev:?}"
        );

        // With no new trigger, the retry timer re-attempts and the change lands.
        let ev = advance_until_event(&mut events, Duration::from_secs(30)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the preserved change must converge via the retry timer, got {ev:?}"
        );
        // The recovery clears the snapshots-failing state back to Ok.
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(
                ev,
                Event::WatchStateChanged {
                    to: WatchState::Ok,
                    ..
                }
            ),
            "a successful snapshot clears the failing state, got {ev:?}"
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
            // Consumed by the startup probe (item 2): a safe repo starts Ok.
            Ok(SafeState::Safe),
            // The real pass's probe fails, then the retry's probe succeeds.
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
        // The failure moves the watch to Attention (snapshots-failing).
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(
                ev,
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    trouble: Some(TroubleKind::SnapshotsFailing),
                    ..
                }
            ),
            "a failing probe surfaces as a snapshots-failing state, got {ev:?}"
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
                    trouble,
                    ..
                } => {
                    saw_attention = true;
                    assert_eq!(
                        trouble,
                        Some(TroubleKind::Degraded),
                        "a panicked backend call is degraded, not a dead signal source"
                    );
                }
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
    async fn panic_attention_self_clears_on_the_next_successful_pass() {
        // VRD-39: `Degraded` (the panic kind) is failure-class — the watch must
        // return to `Ok` the moment a subsequent pass proves it healthy again,
        // not stay parked in Attention until a daemon restart.
        let backend = FakeBackend::new();
        backend.script([Scripted::Panic, Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // Drain the panic's failure + Attention transition.
        let mut saw_attention = false;
        for _ in 0..2 {
            if let Event::WatchStateChanged {
                to: WatchState::Attention,
                trouble: Some(TroubleKind::Degraded),
                ..
            } = advance_until_event(&mut events, Duration::from_secs(1)).await
            {
                saw_attention = true;
            }
        }
        assert!(saw_attention, "the panic must move the watch to Attention");

        // A later trigger snapshots successfully: the watch must self-clear
        // back to Ok, not stay latched in Attention.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let mut saw_completed = false;
        let mut saw_recovery = false;
        for _ in 0..2 {
            match advance_until_event(&mut events, Duration::from_secs(1)).await {
                Event::SnapshotCompleted { .. } => saw_completed = true,
                Event::WatchStateChanged {
                    from: WatchState::Attention,
                    to: WatchState::Ok,
                    trouble: None,
                    ..
                } => saw_recovery = true,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(
            saw_completed,
            "the recovering trigger must actually snapshot"
        );
        assert!(
            saw_recovery,
            "a successful pass after a panic must clear Attention back to Ok"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_quarantining_pass_moves_to_attention_and_emits_the_event() {
        // VRD-22: a pass that withholds secrets emits `snapshot.quarantined` AND
        // moves the watch to Attention/SecretsQuarantined carrying the count.
        let backend = FakeBackend::new();
        backend.script([Scripted::Quarantine(2)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let mut saw_event = false;
        let mut saw_attention = false;
        for _ in 0..4 {
            match advance_until_event(&mut events, Duration::from_secs(1)).await {
                Event::SnapshotQuarantined { count, watch } => {
                    assert_eq!(count, 2);
                    assert_eq!(watch, "w");
                    saw_event = true;
                }
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    trouble: Some(TroubleKind::SecretsQuarantined { count }),
                    ..
                } => {
                    assert_eq!(count, 2);
                    saw_attention = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
            if saw_event && saw_attention {
                break;
            }
        }
        assert!(
            saw_event,
            "a quarantining pass must emit snapshot.quarantined"
        );
        assert!(
            saw_attention,
            "a quarantining pass must move the watch to Attention/SecretsQuarantined"
        );
        // Nothing was committed — the secret was the only change.
        assert!(no_more_outcomes(&mut events));
    }

    #[tokio::test(start_paused = true)]
    async fn quarantine_attention_self_clears_on_a_later_clean_pass() {
        // VRD-22: quarantine is self-clearing — once the secret is gone, the next
        // pass finds nothing to withhold and the watch returns to Ok on its own.
        let backend = FakeBackend::new();
        backend.script([Scripted::Quarantine(1), Scripted::Clean]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let mut saw_attention = false;
        for _ in 0..3 {
            match advance_until_event(&mut events, Duration::from_secs(1)).await {
                Event::SnapshotQuarantined { .. } => {}
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    trouble: Some(TroubleKind::SecretsQuarantined { .. }),
                    ..
                } => {
                    saw_attention = true;
                    break;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(
            saw_attention,
            "the first pass must quarantine into Attention"
        );

        // The user removed the secret; the next pass is clean and self-clears.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::WatchStateChanged {
                from: WatchState::Attention,
                to: WatchState::Ok,
                trouble: None,
                ..
            } => {}
            other => panic!("expected Attention->Ok recovery, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pass_with_no_withheld_secrets_never_asserts_quarantine() {
        // A normal (or disabled-scanner) pass withholds nothing, so no
        // `snapshot.quarantined` is emitted and the watch stays Ok.
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        // The only effect event is the commit; no quarantine, no Attention.
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "expected a plain commit, got {ev:?}"
        );
        wait_snapshot_calls(&backend, 2).await;
        assert!(
            no_more_outcomes(&mut events),
            "a non-withholding pass must not emit quarantine or an Attention change"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn latching_trouble_does_not_clear_on_a_successful_pass() {
        // VRD-39: `SourceDied` latches — a successful pass squeezed out of
        // this same (dying) worker proves nothing about whether the signal
        // source is alive again, only the daemon's engine rebuild does. A
        // successful pass here must NOT silently clear it back to Ok, unlike
        // a failure-class kind (SnapshotsFailing/Degraded).
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trouble {
            kind: TroubleKind::SourceDied,
            detail: "watch task ended abnormally".into(),
        })
        .unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::WatchStateChanged {
                to: WatchState::Attention,
                trouble: Some(TroubleKind::SourceDied),
                ..
            } => {}
            other => panic!("expected the latching Attention transition, got {other:?}"),
        }

        // A trigger snapshots successfully, but the latching trouble must
        // stay: no WatchStateChanged follows the commit.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(ev, Event::SnapshotCompleted { .. }),
            "the trigger must still snapshot successfully, got {ev:?}"
        );
        let followup = recv_no_advance(&mut events).await;
        assert!(
            followup.is_none(),
            "a latching trouble must not self-clear on a successful pass, got {followup:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn latching_trouble_survives_an_unrelated_transition_attempt_too() {
        // The latch is enforced centrally in `set_state`, not only at the
        // recovery call site: an unrelated transition attempt (here, an
        // unsafe-repo pause) must not silently clobber a latched trouble
        // either, or the human/rebuild-only condition it records would be
        // lost the moment anything else happens to the watch.
        let backend = FakeBackend::new();
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trouble {
            kind: TroubleKind::SourceDied,
            detail: "watch task ended abnormally".into(),
        })
        .unwrap();
        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(
            matches!(
                ev,
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    trouble: Some(TroubleKind::SourceDied),
                    ..
                }
            ),
            "expected the latching Attention transition, got {ev:?}"
        );

        // The repo goes unsafe and a trigger drives a pass into it: normally
        // this pauses the watch (see `enter_unsafe`), but the latch must
        // refuse the overwrite.
        backend.set_safe(SafeState::Unsafe(UnsafeReason::MergeInProgress));
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let followup = recv_no_advance(&mut events).await;
        assert!(
            followup.is_none(),
            "an unrelated unsafe-pause attempt must not clobber a latched trouble, got {followup:?}"
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
    async fn failure_retry_cleared_when_the_change_resolves_to_clean() {
        // A failure retry whose change later resolves to Clean (committed or
        // reverted elsewhere) must clear the retry: the timer stops and a later
        // trigger snapshots immediately, not an interval late.
        let backend = FakeBackend::new();
        // Pass 1 hard-fails (arms the failure retry); the retry re-attempt finds
        // a clean tree; a later trigger then commits.
        backend.script([Scripted::Fail, Scripted::Clean, Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(ev, Event::SnapshotFailed { .. }), "got {ev:?}");

        // The retry timer re-attempts and the second call returns Clean.
        advance_until_snapshot_calls(&backend, 2, Duration::from_secs(30)).await;

        // The retry is now cleared: the timer stops making calls without input.
        settle().await;
        let snaps = backend.snapshot_calls();
        let safes = backend.safe_calls();
        for _ in 0..20 {
            settle().await;
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        settle().await;
        assert_eq!(
            backend.snapshot_calls(),
            snaps,
            "the retry timer must stop after the change resolves to Clean"
        );
        assert_eq!(
            backend.safe_calls(),
            safes,
            "no re-poll after the retry is cleared"
        );

        // Drain the buffered lifecycle transitions the episode emitted (Attention
        // on the failure, Ok on the skip-to-clean recovery) so the next assertion
        // reads the snapshot outcome, not a stale state event.
        while events.try_recv().is_ok() {}

        // A later trigger snapshots immediately (not delayed by an interval).
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let ev = recv_no_advance(&mut events).await;
        assert!(
            matches!(ev, Some(Event::SnapshotCompleted { .. })),
            "a trigger after a cleared retry must snapshot at once, got {ev:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn panic_during_a_retry_episode_clears_the_retry() {
        // A panic that lands mid-retry must clear the retry (not leave a zombie
        // that burns the budget and delays later triggers).
        let backend = FakeBackend::new();
        // Pass 1 fails (arms the failure retry); the retry re-attempt panics; a
        // later trigger then commits.
        backend.script([Scripted::Fail, Scripted::Panic, Scripted::Commit(1)]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(ev, Event::SnapshotFailed { .. }), "got {ev:?}");

        // The retry re-runs and panics: surfaced as a failure and Attention.
        let mut saw_attention = false;
        for _ in 0..2 {
            match advance_until_event(&mut events, Duration::from_secs(30)).await {
                Event::WatchStateChanged {
                    to: WatchState::Attention,
                    ..
                } => saw_attention = true,
                Event::SnapshotFailed { .. } => {}
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(
            saw_attention,
            "a panic during retry must move the watch to Attention"
        );

        // The retry is cleared: the timer stops making calls without input ...
        settle().await;
        let snaps = backend.snapshot_calls();
        let safes = backend.safe_calls();
        for _ in 0..20 {
            settle().await;
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        settle().await;
        assert_eq!(
            backend.snapshot_calls(),
            snaps,
            "no zombie retry snapshots after a panic"
        );
        assert_eq!(backend.safe_calls(), safes, "no re-poll after a panic");

        // ... and a later trigger snapshots immediately (retry was cleared, so
        // run() was blocked on the input, not on the retry timer). The panic
        // legitimately re-labels the standing Attention (snapshots-failing ->
        // degraded), so a WatchStateChanged may precede the snapshot event —
        // skip state transitions and assert the snapshot itself.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        let mut ev = recv_no_advance(&mut events).await;
        while matches!(ev, Some(Event::WatchStateChanged { .. })) {
            ev = recv_no_advance(&mut events).await;
        }
        assert!(
            matches!(ev, Some(Event::SnapshotCompleted { .. })),
            "a trigger after a panic-cleared retry must snapshot at once, got {ev:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flapping_unsafe_safe_still_exhausts_the_retry_budget() {
        // A repository flapping unsafe↔safe while snapshots keep failing must
        // still exhaust the bounded retry budget — one episode, not a fresh
        // budget on every safe edge.
        let backend = FakeBackend::new();
        backend.set_always_fail();
        // Effectively endless flapping so a buggy per-edge reset never lets the
        // budget converge within the test window.
        backend.script_safe((0..400).map(|i| {
            if i % 2 == 0 {
                Ok(SafeState::Unsafe(UnsafeReason::MergeInProgress))
            } else {
                Ok(SafeState::Safe)
            }
        }));
        let mut cfg = test_cfg();
        cfg.unsafe_repoll_max_attempts = 6;
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), cfg);

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        // Drive well past the bounded budget.
        for _ in 0..60 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        settle().await;
        let snaps = backend.snapshot_calls();
        // Give it much more time; a bounded worker makes no further attempts.
        for _ in 0..60 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        settle().await;
        assert_eq!(
            backend.snapshot_calls(),
            snaps,
            "a flapping repo must stop self-driving once the budget is spent"
        );
        assert!(
            backend.snapshot_calls() <= cfg.unsafe_repoll_max_attempts as usize,
            "snapshot attempts must be bounded by the retry budget, got {}",
            backend.snapshot_calls()
        );
        // Drain any failure events the flapping surfaced (bounded, not asserted).
        let _ = drain_failures(&mut events);
    }

    #[tokio::test(start_paused = true)]
    async fn unsafe_then_safe_but_failing_snapshot_emits_one_failure() {
        // An unsafe pause that becomes safe but then hard-fails to snapshot must
        // surface exactly one SnapshotFailed for the new failure.
        let backend = FakeBackend::new();
        backend.set_safe(SafeState::Unsafe(UnsafeReason::MergeInProgress));
        backend.script([Scripted::Fail]); // then Clean by default: converges.
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();

        let paused = advance_until_event(&mut events, Duration::from_secs(1)).await;
        assert!(matches!(
            paused,
            Event::WatchStateChanged {
                to: WatchState::Paused,
                ..
            }
        ));

        // Wait until the pause is genuinely *retry-driven* before flipping to
        // safe: the startup probe emits Paused up front (safe_call 1), and the
        // trigger's own pass re-checks (safe_call 2) and arms the UnsafePause
        // retry — but if we flipped to safe before that pass ran, its check would
        // see Safe directly and never arm the retry, so the resume-to-Ok this test
        // pins down could not happen. A retry *tick* re-checking (safe_call 3)
        // only occurs once the retry is armed, so it is the reliable signal.
        for _ in 0..500 {
            settle().await;
            if backend.safe_calls() >= 3 {
                break;
            }
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        assert!(
            backend.safe_calls() >= 3,
            "the unsafe pause must be retry-driven before the safe flip"
        );

        // The repo returns to safe; the pause resolves (Paused->Ok) and the
        // snapshot then fails. Scan to the failure, tolerating the intermediate
        // Ok transition (order between the resume and the new failure is not what
        // this test pins down).
        backend.set_safe(SafeState::Safe);
        let mut saw_resume = false;
        loop {
            match advance_until_event(&mut events, Duration::from_secs(30)).await {
                Event::WatchStateChanged {
                    to: WatchState::Ok, ..
                } => saw_resume = true,
                Event::SnapshotFailed { .. } => break,
                other => panic!("unexpected event before the new failure: {other:?}"),
            }
        }
        assert!(
            saw_resume,
            "the unsafe pause must resolve to Ok before failing"
        );

        // It converges (Clean) and no second failure is emitted.
        advance_until_snapshot_calls(&backend, 2, Duration::from_secs(30)).await;
        for _ in 0..10 {
            settle().await;
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        settle().await;
        assert_eq!(
            drain_failures(&mut events),
            0,
            "exactly one SnapshotFailed for the post-unsafe failure"
        );
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
    async fn trouble_moves_the_watch_to_attention_and_carries_its_kind() {
        let backend = FakeBackend::new();
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trouble {
            kind: TroubleKind::Degraded,
            detail: "inotify queue overflowed".into(),
        })
        .unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::WatchStateChanged {
                to,
                reason,
                trouble,
                ..
            } => {
                assert_eq!(to, WatchState::Attention);
                assert_eq!(reason.as_deref(), Some("inotify queue overflowed"));
                assert_eq!(
                    trouble,
                    Some(TroubleKind::Degraded),
                    "the dispatched kind must arrive unparsed on the bus event"
                );
            }
            other => panic!("expected an attention transition, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn trouble_with_source_died_is_distinguishable_from_degraded_on_the_bus() {
        // A bus subscriber must be able to tell "the signal source died" apart
        // from any other trouble cause without parsing `reason`.
        let backend = FakeBackend::new();
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        tx.send(WatchInput::Trouble {
            kind: TroubleKind::SourceDied,
            detail: "watch task ended abnormally: panic".into(),
        })
        .unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::WatchStateChanged { to, trouble, .. } => {
                assert_eq!(to, WatchState::Attention);
                assert_eq!(trouble, Some(TroubleKind::SourceDied));
            }
            other => panic!("expected an attention transition, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn source_died_is_emitted_even_when_already_in_attention() {
        // Regression: the change-only dedup must key on (state, trouble), not
        // the state alone. A watch already parked in Attention by failing
        // snapshots whose source THEN dies must still emit the SourceDied
        // transition — the daemon's dead-source rebuild triggers on it.
        let backend = FakeBackend::new();
        backend.script([Scripted::Fail]);
        let (tx, mut events, _counter) = spawn_worker(Arc::clone(&backend), test_cfg());

        // Drive a failing snapshot: the watch enters Attention/SnapshotsFailing.
        tx.send(WatchInput::Trigger(Provenance::event())).unwrap();
        loop {
            let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
            if let Event::WatchStateChanged { to, trouble, .. } = ev {
                assert_eq!(to, WatchState::Attention);
                assert_eq!(trouble, Some(TroubleKind::SnapshotsFailing));
                break;
            }
        }

        // The source dies while the watch is already in Attention.
        tx.send(WatchInput::Trouble {
            kind: TroubleKind::SourceDied,
            detail: "watch task ended abnormally".into(),
        })
        .unwrap();

        let ev = advance_until_event(&mut events, Duration::from_secs(1)).await;
        match ev {
            Event::WatchStateChanged {
                from, to, trouble, ..
            } => {
                assert_eq!(from, WatchState::Attention);
                assert_eq!(to, WatchState::Attention);
                assert_eq!(
                    trouble,
                    Some(TroubleKind::SourceDied),
                    "an Attention-to-Attention trouble change must emit"
                );
            }
            other => panic!("expected the SourceDied transition, got {other:?}"),
        }
    }

    // --- EngineHandle::trigger and shutdown ----------------------------------

    /// Builds and starts an interval-only engine over one fake-backed watch.
    /// Interval-only avoids arming a real filesystem watcher, and the long
    /// interval means the scheduler never ticks during the test (first tick is
    /// one full interval after arming), so only an injected manual trigger drives
    /// a snapshot. Real time (not paused) so `start`/`shutdown` behave normally.
    async fn start_interval_engine(backend: Arc<FakeBackend>) -> EngineHandle {
        let spec = WatchSpec::builder("w", "/tmp")
            .trigger(TriggerMode::Interval)
            .interval(Duration::from_secs(3600))
            .build()
            .unwrap();
        Engine::builder()
            .watch_with_backend(spec, backend as SharedBackend)
            .shutdown_drain_timeout(Duration::from_secs(60))
            .build()
            .unwrap()
            .start()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn trigger_injects_a_manual_snapshot() {
        let backend = FakeBackend::new();
        backend.script([Scripted::Commit(1)]);
        let engine = Engine::builder()
            .watch_with_backend(
                WatchSpec::builder("w", "/tmp")
                    .trigger(TriggerMode::Interval)
                    .interval(Duration::from_secs(3600))
                    .build()
                    .unwrap(),
                Arc::clone(&backend) as SharedBackend,
            )
            .build()
            .unwrap();
        let mut events = engine.subscribe();
        let handle = engine.start().await.unwrap();

        assert!(
            handle.trigger("w"),
            "trigger on an existing watch returns true"
        );

        let trigger = timeout(Duration::from_secs(5), async {
            loop {
                if let Event::SnapshotCompleted { trigger, .. } = events.recv().await.unwrap() {
                    return trigger;
                }
            }
        })
        .await
        .expect("an injected manual trigger must produce a snapshot");
        assert_eq!(
            trigger,
            Trigger::Manual,
            "the injected trigger must reach the worker as Manual"
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn trigger_returns_false_for_an_unknown_watch() {
        let backend = FakeBackend::new();
        let handle = start_interval_engine(backend).await;
        assert!(
            !handle.trigger("does-not-exist"),
            "trigger on a missing watch returns false"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_drains_promptly_despite_retained_route_senders() {
        // Regression: the handle now retains a route sender per worker for
        // `trigger`. If shutdown does not drop them before draining, each worker
        // channel keeps a live sender and the drain blocks until the (60s) drain
        // timeout aborts everything. Assert shutdown returns well inside that.
        let backend = FakeBackend::new();
        let handle = start_interval_engine(backend).await;
        timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown must drain promptly, not wait out the drain timeout");
    }

    #[tokio::test]
    async fn a_transient_clobber_converges_during_shutdown_drain() {
        // Finding 3: an acked one-shot sync whose first cycle abandons on a
        // transient WouldClobber arms a short retry. Shutdown's channel-close must
        // NOT drop the request before that retry fires — the worker drains the
        // pending sync to its real terminal outcome, so the ack reports success,
        // never a false "did not run". Real time (a real ~500ms drain sleep).
        let backend = FakeBackend::new();
        // behind > 0 on both fetches so both cycles take the reconcile path.
        backend.script_fetch([remote(1, 1), remote(1, 1)]);
        backend.script_reconcile([
            Ok(ReconcileOutcome::Rebased {
                new_head: SnapshotId::new("cafef00d"),
            }),
            Ok(ReconcileOutcome::AlreadyUpToDate),
        ]);
        backend.script_advance([AdvanceOutcome::WouldClobber]);
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let spec = WatchSpec::builder("w", "/tmp")
            .trigger(TriggerMode::Interval)
            .interval(Duration::from_secs(3600))
            .sync(true)
            .scratch_dir("/tmp/vard-test-scratch")
            .build()
            .unwrap();
        let handle = Engine::builder()
            .watch_with_backend(spec, Arc::clone(&backend) as SharedBackend)
            .shutdown_drain_timeout(Duration::from_secs(60))
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();

        // Request the sync, then immediately shut down (the one-shot CLI pattern).
        let ack = handle.request_sync_ack("w").expect("watch exists");
        handle.shutdown().await;

        let outcome = timeout(Duration::from_secs(5), ack)
            .await
            .expect("the ack resolves within the drain budget")
            .expect("the drained cycle converged, so the ack carries a terminal outcome");
        assert!(
            matches!(outcome, SyncOutcome::Moved { .. }),
            "the retry converged to a successful push, got {outcome:?}"
        );
        assert_eq!(backend.advance_calls(), 1, "the first advance was refused");
        assert_eq!(backend.fetch_calls(), 2, "the cycle was re-attempted once");
    }

    // --- pull-driven sync interval wiring ------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_sync_interval_tick_drives_an_automatic_sync() {
        // A syncing watch with a nonzero sync_interval arms a jittered sync
        // schedule; its tick reaches the worker as an AUTOMATIC sync request. No
        // manual sync is issued, so an observed SyncPushed proves the cadence
        // timer drove the cycle. Interval-only + a long snapshot interval keeps
        // the snapshot scheduler quiet, so the only driver is the sync tick.
        let backend = FakeBackend::new();
        backend.script_fetch([remote(1, 0)]); // local ahead => push-only
        backend.script_push([Ok(PushOutcome::Pushed)]);
        let spec = WatchSpec::builder("w", "/tmp")
            .trigger(TriggerMode::Interval)
            .interval(Duration::from_secs(3600))
            .sync(true)
            .sync_interval(Duration::from_secs(60))
            .scratch_dir("/tmp/vard-test-scratch")
            .build()
            .unwrap();
        let engine = Engine::builder()
            .watch_with_backend(spec, Arc::clone(&backend) as SharedBackend)
            .shutdown_drain_timeout(Duration::from_secs(60))
            .build()
            .unwrap();
        let mut events = engine.subscribe();
        let handle = engine.start().await.unwrap();

        match advance_until_matching(&mut events, Duration::from_secs(5), |e| {
            matches!(e, Event::SyncPushed { .. })
        })
        .await
        {
            Event::SyncPushed { commits, .. } => assert_eq!(commits, 1),
            other => panic!("expected SyncPushed from the cadence tick, got {other:?}"),
        }
        assert!(backend.fetch_calls() >= 1, "the cadence tick ran a cycle");

        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn zero_sync_interval_arms_no_sync_schedule() {
        // sync_interval = 0 disables the cadence timer: no sync schedule is
        // armed, so no automatic sync runs however far time advances
        // (fetch_calls stays 0). Push-driven and manual sync are unaffected.
        let backend = FakeBackend::new();
        let spec = WatchSpec::builder("w", "/tmp")
            .trigger(TriggerMode::Interval)
            .interval(Duration::from_secs(3600))
            .sync(true)
            .sync_interval(Duration::ZERO)
            .scratch_dir("/tmp/vard-test-scratch")
            .build()
            .unwrap();
        let handle = Engine::builder()
            .watch_with_backend(spec, Arc::clone(&backend) as SharedBackend)
            .shutdown_drain_timeout(Duration::from_secs(60))
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();

        // Advance well past many nominal 60s intervals; nothing should fetch.
        for _ in 0..30 {
            settle().await;
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        settle().await;
        assert_eq!(
            backend.fetch_calls(),
            0,
            "a zero sync_interval arms no pull timer"
        );

        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_non_syncing_watch_arms_no_sync_schedule() {
        // sync = false, even with a nonzero sync_interval and a scratch dir: the
        // watch does not sync, so no cadence timer is armed.
        let backend = FakeBackend::new();
        let spec = WatchSpec::builder("w", "/tmp")
            .trigger(TriggerMode::Interval)
            .interval(Duration::from_secs(3600))
            .sync(false)
            .sync_interval(Duration::from_secs(60))
            .scratch_dir("/tmp/vard-test-scratch")
            .build()
            .unwrap();
        let handle = Engine::builder()
            .watch_with_backend(spec, Arc::clone(&backend) as SharedBackend)
            .shutdown_drain_timeout(Duration::from_secs(60))
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();

        for _ in 0..30 {
            settle().await;
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        settle().await;
        assert_eq!(
            backend.fetch_calls(),
            0,
            "a non-syncing watch arms no pull timer"
        );

        handle.shutdown().await;
    }

    // --- watch_states projection ---------------------------------------------

    /// Waits until `pred` holds for the named watch's projected status, or
    /// panics after a generous budget. Polls `watch_states` rather than the bus,
    /// which is exactly what a host does.
    async fn wait_status(handle: &EngineHandle, watch: &str, pred: impl Fn(&WatchStatus) -> bool) {
        for _ in 0..500 {
            if let Some(s) = handle.watch_states().into_iter().find(|s| s.name == watch)
                && pred(&s)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("watch status never satisfied the predicate");
    }

    #[tokio::test]
    async fn watch_states_projects_a_failing_snapshot_then_its_recovery() {
        // The queryable projection reflects the engine's own truth: a healthy
        // start, a snapshots-failing state on a hard failure, and Ok again once a
        // snapshot lands.
        let backend = FakeBackend::new();
        backend.script([Scripted::Fail, Scripted::Commit(1)]);
        // A short repoll so the failure retry converges within the test budget.
        let spec = WatchSpec::builder("w", "/tmp")
            .trigger(TriggerMode::Interval)
            .interval(Duration::from_secs(3600))
            .build()
            .unwrap();
        let handle = Engine::builder()
            .watch_with_backend(spec, Arc::clone(&backend) as SharedBackend)
            .unsafe_repoll_interval(Duration::from_millis(20))
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();

        // Healthy at start.
        let start = handle.watch_states();
        assert_eq!(start.len(), 1);
        assert_eq!(start[0].name, "w");
        assert_eq!(start[0].state, WatchState::Ok);
        assert_eq!(start[0].trouble, None);

        handle.trigger("w");
        wait_status(&handle, "w", |s| {
            s.state == WatchState::Attention
                && s.trouble == Some(TroubleKind::SnapshotsFailing)
                && s.reason.is_some()
        })
        .await;

        // The retry converges and the projection returns to Ok.
        wait_status(&handle, "w", |s| {
            s.state == WatchState::Ok && s.trouble.is_none()
        })
        .await;

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn watch_states_projects_a_panic_then_its_recovery() {
        // VRD-39, through the same host-facing seam as the failing-snapshot
        // case above: a panicked backend call projects as Attention/Degraded,
        // and the next successful pass projects back to Ok — the health
        // projection is regenerated from engine truth, not a sticky flag.
        let backend = FakeBackend::new();
        backend.script([Scripted::Panic, Scripted::Commit(1)]);
        let handle = start_interval_engine(Arc::clone(&backend)).await;

        let start = handle.watch_states();
        assert_eq!(start[0].state, WatchState::Ok);

        handle.trigger("w");
        wait_status(&handle, "w", |s| {
            s.state == WatchState::Attention && s.trouble == Some(TroubleKind::Degraded)
        })
        .await;

        handle.trigger("w");
        wait_status(&handle, "w", |s| {
            s.state == WatchState::Ok && s.trouble.is_none()
        })
        .await;

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn initial_probe_projects_a_blocked_repo_before_any_trigger() {
        // A restart against a genuinely blocked (unsafe) repo must not amnesia to
        // Ok: the startup probe enters the blocked state up front, so the
        // projection shows it before any trigger arrives.
        let backend = FakeBackend::new();
        backend.set_safe(SafeState::Unsafe(UnsafeReason::MergeInProgress));
        let handle = start_interval_engine(Arc::clone(&backend)).await;

        wait_status(&handle, "w", |s| {
            s.state == WatchState::Paused
                && s.trouble.is_none()
                && s.reason.as_deref() == Some("a merge is in progress")
        })
        .await;

        handle.shutdown().await;
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
