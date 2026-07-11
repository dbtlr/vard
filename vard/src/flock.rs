//! The low-level exclusive-`flock` primitive shared by the single-instance lock
//! ([`crate::instance`]) and the per-watch operation lock
//! ([`crate::journal::OpLock`]).
//!
//! Both locks need the identical core — open the file read+write (creating it
//! and its parent directories), take one *non-blocking exclusive* advisory
//! `flock`, and tell a live holder (`WOULDBLOCK`) apart from a genuine I/O error
//! — while differing only in their retry policy and their per-lock bookkeeping
//! (the instance lock's role/holder record, the op lock's guard type). That
//! shared core lives here so the two callers cannot drift on how the lock is
//! taken; everything specific to each lock stays in its own module.
//!
//! `flock` is per-open-file-description, so a fresh open+lock is what serializes
//! even two acquirers in one process, and the kernel releases the lock when the
//! returned [`File`] closes (drop, clean exit, or crash alike) — no unlink
//! needed, and none is done here.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

/// Interval a bounded-retry acquirer sleeps between non-blocking attempts. Short
/// enough to feel responsive, long enough not to spin.
pub(crate) const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Opens `path` read+write (creating it and any parent directories, never
/// truncating) and takes ONE non-blocking exclusive `flock`:
///
/// - `Ok(Some(file))` — the lock is held for as long as `file` lives; closing it
///   releases the `flock`.
/// - `Ok(None)` — `WOULDBLOCK`: a live holder owns the lock right now.
/// - `Err(_)` — any other I/O trouble (opening the file or locking it).
///
/// The file's contents are irrelevant to the lock (the `flock` is the whole
/// mechanism); opening read+write+create matches both callers and leaves room
/// for the diagnostics each writes into its own file.
pub(crate) fn open_and_lock_exclusive(path: &Path) -> io::Result<Option<File>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        // `EWOULDBLOCK` is `EAGAIN` everywhere rustix targets, so one match covers
        // contention: a live holder owns the lock.
        Err(Errno::WOULDBLOCK) => Ok(None),
        Err(errno) => Err(errno.into()),
    }
}

/// [`open_and_lock_exclusive`] retried on [`LOCK_RETRY_INTERVAL`] until `budget`
/// elapses. `Ok(Some(file))` once the lock is ours; `Ok(None)` if a holder never
/// yields in time. For synchronous callers that may block briefly (the CLI op
/// gate); the async engine path uses the non-blocking form directly.
pub(crate) fn acquire_blocking(path: &Path, budget: Duration) -> io::Result<Option<File>> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(file) = open_and_lock_exclusive(path)? {
            return Ok(Some(file));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(LOCK_RETRY_INTERVAL);
    }
}
