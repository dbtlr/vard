//! The day-one [`VcsBackend`] implementation: shelling out to the `git` binary.
//!
//! # Mechanics
//!
//! Every git invocation uses [`std::process::Command`] with `git -C <repo>` so
//! the working directory is explicit, passes each argument as a separate
//! element (never through a shell, so nothing is word-split or glob-expanded),
//! and captures standard error into the returned [`VcsError`]. Network
//! operations set `GIT_TERMINAL_PROMPT=0` so a missing credential fails fast
//! instead of blocking on a terminal prompt.
//!
//! There are **no command timeouts** day one. A hung git process (for example a
//! network operation against an unreachable host that ignores
//! `GIT_TERMINAL_PROMPT`) will block its calling thread; the engine runs these
//! on `spawn_blocking` threads and owns any timeout policy.
//!
//! # What this layer deliberately does not do
//!
//! Protective pre-restore/pre-sync snapshots, retry/backoff on a contended
//! lock, watcher self-suppression, event emission, `.git/info/exclude`
//! seeding, and secret scanning are all out of scope — they belong to later
//! tasks. In particular this layer **never deletes a lock file**: it classifies
//! [`VcsError::LockContended`] and returns, leaving any cleanup (PID- and
//! age-gated) to the engine.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use super::{
    ChangeSummary, CommitMessage, LogFilter, PushOutcome, ReconcileOutcome, RemoteState,
    RestoreTarget, SafeState, Snapshot, SnapshotId, TRAILER_KEY, UnsafeReason, VcsBackend,
    VcsError, VcsRef, trigger_from_str,
};
use crate::config::DEFAULT_REMOTE;

/// The branch name a [`GitBackend::detect`] falls back to when the repository's
/// `HEAD` is detached and no branch can be read. Operational callers always use
/// [`GitBackend::open`] with an explicit branch, so this only affects a backend
/// built by detection on an unusually-detached repository.
const FALLBACK_BRANCH: &str = "main";

/// A [`VcsBackend`] backed by the system `git` binary.
///
/// Bound at construction to one working directory (the repository root vard
/// watches), one configured branch, and one configured remote. It does not read
/// configuration; branch and remote come from the watch's
/// [`WatchSpec`](crate::WatchSpec) at the call site.
#[derive(Clone, Debug)]
pub struct GitBackend {
    path: PathBuf,
    branch: String,
    remote: String,
}

impl GitBackend {
    /// Detects a git repository rooted **at** `path`.
    ///
    /// Returns `Some` only when `path` is inside a git work tree whose root is
    /// `path` itself. A `path` that sits *inside* a deeper repository (the
    /// repository is rooted at an ancestor) counts as detected-elsewhere and
    /// returns `None`, exactly as a `path` that is not in any repository does.
    /// The two `None` cases are not distinguished in the return type; telling
    /// them apart would require a richer result type and is deferred.
    ///
    /// The returned backend adopts the repository's current branch (matching a
    /// watch whose configured branch is `None`) and the default remote
    /// (`origin`). Callers that need an explicit branch or remote should use
    /// [`open`](Self::open).
    pub fn detect(path: impl AsRef<Path>) -> Result<Option<GitBackend>, VcsError> {
        let path = path.as_ref();
        let out = git_output(path, ["rev-parse", "--show-toplevel"], false)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("not a git repository") {
                return Ok(None);
            }
            // A missing directory, permission error, or the like is a real
            // problem, not a "no repo here" answer.
            return Err(command_failed("rev-parse", &out));
        }

        let toplevel = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if same_dir(&toplevel, path)? {
            let branch = current_branch(path)?.unwrap_or_else(|| FALLBACK_BRANCH.to_string());
            Ok(Some(GitBackend {
                path: path.to_path_buf(),
                branch,
                remote: DEFAULT_REMOTE.to_string(),
            }))
        } else {
            // Repository rooted at an ancestor: detected-elsewhere.
            Ok(None)
        }
    }

    /// Initializes a new git repository at `path`.
    ///
    /// When `branch` is `Some`, the initial branch is named accordingly
    /// (`git init -b`); when `None`, git's configured default branch name is
    /// used. The returned backend's branch is read back from the fresh
    /// (unborn) `HEAD`, so it reflects whatever branch git actually created, and
    /// its remote is the default (`origin`).
    pub fn init(path: impl AsRef<Path>, branch: Option<&str>) -> Result<GitBackend, VcsError> {
        let path = path.as_ref();
        match branch {
            Some(b) => {
                checked(path, "init", ["init", "-b", b], false)?;
            }
            None => {
                checked(path, "init", ["init"], false)?;
            }
        }
        let resolved = current_branch(path)?
            .or_else(|| branch.map(str::to_string))
            .unwrap_or_else(|| FALLBACK_BRANCH.to_string());
        Ok(GitBackend {
            path: path.to_path_buf(),
            branch: resolved,
            remote: DEFAULT_REMOTE.to_string(),
        })
    }

    /// Opens an existing repository at `path`, configured to commit to `branch`
    /// and sync with `remote`.
    ///
    /// This is the operational constructor: the engine calls it with the
    /// branch and remote resolved from the watch's
    /// [`WatchSpec`](crate::WatchSpec). It validates that `path` is inside a git
    /// repository, returning [`VcsError::NotARepo`] otherwise, but does not
    /// require that `path` be the repository root.
    pub fn open(
        path: impl AsRef<Path>,
        branch: &str,
        remote: &str,
    ) -> Result<GitBackend, VcsError> {
        let path = path.as_ref();
        let out = git_output(path, ["rev-parse", "--git-dir"], false)?;
        if !out.status.success() {
            return Err(VcsError::NotARepo);
        }
        Ok(GitBackend {
            path: path.to_path_buf(),
            branch: branch.to_string(),
            remote: remote.to_string(),
        })
    }

    /// The working directory this backend operates on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The branch this backend commits to.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The remote this backend pushes to and pulls from.
    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// Summarizes the working tree's current changes from `git status
    /// --porcelain`, for building a [`CommitMessage`].
    ///
    /// Renames count once as a change; the destination name is what appears in
    /// `notable`. This reads the tree exactly as it is at call time; a caller
    /// building a message should compute it immediately before snapshotting.
    pub fn change_summary(&self) -> Result<ChangeSummary, VcsError> {
        let out = checked(
            &self.path,
            "status",
            ["status", "--porcelain", "--untracked-files=all"],
            false,
        )?;
        Ok(parse_porcelain(&out))
    }

    /// Runs a git command that is expected to succeed, returning its stdout.
    fn run<I, S>(&self, op: &str, args: I) -> Result<String, VcsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        checked(&self.path, op, args, false)
    }

    /// Resolves a revision to its full hash, or `None` when the ref does not
    /// exist.
    fn rev_of(&self, refname: &str) -> Result<Option<String>, VcsError> {
        let out = git_output(
            &self.path,
            ["rev-parse", "--verify", "--quiet", refname],
            false,
        )?;
        if out.status.success() {
            Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ))
        } else if out.status.code() == Some(1) {
            // `--verify --quiet` exits 1 with no output for an unknown ref.
            Ok(None)
        } else {
            Err(command_failed("rev-parse", &out))
        }
    }

    /// The absolute path to this repository's git directory.
    fn git_dir(&self) -> Result<PathBuf, VcsError> {
        let out = self.run("rev-parse", ["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(out.trim()))
    }

    /// Whether a rebase is currently in progress (its state directory exists).
    fn rebase_in_progress(&self) -> Result<bool, VcsError> {
        let git_dir = self.git_dir()?;
        Ok(git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists())
    }
}

impl VcsBackend for GitBackend {
    fn is_safe_state(&self) -> Result<SafeState, VcsError> {
        let git_dir = self.git_dir()?;

        // In-progress operations, keyed off the marker files/dirs git leaves in
        // the git dir (spec §3). Checked before the HEAD state so that a rebase
        // — which also detaches HEAD — is reported as a rebase, not as a
        // detached head.
        if git_dir.join("MERGE_HEAD").exists() {
            return Ok(SafeState::Unsafe(UnsafeReason::MergeInProgress));
        }
        if git_dir.join("CHERRY_PICK_HEAD").exists() {
            return Ok(SafeState::Unsafe(UnsafeReason::CherryPickInProgress));
        }
        if git_dir.join("REVERT_HEAD").exists() {
            return Ok(SafeState::Unsafe(UnsafeReason::RevertInProgress));
        }
        if git_dir.join("BISECT_LOG").exists() {
            return Ok(SafeState::Unsafe(UnsafeReason::BisectInProgress));
        }
        if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
            return Ok(SafeState::Unsafe(UnsafeReason::RebaseInProgress));
        }

        match current_branch(&self.path)? {
            None => Ok(SafeState::Unsafe(UnsafeReason::DetachedHead)),
            Some(actual) if actual != self.branch => {
                Ok(SafeState::Unsafe(UnsafeReason::WrongBranch {
                    expected: self.branch.clone(),
                    actual,
                }))
            }
            Some(_) => Ok(SafeState::Safe),
        }
    }

    fn snapshot(&self, msg: &CommitMessage) -> Result<Option<SnapshotId>, VcsError> {
        // Total sweep of the work tree (ADR 0001). Staging a deletion never
        // loses data: the content remains reachable in history (ADR 0002).
        self.run("add", ["add", "-A"])?;

        // Nothing staged means nothing to snapshot; never force an empty commit.
        let diff = git_output(&self.path, ["diff", "--cached", "--quiet"], false)?;
        match diff.status.code() {
            Some(0) => return Ok(None), // no staged changes
            Some(1) => {}               // staged changes present, proceed
            _ => return Err(command_failed("diff", &diff)),
        }

        // Pass the exact message bytes on stdin (`-F -`) so multi-paragraph
        // bodies and the trailer are preserved verbatim. `--no-verify` keeps a
        // user's commit hooks from mutating or blocking vard's snapshots.
        let rendered = msg.render();
        let out = git_output_stdin(
            &self.path,
            ["commit", "--no-verify", "--file", "-"],
            false,
            &rendered,
        )?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if is_lock_contention(&stderr) {
                return Err(VcsError::LockContended);
            }
            return Err(command_failed("commit", &out));
        }

        let head = self
            .rev_of("HEAD")?
            .ok_or_else(|| VcsError::Parse("commit succeeded but HEAD is unborn".to_string()))?;
        Ok(Some(SnapshotId::new(head)))
    }

    fn log(&self, filter: &LogFilter) -> Result<Vec<Snapshot>, VcsError> {
        // Field separator (US) between fields, record separator (RS) after each
        // record; neither appears in commit subjects or trailer values.
        let format =
            format!("--format=%H%x1f%ct%x1f%s%x1f%(trailers:key={TRAILER_KEY},valueonly)%x1e");
        let mut args: Vec<String> = vec!["log".to_string(), format];
        if let Some(since) = filter.since {
            let secs = since
                .duration_since(UNIX_EPOCH)
                .map_err(|_| VcsError::Parse("since is before the unix epoch".to_string()))?
                .as_secs();
            args.push(format!("--since=@{secs}"));
        }
        if let Some(limit) = filter.limit {
            args.push(format!("--max-count={limit}"));
        }

        let out = self.run("log", &args)?;
        parse_log(&out)
    }

    fn diff(&self, from: &VcsRef, to: Option<&VcsRef>) -> Result<String, VcsError> {
        match to {
            Some(to) => self.run("diff", ["diff", from.as_str(), to.as_str()]),
            None => self.run("diff", ["diff", from.as_str()]),
        }
    }

    fn restore(&self, target: &RestoreTarget) -> Result<(), VcsError> {
        // `git checkout <rev> -- <pathspec>` overwrites the named paths in the
        // work tree and index with their content at <rev>. It never moves the
        // branch ref or HEAD, so it cannot drop a commit: the branch tip and
        // reflog are untouched and every commit stays reachable. Preserving the
        // pre-restore working state is the engine's job (a protective snapshot),
        // not this layer's.
        match &target.path {
            Some(path) => {
                let args: [&OsStr; 4] = [
                    OsStr::new("checkout"),
                    OsStr::new(target.rev.as_str()),
                    OsStr::new("--"),
                    path.as_os_str(),
                ];
                self.run("checkout", args)?;
            }
            None => {
                // Whole-tree restore: overlay <rev>'s tracked content across the
                // repository root. Files added *after* <rev> are left in place
                // (removing them is deferred; the engine's protective snapshot
                // preserves current state regardless).
                self.run("checkout", ["checkout", target.rev.as_str(), "--", "."])?;
            }
        }
        Ok(())
    }

    fn fetch(&self) -> Result<RemoteState, VcsError> {
        let tracking = format!("refs/remotes/{}/{}", self.remote, self.branch);
        let before = self.rev_of(&tracking)?;
        self.run(
            "fetch",
            ["fetch", self.remote.as_str(), self.branch.as_str()],
        )?;
        let after = self.rev_of(&tracking)?;

        // "Moved" means the remote-tracking ref changed — new upstream commits,
        // or the upstream appearing for the first time.
        let remote_moved = before != after && after.is_some();

        let (ahead, behind) = match &after {
            Some(_) => self.ahead_behind(&tracking)?,
            None => (0, 0),
        };
        Ok(RemoteState {
            remote_moved,
            ahead,
            behind,
        })
    }

    fn reconcile(&self) -> Result<ReconcileOutcome, VcsError> {
        let branch_ref = format!("refs/heads/{}", self.branch);
        let pre = self.rev_of(&branch_ref)?.ok_or_else(|| {
            VcsError::Parse(format!(
                "configured branch {:?} has no commits",
                self.branch
            ))
        })?;
        let upstream = format!("{}/{}", self.remote, self.branch);

        // Rebase the configured branch (named explicitly, so it is checked out
        // first regardless of current HEAD) onto the already-fetched upstream.
        // This operates on local refs only, so it is not a network op.
        let out = git_output(
            &self.path,
            ["rebase", upstream.as_str(), self.branch.as_str()],
            false,
        )?;

        if out.status.success() {
            let post = self.rev_of(&branch_ref)?.ok_or_else(|| {
                VcsError::Parse("branch vanished after a successful rebase".to_string())
            })?;
            if post == pre {
                Ok(ReconcileOutcome::AlreadyUpToDate)
            } else {
                Ok(ReconcileOutcome::Rebased {
                    new_head: SnapshotId::new(post),
                })
            }
        } else if self.rebase_in_progress()? {
            // A conflict left a rebase in progress. Abort it; the branch and
            // work tree return to exactly their pre-rebase state, so no conflict
            // markers can remain.
            self.run("rebase", ["rebase", "--abort"])?;
            let post = self.rev_of(&branch_ref)?.ok_or_else(|| {
                VcsError::Parse("branch vanished after aborting a rebase".to_string())
            })?;
            if post != pre {
                return Err(VcsError::Parse(format!(
                    "rebase --abort did not restore the branch (was {pre}, now {post})"
                )));
            }
            Ok(ReconcileOutcome::Conflict)
        } else {
            // Non-zero exit without a rebase in progress: a real failure (e.g.
            // an unknown upstream), not a conflict.
            let stderr = String::from_utf8_lossy(&out.stderr);
            if is_lock_contention(&stderr) {
                Err(VcsError::LockContended)
            } else {
                Err(command_failed("rebase", &out))
            }
        }
    }

    fn push(&self) -> Result<PushOutcome, VcsError> {
        let out = git_output(
            &self.path,
            ["push", self.remote.as_str(), self.branch.as_str()],
            true,
        )?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            if stderr.contains("Everything up-to-date") {
                Ok(PushOutcome::UpToDate)
            } else {
                Ok(PushOutcome::Pushed)
            }
        } else if is_non_fast_forward(&stderr) {
            Ok(PushOutcome::NonFastForward)
        } else if is_lock_contention(&stderr) {
            Err(VcsError::LockContended)
        } else {
            Err(command_failed("push", &out))
        }
    }
}

impl GitBackend {
    /// Counts how many commits the local branch is ahead of and behind its
    /// upstream, via `rev-list --left-right --count <branch>...<tracking>`.
    fn ahead_behind(&self, tracking: &str) -> Result<(usize, usize), VcsError> {
        let spec = format!("refs/heads/{}...{}", self.branch, tracking);
        let out = self.run(
            "rev-list",
            ["rev-list", "--left-right", "--count", spec.as_str()],
        )?;
        let mut fields = out.split_whitespace();
        let ahead = fields
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| VcsError::Parse(format!("unexpected rev-list output: {out:?}")))?;
        let behind = fields
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| VcsError::Parse(format!("unexpected rev-list output: {out:?}")))?;
        Ok((ahead, behind))
    }
}

/// Reads the current branch name, or `None` when `HEAD` is detached (or
/// unborn but with no symbolic ref — normally an unborn `HEAD` still names its
/// branch, which `symbolic-ref` reports).
fn current_branch(path: &Path) -> Result<Option<String>, VcsError> {
    let out = git_output(path, ["symbolic-ref", "--quiet", "--short", "HEAD"], false)?;
    if out.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    } else {
        // `--quiet` exits non-zero with no message when HEAD is not a symbolic
        // ref (detached).
        Ok(None)
    }
}

/// Whether two paths refer to the same directory, resolving symlinks (macOS
/// `/tmp` -> `/private/tmp`, for instance).
fn same_dir(a: &Path, b: &Path) -> Result<bool, VcsError> {
    let ca = std::fs::canonicalize(a).map_err(VcsError::Io)?;
    let cb = std::fs::canonicalize(b).map_err(VcsError::Io)?;
    Ok(ca == cb)
}

/// Parses `git status --porcelain` output into a [`ChangeSummary`].
fn parse_porcelain(output: &str) -> ChangeSummary {
    let mut summary = ChangeSummary::default();
    for line in output.lines() {
        if line.len() < 3 {
            continue;
        }
        // Columns 0..2 are the staged/worktree status; the path starts at 3.
        let code = &line[..2];
        let rest = &line[3..];
        // Renames/copies render as "old -> new"; the new name is what matters.
        let name = rest.rsplit(" -> ").next().unwrap_or(rest);
        let basename = name.trim().rsplit('/').next().unwrap_or(name).to_string();

        if code == "??" || code.contains('A') {
            summary.added += 1;
        } else if code.contains('D') {
            summary.deleted += 1;
        } else {
            // Modified, renamed, copied, type-changed, etc.
            summary.changed += 1;
        }
        summary.notable.push(basename);
    }
    summary
}

/// Parses the `%H\x1f%ct\x1f%s\x1f<trigger>\x1e`-formatted log stream into
/// [`Snapshot`]s.
fn parse_log(output: &str) -> Result<Vec<Snapshot>, VcsError> {
    let mut snapshots = Vec::new();
    for record in output.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let mut fields = record.split('\u{1f}');
        let hash = fields
            .next()
            .ok_or_else(|| VcsError::Parse("log record missing hash".to_string()))?;
        let ctime = fields
            .next()
            .ok_or_else(|| VcsError::Parse("log record missing timestamp".to_string()))?;
        let subject = fields
            .next()
            .ok_or_else(|| VcsError::Parse("log record missing subject".to_string()))?;
        // The trailer field is absent when the commit has no such trailer.
        let trigger_raw = fields.next().unwrap_or("");

        let secs: u64 = ctime
            .trim()
            .parse()
            .map_err(|_| VcsError::Parse(format!("bad commit timestamp: {ctime:?}")))?;
        let time = UNIX_EPOCH + Duration::from_secs(secs);
        let trigger = trigger_from_str(trigger_raw).filter(|_| !trigger_raw.trim().is_empty());

        snapshots.push(Snapshot {
            id: SnapshotId::new(hash.trim()),
            time,
            subject: subject.to_string(),
            trigger,
        });
    }
    Ok(snapshots)
}

/// Whether a git stderr indicates a lock held by another process. Focused on
/// the index lock per spec §3, but also catches git's generic "another process"
/// message. This layer only classifies the condition; it never deletes a lock.
fn is_lock_contention(stderr: &str) -> bool {
    (stderr.contains(".lock")
        && (stderr.contains("File exists") || stderr.contains("Unable to create")))
        || stderr.contains("Another git process seems to be running")
}

/// Whether a push stderr indicates a non-fast-forward rejection.
fn is_non_fast_forward(stderr: &str) -> bool {
    stderr.contains("non-fast-forward")
        || stderr.contains("fetch first")
        || stderr.contains("[rejected]")
}

/// Builds a `git -C <repo>` command with the given args, setting
/// `GIT_TERMINAL_PROMPT=0` for network operations so authentication failures
/// error out rather than blocking on a prompt.
fn git_command<I, S>(repo: &Path, args: I, network: bool) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    cmd.args(args);
    if network {
        cmd.env("GIT_TERMINAL_PROMPT", "0");
    }
    cmd
}

/// Runs a git command to completion, capturing its output. Maps a missing `git`
/// binary to [`VcsError::GitNotFound`] and other spawn failures to
/// [`VcsError::Io`]; a non-zero exit is reported through the returned
/// [`Output`], not as an error, so callers can classify it.
fn git_output<I, S>(repo: &Path, args: I, network: bool) -> Result<Output, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_command(repo, args, network)
        .output()
        .map_err(map_spawn_error)
}

/// Like [`git_output`], but writes `input` to the command's standard input.
fn git_output_stdin<I, S>(
    repo: &Path,
    args: I,
    network: bool,
    input: &str,
) -> Result<Output, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    use std::io::Write;
    use std::process::Stdio;

    let mut child = git_command(repo, args, network)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(map_spawn_error)?;

    // The pipe buffer is far larger than any commit message, so a single write
    // before waiting cannot deadlock.
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input.as_bytes())
        .map_err(VcsError::Io)?;

    child.wait_with_output().map_err(VcsError::Io)
}

/// Runs a git command expected to succeed, returning its stdout as a string.
/// Classifies a contended lock as [`VcsError::LockContended`] and any other
/// non-zero exit as [`VcsError::CommandFailed`].
fn checked<I, S>(repo: &Path, op: &str, args: I, network: bool) -> Result<String, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = git_output(repo, args, network)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if is_lock_contention(&stderr) {
            Err(VcsError::LockContended)
        } else {
            Err(command_failed(op, &out))
        }
    }
}

/// Maps a process-spawn error, distinguishing a missing binary.
fn map_spawn_error(e: std::io::Error) -> VcsError {
    if e.kind() == std::io::ErrorKind::NotFound {
        VcsError::GitNotFound
    } else {
        VcsError::Io(e)
    }
}

/// Builds a [`VcsError::CommandFailed`] from a finished command's output.
fn command_failed(op: &str, out: &Output) -> VcsError {
    VcsError::CommandFailed {
        op: op.to_string(),
        status: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_counts_by_disposition() {
        // Modified, added (staged), deleted, untracked, and a rename.
        let output =
            " M src/lib.rs\nA  new.rs\n D gone.rs\n?? scratch.txt\nR  old.rs -> renamed.rs\n";
        let summary = parse_porcelain(output);
        assert_eq!(summary.changed, 2); // modified + rename
        assert_eq!(summary.added, 2); // staged add + untracked
        assert_eq!(summary.deleted, 1);
        assert_eq!(
            summary.notable,
            vec!["lib.rs", "new.rs", "gone.rs", "scratch.txt", "renamed.rs"]
        );
    }

    #[test]
    fn parse_log_extracts_fields_and_trigger() {
        let record = "abc123\u{1f}1700000000\u{1f}snapshot: 1 changed (f)\u{1f}event\u{1e}\n\
             def456\u{1f}1700000060\u{1f}snapshot: 2 added\u{1f}\u{1e}\n";
        let snaps = parse_log(record).unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].id.as_str(), "abc123");
        assert_eq!(snaps[0].subject, "snapshot: 1 changed (f)");
        assert_eq!(snaps[0].trigger, Some(crate::Trigger::Event));
        assert_eq!(
            snaps[0].time,
            UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
        // Second record has no trailer.
        assert_eq!(snaps[1].trigger, None);
    }

    #[test]
    fn is_lock_contention_matches_index_lock() {
        let stderr = "fatal: Unable to create '/repo/.git/index.lock': File exists.\n\n\
                      Another git process seems to be running in this repository";
        assert!(is_lock_contention(stderr));
        assert!(!is_lock_contention("fatal: pathspec 'x' did not match"));
    }

    #[test]
    fn is_non_fast_forward_matches_rejection() {
        let stderr = " ! [rejected]        main -> main (non-fast-forward)\n\
                       error: failed to push some refs";
        assert!(is_non_fast_forward(stderr));
        assert!(!is_non_fast_forward("Everything up-to-date"));
    }
}
