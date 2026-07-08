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

pub mod event;

pub use event::{Event, EventBus, EventReceiver, Resolver, Trigger, WatchState};
