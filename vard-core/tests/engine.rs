//! Integration tests for the snapshot [`Engine`] wired to the real watcher and
//! scheduler.
//!
//! The deterministic worker logic (coalescing, provenance, lock retry,
//! self-suppression, unsafe/resume) is covered by the paused-time unit tests in
//! `src/engine.rs`. These tests prove the other half: that
//! [`Engine::start`](vard_core::Engine::start) actually arms the platform
//! watcher, spawns per-watch workers, and turns a real filesystem write into a
//! [`Event::SnapshotCompleted`] — and that two watches make progress
//! independently. Most use a fake [`VcsBackend`] (so no git is required); one
//! exercises a real [`GitBackend`](vard_core::GitBackend) end to end to show
//! that vard's own commit does not loop into a second snapshot. That the mute
//! is what prevents the loop is pinned by the unit test
//! `worker_is_muted_across_the_operation_and_released_after`; end to end the
//! clean post-commit tree would also converge, so this test demonstrates the
//! wired-up behavior rather than isolating the mute. Generous timeouts keep
//! them robust, in the style of `tests/watcher.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::time::timeout;
use vard_core::{
    AdvanceOutcome, ChangeSummary, Engine, Event, EventReceiver, LogFilter, PushOutcome,
    ReconcileOutcome, RemoteState, RestoreTarget, SafeState, Snapshot, SnapshotId, SnapshotOutcome,
    SnapshotRequest, TriggerMode, VcsBackend, VcsError, VcsRef, WatchSpec,
};

/// A short quiescence window: long enough to absorb event-delivery latency,
/// short enough to keep the test quick.
const QUIESCE: Duration = Duration::from_millis(400);

/// How long a test waits for an expected event before failing. Generous, to
/// absorb FSEvents/inotify latency and CI scheduling jitter.
const WAIT: Duration = Duration::from_secs(20);

/// A fake backend that models real tree dirtiness: it commits only when the
/// test has marked the tree dirty, and clears that mark on commit, so the
/// worker's post-op re-check converges. Requires no git.
///
/// Modeling dirtiness this way (rather than "the first call commits") keeps the
/// test robust against the platform watcher's spurious/early events on a
/// freshly armed watch: an activity signal with no pending change snapshots
/// nothing, exactly as a real backend's `add -A` on a clean tree would.
struct FakeBackend {
    dirty: AtomicBool,
    committed: AtomicUsize,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dirty: AtomicBool::new(false),
            committed: AtomicUsize::new(0),
        })
    }

    /// Marks the tree dirty so the next snapshot commits. The test calls this
    /// immediately before the filesystem write it expects to be captured.
    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }
}

impl VcsBackend for FakeBackend {
    fn is_safe_state(&self) -> Result<SafeState, VcsError> {
        Ok(SafeState::Safe)
    }

    fn is_dirty(&self) -> Result<bool, VcsError> {
        Ok(self.dirty.load(Ordering::SeqCst))
    }

    fn snapshot(&self, _req: &SnapshotRequest) -> Result<Option<SnapshotOutcome>, VcsError> {
        if self.dirty.swap(false, Ordering::SeqCst) {
            self.committed.fetch_add(1, Ordering::SeqCst);
            Ok(Some(SnapshotOutcome {
                id: SnapshotId::new("cafef00d"),
                summary: ChangeSummary {
                    changed: 1,
                    added: 0,
                    deleted: 0,
                    notable: vec!["notes.md".to_string()],
                },
            }))
        } else {
            Ok(None)
        }
    }

    fn log(&self, _filter: &LogFilter) -> Result<Vec<Snapshot>, VcsError> {
        Ok(Vec::new())
    }

    fn diff(
        &self,
        _from: &VcsRef,
        _to: Option<&VcsRef>,
        _pathspec: Option<&std::path::Path>,
    ) -> Result<String, VcsError> {
        Ok(String::new())
    }

    fn verify_ref(&self, _rev: &VcsRef) -> Result<bool, VcsError> {
        unimplemented!("verify_ref is out of scope for the snapshot engine")
    }

    fn path_exists_at(&self, _rev: &VcsRef, _path: &std::path::Path) -> Result<bool, VcsError> {
        unimplemented!("path_exists_at is out of scope for the snapshot engine")
    }

    fn restore(&self, _target: &RestoreTarget) -> Result<(), VcsError> {
        unimplemented!("restore is out of scope for the snapshot engine")
    }

    fn fetch(&self, _timeout: Duration) -> Result<RemoteState, VcsError> {
        unimplemented!("fetch is out of scope for the snapshot engine")
    }

    fn reconcile(&self, _scratch: &Path) -> Result<ReconcileOutcome, VcsError> {
        unimplemented!("reconcile is out of scope for the snapshot engine")
    }

    fn advance(
        &self,
        _target: &SnapshotId,
        _expected_tip: &SnapshotId,
    ) -> Result<AdvanceOutcome, VcsError> {
        unimplemented!("advance is out of scope for the snapshot engine")
    }

    fn prune_scratch(&self, _scratch: &Path) -> Result<(), VcsError> {
        unimplemented!("prune_scratch is out of scope for the snapshot engine")
    }

    fn push(&self, _timeout: Duration) -> Result<PushOutcome, VcsError> {
        unimplemented!("push is out of scope for the snapshot engine")
    }
}

/// Awaits the next [`Event::SnapshotCompleted`] for `watch`, failing on a
/// snapshot failure for it or on timeout.
async fn expect_completed(events: &mut EventReceiver, watch: &str) {
    let wait = timeout(WAIT, async {
        loop {
            match events.recv().await {
                Ok(Event::SnapshotCompleted { watch: w, .. }) if w == watch => break,
                Ok(Event::SnapshotFailed {
                    watch: w, error, ..
                }) if w == watch => {
                    panic!("watch {w:?} reported a failed snapshot: {error}")
                }
                Ok(_) => continue,
                Err(e) => panic!("event channel error: {e:?}"),
            }
        }
    });
    wait.await
        .unwrap_or_else(|_| panic!("no snapshot for watch {watch:?} within {WAIT:?}"));
}

#[tokio::test]
async fn real_write_flows_through_to_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FakeBackend::new();
    let spec = WatchSpec::builder("notes", dir.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();

    let engine = Engine::builder()
        .watch_with_backend(spec, backend.clone())
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    engine.start().await.unwrap();

    // A real filesystem write, absorbed by the quiescence window, becomes a
    // single snapshot through the whole watcher -> dispatcher -> worker ->
    // backend pipeline.
    backend.mark_dirty();
    std::fs::write(dir.path().join("notes.md"), b"hello").unwrap();

    expect_completed(&mut events, "notes").await;
    assert_eq!(
        backend.committed.load(Ordering::SeqCst),
        1,
        "exactly one commit for the write"
    );
}

#[tokio::test]
async fn two_watches_make_progress_independently() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let backend_a = FakeBackend::new();
    let backend_b = FakeBackend::new();

    let spec_a = WatchSpec::builder("a", dir_a.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();
    let spec_b = WatchSpec::builder("b", dir_b.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();

    let engine = Engine::builder()
        .watch_with_backend(spec_a, backend_a.clone())
        .watch_with_backend(spec_b, backend_b.clone())
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    engine.start().await.unwrap();

    // Only watch a sees a write first; it must snapshot without waiting on b.
    backend_a.mark_dirty();
    std::fs::write(dir_a.path().join("only-a.txt"), b"a").unwrap();
    expect_completed(&mut events, "a").await;

    // Then b sees a write and snapshots independently.
    backend_b.mark_dirty();
    std::fs::write(dir_b.path().join("only-b.txt"), b"b").unwrap();
    expect_completed(&mut events, "b").await;

    assert_eq!(backend_a.committed.load(Ordering::SeqCst), 1);
    assert_eq!(backend_b.committed.load(Ordering::SeqCst), 1);
}

/// A fake backend whose in-flight commit can be gated by the test, so a
/// shutdown can be exercised while a pass is genuinely mid-flight.
///
/// Like [`FakeBackend`] it commits only when the tree has been marked dirty (so
/// the post-op re-check converges). When a gate is installed, the dirty commit
/// blocks on it — flagging `at_gate` first — until the test releases it (or
/// never, to model a stuck worker).
struct GatedBackend {
    dirty: AtomicBool,
    committed: AtomicUsize,
    /// Set true while a commit is parked on the gate: precise proof the worker
    /// is blocked *inside* the snapshot, not merely that a call happened.
    at_gate: AtomicBool,
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl GatedBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dirty: AtomicBool::new(false),
            committed: AtomicUsize::new(0),
            at_gate: AtomicBool::new(false),
            gate: Mutex::new(None),
        })
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Installs a gate that the next dirty commit blocks on. Returns the sender
    /// that releases it; never sending models a commit that never returns.
    fn install_gate(&self) -> std::sync::mpsc::Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        *self.gate.lock().unwrap() = Some(rx);
        tx
    }
}

impl VcsBackend for GatedBackend {
    fn is_safe_state(&self) -> Result<SafeState, VcsError> {
        Ok(SafeState::Safe)
    }

    fn is_dirty(&self) -> Result<bool, VcsError> {
        Ok(self.dirty.load(Ordering::SeqCst))
    }

    fn snapshot(&self, _req: &SnapshotRequest) -> Result<Option<SnapshotOutcome>, VcsError> {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
        // A dirty commit: park on the gate if one is installed, so the test can
        // hold the pass in flight across a shutdown.
        if let Some(rx) = self.gate.lock().unwrap().take() {
            self.at_gate.store(true, Ordering::SeqCst);
            let _ = rx.recv();
            self.at_gate.store(false, Ordering::SeqCst);
        }
        self.committed.fetch_add(1, Ordering::SeqCst);
        Ok(Some(SnapshotOutcome {
            id: SnapshotId::new("beadfeed"),
            summary: ChangeSummary {
                changed: 1,
                added: 0,
                deleted: 0,
                notable: vec!["notes.md".to_string()],
            },
        }))
    }

    fn log(&self, _filter: &LogFilter) -> Result<Vec<Snapshot>, VcsError> {
        Ok(Vec::new())
    }

    fn diff(
        &self,
        _from: &VcsRef,
        _to: Option<&VcsRef>,
        _pathspec: Option<&std::path::Path>,
    ) -> Result<String, VcsError> {
        Ok(String::new())
    }

    fn verify_ref(&self, _rev: &VcsRef) -> Result<bool, VcsError> {
        unimplemented!("verify_ref is out of scope for the snapshot engine")
    }

    fn path_exists_at(&self, _rev: &VcsRef, _path: &std::path::Path) -> Result<bool, VcsError> {
        unimplemented!("path_exists_at is out of scope for the snapshot engine")
    }

    fn restore(&self, _target: &RestoreTarget) -> Result<(), VcsError> {
        unimplemented!("restore is out of scope for the snapshot engine")
    }

    fn fetch(&self, _timeout: Duration) -> Result<RemoteState, VcsError> {
        unimplemented!("fetch is out of scope for the snapshot engine")
    }

    fn reconcile(&self, _scratch: &Path) -> Result<ReconcileOutcome, VcsError> {
        unimplemented!("reconcile is out of scope for the snapshot engine")
    }

    fn advance(
        &self,
        _target: &SnapshotId,
        _expected_tip: &SnapshotId,
    ) -> Result<AdvanceOutcome, VcsError> {
        unimplemented!("advance is out of scope for the snapshot engine")
    }

    fn prune_scratch(&self, _scratch: &Path) -> Result<(), VcsError> {
        unimplemented!("prune_scratch is out of scope for the snapshot engine")
    }

    fn push(&self, _timeout: Duration) -> Result<PushOutcome, VcsError> {
        unimplemented!("push is out of scope for the snapshot engine")
    }
}

/// Polls `cond` until it holds or `WAIT` elapses, yielding between checks so
/// the worker's tasks make progress.
async fn wait_until(mut cond: impl FnMut() -> bool) {
    let ok = timeout(WAIT, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "condition never held within {WAIT:?}");
}

/// Drains all events currently queued without blocking, returning them in order.
fn drain_events(events: &mut EventReceiver) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(ev) = events.try_recv() {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn shutdown_drains_an_in_flight_pass() {
    // A pass caught mid-commit must finish (emitting SnapshotCompleted) before
    // shutdown reports DaemonStopped — the drain never abandons in-flight work.
    let dir = tempfile::tempdir().unwrap();
    let backend = GatedBackend::new();
    let release = backend.install_gate();
    backend.mark_dirty();

    let spec = WatchSpec::builder("notes", dir.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();
    let engine = Engine::builder()
        .watch_with_backend(spec, backend.clone())
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    std::fs::write(dir.path().join("notes.md"), b"hello").unwrap();
    // Wait until the worker is genuinely parked inside the gated commit.
    wait_until(|| backend.at_gate.load(Ordering::SeqCst)).await;

    // Shut down while the pass is in flight; release the gate a beat later so
    // shutdown's worker-drain is what waits for the commit to finish.
    let ((), ()) = tokio::join!(handle.shutdown(), async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = release.send(());
    });

    assert_eq!(
        backend.committed.load(Ordering::SeqCst),
        1,
        "the in-flight commit must have completed during the drain"
    );

    // The drained event stream shows the snapshot landing before the stop.
    let evs = drain_events(&mut events);
    let completed = evs
        .iter()
        .position(|e| matches!(e, Event::SnapshotCompleted { .. }));
    let stopped = evs.iter().position(|e| matches!(e, Event::DaemonStopped));
    let completed = completed.expect("an in-flight snapshot must complete");
    let stopped = stopped.expect("shutdown must emit DaemonStopped");
    assert!(
        completed < stopped,
        "SnapshotCompleted must precede DaemonStopped, got {evs:?}"
    );
}

#[tokio::test]
async fn shutdown_stops_all_tasks_and_emits_no_events_after() {
    // After shutdown returns, every task has joined: DaemonStopped is the last
    // event and the keepalive cycle is broken, so a later write snapshots nothing.
    let dir = tempfile::tempdir().unwrap();
    let backend = GatedBackend::new();
    let spec = WatchSpec::builder("notes", dir.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();
    let engine = Engine::builder()
        .watch_with_backend(spec, backend.clone())
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    backend.mark_dirty();
    std::fs::write(dir.path().join("notes.md"), b"hello").unwrap();
    expect_completed(&mut events, "notes").await;

    handle.shutdown().await;

    // DaemonStopped is present and nothing follows it in the stream.
    let evs = drain_events(&mut events);
    assert_eq!(
        evs.last(),
        Some(&Event::DaemonStopped),
        "DaemonStopped must be the final event, got {evs:?}"
    );
    // Every task has joined and the bus has no senders left: the receiver is
    // Closed, not merely Empty — proof no worker or dispatcher survives.
    assert_eq!(
        events.try_recv(),
        Err(vard_core::TryRecvError::Closed),
        "the event bus must be closed once every task has joined"
    );

    // The keepalive cycle is broken: the worker is gone, so a fresh write drives
    // no further commit.
    let committed_before = backend.committed.load(Ordering::SeqCst);
    backend.mark_dirty();
    std::fs::write(dir.path().join("notes.md"), b"world").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        backend.committed.load(Ordering::SeqCst),
        committed_before,
        "no worker should remain to snapshot after shutdown"
    );
}

#[tokio::test]
async fn a_second_engine_can_watch_the_same_path_after_shutdown() {
    // Shutting the first engine down releases the notify backend on the path, so
    // a second engine can arm the same directory and make progress.
    let dir = tempfile::tempdir().unwrap();

    let backend_a = GatedBackend::new();
    let spec_a = WatchSpec::builder("notes", dir.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();
    let engine_a = Engine::builder()
        .watch_with_backend(spec_a, backend_a.clone())
        .build()
        .unwrap();
    let mut events_a = engine_a.subscribe();
    let handle_a = engine_a.start().await.unwrap();

    backend_a.mark_dirty();
    std::fs::write(dir.path().join("first.md"), b"a").unwrap();
    expect_completed(&mut events_a, "notes").await;
    handle_a.shutdown().await;

    // A brand-new engine over the very same path.
    let backend_b = GatedBackend::new();
    let spec_b = WatchSpec::builder("notes", dir.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();
    let engine_b = Engine::builder()
        .watch_with_backend(spec_b, backend_b.clone())
        .build()
        .unwrap();
    let mut events_b = engine_b.subscribe();
    let handle_b = engine_b.start().await.unwrap();

    backend_b.mark_dirty();
    std::fs::write(dir.path().join("second.md"), b"b").unwrap();
    expect_completed(&mut events_b, "notes").await;
    assert_eq!(backend_b.committed.load(Ordering::SeqCst), 1);

    handle_b.shutdown().await;
}

#[tokio::test]
async fn shutdown_aborts_a_worker_stuck_past_the_drain_timeout() {
    // A worker still running a pass when the drain budget elapses is aborted, and
    // shutdown still completes with DaemonStopped rather than hanging forever.
    let dir = tempfile::tempdir().unwrap();
    let backend = GatedBackend::new();
    // Install a gate we never release: the commit blocks indefinitely.
    let _release = backend.install_gate();
    backend.mark_dirty();

    let spec = WatchSpec::builder("notes", dir.path())
        .trigger(TriggerMode::Events)
        .quiesce(QUIESCE)
        .build()
        .unwrap();
    let engine = Engine::builder()
        .watch_with_backend(spec, backend.clone())
        .shutdown_drain_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    std::fs::write(dir.path().join("notes.md"), b"hello").unwrap();
    wait_until(|| backend.at_gate.load(Ordering::SeqCst)).await;

    // Shutdown must return despite the stuck worker (bounded by the drain
    // timeout, then an abort). A generous outer timeout guards against a hang.
    timeout(WAIT, handle.shutdown())
        .await
        .expect("shutdown must not hang on a stuck worker");

    // The stuck commit never finished, yet the engine wound down cleanly.
    assert_eq!(
        backend.committed.load(Ordering::SeqCst),
        0,
        "the stuck commit must not have completed"
    );
    let evs = drain_events(&mut events);
    assert!(
        evs.contains(&Event::DaemonStopped),
        "shutdown must emit DaemonStopped even after aborting a stuck worker, got {evs:?}"
    );
}

/// Runs a git command in `dir`, asserting success.
fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("failed to spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Asserts no [`Event::SnapshotCompleted`] for `watch` arrives within `window`.
async fn assert_no_snapshot(events: &mut EventReceiver, watch: &str, window: Duration) {
    let result = timeout(window, async {
        loop {
            match events.recv().await {
                Ok(Event::SnapshotCompleted { watch: w, .. }) if w == watch => {
                    panic!(
                        "watch {w:?} took an unexpected second snapshot (self-suppression failed)"
                    )
                }
                Ok(_) => continue,
                Err(e) => panic!("event channel error: {e:?}"),
            }
        }
    })
    .await;
    // Timing out is the success case: no snapshot fired in the window.
    assert!(result.is_err());
}

#[tokio::test]
async fn vards_own_commit_does_not_retrigger_a_snapshot() {
    // A real git repository exercises self-suppression end to end: after the
    // snapshot commits, the writes it makes under `.git/` must not feed back as
    // fresh activity and loop. Note this does not isolate the mute — the
    // watcher drops `.git/` unconditionally and the post-commit tree is clean,
    // so the pass would converge without the mute too. The mute's necessity is
    // pinned by the unit test
    // `worker_is_muted_across_the_operation_and_released_after`.
    let dir = tempfile::tempdir().unwrap();
    git_ok(dir.path(), &["init", "-b", "main"]);
    git_ok(
        dir.path(),
        &["config", "user.email", "vard-test@example.com"],
    );
    git_ok(dir.path(), &["config", "user.name", "Vard Test"]);
    git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);

    let spec = WatchSpec::builder("repo", dir.path())
        .trigger(TriggerMode::Events)
        .branch("main")
        .quiesce(QUIESCE)
        .build()
        .unwrap();

    // `watch` (not `watch_with_backend`) opens a real GitBackend at build time.
    let engine = Engine::builder().watch(spec).build().unwrap();
    let mut events = engine.subscribe();
    engine.start().await.unwrap();

    std::fs::write(dir.path().join("notes.md"), b"hello vard").unwrap();
    expect_completed(&mut events, "repo").await;

    // No second snapshot: vard's own commit does not loop.
    assert_no_snapshot(&mut events, "repo", Duration::from_millis(1500)).await;

    // The commit really landed, tagged with its trigger trailer.
    let log = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["log", "--format=%s%n%(trailers:key=Vard-Trigger)"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains("snapshot:"),
        "expected a snapshot commit: {log}"
    );
    assert!(
        log.contains("Vard-Trigger: event"),
        "expected trailer: {log}"
    );
}

// --- sync cycle (real git + a bare file remote) ----------------------------

/// Captures git stdout in `dir`, asserting success.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("failed to spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Deterministic, signer-free commit identity so commits succeed in CI.
fn configure(dir: &Path) {
    git_ok(dir, &["config", "user.email", "vard-test@example.com"]);
    git_ok(dir, &["config", "user.name", "Vard Test"]);
    git_ok(dir, &["config", "commit.gpgsign", "false"]);
}

/// A bare repository usable as a file remote (`origin`).
fn bare_origin() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(d.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    d
}

/// A working repo on `main` with `origin` set to `origin_path`, a base commit,
/// and `main` pushed — so the remote exists and the two agree at the base.
fn synced_repo(origin_path: &Path) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    git_ok(d.path(), &["init", "-b", "main"]);
    configure(d.path());
    git_ok(
        d.path(),
        &["remote", "add", "origin", origin_path.to_str().unwrap()],
    );
    std::fs::write(d.path().join("base.txt"), "base\n").unwrap();
    git_ok(d.path(), &["add", "-A"]);
    git_ok(d.path(), &["commit", "-m", "base"]);
    git_ok(d.path(), &["push", "-u", "origin", "main"]);
    let path = d.path().to_path_buf();
    (d, path)
}

/// A second clone of `origin_path` that can move the remote out from under the
/// watched repo.
fn mover(origin_path: &Path) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("clone");
    let out = Command::new("git")
        .args(["clone", origin_path.to_str().unwrap()])
        .arg(&dest)
        .output()
        .unwrap();
    assert!(out.status.success(), "clone failed");
    configure(&dest);
    (d, dest)
}

/// Builds an interval-only (so no filesystem noise) sync-capable spec with a
/// long interval that will not tick during the test.
fn sync_spec(name: &str, path: &Path, scratch: Option<&Path>, sync: bool) -> WatchSpec {
    let mut builder = WatchSpec::builder(name, path)
        .trigger(TriggerMode::Interval)
        .interval(Duration::from_secs(3600))
        .branch("main")
        .remote("origin")
        .sync(sync);
    if let Some(scratch) = scratch {
        builder = builder.scratch_dir(scratch);
    }
    builder.build().unwrap()
}

/// Awaits the next event matching `pred`, failing on timeout.
async fn recv_matching(events: &mut EventReceiver, mut pred: impl FnMut(&Event) -> bool) -> Event {
    timeout(WAIT, async {
        loop {
            match events.recv().await {
                Ok(ev) if pred(&ev) => break ev,
                Ok(_) => continue,
                Err(e) => panic!("event channel error: {e:?}"),
            }
        }
    })
    .await
    .expect("expected event within WAIT")
}

fn is_sync_event(ev: &Event) -> bool {
    matches!(
        ev,
        Event::SyncPushed { .. }
            | Event::SyncPulled { .. }
            | Event::SyncConflict { .. }
            | Event::SyncFailed { .. }
    )
}

/// Asserts no sync event arrives within `window` (success is a timeout).
async fn assert_no_sync(events: &mut EventReceiver, window: Duration) {
    let result = timeout(window, async {
        loop {
            match events.recv().await {
                Ok(ev) if is_sync_event(&ev) => panic!("unexpected sync event: {ev:?}"),
                Ok(_) => continue,
                Err(e) => panic!("event channel error: {e:?}"),
            }
        }
    })
    .await;
    assert!(result.is_err(), "expected no sync event within {window:?}");
}

#[tokio::test]
async fn sync_happy_path_pulls_the_remote_and_pushes_local() {
    let origin = bare_origin();
    let (_a_dir, a) = synced_repo(origin.path());
    let (_m_dir, m) = mover(origin.path());

    // The remote advances with a non-conflicting file.
    std::fs::write(m.join("b.txt"), "from-remote\n").unwrap();
    git_ok(&m, &["add", "-A"]);
    git_ok(&m, &["commit", "-m", "remote work"]);
    git_ok(&m, &["push", "origin", "main"]);

    // The watched repo has its own un-pushed commit: the cycle must integrate
    // the remote (pull) and then send the local commit (push).
    std::fs::write(a.join("a.txt"), "from-local\n").unwrap();
    git_ok(&a, &["add", "-A"]);
    git_ok(&a, &["commit", "-m", "local work"]);

    let scratch_root = tempfile::tempdir().unwrap();
    let scratch = scratch_root.path().join("scratch");
    let engine = Engine::builder()
        .watch(sync_spec("proj", &a, Some(&scratch), true))
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    assert!(handle.request_sync("proj"));
    assert!(matches!(
        recv_matching(&mut events, |e| matches!(e, Event::SyncPulled { .. })).await,
        Event::SyncPulled { .. }
    ));
    match recv_matching(&mut events, |e| matches!(e, Event::SyncPushed { .. })).await {
        Event::SyncPushed { commits, .. } => assert_eq!(commits, 1, "one local commit pushed"),
        other => panic!("expected SyncPushed, got {other:?}"),
    }

    // Both sides landed, the remote received the local commit, and the watch is
    // healthy (Ok) throughout — no state-change events.
    assert!(a.join("a.txt").exists() && a.join("b.txt").exists());
    assert_eq!(
        git_out(&a, &["rev-parse", "HEAD"]),
        git_out(&a, &["rev-parse", "origin/main"]),
        "the remote received the local tip"
    );
    let states = handle.watch_states();
    assert_eq!(states[0].state, vard_core::WatchState::Ok);
    assert!(!scratch.exists(), "the scratch worktree is cleaned up");
}

#[tokio::test]
async fn sync_pull_only_advances_without_pushing() {
    let origin = bare_origin();
    let (_a_dir, a) = synced_repo(origin.path());
    let (_m_dir, m) = mover(origin.path());

    std::fs::write(m.join("b.txt"), "from-remote\n").unwrap();
    git_ok(&m, &["add", "-A"]);
    git_ok(&m, &["commit", "-m", "remote work"]);
    git_ok(&m, &["push", "origin", "main"]);

    let prev = git_out(&a, &["rev-parse", "HEAD"]);
    let scratch_root = tempfile::tempdir().unwrap();
    let scratch = scratch_root.path().join("scratch");
    let engine = Engine::builder()
        .watch(sync_spec("proj", &a, Some(&scratch), true))
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    assert!(handle.request_sync("proj"));
    match recv_matching(&mut events, |e| matches!(e, Event::SyncPulled { .. })).await {
        Event::SyncPulled {
            prev_ref, new_ref, ..
        } => {
            assert_eq!(prev_ref, prev);
            assert_ne!(new_ref, prev, "the advance moved the tree forward");
        }
        other => panic!("expected SyncPulled, got {other:?}"),
    }
    // Nothing to push (local was purely behind): no SyncPushed follows.
    assert_no_sync(&mut events, Duration::from_millis(600)).await;
    assert!(a.join("b.txt").exists(), "the remote change was pulled in");
}

#[tokio::test]
async fn sync_conflict_latches_snapshots_still_work_and_a_manual_resolve_clears_it() {
    let origin = bare_origin();
    let (_a_dir, a) = synced_repo(origin.path());
    let (_m_dir, m) = mover(origin.path());

    // Remote changes the shared file...
    std::fs::write(m.join("base.txt"), "remote-change\n").unwrap();
    git_ok(&m, &["add", "-A"]);
    git_ok(&m, &["commit", "-m", "remote edit"]);
    git_ok(&m, &["push", "origin", "main"]);
    // ...and the watched repo changes the same line locally (committed) → conflict.
    std::fs::write(a.join("base.txt"), "local-change\n").unwrap();
    git_ok(&a, &["add", "-A"]);
    git_ok(&a, &["commit", "-m", "local edit"]);

    let scratch_root = tempfile::tempdir().unwrap();
    let scratch = scratch_root.path().join("scratch");
    let engine = Engine::builder()
        .watch(sync_spec("proj", &a, Some(&scratch), true))
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    assert!(handle.request_sync("proj"));
    assert!(matches!(
        recv_matching(&mut events, |e| matches!(e, Event::SyncConflict { .. })).await,
        Event::SyncConflict { .. }
    ));
    wait_until(|| handle.watch_states()[0].state == vard_core::WatchState::Conflicted).await;

    // Auto-sync is suppressed while the conflict latches: an automatic request
    // triggers no cycle.
    assert!(handle.request_auto_sync("proj"));
    assert_no_sync(&mut events, Duration::from_millis(600)).await;
    assert_eq!(
        handle.watch_states()[0].state,
        vard_core::WatchState::Conflicted,
        "the conflict still latches"
    );

    // Snapshots STILL work while latched: a manual snapshot commits.
    std::fs::write(a.join("new.txt"), "more local work\n").unwrap();
    assert!(handle.trigger("proj"));
    expect_completed(&mut events, "proj").await;
    assert_eq!(
        handle.watch_states()[0].state,
        vard_core::WatchState::Conflicted,
        "a snapshot does not clear the conflict"
    );

    // The user resolves (adopt the remote), and a manual cycle clears it.
    git_ok(&a, &["reset", "--hard", "origin/main"]);
    assert!(handle.request_sync("proj"));
    wait_until(|| handle.watch_states()[0].state == vard_core::WatchState::Ok).await;
}

#[tokio::test]
async fn sync_false_watch_never_syncs() {
    let origin = bare_origin();
    let (_a_dir, a) = synced_repo(origin.path());
    let scratch_root = tempfile::tempdir().unwrap();
    let scratch = scratch_root.path().join("scratch");

    let engine = Engine::builder()
        .watch(sync_spec("proj", &a, Some(&scratch), false))
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    assert!(handle.request_sync("proj"), "the request is delivered");
    // ...but a sync=false watch does nothing with it.
    assert_no_sync(&mut events, Duration::from_secs(1)).await;
}

#[tokio::test]
async fn sync_without_a_scratch_dir_is_disabled_even_when_sync_is_true() {
    let origin = bare_origin();
    let (_a_dir, a) = synced_repo(origin.path());

    // sync = true but NO scratch dir injected: sync is disabled (the chosen
    // unset-scratch semantics).
    let engine = Engine::builder()
        .watch(sync_spec("proj", &a, None, true))
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    assert!(handle.request_sync("proj"));
    assert_no_sync(&mut events, Duration::from_secs(1)).await;
}

#[tokio::test]
async fn sync_commits_dirty_local_work_with_a_vard_host_trailer_and_pushes_it() {
    let origin = bare_origin();
    let (_a_dir, a) = synced_repo(origin.path());

    // Uncommitted local edits, and the remote has not moved: the fetch reports
    // nothing to pull or push, but the dirty tree must still be captured by the
    // pre-sync snapshot and pushed (the short-circuit's clean-tree check).
    std::fs::write(a.join("draft.txt"), "local work\n").unwrap();

    let scratch_root = tempfile::tempdir().unwrap();
    let scratch = scratch_root.path().join("scratch");
    let engine = Engine::builder()
        .watch(sync_spec("proj", &a, Some(&scratch), true))
        .build()
        .unwrap();
    let mut events = engine.subscribe();
    let handle = engine.start().await.unwrap();

    assert!(handle.request_sync("proj"));
    match recv_matching(&mut events, |e| matches!(e, Event::SyncPushed { .. })).await {
        Event::SyncPushed { commits, .. } => {
            assert_eq!(commits, 1, "the pre-sync commit is the one commit pushed")
        }
        other => panic!("expected SyncPushed, got {other:?}"),
    }

    // The pre-sync snapshot carries both the pre-sync trigger trailer and a
    // Vard-Host trailer naming the machine that took it.
    let body = git_out(&a, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Vard-Trigger: pre-sync"),
        "pre-sync trigger trailer missing: {body:?}"
    );
    assert!(
        body.contains("Vard-Host: "),
        "Vard-Host trailer missing: {body:?}"
    );

    // The commit really reached the remote.
    let local_tip = git_out(&a, &["rev-parse", "HEAD"]);
    let remote_tip = git_out(origin.path(), &["rev-parse", "refs/heads/main"]);
    assert_eq!(
        local_tip, remote_tip,
        "the remote received the local commit"
    );
}
