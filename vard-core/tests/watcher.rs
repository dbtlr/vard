//! Integration tests for the filesystem watcher against a real filesystem.
//!
//! These exercise the notify bridge end to end in a tempdir, so they depend on
//! real OS event delivery and real timers. They use generous timeouts and a
//! short quiescence window to stay robust in CI rather than fast. The
//! deterministic behavior of the quiescence machine, the handler's filtering,
//! and the trouble seam are covered by the paused-time unit tests in
//! `src/watcher.rs`; these only prove that real events flow through the bridge
//! on both backends and that disarm stops delivery.

use std::fs;
use std::time::Duration;

use tokio::time::timeout;
use vard_core::{ArmMode, WatchSpec, Watcher, WatcherRx, WatcherSignal};

/// A quiescence window short enough to keep tests quick but comfortably longer
/// than event-delivery latency.
const QUIESCE: Duration = Duration::from_millis(400);

/// How long a test will wait for an expected signal before failing. Generous,
/// to absorb FSEvents/inotify latency and CI scheduling jitter.
const WAIT_FOR_SIGNAL: Duration = Duration::from_secs(20);

/// Awaits the next activity signal for `watch`, failing if none arrives in
/// [`WAIT_FOR_SIGNAL`]. Ignores signals for other watches; fails on trouble.
async fn expect_activity(rx: &mut WatcherRx, watch: &str) {
    let deadline = timeout(WAIT_FOR_SIGNAL, async {
        loop {
            match rx.recv().await {
                Some(WatcherSignal::Activity { watch: w, .. }) if w == watch => break,
                Some(WatcherSignal::Trouble { watch: w, detail }) if w == watch => {
                    panic!("watch {w:?} reported trouble: {detail}")
                }
                Some(_) => continue,
                None => panic!("signal channel closed before a signal arrived"),
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

    expect_activity(&mut rx, "native").await;
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

    expect_activity(&mut rx, "polled").await;
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
