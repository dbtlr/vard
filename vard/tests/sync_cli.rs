//! End-to-end tests for `vard sync` (VRD-19), driving the real binary against
//! tempdir-isolated config, state, and HOME, with a bare file remote.
//!
//! These exercise the no-daemon in-process path, where the outcome is
//! deterministic without a background process: the command builds a minimal
//! engine, runs one real sync cycle per watch, and reports the result. The
//! daemon-request dispatch path (a sync handed to a running `vard run`) is
//! covered by the daemon's own integration test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;
use common::{Env, code, stderr, stdout};

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

fn git_ok(env: &Env, repo: &Path, args: &[&str]) {
    let out = git_in(env, repo, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        stderr(&out)
    );
}

/// Deterministic, signer-free commits regardless of the host's git config.
fn no_sign(env: &Env) {
    env.set_git_config("commit.gpgsign", "false");
}

/// A bare repository usable as a file remote.
fn bare_origin(env: &Env, name: &str) -> PathBuf {
    let path = env.root.path().join(name);
    let out = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&path)
        .env("GIT_CONFIG_GLOBAL", &env.git_config)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "init --bare failed: {}", stderr(&out));
    path
}

/// A working repo on `main` with `origin` set, a base commit, and `main`
/// pushed — the remote exists and the two agree at the base.
fn synced_repo(env: &Env, name: &str, origin: &Path) -> PathBuf {
    let path = env.root.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    git_ok(env, &path, &["init", "-b", "main"]);
    git_ok(
        env,
        &path,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    git_ok(env, &path, &["add", "-A"]);
    git_ok(env, &path, &["commit", "-m", "base"]);
    git_ok(env, &path, &["push", "-u", "origin", "main"]);
    std::fs::canonicalize(&path).unwrap()
}

/// A second clone of `origin` that moves the remote out from under the watch.
fn mover_pushes(env: &Env, origin: &Path, file: &str, contents: &str) {
    let dest = env.root.path().join("mover");
    let out = Command::new("git")
        .args(["clone", origin.to_str().unwrap()])
        .arg(&dest)
        .env("GIT_CONFIG_GLOBAL", &env.git_config)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "clone failed: {}", stderr(&out));
    std::fs::write(dest.join(file), contents).unwrap();
    git_ok(env, &dest, &["add", "-A"]);
    git_ok(env, &dest, &["commit", "-m", "remote work"]);
    git_ok(env, &dest, &["push", "origin", "main"]);
    std::fs::remove_dir_all(&dest).unwrap();
}

/// A sync-enabled repo whose config names `origin` but whose repository has NO
/// such remote configured — the live remote gate must catch it.
fn repo_missing_remote(env: &Env, name: &str) -> PathBuf {
    let path = env.root.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    git_ok(env, &path, &["init", "-b", "main"]);
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    git_ok(env, &path, &["add", "-A"]);
    git_ok(env, &path, &["commit", "-m", "base"]);
    std::fs::canonicalize(&path).unwrap()
}

fn config_for(watches: &str) -> String {
    format!("version = 1\n{watches}")
}

fn sync_watch(name: &str, path: &Path) -> String {
    format!(
        "[[watch]]\nname = \"{name}\"\npath = \"{}\"\nsync = true\nbranch = \"main\"\n\
         remote = \"origin\"\ntrigger = \"interval\"\ninterval = \"1h\"\n",
        path.display()
    )
}

/// The single journal file is compacted to empty after every clean operation.
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
fn sync_first_push_to_an_empty_remote_succeeds_then_reports_up_to_date() {
    // The dotfiles-first-push onboarding path: a fresh repo with commits and an
    // origin that has NEVER received the branch (empty bare remote). The first
    // `vard sync` must push the branch (not fail on a rebase onto the
    // nonexistent upstream), and a second run must report up to date.
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    // A repo with a base commit and origin configured, but NOTHING pushed.
    let repo = env.root.path().join("dots");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&env, &repo, &["init", "-b", "main"]);
    git_ok(
        &env,
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    std::fs::write(repo.join("vimrc"), "set nocompatible\n").unwrap();
    git_ok(&env, &repo, &["add", "-A"]);
    git_ok(&env, &repo, &["commit", "-m", "base"]);
    let repo = std::fs::canonicalize(&repo).unwrap();
    env.write_config(&config_for(&sync_watch("dots", &repo)));

    // First sync: the branch reaches the remote and the command exits 0.
    let out = env.vard(&["--format", "records", "sync", "dots"]);
    assert!(
        out.status.success(),
        "first push to an empty remote failed: stdout: {} stderr: {}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("status   pushed"),
        "got: {}",
        stdout(&out)
    );
    let remote_head = git_in(&env, &origin, &["rev-parse", "refs/heads/main"]);
    let local_head = git_in(&env, &repo, &["rev-parse", "HEAD"]);
    assert!(
        remote_head.status.success(),
        "the remote never received the branch: {}",
        stderr(&remote_head)
    );
    assert_eq!(stdout(&remote_head).trim(), stdout(&local_head).trim());

    // Second sync: nothing to do.
    let out = env.vard(&["--format", "records", "sync", "dots"]);
    assert!(out.status.success(), "second sync failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("status   up to date"),
        "got: {}",
        stdout(&out)
    );
    assert_journals_clean(&env);
}

#[test]
fn sync_in_process_pushes_dirty_work_and_exits_zero() {
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let repo = synced_repo(&env, "notes", &origin);
    env.write_config(&config_for(&sync_watch("notes", &repo)));

    // Uncommitted local edit, remote unmoved: must be committed and pushed.
    std::fs::write(repo.join("draft.txt"), "local work\n").unwrap();

    let out = env.vard(&["--format", "records", "sync", "notes"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("status   pushed"), "got: {text}");
    assert!(text.contains("commits  1"), "got: {text}");

    // The commit really reached the remote.
    let remote_head = git_in(&env, &origin, &["rev-parse", "refs/heads/main"]);
    let local_head = git_in(&env, &repo, &["rev-parse", "HEAD"]);
    assert_eq!(stdout(&remote_head).trim(), stdout(&local_head).trim());
    assert_journals_clean(&env);
}

#[test]
fn sync_up_to_date_reports_and_exits_zero() {
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let repo = synced_repo(&env, "notes", &origin);
    env.write_config(&config_for(&sync_watch("notes", &repo)));

    // Clean tree, remote unmoved: nothing to do.
    let out = env.vard(&["--format", "records", "sync", "notes"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("status   up to date"),
        "got: {}",
        stdout(&out)
    );
    assert_journals_clean(&env);
}

#[test]
fn sync_json_emits_the_machine_contract() {
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let repo = synced_repo(&env, "notes", &origin);
    env.write_config(&config_for(&sync_watch("notes", &repo)));

    let out = env.vard(&["--format", "json", "sync", "notes"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(r#""name":"notes""#) && text.contains(r#""status":"up to date""#),
        "got: {text}"
    );
    // Stable fixed-shape keys, present even when unused.
    assert!(text.contains(r#""commits":null"#), "got: {text}");
    assert!(text.contains(r#""ref":null"#), "got: {text}");
}

#[test]
fn sync_conflict_reports_and_exits_one() {
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let repo = synced_repo(&env, "notes", &origin);
    env.write_config(&config_for(&sync_watch("notes", &repo)));

    // The remote edits base.txt; the watch edits the same file locally. The
    // pre-sync snapshot commits the local edit, then the reconcile conflicts.
    mover_pushes(&env, &origin, "base.txt", "remote-change\n");
    std::fs::write(repo.join("base.txt"), "local-change\n").unwrap();

    let out = env.vard(&["--format", "records", "sync", "notes"]);
    assert_eq!(
        code(&out),
        1,
        "expected attention exit, stderr: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("status   conflict"),
        "got: {}",
        stdout(&out)
    );
}

#[test]
fn sync_all_shows_a_remote_less_watch_as_a_row_and_still_exits_zero() {
    // Finding 5: with NO selector, a sync-enabled watch whose repository has no
    // configured remote is NOT filtered out — it appears as an informational
    // disabled/no-remote row that does NOT force a non-zero exit, while the other
    // sync-enabled watch is synced normally.
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let notes = synced_repo(&env, "notes", &origin);
    let solo = repo_missing_remote(&env, "solo");

    let watches = format!(
        "{}{}",
        sync_watch("notes", &notes),
        sync_watch("solo", &solo)
    );
    env.write_config(&config_for(&watches));

    let out = env.vard(&["--format", "records", "sync"]);
    assert_eq!(
        code(&out),
        0,
        "an informational no-remote row must not force a non-zero exit; stderr: {}",
        stderr(&out)
    );
    let text = stdout(&out);
    // Both watches get a row.
    assert!(text.contains("name     notes"), "notes row missing: {text}");
    assert!(text.contains("name     solo"), "solo row missing: {text}");
    // The remote-less watch is shown disabled with a no-remote reason.
    assert!(
        text.contains("status   disabled")
            && text.contains("no remote \"origin\" in the repository"),
        "solo should show a disabled/no-remote row: {text}"
    );
    // The remote-having watch actually synced (nothing to do here).
    assert!(
        text.contains("status   up to date"),
        "notes not synced: {text}"
    );
    assert_journals_clean(&env);
}

/// A `[[watch]]` block like [`sync_watch`] but paused.
fn paused_sync_watch(name: &str, path: &Path) -> String {
    format!(
        "[[watch]]\nname = \"{name}\"\npath = \"{}\"\nsync = true\npaused = true\n\
         branch = \"main\"\nremote = \"origin\"\ntrigger = \"interval\"\ninterval = \"1h\"\n",
        path.display()
    )
}

#[test]
fn sync_named_paused_watch_refuses_in_process() {
    // Parity with the daemon-present path: a paused watch is suspended
    // everywhere, so `vard sync <paused>` with NO daemon must refuse (attention
    // row, exit 1) instead of quietly running the cycle.
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let repo = synced_repo(&env, "notes", &origin);
    env.write_config(&config_for(&paused_sync_watch("notes", &repo)));

    // Uncommitted work that a wrongly-run cycle would have pushed.
    std::fs::write(repo.join("draft.txt"), "local work\n").unwrap();

    let out = env.vard(&["--format", "records", "sync", "notes"]);
    assert_eq!(
        code(&out),
        1,
        "expected attention exit; stderr: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("status   paused"),
        "got: {}",
        stdout(&out)
    );
    // No cycle ran: nothing new reached the remote.
    let count = git_in(&env, &origin, &["rev-list", "--count", "refs/heads/main"]);
    assert_eq!(stdout(&count).trim(), "1", "the paused watch must not sync");
}

#[test]
fn sync_named_paused_watch_refuses_with_a_daemon_running() {
    // The daemon-present half of the parity: via_request refuses a paused named
    // watch up front with the same attention exit.
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let repo = synced_repo(&env, "notes", &origin);
    env.write_config(&config_for(&paused_sync_watch("notes", &repo)));

    let daemon = env.spawn_daemon();
    let out = env.vard(&["sync", "notes"]);
    drop(daemon);

    assert_eq!(
        code(&out),
        1,
        "expected attention exit; stderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("paused"),
        "the refusal names the paused state: {}",
        stderr(&out)
    );
}

#[test]
fn sync_all_paused_reports_informational_paused_rows_and_exits_zero() {
    // No selector with every sync-enabled watch paused: the outcome is accurate
    // and informational — one `paused` row per watch, exit 0 (matching the
    // daemon-present path's exit 0) — never the untrue "no sync-enabled watches
    // configured".
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let a = synced_repo(&env, "alpha", &origin);
    let origin_b = bare_origin(&env, "origin-b.git");
    let b = synced_repo(&env, "beta", &origin_b);
    let watches = format!(
        "{}{}",
        paused_sync_watch("alpha", &a),
        paused_sync_watch("beta", &b)
    );
    env.write_config(&config_for(&watches));

    let out = env.vard(&["--format", "records", "sync"]);
    assert_eq!(
        code(&out),
        0,
        "all-paused is informational, not an error; stderr: {}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(
        text.contains("name     alpha") && text.contains("name     beta"),
        "every paused watch gets a row: {text}"
    );
    assert_eq!(
        text.matches("status   paused").count(),
        2,
        "both rows report paused: {text}"
    );
}

#[test]
fn sync_named_unopenable_repo_fails_with_and_without_a_daemon() {
    // Parity for an unopenable named repository: both dispatch paths report a
    // real failure (exit 2, "cannot open repository"), never the misleading
    // no-remote refusal.
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let notes = synced_repo(&env, "notes", &origin);
    // A sync-enabled watch whose path is not a git repository.
    let broken = env.root.path().join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    let broken = std::fs::canonicalize(&broken).unwrap();
    env.write_config(&config_for(&format!(
        "{}{}",
        sync_watch("notes", &notes),
        sync_watch("broken", &broken)
    )));

    // No daemon: the in-process path reports the failed row.
    let out = env.vard(&["--format", "records", "sync", "broken"]);
    assert_eq!(
        code(&out),
        2,
        "in-process: exit 2; stderr: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("cannot open repository"),
        "in-process names the real fault: {}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("no remote"),
        "an unopenable repo is not a no-remote case: {}",
        stdout(&out)
    );

    // Daemon present: the daemon was started BEFORE the broken watch entered
    // the config (a daemon cannot supervise an unopenable repo), which is
    // exactly how via_request meets one — the user edited the config after
    // startup. The pre-check must classify it as the same real failure.
    env.write_config(&config_for(&sync_watch("notes", &notes)));
    let daemon = env.spawn_daemon();
    // The broken watch appears in the config only now, after daemon start.
    env.write_config(&config_for(&format!(
        "{}{}",
        sync_watch("notes", &notes),
        sync_watch("broken", &broken)
    )));

    let out = env.vard(&["sync", "broken"]);
    drop(daemon);

    assert_eq!(
        code(&out),
        2,
        "daemon-present: same exit 2; stderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("cannot open repository"),
        "daemon-present names the real fault: {}",
        stderr(&out)
    );
}

#[test]
fn sync_all_isolates_a_broken_repo_and_still_syncs_the_rest() {
    // Per-watch isolation: a sync-enabled watch whose repository cannot be
    // opened yields an honest failed row (exit 2) while every other watch still
    // syncs — one broken repo never blocks the rest.
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let healthy = synced_repo(&env, "notes", &origin);
    std::fs::write(healthy.join("draft.txt"), "local work\n").unwrap();

    // A sync-enabled watch whose path is not a git repository at all.
    let broken = env.root.path().join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    let broken = std::fs::canonicalize(&broken).unwrap();

    let watches = format!(
        "{}{}",
        sync_watch("notes", &healthy),
        sync_watch("broken", &broken)
    );
    env.write_config(&config_for(&watches));

    let out = env.vard(&["--format", "records", "sync"]);
    assert_eq!(code(&out), 2, "the broken repo contributes an error exit");
    let text = stdout(&out);
    assert!(
        text.contains("name     notes") && text.contains("status   pushed"),
        "the healthy watch still synced: {text}"
    );
    assert!(
        text.contains("name     broken")
            && text.contains("status   failed")
            && text.contains("cannot open repository"),
        "the broken watch gets an honest failed row: {text}"
    );
    // The healthy watch's work really reached the remote.
    let remote_head = git_in(&env, &origin, &["rev-parse", "refs/heads/main"]);
    let local_head = git_in(&env, &healthy, &["rev-parse", "HEAD"]);
    assert_eq!(stdout(&remote_head).trim(), stdout(&local_head).trim());
}

#[test]
fn sync_named_unknown_watch_errors() {
    let env = Env::new();
    let origin = bare_origin(&env, "origin.git");
    let repo = synced_repo(&env, "notes", &origin);
    env.write_config(&config_for(&sync_watch("notes", &repo)));

    let out = env.vard(&["sync", "nope"]);
    assert_eq!(code(&out), 2, "expected operational error");
    assert!(
        stderr(&out).contains("nope"),
        "error should name the bad selector: {}",
        stderr(&out)
    );
}

#[test]
fn sync_named_non_sync_watch_is_disabled() {
    let env = Env::new();
    no_sign(&env);
    // A plain local watch with syncing off.
    let repo = env.root.path().join("local");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&env, &repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("x.txt"), "x\n").unwrap();
    git_ok(&env, &repo, &["add", "-A"]);
    git_ok(&env, &repo, &["commit", "-m", "x"]);
    let canon = std::fs::canonicalize(&repo).unwrap();
    env.write_config(&config_for(&format!(
        "[[watch]]\nname = \"local\"\npath = \"{}\"\nsync = false\n\
         trigger = \"interval\"\ninterval = \"1h\"\n",
        canon.display()
    )));

    let out = env.vard(&["--format", "records", "sync", "local"]);
    assert_eq!(code(&out), 1, "expected attention exit");
    assert!(
        stdout(&out).contains("status   disabled"),
        "got: {}",
        stdout(&out)
    );
}

#[test]
fn sync_all_syncs_enabled_and_skips_disabled() {
    let env = Env::new();
    no_sign(&env);
    let origin = bare_origin(&env, "origin.git");
    let synced = synced_repo(&env, "notes", &origin);

    let local = env.root.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git_ok(&env, &local, &["init", "-b", "main"]);
    std::fs::write(local.join("x.txt"), "x\n").unwrap();
    git_ok(&env, &local, &["add", "-A"]);
    git_ok(&env, &local, &["commit", "-m", "x"]);
    let local_canon = std::fs::canonicalize(&local).unwrap();

    let local_watch = format!(
        "[[watch]]\nname = \"local\"\npath = \"{}\"\nsync = false\n\
         trigger = \"interval\"\ninterval = \"1h\"\n",
        local_canon.display()
    );
    let watches = format!("{}{}", sync_watch("notes", &synced), local_watch);
    env.write_config(&config_for(&watches));

    // No selector: only the sync-enabled watch is acted on; the disabled one is
    // silently skipped (not shown as an error).
    let out = env.vard(&["--format", "json", "sync"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(r#""name":"notes""#), "got: {text}");
    assert!(
        !text.contains(r#""name":"local""#),
        "disabled watch shown: {text}"
    );
}
