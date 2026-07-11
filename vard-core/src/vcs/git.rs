//! The day-one [`VcsBackend`] implementation: shelling out to the `git` binary.
//!
//! # Mechanics
//!
//! Every git invocation uses [`std::process::Command`] with `git -C <repo>` so
//! the working directory is explicit, passes each argument as a separate
//! element (never through a shell, so nothing is word-split or glob-expanded),
//! and captures standard error into the returned [`VcsError`].
//!
//! Two environment pins apply to **every** invocation:
//!
//! - `LC_ALL=C` and `LANGUAGE=C`: this module classifies outcomes by matching
//!   git's English messages (lock contention, missing remote refs, "not a git
//!   repository"). Under another locale (e.g. `de_DE`'s "Kein
//!   Git-Repository") those matches silently fail, so the locale is forced.
//! - `GIT_TERMINAL_PROMPT=0` on network operations, so a missing credential
//!   fails fast instead of blocking on a terminal prompt.
//!
//! Commands that can invoke commit machinery or hooks additionally pin
//! repository config with `-c` (see the `CFG_*` constants): a user's
//! `commit.gpgsign`, `rebase.autostash`, `rerere.enabled`, or hooks must not
//! be able to hang, mutate, or corrupt vard's automated operations — a failing
//! signer mid-rebase would otherwise masquerade as a conflict, and autostash
//! can pop conflict markers into a tree the caller was told is clean.
//!
//! Only the two network-facing methods are time-bounded: [`fetch`] and
//! [`push`] take a caller-supplied `Duration` and, on expiry, kill the git
//! child — and, on unix, its whole process group, so an ssh transport's
//! children die with it — returning [`VcsError::Timeout`]. A host that ignores
//! `GIT_TERMINAL_PROMPT` and hangs therefore cannot block a worker forever.
//! Every other (local-only) operation stays unbounded; the engine runs all of
//! these on `spawn_blocking` threads.
//!
//! [`fetch`]: VcsBackend::fetch
//! [`push`]: VcsBackend::push
//!
//! # What this layer deliberately does not do
//!
//! Protective pre-restore/pre-sync snapshots, retry/backoff on a contended
//! lock, watcher self-suppression, event emission, `.git/info/exclude`
//! seeding, and secret scanning are all out of scope — they belong to later
//! tasks. In particular this layer **never deletes a lock file**: it classifies
//! [`VcsError::LockContended`] and returns, leaving any cleanup (PID- and
//! age-gated) to the engine.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::{
    AdvanceOutcome, ChangeSummary, LogFilter, PushOutcome, ReconcileOutcome, RemoteState,
    RestoreTarget, SafeState, Snapshot, SnapshotId, SnapshotOutcome, SnapshotRequest, TRAILER_KEY,
    UnsafeReason, VcsBackend, VcsError, VcsRef, trigger_from_str,
};
use crate::config::DEFAULT_REMOTE;
use crate::vcs::CommitMessage;

/// The branch name a [`GitBackend::detect`] falls back to when the repository's
/// `HEAD` is detached and no branch can be read. Operational callers always use
/// [`GitBackend::open`] with an explicit branch, so this only affects a backend
/// built by detection on an unusually-detached repository.
const FALLBACK_BRANCH: &str = "main";

/// `-c` pin: never invoke commit signing. A repository configured with
/// `commit.gpgsign=true` and a broken/prompting signer would otherwise fail or
/// hang vard's automated commits — and mid-rebase, a signing failure leaves
/// `rebase-merge/` behind, masquerading as a conflict (proven empirically).
const CFG_NO_SIGN: &str = "commit.gpgsign=false";

/// `-c` pin: no hooks. `/dev/null` is not a directory, so git finds no hooks
/// there. A user's hooks must not block, mutate, or hang vard's automated
/// commits, checkouts (post-checkout), rebases, or pushes (pre-push).
const CFG_NO_HOOKS: &str = "core.hooksPath=/dev/null";

/// `-c` pin: never autostash around a rebase. With `rebase.autostash=true` and
/// a dirty tree, a conflicting stash pop after the rebase leaves conflict
/// markers in the work tree while the rebase itself exits 0 (proven
/// empirically) — reconcile would report success over a corrupted tree. Pinned
/// off, a dirty tree makes the rebase refuse cleanly instead.
const CFG_NO_AUTOSTASH: &str = "rebase.autostash=false";

/// `-c` pin: no rerere. Recorded resolutions could silently auto-resolve a
/// rebase conflict with content vard never saw, so replays stay deterministic.
const CFG_NO_RERERE: &str = "rerere.enabled=false";

/// A [`VcsBackend`] backed by the system `git` binary.
///
/// Bound at construction to one working directory (the repository root vard
/// watches), one configured branch, and one configured remote.
///
/// # Constructor contract
///
/// - [`detect`](Self::detect) finds a repository rooted *at* the path and
///   adopts `HEAD`'s current branch (matching a watch whose configured branch
///   is `None`) and the default remote.
/// - [`init`](Self::init) creates a repository at the path, honoring an
///   explicit initial branch or git's configured default.
/// - [`open`](Self::open) is the operational constructor: the engine calls it
///   with the branch and remote resolved from the watch's
///   [`WatchSpec`](crate::WatchSpec). The backend never reads configuration
///   itself.
///
/// All three require the path to be the repository *root*: vard models the
/// watched directory as a dedicated repository, and operating from anywhere
/// else would make whole-tree operations (`add -A` sweeps the whole
/// repository; a whole-tree restore restores `.`) cover different trees.
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
    /// See the [type docs](GitBackend) for the constructor contract.
    pub fn detect(path: impl AsRef<Path>) -> Result<Option<GitBackend>, VcsError> {
        let path = path.as_ref();
        let out = git_output(path, &[], ["rev-parse", "--show-toplevel"], false)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("not a git repository") {
                return Ok(None);
            }
            // A missing directory, permission error, or the like is a real
            // problem, not a "no repo here" answer.
            return Err(classify_failure("rev-parse", &out));
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
    /// (unborn) `HEAD`, so it reflects whatever branch git actually created,
    /// and its remote is the default (`origin`).
    ///
    /// See the [type docs](GitBackend) for the constructor contract.
    pub fn init(path: impl AsRef<Path>, branch: Option<&str>) -> Result<GitBackend, VcsError> {
        let path = path.as_ref();
        match branch {
            Some(b) => {
                checked(path, &[], ["init", "-b", b], false)?;
            }
            None => {
                checked(path, &[], ["init"], false)?;
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

    /// Opens an existing repository rooted at `path`, configured to commit to
    /// `branch` and sync with `remote`.
    ///
    /// Returns [`VcsError::NotARepo`] when `path` is not inside a git
    /// repository, and [`VcsError::NotRepoRoot`] when it is inside one whose
    /// root is *not* `path` itself — the backend's whole-tree operations
    /// (snapshot's total sweep, whole-tree restore) are only coherent from the
    /// root.
    ///
    /// See the [type docs](GitBackend) for the constructor contract.
    pub fn open(
        path: impl AsRef<Path>,
        branch: &str,
        remote: &str,
    ) -> Result<GitBackend, VcsError> {
        let path = path.as_ref();
        let out = git_output(path, &[], ["rev-parse", "--show-toplevel"], false)?;
        if !out.status.success() {
            return Err(VcsError::NotARepo);
        }
        let toplevel = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if !same_dir(&toplevel, path)? {
            return Err(VcsError::NotRepoRoot {
                path: path.to_path_buf(),
                root: toplevel,
            });
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

    /// Runs a git command that is expected to succeed, returning its stdout.
    fn run<I, S>(&self, configs: &[&str], args: I) -> Result<String, VcsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        checked(&self.path, configs, args, false)
    }

    /// Resolves a revision to its full hash, or `None` when the ref does not
    /// exist.
    fn rev_of(&self, refname: &str) -> Result<Option<String>, VcsError> {
        let out = git_output(
            &self.path,
            &[],
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
            Err(classify_failure("rev-parse", &out))
        }
    }

    /// The absolute path to this repository's git directory. In a linked work
    /// tree this is the per-worktree directory (where the in-progress-operation
    /// markers for that work tree live), not the shared one.
    fn git_dir(&self) -> Result<PathBuf, VcsError> {
        let out = self.run(&[], ["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(out.trim()))
    }

    /// The path to this repository's private `info/exclude` file, resolved via
    /// `git rev-parse --git-path info/exclude`.
    ///
    /// Unlike joining `.git/info/exclude` by hand, this is correct when `.git`
    /// is a *file* rather than a directory: a linked worktree or a submodule
    /// stores its git dir elsewhere, and git resolves the shared `info/exclude`
    /// under the common directory. The returned path is absolute (a relative
    /// answer is resolved against the repository root).
    pub fn info_exclude_path(&self) -> Result<PathBuf, VcsError> {
        let out = self.run(&[], ["rev-parse", "--git-path", "info/exclude"])?;
        let raw = PathBuf::from(out.trim());
        Ok(if raw.is_absolute() {
            raw
        } else {
            self.path.join(raw)
        })
    }

    /// Counts how many commits the local branch is ahead of and behind the
    /// tracking ref, via `rev-list --left-right --count`. The caller must
    /// ensure both refs exist.
    fn ahead_behind(&self, tracking: &str) -> Result<(usize, usize), VcsError> {
        let spec = format!("refs/heads/{}...{}", self.branch, tracking);
        let out = self.run(&[], ["rev-list", "--left-right", "--count", spec.as_str()])?;
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

    /// Number of commits reachable from a ref, or 0 when the ref is unborn.
    fn commit_count(&self, refname: &str) -> Result<usize, VcsError> {
        if self.rev_of(refname)?.is_none() {
            return Ok(0);
        }
        let out = self.run(&[], ["rev-list", "--count", refname])?;
        out.trim()
            .parse()
            .map_err(|_| VcsError::Parse(format!("unexpected rev-list count: {out:?}")))
    }

    /// Returns an error if the repository is not safe to operate on. Defense
    /// in depth for mutating operations; the check-to-act window that remains
    /// is documented on [`VcsBackend::snapshot`].
    fn ensure_safe(&self) -> Result<(), VcsError> {
        match self.is_safe_state()? {
            SafeState::Safe => Ok(()),
            SafeState::Unsafe(reason) => Err(VcsError::UnsafeState(reason)),
        }
    }

    /// Rebases the scratch worktree's detached `HEAD` (checked out at the branch
    /// tip `pre`) onto the fetched upstream ref, classifying the outcome. Runs
    /// entirely with `-C scratch`, so the branch ref and the user's tree are
    /// untouched; the returned [`ReconcileOutcome::Rebased`] tip is a commit in
    /// the shared object store. The caller removes the scratch worktree.
    fn rebase_in_scratch(&self, scratch: &Path, pre: &str) -> Result<ReconcileOutcome, VcsError> {
        let upstream = format!("{}/{}", self.remote, self.branch);
        let rebase_cfg = &[CFG_NO_SIGN, CFG_NO_HOOKS, CFG_NO_AUTOSTASH, CFG_NO_RERERE];
        let out = git_output(scratch, rebase_cfg, ["rebase", upstream.as_str()], false)?;

        if out.status.success() {
            let post = head_of(scratch)?;
            if post == pre {
                // Upstream was already contained: nothing replayed.
                Ok(ReconcileOutcome::AlreadyUpToDate)
            } else {
                Ok(ReconcileOutcome::Rebased {
                    new_head: SnapshotId::new(post),
                })
            }
        } else if rebase_in_progress(&absolute_git_dir(scratch)?) {
            // A conflict left a rebase in progress in the scratch worktree.
            // Abort it for tidiness; the subsequent `worktree remove --force`
            // erases the worktree (and any residual rebase state) regardless,
            // so the abort is best-effort and its failure need not surface —
            // nothing mid-rebase can reach the user's repository.
            let _ = git_output(scratch, rebase_cfg, ["rebase", "--abort"], false);
            Ok(ReconcileOutcome::Conflict)
        } else {
            // Non-zero exit without a rebase in progress: a real failure (an
            // unknown upstream, a held lock), not a conflict.
            Err(classify_failure("rebase", &out))
        }
    }

    /// Removes the linked worktree at `scratch`, treating a `scratch` that is
    /// not (or is no longer) a registered worktree as a clean no-op. `--force`
    /// removes it even when dirty, locked, or mid-rebase.
    fn worktree_remove(&self, scratch: &Path) -> Result<(), VcsError> {
        let out = git_output(
            &self.path,
            &[CFG_NO_HOOKS],
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                scratch.as_os_str(),
            ],
            false,
        )?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Nothing to remove: git spells the "not a registered worktree" case
        // "is not a working tree", and a vanished path "No such file or
        // directory". Either way there is nothing left to clean up here.
        if stderr.contains("is not a working tree") || stderr.contains("No such file or directory")
        {
            return Ok(());
        }
        Err(classify_failure("worktree remove", &out))
    }
}

impl VcsBackend for GitBackend {
    fn is_safe_state(&self) -> Result<SafeState, VcsError> {
        let git_dir = self.git_dir()?;

        // In-progress operations, keyed off the marker files/dirs git leaves
        // in the git dir. Checked before the HEAD state so that a rebase —
        // which also detaches HEAD — is reported as a rebase, not as a
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
        if rebase_in_progress(&git_dir) {
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

    fn is_dirty(&self) -> Result<bool, VcsError> {
        // `status --porcelain` prints one line per changed path and nothing at
        // all for a clean tree, so a non-empty result means dirty. Untracked
        // files are included by default (respecting `.gitignore`), matching the
        // `add -A` sweep the snapshot would perform. No index mutation, no
        // network — a cheap read.
        let out = self.run(&[], ["status", "--porcelain"])?;
        Ok(!out.trim().is_empty())
    }

    fn snapshot(&self, req: &SnapshotRequest) -> Result<Option<SnapshotOutcome>, VcsError> {
        self.ensure_safe()?;

        // Total sweep of the work tree: vard snapshots the directory as a
        // whole, deliberately including (and thereby consuming) any index
        // state a user staged by hand. Staging a deletion never loses data:
        // the content remains reachable in history.
        self.run(&[], ["add", "-A"])?;

        // One source of truth for both "is there anything to commit?" and the
        // summary: the staged diff, read after the sweep, NUL-delimited so
        // unusual filenames arrive as raw bytes rather than C-quoted. This
        // also works on an unborn HEAD (it diffs against the empty tree).
        let staged = self.run(&[], ["diff", "--cached", "--name-status", "-z"])?;
        let summary = parse_name_status(&staged);
        if summary.total() == 0 {
            // Nothing staged means nothing to snapshot; never force an empty
            // commit.
            return Ok(None);
        }

        let msg = CommitMessage::new(
            summary.clone(),
            req.trigger,
            req.user_text.as_deref(),
            req.extra_trailers.clone(),
        );

        // Pass the exact message bytes on stdin (`-F -`) so multi-paragraph
        // bodies and the trailers are preserved verbatim. `--no-verify` plus
        // the hooks pin keeps a user's hooks from mutating or blocking the
        // commit; the signing pin keeps a broken signer from failing it.
        let out = git_output_stdin(
            &self.path,
            &[CFG_NO_SIGN, CFG_NO_HOOKS],
            ["commit", "--no-verify", "--file", "-"],
            &msg.render(),
        )?;
        if !out.status.success() {
            return Err(classify_failure("commit", &out));
        }

        let head = self
            .rev_of("HEAD")?
            .ok_or_else(|| VcsError::Parse("commit succeeded but HEAD is unborn".to_string()))?;
        Ok(Some(SnapshotOutcome {
            id: SnapshotId::new(head),
            summary,
        }))
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
        if let Some(until) = filter.until {
            let secs = until
                .duration_since(UNIX_EPOCH)
                .map_err(|_| VcsError::Parse("until is before the unix epoch".to_string()))?
                .as_secs();
            args.push(format!("--until=@{secs}"));
        }
        if let Some(limit) = filter.limit {
            args.push(format!("--max-count={limit}"));
        }

        let out = git_output(&self.path, &[], &args, false)?;
        if !out.status.success() {
            // A repository with no commits yet has an empty history, not an
            // error state.
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("does not have any commits yet") {
                return Ok(Vec::new());
            }
            return Err(classify_failure("log", &out));
        }
        parse_log(&String::from_utf8_lossy(&out.stdout))
    }

    fn diff(
        &self,
        from: &VcsRef,
        to: Option<&VcsRef>,
        pathspec: Option<&Path>,
    ) -> Result<String, VcsError> {
        // The trailing `--` marks the end of revisions, so a revision string
        // can never be reinterpreted as a pathspec (or vice versa). A scoped
        // path is passed as a literal pathspec (`:(literal)<path>`) so pathspec
        // magic is disabled and quoted/space paths match verbatim.
        let mut args: Vec<OsString> = vec![OsString::from("diff"), OsString::from(from.as_str())];
        if let Some(to) = to {
            args.push(OsString::from(to.as_str()));
        }
        args.push(OsString::from("--"));
        if let Some(path) = pathspec {
            args.push(literal_pathspec(path));
        }
        self.run(&[], args)
    }

    fn verify_ref(&self, rev: &VcsRef) -> Result<bool, VcsError> {
        // `<rev>^{commit}` requires the ref to resolve to (or peel to) a commit;
        // `--verify --quiet` exits 1 with no output for anything that does not.
        let peeled = format!("{}^{{commit}}", rev.as_str());
        Ok(self.rev_of(&peeled)?.is_some())
    }

    fn path_exists_at(&self, rev: &VcsRef, path: &Path) -> Result<bool, VcsError> {
        // `git ls-tree <rev> -- <pathspec>` lists the entry at that path when it
        // exists in the revision (blob or tree) and prints nothing when it does
        // not — exiting 0 either way, so presence is read off the stdout, not
        // the exit code (a real failure like a bad revision still errors). A
        // literal pathspec disables pathspec magic for verbatim matching.
        let spec = literal_pathspec(path);
        let args: [&OsStr; 4] = [
            OsStr::new("ls-tree"),
            OsStr::new(rev.as_str()),
            OsStr::new("--"),
            spec.as_os_str(),
        ];
        let out = git_output(&self.path, &[], args, false)?;
        if out.status.success() {
            Ok(!out.stdout.is_empty())
        } else {
            Err(classify_failure("ls-tree", &out))
        }
    }

    fn restore(&self, target: &RestoreTarget) -> Result<(), VcsError> {
        // `git checkout <rev> -- <pathspec>` overwrites the named paths in the
        // work tree and index with their content at <rev>. It never moves the
        // branch ref or HEAD, so it cannot drop a commit — but it DOES destroy
        // uncommitted working-tree content at those paths, which is why the
        // engine must snapshot before calling this (see the trait docs).
        match &target.path {
            Some(path) => {
                // A literal pathspec (`:(literal)<path>`) disables pathspec magic
                // so a path with a space or a leading `:` restores verbatim — the
                // same spelling the scoped diff preview uses, so the two agree.
                let spec = literal_pathspec(path);
                let args: [&OsStr; 4] = [
                    OsStr::new("checkout"),
                    OsStr::new(target.rev.as_str()),
                    OsStr::new("--"),
                    spec.as_os_str(),
                ];
                self.run(&[CFG_NO_HOOKS], args)?;
            }
            None => {
                // Whole-tree restore: overlay <rev>'s tracked content across
                // the repository root (open() guarantees the backend path IS
                // the root, so `.` covers the whole tree). Files added *after*
                // <rev> are left in place: removing them would destroy content
                // that may exist nowhere else, so it is deferred to a future,
                // engine-guarded flow.
                self.run(
                    &[CFG_NO_HOOKS],
                    ["checkout", target.rev.as_str(), "--", "."],
                )?;
            }
        }
        Ok(())
    }

    fn fetch(&self, timeout: Duration) -> Result<RemoteState, VcsError> {
        let tracking = format!("refs/remotes/{}/{}", self.remote, self.branch);
        let before = self.rev_of(&tracking)?;

        // An explicit refspec updates the tracking ref even for remotes
        // configured without fetch refspecs (e.g. added minimally or with
        // single-branch clones that track something else).
        let refspec = format!(
            "+refs/heads/{b}:refs/remotes/{r}/{b}",
            b = self.branch,
            r = self.remote
        );
        let out = git_output_timed(
            &self.path,
            &[CFG_NO_HOOKS],
            ["fetch", self.remote.as_str(), refspec.as_str()],
            "fetch",
            timeout,
        )?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("couldn't find remote ref") {
                // The branch has never been pushed: a normal first-push state.
                // Everything local is "ahead"; there is nothing to be behind.
                let ahead = self.commit_count(&format!("refs/heads/{}", self.branch))?;
                return Ok(RemoteState {
                    remote_moved: false,
                    ahead,
                    behind: 0,
                });
            }
            return Err(classify_failure("fetch", &out));
        }

        let after = self.rev_of(&tracking)?;
        // "Moved" means the remote-tracking ref changed — new upstream
        // commits, or the upstream appearing for the first time.
        let remote_moved = before != after && after.is_some();

        let local_exists = self
            .rev_of(&format!("refs/heads/{}", self.branch))?
            .is_some();
        let (ahead, behind) = match (&after, local_exists) {
            (Some(_), true) => self.ahead_behind(&tracking)?,
            // Local branch unborn: everything upstream is "behind".
            (Some(_), false) => (0, self.commit_count(&tracking)?),
            (None, _) => (0, 0),
        };
        Ok(RemoteState {
            remote_moved,
            ahead,
            behind,
        })
    }

    fn reconcile(&self, scratch: &Path) -> Result<ReconcileOutcome, VcsError> {
        self.ensure_safe()?;

        let branch_ref = format!("refs/heads/{}", self.branch);
        let pre = self.rev_of(&branch_ref)?.ok_or_else(|| {
            VcsError::Parse(format!(
                "configured branch {:?} has no commits",
                self.branch
            ))
        })?;

        // Create a vard-owned detached-HEAD linked worktree at the branch tip.
        // The rebase — and any conflict abort — happen entirely inside it, so
        // the branch ref and the user's working tree never move here. The
        // branch positional is the SHORT name: `git worktree add --detach`
        // checks the branch's commit out detached, leaving the branch itself
        // unmoved. Branch names cannot begin with `-`, so it cannot inject a
        // flag. The hooks pin keeps the checkout `worktree add` performs from
        // firing a user's post-checkout hook (arbitrary code from a background
        // daemon; it could hang or break the sync).
        let add = git_output(
            &self.path,
            &[CFG_NO_HOOKS],
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                scratch.as_os_str(),
                OsStr::new(self.branch.as_str()),
            ],
            false,
        )?;
        if !add.status.success() {
            return Err(classify_failure("worktree add", &add));
        }

        // From here the scratch worktree exists, so it must be removed on every
        // path. Cleanup is best-effort: reconcile's outcome (or its error) is
        // authoritative, and a cleanup that somehow fails leaves only a scratch
        // worktree that `prune_scratch` reclaims — never anything in the user's
        // tree.
        let outcome = self.rebase_in_scratch(scratch, &pre);
        let _ = self.worktree_remove(scratch);
        outcome
    }

    fn advance(
        &self,
        target: &SnapshotId,
        expected_tip: &SnapshotId,
    ) -> Result<AdvanceOutcome, VcsError> {
        self.ensure_safe()?;

        // Verify the target exists before touching the tree, so a bad id fails
        // cleanly with nothing changed. `verify_ref` peels to a commit.
        if !self.verify_ref(&VcsRef::from(target))? {
            return Err(VcsError::CommandFailed {
                op: "advance".to_string(),
                status: None,
                stderr: format!("target commit {target} does not exist"),
            });
        }

        // Guard the BRANCH REF: `checkout -B` moves the branch to `target`, so a
        // commit the user landed on the branch after the reconcile captured its
        // tip must not be silently stranded. Refuse when the tip no longer equals
        // what the reconcile consumed. Read immediately before the checkout; the
        // residual check-to-checkout race is documented on the trait (the op lock
        // excludes vard writers, so only a user racing their own commit is
        // exposed, and that is reflog-recoverable).
        let branch_ref = format!("refs/heads/{}", self.branch);
        let current_tip = self.rev_of(&branch_ref)?;
        if current_tip.as_deref() != Some(expected_tip.as_str()) {
            return Ok(AdvanceOutcome::WouldClobber);
        }

        // Move the branch and tree to `target` with SAFE checkout semantics:
        // `checkout -B <branch> <target>` refuses (non-zero, nothing changed)
        // when a locally-modified tracked file or an untracked file would be
        // clobbered, and carries non-conflicting local edits over unharmed. The
        // hooks pin keeps a user's post-checkout hook from running. Idempotent:
        // checking the branch out onto its own tip is a clean no-op.
        let out = git_output(
            &self.path,
            &[CFG_NO_HOOKS],
            ["checkout", "-B", self.branch.as_str(), target.as_str()],
            false,
        )?;
        if out.status.success() {
            return Ok(AdvanceOutcome::Advanced);
        }
        // git's checkout refusal messages for the clobber cases. Classify them as
        // a WouldClobber refusal (never destructive); anything else is a real
        // failure (a held lock, a broken repo).
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("would be overwritten")
            || stderr.contains("Please commit your changes or stash them")
            || stderr.contains("Please move or remove them before you switch branches")
        {
            return Ok(AdvanceOutcome::WouldClobber);
        }
        Err(classify_failure("checkout", &out))
    }

    fn has_remote(&self) -> Result<bool, VcsError> {
        // A cheap, non-network config lookup: `git config remote.<name>.url`
        // exits 0 with the URL when the remote is defined and exit 1 (no output)
        // when it is not. Never contacts the remote.
        let key = format!("remote.{}.url", self.remote);
        let out = git_output(&self.path, &[], ["config", "--get", key.as_str()], false)?;
        if out.status.success() {
            return Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty());
        }
        if out.status.code() == Some(1) {
            // `--get` exits 1 for a key that is not set.
            return Ok(false);
        }
        Err(classify_failure("config", &out))
    }

    fn prune_scratch(&self, scratch: &Path) -> Result<(), VcsError> {
        // Force-remove the scratch worktree if it is still registered (this
        // erases any mid-rebase state living under .git/worktrees/<name>)...
        self.worktree_remove(scratch)?;
        // ...then reap metadata for a worktree whose directory a crash deleted
        // out from under git but whose administrative entry remains.
        let prune = git_output(&self.path, &[CFG_NO_HOOKS], ["worktree", "prune"], false)?;
        if !prune.status.success() {
            return Err(classify_failure("worktree prune", &prune));
        }
        Ok(())
    }

    fn push(&self, timeout: Duration) -> Result<PushOutcome, VcsError> {
        // `--porcelain` gives a machine-readable per-ref status line on
        // stdout, so classification does not depend on human-oriented stderr
        // phrasing. Shape (verified empirically):
        //
        //   To <url>
        //   <flag>\t<from>:<to>\t<summary> (<reason>)
        //   Done
        //
        // where flag is `=` (up to date), `*` (new ref), ` ` (fast-forward),
        // `+` (forced), or `!` (rejected).
        let out = git_output_timed(
            &self.path,
            &[CFG_NO_HOOKS],
            [
                "push",
                "--porcelain",
                self.remote.as_str(),
                self.branch.as_str(),
            ],
            "push",
            timeout,
        )?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let mut chars = line.chars();
            let flag = chars.next();
            if chars.next() != Some('\t') {
                continue; // "To <url>" / "Done" framing lines
            }
            match flag {
                Some('=') => return Ok(PushOutcome::UpToDate),
                Some(' ') | Some('+') | Some('*') => return Ok(PushOutcome::Pushed),
                Some('!') => {
                    // Reason text like "[rejected] (non-fast-forward)" or
                    // "[rejected] (fetch first)" — both spellings of the
                    // remote-moved race. Anything else (tag clobber, stale
                    // info, remote-side refusal) is a real failure the sync
                    // engine must not retry blindly.
                    let reason = line.rsplit('\t').next().unwrap_or(line);
                    if reason.contains("non-fast-forward") || reason.contains("fetch first") {
                        return Ok(PushOutcome::NonFastForward);
                    }
                    return Err(VcsError::CommandFailed {
                        op: "push".to_string(),
                        status: out.status.code(),
                        stderr: reason.trim().to_string(),
                    });
                }
                _ => continue,
            }
        }
        // No per-ref line at all: the push failed before negotiating refs
        // (unreachable remote, auth failure, held lock).
        if out.status.success() {
            return Err(VcsError::Parse(format!(
                "push --porcelain produced no per-ref status: {stdout:?}"
            )));
        }
        Err(classify_failure("push", &out))
    }
}

/// Reads the current branch name, or `None` when `HEAD` is detached (or
/// unborn but with no symbolic ref — normally an unborn `HEAD` still names its
/// branch, which `symbolic-ref` reports).
fn current_branch(path: &Path) -> Result<Option<String>, VcsError> {
    let out = git_output(
        path,
        &[],
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        false,
    )?;
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

/// Whether a rebase is in progress in the given git dir (its state directory
/// exists). The single source of truth for this check — used by both the
/// safe-state probe and reconcile's conflict classification.
fn rebase_in_progress(git_dir: &Path) -> bool {
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

/// Builds a literal git pathspec (`:(literal)<path>`) as an `OsString`, so
/// pathspec magic is disabled and a path containing a space, `*`, or a leading
/// `:` matches verbatim. Non-UTF-8 path bytes are preserved.
fn literal_pathspec(path: &Path) -> OsString {
    let mut spec = OsString::from(":(literal)");
    spec.push(path.as_os_str());
    spec
}

/// Whether two paths refer to the same directory, resolving symlinks (macOS
/// `/tmp` -> `/private/tmp`, for instance).
fn same_dir(a: &Path, b: &Path) -> Result<bool, VcsError> {
    let ca = std::fs::canonicalize(a).map_err(VcsError::Io)?;
    let cb = std::fs::canonicalize(b).map_err(VcsError::Io)?;
    Ok(ca == cb)
}

/// How many `notable` names a [`ChangeSummary`] retains: the subject renders
/// three, plus one more so renderers can tell "exactly three" from "more".
const NOTABLE_CAP: usize = 4;

/// Parses NUL-delimited `git diff --cached --name-status -z` output into a
/// [`ChangeSummary`].
///
/// Record shapes: `<status>\0<path>\0`, or `<status>\0<src>\0<dst>\0` for
/// renames/copies (status `R<score>`/`C<score>`), where the destination name
/// is what matters. NUL delimiting means unusual filenames (unicode, spaces,
/// newlines) arrive as raw bytes, never C-quoted.
fn parse_name_status(output: &str) -> ChangeSummary {
    let mut summary = ChangeSummary::default();
    let mut fields = output.split('\0');
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue; // trailing NUL
        }
        let Some(path) = fields.next() else { break };
        let kind = status.chars().next().unwrap_or('M');
        let name = match kind {
            // Renames and copies carry a second path: the destination.
            'R' | 'C' => match fields.next() {
                Some(dst) => dst,
                None => break,
            },
            _ => path,
        };
        match kind {
            // A copy introduces a new file, like an add.
            'A' | 'C' => summary.added += 1,
            'D' => summary.deleted += 1,
            // Modified, renamed, type-changed, unmerged, etc.
            _ => summary.changed += 1,
        }
        if summary.notable.len() < NOTABLE_CAP {
            let basename = name.rsplit('/').next().unwrap_or(name);
            summary.notable.push(basename.to_string());
        }
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

        snapshots.push(Snapshot {
            id: SnapshotId::new(hash.trim()),
            time,
            subject: subject.to_string(),
            trigger: trigger_from_str(trigger_raw),
        });
    }
    Ok(snapshots)
}

/// Whether a git stderr indicates a lock held by another process. Focused on
/// the index lock, but also catches git's generic "another process" message.
/// This layer only classifies the condition; it never deletes a lock.
fn is_lock_contention(stderr: &str) -> bool {
    (stderr.contains(".lock")
        && (stderr.contains("File exists") || stderr.contains("Unable to create")))
        || stderr.contains("Another git process seems to be running")
}

/// Builds a `git -C <repo>` command with the given `-c` config pins and args.
///
/// Always pins `LC_ALL=C` and `LANGUAGE=C`: outcome classification matches
/// git's English messages, which another locale would translate (proven to
/// break repository detection under `de_DE`). Network operations additionally
/// set `GIT_TERMINAL_PROMPT=0` so authentication failures error out rather
/// than blocking on a prompt.
fn git_command<I, S>(repo: &Path, configs: &[&str], args: I, network: bool) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    for config in configs {
        cmd.arg("-c").arg(config);
    }
    cmd.args(args);
    cmd.env("LC_ALL", "C");
    cmd.env("LANGUAGE", "C");
    if network {
        cmd.env("GIT_TERMINAL_PROMPT", "0");
    }
    cmd
}

/// Runs a git command to completion, capturing its output. Maps a missing `git`
/// binary to [`VcsError::GitNotFound`] and other spawn failures to
/// [`VcsError::Io`]; a non-zero exit is reported through the returned
/// [`Output`], not as an error, so callers can classify it.
fn git_output<I, S>(
    repo: &Path,
    configs: &[&str],
    args: I,
    network: bool,
) -> Result<Output, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_command(repo, configs, args, network)
        .output()
        .map_err(map_spawn_error)
}

/// Like [`git_output`], but writes `input` to the command's standard input.
fn git_output_stdin<I, S>(
    repo: &Path,
    configs: &[&str],
    args: I,
    input: &str,
) -> Result<Output, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    use std::io::Write;
    use std::process::Stdio;

    let mut child = git_command(repo, configs, args, false)
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

/// Resolves `HEAD` to its full hash in `repo` (used for the scratch worktree,
/// where `self.rev_of` — bound to the main path — does not apply).
fn head_of(repo: &Path) -> Result<String, VcsError> {
    let out = git_output(repo, &[], ["rev-parse", "HEAD"], false)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(classify_failure("rev-parse", &out))
    }
}

/// The absolute git directory for `repo`. In a linked worktree this is that
/// worktree's private directory (`.git/worktrees/<name>`), where its
/// in-progress-operation markers live.
fn absolute_git_dir(repo: &Path) -> Result<PathBuf, VcsError> {
    let out = git_output(repo, &[], ["rev-parse", "--absolute-git-dir"], false)?;
    if out.status.success() {
        Ok(PathBuf::from(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    } else {
        Err(classify_failure("rev-parse", &out))
    }
}

/// How often [`git_output_timed`] polls a running git child for completion.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Runs a network git command under a wall-clock `timeout`.
///
/// On expiry the child is killed — and, on unix, its whole process group, so an
/// ssh transport's own children die with it rather than leaking as zombies —
/// and [`VcsError::Timeout`] is returned. The child is put in its own process
/// group up front (`process_group(0)`) so the group signal targets exactly its
/// descendants. stdout and stderr are drained on background threads so a chatty
/// git that fills a pipe buffer cannot deadlock against our own wait.
fn git_output_timed<I, S>(
    repo: &Path,
    configs: &[&str],
    args: I,
    op: &str,
    timeout: Duration,
) -> Result<Output, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = git_command(repo, configs, args, true);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group led by the child, so a group-directed kill reaches
        // the whole transport subtree and nothing else.
        cmd.process_group(0);
    }

    let start = Instant::now();
    let mut child = cmd.spawn().map_err(map_spawn_error)?;

    // Drain both pipes concurrently; otherwise a full pipe buffer would wedge
    // git while we poll for its exit.
    let mut child_stdout = child.stdout.take().expect("stdout was piped");
    let mut child_stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = child_stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = child_stderr.read_to_end(&mut buf);
        buf
    });

    let status = loop {
        match child.try_wait().map_err(VcsError::Io)? {
            Some(status) => break status,
            None => {
                if start.elapsed() >= timeout {
                    let elapsed = start.elapsed();
                    kill_process_tree(&mut child);
                    // Reap the child so no zombie is left; the kill makes this
                    // return promptly. The readers see EOF once the pipes close.
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(VcsError::Timeout {
                        op: op.to_string(),
                        elapsed,
                    });
                }
                std::thread::sleep(KILL_POLL_INTERVAL);
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Kills a timed-out git child and, on unix, its whole process group (the child
/// leads its own group, set via `process_group(0)` in [`git_output_timed`]), so
/// transport helpers such as ssh die with it. On non-unix only the child itself
/// is killed (there is no portable process-group signal), so a transport it
/// spawned may briefly outlive it.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Signal the process group led by the child (pgid == child pid).
        // Best-effort: if the group is already gone the error is irrelevant.
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_child(child),
            rustix::process::Signal::KILL,
        );
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

/// Runs a git command expected to succeed, returning its stdout as a string.
/// A non-zero exit is classified via [`classify_failure`], with the op label
/// derived from the subcommand (the first argument).
fn checked<I, S>(repo: &Path, configs: &[&str], args: I, network: bool) -> Result<String, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let op = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    let out = git_output(repo, configs, &args, network)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(classify_failure(&op, &out))
    }
}

/// Classifies a finished, failed git command into a [`VcsError`]: a held lock
/// becomes [`VcsError::LockContended`] (attributed to `op`), anything else
/// [`VcsError::CommandFailed`]. The single classification point, so lock
/// detection cannot drift between call sites.
fn classify_failure(op: &str, out: &Output) -> VcsError {
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_lock_contention(&stderr) {
        VcsError::LockContended { op: op.to_string() }
    } else {
        VcsError::CommandFailed {
            op: op.to_string(),
            status: out.status.code(),
            stderr: stderr.trim().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_status_counts_by_disposition() {
        // Delete, modify, add (with unicode + space), rename.
        let output =
            "D\0gone.txt\0M\0src/keep.rs\0A\0naïve file.txt\0R100\0old.rs\0sub/new name.rs\0";
        let summary = parse_name_status(output);
        assert_eq!(summary.changed, 2); // modify + rename
        assert_eq!(summary.added, 1);
        assert_eq!(summary.deleted, 1);
        assert_eq!(
            summary.notable,
            vec!["gone.txt", "keep.rs", "naïve file.txt", "new name.rs"]
        );
    }

    #[test]
    fn parse_name_status_caps_notable_but_counts_everything() {
        let output = "A\0a\0A\0b\0A\0c\0A\0d\0A\0e\0A\0f\0";
        let summary = parse_name_status(output);
        assert_eq!(summary.added, 6);
        assert_eq!(summary.notable.len(), NOTABLE_CAP);
        assert_eq!(summary.notable, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn parse_name_status_of_empty_output_is_no_changes() {
        let summary = parse_name_status("");
        assert_eq!(summary.total(), 0);
        assert!(summary.notable.is_empty());
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
}
