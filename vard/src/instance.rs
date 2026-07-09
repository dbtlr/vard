//! Single-instance enforcement for the daemon via an exclusive advisory
//! `flock` on [`paths::lock_file`].
//!
//! Exactly one `vard` daemon may own a given state directory at a time.
//! Acquisition takes a non-blocking exclusive `flock`: the first daemon wins,
//! a second startup fails fast with the holder's PID for a clear diagnostic.
//!
//! # Why `flock`, not a PID file
//!
//! `flock` is tied to the open file description, so the kernel releases it
//! automatically when the holding process exits — crash, `kill -9`, or clean
//! shutdown alike. A daemon that dies without running `Drop` therefore leaves
//! no stale lock behind, which a bare PID file cannot promise. The PID we
//! write into the file is diagnostics only (so contention can name the
//! holder); the lock itself is what enforces exclusion.
//!
//! The lock file is deliberately left on disk when the guard drops: `flock`
//! needs no unlink to release, and removing the file would race a concurrent
//! acquirer that has already opened it.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

use crate::paths;

/// A held single-instance lock. The exclusive `flock` lives as long as this
/// guard: dropping it closes the underlying descriptor, which releases the
/// lock. Hold it for the daemon's whole lifetime.
#[derive(Debug)]
pub(crate) struct InstanceLock {
    /// The locked file. Kept open purely to keep the `flock` held; closing the
    /// descriptor (on drop) is what releases the lock.
    file: File,
    /// The lock file's path, retained for diagnostics.
    path: PathBuf,
}

impl InstanceLock {
    /// Acquires the daemon's single-instance lock at the default location,
    /// `$XDG_STATE_HOME/vard/vard.lock`.
    pub(crate) fn acquire() -> Result<InstanceLock, LockError> {
        let path = paths::lock_file().map_err(|e| LockError::Path(e.to_string()))?;
        Self::acquire_at(&path)
    }

    /// [`acquire`](Self::acquire) against an explicit path, so tests inject a
    /// tempdir instead of the real XDG state directory. Creates parent
    /// directories as needed.
    pub(crate) fn acquire_at(path: &Path) -> Result<InstanceLock, LockError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }

        // Open read+write (not truncating): on contention we still want to read
        // the incumbent's PID out of the file.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| LockError::Io {
                path: path.to_path_buf(),
                source,
            })?;

        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            // `EWOULDBLOCK` is `EAGAIN` on every platform rustix targets, so
            // matching one covers contention.
            Err(Errno::WOULDBLOCK) => {
                return Err(LockError::Held {
                    path: path.to_path_buf(),
                    holder: read_pid(path),
                });
            }
            Err(errno) => {
                return Err(LockError::Io {
                    path: path.to_path_buf(),
                    source: errno.into(),
                });
            }
        }

        // Lock is ours: replace any stale contents with our own PID.
        write_pid(&mut file, std::process::id()).map_err(|source| LockError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(InstanceLock {
            file,
            path: path.to_path_buf(),
        })
    }

    /// The lock file's path, for diagnostics.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the `flock`; the kernel does this for
        // us as `self.file` drops. Referencing the field here both documents
        // the release contract and keeps the field live. The lock file is left
        // on disk deliberately (see module docs).
        let _ = &self.file;
    }
}

/// Truncates `file` and writes `pid` followed by a newline, leaving the file
/// positioned at the end.
fn write_pid(file: &mut File, pid: u32) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{pid}")?;
    file.flush()
}

/// Best-effort read of the PID recorded in the lock file at `path`. Returns
/// `None` when the file is missing, empty, or does not hold a bare integer —
/// the caller must render a sensible message regardless.
fn read_pid(path: &Path) -> Option<u32> {
    let mut buf = String::new();
    File::open(path).ok()?.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

/// Why acquiring the single-instance lock failed.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum LockError {
    /// The lock file path could not be resolved (see [`paths`]).
    Path(String),
    /// Creating the parent directory, opening the lock file, or taking the
    /// lock failed for a reason other than contention.
    Io {
        /// The lock file path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Another `vard` instance already holds the lock. `holder` is the PID read
    /// from the file, best-effort — it may be absent if the file is empty or
    /// unreadable.
    Held {
        /// The lock file path.
        path: PathBuf,
        /// The holding process's PID, if it could be read.
        holder: Option<u32>,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Path(msg) => write!(f, "resolving lock path: {msg}"),
            LockError::Io { path, source } => {
                write!(f, "opening lock file {}: {source}", path.display())
            }
            LockError::Held {
                path,
                holder: Some(pid),
            } => write!(
                f,
                "another vard instance is already running (PID {pid}; lock {})",
                path.display()
            ),
            LockError::Held { path, holder: None } => write!(
                f,
                "another vard instance is already running (holder PID unknown; lock {})",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("vard.lock");
        (dir, path)
    }

    #[test]
    fn acquire_creates_parents_and_writes_pid() {
        let (_dir, path) = temp_lock_path();
        let lock = InstanceLock::acquire_at(&path).unwrap();
        assert_eq!(lock.path(), path);
        // The PID is written for diagnostics.
        assert_eq!(read_pid(&path), Some(std::process::id()));
    }

    #[test]
    fn second_acquire_contends_and_reports_holder_pid() {
        // `flock` locks are per open file description, so a second open+flock
        // of the same path — even in this same process — contends.
        let (_dir, path) = temp_lock_path();
        let _held = InstanceLock::acquire_at(&path).unwrap();
        match InstanceLock::acquire_at(&path) {
            Err(LockError::Held { holder, .. }) => {
                assert_eq!(holder, Some(std::process::id()));
            }
            other => panic!("expected Held contention error, got {other:?}"),
        }
    }

    #[test]
    fn releasing_the_lock_allows_reacquire() {
        let (_dir, path) = temp_lock_path();
        {
            let _held = InstanceLock::acquire_at(&path).unwrap();
        } // guard dropped here, releasing the flock

        // A fresh acquire now succeeds. The retry tolerates a test-only race:
        // a sibling test's `Command::spawn` forks this process, and between the
        // fork and the child's exec (which closes O_CLOEXEC fds) the child
        // transiently shares our still-open lock fd, holding the flock a
        // microsecond longer. Production never reacquires its own lock, so this
        // window is a pure test artifact. Retry briefly rather than flake.
        let mut last = None;
        for _ in 0..100 {
            match InstanceLock::acquire_at(&path) {
                Ok(_reacquired) => return,
                Err(e) => last = Some(e),
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("lock never became reacquirable after release: {last:?}");
    }

    #[test]
    fn held_error_without_pid_still_renders_one_line() {
        let err = LockError::Held {
            path: PathBuf::from("/state/vard.lock"),
            holder: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("holder PID unknown"), "got: {msg}");
        assert!(!msg.contains('\n'), "message must be one line: {msg}");
    }
}
