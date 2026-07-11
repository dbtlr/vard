//! Single-instance enforcement via an exclusive advisory `flock` on
//! [`paths::lock_file`], plus the role discriminator that lets a CLI command
//! tell a daemon holder from a peer CLI holder.
//!
//! # The lock proves *someone* holds it — the role says *who*
//!
//! The `flock` alone proves only that some `vard` process holds the lock, not
//! that it is the daemon. That distinction matters: a CLI `snapshot` may only
//! hand its work to a *daemon* (which owns the repositories); a second
//! concurrent CLI holding the lock in-process must be waited on, not handed a
//! request it will never drain. So the lock file records a **role** beside the
//! PID — `"daemon"` or `"cli"` — and every acquirer writes its own. A holder
//! whose role is unreadable is treated as a peer CLI, never as a daemon, so a
//! command never falsely reports "handed to the daemon".
//!
//! # Why `flock`, not a PID file
//!
//! `flock` is tied to the open file description, so the kernel releases it
//! automatically when the holding process exits — crash, `kill -9`, or clean
//! shutdown alike. A daemon that dies without running `Drop` therefore leaves
//! no stale lock behind, which a bare PID file cannot promise. The PID and role
//! written into the file are diagnostics and dispatch only; the lock itself is
//! what enforces exclusion.
//!
//! The lock file is deliberately left on disk when the guard drops: `flock`
//! needs no unlink to release, and removing the file would race a concurrent
//! acquirer that has already opened it.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

use crate::flock::open_and_lock_exclusive;
use crate::paths;

/// How long a CLI/daemon acquirer sleeps between retries while a peer holds the
/// lock. Short enough to feel responsive, long enough not to spin.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// How many times [`acquire_at`](InstanceLock::acquire_at) re-attempts the
/// exclusive `flock` on `WOULDBLOCK` before it trusts the contention as a
/// genuine holder and reads the role.
///
/// A `vard notify` probe holds the *shared* lock for only microseconds, but an
/// acquirer's *exclusive* request `WOULDBLOCK`s against it for that instant. If
/// the acquirer read the role immediately it could misread a crashed daemon's
/// leftover `role = daemon` (or an empty mid-write file) as a live holder and
/// wrongly refuse. Retrying briefly lets a transient probe clear: persistent
/// `WOULDBLOCK` across every retry then means a *genuine* exclusive holder,
/// whose role content — written under its own lock — is trustworthy.
/// Four fast attempts (~24 ms total) — a probe's shared hold lasts
/// microseconds (plus scheduler noise), so a couple dozen milliseconds of
/// patience rides it out, while the common genuinely-held case (a running
/// daemon) pays an imperceptible pause instead of a tenth of a second on
/// every CLI invocation.
const PROBE_CONTENTION_RETRIES: u32 = 4;

/// The pause between [`PROBE_CONTENTION_RETRIES`] attempts (~24 ms total).
const PROBE_CONTENTION_INTERVAL: Duration = Duration::from_millis(6);

/// The role recorded in the lock file, so a contending CLI can tell a daemon
/// holder (route work to it) from a peer CLI (wait for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockRole {
    /// The long-lived `vard run` daemon.
    Daemon,
    /// A transient CLI command holding the lock for one in-process operation.
    Cli,
}

impl LockRole {
    /// The token written into the lock file.
    fn as_str(self) -> &'static str {
        match self {
            LockRole::Daemon => "daemon",
            LockRole::Cli => "cli",
        }
    }

    /// Parses a role token, `None` for anything unrecognized (an old or
    /// corrupt lock file), which callers treat conservatively as a peer CLI.
    fn parse(s: &str) -> Option<LockRole> {
        match s.trim() {
            "daemon" => Some(LockRole::Daemon),
            "cli" => Some(LockRole::Cli),
            _ => None,
        }
    }
}

impl fmt::Display for LockRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of a CLI's attempt to acquire the instance lock for an
/// in-process operation (see [`InstanceLock::acquire_for_cli`]).
pub(crate) enum CliLock {
    /// The lock is ours; perform the operation in-process while holding it.
    Acquired(InstanceLock),
    /// A daemon owns the repositories; route the work to it as a request.
    DaemonHeld,
    /// A peer CLI held the lock for the whole retry budget; the caller reports
    /// an honest "another vard command is running; retry" rather than proceeding.
    BusyPeerCli,
}

/// The result of probing whether a daemon currently owns the instance lock —
/// the microsecond, side-effect-free check `vard notify` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonProbe {
    /// A holder with `role = daemon` owns the lock: a daemon is running.
    Running,
    /// Nobody holds the lock, or a holder with a non-daemon (CLI or unreadable)
    /// role does — either way no daemon is supervising this state directory.
    NotRunning,
}

/// Non-blocking, read-only probe of whether a daemon owns the instance lock at
/// `path`. Unlike [`acquire_for_cli`](InstanceLock::acquire_for_cli) this never
/// writes to or creates the lock file and never retries: it opens the file
/// read-only, attempts one non-blocking *shared* `flock`, and reports the
/// outcome in microseconds —
///
/// - the file is missing ⇒ no daemon ever ran ⇒ [`DaemonProbe::NotRunning`];
/// - the shared lock is granted (nobody holds it exclusively — we take it, then
///   immediately release it on return) ⇒ [`DaemonProbe::NotRunning`];
/// - an exclusive holder blocks the shared request and the recorded role is
///   `daemon` ⇒ [`DaemonProbe::Running`];
/// - an exclusive holder blocks it and the role is `cli` ⇒
///   [`DaemonProbe::NotRunning`] (a CLI holding the lock truthfully means no
///   daemon);
/// - an exclusive holder blocks it but the role read is torn or empty (a holder
///   caught mid-write) ⇒ [`DaemonProbe::Running`], conservatively — a live
///   exclusive holder exists, and notify's Running+no-health path is an honest
///   "starting or stopping" line, never a silent healthy read.
///
/// The lock is taken *shared*, not exclusive, precisely so two concurrent
/// probes never contend with each other: an exclusive probe would `WOULDBLOCK`
/// against a peer probe and then misread a crashed daemon's leftover
/// `role = daemon` as a live daemon (a false `Running`, in the worst case a
/// silent healthy read of a stale file). Shared probes coexist, yet a shared
/// request still `WOULDBLOCK`s against the daemon's (or a CLI's) *exclusive*
/// hold — which is the only thing this needs to detect.
///
/// Any other I/O error (a permission problem opening the file, say) is returned
/// so the caller can surface an honest operational error rather than guess.
pub(crate) fn probe_daemon(path: &Path) -> Result<DaemonProbe, LockError> {
    // Read-only, no create: a probe must never bring the lock file into being.
    let file = match File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DaemonProbe::NotRunning);
        }
        Err(source) => {
            return Err(LockError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    match flock(&file, FlockOperation::NonBlockingLockShared) {
        // The shared lock was granted, so no one holds it exclusively: no
        // daemon. Dropping `file` on return releases it; we never wrote to it.
        Ok(()) => Ok(DaemonProbe::NotRunning),
        Err(Errno::WOULDBLOCK) => {
            let (_holder, role) = read_holder(path);
            match role {
                Some(LockRole::Daemon) => Ok(DaemonProbe::Running),
                // A CLI holding the lock truthfully means no daemon.
                Some(LockRole::Cli) => Ok(DaemonProbe::NotRunning),
                // A live exclusive holder exists but its role read was torn or
                // empty (a holder caught mid-write, say). Be conservative:
                // report Running rather than falsely tell notify the daemon is
                // absent. notify's Running+missing/unparseable-health path is an
                // honest "starting or stopping" line (exit 1), never a silent
                // healthy read, so a rare false Running degrades safely.
                None => Ok(DaemonProbe::Running),
            }
        }
        Err(errno) => Err(LockError::Io {
            path: path.to_path_buf(),
            source: errno.into(),
        }),
    }
}

/// A held single-instance lock. The exclusive `flock` lives as long as this
/// guard: dropping it closes the underlying descriptor, which releases the
/// lock. Hold it for the daemon's whole lifetime.
#[derive(Debug)]
pub(crate) struct InstanceLock {
    /// The locked file. Kept open purely to keep the `flock` held; closing the
    /// descriptor (on drop) is what releases the lock.
    file: File,
    /// The lock file's path, retained for diagnostics (see [`path`](Self::path)).
    #[allow(dead_code)]
    path: PathBuf,
}

impl InstanceLock {
    /// Acquires the single-instance lock at the default location,
    /// `$XDG_STATE_HOME/vard/vard.lock`, recording `role`.
    // The daemon resolves paths through `DaemonPaths` and calls `acquire_at`;
    // this XDG convenience is kept for future callers (e.g. `vard status`).
    #[allow(dead_code)]
    pub(crate) fn acquire(role: LockRole) -> Result<InstanceLock, LockError> {
        let path = paths::lock_file().map_err(|e| LockError::Path(e.to_string()))?;
        Self::acquire_at(&path, role)
    }

    /// [`acquire`](Self::acquire) against an explicit path, so tests inject a
    /// tempdir instead of the real XDG state directory. Records `role` in the
    /// lock file and creates parent directories as needed.
    pub(crate) fn acquire_at(path: &Path, role: LockRole) -> Result<InstanceLock, LockError> {
        // Retry a `WOULDBLOCK` a handful of times before trusting it: a notify
        // probe holds the shared lock for microseconds, so a transient block
        // clears within a couple of retries, while a genuine exclusive holder
        // blocks every attempt — and only then is its recorded role trustworthy
        // (see [`PROBE_CONTENTION_RETRIES`]). The open+exclusive-flock core is the
        // shared [`crate::flock`] primitive; the probe-contention retry and the
        // holder-record read/write stay here (the instance lock's own machinery).
        let mut attempt = 0;
        let mut file = loop {
            match open_and_lock_exclusive(path).map_err(|source| LockError::Io {
                path: path.to_path_buf(),
                source,
            })? {
                Some(file) => break file,
                None => {
                    if attempt < PROBE_CONTENTION_RETRIES {
                        attempt += 1;
                        std::thread::sleep(PROBE_CONTENTION_INTERVAL);
                        continue;
                    }
                    let (holder, role) = read_holder(path);
                    return Err(LockError::Held {
                        path: path.to_path_buf(),
                        holder,
                        role,
                    });
                }
            }
        };

        // Lock is ours: replace any stale contents with our own PID and role.
        write_holder(&mut file, std::process::id(), role).map_err(|source| LockError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(InstanceLock {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Acquires the lock for a CLI in-process operation, retrying a *peer CLI*
    /// holder for up to `budget` before giving up. A *daemon* holder
    /// short-circuits to [`CliLock::DaemonHeld`] (route the work to it); a
    /// holder whose role is unreadable is treated as a peer CLI — never a
    /// daemon — so a command never falsely claims it handed work to a daemon.
    pub(crate) fn acquire_for_cli(path: &Path, budget: Duration) -> Result<CliLock, LockError> {
        let deadline = Instant::now() + budget;
        loop {
            match Self::acquire_at(path, LockRole::Cli) {
                Ok(lock) => return Ok(CliLock::Acquired(lock)),
                Err(LockError::Held {
                    role: Some(LockRole::Daemon),
                    ..
                }) => return Ok(CliLock::DaemonHeld),
                // A peer CLI (or an unreadable role): wait its turn, up to budget.
                Err(LockError::Held { .. }) => {
                    if Instant::now() >= deadline {
                        return Ok(CliLock::BusyPeerCli);
                    }
                    std::thread::sleep(RETRY_INTERVAL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Acquires the lock for the daemon. A duplicate *daemon* holder fails
    /// immediately (no point waiting on a peer that will not exit); a transient
    /// *CLI* holder is retried for up to `cli_wait` before failing, so a daemon
    /// starting a beat after a CLI `snapshot` took the lock in-process does not
    /// spuriously refuse to start. The returned [`LockError::Held`] carries the
    /// holder's role so the caller can word the message correctly.
    pub(crate) fn acquire_for_daemon(
        path: &Path,
        cli_wait: Duration,
    ) -> Result<InstanceLock, LockError> {
        let deadline = Instant::now() + cli_wait;
        loop {
            match Self::acquire_at(path, LockRole::Daemon) {
                Ok(lock) => return Ok(lock),
                // Another daemon: do not wait, it is not going anywhere.
                err @ Err(LockError::Held {
                    role: Some(LockRole::Daemon),
                    ..
                }) => return err,
                // A transient CLI (or unreadable role): retry briefly.
                Err(e @ LockError::Held { .. }) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                    std::thread::sleep(RETRY_INTERVAL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The lock file's path, for diagnostics.
    // Exercised by tests; a diagnostic accessor for future status reporting.
    #[allow(dead_code)]
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

/// Truncates `file` and writes the holder record — the PID on the first line
/// and the role on the second — in a *single* `write_all`, so a concurrent
/// probe reads either the whole record or (mid-write) an empty/partial one,
/// never a half-written role masquerading as a different one. Readers tolerate
/// a partial or empty read (see [`read_holder`]).
fn write_holder(file: &mut File, pid: u32, role: LockRole) -> std::io::Result<()> {
    let content = format!("{pid}\n{role}\n");
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(content.as_bytes())?;
    file.flush()
}

/// Best-effort read of the PID and role recorded in the lock file at `path`:
/// the first line is the PID, the second the role. Either component is `None`
/// when the file is missing, empty, or malformed — the caller renders a
/// sensible message regardless, and an absent role is treated as a peer CLI.
fn read_holder(path: &Path) -> (Option<u32>, Option<LockRole>) {
    let mut buf = String::new();
    if File::open(path)
        .and_then(|mut f| f.read_to_string(&mut buf))
        .is_err()
    {
        return (None, None);
    }
    let mut lines = buf.lines();
    let pid = lines.next().and_then(|l| l.trim().parse().ok());
    let role = lines.next().and_then(LockRole::parse);
    (pid, role)
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
    /// unreadable. `role` is the holder's recorded role (daemon or CLI), absent
    /// when unreadable — in which case callers assume a peer CLI.
    Held {
        /// The lock file path.
        path: PathBuf,
        /// The holding process's PID, if it could be read.
        holder: Option<u32>,
        /// The holding process's role, if it could be read.
        role: Option<LockRole>,
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
                ..
            } => write!(
                f,
                "another vard instance is already running (PID {pid}; lock {})",
                path.display()
            ),
            LockError::Held {
                path, holder: None, ..
            } => write!(
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
    use crate::journal::test_support::retry_until;

    fn temp_lock_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("vard.lock");
        (dir, path)
    }

    #[test]
    fn acquire_creates_parents_and_writes_pid_and_role() {
        let (_dir, path) = temp_lock_path();
        let lock = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        assert_eq!(lock.path(), path);
        // Both the PID and the role are written for diagnostics and dispatch.
        assert_eq!(
            read_holder(&path),
            (Some(std::process::id()), Some(LockRole::Daemon))
        );
    }

    #[test]
    fn second_acquire_contends_and_reports_holder_pid_and_role() {
        // `flock` locks are per open file description, so a second open+flock
        // of the same path — even in this same process — contends.
        let (_dir, path) = temp_lock_path();
        let _held = InstanceLock::acquire_at(&path, LockRole::Cli).unwrap();
        match InstanceLock::acquire_at(&path, LockRole::Daemon) {
            Err(LockError::Held { holder, role, .. }) => {
                assert_eq!(holder, Some(std::process::id()));
                assert_eq!(
                    role,
                    Some(LockRole::Cli),
                    "the incumbent's role is read back"
                );
            }
            other => panic!("expected Held contention error, got {other:?}"),
        }
    }

    #[test]
    fn acquire_for_cli_sees_a_daemon_holder_as_daemon_held() {
        let (_dir, path) = temp_lock_path();
        let _daemon = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        match InstanceLock::acquire_for_cli(&path, Duration::from_millis(200)).unwrap() {
            CliLock::DaemonHeld => {}
            _ => panic!("a daemon holder must be reported as DaemonHeld"),
        }
    }

    #[test]
    fn acquire_for_cli_times_out_against_a_peer_cli() {
        let (_dir, path) = temp_lock_path();
        let _peer = InstanceLock::acquire_at(&path, LockRole::Cli).unwrap();
        // A peer CLI never yields, so the bounded retry reports busy — never a
        // false DaemonHeld and never an orphaned request.
        match InstanceLock::acquire_for_cli(&path, Duration::from_millis(150)).unwrap() {
            CliLock::BusyPeerCli => {}
            _ => panic!("a peer CLI holder past the budget must be BusyPeerCli"),
        }
    }

    #[test]
    fn acquire_for_daemon_rejects_a_duplicate_daemon_immediately() {
        let (_dir, path) = temp_lock_path();
        let _daemon = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        match InstanceLock::acquire_for_daemon(&path, Duration::from_secs(5)) {
            Err(LockError::Held {
                role: Some(LockRole::Daemon),
                ..
            }) => {}
            other => panic!("a duplicate daemon must fail as a daemon holder, got {other:?}"),
        }
    }

    #[test]
    fn releasing_the_lock_allows_reacquire() {
        let (_dir, path) = temp_lock_path();
        {
            let _held = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        } // guard dropped here, releasing the flock

        // A fresh acquire now succeeds. The retry tolerates a test-only race:
        // a sibling test's `Command::spawn` forks this process, and between the
        // fork and the child's exec (which closes O_CLOEXEC fds) the child
        // transiently shares our still-open lock fd, holding the flock a
        // microsecond longer. Production never reacquires its own lock, so this
        // window is a pure test artifact. Retry briefly rather than flake.
        assert!(
            retry_until(|| InstanceLock::acquire_at(&path, LockRole::Daemon).is_ok()),
            "lock never became reacquirable after release"
        );
    }

    #[test]
    fn probe_reports_not_running_when_the_lock_file_is_absent() {
        let (_dir, path) = temp_lock_path();
        // No file created and no directory even; a missing lock means no daemon.
        assert_eq!(probe_daemon(&path).unwrap(), DaemonProbe::NotRunning);
        // And the probe must not have created the file.
        assert!(!path.exists(), "probe must not create the lock file");
    }

    #[test]
    fn probe_reports_running_only_for_a_daemon_holder() {
        let (_dir, path) = temp_lock_path();
        let held = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        assert_eq!(probe_daemon(&path).unwrap(), DaemonProbe::Running);
        drop(held);
    }

    #[test]
    fn probe_reports_not_running_for_a_cli_holder() {
        // A CLI holding the lock truthfully means no daemon is supervising.
        let (_dir, path) = temp_lock_path();
        let held = InstanceLock::acquire_at(&path, LockRole::Cli).unwrap();
        assert_eq!(probe_daemon(&path).unwrap(), DaemonProbe::NotRunning);
        drop(held);
    }

    #[test]
    fn probe_reports_running_for_an_exclusive_holder_with_a_torn_role() {
        // A live exclusive holder whose role bytes are momentarily empty (a
        // mid-write window) must be reported Running, not falsely absent: notify
        // renders that as a "starting or stopping" line, never silent-healthy.
        let (_dir, path) = temp_lock_path();
        let held = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        // Simulate the mid-write window: the record is momentarily empty while
        // the exclusive lock is still held.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert_eq!(probe_daemon(&path).unwrap(), DaemonProbe::Running);
        drop(held);
    }

    #[test]
    fn acquire_retries_past_a_transient_shared_probe_then_succeeds() {
        // A notify probe holds the shared lock briefly. A daemon acquiring
        // exclusively must retry past it and succeed once the probe releases,
        // rather than reading a (possibly stale) role and refusing.
        let (_dir, path) = temp_lock_path();
        {
            let _seed = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        } // released, but leaves "pid\ndaemon" on disk (a crashed-daemon leftover)

        // A probe grabs the shared lock, signals that it holds it, then releases
        // it after a short delay — a realistic probe hold (microseconds plus
        // scheduler noise), well inside the acquirer's total probe-retry budget.
        // The ready signal makes the ordering deterministic: the acquirer
        // only starts once the shared hold is in place.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let probe_path = path.clone();
        let probe = std::thread::spawn(move || {
            let f = File::open(&probe_path).unwrap();
            // Retry the acquire to ride out the sibling-fork/exec window that can
            // transiently hold the just-released exclusive seed fd (the same
            // artifact `concurrent_probes_do_not_misread_a_crashed_daemon_as_running`'s
            // peer and the other flock tests document).
            assert!(
                retry_until(|| flock(&f, FlockOperation::NonBlockingLockShared).is_ok()),
                "the probe's shared lock must ultimately acquire once the fork/exec race clears"
            );
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            drop(f);
        });
        ready_rx.recv().unwrap();

        // The acquirer must not read the stale role=daemon and refuse; it retries
        // past the transient shared hold and takes the lock.
        let lock = InstanceLock::acquire_at(&path, LockRole::Cli)
            .expect("acquire must retry past a transient probe, not refuse");
        assert_eq!(
            read_holder(&path),
            (Some(std::process::id()), Some(LockRole::Cli)),
            "the acquirer's own role overwrote the stale one"
        );
        drop(lock);
        probe.join().unwrap();
    }

    #[test]
    fn concurrent_probes_do_not_misread_a_crashed_daemon_as_running() {
        // A crashed daemon (not a clean shutdown) leaves the lock file on disk
        // still recording role=daemon, but with no live flock holder.
        let (_dir, path) = temp_lock_path();
        {
            let _lock = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        } // dropped: the flock is released, but "pid\ndaemon" remains in the file.

        // A peer `vard notify` probe is mid-flight, holding its *shared* lock on
        // the same file (exactly what probe_daemon takes). Retry the acquire to
        // ride out the sibling-fork/exec window that can transiently hold the
        // just-released exclusive fd (the same artifact the flock tests document).
        let peer = File::open(&path).unwrap();
        assert!(
            retry_until(|| flock(&peer, FlockOperation::NonBlockingLockShared).is_ok()),
            "the peer shared probe must ultimately acquire once the fork/exec race clears"
        );

        // A second concurrent probe must still report NotRunning: shared locks
        // coexist, so it never falls into the stale-role read that an exclusive
        // probe would (which would misreport the dead daemon as Running).
        assert_eq!(probe_daemon(&path).unwrap(), DaemonProbe::NotRunning);
        drop(peer);
    }

    #[test]
    fn probe_reports_not_running_when_the_lock_is_free() {
        // A leftover lock file from a crashed daemon (no live holder) probes as
        // not-running, and the probe leaves the file releasable for the next
        // acquirer.
        let (_dir, path) = temp_lock_path();
        {
            let _held = InstanceLock::acquire_at(&path, LockRole::Daemon).unwrap();
        } // released, file remains on disk

        // Retry briefly: a sibling test's `Command::spawn` forks this process, and
        // between the fork and the child's exec the child transiently shares our
        // just-released lock fd, so the probe's non-blocking shared flock can
        // momentarily WOULDBLOCK and read the leftover role. This window is a pure
        // test artifact (the same race `releasing_the_lock_allows_reacquire` and
        // `acquire_retries_past_a_transient_shared_probe_then_succeeds` ride out);
        // production `probe_daemon` is correct.
        assert!(
            retry_until(|| probe_daemon(&path).unwrap() == DaemonProbe::NotRunning),
            "a free lock must ultimately probe as NotRunning"
        );
    }

    #[test]
    fn held_error_without_pid_still_renders_one_line() {
        let err = LockError::Held {
            path: PathBuf::from("/state/vard.lock"),
            holder: None,
            role: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("holder PID unknown"), "got: {msg}");
        assert!(!msg.contains('\n'), "message must be one line: {msg}");
    }
}
