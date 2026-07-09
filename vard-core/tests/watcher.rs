//! Integration tests for the filesystem watcher against a real filesystem.
//!
//! These exercise the notify bridge end to end in a tempdir, so they depend on
//! real OS event delivery and real timers. They use generous timeouts and a
//! short quiescence window to stay robust in CI rather than fast. The
//! deterministic behavior of the quiescence machine itself (exact windows,
//! coalescing, muting, filtering) is covered by the paused-time unit tests in
//! `src/watcher.rs`; these only prove that real events flow through the bridge
//! and that filtering and forced polling work against a live filesystem.

use std::fs;
use std::time::Duration;

use tokio::time::timeout;
use vard_core::{ArmMode, WatchSpec, Watcher, WatcherRx};

/// A quiescence window short enough to keep tests quick but comfortably longer
/// than event-delivery latency.
const QUIESCE: Duration = Duration::from_millis(400);

/// How long a test will wait for an expected signal before failing. Generous,
/// to absorb FSEvents/inotify latency and CI scheduling jitter.
const WAIT_FOR_SIGNAL: Duration = Duration::from_secs(20);

/// Awaits the next activity signal for `watch`, failing if none arrives in
/// [`WAIT_FOR_SIGNAL`]. Ignores signals for other watches.
async fn expect_signal(rx: &mut WatcherRx, watch: &str) {
    let deadline = timeout(WAIT_FOR_SIGNAL, async {
        loop {
            match rx.recv().await {
                Some(activity) if activity.watch == watch => break,
                Some(_) => continue,
                None => panic!("activity channel closed before a signal arrived"),
            }
        }
    });
    deadline
        .await
        .unwrap_or_else(|_| panic!("no signal for watch {watch:?} within {WAIT_FOR_SIGNAL:?}"));
}

#[tokio::test]
async fn native_watch_signals_on_real_write() {
    let dir = tempfile::tempdir().unwrap();
    let spec = WatchSpec::builder("native", dir.path())
        .quiesce(QUIESCE)
        .build()
        .unwrap();

    let (watcher, mut rx) = Watcher::new();
    let handle = watcher.arm(&spec).expect("native arm should succeed");
    assert_eq!(
        handle.arm_mode(),
        ArmMode::Native,
        "a plain tempdir should arm natively"
    );

    fs::write(dir.path().join("hello.txt"), b"hi").unwrap();

    expect_signal(&mut rx, "native").await;
}

#[tokio::test]
async fn writes_under_git_dir_produce_no_signal() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    fs::create_dir_all(&git_dir).unwrap();

    // Force polling: PollWatcher reports the exact changed path, which makes the
    // `.git/`-drop deterministic (native backends may coalesce to the parent
    // directory). The point under test is the filter, not the backend.
    let spec = WatchSpec::builder("gitwatch", dir.path())
        .quiesce(QUIESCE)
        .poll_interval(Duration::from_millis(150))
        .build()
        .unwrap();

    let (watcher, mut rx) = Watcher::new();
    let _handle = watcher.arm(&spec).expect("poll arm should succeed");

    // Only churn inside .git/. If the filter works, no signal ever fires.
    for i in 0..5 {
        fs::write(git_dir.join(format!("obj{i}")), b"x").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait well past a full window's worth of poll cycles; expect silence.
    let got = timeout(Duration::from_secs(3), rx.recv()).await;
    assert!(
        got.is_err(),
        "expected no signal from .git/ churn, got {got:?}"
    );
}

#[tokio::test]
async fn forced_polling_mode_signals_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let spec = WatchSpec::builder("polled", dir.path())
        .quiesce(QUIESCE)
        .poll_interval(Duration::from_millis(150))
        .build()
        .unwrap();

    let (watcher, mut rx) = Watcher::new();
    let handle = watcher.arm(&spec).expect("poll arm should succeed");
    assert_eq!(
        handle.arm_mode(),
        ArmMode::Polling {
            period: Duration::from_millis(150)
        },
        "an explicit poll_interval forces polling"
    );

    fs::write(dir.path().join("data.bin"), b"payload").unwrap();

    expect_signal(&mut rx, "polled").await;
}

#[tokio::test]
async fn disarm_by_drop_stops_signals() {
    let dir = tempfile::tempdir().unwrap();
    let spec = WatchSpec::builder("dropped", dir.path())
        .quiesce(QUIESCE)
        .poll_interval(Duration::from_millis(150))
        .build()
        .unwrap();

    let (watcher, mut rx) = Watcher::new();
    let handle = watcher.arm(&spec).expect("poll arm should succeed");

    // Disarm before any write. No backend remains to observe the change.
    handle.disarm();

    fs::write(dir.path().join("late.txt"), b"nope").unwrap();

    let got = timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(got.is_err(), "no signal after disarm, got {got:?}");
}
