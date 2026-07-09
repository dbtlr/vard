//! The embeddable engine behind [vard](https://github.com/dbtlr/vard):
//! directory watching, snapshotting into version control, and VCS backends.
//!
//! # The seam
//!
//! This crate owns **correctness**: safe-state checks, snapshot invariants,
//! quiescence, and locking. Hosts own configuration and presentation — the
//! engine takes watch specifications as values and never reads files, and it
//! reports activity through a typed event stream rather than printing.
//!
//! Deliberately outside this crate: CLI, file-config I/O, the health file,
//! hooks, and service management. Those are host (binary) concerns built as
//! subscribers on the event stream.
//!
//! The API is unstable (0.x) until published with semver guarantees.

pub mod config;
pub mod engine;
pub mod event;
pub mod scheduler;
pub mod vcs;
pub mod watcher;

pub use config::{
    ConfigError, DEFAULT_INTERVAL, DEFAULT_QUIESCE, DEFAULT_REMOTE, DEFAULT_SYNC,
    DEFAULT_SYNC_INTERVAL, DEFAULT_TRIGGER, TriggerMode, WatchSpec, WatchSpecBuilder,
    parse_duration,
};
pub use engine::{
    DEFAULT_LOCK_RETRY_ATTEMPTS, DEFAULT_LOCK_RETRY_BASE, DEFAULT_SHUTDOWN_DRAIN_TIMEOUT,
    DEFAULT_UNSAFE_REPOLL_INTERVAL, DEFAULT_UNSAFE_REPOLL_MAX_ATTEMPTS, Engine, EngineBuilder,
    EngineError, EngineHandle, SharedBackend,
};
pub use event::{
    DEFAULT_CAPACITY, Event, EventBus, EventReceiver, RecvError, Resolver, Trigger, TroubleKind,
    TryRecvError, WatchState,
};
pub use scheduler::{ScheduleHandle, Scheduler, SchedulerError, SchedulerRx, SchedulerSignal};
pub use vcs::git::GitBackend;
pub use vcs::{
    ChangeSummary, CommitMessage, LogFilter, PushOutcome, ReconcileOutcome, RemoteState,
    RestoreTarget, SafeState, Snapshot, SnapshotId, SnapshotOutcome, SnapshotRequest, UnsafeReason,
    VcsBackend, VcsError, VcsRef,
};
pub use watcher::{
    ArmMode, DEFAULT_POLL_INTERVAL, MuteGuard, WatchHandle, Watcher, WatcherError, WatcherRx,
    WatcherSignal,
};
