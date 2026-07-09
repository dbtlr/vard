//! Per-watch operation journal and stale-lock recovery (spec §16).
//!
//! # Purpose
//!
//! A daemon that dies mid-operation can leave a git index lock
//! (`<repo>/.git/index.lock`) behind, which would block every later git
//! command on that repo. The journal lets the next startup tell *its own*
//! abandoned lock — provably ours, from an operation we recorded — apart from a
//! foreign lock some other git process is legitimately holding.
//!
//! # What recovery does — and does not — do
//!
//! Recovery does **not** replay or complete operations. The engine owns a
//! bounded self-driving retry contract, so re-snapshotting anything still
//! pending is its job, not ours. Recovery's only mandate is to clean a
//! *provably stale* git lock so the engine's next pass is not wedged. A lock is
//! removed only when all of these hold:
//!
//! - the journal has a dangling `begin` record for this watch (we started an
//!   operation and never recorded its completion), **and**
//! - the PID recorded in that record is no longer alive
//!   ([`kill(pid, 0)`](rustix::process::test_kill_process) reports `ESRCH`),
//!   **and**
//! - the lock file's mtime falls inside the recorded operation's time window —
//!   from [`STALE_LOCK_TS_SLACK`] before the record's `begin` timestamp through
//!   [`MAX_OP_WINDOW`] after it. A lock created before our operation began, or
//!   materially after it, cannot be ours: the recorded owner is dead and wrote
//!   nothing outside that window, so such a lock belongs to another process (a
//!   long `git gc`, an interactive rebase) and is never touched, **and**
//! - the lock file's mtime is older than [`STALE_LOCK_MIN_AGE`].
//!
//! If the journal has no dangling record, the lock is foreign and is left
//! untouched. If the owning PID is still alive, or the lock is younger than the
//! age gate, the lock is left for a later start to reconsider. A lock outside
//! the operation window is reported as foreign and left in place. Corrupt or
//! unreadable journals are treated conservatively: nothing is removed.
//!
//! # File format
//!
//! One journal file per watch under [`paths::journal_dir`], the filename
//! derived from the watch name (see [`Journal::in_dir`]). The format is
//! line-oriented, human-readable, and internal — no serialization crate. A
//! `begin` record is written before an operation starts:
//!
//! ```text
//! begin <op> pid=<pid> ts=<unix-seconds>
//! ```
//!
//! On clean completion the file is compacted to empty. An empty **or** absent
//! journal therefore means "no dangling operation". Operations against one
//! watch are serial, so at most one `begin` record is ever live.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};

use crate::paths;

/// A git lock whose mtime is younger than this is left alone even when its
/// journaled owner is dead: a just-created lock may belong to an operation that
/// only *looks* abandoned because its `end` record has not landed yet. Fifteen
/// minutes comfortably exceeds any single snapshot or sync.
pub(crate) const STALE_LOCK_MIN_AGE: Duration = Duration::from_secs(15 * 60);

/// Clock slack when matching a git lock's mtime against the journaled
/// operation's `begin` timestamp: the lock's mtime may fall up to this far
/// *before* the record (filesystem timestamp granularity, the lock being
/// written a beat before the journal record lands). A lock older than that
/// predates the operation entirely and cannot be ours.
pub(crate) const STALE_LOCK_TS_SLACK: Duration = Duration::from_secs(60);

/// The longest a single journaled operation is assumed to hold its git lock:
/// a lock whose mtime is more than this *after* the operation's `begin`
/// timestamp was created by someone else — the recorded owner is dead and
/// wrote nothing after the window — so it is foreign and never removed.
/// Fifteen minutes matches [`STALE_LOCK_MIN_AGE`]'s "comfortably exceeds any
/// single snapshot or sync" rationale.
pub(crate) const MAX_OP_WINDOW: Duration = Duration::from_secs(15 * 60);

/// The per-watch operation journal: a single line-oriented file recording the
/// in-flight daemon operation for one watch.
pub(crate) struct Journal {
    /// The journal file for this watch.
    path: PathBuf,
}

impl Journal {
    /// The journal for `watch_name` at the default location, one file under
    /// `$XDG_STATE_HOME/vard/journal`.
    // The daemon resolves the journal dir via `DaemonPaths` and uses `in_dir`;
    // this XDG convenience is kept for future non-daemon callers.
    #[allow(dead_code)]
    pub(crate) fn for_watch(watch_name: &str) -> Result<Journal, JournalError> {
        let dir = paths::journal_dir().map_err(|e| JournalError::Path(e.to_string()))?;
        Ok(Self::in_dir(&dir, watch_name))
    }

    /// The journal for `watch_name` inside `dir`, so tests inject a tempdir.
    ///
    /// The filename is the watch name with path-hostile characters replaced by
    /// `_`, suffixed with a hash of the original name so that two names that
    /// sanitize alike (e.g. `a/b` and `a_b`) still get distinct files.
    pub(crate) fn in_dir(dir: &Path, watch_name: &str) -> Journal {
        Journal {
            path: dir.join(journal_file_name(watch_name)),
        }
    }

    /// The journal file's path.
    // Exercised by tests; a diagnostic accessor for future callers.
    #[allow(dead_code)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Records the start of daemon operation `op` (e.g. `"snapshot"`),
    /// truncating any prior contents. Call before the operation begins; call
    /// [`complete`](Self::complete) after it finishes cleanly.
    ///
    /// `op` must be a single whitespace-free token; any whitespace is replaced
    /// with `_` to keep the record parseable.
    pub(crate) fn begin(&self, op: &str) -> Result<(), JournalError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| self.io_err(source))?;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)
            .map_err(|source| self.io_err(source))?;
        writeln!(
            file,
            "begin {} pid={} ts={ts}",
            sanitize_token(op),
            std::process::id()
        )
        .map_err(|source| self.io_err(source))?;
        file.flush().map_err(|source| self.io_err(source))
    }

    /// Marks the current operation complete by compacting the journal to empty.
    /// An empty journal is indistinguishable from "no dangling operation",
    /// which is exactly the post-completion state.
    pub(crate) fn complete(&self) -> Result<(), JournalError> {
        self.compact()
    }

    /// Startup recovery for this watch's repository at `repo_path`. Reads the
    /// journal and, if a dangling operation is found, cleans the git index lock
    /// **only** when it is provably a stale remnant of ours (see the [module
    /// docs](self)). Never panics and never returns an error: every outcome,
    /// including I/O trouble, is folded into the returned [`RecoveryReport`] so
    /// startup is never blocked.
    pub(crate) fn recover(&self, repo_path: &Path, opts: RecoveryOpts) -> RecoveryReport {
        let dangling = match self.read_dangling() {
            Ok(Some(record)) => record,
            Ok(None) => return RecoveryReport::Clean,
            Err(detail) => return RecoveryReport::Corrupt { detail },
        };

        let lock_path = repo_path.join(".git").join("index.lock");
        let mtime = match lock_mtime(&lock_path) {
            LockMtime::Absent => {
                // Nothing wedged; drop the dangling record so we don't
                // reconsider it every start.
                let _ = self.compact();
                return RecoveryReport::NoLockPresent { op: dangling.op };
            }
            LockMtime::Unreadable(detail) => {
                return RecoveryReport::Failed { detail };
            }
            LockMtime::At(mtime) => mtime,
        };

        if pid_is_alive(dangling.pid) {
            return RecoveryReport::HolderAlive {
                op: dangling.op,
                pid: dangling.pid,
            };
        }

        // The lock is ours only if its mtime falls inside the recorded
        // operation's window ([STALE_LOCK_TS_SLACK] before the begin timestamp
        // through [MAX_OP_WINDOW] after it). Outside it, the lock belongs to
        // some other process — our dead owner wrote nothing outside that window
        // — so it is left untouched. The journal is compacted: our operation
        // demonstrably left no lock behind, and the record must not condemn a
        // foreign lock on every future start.
        if !lock_in_op_window(mtime, dangling.ts) {
            let _ = self.compact();
            return RecoveryReport::LockNotOurs {
                op: dangling.op,
                pid: dangling.pid,
            };
        }

        let age = opts.now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age < opts.min_lock_age {
            return RecoveryReport::LockTooFresh {
                op: dangling.op,
                pid: dangling.pid,
                age,
            };
        }

        // Provably ours and provably stale: remove the lock, then compact.
        match fs::remove_file(&lock_path) {
            Ok(()) => {
                let _ = self.compact();
                RecoveryReport::LockRemoved {
                    op: dangling.op,
                    pid: dangling.pid,
                    age,
                }
            }
            Err(source) => RecoveryReport::Failed {
                detail: format!("removing {}: {source}", lock_path.display()),
            },
        }
    }

    /// Reads the journal and extracts the dangling `begin` record, if any.
    /// `Ok(None)` means the journal is absent or empty (no dangling op);
    /// `Err(detail)` means the file exists but could not be read or parsed —
    /// the caller must treat that conservatively.
    fn read_dangling(&self) -> Result<Option<DanglingOp>, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("reading {}: {e}", self.path.display())),
        };

        let mut found: Option<DanglingOp> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record =
                parse_begin(line).ok_or_else(|| format!("unparseable journal line: {line:?}"))?;
            // Serial operations mean one live record; keep the last if several.
            found = Some(record);
        }
        Ok(found)
    }

    /// Compacts the journal to empty (if it exists). Best-effort: a compaction
    /// error is reported by the caller but never fatal.
    fn compact(&self) -> Result<(), JournalError> {
        match OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
        {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(self.io_err(source)),
        }
    }

    fn io_err(&self, source: std::io::Error) -> JournalError {
        JournalError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

/// A parsed dangling `begin` record.
struct DanglingOp {
    op: String,
    pid: u32,
    /// The record's `ts=` field: when the operation began, in unix seconds.
    /// Ties a present git lock to *this* operation — a lock whose mtime falls
    /// outside the operation's window cannot be ours (see [`lock_in_op_window`]).
    ts: u64,
}

/// Whether a git lock with the given mtime could belong to an operation that
/// began at unix-seconds `begin_ts`: inside
/// `[begin_ts - STALE_LOCK_TS_SLACK, begin_ts + MAX_OP_WINDOW]`. Conservative
/// on unrepresentable timestamps (a `ts` too large for [`SystemTime`]): returns
/// `false`, so the lock is treated as foreign and never removed.
fn lock_in_op_window(mtime: SystemTime, begin_ts: u64) -> bool {
    let Some(begin) = UNIX_EPOCH.checked_add(Duration::from_secs(begin_ts)) else {
        return false;
    };
    let earliest = begin.checked_sub(STALE_LOCK_TS_SLACK).unwrap_or(UNIX_EPOCH);
    let Some(latest) = begin.checked_add(MAX_OP_WINDOW) else {
        return false;
    };
    mtime >= earliest && mtime <= latest
}

/// Tunables for [`Journal::recover`], injectable so tests need not manipulate
/// real file mtimes or wait real time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecoveryOpts {
    /// "Now", against which the lock's mtime age is measured. Production passes
    /// [`SystemTime::now`].
    pub now: SystemTime,
    /// A lock younger than this (by mtime) is left alone even if its owner is
    /// dead. Production uses [`STALE_LOCK_MIN_AGE`].
    pub min_lock_age: Duration,
}

impl RecoveryOpts {
    /// Production defaults: real wall clock and [`STALE_LOCK_MIN_AGE`].
    pub(crate) fn new() -> RecoveryOpts {
        RecoveryOpts {
            now: SystemTime::now(),
            min_lock_age: STALE_LOCK_MIN_AGE,
        }
    }
}

impl Default for RecoveryOpts {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`Journal::recover`] found and did, for the caller to log.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum RecoveryReport {
    /// No dangling operation — journal absent or empty. Nothing to do.
    Clean,
    /// A dangling operation's git lock was provably stale and was removed.
    LockRemoved {
        /// The dangling operation's kind.
        op: String,
        /// The dead PID that had recorded it.
        pid: u32,
        /// How far past the age gate the lock's mtime was.
        age: Duration,
    },
    /// A dangling operation existed but no git lock was present; the journal
    /// was compacted.
    NoLockPresent {
        /// The dangling operation's kind.
        op: String,
    },
    /// A dangling operation existed and its recorded PID is still alive; the
    /// lock (if any) was left untouched.
    HolderAlive {
        /// The dangling operation's kind.
        op: String,
        /// The still-live PID.
        pid: u32,
    },
    /// A dangling operation's owner is dead, but the git lock's mtime falls
    /// outside that operation's time window
    /// ([`STALE_LOCK_TS_SLACK`]/[`MAX_OP_WINDOW`]), so the lock cannot be ours:
    /// it is foreign and was left untouched. The journal was compacted — our
    /// operation demonstrably left no lock behind.
    LockNotOurs {
        /// The dangling operation's kind.
        op: String,
        /// The dead PID that had recorded it.
        pid: u32,
    },
    /// A dangling operation's owner is gone, but the lock is younger than the
    /// age gate; left untouched pending a later start.
    LockTooFresh {
        /// The dangling operation's kind.
        op: String,
        /// The dead PID that had recorded it.
        pid: u32,
        /// The lock's current age (below the gate).
        age: Duration,
    },
    /// The journal existed but could not be read or parsed, or lock handling
    /// hit an I/O error. Conservative: nothing was removed.
    Corrupt {
        /// A human-readable description of the trouble.
        detail: String,
    },
    /// An I/O error while inspecting or removing the lock. Conservative:
    /// nothing was removed.
    Failed {
        /// A human-readable description of the trouble.
        detail: String,
    },
}

impl fmt::Display for RecoveryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryReport::Clean => f.write_str("no dangling operation; nothing to recover"),
            RecoveryReport::LockRemoved { op, pid, age } => write!(
                f,
                "removed stale git lock for dangling {op:?} (dead PID {pid}, lock age {}s)",
                age.as_secs()
            ),
            RecoveryReport::NoLockPresent { op } => {
                write!(f, "dangling {op:?} had no git lock; journal compacted")
            }
            RecoveryReport::HolderAlive { op, pid } => write!(
                f,
                "dangling {op:?} still owned by live PID {pid}; git lock left in place"
            ),
            RecoveryReport::LockNotOurs { op, pid } => write!(
                f,
                "dangling {op:?} owner (PID {pid}) is gone but the git lock's mtime is outside \
                 that operation's window; foreign lock left in place"
            ),
            RecoveryReport::LockTooFresh { op, pid, age } => write!(
                f,
                "dangling {op:?} owner (PID {pid}) is gone but git lock is fresh ({}s); left in place",
                age.as_secs()
            ),
            RecoveryReport::Corrupt { detail } => {
                write!(f, "journal unreadable, left in place: {detail}")
            }
            RecoveryReport::Failed { detail } => {
                write!(f, "recovery could not touch the git lock: {detail}")
            }
        }
    }
}

/// The mtime lookup result for a git lock file.
enum LockMtime {
    /// No lock file present.
    Absent,
    /// The lock's last-modified time.
    At(SystemTime),
    /// The lock exists but its metadata or mtime could not be read.
    Unreadable(String),
}

fn lock_mtime(path: &Path) -> LockMtime {
    match fs::metadata(path) {
        Ok(meta) => match meta.modified() {
            Ok(mtime) => LockMtime::At(mtime),
            Err(e) => LockMtime::Unreadable(format!("reading mtime of {}: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LockMtime::Absent,
        Err(e) => LockMtime::Unreadable(format!("stat {}: {e}", path.display())),
    }
}

/// `true` unless the process is provably gone. `kill(pid, 0)` returning `ESRCH`
/// means no such process (dead); `Ok` (exists) or `EPERM` (exists, no
/// permission) mean alive. Anything unexpected errs on the side of "alive" so a
/// live process's lock is never removed.
fn pid_is_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return true;
    };
    let Some(pid) = Pid::from_raw(raw) else {
        return true;
    };
    !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

/// Parses a `begin <op> pid=<pid> ts=<ts>` line into a [`DanglingOp`], tolerant
/// of `pid`/`ts` ordering. Returns `None` for any line that is not a
/// well-formed `begin` record with a parseable PID and timestamp — both are
/// required, since recovery cannot tie a lock to an operation without them
/// ([`begin`](Journal::begin) always writes both).
fn parse_begin(line: &str) -> Option<DanglingOp> {
    let mut tokens = line.split_whitespace();
    if tokens.next()? != "begin" {
        return None;
    }
    let op = tokens.next()?.to_string();
    let mut pid: Option<u32> = None;
    let mut ts: Option<u64> = None;
    for token in tokens {
        if let Some(raw) = token.strip_prefix("pid=") {
            pid = Some(raw.parse().ok()?);
        } else if let Some(raw) = token.strip_prefix("ts=") {
            ts = Some(raw.parse().ok()?);
        }
        // Any future fields are ignored here.
    }
    Some(DanglingOp {
        op,
        pid: pid?,
        ts: ts?,
    })
}

/// Replaces ASCII whitespace in `token` with `_` so a written record stays a
/// single parseable line.
fn sanitize_token(token: &str) -> String {
    token
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect()
}

/// Derives a filesystem-safe journal filename from a watch name: path-hostile
/// characters become `_`, and a hash of the original name is appended so
/// distinct names never collide on one file.
///
/// The hash is FNV-1a, hand-rolled so the filename is stable across Rust
/// toolchains — `DefaultHasher` makes no such guarantee, and an unstable
/// filename would orphan a dangling journal on upgrade, leaving a stale lock
/// that recovery could never clean.
fn journal_file_name(watch_name: &str) -> String {
    let sanitized: String = watch_name
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' => c,
            _ => '_',
        })
        .collect();
    format!("{sanitized}-{:016x}.journal", fnv1a(watch_name.as_bytes()))
}

/// 64-bit FNV-1a over `bytes`; a fixed, documented algorithm rather than a
/// toolchain-dependent one.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(PRIME)
    })
}

/// Everything that can go wrong writing or compacting the journal. Recovery
/// itself never surfaces these — it folds failures into [`RecoveryReport`].
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum JournalError {
    /// The journal directory path could not be resolved (see [`paths`]).
    // Only produced by the XDG `for_watch` constructor, dead until a non-daemon
    // caller uses it; the daemon builds journals from an explicit dir.
    #[allow(dead_code)]
    Path(String),
    /// A journal file operation failed.
    Io {
        /// The journal file path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::Path(msg) => write!(f, "resolving journal path: {msg}"),
            JournalError::Io { path, source } => {
                write!(f, "journal {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JournalError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `repo_path` whose `.git` directory exists, plus a pre-written
    /// `index.lock` with the current wall-clock mtime.
    fn repo_with_lock(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git").join("index.lock"), b"lock").unwrap();
        repo
    }

    fn lock_path(repo: &Path) -> PathBuf {
        repo.join(".git").join("index.lock")
    }

    /// A dead PID: allocate one by spawning `true` and reaping it, so the PID is
    /// known to have exited. Small reuse risk, acceptable for a test.
    fn dead_pid() -> u32 {
        let child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let mut child = child;
        child.wait().expect("reap true");
        pid
    }

    /// Hand-writes a dangling `begin` record with `pid` and `ts` into `journal`.
    fn write_dangling(journal: &Journal, op: &str, pid: u32, ts: u64) {
        if let Some(parent) = journal.path().parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(journal.path(), format!("begin {op} pid={pid} ts={ts}\n")).unwrap();
    }

    /// The lock file's mtime as unix seconds, for writing a `begin` record
    /// whose timestamp is coherent with the lock (inside the operation window).
    fn mtime_secs(path: &Path) -> u64 {
        fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn begin_then_complete_leaves_a_compacted_file() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::in_dir(dir.path(), "notes");
        journal.begin("snapshot").unwrap();
        assert!(
            fs::metadata(journal.path()).unwrap().len() > 0,
            "begin should write a record"
        );
        journal.complete().unwrap();
        assert_eq!(
            fs::metadata(journal.path()).unwrap().len(),
            0,
            "complete should compact the journal to empty"
        );
    }

    #[test]
    fn no_journal_recovers_clean() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        // A foreign lock is present, but with no journal it is not ours.
        let report = journal.recover(&repo, RecoveryOpts::new());
        assert!(matches!(report, RecoveryReport::Clean), "got: {report}");
        assert!(lock_path(&repo).exists(), "foreign lock must be left alone");
    }

    #[test]
    fn dangling_dead_pid_old_lock_removes_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        // ts coherent with the lock's mtime: the lock is inside the op window.
        let ts = mtime_secs(&lock_path(&repo));
        write_dangling(&journal, "snapshot", dead_pid(), ts);

        // now = lock mtime + well past the gate.
        let mtime = fs::metadata(lock_path(&repo)).unwrap().modified().unwrap();
        let opts = RecoveryOpts {
            now: mtime + STALE_LOCK_MIN_AGE + Duration::from_secs(1),
            min_lock_age: STALE_LOCK_MIN_AGE,
        };
        let report = journal.recover(&repo, opts);
        assert!(
            matches!(report, RecoveryReport::LockRemoved { .. }),
            "got: {report}"
        );
        assert!(!lock_path(&repo).exists(), "stale lock should be removed");
        // Journal compacted afterwards.
        assert_eq!(fs::metadata(journal.path()).unwrap().len(), 0);
    }

    #[test]
    fn dangling_alive_pid_leaves_lock() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        // Our own PID is alive.
        let ts = mtime_secs(&lock_path(&repo));
        write_dangling(&journal, "snapshot", std::process::id(), ts);

        let mtime = fs::metadata(lock_path(&repo)).unwrap().modified().unwrap();
        let opts = RecoveryOpts {
            now: mtime + STALE_LOCK_MIN_AGE + Duration::from_secs(1),
            min_lock_age: STALE_LOCK_MIN_AGE,
        };
        let report = journal.recover(&repo, opts);
        assert!(
            matches!(report, RecoveryReport::HolderAlive { .. }),
            "got: {report}"
        );
        assert!(lock_path(&repo).exists(), "live owner's lock must be kept");
    }

    #[test]
    fn dangling_dead_pid_fresh_lock_leaves_lock() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        // ts coherent with the mtime so the window check passes; only the age
        // gate blocks removal here.
        let ts = mtime_secs(&lock_path(&repo));
        write_dangling(&journal, "snapshot", dead_pid(), ts);

        // now == mtime, so age is zero — below the gate.
        let mtime = fs::metadata(lock_path(&repo)).unwrap().modified().unwrap();
        let opts = RecoveryOpts {
            now: mtime,
            min_lock_age: STALE_LOCK_MIN_AGE,
        };
        let report = journal.recover(&repo, opts);
        assert!(
            matches!(report, RecoveryReport::LockTooFresh { .. }),
            "got: {report}"
        );
        assert!(lock_path(&repo).exists(), "fresh lock must be kept");
    }

    #[test]
    fn no_dangling_begin_leaves_foreign_lock() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        // Empty (compacted) journal — no dangling begin.
        fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        fs::write(journal.path(), b"").unwrap();

        let report = journal.recover(&repo, RecoveryOpts::new());
        assert!(matches!(report, RecoveryReport::Clean), "got: {report}");
        assert!(lock_path(&repo).exists(), "foreign lock must be left alone");
    }

    #[test]
    fn corrupt_journal_is_conservative() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        fs::write(journal.path(), b"this is not a valid record\n").unwrap();

        let report = journal.recover(&repo, RecoveryOpts::new());
        assert!(
            matches!(report, RecoveryReport::Corrupt { .. }),
            "got: {report}"
        );
        assert!(
            lock_path(&repo).exists(),
            "a corrupt journal must never remove a lock"
        );
    }

    #[test]
    fn dangling_begin_without_lock_compacts_journal() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap(); // no index.lock
        let journal = Journal::in_dir(dir.path(), "notes");
        write_dangling(&journal, "snapshot", dead_pid(), 1000);

        let report = journal.recover(&repo, RecoveryOpts::new());
        assert!(
            matches!(report, RecoveryReport::NoLockPresent { .. }),
            "got: {report}"
        );
        assert_eq!(
            fs::metadata(journal.path()).unwrap().len(),
            0,
            "journal should be compacted when there is no lock to clean"
        );
    }

    #[test]
    fn lock_predating_the_begin_record_is_left_as_foreign() {
        // A lock created before our operation began (mtime more than the slack
        // before ts) cannot be ours — e.g. a long-running foreign git process
        // whose lock predates the daemon's whole episode. It must be left in
        // place even though our owner is dead and the lock is old.
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        // begin recorded well after the lock's mtime: lock predates the op.
        let ts = mtime_secs(&lock_path(&repo)) + STALE_LOCK_TS_SLACK.as_secs() + 100;
        write_dangling(&journal, "snapshot", dead_pid(), ts);

        let mtime = fs::metadata(lock_path(&repo)).unwrap().modified().unwrap();
        let opts = RecoveryOpts {
            now: mtime + STALE_LOCK_MIN_AGE + Duration::from_secs(1),
            min_lock_age: STALE_LOCK_MIN_AGE,
        };
        let report = journal.recover(&repo, opts);
        assert!(
            matches!(report, RecoveryReport::LockNotOurs { .. }),
            "got: {report}"
        );
        assert!(
            lock_path(&repo).exists(),
            "a lock predating our operation must never be removed"
        );
        assert_eq!(
            fs::metadata(journal.path()).unwrap().len(),
            0,
            "the dangling record is compacted: our op left no lock"
        );
    }

    #[test]
    fn lock_much_newer_than_the_begin_record_is_left_as_foreign() {
        // A lock created long after our operation began (mtime beyond the op
        // window) was made by someone else after our owner died — e.g. a user's
        // live rebase started hours later. It must be left in place.
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_lock(dir.path());
        let journal = Journal::in_dir(dir.path(), "notes");
        // begin recorded long before the lock's mtime: lock postdates the op.
        let ts = mtime_secs(&lock_path(&repo)) - MAX_OP_WINDOW.as_secs() - 100;
        write_dangling(&journal, "snapshot", dead_pid(), ts);

        let mtime = fs::metadata(lock_path(&repo)).unwrap().modified().unwrap();
        let opts = RecoveryOpts {
            now: mtime + STALE_LOCK_MIN_AGE + Duration::from_secs(1),
            min_lock_age: STALE_LOCK_MIN_AGE,
        };
        let report = journal.recover(&repo, opts);
        assert!(
            matches!(report, RecoveryReport::LockNotOurs { .. }),
            "got: {report}"
        );
        assert!(
            lock_path(&repo).exists(),
            "a lock created after our operation's window must never be removed"
        );
    }

    #[test]
    fn distinct_names_that_sanitize_alike_get_distinct_files() {
        assert_ne!(journal_file_name("a/b"), journal_file_name("a_b"));
    }

    #[test]
    fn parse_begin_tolerates_field_order() {
        let a = parse_begin("begin snapshot pid=42 ts=100").unwrap();
        assert_eq!(a.op, "snapshot");
        assert_eq!(a.pid, 42);
        assert_eq!(a.ts, 100);
        let b = parse_begin("begin sync ts=100 pid=7").unwrap();
        assert_eq!(b.op, "sync");
        assert_eq!(b.pid, 7);
        assert_eq!(b.ts, 100);
        assert!(
            parse_begin("begin snapshot ts=100").is_none(),
            "pid required"
        );
        assert!(
            parse_begin("begin snapshot pid=42").is_none(),
            "ts required: without it a lock cannot be tied to the operation"
        );
        assert!(parse_begin("garbage line").is_none());
    }

    #[test]
    fn lock_window_bounds_are_inclusive_and_overflow_safe() {
        let begin_ts = 1_000_000u64;
        let begin = UNIX_EPOCH + Duration::from_secs(begin_ts);
        // Inside: at begin, at the slack edge, at the window edge.
        assert!(lock_in_op_window(begin, begin_ts));
        assert!(lock_in_op_window(begin - STALE_LOCK_TS_SLACK, begin_ts));
        assert!(lock_in_op_window(begin + MAX_OP_WINDOW, begin_ts));
        // Outside: a second past either edge.
        assert!(!lock_in_op_window(
            begin - STALE_LOCK_TS_SLACK - Duration::from_secs(1),
            begin_ts
        ));
        assert!(!lock_in_op_window(
            begin + MAX_OP_WINDOW + Duration::from_secs(1),
            begin_ts
        ));
        // An unrepresentable timestamp is conservative: never ours.
        assert!(!lock_in_op_window(begin, u64::MAX));
    }
}
