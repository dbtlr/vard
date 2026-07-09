//! Integration tests for the snapshot [`Engine`] wired to the real watcher and
//! scheduler.
//!
//! The deterministic worker logic (coalescing, provenance, lock retry,
//! self-suppression, unsafe/resume) is covered by the paused-time unit tests in
//! `src/engine.rs`. These tests prove the other half: that
//! [`Engine::start`](vard_core::Engine::start) actually arms the platform
//! watcher, spawns per-watch workers, and turns a real filesystem write into a
//! [`Event::SnapshotCompleted`] — and that two watches make progress
//! independently. They use a fake [`VcsBackend`] (so no git is required) with
//! generous timeouts, in the style of `tests/watcher.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::time::timeout;
use vard_core::{
    ChangeSummary, Engine, Event, EventReceiver, LogFilter, PushOutcome, ReconcileOutcome,
    RemoteState, RestoreTarget, SafeState, Snapshot, SnapshotId, SnapshotOutcome, SnapshotRequest,
    TriggerMode, VcsBackend, VcsError, VcsRef, WatchSpec,
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

    fn diff(&self, _from: &VcsRef, _to: Option<&VcsRef>) -> Result<String, VcsError> {
        Ok(String::new())
    }

    fn restore(&self, _target: &RestoreTarget) -> Result<(), VcsError> {
        unimplemented!("restore is out of scope for the snapshot engine")
    }

    fn fetch(&self) -> Result<RemoteState, VcsError> {
        unimplemented!("fetch is out of scope for the snapshot engine")
    }

    fn reconcile(&self) -> Result<ReconcileOutcome, VcsError> {
        unimplemented!("reconcile is out of scope for the snapshot engine")
    }

    fn push(&self) -> Result<PushOutcome, VcsError> {
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
