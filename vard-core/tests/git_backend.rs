//! Integration tests for the git shell-out backend, exercised against real
//! git repositories created in tempdirs. Network operations run against a
//! local bare repository used as `origin` (a file remote), so nothing here
//! touches the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;
use vard_core::vcs::git::GitBackend;
use vard_core::{
    AdvanceOutcome, LogFilter, PushOutcome, ReconcileOutcome, RestoreTarget, SafeState,
    SecretReason, SecretScanner, SnapshotId, SnapshotOutcome, SnapshotRequest, Trigger,
    UnsafeReason, VcsBackend, VcsError, VcsRef,
};

// --- helpers ---------------------------------------------------------------

/// A network timeout generous enough that a local file-remote fetch/push never
/// approaches it; the dedicated timeout tests drive expiry deliberately.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Snapshots the whole tree with the given trigger, returning the commit it
/// produced (if any). A plain `SnapshotRequest::new` carries no scanner, so
/// these helpers exercise the byte-identical no-quarantine path.
fn snap(backend: &GitBackend, trigger: Trigger) -> Option<SnapshotOutcome> {
    backend
        .snapshot(&SnapshotRequest::new(trigger))
        .unwrap()
        .committed
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

/// A not-yet-existing scratch-worktree path under a fresh tempdir. The `TempDir`
/// is returned so the caller keeps it alive across the reconcile call; the
/// backend creates and removes the linked worktree at the returned path.
fn scratch() -> (TempDir, PathBuf) {
    let holder = TempDir::new().unwrap();
    let path = holder.path().join("scratch");
    (holder, path)
}

/// `git status --porcelain` for `dir`, trimmed. Empty means a clean tree.
fn porcelain(dir: &Path) -> String {
    let out = git(dir, &["status", "--porcelain"]);
    assert!(out.status.success(), "git status failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A TCP endpoint that accepts connections and then stays silent forever, so a
/// git transport dialing it (as a `git://` remote) blocks reading the protocol
/// banner and never returns on its own. Dropping the guard stops the accept
/// loop. Used to drive the fetch/push timeout kill-path hermetically, with no
/// real network.
struct SilentEndpoint {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SilentEndpoint {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            // Hold every accepted connection open and never write a byte.
            let mut held = Vec::new();
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, _)) => held.push(sock),
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        SilentEndpoint {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    /// A `git://` URL naming this endpoint (the path is irrelevant — git never
    /// gets a reply).
    fn url(&self) -> String {
        format!("git://{}/repo", self.addr)
    }
}

impl Drop for SilentEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
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
        scanner: None,
    };
    backend
        .snapshot(&req)
        .unwrap()
        .committed
        .expect("a commit was made");

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
    let report = backend
        .snapshot(&SnapshotRequest::new(Trigger::Interval))
        .unwrap();
    assert_eq!(report.committed, None);
    assert!(report.quarantined.is_empty());
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

// --- secret quarantine (VRD-22) --------------------------------------------

/// A snapshot request carrying an ENABLED scanner with the given extra
/// filename patterns (on top of the built-in catalog).
fn scanning_req(trigger: Trigger, extra: &[&str]) -> SnapshotRequest {
    let patterns: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    let scanner = Arc::new(SecretScanner::compile(true, &patterns).unwrap());
    SnapshotRequest {
        trigger,
        user_text: None,
        extra_trailers: Vec::new(),
        scanner: Some(scanner),
    }
}

/// The names tracked in `HEAD` (the committed tree), sorted.
fn tracked(dir: &Path) -> Vec<String> {
    let out = git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(out.status.success(), "ls-tree failed");
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

#[test]
fn quarantine_withholds_a_token_secret_and_reports_the_match() {
    let (tmp, backend) = new_repo();
    // A file whose CONTENT is a credential (AWS access key), name innocuous.
    write(tmp.path(), "creds.txt", "aws_key = AKIAIOSFODNN7EXAMPLE\n");

    let report = backend
        .snapshot(&scanning_req(Trigger::Event, &[]))
        .unwrap();

    // The secret was the only change, so nothing was committed...
    assert_eq!(report.committed, None);
    // ...but the withheld match is reported, with a content reason and no bytes.
    assert_eq!(report.quarantined.len(), 1);
    assert_eq!(report.quarantined[0].path, PathBuf::from("creds.txt"));
    assert!(matches!(
        report.quarantined[0].reason,
        SecretReason::TokenPrefix { .. }
    ));
    // The file is untracked (never staged), and still on disk unharmed.
    assert!(
        git(tmp.path(), &["ls-files", "--error-unmatch", "creds.txt"])
            .status
            .code()
            != Some(0)
    );
    assert!(tmp.path().join("creds.txt").exists());
}

#[test]
fn quarantine_commits_legit_change_and_excludes_the_secret_beside_it() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "app.rs", "fn main() {}\n");
    write(tmp.path(), ".env", "TOKEN=hunter2\n"); // matches the `.env` catalog pattern

    let report = backend
        .snapshot(&scanning_req(Trigger::Manual, &[]))
        .unwrap();

    // The legit file committed; the secret is withheld, not blocked.
    let committed = report.committed.expect("the legit change was committed");
    assert_eq!(committed.summary.added, 1);
    assert_eq!(report.quarantined.len(), 1);
    assert_eq!(report.quarantined[0].path, PathBuf::from(".env"));
    assert!(matches!(
        report.quarantined[0].reason,
        SecretReason::FilenamePattern { .. }
    ));
    // The commit contains app.rs and NOT .env.
    assert_eq!(tracked(tmp.path()), vec!["app.rs".to_string()]);
    // .env remains on disk, untracked.
    assert!(tmp.path().join(".env").exists());
}

#[test]
fn quarantine_only_pass_commits_nothing_then_clears_when_the_secret_is_removed() {
    let (tmp, backend) = new_repo();
    write(
        tmp.path(),
        "id_rsa",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nx\n",
    );

    // Pass 1: only a secret present → no commit, but reported.
    let first = backend
        .snapshot(&scanning_req(Trigger::Event, &[]))
        .unwrap();
    assert_eq!(first.committed, None);
    assert_eq!(first.quarantined.len(), 1);
    assert_eq!(first.quarantined[0].path, PathBuf::from("id_rsa"));

    // The user removes the secret; the next pass is clean with nothing withheld.
    fs::remove_file(tmp.path().join("id_rsa")).unwrap();
    let second = backend
        .snapshot(&scanning_req(Trigger::Event, &[]))
        .unwrap();
    assert_eq!(second.committed, None);
    assert!(second.quarantined.is_empty());
}

#[test]
fn quarantine_matches_an_extra_pattern_not_in_the_builtin_catalog() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "readme.md", "hello\n");
    write(tmp.path(), "prod.secret", "anything\n"); // only matched via the extra

    let report = backend
        .snapshot(&scanning_req(Trigger::Manual, &["*.secret"]))
        .unwrap();

    assert!(report.committed.is_some());
    assert_eq!(report.quarantined.len(), 1);
    assert_eq!(report.quarantined[0].path, PathBuf::from("prod.secret"));
    match &report.quarantined[0].reason {
        SecretReason::FilenamePattern { pattern } => assert_eq!(pattern, "*.secret"),
        other => panic!("expected FilenamePattern, got {other:?}"),
    }
    assert_eq!(tracked(tmp.path()), vec!["readme.md".to_string()]);
}

#[test]
fn quarantine_recurses_an_untracked_directory_file_by_file() {
    let (tmp, backend) = new_repo();
    fs::create_dir(tmp.path().join(".aws")).unwrap();
    // A `.aws/` dir dropped in wholesale: only its secret member is withheld.
    write(
        tmp.path(),
        ".aws/credentials",
        "aws_key = AKIAIOSFODNN7EXAMPLE\n",
    );
    write(tmp.path(), ".aws/config", "region = us-east-1\n");

    let report = backend
        .snapshot(&scanning_req(Trigger::Event, &[]))
        .unwrap();

    // The non-secret member committed; the secret member is withheld — proof the
    // directory was scanned member by member, not excluded wholesale.
    assert!(report.committed.is_some());
    assert_eq!(report.quarantined.len(), 1);
    assert_eq!(
        report.quarantined[0].path,
        PathBuf::from(".aws/credentials")
    );
    assert_eq!(tracked(tmp.path()), vec![".aws/config".to_string()]);
    assert!(tmp.path().join(".aws/credentials").exists());
}

#[test]
fn a_disabled_scanner_commits_the_secret_verbatim() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), ".env", "TOKEN=AKIAIOSFODNN7EXAMPLE\n");
    // A disabled scanner (secret_scan = false) flags nothing, so the sweep is
    // the plain total add: the secret is committed, none withheld.
    let scanner = Arc::new(SecretScanner::compile(false, &[]).unwrap());
    let req = SnapshotRequest {
        trigger: Trigger::Event,
        user_text: None,
        extra_trailers: Vec::new(),
        scanner: Some(scanner),
    };
    let report = backend.snapshot(&req).unwrap();
    assert!(report.committed.is_some());
    assert!(report.quarantined.is_empty());
    assert_eq!(tracked(tmp.path()), vec![".env".to_string()]);
}

#[test]
fn no_scanner_request_is_byte_identical_and_commits_a_secret() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), ".env", "TOKEN=AKIAIOSFODNN7EXAMPLE\n");
    // `SnapshotRequest::new` carries no scanner: the historical total sweep,
    // which commits the secret and quarantines nothing.
    let report = backend
        .snapshot(&SnapshotRequest::new(Trigger::Event))
        .unwrap();
    assert!(report.committed.is_some());
    assert!(report.quarantined.is_empty());
    assert_eq!(tracked(tmp.path()), vec![".env".to_string()]);
}

#[cfg(unix)]
#[test]
fn quarantine_scans_only_regular_files_never_symlinks() {
    use std::os::unix::fs::symlink;
    let (tmp, backend) = new_repo();
    write(tmp.path(), "real.txt", "just text\n");
    // A symlink whose NAME matches a secret filename pattern (`id_rsa`), pointing
    // at a regular file: git commits it as its link string, never the target's
    // content, so it can never leak — it must be neither scanned nor quarantined.
    symlink("real.txt", tmp.path().join("id_rsa")).unwrap();
    // A DANGLING symlink whose name also matches a pattern (`.env`): same
    // treatment, and it must never be followed (its target does not exist).
    symlink("nowhere", tmp.path().join(".env")).unwrap();

    let report = backend
        .snapshot(&scanning_req(Trigger::Event, &[]))
        .unwrap();

    assert!(
        report.quarantined.is_empty(),
        "symlinks must never be quarantined: {:?}",
        report.quarantined
    );
    let names = tracked(tmp.path());
    assert!(
        names.contains(&"id_rsa".to_string()),
        "the symlink commits as a link: {names:?}"
    );
    assert!(
        names.contains(&".env".to_string()),
        "the dangling symlink commits as a link: {names:?}"
    );
    assert!(names.contains(&"real.txt".to_string()));
    // git stored id_rsa as a symlink (mode 120000) — the link string, not the
    // target's content — direct proof nothing was read through it.
    let ls = git(tmp.path(), &["ls-files", "-s", "id_rsa"]);
    assert!(
        String::from_utf8_lossy(&ls.stdout).starts_with("120000"),
        "id_rsa must be committed as a symlink, got: {}",
        String::from_utf8_lossy(&ls.stdout)
    );
}

#[cfg(unix)]
#[test]
fn quarantine_does_not_hang_or_read_a_fifo() {
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    let (tmp, backend) = new_repo();
    write(tmp.path(), "keep.txt", "hi\n");
    // The real wedge vector: git does not list a bare FIFO as an untracked
    // candidate, but it DOES list a symlink — and a symlink whose name matches
    // no filename pattern previously fell through to the content layer, where
    // a blocking `open()` follows the link into the FIFO and never returns.
    // Only regular files may be scan candidates; the snapshot must return
    // promptly without ever opening the link target.
    let status = Command::new("mkfifo")
        .arg(tmp.path().join("pipe"))
        .status()
        .expect("spawn mkfifo");
    assert!(status.success(), "mkfifo failed");
    symlink("pipe", tmp.path().join("data.bin")).expect("symlink to fifo");

    // Run the snapshot on a worker thread and bound it: on regression (a blocking
    // open of the FIFO) this fails after the timeout instead of hanging forever.
    let (tx, rx) = mpsc::channel();
    let backend2 = backend.clone();
    std::thread::spawn(move || {
        let _ = tx.send(backend2.snapshot(&scanning_req(Trigger::Event, &[])));
    });
    let report = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("snapshot hung on a FIFO (did not return within 10s)")
        .unwrap();

    assert!(
        report.quarantined.is_empty(),
        "a non-regular candidate is never scanned or quarantined"
    );
    // The regular file still commits; the symlink commits as a link string
    // (mode 120000); git itself does not track a bare FIFO.
    assert!(tracked(tmp.path()).contains(&"keep.txt".to_string()));
    let ls = git(tmp.path(), &["ls-files", "-s", "data.bin"]);
    assert!(
        String::from_utf8_lossy(&ls.stdout).starts_with("120000"),
        "data.bin must be committed as a symlink, got: {}",
        String::from_utf8_lossy(&ls.stdout)
    );
}

// Linux-only: macOS (APFS/HFS+) and the BSDs reject non-UTF-8 filenames at the
// filesystem layer (EILSEQ), so the case can only be exercised where the kernel
// permits such names. The code path itself is unix-wide (`bytes_to_path` uses
// `OsStrExt` on every unix); this test just needs a filesystem that will hold
// the name.
#[cfg(target_os = "linux")]
#[test]
fn quarantine_is_byte_exact_under_a_non_utf8_directory() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (tmp, backend) = new_repo();

    // A directory whose name carries a non-UTF-8 byte (legal on Linux).
    let mut dir_name = b"dir-".to_vec();
    dir_name.push(0xFF);
    let dir = tmp.path().join(OsStr::from_bytes(&dir_name));
    fs::create_dir(&dir).unwrap();
    // A valid-UTF-8 secret FILENAME nested under it, with innocuous content, so
    // only the filename layer can catch it: a hit proves path-layer matching
    // works even under a non-UTF-8 parent.
    fs::write(dir.join(".env"), "innocuous\n").unwrap();
    write(tmp.path(), "keep.txt", "hi\n");

    let report = backend
        .snapshot(&scanning_req(Trigger::Event, &[]))
        .unwrap();

    // The legit file committed; exactly one secret was withheld, its path
    // byte-exact (no lossy U+FFFD substitution).
    assert!(report.committed.is_some());
    assert_eq!(
        report.quarantined.len(),
        1,
        "the nested .env must be withheld"
    );
    let mut expected = b"dir-".to_vec();
    expected.push(0xFF);
    expected.extend_from_slice(b"/.env");
    assert_eq!(
        report.quarantined[0].path.as_os_str().as_bytes(),
        &expected[..],
        "the withheld path must be byte-exact"
    );

    // Prove the withhold via the COMMITTED TREE, not the report: the exclusion
    // byte-matched, so the secret is absent from HEAD while keep.txt is present.
    let tree = git(tmp.path(), &["ls-tree", "-r", "-z", "--name-only", "HEAD"]);
    assert!(tree.status.success(), "ls-tree failed");
    let committed: Vec<&[u8]> = tree
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    assert!(
        committed.iter().any(|p| *p == b"keep.txt"),
        "keep.txt must be committed"
    );
    assert!(
        !committed.iter().any(|p| *p == expected.as_slice()),
        "the non-UTF-8 secret path must NOT be in the commit"
    );
    // Still on disk, untracked.
    assert!(dir.join(".env").exists());
}

// --- tracked-file audit (VRD-22) -------------------------------------------

#[test]
fn tracked_files_lists_committed_paths_and_is_empty_on_an_unborn_repo() {
    let (tmp, backend) = new_repo();
    // Nothing committed yet: an unborn HEAD lists no tracked files (not an error).
    assert!(backend.tracked_files().unwrap().is_empty());

    write(tmp.path(), "main.rs", "fn main() {}\n");
    write(
        tmp.path(),
        "id_rsa",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nx\n",
    );
    // Commit both directly (no scanner) so they become TRACKED — the audit's
    // domain, exactly the case quarantine cannot reach (already committed).
    git_ok(tmp.path(), &["add", "-A"]);
    git_ok(tmp.path(), &["commit", "-m", "seed", "--no-verify"]);

    let mut tracked = backend.tracked_files().unwrap();
    tracked.sort();
    assert_eq!(
        tracked,
        vec![PathBuf::from("id_rsa"), PathBuf::from("main.rs")]
    );
}

#[test]
fn audit_reports_a_precommitted_secret_name_and_nothing_on_a_clean_repo() {
    // A pre-committed `id_rsa` is reported by the audit...
    let (tmp, backend) = new_repo();
    write(
        tmp.path(),
        "id_rsa",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nx\n",
    );
    write(tmp.path(), "main.rs", "fn main() {}\n");
    git_ok(tmp.path(), &["add", "-A"]);
    git_ok(tmp.path(), &["commit", "-m", "seed", "--no-verify"]);

    let scanner = SecretScanner::compile(true, &[]).unwrap();
    let hits = scanner.audit_tracked(&backend.tracked_files().unwrap());
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, PathBuf::from("id_rsa"));
    assert!(matches!(
        hits[0].reason,
        SecretReason::FilenamePattern { .. }
    ));

    // ...and a repo with no secret-shaped names audits clean.
    let (tmp2, backend2) = new_repo();
    write(tmp2.path(), "main.rs", "fn main() {}\n");
    git_ok(tmp2.path(), &["add", "-A"]);
    git_ok(tmp2.path(), &["commit", "-m", "seed", "--no-verify"]);
    assert!(
        scanner
            .audit_tracked(&backend2.tracked_files().unwrap())
            .is_empty()
    );
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
            until: None,
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
                until: None,
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
                until: None,
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
        until: None,
        limit: None,
    };
    assert_eq!(backend.log(&at(t)).unwrap().len(), 1, "exact boundary");
    assert_eq!(backend.log(&at(t - 1)).unwrap().len(), 1, "just before");
    assert!(backend.log(&at(t + 1)).unwrap().is_empty(), "just after");
}

#[test]
fn log_until_with_limit_one_picks_newest_at_or_before() {
    // Two commits at pinned times; `--until` + max-count=1 asks git for the
    // newest snapshot as of a past instant, without fetching the whole history.
    let (tmp, backend) = new_repo();
    let commit_at = |msg: &str, content: &str, t: u64| {
        write(tmp.path(), "a.txt", content);
        git_ok(tmp.path(), &["add", "-A"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["commit", "-m", msg])
            .env("GIT_COMMITTER_DATE", format!("@{t} +0000"))
            .env("GIT_AUTHOR_DATE", format!("@{t} +0000"))
            .output()
            .unwrap();
        assert!(out.status.success());
    };
    let early = 1_700_000_000u64;
    let late = early + 3_600;
    commit_at("early", "early\n", early);
    commit_at("late", "late\n", late);

    let newest_by = |secs: u64| {
        backend
            .log(&LogFilter {
                since: None,
                until: Some(UNIX_EPOCH + Duration::from_secs(secs)),
                limit: Some(1),
            })
            .unwrap()
    };
    // Between the two commits: the early one is the newest at-or-before.
    let mid = newest_by(early + 60);
    assert_eq!(mid.len(), 1);
    assert_eq!(mid[0].subject, "early");
    // At or after the late commit: the late one wins.
    assert_eq!(newest_by(late).len(), 1);
    // Before either commit: nothing.
    assert!(newest_by(early - 1).is_empty());
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
        .diff(&VcsRef::from(&first), Some(&VcsRef::from(&second)), None)
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
    let diff = backend.diff(&VcsRef::from(&first), None, None).unwrap();
    assert!(diff.contains("+working"), "diff was: {diff}");
}

#[test]
fn diff_scoped_to_a_literal_path_with_spaces() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "keep.txt", "keep\n");
    write(tmp.path(), "with space.txt", "old\n");
    let first = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "keep.txt", "changed\n");
    write(tmp.path(), "with space.txt", "new\n");

    // A literal pathspec scopes the diff to just the space-containing file, and
    // the other changed file is excluded.
    let scoped = backend
        .diff(
            &VcsRef::from(&first),
            None,
            Some(std::path::Path::new("with space.txt")),
        )
        .unwrap();
    assert!(scoped.contains("with space.txt"), "diff was: {scoped}");
    assert!(scoped.contains("+new"), "diff was: {scoped}");
    assert!(!scoped.contains("keep.txt"), "diff was: {scoped}");
}

#[test]
fn verify_ref_distinguishes_real_and_bogus_revisions() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    let c1 = snap_id(&backend, Trigger::Manual);
    assert!(backend.verify_ref(&VcsRef::from(&c1)).unwrap());
    assert!(backend.verify_ref(&VcsRef::new("HEAD")).unwrap());
    assert!(!backend.verify_ref(&VcsRef::new("deadbeef")).unwrap());
    assert!(!backend.verify_ref(&VcsRef::new("no-such-branch")).unwrap());
}

#[test]
fn path_exists_at_reports_presence_at_a_revision() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "a.txt", "1\n");
    let c1 = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "later.txt", "new\n");
    let c2 = snap_id(&backend, Trigger::Manual);

    assert!(
        backend
            .path_exists_at(&VcsRef::from(&c1), std::path::Path::new("a.txt"))
            .unwrap()
    );
    // later.txt was added after c1, so it is absent there but present at c2.
    assert!(
        !backend
            .path_exists_at(&VcsRef::from(&c1), std::path::Path::new("later.txt"))
            .unwrap()
    );
    assert!(
        backend
            .path_exists_at(&VcsRef::from(&c2), std::path::Path::new("later.txt"))
            .unwrap()
    );
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
    assert_eq!(a.push(TEST_TIMEOUT).unwrap(), PushOutcome::Pushed);

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
    assert_eq!(fx.a.push(TEST_TIMEOUT).unwrap(), PushOutcome::Pushed);
    // Nothing new to send.
    assert_eq!(fx.a.push(TEST_TIMEOUT).unwrap(), PushOutcome::UpToDate);
}

#[test]
fn push_reports_non_fast_forward_after_a_competing_push() {
    let fx = remote_fixture();
    // B advances origin.
    write(&fx.b_path, "file.txt", "from-b\n");
    snap(&fx.b, Trigger::Manual);
    assert_eq!(fx.b.push(TEST_TIMEOUT).unwrap(), PushOutcome::Pushed);

    // A commits on the stale base and pushes without fetching: rejected.
    write(fx.a_tmp.path(), "file.txt", "from-a\n");
    snap(&fx.a, Trigger::Manual);
    assert_eq!(
        fx.a.push(TEST_TIMEOUT).unwrap(),
        PushOutcome::NonFastForward
    );

    // The same race after fetching reports the other porcelain spelling
    // ("non-fast-forward" instead of "fetch first") — both are the race.
    fx.a.fetch(TEST_TIMEOUT).unwrap();
    assert_eq!(
        fx.a.push(TEST_TIMEOUT).unwrap(),
        PushOutcome::NonFastForward
    );
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
    match backend.push(TEST_TIMEOUT) {
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

    let state = backend.fetch(TEST_TIMEOUT).unwrap();
    assert!(!state.remote_moved);
    assert_eq!((state.ahead, state.behind), (1, 0));
    assert!(
        !state.stale_tracking_ref,
        "a never-pushed branch has no tracking ref to distrust: the push-time \
         count (ahead_of_upstream) stays in effect"
    );

    // Even with an unborn local branch it is a state, not an error.
    let (tmp2, backend2) = new_repo();
    git_ok(
        tmp2.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    let state = backend2.fetch(TEST_TIMEOUT).unwrap();
    assert_eq!(
        (state.remote_moved, state.ahead, state.behind),
        (false, 0, 0)
    );
}

#[test]
fn ahead_of_upstream_counts_commits_since_the_last_fetch() {
    // The local-only ahead read against the tracking ref the last fetch left.
    // Commits landing AFTER a fetch are included — the number a push right now
    // would send, which is what keeps the push-time commit count accurate.
    let fx = remote_fixture(); // base commit pushed; clone `b` tracks it
    assert_eq!(
        fx.b.ahead_of_upstream().unwrap(),
        Some(0),
        "freshly cloned: nothing to send"
    );
    write(&fx.b_path, "a", "1\n");
    snap_id(&fx.b, Trigger::Manual);
    write(&fx.b_path, "b", "2\n");
    snap_id(&fx.b, Trigger::Manual);
    assert_eq!(
        fx.b.ahead_of_upstream().unwrap(),
        Some(2),
        "two commits since the clone's implicit fetch"
    );

    // A manual `git push` updates the tracking ref too, so the push-time count
    // stays accurate (zero) even for commits the user pushed by hand rather
    // than through a fetch of ours.
    assert_eq!(fx.b.push(TEST_TIMEOUT).unwrap(), PushOutcome::Pushed);
    assert_eq!(
        fx.b.ahead_of_upstream().unwrap(),
        Some(0),
        "a manual push moves the tracking ref, keeping the count accurate"
    );

    // A never-pushed branch (no tracking ref): ahead by its whole history —
    // a trustworthy local read, so the push-time count applies there too.
    let origin2 = bare_origin();
    let (tmp2, backend2) = new_repo();
    git_ok(
        tmp2.path(),
        &["remote", "add", "origin", origin2.path().to_str().unwrap()],
    );
    write(tmp2.path(), "f", "x\n");
    snap_id(&backend2, Trigger::Manual);
    assert_eq!(backend2.ahead_of_upstream().unwrap(), Some(1));
}

#[test]
fn a_deleted_remote_branch_flags_the_stale_tracking_ref_with_the_full_history_count() {
    // The remote branch is deleted after a normal synced state: the next fetch
    // reports the missing remote ref via `stale_tracking_ref` (a local tracking
    // ref survives from before the deletion), with ahead = the FULL history
    // (what the next push recreates). ZERO mutation: the stale tracking ref is
    // deliberately left alone (deleting it from this lock-free path would run
    // user reference-transaction hooks and race concurrent fetches); the flag
    // is what tells callers not to trust local reads against it for this cycle.
    let fx = remote_fixture();
    // `b` synced at the base commit, tracking ref present. Add one local commit.
    write(&fx.b_path, "extra.txt", "x\n");
    snap_id(&fx.b, Trigger::Manual);

    // Delete the branch on the remote out from under `b` (directly on the bare
    // repository — a push deletion of the current branch is denied by default).
    git_ok(fx._origin.path(), &["update-ref", "-d", "refs/heads/main"]);

    let state = fx.b.fetch(TEST_TIMEOUT).unwrap();
    assert!(
        state.stale_tracking_ref,
        "the surviving pre-deletion tracking ref is flagged"
    );
    assert_eq!(
        (state.ahead, state.behind),
        (2, 0),
        "ahead is the FULL history the next push recreates (base + extra)"
    );
    // The stale tracking ref is LEFT ALONE (no ref mutation outside a locked
    // window) — the flag, not a prune, carries the truth.
    let out = git(
        &fx.b_path,
        &["rev-parse", "--verify", "refs/remotes/origin/main"],
    );
    assert!(
        out.status.success(),
        "the stale tracking ref is deliberately not pruned"
    );
}

#[test]
fn fetch_reports_remote_movement_and_behind_count() {
    let fx = remote_fixture();
    // Before any remote movement.
    let before = fx.a.fetch(TEST_TIMEOUT).unwrap();
    assert!(!before.remote_moved);
    assert_eq!((before.ahead, before.behind), (0, 0));

    // B advances origin by one commit.
    write(&fx.b_path, "file.txt", "from-b\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();

    let after = fx.a.fetch(TEST_TIMEOUT).unwrap();
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
    fx.b.push(TEST_TIMEOUT).unwrap();

    let state = fx.a.fetch(TEST_TIMEOUT).unwrap();
    assert!(state.remote_moved);
    assert_eq!(state.behind, 1);
}

#[test]
fn reconcile_already_up_to_date_when_nothing_moved() {
    let fx = remote_fixture();
    fx.a.fetch(TEST_TIMEOUT).unwrap();
    let head_before = rev(fx.a_tmp.path(), "HEAD");

    let (_h, scr) = scratch();
    assert_eq!(
        fx.a.reconcile(&scr).unwrap(),
        ReconcileOutcome::AlreadyUpToDate
    );

    // Nothing moved and the scratch worktree was cleaned up.
    assert_eq!(rev(fx.a_tmp.path(), "HEAD"), head_before);
    assert!(!scr.exists(), "scratch worktree must be removed");
}

#[test]
fn reconcile_rebases_cleanly_onto_a_moved_remote() {
    let fx = remote_fixture();
    // B advances origin (a file A does not touch → clean).
    write(&fx.b_path, "other.txt", "b-only\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();
    let remote_head = rev(&fx.b_path, "HEAD");
    let branch_before = rev(fx.a_tmp.path(), "refs/heads/main");

    fx.a.fetch(TEST_TIMEOUT).unwrap();
    let (_h, scr) = scratch();
    match fx.a.reconcile(&scr).unwrap() {
        ReconcileOutcome::Rebased { new_head } => {
            // With no local commits the rebase fast-forwards to the remote tip,
            // but the branch ref itself is NOT moved — that is advance's job.
            assert_eq!(new_head.as_str(), remote_head);
        }
        other => panic!("expected Rebased, got {other:?}"),
    }
    assert_eq!(rev(fx.a_tmp.path(), "refs/heads/main"), branch_before);
    assert!(!scr.exists());
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_out_of_tree_then_advance_lands_both_sides() {
    // The clean-path acceptance: local and remote diverge, reconcile rebases
    // out of tree leaving the user's tree/HEAD provably unmoved, and advance
    // then makes the result live with both sides' changes present.
    let fx = remote_fixture();
    // B advances origin with a non-conflicting file.
    write(&fx.b_path, "b.txt", "from-b\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();

    // A commits its own change (so reconcile must replay it, not just ff).
    write(fx.a_tmp.path(), "a.txt", "from-a\n");
    let a_local = snap_id(&fx.a, Trigger::Manual);

    fx.a.fetch(TEST_TIMEOUT).unwrap();
    let head_before = rev(fx.a_tmp.path(), "HEAD");
    let tree_before = rev(fx.a_tmp.path(), "HEAD^{tree}");

    let (_h, scr) = scratch();
    let new_head = match fx.a.reconcile(&scr).unwrap() {
        ReconcileOutcome::Rebased { new_head } => new_head,
        other => panic!("expected Rebased, got {other:?}"),
    };

    // Reconcile moved nothing in the user's repo: branch, HEAD, and tree are
    // exactly as before; the rebased tip exists only in the object store.
    assert_eq!(rev(fx.a_tmp.path(), "refs/heads/main"), a_local.as_str());
    assert_eq!(rev(fx.a_tmp.path(), "HEAD"), head_before);
    assert_eq!(rev(fx.a_tmp.path(), "HEAD^{tree}"), tree_before);
    assert_ne!(new_head.as_str(), a_local.as_str());
    assert!(commit_exists(fx.a_tmp.path(), new_head.as_str()));
    assert!(
        porcelain(fx.a_tmp.path()).is_empty(),
        "main tree must be clean"
    );
    assert!(!scr.exists(), "scratch worktree removed");

    // Advance lands it: branch and tree move to the rebased tip, still clean.
    // The expected tip is the branch tip the reconcile consumed (a_local).
    assert_eq!(
        fx.a.advance(&new_head, &a_local).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(rev(fx.a_tmp.path(), "refs/heads/main"), new_head.as_str());
    assert!(
        porcelain(fx.a_tmp.path()).is_empty(),
        "tree clean after advance"
    );
    // Both sides' changes are present in the working tree.
    assert_eq!(
        fs::read_to_string(fx.a_tmp.path().join("a.txt")).unwrap(),
        "from-a\n"
    );
    assert_eq!(
        fs::read_to_string(fx.a_tmp.path().join("b.txt")).unwrap(),
        "from-b\n"
    );
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_conflict_leaves_main_untouched_and_removes_scratch() {
    // The conflict-path acceptance: the rebase only ever runs in the scratch
    // worktree, so a conflict leaves the user's repo provably untouched, with
    // no rebase state anywhere under the main worktree's git dir.
    let fx = remote_fixture();
    // B changes the shared file and advances origin.
    write(&fx.b_path, "file.txt", "b-change\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();

    // A changes the same line locally (committed, not pushed) → rebase conflicts.
    write(fx.a_tmp.path(), "file.txt", "a-change\n");
    let a_local = snap_id(&fx.a, Trigger::Manual);
    let head_before = rev(fx.a_tmp.path(), "HEAD");

    fx.a.fetch(TEST_TIMEOUT).unwrap();
    let (_h, scr) = scratch();
    assert_eq!(fx.a.reconcile(&scr).unwrap(), ReconcileOutcome::Conflict);

    // Branch and HEAD are exactly where they were; the file is byte-identical.
    assert_eq!(rev(fx.a_tmp.path(), "refs/heads/main"), a_local.as_str());
    assert_eq!(rev(fx.a_tmp.path(), "HEAD"), head_before);
    let contents = fs::read_to_string(fx.a_tmp.path().join("file.txt")).unwrap();
    assert_eq!(contents, "a-change\n");
    assert!(!contents.contains("<<<<<<<"));
    // No rebase state under the MAIN worktree's git dir, and the scratch
    // worktree (and its metadata) is gone.
    assert!(!git_dir(fx.a_tmp.path()).join("rebase-merge").exists());
    assert!(!git_dir(fx.a_tmp.path()).join("rebase-apply").exists());
    assert!(
        !scr.exists(),
        "scratch worktree must be removed on conflict"
    );
    assert!(
        !git_dir(fx.a_tmp.path()).join("worktrees/scratch").exists(),
        "scratch worktree metadata must be gone"
    );
    assert!(
        porcelain(fx.a_tmp.path()).is_empty(),
        "main tree stays clean"
    );
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_with_a_broken_signer_still_rebases() {
    // A broken signer must not derail the out-of-tree replay: the scratch
    // rebase pins commit.gpgsign=false, so commit machinery during the replay
    // never invokes the failing signer.
    let fx = remote_fixture();
    write(&fx.b_path, "other.txt", "b-only\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();

    // A has a local commit (so the rebase must replay → commit machinery) and
    // a broken signing config, as a user's repo might. Config is shared with
    // the linked scratch worktree, so the pin is what saves the replay.
    write(fx.a_tmp.path(), "mine.txt", "a-only\n");
    snap(&fx.a, Trigger::Manual);
    git_ok(fx.a_tmp.path(), &["config", "commit.gpgsign", "true"]);
    git_ok(
        fx.a_tmp.path(),
        &["config", "gpg.program", "/usr/bin/false"],
    );

    fx.a.fetch(TEST_TIMEOUT).unwrap();
    let (_h, scr) = scratch();
    match fx.a.reconcile(&scr).unwrap() {
        ReconcileOutcome::Rebased { .. } => {}
        other => panic!("expected Rebased, got {other:?}"),
    }
    assert!(!scr.exists());
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_and_advance_ignore_a_failing_post_checkout_hook() {
    // A user's post-checkout hook must never run under vard's automated sync:
    // `worktree add` (reconcile) and `checkout -B` (advance) both pin
    // core.hooksPath=/dev/null, so a hook that would fail (or run arbitrary code
    // from a background daemon) cannot break the cycle.
    let fx = remote_fixture();
    // A diverging remote so reconcile actually rebases, and a local commit.
    write(&fx.b_path, "other.txt", "b-only\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();
    write(fx.a_tmp.path(), "mine.txt", "a-only\n");
    let a_local = snap_id(&fx.a, Trigger::Manual);

    // Install a post-checkout hook that always fails. Hooks live under the shared
    // git dir, so this covers the linked scratch worktree too.
    let hooks = git_dir(fx.a_tmp.path()).join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    fs::write(&hook, "#!/bin/sh\necho 'hook ran' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fx.a.fetch(TEST_TIMEOUT).unwrap();
    let (_h, scr) = scratch();
    let new_head = match fx.a.reconcile(&scr).unwrap() {
        ReconcileOutcome::Rebased { new_head } => new_head,
        other => panic!("expected Rebased despite the hook, got {other:?}"),
    };
    assert!(!scr.exists());
    // Advance lands cleanly despite the failing hook.
    assert_eq!(
        fx.a.advance(&new_head, &a_local).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(rev(fx.a_tmp.path(), "refs/heads/main"), new_head.as_str());
    assert!(porcelain(fx.a_tmp.path()).is_empty());
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_leaves_a_dirty_main_tree_untouched() {
    // The out-of-tree rebase runs in a clean scratch worktree, so a dirty main
    // tree is neither stashed, popped, nor clobbered — even with autostash on,
    // which an in-tree rebase could have used to pop conflict markers into it.
    let fx = remote_fixture();
    write(&fx.b_path, "file.txt", "remote-change\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();

    git_ok(fx.a_tmp.path(), &["config", "rebase.autostash", "true"]);
    write(fx.a_tmp.path(), "file.txt", "dirty-local\n"); // uncommitted
    let branch_before = rev(fx.a_tmp.path(), "refs/heads/main");

    fx.a.fetch(TEST_TIMEOUT).unwrap();
    let (_h, scr) = scratch();
    let outcome = fx.a.reconcile(&scr).unwrap();

    // The branch reconciled out of tree (a fast-forward onto the remote tip)...
    assert!(
        matches!(outcome, ReconcileOutcome::Rebased { .. }),
        "expected Rebased, got {outcome:?}"
    );
    // ...while the user's branch ref never moved and the dirty file is intact.
    assert_eq!(rev(fx.a_tmp.path(), "refs/heads/main"), branch_before);
    let contents = fs::read_to_string(fx.a_tmp.path().join("file.txt")).unwrap();
    assert_eq!(contents, "dirty-local\n", "dirty content must be untouched");
    assert!(!contents.contains("<<<<<<<"));
    assert!(!scr.exists());
}

#[test]
fn reconcile_non_conflict_failure_is_not_reported_as_conflict() {
    // A missing upstream ref (never fetched) fails the scratch rebase without
    // leaving a rebase in progress: that is a CommandFailed, not a Conflict.
    let origin = bare_origin();
    let (tmp, backend) = new_repo();
    git_ok(
        tmp.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    write(tmp.path(), "f", "x\n");
    snap(&backend, Trigger::Manual);

    let (_h, scr) = scratch();
    match backend.reconcile(&scr) {
        Err(VcsError::CommandFailed { op, .. }) => assert_eq!(op, "rebase"),
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    // Even on this error path the scratch worktree is cleaned up.
    assert!(!scr.exists(), "scratch worktree must be removed on error");
    assert_eq!(backend.is_safe_state().unwrap(), SafeState::Safe);
}

#[test]
fn reconcile_refuses_an_unsafe_repo() {
    let fx = remote_fixture();
    fs::write(git_dir(fx.a_tmp.path()).join("MERGE_HEAD"), "sentinel\n").unwrap();
    let (_h, scr) = scratch();
    match fx.a.reconcile(&scr) {
        Err(VcsError::UnsafeState(UnsafeReason::MergeInProgress)) => {}
        other => panic!("expected UnsafeState(MergeInProgress), got {other:?}"),
    }
    // Refused before any worktree was created.
    assert!(!scr.exists());
}

// --- advance ---------------------------------------------------------------

#[test]
fn advance_is_idempotent_to_current_head() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "x\n");
    let head = snap_id(&backend, Trigger::Manual);

    // Advancing to the current tip (expected == current) is a clean no-op.
    assert_eq!(
        backend.advance(&head, &head).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(rev(tmp.path(), "HEAD"), head.as_str());
    assert_eq!(rev(tmp.path(), "refs/heads/main"), head.as_str());
    assert!(porcelain(tmp.path()).is_empty());

    // Still a no-op on a second call.
    assert_eq!(
        backend.advance(&head, &head).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(rev(tmp.path(), "HEAD"), head.as_str());
}

#[test]
fn advance_moves_the_branch_and_tree_forward() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "one\n");
    let first = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "f", "two\n");
    let second = snap_id(&backend, Trigger::Manual);

    // Roll the branch/tree back to the first commit (expected tip is `second`),
    // then forward again (expected tip is now `first`). A clean tree is never a
    // clobber.
    assert_eq!(
        backend.advance(&first, &second).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(rev(tmp.path(), "refs/heads/main"), first.as_str());
    assert_eq!(fs::read_to_string(tmp.path().join("f")).unwrap(), "one\n");
    assert!(porcelain(tmp.path()).is_empty());

    assert_eq!(
        backend.advance(&second, &first).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(rev(tmp.path(), "refs/heads/main"), second.as_str());
    assert_eq!(fs::read_to_string(tmp.path().join("f")).unwrap(), "two\n");
}

#[test]
fn advance_rejects_a_missing_target() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "x\n");
    let head = snap_id(&backend, Trigger::Manual);

    let bogus = SnapshotId::new("0".repeat(40));
    match backend.advance(&bogus, &head) {
        Err(VcsError::CommandFailed { op, .. }) => assert_eq!(op, "advance"),
        other => panic!("expected CommandFailed for a missing target, got {other:?}"),
    }
    // Nothing moved: the target was verified before anything was checked out.
    assert_eq!(rev(tmp.path(), "HEAD"), head.as_str());
}

#[test]
fn advance_reports_lock_contention() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "one\n");
    let first = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "f", "two\n");
    let second = snap_id(&backend, Trigger::Manual);

    // A held index.lock makes the checkout that would move the tree back to
    // `first` fail as lock contention (attributed to the checkout).
    fs::write(git_dir(tmp.path()).join("index.lock"), "").unwrap();
    match backend.advance(&first, &second) {
        Err(VcsError::LockContended { op }) => assert_eq!(op, "checkout"),
        other => panic!("expected LockContended, got {other:?}"),
    }
    assert!(git_dir(tmp.path()).join("index.lock").exists());
}

#[test]
fn advance_refuses_an_unsafe_repo() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "x\n");
    let head = snap_id(&backend, Trigger::Manual);
    fs::write(git_dir(tmp.path()).join("MERGE_HEAD"), "sentinel\n").unwrap();
    match backend.advance(&head, &head) {
        Err(VcsError::UnsafeState(UnsafeReason::MergeInProgress)) => {}
        other => panic!("expected UnsafeState(MergeInProgress), got {other:?}"),
    }
}

#[test]
fn advance_refuses_a_conflicting_tracked_modification() {
    // A tracked file locally modified where the target also changed it: advance
    // must REFUSE (WouldClobber) rather than destroy the uncommitted edit.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "one\n");
    let first = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "f", "two\n");
    let second = snap_id(&backend, Trigger::Manual); // branch tip

    // Uncommitted local change to the same file the target (`first`) differs on.
    write(tmp.path(), "f", "dirty-local\n");
    assert_eq!(
        backend.advance(&first, &second).unwrap(),
        AdvanceOutcome::WouldClobber
    );
    // Tree and branch are exactly as they were; the edit survives.
    assert_eq!(rev(tmp.path(), "refs/heads/main"), second.as_str());
    assert_eq!(
        fs::read_to_string(tmp.path().join("f")).unwrap(),
        "dirty-local\n"
    );
}

#[test]
fn advance_refuses_a_conflicting_untracked_file() {
    // An untracked file the target would introduce: advance must REFUSE rather
    // than overwrite the user's untracked file.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "keep", "x\n");
    let first = snap_id(&backend, Trigger::Manual);
    // `second` adds `extra`, so rolling forward to it would create that path.
    write(tmp.path(), "extra", "from-commit\n");
    let second = snap_id(&backend, Trigger::Manual);
    // Go back to `first` (which lacks `extra`)...
    assert_eq!(
        backend.advance(&first, &second).unwrap(),
        AdvanceOutcome::Advanced
    );
    // ...then leave an UNTRACKED `extra` and try to advance forward to `second`,
    // which would overwrite it.
    write(tmp.path(), "extra", "my-untracked\n");
    assert_eq!(
        backend.advance(&second, &first).unwrap(),
        AdvanceOutcome::WouldClobber
    );
    assert_eq!(rev(tmp.path(), "refs/heads/main"), first.as_str());
    assert_eq!(
        fs::read_to_string(tmp.path().join("extra")).unwrap(),
        "my-untracked\n"
    );
}

#[test]
fn advance_refuses_a_conflicting_gitignored_file() {
    // Data-loss hole (finding 1): a remote commit adding a path that is a LOCAL
    // gitignored file must not silently clobber the local copy. The pre-sync
    // snapshot never captures an ignored file, so advance's `--no-overwrite-ignore`
    // is the only guard — it must REFUSE (WouldClobber), leaving the local file
    // intact, exactly like the untracked-file case.
    let (tmp, backend) = new_repo();
    write(tmp.path(), ".gitignore", ".env\n");
    let first = snap_id(&backend, Trigger::Manual);
    // `second` adds a TRACKED `.env` (as a remote commit would). Force-add it so
    // the ignore rule does not skip it.
    write(tmp.path(), ".env", "from-commit\n");
    git_ok(tmp.path(), &["add", "-f", ".env"]);
    let second = snap_id(&backend, Trigger::Manual);

    // Go back to `first` (which has `.env` only as an ignored file)...
    assert_eq!(
        backend.advance(&first, &second).unwrap(),
        AdvanceOutcome::Advanced
    );
    // ...leave a LOCAL gitignored `.env` with private content, and try to advance
    // forward to `second`, which tracks `.env` — this would clobber the local file.
    write(tmp.path(), ".env", "LOCAL_SECRET\n");
    assert!(
        porcelain(tmp.path()).is_empty(),
        "the local .env is ignored, so the tree reads clean"
    );
    assert_eq!(
        backend.advance(&second, &first).unwrap(),
        AdvanceOutcome::WouldClobber,
        "advancing over a local gitignored file must refuse, not overwrite it"
    );
    // Branch unmoved and the local secret intact.
    assert_eq!(rev(tmp.path(), "refs/heads/main"), first.as_str());
    assert_eq!(
        fs::read_to_string(tmp.path().join(".env")).unwrap(),
        "LOCAL_SECRET\n"
    );
}

#[test]
fn advance_carries_a_non_conflicting_dirty_file_over() {
    // A dirty file the target does NOT touch is carried over unharmed: advance
    // succeeds and the local edit survives.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "one\n");
    write(tmp.path(), "other", "base\n");
    let first = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "f", "two\n"); // only `f` changes between first and second
    let second = snap_id(&backend, Trigger::Manual);

    // Uncommitted edit to `other`, which neither commit differs on.
    write(tmp.path(), "other", "dirty-but-safe\n");
    assert_eq!(
        backend.advance(&first, &second).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(rev(tmp.path(), "refs/heads/main"), first.as_str());
    assert_eq!(fs::read_to_string(tmp.path().join("f")).unwrap(), "one\n");
    // The non-conflicting edit was carried over.
    assert_eq!(
        fs::read_to_string(tmp.path().join("other")).unwrap(),
        "dirty-but-safe\n"
    );
}

#[test]
fn advance_refuses_when_the_branch_tip_moved() {
    // The addendum guard: a commit the user landed on the branch after the
    // reconcile read its tip must not be stranded. advance refuses when the
    // current branch tip no longer equals the expected tip.
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "one\n");
    let first = snap_id(&backend, Trigger::Manual);
    write(tmp.path(), "g", "two\n");
    let second = snap_id(&backend, Trigger::Manual); // the user's raced commit

    // Reconcile "consumed" `first` as the expected tip, but the branch is now at
    // `second`. Advancing to some target with the stale expectation refuses.
    assert_eq!(
        backend.advance(&first, &first).unwrap(),
        AdvanceOutcome::WouldClobber
    );
    // The user's commit is intact on the branch.
    assert_eq!(rev(tmp.path(), "refs/heads/main"), second.as_str());
    assert!(commit_exists(tmp.path(), second.as_str()));
}

#[test]
fn has_remote_reflects_the_configured_remote() {
    let origin = bare_origin();
    let (tmp, _backend) = new_repo();
    // No remote configured yet.
    let backend = GitBackend::open(tmp.path(), "main", "origin").unwrap();
    assert!(!backend.has_remote().unwrap(), "no remote configured");

    git_ok(
        tmp.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    assert!(backend.has_remote().unwrap(), "remote now present");

    // A backend configured for a *different* remote name still reports absent.
    let other = GitBackend::open(tmp.path(), "main", "upstream").unwrap();
    assert!(!other.has_remote().unwrap(), "different remote name absent");
}

// --- scratch-worktree pruning (crash recovery) -----------------------------

#[test]
fn prune_scratch_is_a_no_op_when_absent() {
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "x\n");
    snap(&backend, Trigger::Manual);

    // No scratch worktree has ever existed: pruning is a clean no-op.
    let (_h, scr) = scratch();
    backend.prune_scratch(&scr).unwrap();
    // Idempotent — safe to call again.
    backend.prune_scratch(&scr).unwrap();
}

#[test]
fn prune_scratch_recovers_a_crashed_mid_rebase_then_reconcile_succeeds() {
    // Crash simulation: a scratch worktree is created and left mid-rebase (as a
    // killed/abandoned process would), then the prune primitive reclaims it and
    // a subsequent reconcile runs to a clean outcome.
    let fx = remote_fixture();
    // B changes the shared file so a replay of A's local change would conflict.
    write(&fx.b_path, "file.txt", "b-change\n");
    snap(&fx.b, Trigger::Manual);
    fx.b.push(TEST_TIMEOUT).unwrap();
    write(fx.a_tmp.path(), "file.txt", "a-change\n");
    snap(&fx.a, Trigger::Manual);
    fx.a.fetch(TEST_TIMEOUT).unwrap();

    // Hand-build a scratch worktree and leave it mid-rebase — the state a crash
    // during reconcile would abandon.
    let (_h, scr) = scratch();
    git_ok(
        fx.a_tmp.path(),
        &["worktree", "add", "--detach", scr.to_str().unwrap(), "main"],
    );
    let _ = Command::new("git")
        .arg("-C")
        .arg(&scr)
        .args(["-c", "rebase.autostash=false", "rebase", "origin/main"])
        .output()
        .expect("failed to spawn git rebase");
    let meta = git_dir(fx.a_tmp.path()).join("worktrees/scratch");
    assert!(
        meta.join("rebase-merge").exists(),
        "precondition: scratch left mid-rebase"
    );

    // Crash recovery: prune force-removes the worktree and reaps its metadata.
    fx.a.prune_scratch(&scr).unwrap();
    assert!(!scr.exists(), "scratch dir removed");
    assert!(!meta.exists(), "scratch worktree metadata pruned");

    // A subsequent reconcile now works end-to-end at the same path (this pair
    // of changes conflicts, so the defined outcome is Conflict — the point is
    // that the machinery runs and cleans up, proving recovery).
    let outcome = fx.a.reconcile(&scr).unwrap();
    assert_eq!(outcome, ReconcileOutcome::Conflict);
    assert!(!scr.exists(), "scratch removed after the retry");
    assert_eq!(fx.a.is_safe_state().unwrap(), SafeState::Safe);
}

// --- network timeouts ------------------------------------------------------

#[test]
fn fetch_times_out_against_a_silent_endpoint() {
    // A git:// remote that accepts but never replies makes fetch block reading;
    // the timeout must kill it and return VcsError::Timeout promptly.
    let endpoint = SilentEndpoint::start();
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "x\n");
    snap(&backend, Trigger::Manual);
    git_ok(tmp.path(), &["remote", "add", "origin", &endpoint.url()]);

    let started = Instant::now();
    match backend.fetch(Duration::from_millis(750)) {
        Err(VcsError::Timeout { op, elapsed }) => {
            assert_eq!(op, "fetch");
            assert!(
                elapsed >= Duration::from_millis(700),
                "elapsed under the budget: {elapsed:?}"
            );
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
    // The kill is prompt — nowhere near a hang.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "kill was not prompt: {:?}",
        started.elapsed()
    );

    // The child was reaped (no zombie): a fresh, well-formed operation still
    // works against a real file remote right after.
    let origin = bare_origin();
    git_ok(
        tmp.path(),
        &[
            "remote",
            "set-url",
            "origin",
            origin.path().to_str().unwrap(),
        ],
    );
    assert_eq!(backend.push(TEST_TIMEOUT).unwrap(), PushOutcome::Pushed);
}

#[test]
fn push_times_out_against_a_silent_endpoint() {
    let endpoint = SilentEndpoint::start();
    let (tmp, backend) = new_repo();
    write(tmp.path(), "f", "x\n");
    snap(&backend, Trigger::Manual);
    git_ok(tmp.path(), &["remote", "add", "origin", &endpoint.url()]);

    let started = Instant::now();
    match backend.push(Duration::from_millis(750)) {
        Err(VcsError::Timeout { op, elapsed }) => {
            assert_eq!(op, "push");
            assert!(
                elapsed >= Duration::from_millis(700),
                "elapsed under the budget: {elapsed:?}"
            );
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "kill was not prompt: {:?}",
        started.elapsed()
    );
}
