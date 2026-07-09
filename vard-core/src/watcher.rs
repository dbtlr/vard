//! The filesystem watcher: turn raw file events into a quiescence signal.
//!
//! A watch fires exactly one [`Activity`] signal each time its directory goes
//! quiet *after* activity — never mid-write. Every accepted filesystem event
//! restarts a per-watch quiescence timer (default [`WatchSpec::quiesce`]); the
//! signal is emitted only once the directory has been idle for the full window.
//! That window absorbs an editor's atomic-save dance (write temp, rename over
//! the target) and an agent's multi-file write burst into a single signal,
//! which is what the snapshot engine consumes so it never captures a
//! half-written tree.
//!
//! # Two layers, one seam
//!
//! The core of this module is a quiescence state machine
//! ([`run_quiescence`]): it consumes a stream of raw path events, applies the
//! filter, and emits [`Activity`] after quiet. It has no dependency on any
//! filesystem source, so it is driven deterministically in tests with injected
//! events and paused time. The [notify](https://docs.rs/notify) bridge in
//! [`Watcher::arm`] is a thin layer that feeds real filesystem events into that
//! same machine.
//!
//! # Filtering
//!
//! Three filters apply to every raw event *before* it can restart the timer, so
//! ignored churn never delays or spuriously produces a signal:
//!
//! 1. Anything under `<watch>/.git/` is dropped unconditionally — vard's own
//!    commits live there and must not feed activity back into the watcher.
//! 2. The watch's [`exclude`](WatchSpec::exclude) patterns are matched in the
//!    gitignore dialect (via the `ignore` crate), rooted at the watch path —
//!    the same dialect git itself uses, since the same patterns are written to
//!    `.git/info/exclude` at registration. Invalid patterns fail
//!    [`arm`](Watcher::arm), not silently.
//! 3. While any [`MuteGuard`] is alive the watch is muted and every event is
//!    dropped. Muting exists for self-suppression: vard mutes a watch around
//!    its own commits, restores, and syncs so its writes do not trigger a fresh
//!    snapshot. **Hook-driven churn is deliberately not muted** — state a hook
//!    applies to the tree is real state that belongs in history.
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Config, PollWatcher, RecursiveMode, Watcher as _, recommended_watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

/// Poll period used when the native watcher fails to arm and the watch falls
/// back to polling. A watch that opts into polling explicitly overrides this
/// with [`WatchSpec::poll_interval`].
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Bound on the per-watch channel bridging notify's threads to the quiescence
/// task. See [`Watcher::arm`] for why overflow drops events harmlessly.
const RAW_EVENT_CAPACITY: usize = 1024;

use crate::config::WatchSpec;

/// One quiescence signal: a watch went quiet after activity.
///
/// Emitted once per elapsed quiescence window that absorbed at least one
/// accepted event. `events_coalesced` counts the raw events the window
/// absorbed (for future logging); it carries no other meaning — a value of 5
/// and a value of 500 both mean "the directory changed, then settled".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Stable name of the watch that went quiet.
    pub watch: String,
    /// How many raw events this window absorbed before the directory settled.
    pub events_coalesced: usize,
}

/// Which backend a watch is actually running on, reported by
/// [`WatchHandle::arm_mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmMode {
    /// The platform-native backend (FSEvents, inotify, ReadDirectoryChanges).
    Native,
    /// The polling fallback, sampling the tree every `period`.
    Polling {
        /// How often the tree is sampled.
        period: Duration,
    },
}

/// The receiving end of a [`Watcher`]'s activity stream, returned by
/// [`Watcher::new`].
///
/// Every armed watch feeds its [`Activity`] signals into this one receiver.
/// Call `recv().await` to take the next signal. The channel is unbounded:
/// signals are low-rate (at most one per quiescence window per watch), so the
/// quiescence tasks never block sending, and the consumer sees every signal in
/// emission order.
pub type WatcherRx = mpsc::UnboundedReceiver<Activity>;

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
    /// Neither the native backend nor the polling fallback could arm the watch
    /// (for example, the path does not exist, or a resource limit was hit for
    /// both backends). Carries the watch name and the underlying notify error.
    Arm {
        /// Stable name of the watch that failed to arm.
        watch: String,
        /// The underlying notify error from the last backend attempted.
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

/// Decides which raw events may restart a watch's quiescence timer.
///
/// Combines the unconditional `<watch>/.git/` drop with the watch's gitignore
/// exclude patterns. Built once per watch at arm time (which is where an
/// invalid pattern is reported); thereafter [`accepts`](EventFilter::accepts)
/// is a pure, allocation-free classification of a path.
struct EventFilter {
    root: PathBuf,
    exclude: Gitignore,
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
        Ok(Self { root, exclude })
    }

    /// Whether an event touching `path` may restart the quiescence timer.
    ///
    /// `false` for anything under `<root>/.git/` and for anything matched by
    /// the exclude patterns; `true` otherwise.
    fn accepts(&self, path: &Path) -> bool {
        if self.under_git(path) {
            return false;
        }
        // A directory-only pattern (`target/`) only matches when the walker
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

/// The quiescence state machine: one instance runs per armed watch.
///
/// Consumes filtered-and-muted raw events from `rx` and emits one [`Activity`]
/// on `activity_tx` each time the watch settles. Every accepted event resets
/// the deadline to `now + quiesce`; when the deadline elapses with at least one
/// absorbed event, an [`Activity`] is emitted and the counter resets. The loop
/// parks on either the next event or the deadline — never busy-polls — and ends
/// when `rx` closes (the notify watcher was dropped, i.e. the watch disarmed).
///
/// This is deliberately independent of notify: `rx` is a plain channel, so
/// tests drive it with injected events under paused time.
async fn run_quiescence(
    name: String,
    quiesce: Duration,
    mut rx: mpsc::Receiver<Vec<PathBuf>>,
    filter: EventFilter,
    mute: Arc<AtomicUsize>,
    activity_tx: mpsc::UnboundedSender<Activity>,
) {
    let mut coalesced: usize = 0;
    // `None` while idle (no pending activity); `Some(instant)` once at least
    // one event has been absorbed and is awaiting the quiet window.
    let mut deadline: Option<Instant> = None;

    loop {
        let received = match deadline {
            // Idle: block indefinitely for the next event. No timer is armed,
            // so a quiet watch consumes nothing.
            None => rx.recv().await,
            Some(dl) => {
                let now = Instant::now();
                if now >= dl {
                    emit(&name, &mut coalesced, &mut deadline, &activity_tx);
                    continue;
                }
                match timeout(dl - now, rx.recv()).await {
                    Ok(received) => received,
                    // The window elapsed with no new event: the watch settled.
                    Err(_) => {
                        emit(&name, &mut coalesced, &mut deadline, &activity_tx);
                        continue;
                    }
                }
            }
        };

        match received {
            Some(paths) => {
                // Muted events are dropped before they can touch the timer, so
                // vard's own writes neither produce nor delay a signal.
                if mute.load(Ordering::SeqCst) > 0 {
                    continue;
                }
                if !paths.iter().any(|p| filter.accepts(p)) {
                    continue;
                }
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

/// Emits one [`Activity`] and resets the window state. A send failure means the
/// consumer dropped its [`WatcherRx`]; there is nothing to do but drop the
/// signal.
fn emit(
    name: &str,
    coalesced: &mut usize,
    deadline: &mut Option<Instant>,
    activity_tx: &mpsc::UnboundedSender<Activity>,
) {
    let _ = activity_tx.send(Activity {
        watch: name.to_string(),
        events_coalesced: *coalesced,
    });
    *coalesced = 0;
    *deadline = None;
}

/// A filesystem watcher that reports directory quiescence.
///
/// Construct with [`new`](Watcher::new), then [`arm`](Watcher::arm) each watch;
/// arming and disarming are dynamic, so watches can be added and removed while
/// the watcher runs. Every armed watch feeds the single [`WatcherRx`] returned
/// alongside the watcher.
///
/// The watcher is cheap to clone-by-handle: [`arm`](Watcher::arm) takes
/// `&self`, so one `Watcher` value serves the whole process.
pub struct Watcher {
    activity_tx: mpsc::UnboundedSender<Activity>,
}

impl Watcher {
    /// Creates a watcher and the receiver for every watch's activity signals.
    pub fn new() -> (Watcher, WatcherRx) {
        let (activity_tx, activity_rx) = mpsc::unbounded_channel();
        (Watcher { activity_tx }, activity_rx)
    }

    /// Arms a watch over `spec.path()`, recursively, and returns its handle.
    ///
    /// Validates the watch's exclude patterns first, failing with
    /// [`WatcherError::InvalidExclude`] (naming the watch and the offending
    /// pattern) before any watcher is created. Then arms the native backend,
    /// falling back to polling at [`DEFAULT_POLL_INTERVAL`] if the native
    /// backend fails — unless [`spec.poll_interval()`](WatchSpec::poll_interval)
    /// forces polling outright. The resulting [`ArmMode`] is on the handle.
    ///
    /// # Runtime
    ///
    /// Must be called from within a Tokio runtime: it spawns one task to own
    /// the watch's quiescence deadline. notify delivers events on its own
    /// thread(s), bridged into that task through a bounded channel.
    pub fn arm(&self, spec: &WatchSpec) -> Result<WatchHandle, WatcherError> {
        // Canonicalize so notify's (canonical) event paths line up with the
        // filter's root for the `.git/` and exclude checks. Fall back to the
        // path as given if it cannot be canonicalized yet.
        let root = spec
            .path()
            .canonicalize()
            .unwrap_or_else(|_| spec.path().to_path_buf());

        let filter = EventFilter::build(spec, root.clone())?;

        let (raw_tx, raw_rx) = mpsc::channel::<Vec<PathBuf>>(RAW_EVENT_CAPACITY);

        // notify runs the handler on its own thread(s). `try_send` never
        // blocks that thread; if the bounded channel is full the event is
        // dropped, which is harmless — a full channel already holds unprocessed
        // events that will each reset the deadline, so the dropped event
        // changes nothing observable but the (advisory) coalesced count.
        let make_handler = || {
            let tx = raw_tx.clone();
            move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result {
                    let _ = tx.try_send(event.paths);
                }
            }
        };

        let (backend, arm_mode) = arm_backend(spec, &root, make_handler)?;

        let mute = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_quiescence(
            spec.name().to_string(),
            spec.quiesce(),
            raw_rx,
            filter,
            Arc::clone(&mute),
            self.activity_tx.clone(),
        ));

        Ok(WatchHandle {
            backend: Some(backend),
            task,
            mute,
            arm_mode,
        })
    }
}

/// Arms the concrete notify backend for `spec`, returning it boxed (kept alive
/// by the [`WatchHandle`]) alongside the [`ArmMode`] it represents.
///
/// `make_handler` builds a fresh event handler on each call, so the native
/// attempt and the polling fallback each get their own.
fn arm_backend<F, H>(
    spec: &WatchSpec,
    root: &Path,
    make_handler: F,
) -> Result<(Box<dyn notify::Watcher + Send>, ArmMode), WatcherError>
where
    F: Fn() -> H,
    H: notify::EventHandler,
{
    // An explicit poll interval forces polling and never tries native.
    if let Some(period) = spec.poll_interval() {
        let backend = arm_poll(root, make_handler(), period).map_err(|e| WatcherError::Arm {
            watch: spec.name().to_string(),
            source: e,
        })?;
        return Ok((backend, ArmMode::Polling { period }));
    }

    match arm_native(root, make_handler()) {
        Ok(backend) => Ok((backend, ArmMode::Native)),
        // Native failed for this path (unsupported filesystem, resource limit,
        // ...): fall back to polling rather than failing the watch.
        Err(_native_err) => {
            let period = DEFAULT_POLL_INTERVAL;
            let backend =
                arm_poll(root, make_handler(), period).map_err(|e| WatcherError::Arm {
                    watch: spec.name().to_string(),
                    source: e,
                })?;
            Ok((backend, ArmMode::Polling { period }))
        }
    }
}

/// Creates and starts a native recommended watcher over `root`, recursively.
fn arm_native<H: notify::EventHandler>(
    root: &Path,
    handler: H,
) -> notify::Result<Box<dyn notify::Watcher + Send>> {
    let mut watcher = recommended_watcher(handler)?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(Box::new(watcher))
}

/// Creates and starts a polling watcher over `root`, recursively, at `period`.
fn arm_poll<H: notify::EventHandler>(
    root: &Path,
    handler: H,
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
/// events, which closes the bridge channel and ends the quiescence task.
pub struct WatchHandle {
    // `Option` so `Drop` can drop the backend explicitly before aborting the
    // task; always `Some` until drop.
    backend: Option<Box<dyn notify::Watcher + Send>>,
    task: JoinHandle<()>,
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
    /// Every filesystem event arriving while at least one guard is alive is
    /// dropped before it can restart the quiescence timer. Guards nest: the
    /// watch stays muted until the last one is dropped. Use this to suppress
    /// self-inflicted activity — vard mutes a watch around its own commits,
    /// restores, and syncs so those writes do not trigger a fresh snapshot.
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
        // Drop the backend first so notify stops delivering and closes the
        // bridge channel, then abort the task in case it is mid-window.
        self.backend = None;
        self.task.abort();
    }
}

/// A RAII mute: while it is alive, its watch drops every filesystem event.
///
/// Obtained from [`WatchHandle::mute`]. Dropping it decrements the watch's mute
/// count; the watch resumes accepting events once every guard is dropped.
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
    use super::*;

    /// Drives the quiescence machine directly with injected events, isolating
    /// it from notify. Returns the raw-event sender, the activity receiver, the
    /// mute handle, and the task handle.
    fn spawn_machine(
        name: &str,
        quiesce: Duration,
        root: PathBuf,
        exclude: &[&str],
    ) -> (
        mpsc::Sender<Vec<PathBuf>>,
        mpsc::UnboundedReceiver<Activity>,
        Arc<AtomicUsize>,
        JoinHandle<()>,
    ) {
        let spec = WatchSpec::builder(name, &root)
            .quiesce(quiesce)
            .exclude(exclude.iter().map(|s| s.to_string()))
            .build()
            .unwrap();
        let filter = EventFilter::build(&spec, root).unwrap();
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<PathBuf>>(RAW_EVENT_CAPACITY);
        let (act_tx, act_rx) = mpsc::unbounded_channel();
        let mute = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_quiescence(
            name.to_string(),
            quiesce,
            raw_rx,
            filter,
            Arc::clone(&mute),
            act_tx,
        ));
        (raw_tx, act_rx, mute, task)
    }

    /// Lets the spawned quiescence task make progress without advancing the
    /// paused clock (which `yield_now` does not touch).
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    // --- filter unit tests -------------------------------------------------

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
    fn build_rejects_invalid_exclude_naming_watch_and_pattern() {
        let root = PathBuf::from("/watch");
        // An unmatched-'[' character class is invalid gitignore glob syntax.
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

    // --- quiescence state-machine tests (paused time) ----------------------

    #[tokio::test(start_paused = true)]
    async fn fires_only_after_full_window_from_last_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let quiesce = Duration::from_secs(10);
        let (tx, mut rx, _mute, _task) = spawn_machine("w", quiesce, root.clone(), &[]);

        // Event at t=0 arms a deadline at t=10.
        tx.send(vec![root.join("a.txt")]).await.unwrap();
        settle().await;

        // A second event at t=5 pushes the deadline out to t=15.
        tokio::time::advance(Duration::from_secs(5)).await;
        tx.send(vec![root.join("b.txt")]).await.unwrap();
        settle().await;

        // At t=10 the original window would have elapsed, but the reset moved
        // it: nothing yet.
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "must not fire at t=10 after a reset"
        );

        // At t=15 the window since the last event elapses: one signal, both
        // events coalesced.
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        let activity = rx.try_recv().expect("must fire at t=15");
        assert_eq!(activity.watch, "w");
        assert_eq!(activity.events_coalesced, 2);
        assert!(rx.try_recv().is_err(), "exactly one signal per window");
    }

    #[tokio::test(start_paused = true)]
    async fn burst_of_events_coalesces_into_one_signal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let quiesce = Duration::from_secs(10);
        let (tx, mut rx, _mute, _task) = spawn_machine("w", quiesce, root.clone(), &[]);

        for i in 0..50 {
            tx.send(vec![root.join(format!("f{i}"))]).await.unwrap();
        }
        settle().await;

        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;

        let activity = rx.try_recv().expect("burst must produce one signal");
        assert_eq!(activity.events_coalesced, 50);
        assert!(rx.try_recv().is_err(), "a burst is one signal, not many");
    }

    #[tokio::test(start_paused = true)]
    async fn no_events_no_signal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (_tx, mut rx, _mute, _task) = spawn_machine("w", Duration::from_secs(10), root, &[]);

        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert!(rx.try_recv().is_err(), "a quiet watch never signals");
    }

    #[tokio::test(start_paused = true)]
    async fn muted_events_are_dropped_and_resume_when_unmuted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let quiesce = Duration::from_secs(10);
        let (tx, mut rx, mute, _task) = spawn_machine("w", quiesce, root.clone(), &[]);

        {
            let _guard = MuteGuard::new(Arc::clone(&mute));
            tx.send(vec![root.join("while_muted.txt")]).await.unwrap();
            settle().await;
            tokio::time::advance(Duration::from_secs(20)).await;
            settle().await;
            assert!(rx.try_recv().is_err(), "muted events must not signal");
        }

        // Guard dropped: the watch accepts events again.
        tx.send(vec![root.join("after_unmute.txt")]).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        let activity = rx.try_recv().expect("must signal once unmuted");
        assert_eq!(activity.events_coalesced, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn nested_mute_guards_stay_muted_until_last_drops() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, mut rx, mute, _task) =
            spawn_machine("w", Duration::from_secs(10), root.clone(), &[]);

        let outer = MuteGuard::new(Arc::clone(&mute));
        let inner = MuteGuard::new(Arc::clone(&mute));
        drop(inner);
        // Still muted: one guard remains.
        tx.send(vec![root.join("x.txt")]).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(20)).await;
        settle().await;
        assert!(rx.try_recv().is_err(), "still muted with one guard alive");
        drop(outer);
    }

    #[tokio::test(start_paused = true)]
    async fn git_and_excluded_paths_never_reset_the_timer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let quiesce = Duration::from_secs(10);
        let (tx, mut rx, _mute, _task) =
            spawn_machine("w", quiesce, root.clone(), &["target", "*.log"]);

        // A real event arms the window at t=10.
        tx.send(vec![root.join("real.txt")]).await.unwrap();
        settle().await;

        // Ignored events at t=5 must NOT reset the deadline.
        tokio::time::advance(Duration::from_secs(5)).await;
        tx.send(vec![root.join(".git/index")]).await.unwrap();
        tx.send(vec![root.join("target/x")]).await.unwrap();
        tx.send(vec![root.join("debug.log")]).await.unwrap();
        settle().await;

        // The window still elapses at t=10 as if those events never happened.
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        let activity = rx
            .try_recv()
            .expect("ignored events do not delay the signal");
        assert_eq!(
            activity.events_coalesced, 1,
            "only the one real event was counted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_watches_are_independent() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let root_a = dir_a.path().to_path_buf();
        let root_b = dir_b.path().to_path_buf();
        let (tx_a, mut rx_a, _ma, _ta) =
            spawn_machine("a", Duration::from_secs(10), root_a.clone(), &[]);
        let (tx_b, mut rx_b, _mb, _tb) =
            spawn_machine("b", Duration::from_secs(10), root_b.clone(), &[]);

        // Only watch a sees activity.
        tx_a.send(vec![root_a.join("f")]).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;

        let a = rx_a.try_recv().expect("watch a fires");
        assert_eq!(a.watch, "a");
        assert!(rx_b.try_recv().is_err(), "watch b saw no events");

        // b remains armable afterwards, independent of a.
        tx_b.send(vec![root_b.join("g")]).await.unwrap();
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(rx_b.try_recv().expect("watch b fires").watch, "b");
    }

    #[tokio::test(start_paused = true)]
    async fn disarm_stops_signals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (tx, mut rx, _mute, task) =
            spawn_machine("w", Duration::from_secs(10), root.clone(), &[]);

        tx.send(vec![root.join("f")]).await.unwrap();
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

    // --- surface smoke test ------------------------------------------------

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
    fn arm_mode_reports_forced_polling_period() {
        // Pure value check on ArmMode; no runtime needed.
        let mode = ArmMode::Polling {
            period: Duration::from_secs(3),
        };
        assert_eq!(
            mode,
            ArmMode::Polling {
                period: Duration::from_secs(3)
            }
        );
        assert_ne!(mode, ArmMode::Native);
    }
}
