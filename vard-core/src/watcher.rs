//! The filesystem watcher: turn raw file events into a quiescence signal.
//!
//! A watch fires exactly one [`WatcherSignal::Activity`] signal each time its
//! directory goes quiet *after* activity — never mid-write. Every accepted
//! filesystem event restarts a per-watch quiescence timer (default
//! [`WatchSpec::quiesce`]); the signal is emitted only once the directory has
//! been idle for the full window. That window absorbs an editor's atomic-save
//! dance (write temp, rename over the target) and an agent's multi-file write
//! burst into a single signal, which is what the snapshot engine consumes so
//! it never captures a half-written tree.
//!
//! # Two layers, one seam
//!
//! The core of this module is a quiescence state machine: a per-watch task
//! that consumes already-accepted activity markers and emits
//! [`WatcherSignal::Activity`] after quiet. It has no dependency on any
//! filesystem source, so it is driven deterministically in tests with injected
//! markers and paused time. The [notify](https://docs.rs/notify) bridge in
//! [`Watcher::arm`] feeds it: notify's handler runs on notify's own thread,
//! applies every filter there, and forwards only accepted activity across a
//! bounded channel.
//!
//! # Filtering
//!
//! Four filters run in the notify handler, *before* an event reaches the
//! channel — so ignored churn can neither delay a signal nor crowd legitimate
//! events out of the channel:
//!
//! 1. **Event kind.** Only mutations count as activity: create, modify (data,
//!    metadata, renames), and remove. Access events — opening, reading,
//!    closing files — are dropped: on Linux, inotify reports reads, and a tree
//!    under read pressure (indexing, grep, backups) must still go quiet.
//!    Unknown or unclassified kinds are treated as activity: for a backup
//!    tool, "unknown" must mean "maybe changed", never "ignore".
//! 2. Anything under `<watch>/.git/` is dropped unconditionally — vard's own
//!    commits live there and must not feed activity back into the watcher.
//! 3. The watch's [`exclude`](WatchSpec::exclude) patterns are matched in the
//!    gitignore dialect (via the `ignore` crate), rooted at the watch path —
//!    the same dialect git itself uses, since the same patterns are written to
//!    `.git/info/exclude` at registration. Invalid patterns fail
//!    [`arm`](Watcher::arm), not silently.
//! 4. While any [`MuteGuard`] is alive the watch is muted: events are dropped
//!    at intake, and a quiescence window that elapses while muted is discarded
//!    rather than emitted (see [`MuteGuard`] for the reasoning). Muting exists
//!    for self-suppression: vard mutes a watch around its own commits,
//!    restores, and syncs so its writes do not trigger a fresh snapshot.
//!    **Hook-driven churn is deliberately not muted** — state a hook applies
//!    to the tree is real state that belongs in history.
//!
//! Events flagged by notify as needing a rescan bypass filters 2 and 3:
//! a rescan means the backend lost track of the tree, so something may have
//! changed anywhere.
//!
//! # Trouble reporting
//!
//! Errors do not vanish. A notify error (inotify queue overflow, a watched
//! root invalidated, an I/O failure inside the backend) is forwarded as
//! [`WatcherSignal::Trouble`] and conservatively counted as activity — an
//! error may hide changes. A quiescence task that dies abnormally is likewise
//! reported as `Trouble` by a supervisor, so a watch can never silently turn
//! into a zombie that looks armed but reports nothing. Consumers route
//! `Trouble` to the watch's attention state.
//!
//! # Native events with polling fallback
//!
//! [`arm`](Watcher::arm) uses the platform-native backend
//! (`notify::recommended_watcher` — FSEvents, inotify, ReadDirectoryChanges).
//! If the native backend fails to arm for a path it falls back automatically to
//! a [`notify::PollWatcher`] at [`DEFAULT_POLL_INTERVAL`]; the resulting
//! [`ArmMode`] records which backend the watch actually got. A watch may also
//! force polling via [`WatchSpec::poll_interval`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Config, EventKind, PollWatcher, RecursiveMode, Watcher as _, recommended_watcher};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Instant, timeout};

use crate::config::WatchSpec;

/// Poll period used when the native watcher fails to arm and the watch falls
/// back to polling. A watch that opts into polling explicitly overrides this
/// with [`WatchSpec::poll_interval`].
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Bound on the per-watch channel bridging notify's threads to the quiescence
/// task. Only accepted activity crosses it (filtering happens in the notify
/// handler), so it fills only under a sustained burst of legitimate events;
/// overflow is recorded and surfaced, never silently swallowed (see
/// [`Watcher::arm`]).
const RAW_EVENT_CAPACITY: usize = 1024;

/// What the watcher reports on its one stream: settled activity or trouble.
///
/// `Activity` is emitted once per elapsed quiescence window that absorbed at
/// least one accepted event. `events_coalesced` counts the raw events the
/// window absorbed (advisory, for logging); a value of 5 and a value of 500
/// both mean "the directory changed, then settled".
///
/// `Trouble` means the watch needs attention: the filesystem backend reported
/// an error, the bridge channel overflowed, or the watch's own task died. A
/// watch that emits `Trouble` may still be delivering events, but the consumer
/// cannot assume completeness and should surface the condition.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatcherSignal {
    /// A watch went quiet after activity.
    Activity {
        /// Stable name of the watch that went quiet.
        watch: String,
        /// How many raw events this window absorbed before settling.
        events_coalesced: usize,
    },
    /// A watch hit a condition that needs attention.
    Trouble {
        /// Stable name of the watch.
        watch: String,
        /// Human-readable description of the condition.
        detail: String,
    },
}

/// Which backend a watch is actually running on, reported by
/// [`WatchHandle::arm_mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArmMode {
    /// The platform-native backend (FSEvents, inotify, ReadDirectoryChanges).
    Native,
    /// The polling fallback, sampling the tree every `period`.
    Polling {
        /// How often the tree is sampled.
        period: Duration,
    },
}

/// The receiving end of a [`Watcher`]'s signal stream, returned by
/// [`Watcher::new`].
///
/// Every armed watch feeds its [`WatcherSignal`]s into this one receiver.
/// Call `recv().await` to take the next signal. The channel is unbounded:
/// signals are low-rate (at most one `Activity` per quiescence window per
/// watch, plus rare `Trouble`), so senders never block, and the consumer sees
/// every signal in emission order.
pub type WatcherRx = mpsc::UnboundedReceiver<WatcherSignal>;

/// Everything that can go wrong arming a watch.
#[derive(Debug)]
#[non_exhaustive]
pub enum WatcherError {
    /// An exclude pattern was not valid gitignore syntax. Carries the watch it
    /// belongs to and the offending pattern text so the failure is
    /// attributable to a specific line of a specific watch's config.
    InvalidExclude {
        /// Stable name of the watch whose exclude list held the pattern.
        watch: String,
        /// The offending pattern text, verbatim.
        pattern: String,
        /// Why the pattern was rejected.
        reason: String,
    },
    /// The watch could not be armed: the path could not be resolved (it must
    /// exist to be watched), or neither the native backend nor the polling
    /// fallback could start. Carries the watch name and the underlying error.
    Arm {
        /// Stable name of the watch that failed to arm.
        watch: String,
        /// The underlying error from the failing step.
        source: notify::Error,
    },
}

impl std::fmt::Display for WatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatcherError::InvalidExclude {
                watch,
                pattern,
                reason,
            } => write!(
                f,
                "watch {watch:?}: invalid exclude pattern {pattern:?}: {reason}"
            ),
            WatcherError::Arm { watch, source } => {
                write!(
                    f,
                    "watch {watch:?}: could not arm filesystem watcher: {source}"
                )
            }
        }
    }
}

impl std::error::Error for WatcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WatcherError::Arm { source, .. } => Some(source),
            WatcherError::InvalidExclude { .. } => None,
        }
    }
}

/// Whether an event kind counts as a mutation of the tree.
///
/// Only mutations may restart a quiescence timer. Access kinds (open, read,
/// close) are explicitly not mutations — inotify reports them, and reading a
/// tree must never keep it from going quiet. Every non-Access kind, including
/// `Any`, `Other`, and kinds added by future notify versions, is treated as a
/// mutation: for a backup tool, an unclassified event must mean "maybe
/// changed", never "ignore".
fn kind_is_mutation(kind: &EventKind) -> bool {
    // Only Access is known-safe to ignore; everything else — including kinds
    // this build does not know about — is treated as a mutation.
    !matches!(kind, EventKind::Access(_))
}

/// Decides which event paths may restart a watch's quiescence timer.
///
/// Combines the unconditional `<watch>/.git/` drop with the watch's gitignore
/// exclude patterns. Built once per watch at arm time (which is where an
/// invalid pattern is reported). [`accepts`](EventFilter::accepts) runs on
/// notify's own thread and, when the watch has exclude patterns, stats the
/// filesystem to resolve directory-only patterns (`cache/`) — it is not a pure
/// function of the path. With no exclude patterns the stat is skipped
/// entirely.
struct EventFilter {
    root: PathBuf,
    exclude: Gitignore,
    has_excludes: bool,
}

impl EventFilter {
    /// Builds the filter for `spec`, compiling its exclude patterns rooted at
    /// `root` (the watch path, canonicalized by the caller). Returns
    /// [`WatcherError::InvalidExclude`] naming the watch and the first pattern
    /// that is not valid gitignore syntax.
    fn build(spec: &WatchSpec, root: PathBuf) -> Result<Self, WatcherError> {
        let mut builder = GitignoreBuilder::new(&root);
        for pattern in spec.exclude() {
            builder
                .add_line(None, pattern)
                .map_err(|e| WatcherError::InvalidExclude {
                    watch: spec.name().to_string(),
                    pattern: pattern.clone(),
                    reason: e.to_string(),
                })?;
        }
        let exclude = builder.build().map_err(|e| WatcherError::InvalidExclude {
            watch: spec.name().to_string(),
            pattern: spec.exclude().join(", "),
            reason: e.to_string(),
        })?;
        Ok(Self {
            root,
            exclude,
            has_excludes: !spec.exclude().is_empty(),
        })
    }

    /// Whether an event touching `path` may restart the quiescence timer.
    ///
    /// `false` for anything under `<root>/.git/` and for anything matched by
    /// the exclude patterns; `true` otherwise.
    fn accepts(&self, path: &Path) -> bool {
        if self.under_git(path) {
            return false;
        }
        if !self.has_excludes {
            return true;
        }
        // A directory-only pattern (`cache/`) only matches when the matcher
        // knows the path is a directory; probe the filesystem, defaulting to
        // "not a directory" when the path no longer exists (e.g. a deletion).
        let is_dir = path.is_dir();
        !self
            .exclude
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
    }

    /// Whether `path` lies under the watch's own `.git` directory.
    fn under_git(&self, path: &Path) -> bool {
        match path.strip_prefix(&self.root) {
            Ok(rel) => rel
                .components()
                .next()
                .is_some_and(|c| c.as_os_str() == ".git"),
            Err(_) => false,
        }
    }
}

/// Everything the notify handler needs, shared with `arm` and (for the mute
/// counter and overflow flag) the quiescence task.
struct HandlerShared {
    watch: String,
    filter: EventFilter,
    mute: Arc<AtomicUsize>,
    overflow: Arc<AtomicBool>,
    raw_tx: mpsc::Sender<()>,
    signal_tx: mpsc::UnboundedSender<WatcherSignal>,
}

/// Wraps [`handle_event`] into the closure notify calls on its own thread.
fn make_handler(
    shared: Arc<HandlerShared>,
) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
    move |result| handle_event(&shared, result)
}

/// The notify handler body: classify one delivery and forward it.
///
/// Runs on notify's thread(s), so it must never block. All filtering happens
/// here, before the channel: a burst of ignored churn (excluded build output,
/// `.git/` writes, reads) never occupies channel capacity, so it can never
/// crowd out a legitimate event. Accepted activity is forwarded as a unit
/// marker with `try_send`; a full channel sets the shared overflow flag
/// instead of blocking, and the quiescence task surfaces it (see
/// [`WatcherSignal::Trouble`]).
///
/// Errors from notify are forwarded as [`WatcherSignal::Trouble`] and counted
/// as activity: an inotify queue overflow or invalidated watch may hide real
/// changes, so the conservative reading is "something may have changed".
fn handle_event(shared: &HandlerShared, result: notify::Result<notify::Event>) {
    match result {
        Ok(event) => {
            let rescan = event.need_rescan();
            if !rescan && !kind_is_mutation(&event.kind) {
                return;
            }
            if shared.mute.load(Ordering::SeqCst) > 0 {
                return;
            }
            // A rescan means the backend lost track of the tree: paths (if
            // any) are unreliable, so skip path filtering and count it as
            // activity. An event with no paths at all is likewise counted —
            // unattributable must mean "maybe changed" for a backup tool.
            if !rescan
                && !event.paths.is_empty()
                && !event.paths.iter().any(|p| shared.filter.accepts(p))
            {
                return;
            }
            forward_marker(shared);
        }
        Err(error) => {
            // An error from the backend (inotify queue overflow, invalidated
            // watch, I/O failure) must never vanish: report it, and count it
            // as activity — it may hide real changes.
            let _ = shared.signal_tx.send(WatcherSignal::Trouble {
                watch: shared.watch.clone(),
                detail: error.to_string(),
            });
            forward_marker(shared);
        }
    }
}

/// Forwards one accepted-activity marker, recording (not blocking on) a full
/// channel.
fn forward_marker(shared: &HandlerShared) {
    if let Err(mpsc::error::TrySendError::Full(())) = shared.raw_tx.try_send(()) {
        shared.overflow.store(true, Ordering::SeqCst);
    }
}

/// The quiescence state machine: one instance runs per armed watch.
///
/// Consumes accepted-activity markers from `rx` (the notify handler has
/// already filtered) and emits one [`WatcherSignal::Activity`] each time the
/// watch settles. Every marker resets the deadline to `now + quiesce`; when
/// the deadline elapses, the window settles: if the watch is muted the pending
/// window is discarded, otherwise `Activity` is emitted. The loop parks on
/// either the next marker or the deadline — never busy-polls — and ends when
/// `rx` closes (the notify backend was dropped, i.e. the watch disarmed).
///
/// The overflow flag is checked whenever the task wakes; a set flag becomes a
/// [`WatcherSignal::Trouble`] so dropped markers are surfaced, and the marker
/// that accompanied the overflow has already reset the window.
async fn run_quiescence(
    name: String,
    quiesce: Duration,
    mut rx: mpsc::Receiver<()>,
    mute: Arc<AtomicUsize>,
    overflow: Arc<AtomicBool>,
    signal_tx: mpsc::UnboundedSender<WatcherSignal>,
) {
    let mut coalesced: usize = 0;
    // `None` while idle (no pending activity); `Some(instant)` once at least
    // one marker has been absorbed and is awaiting the quiet window.
    let mut deadline: Option<Instant> = None;

    loop {
        let received = match deadline {
            // Idle: block indefinitely for the next marker. No timer is armed,
            // so a quiet watch consumes nothing.
            None => rx.recv().await,
            Some(dl) => {
                let now = Instant::now();
                if now >= dl {
                    settle_window(&name, &mut coalesced, &mut deadline, &mute, &signal_tx);
                    continue;
                }
                match timeout(dl - now, rx.recv()).await {
                    Ok(received) => received,
                    // The window elapsed with no new marker: the watch settled.
                    Err(_) => {
                        settle_window(&name, &mut coalesced, &mut deadline, &mute, &signal_tx);
                        continue;
                    }
                }
            }
        };

        match received {
            Some(()) => {
                // A recorded overflow is surfaced with the marker that
                // accompanied it (a full channel guarantees markers are in
                // flight, so observation here is guaranteed); the marker
                // itself resets the window, so no reset is lost.
                surface_overflow(&name, &overflow, &signal_tx);
                coalesced += 1;
                deadline = Some(Instant::now() + quiesce);
            }
            // All senders dropped: the watch was disarmed. Any absorbed but
            // unsettled activity is intentionally not flushed — disarm means
            // stop, not "snapshot one last time".
            None => break,
        }
    }
}

/// Settles an elapsed window: emits one [`WatcherSignal::Activity`] and resets
/// the window state — unless the watch is muted, in which case the pending
/// window is discarded without emitting (see [`MuteGuard`] for why discarding
/// loses nothing). A send failure means the consumer dropped its
/// [`WatcherRx`]; there is nothing to do but drop the signal.
fn settle_window(
    name: &str,
    coalesced: &mut usize,
    deadline: &mut Option<Instant>,
    mute: &AtomicUsize,
    signal_tx: &mpsc::UnboundedSender<WatcherSignal>,
) {
    // A window elapsing under mute is discarded, not emitted: the engine
    // operation that took the guard captures the pre-mute state itself, and
    // emitting here would trigger a snapshot in the middle of that operation —
    // the exact thing muting exists to prevent.
    if mute.load(Ordering::SeqCst) == 0 {
        let _ = signal_tx.send(WatcherSignal::Activity {
            watch: name.to_string(),
            events_coalesced: *coalesced,
        });
    }
    *coalesced = 0;
    *deadline = None;
}

/// Surfaces a recorded channel overflow as [`WatcherSignal::Trouble`], once
/// per overflow episode.
fn surface_overflow(
    name: &str,
    overflow: &AtomicBool,
    signal_tx: &mpsc::UnboundedSender<WatcherSignal>,
) {
    if overflow.swap(false, Ordering::SeqCst) {
        let _ = signal_tx.send(WatcherSignal::Trouble {
            watch: name.to_string(),
            detail: "event channel overflowed; some filesystem events were dropped".to_string(),
        });
    }
}

/// Watches the quiescence task and reports an abnormal end as
/// [`WatcherSignal::Trouble`], so a watch can never die silently. A
/// deliberate abort (disarm) is not abnormal and reports nothing.
fn supervise(
    watch: String,
    task: JoinHandle<()>,
    signal_tx: mpsc::UnboundedSender<WatcherSignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = task.await
            && !err.is_cancelled()
        {
            let _ = signal_tx.send(WatcherSignal::Trouble {
                watch,
                detail: format!("watch task ended abnormally: {err}"),
            });
        }
    })
}

/// A filesystem watcher that reports directory quiescence and trouble.
///
/// Construct with [`new`](Watcher::new), then [`arm`](Watcher::arm) each watch;
/// arming and disarming are dynamic, so watches can be added and removed while
/// the watcher runs. Every armed watch feeds the single [`WatcherRx`] returned
/// alongside the watcher.
///
/// The watcher is cheap to clone-by-handle: [`arm`](Watcher::arm) takes
/// `&self`, so one `Watcher` value serves the whole process.
pub struct Watcher {
    signal_tx: mpsc::UnboundedSender<WatcherSignal>,
}

impl Watcher {
    /// Creates a watcher and the receiver for every watch's signals.
    pub fn new() -> (Watcher, WatcherRx) {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        (Watcher { signal_tx }, signal_rx)
    }

    /// Arms a watch over `spec.path()`, recursively, and returns its handle.
    ///
    /// The path must exist: it is canonicalized first (so the `.git/` and
    /// exclude filters see the same canonical paths notify reports), and a
    /// path that cannot be resolved fails with [`WatcherError::Arm`] rather
    /// than arming a watcher that can never deliver. Exclude patterns are
    /// validated next, failing with [`WatcherError::InvalidExclude`] (naming
    /// the watch and the offending pattern) before any watcher is created.
    /// Then the native backend is armed, falling back to polling at
    /// [`DEFAULT_POLL_INTERVAL`] if the native backend fails — unless
    /// [`spec.poll_interval()`](WatchSpec::poll_interval) forces polling
    /// outright. The resulting [`ArmMode`] is on the handle.
    ///
    /// # No same-path deduplication
    ///
    /// Arming two watches over the same directory is not detected here: both
    /// arm, and both signal — every signal is doubled. The binary is guarded
    /// by its config layer, which rejects duplicate watch paths; SDK callers
    /// own this invariant themselves.
    ///
    /// # Runtime
    ///
    /// Must be called from within a Tokio runtime: it spawns the watch's
    /// quiescence task (and its supervisor). notify delivers events on its
    /// own thread(s); its handler filters there and forwards accepted
    /// activity through a bounded channel.
    pub fn arm(&self, spec: &WatchSpec) -> Result<WatchHandle, WatcherError> {
        let watch = spec.name().to_string();

        // The `.git/` and exclude filters compare against notify's event
        // paths, which are canonical; a non-canonical root would silently
        // disable both (a `strip_prefix` mismatch), letting vard's own
        // commits feed back as activity. So canonicalization failing —
        // which includes the path not existing — must fail the arm, never
        // fall back to a watcher whose filters cannot work.
        let root = spec.path().canonicalize().map_err(|e| WatcherError::Arm {
            watch: watch.clone(),
            source: notify::Error::io(e).add_path(spec.path().to_path_buf()),
        })?;

        let filter = EventFilter::build(spec, root.clone())?;

        let (raw_tx, raw_rx) = mpsc::channel::<()>(RAW_EVENT_CAPACITY);
        let mute = Arc::new(AtomicUsize::new(0));
        let overflow = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(HandlerShared {
            watch: watch.clone(),
            filter,
            mute: Arc::clone(&mute),
            overflow: Arc::clone(&overflow),
            raw_tx,
            signal_tx: self.signal_tx.clone(),
        });

        let arm_err = |source: notify::Error| WatcherError::Arm {
            watch: watch.clone(),
            source,
        };

        // Each backend attempt gets its own handler closure over the same
        // shared state.
        let (backend, arm_mode) = if let Some(period) = spec.poll_interval() {
            // An explicit poll interval forces polling and never tries native.
            let backend =
                arm_poll(&root, make_handler(Arc::clone(&shared)), period).map_err(arm_err)?;
            (backend, ArmMode::Polling { period })
        } else {
            match arm_native(&root, make_handler(Arc::clone(&shared))) {
                Ok(backend) => (backend, ArmMode::Native),
                // Native failed for this path (unsupported filesystem,
                // resource limit, ...): fall back to polling rather than
                // failing the watch.
                Err(_native_err) => {
                    let period = DEFAULT_POLL_INTERVAL;
                    let backend = arm_poll(&root, make_handler(Arc::clone(&shared)), period)
                        .map_err(arm_err)?;
                    (backend, ArmMode::Polling { period })
                }
            }
        };

        let task = tokio::spawn(run_quiescence(
            watch.clone(),
            spec.quiesce(),
            raw_rx,
            Arc::clone(&mute),
            Arc::clone(&overflow),
            self.signal_tx.clone(),
        ));
        let abort = task.abort_handle();
        supervise(watch, task, self.signal_tx.clone());

        Ok(WatchHandle {
            backend: Some(backend),
            task: abort,
            mute,
            arm_mode,
        })
    }
}

/// Creates and starts a native recommended watcher over `root`, recursively.
fn arm_native(
    root: &Path,
    handler: impl notify::EventHandler,
) -> notify::Result<Box<dyn notify::Watcher + Send>> {
    let mut watcher = recommended_watcher(handler)?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(Box::new(watcher))
}

/// Creates and starts a polling watcher over `root`, recursively, at `period`.
fn arm_poll(
    root: &Path,
    handler: impl notify::EventHandler,
    period: Duration,
) -> notify::Result<Box<dyn notify::Watcher + Send>> {
    let config = Config::default().with_poll_interval(period);
    let mut watcher = PollWatcher::new(handler, config)?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(Box::new(watcher))
}

/// A live watch. Dropping it disarms the watch (see [`disarm`](Self::disarm)).
///
/// Holds the notify backend alive — dropping the backend stops filesystem
/// events, which closes the bridge channel.
pub struct WatchHandle {
    // `Option` so `Drop` can move the backend out; always `Some` until drop.
    backend: Option<Box<dyn notify::Watcher + Send>>,
    task: AbortHandle,
    mute: Arc<AtomicUsize>,
    arm_mode: ArmMode,
}

impl WatchHandle {
    /// Which backend this watch is running on.
    pub fn arm_mode(&self) -> ArmMode {
        self.arm_mode
    }

    /// Mutes the watch until the returned guard is dropped.
    ///
    /// While at least one guard is alive, filesystem events are dropped at
    /// intake **and** any quiescence window that elapses is discarded rather
    /// than emitted — including a window armed by activity from *before* the
    /// mute. Discarding loses nothing: the engine operation that took the
    /// guard captures that pre-mute state anyway (a snapshot sweeps the whole
    /// tree; a restore is preceded by a protective snapshot). Guards nest: the
    /// watch stays muted until the last one is dropped.
    ///
    /// Use this to suppress self-inflicted activity — vard mutes a watch
    /// around its own commits, restores, and syncs so those writes do not
    /// trigger a fresh snapshot. **Hold the guard until the underlying VCS
    /// call has returned**, not merely until it was issued: filesystem
    /// delivery lags the writes, and a guard dropped early lets the tail of
    /// the operation's own events through. Events that still arrive after the
    /// guard drops (delivery latency) are absorbed by the quiescence window
    /// like any other burst; the residual race is one spurious snapshot, never
    /// a corrupt one.
    pub fn mute(&self) -> MuteGuard {
        MuteGuard::new(Arc::clone(&self.mute))
    }

    /// Disarms the watch: stops filesystem events and ends its quiescence task.
    ///
    /// This is exactly what dropping the handle does; call it to disarm
    /// explicitly and read as intent. In-flight, unsettled activity is not
    /// flushed — no final signal is emitted.
    pub fn disarm(self) {
        // Consumes `self`; `Drop` does the work.
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        // Stop the task first so nothing processes further deliveries.
        self.task.abort();
        // notify backends join their event thread in drop (the FSEvents
        // backend even spin-waits for it) — blocking. Dropping that on a
        // tokio worker would stall the executor, so inside a runtime the
        // backend is handed to a blocking thread to die; outside one, a
        // plain drop is safe.
        if let Some(backend) = self.backend.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn_blocking(move || drop(backend));
                }
                Err(_) => drop(backend),
            }
        }
    }
}

/// A RAII mute: while it is alive, its watch drops every filesystem event and
/// discards (rather than emits) any quiescence window that elapses.
///
/// Obtained from [`WatchHandle::mute`]; see there for the discard rationale,
/// nesting, and how long to hold the guard. Dropping it decrements the
/// watch's mute count; the watch resumes normal operation once every guard is
/// dropped.
pub struct MuteGuard {
    mute: Arc<AtomicUsize>,
}

impl MuteGuard {
    fn new(mute: Arc<AtomicUsize>) -> Self {
        mute.fetch_add(1, Ordering::SeqCst);
        Self { mute }
    }
}

impl Drop for MuteGuard {
    fn drop(&mut self) {
        self.mute.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };

    use super::*;

    /// Builds the handler-side state over `root`, with a raw channel of
    /// `capacity`, plus the receivers to observe what crosses each channel.
    fn handler_shared(
        name: &str,
        root: &Path,
        exclude: &[&str],
        capacity: usize,
    ) -> (Arc<HandlerShared>, mpsc::Receiver<()>, WatcherRx) {
        let spec = WatchSpec::builder(name, root)
            .exclude(exclude.iter().map(|s| s.to_string()))
            .build()
            .unwrap();
        let filter = EventFilter::build(&spec, root.to_path_buf()).unwrap();
        let (raw_tx, raw_rx) = mpsc::channel(capacity);
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(HandlerShared {
            watch: name.to_string(),
            filter,
            mute: Arc::new(AtomicUsize::new(0)),
            overflow: Arc::new(AtomicBool::new(false)),
            raw_tx,
            signal_tx,
        });
        (shared, raw_rx, signal_rx)
    }

    /// A mutation event touching `path`.
    fn modify(path: PathBuf) -> notify::Event {
        notify::Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(path)
    }

    /// Spawns the quiescence machine with its own channels, returning the
    /// marker sender, signal receiver, mute counter, overflow flag, and task.
    fn spawn_machine(
        name: &str,
        quiesce: Duration,
    ) -> (
        mpsc::Sender<()>,
        WatcherRx,
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
        JoinHandle<()>,
    ) {
        let (raw_tx, raw_rx) = mpsc::channel(RAW_EVENT_CAPACITY);
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let mute = Arc::new(AtomicUsize::new(0));
        let overflow = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run_quiescence(
            name.to_string(),
            quiesce,
            raw_rx,
            Arc::clone(&mute),
            Arc::clone(&overflow),
            signal_tx,
        ));
        (raw_tx, signal_rx, mute, overflow, task)
    }

    /// Lets the spawned quiescence task make progress without advancing the
    /// paused clock (which `yield_now` does not touch).
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// Asserts the next pending signal is `Activity` for `watch` and returns
    /// its coalesced count.
    fn expect_activity(rx: &mut WatcherRx, watch: &str) -> usize {
        match rx.try_recv().expect("expected a pending signal") {
            WatcherSignal::Activity {
                watch: w,
                events_coalesced,
            } => {
                assert_eq!(w, watch);
                events_coalesced
            }
            other => panic!("expected Activity, got {other:?}"),
        }
    }

    // --- kind predicate ------------------------------------------------------

    #[test]
    fn kind_predicate_rejects_access_and_accepts_mutations_and_unknown() {
        // Reads must never reset quiescence: a tree under read pressure
        // (indexing, grep, backup reads) still has to go quiet.
        assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Any)));
        assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));

        // Mutations, including metadata and renames, are activity.
        assert!(kind_is_mutation(&EventKind::Create(CreateKind::File)));
        assert!(kind_is_mutation(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(kind_is_mutation(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any
        ))));
        assert!(kind_is_mutation(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Any
        ))));
        assert!(kind_is_mutation(&EventKind::Remove(RemoveKind::File)));

        // Unknown/unclassified kinds are fail-safe activity for a backup tool.
        assert!(kind_is_mutation(&EventKind::Any));
        assert!(kind_is_mutation(&EventKind::Other));
    }

    // --- handler (notify-thread) behavior ------------------------------------

    #[test]
    fn handler_drops_access_events_but_forwards_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (shared, mut raw_rx, _signals) = handler_shared("w", root, &[], 8);

        let access = notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
            .add_path(root.join("read.txt"));
        handle_event(&shared, Ok(access));
        assert!(
            raw_rx.try_recv().is_err(),
            "an access event must not become activity"
        );

        handle_event(&shared, Ok(modify(root.join("written.txt"))));
        assert!(raw_rx.try_recv().is_ok(), "a mutation must become activity");
    }

    #[test]
    fn handler_filters_git_excluded_and_muted_before_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Capacity 1: if filtered churn crossed the channel, it would occupy
        // the only slot and the later legitimate event would be lost.
        let (shared, mut raw_rx, _signals) = handler_shared("w", root, &["target"], 1);

        for i in 0..16 {
            handle_event(&shared, Ok(modify(root.join(format!("target/junk{i}")))));
            handle_event(&shared, Ok(modify(root.join(".git/objects/x"))));
        }
        assert!(
            raw_rx.try_recv().is_err(),
            "filtered churn must never reach the channel"
        );

        // The single slot is still free for the one legitimate event.
        handle_event(&shared, Ok(modify(root.join("src/lib.rs"))));
        assert!(raw_rx.try_recv().is_ok(), "the real event must get through");
        assert!(
            !shared.overflow.load(Ordering::SeqCst),
            "filtered churn must not trip the overflow flag"
        );

        // Muted events are dropped at intake.
        shared.mute.fetch_add(1, Ordering::SeqCst);
        handle_event(&shared, Ok(modify(root.join("src/lib.rs"))));
        assert!(
            raw_rx.try_recv().is_err(),
            "muted events must be dropped at intake"
        );
    }

    #[test]
    fn handler_forwards_errors_as_trouble_and_conservative_activity() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut raw_rx, mut signals) = handler_shared("w", dir.path(), &[], 8);

        handle_event(&shared, Err(notify::Error::generic("queue overflowed")));

        match signals.try_recv() {
            Ok(WatcherSignal::Trouble { watch, detail }) => {
                assert_eq!(watch, "w");
                assert!(detail.contains("queue overflowed"), "detail: {detail}");
            }
            other => panic!("expected Trouble, got {other:?}"),
        }
        // An error may hide changes, so it also counts as activity.
        assert!(
            raw_rx.try_recv().is_ok(),
            "an error must conservatively count as activity"
        );
    }

    #[test]
    fn handler_treats_rescan_as_activity_despite_path_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (shared, mut raw_rx, _signals) = handler_shared("w", root, &["target"], 8);

        // A rescan event touching only an excluded path still counts: the
        // backend lost track of the tree, so anything may have changed.
        let rescan = modify(root.join("target/x")).set_flag(Flag::Rescan);
        handle_event(&shared, Ok(rescan));
        assert!(
            raw_rx.try_recv().is_ok(),
            "a rescan must count as activity regardless of paths"
        );
    }

    #[test]
    fn handler_records_overflow_when_the_channel_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (shared, mut raw_rx, _signals) = handler_shared("w", root, &[], 1);

        handle_event(&shared, Ok(modify(root.join("a"))));
        assert!(!shared.overflow.load(Ordering::SeqCst));

        // Channel full: the marker is dropped but the drop is recorded.
        handle_event(&shared, Ok(modify(root.join("b"))));
        assert!(
            shared.overflow.load(Ordering::SeqCst),
            "a dropped marker must set the overflow flag"
        );
        assert!(raw_rx.try_recv().is_ok());
    }

    // --- quiescence state machine (paused time) -------------------------------

    #[tokio::test(start_paused = true)]
    async fn fires_only_after_full_window_from_last_event() {
        let quiesce = Duration::from_secs(10);
        let (tx, mut rx, _mute, _ovf, _task) = spawn_machine("w", quiesce);

        // Marker at t=0 arms a deadline at t=10.
        tx.send(()).await.unwrap();
        settle().await;

        // A second marker at t=5 pushes the deadline out to t=15.
        tokio::time::advance(Duration::from_secs(5)).await;
        tx.send(()).await.unwrap();
        settle().await;

        // At t=10 the original window would have elapsed, but the reset moved
        // it: nothing yet.
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "must not fire at t=10 after a reset"
        );

        // At t=15 the window since the last marker elapses: one signal, both
        // markers coalesced.
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        assert_eq!(expect_activity(&mut rx, "w"), 2);
        assert!(rx.try_recv().is_err(), "exactly one signal per window");
    }

    #[tokio::test(start_paused = true)]
    async fn burst_of_events_coalesces_into_one_signal() {
        let (tx, mut rx, _mute, _ovf, _task) = spawn_machine("w", Duration::from_secs(10));

        for _ in 0..50 {
            tx.send(()).await.unwrap();
        }
        settle().await;

        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;

        assert_eq!(expect_activity(&mut rx, "w"), 50);
        assert!(rx.try_recv().is_err(), "a burst is one signal, not many");
    }

    #[tokio::test(start_paused = true)]
    async fn no_events_no_signal() {
        let (_tx, mut rx, _mute, _ovf, _task) = spawn_machine("w", Duration::from_secs(10));

        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert!(rx.try_recv().is_err(), "a quiet watch never signals");
    }

    #[tokio::test(start_paused = true)]
    async fn window_armed_before_mute_is_discarded_not_emitted() {
        let (tx, mut rx, mute, _ovf, _task) = spawn_machine("w", Duration::from_secs(10));

        // Activity arms a window...
        tx.send(()).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(5)).await;

        // ...then the engine mutes (its own operation begins) before the
        // window elapses. Firing now would snapshot mid-self-operation.
        let guard = MuteGuard::new(Arc::clone(&mute));
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "a window elapsing under mute must not emit"
        );

        // The discarded window stays discarded after unmute: no late signal.
        drop(guard);
        tokio::time::advance(Duration::from_secs(30)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "the discarded window must not fire after unmute"
        );

        // New activity after unmute starts a fresh window with a fresh count.
        tx.send(()).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(
            expect_activity(&mut rx, "w"),
            1,
            "the discarded window's count must not leak into the next signal"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn nested_mute_guards_stay_muted_until_last_drops() {
        let (tx, mut rx, mute, _ovf, _task) = spawn_machine("w", Duration::from_secs(10));

        let outer = MuteGuard::new(Arc::clone(&mute));
        let inner = MuteGuard::new(Arc::clone(&mute));
        drop(inner);
        // Still muted: one guard remains, so an elapsing window is discarded.
        tx.send(()).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(20)).await;
        settle().await;
        assert!(rx.try_recv().is_err(), "still muted with one guard alive");
        drop(outer);
    }

    #[tokio::test(start_paused = true)]
    async fn overflow_is_surfaced_as_trouble_and_window_still_settles() {
        let (tx, mut rx, _mute, overflow, _task) = spawn_machine("w", Duration::from_secs(10));

        // Model the handler hitting a full channel: flag set, marker delivered
        // (a full channel guarantees markers are in flight).
        overflow.store(true, Ordering::SeqCst);
        tx.send(()).await.unwrap();
        settle().await;

        match rx.try_recv() {
            Ok(WatcherSignal::Trouble { watch, detail }) => {
                assert_eq!(watch, "w");
                assert!(detail.contains("overflow"), "detail: {detail}");
            }
            other => panic!("expected Trouble for the overflow, got {other:?}"),
        }
        assert!(
            !overflow.load(Ordering::SeqCst),
            "the flag must be consumed with the report"
        );

        // The window itself still settles normally.
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(expect_activity(&mut rx, "w"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn two_watches_are_independent() {
        let (tx_a, mut rx_a, _ma, _oa, _ta) = spawn_machine("a", Duration::from_secs(10));
        let (tx_b, mut rx_b, _mb, _ob, _tb) = spawn_machine("b", Duration::from_secs(10));

        // Only watch a sees activity.
        tx_a.send(()).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;

        assert_eq!(expect_activity(&mut rx_a, "a"), 1);
        assert!(rx_b.try_recv().is_err(), "watch b saw no events");

        // b remains armable afterwards, independent of a.
        tx_b.send(()).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(expect_activity(&mut rx_b, "b"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn disarm_stops_signals() {
        let (tx, mut rx, _mute, _ovf, task) = spawn_machine("w", Duration::from_secs(10));

        tx.send(()).await.unwrap();
        settle().await;

        // Dropping the sender models disarm: the machine ends without flushing
        // the in-flight window.
        drop(tx);
        settle().await;
        assert!(task.await.is_ok(), "machine ends cleanly on disarm");

        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert!(rx.try_recv().is_err(), "no signal after disarm");
    }

    // --- handler + machine wired together (paused time) -----------------------

    #[tokio::test(start_paused = true)]
    async fn access_events_neither_signal_nor_extend_a_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (shared, raw_rx, _unused_signals) = handler_shared("w", &root, &[], 64);
        // Wire the machine to the handler's channel, observing the handler's
        // signal sender end-to-end.
        let (signal_tx, mut rx) = mpsc::unbounded_channel();
        let _task = tokio::spawn(run_quiescence(
            "w".to_string(),
            Duration::from_secs(10),
            raw_rx,
            Arc::clone(&shared.mute),
            Arc::clone(&shared.overflow),
            signal_tx,
        ));

        // Pure read pressure: no signal, ever.
        for _ in 0..10 {
            let access = notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
                .add_path(root.join("read.txt"));
            handle_event(&shared, Ok(access));
        }
        settle().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "read pressure alone must never produce a signal"
        );

        // One real write arms a window; reads at t=5 must NOT extend it.
        handle_event(&shared, Ok(modify(root.join("real.txt"))));
        settle().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        let access = notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
            .add_path(root.join("real.txt"));
        handle_event(&shared, Ok(access));
        settle().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        assert_eq!(
            expect_activity(&mut rx, "w"),
            1,
            "the window must elapse at t=10 as if the reads never happened"
        );
    }

    // --- supervisor ------------------------------------------------------------

    #[tokio::test]
    async fn panicking_watch_task_is_reported_as_trouble() {
        let (signal_tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async { panic!("machine bug") });
        supervise("w".to_string(), task, signal_tx)
            .await
            .expect("supervisor itself must not die");

        match rx.try_recv() {
            Ok(WatcherSignal::Trouble { watch, .. }) => assert_eq!(watch, "w"),
            other => panic!("expected Trouble after a task panic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliberately_aborted_watch_task_is_not_trouble() {
        let (signal_tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        let supervisor = supervise("w".to_string(), task, signal_tx);
        abort.abort();
        supervisor.await.expect("supervisor must end cleanly");

        assert!(
            rx.try_recv().is_err(),
            "a deliberate abort (disarm) must not be reported as trouble"
        );
    }

    // --- filter unit tests ------------------------------------------------------

    #[test]
    fn filter_drops_git_dir_and_excludes_but_keeps_others() {
        let root = PathBuf::from("/watch");
        let spec = WatchSpec::builder("w", &root)
            .exclude(["target", "*.log"])
            .build()
            .unwrap();
        let filter = EventFilter::build(&spec, root.clone()).unwrap();

        assert!(!filter.accepts(&root.join(".git/HEAD")));
        assert!(!filter.accepts(&root.join(".git")));
        assert!(!filter.accepts(&root.join("target/debug/app")));
        assert!(!filter.accepts(&root.join("build.log")));
        assert!(!filter.accepts(&root.join("sub/nested.log")));

        assert!(filter.accepts(&root.join("src/main.rs")));
        assert!(filter.accepts(&root.join("notes.md")));
        // A file merely named similarly to `.git` is not the git dir.
        assert!(filter.accepts(&root.join(".gitignore")));
    }

    #[test]
    fn filter_handles_negation_anchored_and_dir_only_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Real filesystem entries so directory-only matching is exercised
        // faithfully: `cache` is a directory, `build` is a plain file.
        fs::create_dir(root.join("cache")).unwrap();
        fs::write(root.join("build"), b"file, not dir").unwrap();

        let spec = WatchSpec::builder("w", &root)
            .exclude(["*.txt", "!keep.txt", "/root.md", "cache/", "build/"])
            .build()
            .unwrap();
        let filter = EventFilter::build(&spec, root.clone()).unwrap();

        // Negation re-includes a file the wildcard would exclude.
        assert!(!filter.accepts(&root.join("notes.txt")));
        assert!(filter.accepts(&root.join("keep.txt")));

        // Anchored patterns match at the root only.
        assert!(!filter.accepts(&root.join("root.md")));
        assert!(filter.accepts(&root.join("sub/root.md")));

        // Directory-only patterns match the directory and its contents...
        assert!(!filter.accepts(&root.join("cache")));
        assert!(!filter.accepts(&root.join("cache/entry")));
        // ...but not a plain file of the same name.
        assert!(filter.accepts(&root.join("build")));
    }

    #[test]
    fn build_rejects_invalid_exclude_naming_watch_and_pattern() {
        let root = PathBuf::from("/watch");
        // A reversed character range is invalid gitignore glob syntax.
        let spec = WatchSpec::builder("mywatch", &root)
            .exclude(["ok", "[z-a]"])
            .build()
            .unwrap();
        match EventFilter::build(&spec, root).map(|_| ()) {
            Err(WatcherError::InvalidExclude { watch, pattern, .. }) => {
                assert_eq!(watch, "mywatch");
                assert_eq!(pattern, "[z-a]");
            }
            other => panic!("expected InvalidExclude, got {other:?}"),
        }
    }

    // --- arm-time validation -----------------------------------------------------

    #[tokio::test]
    async fn arm_rejects_invalid_exclude_before_watching() {
        let dir = tempfile::tempdir().unwrap();
        let spec = WatchSpec::builder("cfg", dir.path())
            .exclude(["[z-a]"])
            .build()
            .unwrap();
        let (watcher, _rx) = Watcher::new();
        match watcher.arm(&spec) {
            Err(WatcherError::InvalidExclude { watch, pattern, .. }) => {
                assert_eq!(watch, "cfg");
                assert_eq!(pattern, "[z-a]");
            }
            other => panic!("expected InvalidExclude, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn arm_fails_loudly_when_the_path_does_not_exist() {
        // Plain #[test]: the failure must occur before anything needs a
        // runtime. A nonexistent path cannot be watched; silently arming a
        // watcher that will never deliver would be a dead watch.
        let spec = WatchSpec::builder("ghost", "/nonexistent/vard-watcher-test-path")
            .build()
            .unwrap();
        let (watcher, _rx) = Watcher::new();
        match watcher.arm(&spec) {
            Err(WatcherError::Arm { watch, .. }) => assert_eq!(watch, "ghost"),
            Err(other) => panic!("expected Arm error, got {other:?}"),
            Ok(_) => panic!("arming a nonexistent path must fail loudly"),
        }
    }
}
