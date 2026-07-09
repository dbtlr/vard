//! The version-control seam: the [`VcsBackend`] trait, the value types that
//! flow across it, and the [`CommitMessage`] model the engine renders into
//! snapshot commits.
//!
//! This crate owns **correctness**, and the sharpest correctness concern lives
//! here: no VCS operation may destroy the only copy of anything (ADR 0002), and
//! vard only ever commits into the watched repository's own configured branch
//! (ADR 0001), never into someone else's mid-operation repository (spec §3).
//! The trait encodes those rules as its contract; [`git`] is the day-one
//! implementation that shells out to the `git` binary.
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
//! The spec's §13 sketch lists `detect`/`init` on the trait. That sketch is
//! illustrative; dyn-compatibility wins. Constructors that return `Self` cannot
//! live on a dyn-compatible trait, so [`GitBackend::detect`](git::GitBackend::detect),
//! [`GitBackend::init`](git::GitBackend::init), and
//! [`GitBackend::open`](git::GitBackend::open) are inherent methods on the
//! concrete backend. The trait is purely the per-watch operational surface.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::event::Trigger;

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
    /// This is the guard behind ADR 0001's "one configured branch" rule and
    /// spec §3's "never commit into someone else's mid-operation repository":
    /// an in-progress merge/cherry-pick/revert/bisect/rebase, a detached
    /// `HEAD`, or a `HEAD` on the wrong branch all report `Unsafe`.
    fn is_safe_state(&self) -> Result<SafeState, VcsError>;

    /// Sweeps the whole work tree and commits it as one snapshot, returning the
    /// new snapshot's id — or `None` when nothing changed.
    ///
    /// The sweep is intentionally total (`git add -A`, per ADR 0001): vard
    /// snapshots the directory as a whole, not a curated index. When the sweep
    /// leaves no staged difference, no commit is made and `None` is returned;
    /// an empty commit is never forced.
    fn snapshot(&self, msg: &CommitMessage) -> Result<Option<SnapshotId>, VcsError>;

    /// Lists snapshots most-recent-first, filtered by [`LogFilter`].
    ///
    /// Each [`Snapshot`] carries the `Vard-Trigger` trailer parsed back into a
    /// [`Trigger`] when present (and `None` when absent or unrecognized).
    fn log(&self, filter: &LogFilter) -> Result<Vec<Snapshot>, VcsError>;

    /// Returns the raw unified diff between two references, or between one
    /// reference and the current work tree when `to` is `None`.
    fn diff(&self, from: &VcsRef, to: Option<&VcsRef>) -> Result<String, VcsError>;

    /// Restores the work tree (or a single path within it) to a prior
    /// reference.
    ///
    /// This overwrites working-tree content but never moves the branch ref or
    /// `HEAD`, so no commit can be lost by a restore (see
    /// [`GitBackend::restore`](git::GitBackend::restore) for the exact
    /// mechanism). Taking a protective snapshot of the current state *before*
    /// restoring is the engine's responsibility, not the backend's.
    fn restore(&self, target: &RestoreTarget) -> Result<(), VcsError>;

    /// Fetches the configured branch from the configured remote and reports how
    /// local and remote now relate (see [`RemoteState`]).
    fn fetch(&self) -> Result<RemoteState, VcsError>;

    /// Makes a single attempt to rebase the configured branch onto the
    /// already-fetched remote ref (see [`ReconcileOutcome`]).
    ///
    /// On conflict the rebase is aborted and the branch is left exactly as it
    /// was — no conflict markers ever remain in the tree. This performs exactly
    /// one attempt: ret/ry, backoff, and watch-state transitions are the sync
    /// engine's concern, not the backend's.
    fn reconcile(&self) -> Result<ReconcileOutcome, VcsError>;

    /// Pushes the configured branch to the configured remote (see
    /// [`PushOutcome`]).
    ///
    /// A non-fast-forward rejection is a normal [`PushOutcome::NonFastForward`]
    /// result, not an error; resolving the race is the sync engine's job.
    fn push(&self) -> Result<PushOutcome, VcsError>;
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
/// Detection follows spec §3: the in-progress-operation variants are keyed off
/// the marker files and directories git leaves in the git dir, and the `HEAD`
/// variants off the current branch versus the backend's configured branch.
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
/// Both bounds are optional and independent. `since` keeps only snapshots at or
/// after the given time; `limit` caps the count, taking the most recent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// Keep only snapshots committed at or after this time.
    pub since: Option<SystemTime>,
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
    /// How many commits the local branch is ahead of the upstream.
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
    Rebased {
        /// The branch's new tip after the rebase.
        new_head: SnapshotId,
    },
    /// The rebase hit a conflict and was aborted; the branch is unchanged and
    /// the tree contains no conflict markers.
    Conflict,
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

/// A tally of what changed in a snapshot, used to render its commit subject.
///
/// The counts are of files by disposition; `notable` is a list of changed file
/// names from which the subject shows up to the first three. A backend computes
/// this from `git status --porcelain` for the tree it is about to commit (see
/// [`GitBackend::change_summary`](git::GitBackend::change_summary)).
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

/// The message the engine hands [`VcsBackend::snapshot`] to commit.
///
/// Built via [`CommitMessage::new`] from a [`ChangeSummary`], the [`Trigger`]
/// that caused the snapshot, and optional free-form user text. [`render`] turns
/// it into the exact commit-message bytes (spec §4): a `snapshot:` subject line
/// summarizing the change, the optional user text as its own paragraph, and a
/// `Vard-Trigger` trailer.
///
/// [`render`]: CommitMessage::render
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMessage {
    summary: ChangeSummary,
    trigger: Trigger,
    user_text: Option<String>,
}

impl CommitMessage {
    /// Builds a commit message from a change summary, the causing trigger, and
    /// optional user text.
    pub fn new(summary: ChangeSummary, trigger: Trigger, user_text: Option<&str>) -> Self {
        Self {
            summary,
            trigger,
            user_text: user_text.map(str::to_string),
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
    /// then the `Vard-Trigger` trailer.
    ///
    /// The trailer's value is the [`Trigger`]'s `Display` spelling, so it round
    /// trips back through [`VcsBackend::log`].
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
        out
    }
}

/// The commit trailer key carrying the snapshot's trigger.
pub(crate) const TRAILER_KEY: &str = "Vard-Trigger";

/// Parses a `Vard-Trigger` trailer value back into a [`Trigger`].
///
/// Returns `None` for an unrecognized value. The spellings mirror
/// [`Trigger`]'s `Display`; keep them in step when the vocabulary grows.
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
/// tell apart are distinct: a missing git binary ([`GitNotFound`]), a contended
/// index lock ([`LockContended`], which the engine — not this layer — retries),
/// a path that is not a repository ([`NotARepo`]), and the catch-alls for a
/// non-zero git exit ([`CommandFailed`]), I/O ([`Io`]), and output parsing
/// ([`Parse`]).
///
/// [`GitNotFound`]: VcsError::GitNotFound
/// [`LockContended`]: VcsError::LockContended
/// [`NotARepo`]: VcsError::NotARepo
/// [`CommandFailed`]: VcsError::CommandFailed
/// [`Io`]: VcsError::Io
/// [`Parse`]: VcsError::Parse
#[derive(Debug)]
#[non_exhaustive]
pub enum VcsError {
    /// The `git` binary could not be found on the system.
    GitNotFound,
    /// A git index (or ref) lock was held by another process. The engine owns
    /// the retry/backoff policy; this layer only classifies the condition and
    /// never touches the lock file.
    LockContended,
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
    /// An I/O error occurred spawning or communicating with git.
    Io(std::io::Error),
    /// Git's output could not be parsed as expected.
    Parse(String),
}

impl fmt::Display for VcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VcsError::GitNotFound => f.write_str("the git executable could not be found on PATH"),
            VcsError::LockContended => {
                f.write_str("a git lock is held by another process; retry later")
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
            VcsError::Io(e) => write!(f, "git I/O error: {e}"),
            VcsError::Parse(msg) => write!(f, "could not parse git output: {msg}"),
        }
    }
}

impl Error for VcsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            VcsError::Io(e) => Some(e),
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

    #[test]
    fn subject_renders_all_counts_and_notable_files() {
        let msg = CommitMessage::new(
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
        let msg = CommitMessage::new(summary(0, 3, 0, &["a", "b"]), Trigger::Interval, None);
        assert_eq!(msg.subject(), "snapshot: 3 added (a, b)");

        let msg = CommitMessage::new(summary(5, 0, 0, &["only"]), Trigger::Manual, None);
        assert_eq!(msg.subject(), "snapshot: 5 changed (only)");
    }

    #[test]
    fn subject_does_not_pluralize_the_disposition_words() {
        // The words are invariant regardless of count: "1 changed", not
        // "1 change" or "1 changeds".
        let one = CommitMessage::new(summary(1, 1, 1, &["x"]), Trigger::Manual, None);
        assert_eq!(one.subject(), "snapshot: 1 changed, 1 added, 1 deleted (x)");
    }

    #[test]
    fn subject_truncates_notable_to_three_with_ellipsis() {
        let msg = CommitMessage::new(
            summary(9, 0, 0, &["a", "b", "c", "d", "e"]),
            Trigger::Event,
            None,
        );
        assert_eq!(msg.subject(), "snapshot: 9 changed (a, b, c, …)");

        // Exactly three shows all three with no ellipsis.
        let three = CommitMessage::new(summary(3, 0, 0, &["a", "b", "c"]), Trigger::Event, None);
        assert_eq!(three.subject(), "snapshot: 3 changed (a, b, c)");
    }

    #[test]
    fn subject_without_notable_has_no_parens() {
        let msg = CommitMessage::new(summary(2, 0, 0, &[]), Trigger::Event, None);
        assert_eq!(msg.subject(), "snapshot: 2 changed");
    }

    #[test]
    fn subject_with_no_changes_reads_no_changes() {
        let msg = CommitMessage::new(summary(0, 0, 0, &[]), Trigger::Manual, None);
        assert_eq!(msg.subject(), "snapshot: no changes");
    }

    #[test]
    fn render_places_trailer_last_with_no_user_text() {
        let msg = CommitMessage::new(summary(1, 0, 0, &["f"]), Trigger::Event, None);
        assert_eq!(
            msg.render(),
            "snapshot: 1 changed (f)\n\nVard-Trigger: event\n"
        );
    }

    #[test]
    fn render_inserts_user_text_as_its_own_paragraph() {
        let msg = CommitMessage::new(
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
        let msg = CommitMessage::new(summary(1, 0, 0, &["f"]), Trigger::Manual, Some("   \n  "));
        assert_eq!(
            msg.render(),
            "snapshot: 1 changed (f)\n\nVard-Trigger: manual\n"
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
            let msg = CommitMessage::new(summary(1, 0, 0, &["f"]), trigger, None);
            let rendered = msg.render();
            let expected = format!("Vard-Trigger: {spelling}\n");
            assert!(
                rendered.ends_with(&expected),
                "trigger {trigger:?} rendered {rendered:?}, expected it to end with {expected:?}"
            );
            // And the trailer round-trips back to the same trigger.
            assert_eq!(trigger_from_str(spelling), Some(trigger));
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
    }

    #[test]
    fn backend_is_dyn_compatible() {
        // Compile-time proof that the trait can be used as a trait object.
        fn _takes_dyn(_: &dyn VcsBackend) {}
    }
}
