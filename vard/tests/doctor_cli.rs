//! End-to-end tests for `vard doctor` (VRD-23), driving the real binary against
//! tempdir-isolated config, state, and HOME. Nothing here touches the developer's
//! real environment; repositories use a throwaway global git config.
//!
//! The headline is the per-watch secret audit: a tracked file whose NAME is
//! secret-shaped is flagged (`fail`, exit 1), while a tracked file with
//! secret-shaped CONTENT but an innocent name is NOT — the audit is filename-only
//! by contract. Any secret-shaped literal in this source is split via `concat!`
//! so GitHub push protection never sees a token-shaped string in the blob.

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

/// Adds a watch named `name` at a fresh `--init` repository and returns its
/// canonical path.
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

#[test]
fn secret_audit_flags_a_tracked_secret_named_file_but_not_secret_content() {
    let env = Env::new();
    let repo = add_watch(&env, "vault");

    // A secret-shaped NAME (in the built-in catalog). `watch add --init` seeds
    // .git/info/exclude with `.env`, so force-add it to model a secret committed
    // before scanning was on — exactly what the audit exists to catch.
    std::fs::write(repo.join(".env"), "DB_PASSWORD=hunter2\n").unwrap();
    assert!(
        git_in(&env, &repo, &["add", "-f", ".env"]).status.success(),
        "force-add .env"
    );

    // An INNOCENT name carrying secret-shaped CONTENT — the literal is split so
    // the source blob never contains a token-shaped string. The filename-only
    // audit must NOT flag this.
    let akia = concat!("AKIA", "IOSFODNN7", "EXAMPLE");
    std::fs::write(
        repo.join("notes.txt"),
        format!("aws_access_key_id = {akia}\n"),
    )
    .unwrap();
    assert!(
        git_in(&env, &repo, &["add", "notes.txt"]).status.success(),
        "add notes.txt"
    );
    assert!(
        git_in(&env, &repo, &["commit", "-m", "seed"])
            .status
            .success(),
        "commit"
    );

    let out = env.vard(&["--format", "json", "doctor"]);
    // A finding folds exit 1 (attention).
    assert_eq!(
        code(&out),
        1,
        "a secret finding must exit 1: {}",
        stderr(&out)
    );
    let json = stdout(&out);

    // The secret-audit row is a fail naming the secret-shaped file.
    assert!(
        json.contains(r#""check":"secret-audit""#),
        "no secret-audit row: {json}"
    );
    assert!(json.contains(r#""status":"fail""#), "not a fail: {json}");
    assert!(
        json.contains(".env"),
        "the .env name is not reported: {json}"
    );

    // The content-only file must NOT be flagged — the audit is filename-only.
    assert!(
        !json.contains("notes.txt"),
        "a secret-shaped CONTENT file with an innocent name must not be flagged: {json}"
    );
}

#[test]
fn secret_audit_skips_a_watch_with_scanning_disabled() {
    let env = Env::new();
    let repo = add_watch(&env, "vault");
    // Force-add a secret-named file that WOULD flag, then disable scanning.
    std::fs::write(repo.join(".env"), "x\n").unwrap();
    git_in(&env, &repo, &["add", "-f", ".env"]);
    git_in(&env, &repo, &["commit", "-m", "seed"]);

    // Turn scanning off for the watch by editing its config table.
    let cfg = env.read_config();
    let cfg = cfg.replace("[[watch]]", "[[watch]]\nsecret_scan = false");
    std::fs::write(env.config_path(), cfg).unwrap();

    let out = env.vard(&["--format", "json", "doctor"]);
    let json = stdout(&out);
    // Bind the status to the secret-audit row itself — a `skipped` on some other
    // row (e.g. inotify off Linux) must not let this pass.
    assert!(
        json.contains(r#""check":"secret-audit","status":"skipped""#),
        "the secret-audit row itself must be skipped: {json}"
    );
    // With scanning off, that fail is gone; git/health/etc are all ok → exit 0.
    assert_eq!(
        code(&out),
        0,
        "a skipped audit must not fold attention: {json}"
    );
}

#[test]
fn doctor_is_read_only_and_reports_a_clean_repo_ok() {
    let env = Env::new();
    let repo = add_watch(&env, "notes");
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    git_in(&env, &repo, &["add", "a.txt"]);
    git_in(&env, &repo, &["commit", "-m", "seed"]);

    let before = git_in(&env, &repo, &["rev-parse", "HEAD"]);
    let out = env.vard(&["--format", "json", "doctor"]);
    let json = stdout(&out);
    assert_eq!(code(&out), 0, "a clean repo is all-clear: {json}");
    assert!(
        json.contains(r#""check":"secret-audit""#) && json.contains(r#""status":"ok""#),
        "clean secret audit: {json}"
    );
    // Read-only: HEAD is untouched and no request file was written.
    let after = git_in(&env, &repo, &["rev-parse", "HEAD"]);
    assert_eq!(stdout(&before), stdout(&after), "doctor moved HEAD");
    let requests = env.state_home.join("vard").join("requests");
    if let Ok(entries) = std::fs::read_dir(&requests) {
        assert_eq!(entries.count(), 0, "doctor must not write a request file");
    }
}

/// A running daemon whose stamped version differs from the installed binary is
/// version skew launchd never restarts away on a binary swap (VRD-72): the
/// `daemon-version` check `warn`s and names `vard service restart`. Fabricated
/// by spawning the real daemon (so the instance lock is held and `collect`
/// reports it running) and then overwriting its health file with a differing,
/// fresh `daemon_version` — the daemon is on a 24h interval, so it sits idle and
/// never overwrites the injected file.
#[test]
fn daemon_version_skew_warns_with_the_restart_hint() {
    let env = Env::new();
    let repo = env.root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        git_in(&env, &repo, &["init", "-b", "main"])
            .status
            .success()
    );
    assert!(
        git_in(&env, &repo, &["commit", "--allow-empty", "-m", "root"])
            .status
            .success()
    );
    env.write_config(&format!(
        "version = 1\n\n[[watch]]\nname = \"vault\"\npath = {repo:?}\ntrigger = \"interval\"\ninterval = \"24h\"\n"
    ));

    let _daemon = env.spawn_daemon();
    // Overwrite the health file with a fresh document stamped with a version the
    // running binary can never be (a differing, older stamp).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        env.health_file(),
        format!("version = 2\nwritten_at = {now}\ndaemon_version = \"0.0.1\"\n"),
    )
    .unwrap();

    let out = env.vard(&["--format", "json", "doctor"]);
    let json = stdout(&out);
    assert_eq!(code(&out), 1, "a version-skew warn folds exit 1: {json}");
    assert!(
        json.contains(r#""check":"daemon-version","status":"warn""#),
        "the daemon-version check must warn on skew: {json}"
    );
    assert!(
        json.contains("0.0.1") && json.contains("vard service restart"),
        "it must name the stamped version and the fix: {json}"
    );
}
