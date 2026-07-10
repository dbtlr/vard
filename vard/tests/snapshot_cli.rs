//! End-to-end tests for `vard snapshot / log / diff / restore` (VRD-16),
//! driving the real binary against tempdir-isolated config, state, and HOME.
//! Nothing here touches the developer's real environment; repositories use a
//! throwaway global git config so commits are deterministic in CI.
//!
//! The daemon-request dispatch path (a snapshot handed to a running `vard run`)
//! is covered by the daemon's own integration test and the `scripts/ci-smoke.sh`
//! live round-trip; these tests exercise the no-daemon in-process path and the
//! read-only commands, where behavior is deterministic without a background
//! process.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;
use common::{Env, stderr, stdout};

/// Runs `git -C <repo> <args>` against the throwaway global git config.
fn git_in(env: &Env, repo: &Path, args: &[&str]) -> Output {
    let mut full = vec!["-C", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new("git")
        .args(&full)
        .env("GIT_CONFIG_GLOBAL", &env.git_config)
        .output()
        .expect("spawn git")
}

/// Adds a watch named `name` at a fresh `--init` repository and returns its path.
fn add_watch(env: &Env, name: &str) -> PathBuf {
    let path = env.root.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    let out = env.vard(&[
        "watch",
        "add",
        path.to_str().unwrap(),
        "--name",
        name,
        "--init",
    ]);
    assert!(out.status.success(), "watch add failed: {}", stderr(&out));
    std::fs::canonicalize(&path).unwrap()
}

fn commit_count(env: &Env, repo: &Path) -> usize {
    let out = git_in(env, repo, &["rev-list", "--count", "HEAD"]);
    if !out.status.success() {
        return 0;
    }
    stdout(&out).trim().parse().unwrap_or(0)
}

/// The single journal file is compacted to empty after every clean operation —
/// a non-empty journal would mean a dangling `begin` (recovery would then treat
/// a foreign lock as ours). Asserts no journal holds content.
fn assert_journals_clean(env: &Env) {
    let dir = env.journal_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let len = std::fs::metadata(entry.path())
                .map(|m| m.len())
                .unwrap_or(0);
            assert_eq!(len, 0, "journal {:?} holds a dangling record", entry.path());
        }
    }
}

#[test]
fn snapshot_in_process_commits_and_leaves_journal_clean() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();

    let out = env.vard(&["--format", "json", "snapshot", "notes", "-m", "checkpoint"]);
    assert!(out.status.success(), "snapshot failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"status\":\"committed\""),
        "got: {}",
        stdout(&out)
    );
    assert_eq!(commit_count(&env, &repo), 1);
    assert_journals_clean(&env);

    // The message is a body paragraph, not the subject; the trigger is manual.
    let body = git_in(&env, &repo, &["log", "-1", "--format=%B"]);
    let body = stdout(&body);
    assert!(body.contains("checkpoint"), "message not in body: {body}");
    assert!(
        body.contains("Vard-Trigger: manual"),
        "trigger trailer missing: {body}"
    );
}

#[test]
fn second_snapshot_with_no_changes_is_a_clean_noop() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());

    let out = env.vard(&["--format", "json", "snapshot", "notes"]);
    assert!(
        out.status.success(),
        "no-op snapshot should exit 0: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("\"status\":\"no changes\""),
        "got: {}",
        stdout(&out)
    );
    assert_eq!(commit_count(&env, &repo), 1, "no-op must not add a commit");
}

#[test]
fn snapshot_no_target_covers_every_watch() {
    let env = Env::new();
    let a = add_watch(&env, "alpha");
    let b = add_watch(&env, "beta");
    std::fs::write(a.join("x"), "1").unwrap();
    std::fs::write(b.join("y"), "2").unwrap();

    let out = env.vard(&["--format", "json", "snapshot"]);
    assert!(
        out.status.success(),
        "snapshot-all failed: {}",
        stderr(&out)
    );
    let json = stdout(&out);
    assert!(json.contains("\"name\":\"alpha\""), "got: {json}");
    assert!(json.contains("\"name\":\"beta\""), "got: {json}");
    assert_eq!(commit_count(&env, &a), 1);
    assert_eq!(commit_count(&env, &b), 1);
}

#[test]
fn snapshot_refuses_unsafe_repo_with_attention_exit() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());

    // Manufacture a mid-merge (unsafe) state.
    run_conflict_merge(&env, &repo);
    assert!(
        repo.join(".git").join("MERGE_HEAD").exists(),
        "expected mid-merge state"
    );

    let out = env.vard(&["--format", "json", "snapshot", "notes"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "unsafe repo must exit 1: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("\"status\":\"unsafe\""),
        "got: {}",
        stdout(&out)
    );
}

#[test]
fn log_records_and_since_filter() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());

    // Records form (explicit, since a spawned process pipes stdout and would
    // otherwise auto-resolve to JSON): a leading count line then a block.
    let recs = env.vard(&["--format", "records", "log", "notes"]);
    assert!(recs.status.success(), "log failed: {}", stderr(&recs));
    assert!(
        stdout(&recs).contains("1 snapshots"),
        "records count line: {}",
        stdout(&recs)
    );
    assert!(
        stdout(&recs).contains("subject"),
        "records block: {}",
        stdout(&recs)
    );

    let json = env.vard(&["--format", "json", "log", "notes"]);
    assert!(
        stdout(&json).contains("\"trigger\":\"manual\""),
        "got: {}",
        stdout(&json)
    );

    // A generous --since window includes the fresh snapshot (the boundary
    // semantics themselves are unit-tested in vard-core; here we only prove the
    // filter is parsed and applied).
    let since = env.vard(&["--format", "json", "log", "notes", "--since", "100d"]);
    assert!(
        since.status.success(),
        "log --since failed: {}",
        stderr(&since)
    );
    assert!(
        stdout(&since).contains("\"trigger\":\"manual\""),
        "100d window should include it: {}",
        stdout(&since)
    );
}

#[test]
fn diff_shows_working_changes_and_rejects_json() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());
    std::fs::write(repo.join("a.txt"), "v1\nv2\n").unwrap();

    let out = env.vard(&["diff", "notes"]);
    assert!(out.status.success(), "diff failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("diff --git a/a.txt b/a.txt"), "got: {text}");
    assert!(text.contains("+v2"), "got: {text}");

    // Explicit machine format is rejected as text-only (exit 2).
    let json = env.vard(&["--format", "json", "diff", "notes"]);
    assert_eq!(json.status.code(), Some(2), "json diff should be rejected");
    assert!(
        stderr(&json).contains("text-only"),
        "got: {}",
        stderr(&json)
    );
}

#[test]
fn restore_takes_protective_snapshot_then_restores_file() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "original\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());
    let first = stdout(&git_in(&env, &repo, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();

    // Commit a second version, then leave an uncommitted change so the
    // protective snapshot has something to preserve.
    std::fs::write(repo.join("a.txt"), "second\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());
    std::fs::write(repo.join("a.txt"), "uncommitted work\n").unwrap();

    let out = env.vard(&[
        "--format", "json", "restore", "notes", "--ref", &first, "--file", "a.txt",
    ]);
    assert!(out.status.success(), "restore failed: {}", stderr(&out));
    let json = stdout(&out);
    assert!(json.contains("\"status\":\"restored\""), "got: {json}");
    assert!(
        json.contains("\"protective_snapshot\":\""),
        "protective snapshot expected: {json}"
    );
    assert_journals_clean(&env);

    // The file is back to its first-snapshot content.
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "original\n"
    );

    // The uncommitted work is recoverable: a pre-restore snapshot is in the log.
    let log = stdout(&env.vard(&["--format", "json", "log", "notes"]));
    assert!(
        log.contains("\"trigger\":\"pre-restore\""),
        "pre-restore snapshot missing: {log}"
    );
}

#[test]
fn restore_dry_run_previews_without_touching_tree() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());
    let first = stdout(&git_in(&env, &repo, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();
    std::fs::write(repo.join("a.txt"), "v2\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());

    let before = commit_count(&env, &repo);
    let out = env.vard(&["restore", "notes", "--ref", &first, "--dry-run"]);
    assert!(out.status.success(), "dry-run failed: {}", stderr(&out));
    // The preview is the diff between the target rev (v1) and the working tree
    // (v2) — the difference a restore would overwrite.
    let preview = stdout(&out);
    assert!(
        preview.contains("a.txt") && preview.contains("v1") && preview.contains("v2"),
        "got: {preview}"
    );
    // Nothing changed: no protective snapshot, tree untouched.
    assert_eq!(commit_count(&env, &repo), before, "dry-run must not commit");
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "v2\n",
        "dry-run must not touch the tree"
    );
}

#[test]
fn restore_absent_path_gives_a_friendly_error() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());
    let first = stdout(&git_in(&env, &repo, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();

    let out = env.vard(&["restore", "notes", "--ref", &first, "--file", "ghost.txt"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "absent path should fail: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("ghost.txt"),
        "error should name the path: {err}"
    );
    assert!(
        err.contains(&first[..8]) || err.contains("does not exist"),
        "error should name the rev: {err}"
    );
}

#[test]
fn restore_at_natural_language_is_rejected() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());

    let out = env.vard(&["restore", "notes", "--at", "yesterday 3pm"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "natural language should be rejected"
    );
    assert!(
        stderr(&out).contains("YYYY-MM-DD"),
        "should list supported forms: {}",
        stderr(&out)
    );
}

#[test]
fn restore_at_absolute_date_selects_a_snapshot() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
    assert!(env.vard(&["snapshot", "notes"]).status.success());

    // `--at <duration>` means "that long ago", so a relative window cannot
    // select a snapshot made moments ago (nothing is that old yet). A far-future
    // absolute cutoff, however, resolves to the most recent snapshot — proving
    // the `--at` → log composition and the absolute-date parser. The dry-run
    // preview then succeeds against it.
    let out = env.vard(&["restore", "notes", "--at", "2999-12-31", "--dry-run"]);
    assert!(
        out.status.success(),
        "restore --at absolute date failed: {}",
        stderr(&out)
    );

    // And a duration older than any snapshot correctly finds none.
    let none = env.vard(&["restore", "notes", "--at", "3d"]);
    assert_eq!(
        none.status.code(),
        Some(2),
        "no snapshot 3 days old should be found"
    );
    assert!(
        stderr(&none).contains("no snapshot at or before"),
        "got: {}",
        stderr(&none)
    );
}

#[test]
fn commands_on_unknown_watch_fail_cleanly() {
    let env = Env::new();
    add_watch(&env, "notes");
    for args in [
        vec!["log", "ghost"],
        vec!["diff", "ghost"],
        vec!["restore", "ghost", "--ref", "HEAD"],
    ] {
        let out = env.vard(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} should fail: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("ghost"),
            "{args:?} error should name selector: {}",
            stderr(&out)
        );
    }
}

/// Leaves `repo` in a mid-merge (unsafe) state by creating divergent commits on
/// two branches that both edit the same file, then merging.
fn run_conflict_merge(env: &Env, repo: &Path) {
    std::fs::write(repo.join("conflict.txt"), "base\n").unwrap();
    assert!(git_in(env, repo, &["add", "."]).status.success());
    assert!(
        git_in(env, repo, &["commit", "-q", "-m", "base"])
            .status
            .success()
    );
    assert!(
        git_in(env, repo, &["checkout", "-q", "-b", "other"])
            .status
            .success()
    );
    std::fs::write(repo.join("conflict.txt"), "other side\n").unwrap();
    assert!(
        git_in(env, repo, &["commit", "-q", "-am", "other"])
            .status
            .success()
    );
    assert!(
        git_in(env, repo, &["checkout", "-q", "main"])
            .status
            .success()
    );
    std::fs::write(repo.join("conflict.txt"), "main side\n").unwrap();
    assert!(
        git_in(env, repo, &["commit", "-q", "-am", "main"])
            .status
            .success()
    );
    // This merge conflicts, leaving MERGE_HEAD in place.
    let _ = git_in(env, repo, &["merge", "other"]);
}
