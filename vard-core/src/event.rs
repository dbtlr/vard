//! The event bus: the engine's single reporting spine.
//!
//! The engine owns correctness but not presentation. Instead of printing,
//! every subsystem — watcher, scheduler, snapshotter, sync — reports what it
//! did by emitting an [`Event`] onto one [`EventBus`]. All user-visible
//! reactors (the binary's logger, health file, and hooks) subscribe to that
//! same stream. There is no privileged internal channel: an embedder that
//! calls [`EventBus::subscribe`] sees exactly what the daemon sees.
//!
//! # Design tradeoffs
//!
//! The bus wraps [`tokio::sync::broadcast`], a bounded fan-out channel. Two
//! consequences follow, and both are deliberate:
//!
//! - **Emitting never blocks and never fails.** The engine must make progress
//!   regardless of how many subscribers exist or how slow they are. Emitting
//!   with zero subscribers is fine — the event is simply dropped (see
//!   [`EventBus::emit`]).
//! - **Slow subscribers can miss events.** Each subscriber has a buffer of
//!   `capacity` events. A subscriber that falls more than `capacity` behind
//!   observes a lag signal and skips the events it missed, rather than
//!   stalling the engine. Reactors that need every event must keep up.
//!
//! # Example
//!
//! ```
//! use vard_core::{Event, EventBus};
//!
//! # async fn run() {
//! let bus = EventBus::default();
//! let mut rx = bus.subscribe();
//!
//! bus.emit(Event::DaemonStarted);
//!
//! assert_eq!(rx.recv().await.unwrap().name(), "daemon.started");
//! # }
//! ```

use std::fmt;

use tokio::sync::broadcast;

/// The default per-subscriber buffer capacity for [`EventBus::default`].
pub const DEFAULT_CAPACITY: usize = 256;

/// Everything the engine reports about its own activity.
///
/// Each variant carries the payload a reactor needs to log, surface, or run a
/// hook without calling back into the engine. Watch identity always travels as
/// the watch's stable name (a plain [`String`]); VCS references likewise travel
/// as [`String`] values.
///
/// [`Event::name`] maps each variant to a stable dotted catalog string. That
/// mapping is a public contract: it backs hook-configuration keys and log
/// lines, so the strings must not change without a deliberate, breaking bump.
///
/// Events are `Clone` (broadcast delivers a copy to every subscriber) as well
/// as `Debug + Send + Sync + 'static`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A snapshot was written successfully.
    SnapshotCompleted {
        /// Stable name of the watch that was snapshotted.
        watch: String,
        /// VCS reference or id of the snapshot that was created.
        snapshot: String,
        /// Number of files that changed in this snapshot.
        files_changed: usize,
        /// Why the snapshot was taken.
        trigger: Trigger,
    },
    /// A snapshot attempt failed.
    SnapshotFailed {
        /// Stable name of the watch.
        watch: String,
        /// Human-readable description of the failure.
        error: String,
    },
    /// Local snapshots were pushed to the remote.
    SyncPushed {
        /// Stable name of the watch.
        watch: String,
    },
    /// Remote changes were pulled into the local repository.
    SyncPulled {
        /// Stable name of the watch.
        watch: String,
        /// The reference the watch pointed at before the pull.
        prev_ref: String,
        /// The reference the watch points at after the pull.
        new_ref: String,
    },
    /// A pull produced a conflict that needs resolution.
    SyncConflict {
        /// Stable name of the watch.
        watch: String,
    },
    /// A sync conflict was resolved.
    SyncResolved {
        /// Stable name of the watch.
        watch: String,
        /// Who resolved the conflict.
        resolver: Resolver,
    },
    /// A sync operation failed.
    SyncFailed {
        /// Stable name of the watch.
        watch: String,
        /// Human-readable description of the failure.
        error: String,
    },
    /// A restore completed, moving the working tree to a prior snapshot.
    RestoreCompleted {
        /// Stable name of the watch.
        watch: String,
        /// The reference the watch was restored to.
        restored_to: String,
        /// The reference the watch pointed at before the restore.
        prev_ref: String,
    },
    /// A watch moved from one [`WatchState`] to another.
    WatchStateChanged {
        /// Stable name of the watch.
        watch: String,
        /// The state the watch was in before the transition.
        from: WatchState,
        /// The state the watch is in after the transition.
        to: WatchState,
        /// Optional human-readable reason for the transition.
        reason: Option<String>,
    },
    /// The daemon finished starting up.
    DaemonStarted,
    /// The daemon began shutting down.
    DaemonStopped,
    /// A newer release of vard is available.
    UpdateAvailable {
        /// The version string of the available release.
        version: String,
    },
}

impl Event {
    /// Returns the stable dotted catalog name for this event.
    ///
    /// These strings are a public contract (hook-configuration keys, log
    /// lines) and must remain stable. The match is exhaustive with no
    /// wildcard, so adding a variant without a name fails to compile.
    pub fn name(&self) -> &'static str {
        match self {
            Event::SnapshotCompleted { .. } => "snapshot.completed",
            Event::SnapshotFailed { .. } => "snapshot.failed",
            Event::SyncPushed { .. } => "sync.pushed",
            Event::SyncPulled { .. } => "sync.pulled",
            Event::SyncConflict { .. } => "sync.conflict",
            Event::SyncResolved { .. } => "sync.resolved",
            Event::SyncFailed { .. } => "sync.failed",
            Event::RestoreCompleted { .. } => "restore.completed",
            Event::WatchStateChanged { .. } => "watch.state_changed",
            Event::DaemonStarted => "daemon.started",
            Event::DaemonStopped => "daemon.stopped",
            Event::UpdateAvailable { .. } => "update.available",
        }
    }
}

/// Why a snapshot was taken.
///
/// Later tasks extend this vocabulary; keep additions in step with the
/// scheduler and restore/sync flows that produce them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Trigger {
    /// A filesystem change was observed.
    Event,
    /// A periodic interval elapsed.
    Interval,
    /// A user requested the snapshot explicitly.
    Manual,
    /// Taken automatically before a restore, to preserve current state.
    PreRestore,
    /// Taken automatically before a sync, to preserve current state.
    PreSync,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Trigger::Event => "event",
            Trigger::Interval => "interval",
            Trigger::Manual => "manual",
            Trigger::PreRestore => "pre-restore",
            Trigger::PreSync => "pre-sync",
        };
        f.write_str(s)
    }
}

/// The lifecycle state of a watch.
///
/// The reason for any transition travels on
/// [`Event::WatchStateChanged`], never inside the enum, so the state set stays
/// small and comparable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchState {
    /// Watching and snapshotting normally.
    Ok,
    /// Suspended by the user; no snapshots are taken.
    Paused,
    /// A sync conflict is blocking progress until resolved.
    Conflicted,
    /// A sync operation is failing (for example, the remote is unreachable).
    SyncError,
    /// Something needs a human's attention.
    Attention,
}

impl fmt::Display for WatchState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WatchState::Ok => "ok",
            WatchState::Paused => "paused",
            WatchState::Conflicted => "conflicted",
            WatchState::SyncError => "sync-error",
            WatchState::Attention => "attention",
        };
        f.write_str(s)
    }
}

/// Who resolved a sync conflict, reported on [`Event::SyncResolved`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resolver {
    /// A human resolved the conflict.
    Human,
    /// An automated (AI) resolver handled the conflict.
    Ai,
}

impl fmt::Display for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Resolver::Human => "human",
            Resolver::Ai => "ai",
        };
        f.write_str(s)
    }
}

/// The receiving end of an [`EventBus`], returned by [`EventBus::subscribe`].
///
/// This is [`tokio::sync::broadcast::Receiver`]. Call `recv().await` to take
/// the next event. A subscriber that falls more than the bus's `capacity`
/// behind receives [`broadcast::error::RecvError::Lagged`] carrying the number
/// of events it skipped, after which it resumes from the oldest event still
/// buffered — it never blocks the engine.
pub type EventReceiver = broadcast::Receiver<Event>;

/// The engine's reporting spine: a bounded, multi-subscriber broadcast channel.
///
/// Clone the bus freely — every subsystem holds its own handle and emits
/// through it. All clones and all [`subscribe`](EventBus::subscribe)rs share
/// the same underlying channel.
///
/// See the [module documentation](self) for the design and its tradeoffs.
#[derive(Clone, Debug)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    /// Creates a bus where each subscriber buffers up to `capacity` events.
    ///
    /// A subscriber that falls further than `capacity` behind will observe a
    /// lag signal and skip missed events (see [`EventReceiver`]).
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Emits an event to all current subscribers.
    ///
    /// This never blocks and never returns an error. If there are no
    /// subscribers, the event is dropped — that is correct behavior, since the
    /// engine reports unconditionally and reactors are optional. Delivery to a
    /// subscriber that has fallen too far behind is likewise best-effort (see
    /// the [module documentation](self)).
    pub fn emit(&self, event: Event) {
        // A send error means only that no receivers are currently listening,
        // which is expected; the event is intentionally dropped.
        let _ = self.sender.send(event);
    }

    /// Subscribes to the bus, receiving every event emitted from now on.
    ///
    /// Events emitted before this call are not delivered to the returned
    /// receiver.
    pub fn subscribe(&self) -> EventReceiver {
        self.sender.subscribe()
    }

    /// Returns the number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    /// Creates a bus with [`DEFAULT_CAPACITY`] buffering per subscriber.
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    #[tokio::test]
    async fn emit_with_no_subscribers_does_not_panic() {
        let bus = EventBus::default();
        assert_eq!(bus.subscriber_count(), 0);
        // Must not panic or otherwise fail.
        bus.emit(Event::DaemonStarted);
    }

    #[tokio::test]
    async fn two_subscribers_both_receive_events_in_order() {
        let bus = EventBus::default();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        let first = Event::SnapshotCompleted {
            watch: "docs".to_string(),
            snapshot: "abc123".to_string(),
            files_changed: 3,
            trigger: Trigger::Event,
        };
        let second = Event::DaemonStopped;

        bus.emit(first.clone());
        bus.emit(second.clone());

        assert_eq!(a.recv().await.unwrap(), first);
        assert_eq!(a.recv().await.unwrap(), second);
        assert_eq!(b.recv().await.unwrap(), first);
        assert_eq!(b.recv().await.unwrap(), second);
    }

    #[tokio::test]
    async fn late_subscriber_does_not_see_earlier_events() {
        let bus = EventBus::default();
        bus.emit(Event::DaemonStarted);

        let mut late = bus.subscribe();
        // Nothing buffered for a subscriber created after the emit.
        assert_eq!(late.try_recv(), Err(TryRecvError::Empty));

        bus.emit(Event::DaemonStopped);
        assert_eq!(late.recv().await.unwrap(), Event::DaemonStopped);
    }

    #[tokio::test]
    async fn lagged_subscriber_observes_lag_then_continues() {
        // Capacity 2, but emit 4 events before reading: the receiver falls
        // behind and must observe the lag rather than blocking the engine.
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();

        for version in ["1", "2", "3", "4"] {
            bus.emit(Event::UpdateAvailable {
                version: version.to_string(),
            });
        }

        // First recv reports how many events were skipped.
        match rx.recv().await {
            Err(RecvError::Lagged(skipped)) => assert_eq!(skipped, 2),
            other => panic!("expected Lagged(2), got {other:?}"),
        }

        // The receiver continues from the oldest event still buffered.
        assert_eq!(
            rx.recv().await.unwrap(),
            Event::UpdateAvailable {
                version: "3".to_string()
            }
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            Event::UpdateAvailable {
                version: "4".to_string()
            }
        );
    }

    #[test]
    fn name_maps_every_variant_to_its_catalog_string() {
        // Exhaustive, wildcard-free: a new variant forces this list to grow.
        let cases = [
            (
                Event::SnapshotCompleted {
                    watch: "w".to_string(),
                    snapshot: "r".to_string(),
                    files_changed: 0,
                    trigger: Trigger::Manual,
                },
                "snapshot.completed",
            ),
            (
                Event::SnapshotFailed {
                    watch: "w".to_string(),
                    error: "e".to_string(),
                },
                "snapshot.failed",
            ),
            (
                Event::SyncPushed {
                    watch: "w".to_string(),
                },
                "sync.pushed",
            ),
            (
                Event::SyncPulled {
                    watch: "w".to_string(),
                    prev_ref: "a".to_string(),
                    new_ref: "b".to_string(),
                },
                "sync.pulled",
            ),
            (
                Event::SyncConflict {
                    watch: "w".to_string(),
                },
                "sync.conflict",
            ),
            (
                Event::SyncResolved {
                    watch: "w".to_string(),
                    resolver: Resolver::Human,
                },
                "sync.resolved",
            ),
            (
                Event::SyncFailed {
                    watch: "w".to_string(),
                    error: "e".to_string(),
                },
                "sync.failed",
            ),
            (
                Event::RestoreCompleted {
                    watch: "w".to_string(),
                    restored_to: "a".to_string(),
                    prev_ref: "b".to_string(),
                },
                "restore.completed",
            ),
            (
                Event::WatchStateChanged {
                    watch: "w".to_string(),
                    from: WatchState::Ok,
                    to: WatchState::Paused,
                    reason: None,
                },
                "watch.state_changed",
            ),
            (Event::DaemonStarted, "daemon.started"),
            (Event::DaemonStopped, "daemon.stopped"),
            (
                Event::UpdateAvailable {
                    version: "1.0.0".to_string(),
                },
                "update.available",
            ),
        ];

        for (event, expected) in cases {
            // Re-match without a wildcard so an unnamed new variant fails to
            // compile here as well as in `Event::name`.
            let matched = match event {
                Event::SnapshotCompleted { .. }
                | Event::SnapshotFailed { .. }
                | Event::SyncPushed { .. }
                | Event::SyncPulled { .. }
                | Event::SyncConflict { .. }
                | Event::SyncResolved { .. }
                | Event::SyncFailed { .. }
                | Event::RestoreCompleted { .. }
                | Event::WatchStateChanged { .. }
                | Event::DaemonStarted
                | Event::DaemonStopped
                | Event::UpdateAvailable { .. } => event.name(),
            };
            assert_eq!(matched, expected);
        }
    }

    #[test]
    fn vocabulary_display_uses_spec_spellings() {
        assert_eq!(Trigger::Event.to_string(), "event");
        assert_eq!(Trigger::Interval.to_string(), "interval");
        assert_eq!(Trigger::Manual.to_string(), "manual");
        assert_eq!(Trigger::PreRestore.to_string(), "pre-restore");
        assert_eq!(Trigger::PreSync.to_string(), "pre-sync");

        assert_eq!(WatchState::Ok.to_string(), "ok");
        assert_eq!(WatchState::Paused.to_string(), "paused");
        assert_eq!(WatchState::Conflicted.to_string(), "conflicted");
        assert_eq!(WatchState::SyncError.to_string(), "sync-error");
        assert_eq!(WatchState::Attention.to_string(), "attention");

        assert_eq!(Resolver::Human.to_string(), "human");
        assert_eq!(Resolver::Ai.to_string(), "ai");
    }
}
