//! The version-control seam: the [`VcsBackend`] trait, the value types that
//! flow across it, and the [`CommitMessage`] model rendered into snapshot
//! commits.
//!
//! This crate owns **correctness**, and the sharpest correctness concerns live
//! here: no VCS operation may destroy the only copy of anything, and vard only
//! ever commits into the watched repository's own configured branch — never
//! into a repository that is mid-operation (merging, rebasing, bisecting) or
//! sitting on some other branch. The trait encodes those rules as its
//! contract; [`git`] is the day-one implementation that shells out to the
//! `git` binary.
//!
//! # Why the trait is synchronous
//!
//! Shelling out to `git` is blocking work. The async engine (later tasks) wraps
//! every call in `spawn_blocking`, so the trait itself stays plain synchronous
//! `fn`s. That is also what keeps it **dyn-compatible**: the engine holds a
//! `Box<dyn VcsBackend>` per watch, which an `async fn` in the trait would
//! forbid.
//!
//! # Why construction lives on the concrete type
//!
//! Constructors that return `Self` cannot live on a dyn-compatible trait, so
//! [`GitBackend::detect`](git::GitBackend::detect),
//! [`GitBackend::init`](git::GitBackend::init), and
//! [`GitBackend::open`](git::GitBackend::open) are inherent methods on the
//! concrete backend. The trait is purely the per-watch operational surface.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::event::Trigger;

pub mod git;

/// The per-watch operational surface of a version-control backend.
///
/// Every method borrows `&self`, returns `Result<_, VcsError>`, and is
/// synchronous (see the [module docs](self) for why). The trait is
/// deliberately dyn-compatible so the engine can hold a `Box<dyn VcsBackend>`
/// per watch; constructors therefore live on the concrete backend type, not
/// here.
///
/// A backend is bound to one directory, one configured branch, and one
/// configured remote at construction. It never reads configuration itself —
/// those three values come from the watch's [`WatchSpec`](crate::WatchSpec) at
/// the call site.
pub trait VcsBackend {
    /// Reports whether the repository is in a state where vard may safely
    /// commit, or why it is not (see [`SafeState`] and [`UnsafeReason`]).
    ///
    /// This is the guard behind the rule that vard commits only on its one
    /// configured branch and never into a repository that is mid-operation:
    /// an in-progress merge/cherry-pick/revert/bisect/rebase, a detached
    /// `HEAD`, or a `HEAD` on the wrong branch all report `Unsafe`.
    fn is_safe_state(&self) -> Result<SafeState, VcsError>;

    /// Reports whether the work tree holds uncommitted changes a
    /// [`snapshot`](Self::snapshot) would capture — staged, unstaged, or
    /// untracked-and-not-ignored files alike (the same `git add -A` sweep the
    /// snapshot performs). A clean tree returns `false`.
    ///
    /// This is a cheap, non-network status probe. The sync cycle consults it so
    /// that a watch with local edits but an unmoved remote is not mistaken for
    /// "nothing to do": a dirty tree must proceed into the locked window, where
    /// the pre-sync snapshot commits it before it can be pushed.
    fn is_dirty(&self) -> Result<bool, VcsError>;

    /// Sweeps the whole work tree and commits it as one snapshot, returning
    /// the new snapshot's id and a summary of what it contains — or `None`
    /// when nothing changed.
    ///
    /// The sweep is intentionally total (`git add -A`): vard snapshots the
    /// directory as a whole, not a curated index. **Any index state a user
    /// staged by hand is deliberately swept into the snapshot** — the watched
    /// directory is modeled as a dedicated repository that vard alone commits
    /// to, so a curated index has no meaning here. The summary in the returned
    /// [`SnapshotOutcome`] is computed from the staged diff *after* the sweep,
    /// so it describes exactly what the commit contains. When the sweep leaves
    /// no staged difference, no commit is made and `None` is returned; an
    /// empty commit is never forced.
    ///
    /// The backend re-checks [`is_safe_state`](Self::is_safe_state) before
    /// committing and returns [`VcsError::UnsafeState`] if the repository is
    /// no longer safe. A window between that check and the commit remains
    /// (another process can start an operation in it); the engine's
    /// serialization of operations per watch narrows but cannot close it.
    fn snapshot(&self, req: &SnapshotRequest) -> Result<Option<SnapshotOutcome>, VcsError>;

    /// Lists snapshots most-recent-first, filtered by [`LogFilter`].
    ///
    /// Each [`Snapshot`] carries the `Vard-Trigger` trailer parsed back into a
    /// [`Trigger`] when present (and `None` when absent or unrecognized).
    /// Extra trailers written via [`SnapshotRequest::extra_trailers`] are not
    /// parsed back out. A repository with no commits yet returns an empty
    /// vector, not an error.
    fn log(&self, filter: &LogFilter) -> Result<Vec<Snapshot>, VcsError>;

    /// Returns the raw unified diff between two references, or between one
    /// reference and the current work tree when `to` is `None`.
    ///
    /// When `pathspec` is `Some`, the diff is scoped to that single path,
    /// matched as a literal (git pathspec magic disabled) so paths with spaces
    /// or leading `:` are handled verbatim — the same literal matching the
    /// scoped [`restore`](Self::restore) uses, so a `--file` diff preview and
    /// the restore it previews agree exactly.
    fn diff(
        &self,
        from: &VcsRef,
        to: Option<&VcsRef>,
        pathspec: Option<&std::path::Path>,
    ) -> Result<String, VcsError>;

    /// Reports whether `rev` resolves to a commit in this repository, without
    /// side effects. Used to validate a caller-supplied `--ref` *before* a
    /// destructive operation takes any protective snapshot, so a typo fails
    /// cleanly with nothing changed.
    fn verify_ref(&self, rev: &VcsRef) -> Result<bool, VcsError>;

    /// Reports whether `path` exists at `rev` (as a tracked blob or tree).
    /// Lets a `--file` restore preview pre-check exactly what the real restore
    /// checks, so both agree on a path absent at the chosen revision.
    fn path_exists_at(&self, rev: &VcsRef, path: &std::path::Path) -> Result<bool, VcsError>;

    /// Restores the work tree (or a single path within it) to a prior
    /// reference.
    ///
    /// **WARNING: this overwrites uncommitted working-tree changes at the
    /// restored paths, and those changes are unrecoverable** — they exist in
    /// no commit, so no VCS mechanism can bring them back. Restoring never
    /// moves the branch ref or `HEAD`, so no *commit* can be lost, but "no
    /// commit can be lost" is not the whole story: the engine MUST take a
    /// protective snapshot of the current state before calling this. The
    /// backend does not take that snapshot itself.
    fn restore(&self, target: &RestoreTarget) -> Result<(), VcsError>;

    /// Fetches the configured branch from the configured remote and reports how
    /// local and remote now relate (see [`RemoteState`]).
    ///
    /// A branch that does not exist on the remote yet (nothing has been pushed)
    /// is a normal state, not an error: it reports as not-moved, zero behind,
    /// and ahead by however many local commits exist.
    ///
    /// `timeout` bounds the wall-clock time the fetch may take: on expiry the
    /// git child (and, on unix, its whole process group, so an ssh transport's
    /// children die with it) is killed and [`VcsError::Timeout`] is returned.
    /// The caller owns the policy — a hung network operation cannot block a
    /// worker forever. This is the only network-facing method here besides
    /// [`push`](Self::push); non-network operations stay unbounded.
    fn fetch(&self, timeout: Duration) -> Result<RemoteState, VcsError>;

    /// Reconciles the configured branch with the already-fetched upstream by
    /// rebasing **out of tree**, reporting the outcome (see
    /// [`ReconcileOutcome`]). Exactly one attempt; retry, backoff, and
    /// watch-state transitions are the sync engine's concern.
    ///
    /// # No dirty tree, ever
    ///
    /// This never touches the user's working tree and never moves the branch
    /// ref. It creates a vard-owned detached-`HEAD` linked worktree at
    /// `scratch` (`git worktree add --detach`), replays the branch's commits
    /// onto the upstream ref *inside that scratch worktree*, and returns
    /// [`ReconcileOutcome::Rebased`] carrying the rebased tip — a commit in the
    /// shared object store that the caller makes live with
    /// [`advance`](Self::advance). The branch and the user's tree stay
    /// bit-for-bit unchanged on every path.
    ///
    /// On conflict the scratch rebase is aborted and the scratch worktree
    /// removed, and [`ReconcileOutcome::Conflict`] is returned: because the
    /// rebase only ever ran inside the scratch worktree, the user's repository
    /// is provably untouched and no conflict markers can reach it. On
    /// [`ReconcileOutcome::AlreadyUpToDate`] nothing was replayed.
    ///
    /// `scratch` must be a path that does not yet exist; vard-core creates the
    /// linked worktree there and removes it before returning on every path
    /// (success, conflict, and error). vard-core resolves no paths itself —
    /// the caller owns where scratch lives (tests use a tempdir). A *crash*
    /// mid-reconcile can still leave a scratch worktree behind, possibly
    /// mid-rebase; [`prune_scratch`](Self::prune_scratch) reclaims it.
    ///
    /// Re-checks [`is_safe_state`](Self::is_safe_state) first and returns
    /// [`VcsError::UnsafeState`] if the repository is not safe; the same
    /// residual check-to-act window as [`snapshot`](Self::snapshot) applies.
    fn reconcile(&self, scratch: &Path) -> Result<ReconcileOutcome, VcsError>;

    /// Advances the user's tree and the configured branch to `target` — the
    /// single move that makes a [`reconcile`](Self::reconcile) result live —
    /// **without ever destroying uncommitted or unmerged work**, reporting
    /// whether it advanced or refused (see [`AdvanceOutcome`]).
    ///
    /// # No dirty tree, ever — safe by construction
    ///
    /// This does **not** hard-reset. It updates the branch and tree with safe
    /// checkout semantics (`git checkout -B <branch> <target>`), which git's own
    /// index locking makes race-free: a locally-modified tracked file, an
    /// untracked file, **or a locally-gitignored file** that `target` would
    /// clobber makes the checkout **refuse**, and this returns
    /// [`AdvanceOutcome::WouldClobber`] with the branch and tree left exactly as
    /// they were. (The ignored-file case is not git's default — the git backend
    /// passes `--no-overwrite-ignore` so a remote commit adding a path that is a
    /// local ignored file, never captured by the pre-sync snapshot, refuses
    /// rather than silently destroying the local copy.) Non-conflicting local
    /// edits are carried over unharmed. advance therefore never destroys
    /// uncommitted or gitignored work, period — the
    /// engine's per-watch lock plus its pre-sync snapshot make the common path a
    /// clean fast-forward, and any residual local change refuses rather than
    /// vanishes.
    ///
    /// `expected_tip` guards the **branch ref** the checkout would overwrite:
    /// `-B` moves the branch to `target`, so a commit the user landed on the
    /// branch after the reconcile read its tip would be stranded. This refuses
    /// with [`AdvanceOutcome::WouldClobber`] when the branch tip no longer equals
    /// `expected_tip` (the pre-reconcile tip the caller captured). The check runs
    /// immediately before the checkout inside the same call; a millisecond race
    /// between the check and the checkout remains, but the engine's op lock
    /// excludes all *vard* writers, so only a user racing their own commit into
    /// that window is exposed — a self-inflicted, reflog-recoverable case. The
    /// real exposure the guard closes is the seconds-long reconcile window.
    ///
    /// `target` is verified to exist before anything moves, so a bad id fails
    /// cleanly ([`VcsError::CommandFailed`]) with nothing changed. Idempotent:
    /// advancing to the current tip (with `expected_tip` equal to it) is a clean
    /// [`AdvanceOutcome::Advanced`] no-op. Re-checks
    /// [`is_safe_state`](Self::is_safe_state) first.
    fn advance(
        &self,
        target: &SnapshotId,
        expected_tip: &SnapshotId,
    ) -> Result<AdvanceOutcome, VcsError>;

    /// Reports whether the backend's configured remote is defined in this
    /// repository, as a cheap **non-network** config lookup
    /// (`git config remote.<name>.url`) — it never contacts the remote.
    ///
    /// The sync engine's host probes this at build/injection time so a
    /// `sync = true` watch whose repository has no such remote is left with sync
    /// disabled (one log line, no state change) rather than latching a
    /// [`VcsError`]/`SyncError` storm on every doomed fetch. The default returns
    /// `Ok(true)` for backends with no notion of a remote (test doubles); the git
    /// backend performs the real lookup.
    fn has_remote(&self) -> Result<bool, VcsError> {
        Ok(true)
    }

    /// How the local branch relates to its remote-tracking ref **right now** —
    /// `(ahead, behind)` as a cheap, local-only read (no network; the tracking
    /// ref reflects the last fetch, and a manual `git push` updates it too).
    ///
    /// The sync engine uses it twice: the gate-free push resolves its commit
    /// count at push time (so commits landing after the cycle's fetch are
    /// counted), and a gate-busy wait re-derives whether locked work is still
    /// needed without any network — a user who hand-resolved mid-wait
    /// (committed and pushed manually) reads as clean and `(0, 0)` here, so
    /// the request terminates up-to-date instead of waiting on a lock it no
    /// longer needs.
    ///
    /// Returns `Ok(None)` when the backend has no such notion (the default);
    /// callers must then fall back to their fetch-time knowledge. The values
    /// are advisory (counts, wait short-circuits) — never a correctness gate.
    fn upstream_status(&self) -> Result<Option<(usize, usize)>, VcsError> {
        Ok(None)
    }

    /// Force-removes a leftover scratch worktree at `scratch` and prunes stale
    /// worktree metadata (`git worktree remove --force` then
    /// `git worktree prune`).
    ///
    /// This is crash recovery. [`reconcile`](Self::reconcile) removes its
    /// scratch worktree on every normal path, but a crash mid-reconcile can
    /// leave one behind (possibly mid-rebase, its dir on disk or already gone).
    /// Calling this is always safe: a `scratch` that is not — or is no longer —
    /// a registered worktree is a clean no-op. It operates only on vard's
    /// scratch worktree, never on the user's files.
    fn prune_scratch(&self, scratch: &Path) -> Result<(), VcsError>;

    /// Pushes the configured branch to the configured remote (see
    /// [`PushOutcome`]).
    ///
    /// A non-fast-forward rejection is a normal [`PushOutcome::NonFastForward`]
    /// result, not an error; resolving the race is the sync engine's job. Any
    /// other rejection (for example the remote refusing an update to a
    /// checked-out branch) is a [`VcsError::CommandFailed`].
    ///
    /// `timeout` bounds the push exactly as it bounds [`fetch`](Self::fetch):
    /// on expiry the git child and its process group are killed and
    /// [`VcsError::Timeout`] is returned.
    fn push(&self, timeout: Duration) -> Result<PushOutcome, VcsError>;
}

/// What [`VcsBackend::snapshot`] should commit: why, with what optional user
/// text, and with which extra trailers.
///
/// The backend computes the change summary itself from what it actually
/// stages, so the request carries only the caller's intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRequest {
    /// Why the snapshot is being taken; rendered as the `Vard-Trigger` trailer.
    pub trigger: Trigger,
    /// Optional free-form text, rendered as its own paragraph in the message.
    pub user_text: Option<String>,
    /// Additional `Key: value` trailers rendered after `Vard-Trigger` (for
    /// example a `Vard-Host` trailer identifying the machine).
    pub extra_trailers: Vec<(String, String)>,
}

impl SnapshotRequest {
    /// A request with the given trigger and no user text or extra trailers.
    pub fn new(trigger: Trigger) -> Self {
        Self {
            trigger,
            user_text: None,
            extra_trailers: Vec::new(),
        }
    }
}

/// What [`VcsBackend::snapshot`] produced: the new snapshot's id and a summary
/// of exactly what was committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotOutcome {
    /// The id of the commit that was created.
    pub id: SnapshotId,
    /// The change summary of the committed content, computed from the staged
    /// diff immediately before committing.
    pub summary: ChangeSummary,
}

/// Whether the repository is safe for vard to commit into, per
/// [`VcsBackend::is_safe_state`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SafeState {
    /// The repository is on its configured branch with no operation in
    /// progress; vard may commit.
    Safe,
    /// The repository must not be committed into, for the given reason.
    Unsafe(UnsafeReason),
}

/// Why a repository is not safe to commit into (see [`SafeState::Unsafe`]).
///
/// The in-progress-operation variants are keyed off the marker files and
/// directories git leaves in the repository's git dir while an operation is
/// underway, and the `HEAD` variants off the current branch versus the
/// backend's configured branch.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsafeReason {
    /// A merge is in progress (`MERGE_HEAD` present).
    MergeInProgress,
    /// A cherry-pick is in progress (`CHERRY_PICK_HEAD` present).
    CherryPickInProgress,
    /// A revert is in progress (`REVERT_HEAD` present).
    RevertInProgress,
    /// A bisect is in progress (`BISECT_LOG` present).
    BisectInProgress,
    /// A rebase is in progress (`rebase-merge/` or `rebase-apply/` present).
    RebaseInProgress,
    /// `HEAD` is detached — not on any branch.
    DetachedHead,
    /// `HEAD` is on a branch other than the backend's configured branch.
    WrongBranch {
        /// The branch the backend is configured to commit to.
        expected: String,
        /// The branch `HEAD` is actually on.
        actual: String,
    },
}

impl fmt::Display for UnsafeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnsafeReason::MergeInProgress => f.write_str("a merge is in progress"),
            UnsafeReason::CherryPickInProgress => f.write_str("a cherry-pick is in progress"),
            UnsafeReason::RevertInProgress => f.write_str("a revert is in progress"),
            UnsafeReason::BisectInProgress => f.write_str("a bisect is in progress"),
            UnsafeReason::RebaseInProgress => f.write_str("a rebase is in progress"),
            UnsafeReason::DetachedHead => f.write_str("HEAD is detached"),
            UnsafeReason::WrongBranch { expected, actual } => write!(
                f,
                "HEAD is on branch {actual:?}, not the configured branch {expected:?}"
            ),
        }
    }
}

/// The id of a committed snapshot (a git commit hash for the git backend).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Wraps a raw id string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A reference to a point in history: any revision spelling the backend
/// understands (a [`SnapshotId`], branch name, tag, `HEAD~3`, and so on).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VcsRef(String);

impl VcsRef {
    /// Wraps a revision string.
    pub fn new(rev: impl Into<String>) -> Self {
        Self(rev.into())
    }

    /// The revision as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VcsRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&SnapshotId> for VcsRef {
    fn from(id: &SnapshotId) -> Self {
        VcsRef(id.0.clone())
    }
}

/// One entry returned by [`VcsBackend::log`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The snapshot's id.
    pub id: SnapshotId,
    /// When the snapshot was committed.
    pub time: SystemTime,
    /// The commit subject line.
    pub subject: String,
    /// The trigger parsed from the `Vard-Trigger` trailer, if present and
    /// recognized.
    pub trigger: Option<Trigger>,
}

/// Filters which snapshots [`VcsBackend::log`] returns.
///
/// Both bounds are optional and independent. `limit` caps the count, taking
/// the most recent.
///
/// # `since` boundary semantics
///
/// `since` maps to git's `--since` cutoff, whose observed behavior is
/// *inclusive* at the boundary: a snapshot committed at exactly `since` is
/// returned (a regression test pins this). Git applies the cutoff by walking
/// history from the tip and stopping at the first commit past it, which for
/// vard's linear snapshot history behaves as a plain time filter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// Keep only snapshots committed at or after this time (see the type docs
    /// for the exact boundary behavior).
    pub since: Option<SystemTime>,
    /// Keep only snapshots committed at or before this time (git's `--until`,
    /// inclusive at the boundary like `since`). Combined with `limit = Some(1)`
    /// this asks git directly for "the newest snapshot as of a past instant"
    /// rather than fetching the whole history and scanning it.
    pub until: Option<SystemTime>,
    /// Keep at most this many of the most-recent snapshots.
    pub limit: Option<usize>,
}

/// What to restore, for [`VcsBackend::restore`].
///
/// With `path` set, only that single path is restored; with `path` `None`, the
/// whole work tree is restored to `rev`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreTarget {
    /// The reference to restore from.
    pub rev: VcsRef,
    /// The single path to restore, or `None` to restore the whole work tree.
    pub path: Option<PathBuf>,
}

/// How the local branch relates to its upstream after a [`VcsBackend::fetch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteState {
    /// The remote-tracking ref changed as a result of the fetch (new upstream
    /// commits, or the upstream appeared for the first time).
    pub remote_moved: bool,
    /// How many commits the local branch is ahead of the upstream (all of
    /// them, when the branch does not exist on the remote yet).
    pub ahead: usize,
    /// How many commits the local branch is behind the upstream.
    pub behind: usize,
}

/// The result of a single [`VcsBackend::reconcile`] attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// The branch already contained the upstream; nothing was replayed.
    AlreadyUpToDate,
    /// The branch was rebased onto the upstream and now points at `new_head`.
    ///
    /// The pre-rebase commits are no longer reachable from the branch; they
    /// survive only via the reflog and `ORIG_HEAD`, both of which git retains
    /// for a limited time (reflog entries expire, 90 days by default). A
    /// caller that needs the pre-rebase tip durably must record it before the
    /// reflog ages out.
    Rebased {
        /// The branch's new tip after the rebase.
        new_head: SnapshotId,
    },
    /// The rebase hit a conflict and was aborted; the branch is unchanged and
    /// the tree contains no conflict markers.
    Conflict,
}

/// The result of a [`VcsBackend::advance`]: either the branch and tree advanced,
/// or the move was **refused** because it would have overwritten uncommitted or
/// unmerged work (a locally-modified or untracked file the target would clobber,
/// or a branch tip that moved out from under the reconcile). A refusal leaves the
/// repository exactly as it was; it is never an error, and the sync engine treats
/// it as "abandon this cycle and retry", not as a sync failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvanceOutcome {
    /// The branch and work tree now point at the reconcile target.
    Advanced,
    /// The move was refused to protect uncommitted work or a moved branch tip;
    /// nothing changed.
    WouldClobber,
}

/// The result of a [`VcsBackend::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// Local commits were pushed to the remote.
    Pushed,
    /// The remote already had everything; nothing was pushed.
    UpToDate,
    /// The push was rejected because it was not a fast-forward (the remote
    /// moved). This is a normal outcome, not an error.
    NonFastForward,
}

/// A tally of what a snapshot contains, used to render its commit subject.
///
/// The counts are of files by disposition; `notable` holds changed file names
/// (base names), of which the subject shows up to the first three. The git
/// backend computes this from the staged diff immediately before committing,
/// so it describes exactly what the commit contains; `notable` is capped at
/// four entries (three shown plus one to prove truncation) while the counts
/// cover every file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSummary {
    /// Number of modified files.
    pub changed: usize,
    /// Number of newly added files.
    pub added: usize,
    /// Number of deleted files.
    pub deleted: usize,
    /// Names of changed files, in the order the subject should show them; only
    /// the first three appear, followed by an ellipsis when there are more.
    pub notable: Vec<String>,
}

impl ChangeSummary {
    /// Total number of files in the summary.
    pub fn total(&self) -> usize {
        self.changed + self.added + self.deleted
    }
}

/// The rendered model of a snapshot commit message.
///
/// Built via [`CommitMessage::new`] from a [`ChangeSummary`], the [`Trigger`]
/// that caused the snapshot, optional free-form user text, and any extra
/// trailers. [`render`] turns it into the exact commit-message bytes: a
/// `snapshot:` subject line summarizing the change, the optional user text as
/// its own paragraph, a `Vard-Trigger` trailer, then the extra trailers in
/// order.
///
/// [`render`]: CommitMessage::render
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMessage {
    summary: ChangeSummary,
    trigger: Trigger,
    user_text: Option<String>,
    extra_trailers: Vec<(String, String)>,
}

impl CommitMessage {
    /// Builds a commit message from a change summary, the causing trigger,
    /// optional user text, and extra trailers rendered after `Vard-Trigger`.
    pub fn new(
        summary: ChangeSummary,
        trigger: Trigger,
        user_text: Option<&str>,
        extra_trailers: Vec<(String, String)>,
    ) -> Self {
        Self {
            summary,
            trigger,
            user_text: user_text.map(str::to_string),
            extra_trailers,
        }
    }

    /// The trigger that caused this snapshot.
    pub fn trigger(&self) -> Trigger {
        self.trigger
    }

    /// Renders the subject line, e.g. `snapshot: 4 changed, 1 added (zshrc, gitconfig, …)`.
    ///
    /// Zero counts are omitted; up to three notable file names are shown, and a
    /// trailing `…` follows when there are more than three. When nothing was
    /// counted and nothing is notable, the subject is a bare `snapshot:`
    /// summary of `no changes` (the backend does not commit in that case, so
    /// this only surfaces when a caller renders a summary directly).
    pub fn subject(&self) -> String {
        let s = &self.summary;
        let mut parts = Vec::new();
        if s.changed > 0 {
            parts.push(format!("{} changed", s.changed));
        }
        if s.added > 0 {
            parts.push(format!("{} added", s.added));
        }
        if s.deleted > 0 {
            parts.push(format!("{} deleted", s.deleted));
        }
        let counts = if parts.is_empty() {
            "no changes".to_string()
        } else {
            parts.join(", ")
        };

        let mut subject = format!("snapshot: {counts}");
        if !s.notable.is_empty() {
            let names: Vec<&str> = s.notable.iter().take(3).map(String::as_str).collect();
            let mut list = names.join(", ");
            if s.notable.len() > 3 {
                // Keep the ellipsis a distinct token so the subject reads
                // "gitconfig, …" rather than gluing it onto a name.
                list.push_str(", …");
            }
            subject.push_str(&format!(" ({list})"));
        }
        subject
    }

    /// Renders the full commit message: subject, optional user-text paragraph,
    /// the `Vard-Trigger` trailer, then the extra trailers in order.
    ///
    /// The `Vard-Trigger` value is the [`Trigger`]'s `Display` spelling, so it
    /// round-trips back through [`VcsBackend::log`].
    pub fn render(&self) -> String {
        let mut out = self.subject();
        out.push_str("\n\n");
        if let Some(text) = &self.user_text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push_str(trimmed);
                out.push_str("\n\n");
            }
        }
        out.push_str(TRAILER_KEY);
        out.push_str(": ");
        out.push_str(&self.trigger.to_string());
        out.push('\n');
        for (key, value) in &self.extra_trailers {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        out
    }
}

/// The commit trailer key carrying the snapshot's trigger.
pub(crate) const TRAILER_KEY: &str = "Vard-Trigger";

/// Parses a `Vard-Trigger` trailer value back into a [`Trigger`].
///
/// Returns `None` for an unrecognized value. The spellings mirror
/// [`Trigger`]'s `Display`; keep them in step when the vocabulary grows (a
/// round-trip test over every variant guards this).
pub(crate) fn trigger_from_str(value: &str) -> Option<Trigger> {
    match value.trim() {
        "event" => Some(Trigger::Event),
        "interval" => Some(Trigger::Interval),
        "manual" => Some(Trigger::Manual),
        "pre-restore" => Some(Trigger::PreRestore),
        "pre-sync" => Some(Trigger::PreSync),
        _ => None,
    }
}

/// Everything that can go wrong in a VCS operation.
///
/// Hand-rolled `Display` + `Error` with no error-crate dependency, matching
/// [`ConfigError`](crate::ConfigError). The variants a caller must be able to
/// tell apart are distinct: a missing git binary, a contended lock (which the
/// engine — not this layer — retries), an unsafe repository state, a path
/// that is not a repository (or not its root), a repository left mid-rebase,
/// and the catch-alls for a non-zero git exit, I/O, and output parsing.
#[derive(Debug)]
#[non_exhaustive]
pub enum VcsError {
    /// The `git` binary could not be found on the system.
    GitNotFound,
    /// A git index (or ref) lock was held by another process. The engine owns
    /// the retry/backoff policy; this layer only classifies the condition and
    /// never touches the lock file.
    LockContended {
        /// A short label for the operation that hit the lock (e.g. `"commit"`),
        /// so the engine can attribute retries.
        op: String,
    },
    /// The repository was not in a safe state for the attempted operation
    /// (see [`VcsBackend::is_safe_state`]).
    UnsafeState(UnsafeReason),
    /// A git command exited non-zero for a reason that is not a recognized,
    /// normal outcome.
    CommandFailed {
        /// A short label for the operation that failed (e.g. `"commit"`).
        op: String,
        /// The process exit code, if one was returned.
        status: Option<i32>,
        /// The trimmed contents of the command's standard error.
        stderr: String,
    },
    /// The path is not inside a git repository.
    NotARepo,
    /// The path is inside a git repository, but is not that repository's root.
    /// The backend only operates on repositories rooted at the watch path.
    NotRepoRoot {
        /// The path that was opened.
        path: PathBuf,
        /// The root of the repository that actually contains it.
        root: PathBuf,
    },
    /// A conflicted rebase could not be aborted and the repository is left
    /// mid-rebase, needing human (or doctor-tool) attention. Dormant since
    /// reconciliation moved out of the working tree ([`VcsBackend::reconcile`]
    /// rebases in a scratch worktree, where a failed abort is absorbed by the
    /// worktree's forced removal); no production path constructs it today.
    /// Kept public for API stability while the trait is pre-1.0.
    RepoLeftInRebase {
        /// Why `rebase --abort` failed.
        source: Box<VcsError>,
    },
    /// A network operation (a [`fetch`](VcsBackend::fetch) or
    /// [`push`](VcsBackend::push)) exceeded its caller-supplied timeout and its
    /// git child — along with its whole process group on unix, so an ssh
    /// transport's children die with it — was killed. Distinct from
    /// [`CommandFailed`](Self::CommandFailed) so a hung endpoint is retried on
    /// its own schedule rather than treated as a hard git error.
    Timeout {
        /// A short label for the operation that timed out (`"fetch"` or
        /// `"push"`).
        op: String,
        /// How long the operation ran before it was killed.
        elapsed: Duration,
    },
    /// An I/O error occurred spawning or communicating with git.
    Io(std::io::Error),
    /// Git's output could not be parsed as expected.
    Parse(String),
}

impl fmt::Display for VcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VcsError::GitNotFound => f.write_str("the git executable could not be found on PATH"),
            VcsError::LockContended { op } => {
                write!(
                    f,
                    "git {op}: a git lock is held by another process; retry later"
                )
            }
            VcsError::UnsafeState(reason) => {
                write!(f, "repository is not in a safe state: {reason}")
            }
            VcsError::CommandFailed { op, status, stderr } => {
                match status {
                    Some(code) => write!(f, "git {op} failed (exit {code})")?,
                    None => write!(f, "git {op} failed (terminated by signal)")?,
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            VcsError::NotARepo => f.write_str("path is not inside a git repository"),
            VcsError::NotRepoRoot { path, root } => write!(
                f,
                "path {} is inside a repository rooted at {}, not at the path itself",
                path.display(),
                root.display()
            ),
            VcsError::RepoLeftInRebase { source } => write!(
                f,
                "a conflicted rebase could not be aborted and the repository \
                 is left mid-rebase; manual attention required: {source}"
            ),
            VcsError::Timeout { op, elapsed } => write!(
                f,
                "git {op} exceeded its timeout after {elapsed:.1?} and was killed"
            ),
            VcsError::Io(e) => write!(f, "git I/O error: {e}"),
            VcsError::Parse(msg) => write!(f, "could not parse git output: {msg}"),
        }
    }
}

impl Error for VcsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            VcsError::Io(e) => Some(e),
            VcsError::RepoLeftInRebase { source } => Some(source.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(changed: usize, added: usize, deleted: usize, notable: &[&str]) -> ChangeSummary {
        ChangeSummary {
            changed,
            added,
            deleted,
            notable: notable.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn message(summary: ChangeSummary, trigger: Trigger, user_text: Option<&str>) -> CommitMessage {
        CommitMessage::new(summary, trigger, user_text, Vec::new())
    }

    #[test]
    fn subject_renders_all_counts_and_notable_files() {
        let msg = message(
            summary(4, 1, 2, &["zshrc", "gitconfig", "vimrc"]),
            Trigger::Event,
            None,
        );
        assert_eq!(
            msg.subject(),
            "snapshot: 4 changed, 1 added, 2 deleted (zshrc, gitconfig, vimrc)"
        );
    }

    #[test]
    fn subject_omits_zero_counts() {
        let msg = message(summary(0, 3, 0, &["a", "b"]), Trigger::Interval, None);
        assert_eq!(msg.subject(), "snapshot: 3 added (a, b)");

        let msg = message(summary(5, 0, 0, &["only"]), Trigger::Manual, None);
        assert_eq!(msg.subject(), "snapshot: 5 changed (only)");
    }

    #[test]
    fn subject_does_not_pluralize_the_disposition_words() {
        // The words are invariant regardless of count: "1 changed", not
        // "1 change" or "1 changeds".
        let one = message(summary(1, 1, 1, &["x"]), Trigger::Manual, None);
        assert_eq!(one.subject(), "snapshot: 1 changed, 1 added, 1 deleted (x)");
    }

    #[test]
    fn subject_truncates_notable_to_three_with_ellipsis() {
        let msg = message(
            summary(9, 0, 0, &["a", "b", "c", "d"]),
            Trigger::Event,
            None,
        );
        assert_eq!(msg.subject(), "snapshot: 9 changed (a, b, c, …)");

        // Exactly three shows all three with no ellipsis.
        let three = message(summary(3, 0, 0, &["a", "b", "c"]), Trigger::Event, None);
        assert_eq!(three.subject(), "snapshot: 3 changed (a, b, c)");
    }

    #[test]
    fn subject_without_notable_has_no_parens() {
        let msg = message(summary(2, 0, 0, &[]), Trigger::Event, None);
        assert_eq!(msg.subject(), "snapshot: 2 changed");
    }

    #[test]
    fn subject_with_no_changes_reads_no_changes() {
        let msg = message(summary(0, 0, 0, &[]), Trigger::Manual, None);
        assert_eq!(msg.subject(), "snapshot: no changes");
    }

    #[test]
    fn render_places_trailer_last_with_no_user_text() {
        let msg = message(summary(1, 0, 0, &["f"]), Trigger::Event, None);
        assert_eq!(
            msg.render(),
            "snapshot: 1 changed (f)\n\nVard-Trigger: event\n"
        );
    }

    #[test]
    fn render_inserts_user_text_as_its_own_paragraph() {
        let msg = message(
            summary(1, 0, 0, &["f"]),
            Trigger::Manual,
            Some("checkpoint before the demo"),
        );
        assert_eq!(
            msg.render(),
            "snapshot: 1 changed (f)\n\ncheckpoint before the demo\n\nVard-Trigger: manual\n"
        );
    }

    #[test]
    fn render_ignores_blank_user_text() {
        let msg = message(summary(1, 0, 0, &["f"]), Trigger::Manual, Some("   \n  "));
        assert_eq!(
            msg.render(),
            "snapshot: 1 changed (f)\n\nVard-Trigger: manual\n"
        );
    }

    #[test]
    fn render_places_extra_trailers_after_vard_trigger() {
        let msg = CommitMessage::new(
            summary(1, 0, 0, &["f"]),
            Trigger::PreSync,
            None,
            vec![
                ("Vard-Host".to_string(), "laptop".to_string()),
                ("Vard-Session".to_string(), "abc".to_string()),
            ],
        );
        assert_eq!(
            msg.render(),
            "snapshot: 1 changed (f)\n\nVard-Trigger: pre-sync\nVard-Host: laptop\nVard-Session: abc\n"
        );
    }

    #[test]
    fn render_spells_the_trailer_for_every_trigger_variant() {
        let cases = [
            (Trigger::Event, "event"),
            (Trigger::Interval, "interval"),
            (Trigger::Manual, "manual"),
            (Trigger::PreRestore, "pre-restore"),
            (Trigger::PreSync, "pre-sync"),
        ];
        for (trigger, spelling) in cases {
            let msg = message(summary(1, 0, 0, &["f"]), trigger, None);
            let rendered = msg.render();
            let expected = format!("Vard-Trigger: {spelling}\n");
            assert!(
                rendered.ends_with(&expected),
                "trigger {trigger:?} rendered {rendered:?}, expected it to end with {expected:?}"
            );
        }
    }

    #[test]
    fn trigger_trailer_round_trips_every_variant() {
        // Compile-time exhaustiveness guard: adding a Trigger variant breaks
        // this match, forcing the list below (and trigger_from_str, whose
        // wildcard arm would otherwise silently swallow the new spelling) to
        // be extended together.
        fn covered(t: Trigger) {
            match t {
                Trigger::Event => {}
                Trigger::Interval => {}
                Trigger::Manual => {}
                Trigger::PreRestore => {}
                Trigger::PreSync => {}
            }
        }
        let all = [
            Trigger::Event,
            Trigger::Interval,
            Trigger::Manual,
            Trigger::PreRestore,
            Trigger::PreSync,
        ];
        for trigger in all {
            covered(trigger);
            assert_eq!(
                trigger_from_str(&trigger.to_string()),
                Some(trigger),
                "Display spelling of {trigger:?} must parse back to itself"
            );
        }
    }

    #[test]
    fn trigger_from_str_rejects_unknown_values() {
        assert_eq!(trigger_from_str("bogus"), None);
        assert_eq!(trigger_from_str(""), None);
    }

    #[test]
    fn unsafe_reason_display_is_human_readable() {
        assert_eq!(
            UnsafeReason::MergeInProgress.to_string(),
            "a merge is in progress"
        );
        assert_eq!(
            UnsafeReason::WrongBranch {
                expected: "main".to_string(),
                actual: "feature".to_string(),
            }
            .to_string(),
            "HEAD is on branch \"feature\", not the configured branch \"main\""
        );
    }

    #[test]
    fn vcs_error_display_includes_op_and_stderr() {
        let e = VcsError::CommandFailed {
            op: "commit".to_string(),
            status: Some(128),
            stderr: "nothing to commit".to_string(),
        };
        assert_eq!(
            e.to_string(),
            "git commit failed (exit 128): nothing to commit"
        );

        let lock = VcsError::LockContended {
            op: "commit".to_string(),
        };
        assert!(lock.to_string().contains("commit"));
    }

    #[test]
    fn repo_left_in_rebase_chains_its_source() {
        let source = VcsError::CommandFailed {
            op: "rebase --abort".to_string(),
            status: Some(128),
            stderr: "disk full".to_string(),
        };
        let e = VcsError::RepoLeftInRebase {
            source: Box::new(source),
        };
        assert!(e.to_string().contains("mid-rebase"));
        assert!(Error::source(&e).is_some());
    }

    #[test]
    fn backend_is_dyn_compatible() {
        // Compile-time proof that the trait can be used as a trait object.
        fn _takes_dyn(_: &dyn VcsBackend) {}
    }
}
