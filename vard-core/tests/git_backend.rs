//! Integration tests for the git shell-out backend, exercised against real
//! git repositories created in tempdirs. Network operations run against a
//! local bare repository used as `origin` (a file remote), so nothing here
//! touches the network.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;
use vard_core::vcs::git::GitBackend;
use vard_core::{
    ChangeSummary, CommitMessage, LogFilter, PushOutcome, ReconcileOutcome, RestoreTarget,
    SafeState, SnapshotId, Trigger, UnsafeReason, VcsBackend, VcsError, VcsRef,
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
/// independent of ambient global git config (identity, no signing, no hooks).
fn configure(dir: &Path) {
    git_ok(dir, &["config", "user.email", "vard-test@example.com"]);
    git_ok(dir, &["config", "user.name", "Vard Test"]);
    git_ok(dir, &["config", "commit.gpgsign", "false"]);
    // Point hooks at a path with no hooks so a developer's global hooks cannot
    // interfere with rebase/checkout during tests.
    git_ok(dir, &["config", "core.hooksPath", "vard-no-such-hooks"]);
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
fn clone_of(origin: &Path) -> (TempDir, GitBackend) {
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
    (tmp, backend)
}

fn write(dir: &Path, name: &str, content: &str) {
    fs::write(dir.join(name), content).unwrap();
}

/// Snapshots the whole tree with the given trigger (computing the summary the
/// way the engine would).
fn snap(backend: &GitBackend, trigger: Trigger) -> Option<SnapshotId> {
    let summary = backend.change_summary().unwrap();
    let msg = CommitMessage::new(summary, trigger, None);
    backend.snapshot(&msg).unwrap()
}

fn rev(dir: &Path, refname: &str) -> String {
    let out = git(dir, &["rev-parse", refname]);
    assert!(out.status.success(), "rev-parse {refname} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_exists(dir: &Path, id: &str) -> bool {
    git(dir, &["cat-file", "-e", id]).status.success()
}

// --- detect ----------------------------------------------------------------

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
fn detect_returns_none_for_path_inside_a_deeper_repo() {
    let (tmp, _backend) = new_repo();
    let nested = tmp.path().join("sub/dir");
    fs::create_dir_all(&nested).unwrap();
    // The repo is rooted at tmp, not at the nested path: detected-elsewhere.
    assert!(GitBackend::detect(&nested).unwrap().is_none());
}

// --- init ------------------------------------------------------------------

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

// --- safe state ------------------------------------------------------------

fn git_dir(dir: &Path) -> std::path::PathBuf {
    dir.join(".git")
}

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

// --- snapshot --------------------------------------------------------------

#[test]
fn snapshot_commits_a_change_with_subject_and_trailer() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "zshrc", "1\n");
    write(tmp.path(), "gitconfig", "1\n");
    let summary = backend.change_summary().unwrap();
    let msg = CommitMessage::new(summary, Trigger::Event, None);
    let id = backend.snapshot(&msg).unwrap().expect("a commit was made");

    // Subject and trailer landed in the actual commit. `git status --porcelain`
    // lists paths sorted, so notable names appear in that order.
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
    assert_eq!(rev(tmp.path(), "HEAD"), id.as_str());
}

#[test]
fn snapshot_returns_none_on_a_clean_tree() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    snap(&backend, Trigger::Manual);
    // Nothing changed since the last snapshot.
    let msg = CommitMessage::new(ChangeSummary::default(), Trigger::Interval, None);
    assert_eq!(backend.snapshot(&msg).unwrap(), None);
}

#[test]
fn snapshot_reports_lock_contention() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    // A held index lock (as if another git process were mid-operation).
    fs::write(git_dir(tmp.path()).join("index.lock"), "").unwrap();
    let msg = CommitMessage::new(
        ChangeSummary {
            added: 1,
            notable: vec!["a.txt".to_string()],
            ..Default::default()
        },
        Trigger::Event,
        None,
    );
    match backend.snapshot(&msg) {
        Err(VcsError::LockContended) => {}
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

// --- diff ------------------------------------------------------------------

#[test]
fn diff_between_two_snapshots_shows_the_change() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "old\n");
    let first = snap(&backend, Trigger::Manual).unwrap();
    write(tmp.path(), "a.txt", "new\n");
    let second = snap(&backend, Trigger::Manual).unwrap();

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
    let first = snap(&backend, Trigger::Manual).unwrap();
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
    let c1 = snap(&backend, Trigger::Manual).unwrap();
    write(tmp.path(), "a.txt", "v2\n");
    let c2 = snap(&backend, Trigger::Manual).unwrap();

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
    let c1 = snap(&backend, Trigger::Manual).unwrap();
    write(tmp.path(), "a.txt", "2\n");
    write(tmp.path(), "b.txt", "2\n");
    let c2 = snap(&backend, Trigger::Manual).unwrap();

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

// --- fetch / reconcile / push ----------------------------------------------

/// A working repo pushed to a shared bare origin, plus a second clone that can
/// move the remote out from under the first.
struct RemoteFixture {
    _origin: TempDir,
    a_tmp: TempDir,
    a: GitBackend,
    _b_tmp: TempDir,
    b: GitBackend,
    b_path: std::path::PathBuf,
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

    let (b_tmp, b) = clone_of(origin.path());
    let b_path = b_tmp.path().join("wc");
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
    let a_local = snap(&fx.a, Trigger::Manual).unwrap();

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

// --- trait object ----------------------------------------------------------

#[test]
fn usable_as_a_boxed_trait_object() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    let boxed: Box<dyn VcsBackend> = Box::new(backend);
    assert_eq!(boxed.is_safe_state().unwrap(), SafeState::Safe);
    let summary = ChangeSummary {
        added: 1,
        notable: vec!["a.txt".to_string()],
        ..Default::default()
    };
    let msg = CommitMessage::new(summary, Trigger::Manual, None);
    assert!(boxed.snapshot(&msg).unwrap().is_some());
}
