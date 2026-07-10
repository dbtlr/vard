//! Integration tests for the git shell-out backend, exercised against real
//! git repositories created in tempdirs. Network operations run against a
//! local bare repository used as `origin` (a file remote), so nothing here
//! touches the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;
use vard_core::vcs::git::GitBackend;
use vard_core::{
    LogFilter, PushOutcome, ReconcileOutcome, RestoreTarget, SafeState, SnapshotId,
    SnapshotOutcome, SnapshotRequest, Trigger, UnsafeReason, VcsBackend, VcsError, VcsRef,
};

// --- helpers ---------------------------------------------------------------

/// Runs a raw git command in `dir`, returning its output.
fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("failed to spawn git")
}

/// Runs a raw git command in `dir`, asserting success.
fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Configures a repository so commits succeed deterministically in CI,
/// independent of ambient global git config (identity, no signing).
fn configure(dir: &Path) {
    git_ok(dir, &["config", "user.email", "vard-test@example.com"]);
    git_ok(dir, &["config", "user.name", "Vard Test"]);
    git_ok(dir, &["config", "commit.gpgsign", "false"]);
}

/// A fresh repository on branch `main`, ready to commit into.
fn new_repo() -> (TempDir, GitBackend) {
    let tmp = TempDir::new().unwrap();
    let backend = GitBackend::init(tmp.path(), Some("main")).unwrap();
    configure(tmp.path());
    (tmp, backend)
}

/// A bare repository usable as a file remote (`origin`).
fn bare_origin() -> TempDir {
    let tmp = TempDir::new().unwrap();
    git_ok(tmp.path(), &["init", "--bare", "-b", "main"]);
    tmp
}

/// Clones `origin` into a fresh tempdir and opens it as a backend.
fn clone_of(origin: &Path) -> (TempDir, PathBuf, GitBackend) {
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("wc");
    let out = Command::new("git")
        .args(["clone", origin.to_str().unwrap(), dest.to_str().unwrap()])
        .output()
        .expect("failed to spawn git clone");
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    configure(&dest);
    let backend = GitBackend::open(&dest, "main", "origin").unwrap();
    (tmp, dest, backend)
}

fn write(dir: &Path, name: &str, content: &str) {
    fs::write(dir.join(name), content).unwrap();
}

/// Snapshots the whole tree with the given trigger.
fn snap(backend: &GitBackend, trigger: Trigger) -> Option<SnapshotOutcome> {
    backend.snapshot(&SnapshotRequest::new(trigger)).unwrap()
}

/// Like [`snap`], asserting a commit was made and returning its id.
fn snap_id(backend: &GitBackend, trigger: Trigger) -> SnapshotId {
    snap(backend, trigger).expect("a commit was made").id
}

fn rev(dir: &Path, refname: &str) -> String {
    let out = git(dir, &["rev-parse", refname]);
    assert!(out.status.success(), "rev-parse {refname} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_exists(dir: &Path, id: &str) -> bool {
    git(dir, &["cat-file", "-e", id]).status.success()
}

fn git_dir(dir: &Path) -> PathBuf {
    dir.join(".git")
}

// --- detect / init / open --------------------------------------------------

#[test]
fn detect_finds_repo_at_path() {
    let (tmp, _backend) = new_repo();
    let detected = GitBackend::detect(tmp.path()).unwrap();
    assert!(detected.is_some());
    let backend = detected.unwrap();
    assert_eq!(backend.branch(), "main");
    assert_eq!(backend.remote(), "origin");
}

#[test]
fn detect_returns_none_for_non_repo() {
    let tmp = TempDir::new().unwrap();
    assert!(GitBackend::detect(tmp.path()).unwrap().is_none());
}

#[test]
fn info_exclude_path_lives_in_the_git_dir_for_a_normal_repo() {
    let (tmp, backend) = new_repo();
    let exclude = backend.info_exclude_path().unwrap();
    assert!(
        exclude.is_absolute(),
        "expected an absolute path: {exclude:?}"
    );
    assert!(
        exclude.ends_with("info/exclude"),
        "expected .../info/exclude, got {exclude:?}"
    );
    // It sits under this repository's own .git directory.
    assert!(
        fs::canonicalize(exclude.parent().unwrap().parent().unwrap())
            .unwrap()
            .starts_with(fs::canonicalize(git_dir(tmp.path())).unwrap()),
        "exclude file should live under the repo's .git dir"
    );
}

#[test]
fn info_exclude_path_resolves_for_a_linked_worktree() {
    // In a linked worktree `.git` is a file (a gitlink), not a directory, so
    // joining ".git/info/exclude" by hand would be wrong. `--git-path` resolves
    // the shared exclude under the common git dir instead.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);

    let wt_holder = TempDir::new().unwrap();
    let wt_path = wt_holder.path().join("wt");
    git_ok(
        tmp.path(),
        &[
            "worktree",
            "add",
            "-b",
            "wtbranch",
            wt_path.to_str().unwrap(),
        ],
    );
    let wt_backend = GitBackend::open(&wt_path, "wtbranch", "origin").unwrap();

    let exclude = wt_backend.info_exclude_path().unwrap();
    assert!(
        exclude.is_absolute(),
        "expected an absolute path: {exclude:?}"
    );
    assert!(
        exclude.ends_with("info/exclude"),
        "expected .../info/exclude, got {exclude:?}"
    );
    // The worktree's `.git` is a file, not a directory — proof this is a linked
    // worktree and that hand-joining would have failed.
    assert!(
        wt_path.join(".git").is_file(),
        "a linked worktree's .git must be a gitlink file"
    );
    // The resolved exclude sits under the main repo's shared git dir.
    assert!(
        fs::canonicalize(exclude.parent().unwrap().parent().unwrap())
            .unwrap()
            .starts_with(fs::canonicalize(git_dir(tmp.path())).unwrap()),
        "worktree exclude should resolve under the main repo's .git dir, got {exclude:?}"
    );
}

#[test]
fn detect_returns_none_for_path_inside_a_deeper_repo() {
    let (tmp, _backend) = new_repo();
    let nested = tmp.path().join("sub/dir");
    fs::create_dir_all(&nested).unwrap();
    // The repo is rooted at tmp, not at the nested path: detected-elsewhere.
    assert!(GitBackend::detect(&nested).unwrap().is_none());
}

#[test]
fn init_honors_the_requested_branch() {
    let tmp = TempDir::new().unwrap();
    let backend = GitBackend::init(tmp.path(), Some("backup")).unwrap();
    assert_eq!(backend.branch(), "backup");
    // git agrees the unborn HEAD is on that branch.
    let out = git(tmp.path(), &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "backup");
}

#[test]
fn init_without_branch_adopts_gits_default() {
    let tmp = TempDir::new().unwrap();
    let backend = GitBackend::init(tmp.path(), None).unwrap();
    // Whatever git's default is, the backend reflects it.
    let out = git(tmp.path(), &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(
        backend.branch(),
        String::from_utf8_lossy(&out.stdout).trim()
    );
}

#[test]
fn open_rejects_non_repo() {
    let tmp = TempDir::new().unwrap();
    match GitBackend::open(tmp.path(), "main", "origin") {
        Err(VcsError::NotARepo) => {}
        other => panic!("expected NotARepo, got {other:?}"),
    }
}

#[test]
fn open_rejects_a_path_that_is_not_the_repo_root() {
    // Red-proven: pre-fix open() accepted any path inside a repo, so a
    // whole-tree restore from a subdir restored only the subtree while
    // add -A swept the whole repository.
    let (tmp, _backend) = new_repo();
    let sub = tmp.path().join("sub");
    fs::create_dir(&sub).unwrap();
    match GitBackend::open(&sub, "main", "origin") {
        Err(VcsError::NotRepoRoot { path, root }) => {
            assert_eq!(path, sub);
            assert_eq!(
                fs::canonicalize(&root).unwrap(),
                fs::canonicalize(tmp.path()).unwrap()
            );
        }
        other => panic!("expected NotRepoRoot, got {other:?}"),
    }
}

// --- safe state ------------------------------------------------------------

#[test]
fn safe_state_is_safe_on_a_clean_repo() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);
    assert_eq!(backend.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn safe_state_detects_each_in_progress_operation() {
    let cases = [
        ("MERGE_HEAD", UnsafeReason::MergeInProgress),
        ("CHERRY_PICK_HEAD", UnsafeReason::CherryPickInProgress),
        ("REVERT_HEAD", UnsafeReason::RevertInProgress),
        ("BISECT_LOG", UnsafeReason::BisectInProgress),
    ];
    for (marker, expected) in cases {
        let (tmp, backend) = new_repo();
        write(tmp.path(), "a.txt", "1\n");
        snap(&backend, Trigger::Manual);
        fs::write(git_dir(tmp.path()).join(marker), "sentinel\n").unwrap();
        assert_eq!(
            backend.is_safe_state().unwrap(),
            SafeState::Unsafe(expected),
            "marker {marker} should be detected"
        );
    }
}

#[test]
fn safe_state_detects_rebase_directories() {
    for dir_name in ["rebase-merge", "rebase-apply"] {
        let (tmp, backend) = new_repo();
        write(tmp.path(), "a.txt", "1\n");
        snap(&backend, Trigger::Manual);
        fs::create_dir(git_dir(tmp.path()).join(dir_name)).unwrap();
        assert_eq!(
            backend.is_safe_state().unwrap(),
            SafeState::Unsafe(UnsafeReason::RebaseInProgress),
            "{dir_name} should be detected"
        );
    }
}

#[test]
fn safe_state_detects_detached_head() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);
    git_ok(tmp.path(), &["checkout", "--detach", "HEAD"]);
    assert_eq!(
        backend.is_safe_state().unwrap(),
        SafeState::Unsafe(UnsafeReason::DetachedHead)
    );
}

#[test]
fn safe_state_detects_wrong_branch() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);
    git_ok(tmp.path(), &["checkout", "-b", "other"]);
    assert_eq!(
        backend.is_safe_state().unwrap(),
        SafeState::Unsafe(UnsafeReason::WrongBranch {
            expected: "main".to_string(),
            actual: "other".to_string(),
        })
    );
}

#[test]
fn safe_state_detects_markers_in_a_linked_worktree() {
    // The backend resolves the git dir via `rev-parse --absolute-git-dir`,
    // which in a linked worktree is the per-worktree directory (the .git file
    // in the worktree is a gitlink pointing there). Operation markers for the
    // worktree live in THAT directory, not the shared one.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);

    let wt_holder = TempDir::new().unwrap();
    let wt_path = wt_holder.path().join("wt");
    git_ok(
        tmp.path(),
        &[
            "worktree",
            "add",
            "-b",
            "wtbranch",
            wt_path.to_str().unwrap(),
        ],
    );
    let wt_backend = GitBackend::open(&wt_path, "wtbranch", "origin").unwrap();
    assert_eq!(wt_backend.is_safe_state().unwrap(), SafeState::Safe);

    // Place MERGE_HEAD in the per-worktree git dir and expect detection.
    let out = git(&wt_path, &["rev-parse", "--absolute-git-dir"]);
    let wt_git_dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    // Canonicalize both sides (macOS tempdirs live behind /var -> /private/var).
    assert!(
        fs::canonicalize(&wt_git_dir)
            .unwrap()
            .starts_with(fs::canonicalize(git_dir(tmp.path())).unwrap()),
        "worktree git dir should live under the main repo's .git"
    );
    fs::write(wt_git_dir.join("MERGE_HEAD"), "sentinel\n").unwrap();
    assert_eq!(
        wt_backend.is_safe_state().unwrap(),
        SafeState::Unsafe(UnsafeReason::MergeInProgress)
    );
}

// --- snapshot --------------------------------------------------------------

#[test]
fn snapshot_commits_a_change_with_subject_trailer_and_summary() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "zshrc", "1\n");
    write(tmp.path(), "gitconfig", "1\n");
    let outcome = snap(&backend, Trigger::Event).expect("a commit was made");

    // The outcome summary describes exactly what was committed.
    assert_eq!(outcome.summary.added, 2);
    assert_eq!(outcome.summary.total(), 2);

    // Subject and trailer landed in the actual commit. The staged diff lists
    // paths sorted, so notable names appear in that order.
    let subject = git(tmp.path(), &["log", "-1", "--format=%s"]);
    assert_eq!(
        String::from_utf8_lossy(&subject.stdout).trim(),
        "snapshot: 2 added (gitconfig, zshrc)"
    );
    let trailer = git(
        tmp.path(),
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Vard-Trigger,valueonly)",
        ],
    );
    assert_eq!(String::from_utf8_lossy(&trailer.stdout).trim(), "event");
    assert_eq!(rev(tmp.path(), "HEAD"), outcome.id.as_str());
}

#[test]
fn snapshot_subject_preserves_unicode_and_spaced_filenames() {
    // Red-proven class: `git status --porcelain` C-quotes unusual names
    // ("na\303\257ve file.txt"); the staged-diff `-z` source delivers raw
    // bytes, so the subject shows the real name.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "naïve file.txt", "x\n");
    let outcome = snap(&backend, Trigger::Manual).expect("a commit was made");
    assert_eq!(outcome.summary.notable, vec!["naïve file.txt"]);

    let subject = git(tmp.path(), &["log", "-1", "--format=%s"]);
    assert_eq!(
        String::from_utf8_lossy(&subject.stdout).trim(),
        "snapshot: 1 added (naïve file.txt)"
    );
}

#[test]
fn snapshot_truncates_notable_after_three_names() {
    let (tmp, backend) = new_repo();
    for name in ["a", "b", "c", "d", "e"] {
        write(tmp.path(), name, "x\n");
    }
    let outcome = snap(&backend, Trigger::Manual).expect("a commit was made");
    assert_eq!(outcome.summary.added, 5);
    // notable is capped during parsing (3 shown + 1 to prove truncation).
    assert_eq!(outcome.summary.notable, vec!["a", "b", "c", "d"]);

    let subject = git(tmp.path(), &["log", "-1", "--format=%s"]);
    assert_eq!(
        String::from_utf8_lossy(&subject.stdout).trim(),
        "snapshot: 5 added (a, b, c, …)"
    );
}

#[test]
fn snapshot_writes_user_text_and_extra_trailers() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "x\n");
    let req = SnapshotRequest {
        trigger: Trigger::Manual,
        user_text: Some("before the demo".to_string()),
        extra_trailers: vec![("Vard-Host".to_string(), "laptop".to_string())],
    };
    backend.snapshot(&req).unwrap().expect("a commit was made");

    let body = git(tmp.path(), &["log", "-1", "--format=%B"]);
    let body = String::from_utf8_lossy(&body.stdout);
    assert!(body.contains("before the demo"), "body was: {body}");
    let host = git(
        tmp.path(),
        &["log", "-1", "--format=%(trailers:key=Vard-Host,valueonly)"],
    );
    assert_eq!(String::from_utf8_lossy(&host.stdout).trim(), "laptop");
}

#[test]
fn snapshot_returns_none_on_a_clean_tree() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);
    // Nothing changed since the last snapshot.
    assert_eq!(
        backend
            .snapshot(&SnapshotRequest::new(Trigger::Interval))
            .unwrap(),
        None
    );
}

#[test]
fn snapshot_refuses_an_unsafe_repo() {
    // Defense in depth: the backend re-checks safe state itself.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);
    git_ok(tmp.path(), &["checkout", "-b", "other"]);
    write(tmp.path(), "a.txt", "2\n");
    match backend.snapshot(&SnapshotRequest::new(Trigger::Event)) {
        Err(VcsError::UnsafeState(UnsafeReason::WrongBranch { .. })) => {}
        other => panic!("expected UnsafeState(WrongBranch), got {other:?}"),
    }
}

#[test]
fn snapshot_reports_lock_contention() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    // A held index lock (as if another git process were mid-operation).
    fs::write(git_dir(tmp.path()).join("index.lock"), "").unwrap();
    match backend.snapshot(&SnapshotRequest::new(Trigger::Event)) {
        Err(VcsError::LockContended { op }) => assert_eq!(op, "add"),
        other => panic!("expected LockContended, got {other:?}"),
    }
    // The backend must never remove the lock; that is the engine's job.
    assert!(git_dir(tmp.path()).join("index.lock").exists());
}

// --- log -------------------------------------------------------------------

#[test]
fn log_round_trips_triggers_newest_first() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);
    write(tmp.path(), "a.txt", "2\n");
    snap(&backend, Trigger::Event);

    let snaps = backend.log(&LogFilter::default()).unwrap();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].trigger, Some(Trigger::Event));
    assert_eq!(snaps[1].trigger, Some(Trigger::Manual));
    assert!(snaps[0].subject.starts_with("snapshot:"));
}

#[test]
fn log_on_a_repo_with_no_commits_is_empty() {
    // Red-proven: pre-fix this was a CommandFailed from git's
    // "does not have any commits yet".
    let (_tmp, backend) = new_repo();
    assert!(backend.log(&LogFilter::default()).unwrap().is_empty());
}

#[test]
fn log_respects_limit() {
    let (tmp, backend) = new_repo();
    for i in 0..3 {
        write(tmp.path(), "a.txt", &format!("{i}\n"));
        snap(&backend, Trigger::Interval);
    }
    let snaps = backend
        .log(&LogFilter {
            since: None,
            limit: Some(2),
        })
        .unwrap();
    assert_eq!(snaps.len(), 2);
}

#[test]
fn log_respects_since_bounds() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);

    // A floor well in the past keeps the commit; a floor in the future drops it.
    let past = SystemTime::now() - Duration::from_secs(3600);
    let future = SystemTime::now() + Duration::from_secs(3600);
    assert_eq!(
        backend
            .log(&LogFilter {
                since: Some(past),
                limit: None,
            })
            .unwrap()
            .len(),
        1
    );
    assert!(
        backend
            .log(&LogFilter {
                since: Some(future),
                limit: None,
            })
            .unwrap()
            .is_empty()
    );
}

#[test]
fn log_since_boundary_is_inclusive() {
    // Pins git's observed --since boundary behavior (documented on
    // LogFilter): a commit at exactly `since` is returned.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    git_ok(tmp.path(), &["add", "-A"]);
    let t = 1_700_000_000u64;
    let out = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["commit", "-m", "pinned"])
        .env("GIT_COMMITTER_DATE", format!("@{t} +0000"))
        .env("GIT_AUTHOR_DATE", format!("@{t} +0000"))
        .output()
        .unwrap();
    assert!(out.status.success());

    let at = |secs: u64| LogFilter {
        since: Some(UNIX_EPOCH + Duration::from_secs(secs)),
        limit: None,
    };
    assert_eq!(backend.log(&at(t)).unwrap().len(), 1, "exact boundary");
    assert_eq!(backend.log(&at(t - 1)).unwrap().len(), 1, "just before");
    assert!(backend.log(&at(t + 1)).unwrap().is_empty(), "just after");
}

// --- diff ------------------------------------------------------------------

#[test]
fn diff_between_two_snapshots_shows_the_change() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "old\n");
    let first = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "a.txt", "new\n");
    let second = snap_id(&backend, Trigger::Manual);

    let diff = backend
        .diff(&VcsRef::from(&first), Some(&VcsRef::from(&second)))
        .unwrap();
    assert!(diff.contains("-old"), "diff was: {diff}");
    assert!(diff.contains("+new"), "diff was: {diff}");
}

#[test]
fn diff_against_working_tree_when_to_is_none() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "committed\n");
    let first = snap_id(&backend, Trigger::Manual);
    // Uncommitted working-tree edit.
    write(tmp.path(), "a.txt", "working\n");
    let diff = backend.diff(&VcsRef::from(&first), None).unwrap();
    assert!(diff.contains("+working"), "diff was: {diff}");
}

// --- restore ---------------------------------------------------------------

#[test]
fn restore_single_file_without_dropping_commits() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "v1\n");
    let c1 = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "a.txt", "v2\n");
    let c2 = snap_id(&backend, Trigger::Manual);

    backend
        .restore(&RestoreTarget {
            rev: VcsRef::from(&c1),
            path: Some("a.txt".into()),
        })
        .unwrap();

    assert_eq!(
        fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
        "v1\n"
    );
    // The commit we restored away from is still reachable — nothing was lost.
    assert!(commit_exists(tmp.path(), c2.as_str()));
    assert_eq!(rev(tmp.path(), "HEAD"), c2.as_str());
}

#[test]
fn restore_whole_tree_without_dropping_commits() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    write(tmp.path(), "b.txt", "1\n");
    let c1 = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "a.txt", "2\n");
    write(tmp.path(), "b.txt", "2\n");
    let c2 = snap_id(&backend, Trigger::Manual);

    backend
        .restore(&RestoreTarget {
            rev: VcsRef::from(&c1),
            path: None,
        })
        .unwrap();

    assert_eq!(fs::read_to_string(tmp.path().join("a.txt")).unwrap(), "1\n");
    assert_eq!(fs::read_to_string(tmp.path().join("b.txt")).unwrap(), "1\n");
    assert!(commit_exists(tmp.path(), c2.as_str()));
}

#[test]
fn restore_of_a_path_absent_at_the_target_rev_errors_cleanly() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    let c1 = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "later.txt", "new\n");
    snap(&backend, Trigger::Manual);

    // later.txt does not exist at c1: the restore must fail without touching
    // the work tree.
    match backend.restore(&RestoreTarget {
        rev: VcsRef::from(&c1),
        path: Some("later.txt".into()),
    }) {
        Err(VcsError::CommandFailed { op, .. }) => assert_eq!(op, "checkout"),
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    // No corruption: the file is still there with its content.
    assert_eq!(
        fs::read_to_string(tmp.path().join("later.txt")).unwrap(),
        "new\n"
    );
    let status = git(tmp.path(), &["status", "--porcelain"]);
    assert!(
        status.stdout.is_empty(),
        "tree should be clean after the failed restore"
    );
}

#[test]
fn restore_reports_lock_contention() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "v1\n");
    let c1 = snap_id(&backend, Trigger::Manual);
    fs::write(git_dir(tmp.path()).join("index.lock"), "").unwrap();
    match backend.restore(&RestoreTarget {
        rev: VcsRef::from(&c1),
        path: Some("a.txt".into()),
    }) {
        Err(VcsError::LockContended { op }) => assert_eq!(op, "checkout"),
        other => panic!("expected LockContended, got {other:?}"),
    }
    assert!(git_dir(tmp.path()).join("index.lock").exists());
}

// --- fetch / reconcile / push ----------------------------------------------

/// A working repo pushed to a shared bare origin, plus a second clone that can
/// move the remote out from under the first.
struct RemoteFixture {
    _origin: TempDir,
    a_tmp: TempDir,
    a: GitBackend,
    _b_tmp: TempDir,
    b: GitBackend,
    b_path: PathBuf,
}

fn remote_fixture() -> RemoteFixture {
    let origin = bare_origin();
    let (a_tmp, a) = new_repo();
    git_ok(
        a_tmp.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    write(a_tmp.path(), "file.txt", "base\n");
    snap(&a, Trigger::Manual);
    assert_eq!(a.push().unwrap(), PushOutcome::Pushed);

    let (b_tmp, b_path, b) = clone_of(origin.path());
    RemoteFixture {
        _origin: origin,
        a_tmp,
        a,
        _b_tmp: b_tmp,
        b,
        b_path,
    }
}

#[test]
fn push_reports_pushed_then_up_to_date() {
    let fx = remote_fixture();
    write(fx.a_tmp.path(), "file.txt", "more\n");
    snap(&fx.a, Trigger::Manual);
    assert_eq!(fx.a.push().unwrap(), PushOutcome::Pushed);
    // Nothing new to send.
    assert_eq!(fx.a.push().unwrap(), PushOutcome::UpToDate);
}

#[test]
fn push_reports_non_fast_forward_after_a_competing_push() {
    let fx = remote_fixture();
    // B advances origin.
    write(&fx.b_path, "file.txt", "from-b\n");
    snap(&fx.b, Trigger::Manual);
    assert_eq!(fx.b.push().unwrap(), PushOutcome::Pushed);

    // A commits on the stale base and pushes without fetching: rejected.
    write(fx.a_tmp.path(), "file.txt", "from-a\n");
    snap(&fx.a, Trigger::Manual);
    assert_eq!(fx.a.push().unwrap(), PushOutcome::NonFastForward);

    // The same race after fetching reports the other porcelain spelling
    // ("non-fast-forward" instead of "fetch first") — both are the race.
    fx.a.fetch().unwrap();
    assert_eq!(fx.a.push().unwrap(), PushOutcome::NonFastForward);
}

#[test]
fn push_rejection_that_is_not_the_race_is_a_command_failure() {
    // Red-proven: pre-fix, any "[rejected]" in stderr classified as
    // NonFastForward, sending the sync engine into a hopeless retry loop. A
    // fast-forwardable push to a NON-bare remote checked out on main is
    // rejected with "[remote rejected] (branch is currently checked out)" —
    // not a race, must be CommandFailed.
    let remote = TempDir::new().unwrap();
    git_ok(remote.path(), &["init", "-b", "main"]);
    configure(remote.path());
    write(remote.path(), "r", "x\n");
    git_ok(remote.path(), &["add", "-A"]);
    git_ok(remote.path(), &["commit", "-m", "r"]);

    // Clone so histories are shared and the push would fast-forward (a
    // divergent history would be a fetch-first race instead).
    let (_tmp, dest, backend) = clone_of(remote.path());
    write(&dest, "f", "x\n");
    snap(&backend, Trigger::Manual);
    match backend.push() {
        Err(VcsError::CommandFailed { op, stderr, .. }) => {
            assert_eq!(op, "push");
            assert!(
                stderr.contains("checked out"),
                "reason should be carried: {stderr}"
            );
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
}

#[test]
fn fetch_before_first_push_is_a_normal_state() {
    // Red-proven: pre-fix, fetching a branch absent on the remote was a
    // CommandFailed (exit 128, "couldn't find remote ref").
    let origin = bare_origin();
    let (tmp, backend) = new_repo();
    git_ok(
        tmp.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    write(tmp.path(), "f", "x\n");
    snap(&backend, Trigger::Manual);

    let state = backend.fetch().unwrap();
    assert!(!state.remote_moved);
    assert_eq!((state.ahead, state.behind), (1, 0));

    // Even with an unborn local branch it is a state, not an error.
    let (tmp2, backend2) = new_repo();
    git_ok(
        tmp2.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    let state = backend2.fetch().unwrap();
    assert_eq!(
        (state.remote_moved, state.ahead, state.behind),
        (false, 0, 0)
    );
}

#[test]
fn fetch_reports_remote_movement_and_behind_count() {
    let fx = remote_fixture();
    // Before any remote movement.
    let before = fx.a.fetch().unwrap();
    assert!(!before.remote_moved);
    assert_eq!((before.ahead, before.behind), (0, 0));

    // B advances origin by one commit.
    write(&fx.b_path, "file.txt", "from-b\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push().unwrap();

    let after = fx.a.fetch().unwrap();
    assert!(after.remote_moved);
    assert_eq!((after.ahead, after.behind), (0, 1));
}

#[test]
fn fetch_works_without_a_configured_fetch_refspec() {
    // The explicit refspec updates the tracking ref even when the remote was
    // added with no fetch refspec at all.
    let fx = remote_fixture();
    git_ok(
        fx.a_tmp.path(),
        &["config", "--unset-all", "remote.origin.fetch"],
    );
    write(&fx.b_path, "file.txt", "from-b\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push().unwrap();

    let state = fx.a.fetch().unwrap();
    assert!(state.remote_moved);
    assert_eq!(state.behind, 1);
}

#[test]
fn reconcile_already_up_to_date_when_nothing_moved() {
    let fx = remote_fixture();
    fx.a.fetch().unwrap();
    assert_eq!(fx.a.reconcile().unwrap(), ReconcileOutcome::AlreadyUpToDate);
}

#[test]
fn reconcile_rebases_cleanly_onto_a_moved_remote() {
    let fx = remote_fixture();
    // B advances origin (a file A does not touch → clean).
    write(&fx.b_path, "other.txt", "b-only\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push().unwrap();
    let remote_head = rev(&fx.b_path, "HEAD");

    fx.a.fetch().unwrap();
    match fx.a.reconcile().unwrap() {
        ReconcileOutcome::Rebased { new_head } => {
            // With no local commits, the branch fast-forwards to the remote tip.
            assert_eq!(new_head.as_str(), remote_head);
        }
        other => panic!("expected Rebased, got {other:?}"),
    }
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_conflict_aborts_to_a_pristine_tree() {
    let fx = remote_fixture();
    // B changes the shared file and advances origin.
    write(&fx.b_path, "file.txt", "b-change\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push().unwrap();

    // A changes the same line locally (not pushed) → rebase will conflict.
    write(fx.a_tmp.path(), "file.txt", "a-change\n");
    let a_local = snap_id(&fx.a, Trigger::Manual);

    fx.a.fetch().unwrap();
    assert_eq!(fx.a.reconcile().unwrap(), ReconcileOutcome::Conflict);

    // Branch is exactly where it was before the rebase.
    assert_eq!(rev(fx.a_tmp.path(), "refs/heads/main"), a_local.as_str());
    // No conflict markers survived the abort.
    let contents = fs::read_to_string(fx.a_tmp.path().join("file.txt")).unwrap();
    assert_eq!(contents, "a-change\n");
    assert!(!contents.contains("<<<<<<<"));
    // No rebase left in progress, and the repo is safe again.
    assert!(!git_dir(fx.a_tmp.path()).join("rebase-merge").exists());
    assert!(!git_dir(fx.a_tmp.path()).join("rebase-apply").exists());
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_with_a_broken_signer_still_rebases() {
    // Red-proven: pre-fix, commit.gpgsign=true with a failing signer aborted
    // the replay mid-rebase, leaving rebase-merge/ behind, and reconcile
    // misreported it as Conflict. The backend pins signing off.
    let fx = remote_fixture();
    write(&fx.b_path, "other.txt", "b-only\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push().unwrap();

    // A has a local commit (so the rebase must replay → commit machinery) and
    // a broken signing config, as a user's repo might.
    write(fx.a_tmp.path(), "mine.txt", "a-only\n");
    snap(&fx.a, Trigger::Manual);
    git_ok(fx.a_tmp.path(), &["config", "commit.gpgsign", "true"]);
    git_ok(
        fx.a_tmp.path(),
        &["config", "gpg.program", "/usr/bin/false"],
    );

    fx.a.fetch().unwrap();
    match fx.a.reconcile() {
        Ok(ReconcileOutcome::Rebased { .. }) => {}
        other => panic!("expected Rebased, got {other:?}"),
    }
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_never_lets_autostash_leave_markers() {
    // Red-proven: with rebase.autostash=true and a dirty conflicting tree,
    // the rebase exits 0 but the stash pop leaves conflict markers — pre-fix
    // reconcile reported Rebased over a corrupted tree. The backend pins
    // autostash off, so a dirty tree is a refused rebase instead.
    let fx = remote_fixture();
    write(&fx.b_path, "file.txt", "remote-change\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push().unwrap();

    git_ok(fx.a_tmp.path(), &["config", "rebase.autostash", "true"]);
    write(fx.a_tmp.path(), "file.txt", "dirty-local\n"); // uncommitted

    fx.a.fetch().unwrap();
    let result = fx.a.reconcile();
    let contents = fs::read_to_string(fx.a_tmp.path().join("file.txt")).unwrap();
    assert!(
        !contents.contains("<<<<<<<"),
        "conflict markers in tree after reconcile ({result:?}): {contents}"
    );
    assert_eq!(contents, "dirty-local\n", "dirty content must be untouched");
    match result {
        Err(VcsError::CommandFailed { op, .. }) => assert_eq!(op, "rebase"),
        other => panic!("expected CommandFailed for a dirty tree, got {other:?}"),
    }
}

#[test]
fn reconcile_non_conflict_failure_is_not_reported_as_conflict() {
    // A missing upstream ref (never fetched) fails the rebase without leaving
    // a rebase in progress: that is a CommandFailed, not a Conflict.
    let origin = bare_origin();
    let (tmp, backend) = new_repo();
    git_ok(
        tmp.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    write(tmp.path(), "f", "x\n");
    snap(&backend, Trigger::Manual);

    match backend.reconcile() {
        Err(VcsError::CommandFailed { op, .. }) => assert_eq!(op, "rebase"),
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    assert_eq!(backend.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_reports_lock_contention() {
    let fx = remote_fixture();
    write(&fx.b_path, "other.txt", "b\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push().unwrap();
    fx.a.fetch().unwrap();

    fs::write(git_dir(fx.a_tmp.path()).join("index.lock"), "").unwrap();
    match fx.a.reconcile() {
        Err(VcsError::LockContended { op }) => assert_eq!(op, "rebase"),
        other => panic!("expected LockContended, got {other:?}"),
    }
    assert!(git_dir(fx.a_tmp.path()).join("index.lock").exists());
}

#[test]
fn reconcile_refuses_an_unsafe_repo() {
    let fx = remote_fixture();
    fs::write(git_dir(fx.a_tmp.path()).join("MERGE_HEAD"), "sentinel\n").unwrap();
    match fx.a.reconcile() {
        Err(VcsError::UnsafeState(UnsafeReason::MergeInProgress)) => {}
        other => panic!("expected UnsafeState(MergeInProgress), got {other:?}"),
    }
}
