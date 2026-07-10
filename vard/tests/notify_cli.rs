//! End-to-end tests for `vard notify` (VRD-18), driving the real binary against
//! tempdir-isolated XDG dirs. Two layers:
//!
//! * The no-daemon path is fully deterministic and needs no background process:
//!   with nothing holding the instance lock, notify reports the
//!   daemon-not-running line and exits 1, in both the human and JSON forms, and
//!   it does so without a config file (the performance/independence contract).
//! * A lifecycle test spawns a real `vard run`, watches the health file it
//!   maintains flip from healthy → problem → gone across the daemon's life, and
//!   confirms the lock probe tracks the daemon start and clean stop.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::{Env, code, stdout};

// --- no-daemon path (deterministic) ----------------------------------------

#[test]
fn no_daemon_reports_not_running_and_exits_one() {
    let env = Env::new();
    // No lock file exists at all: the probe must read this as no daemon.
    let out = env.vard(&["notify"]);
    assert_eq!(code(&out), 1, "no daemon must exit 1, got {out:?}");
    assert!(
        stdout(&out).contains("daemon not running"),
        "expected the daemon-not-running line, got: {}",
        stdout(&out)
    );
}

#[test]
fn no_daemon_json_is_a_problem_array_not_empty() {
    let env = Env::new();
    let out = env.vard(&["notify", "--format", "json"]);
    assert_eq!(code(&out), 1);
    let s = stdout(&out);
    assert!(s.contains("\"state\":\"daemon-not-running\""), "got: {s}");
    assert!(s.contains("\"watch\":null"), "got: {s}");
    // A stable JSON array, parseable by a status bar.
    assert!(s.trim_start().starts_with('['), "got: {s}");
}

#[test]
fn notify_does_not_require_a_config_file() {
    // The performance/independence contract: notify reads only the health file
    // and the lock — never config.toml. A fresh env has no config at all, yet
    // notify still runs and reports the stopped daemon.
    let env = Env::new();
    assert!(
        !env.config_home.join("vard").join("config.toml").exists(),
        "precondition: no config file"
    );
    let out = env.vard(&["notify"]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("daemon not running"));
}

// --- lifecycle against a real daemon ---------------------------------------

/// Initializes a git repo with one root commit so `vard run` has a valid watch.
fn init_repo(env: &Env, repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", &env.git_config)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["commit", "--allow-empty", "-m", "root"]);
}

fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn daemon_lifecycle_drives_notify_from_healthy_to_problem_to_stopped() {
    let env = Env::new();
    let repo = env.root.path().join("repo");
    init_repo(&env, &repo);

    // An interval-only watch with a huge interval: the daemon starts, watches,
    // and then sits idle (no filesystem trigger, no auto snapshot), so the
    // health file we inject below is not overwritten out from under the test.
    let config_dir = env.config_home.join("vard");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "version = 1\n\n[[watch]]\nname = \"vault\"\npath = {:?}\ntrigger = \"interval\"\ninterval = \"24h\"\n",
            repo
        ),
    )
    .unwrap();

    // Spawn the real daemon in the foreground (detached from the test's stdio).
    let mut daemon = env
        .command(&["run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vard run");

    // It writes a fresh (empty) health file on startup; wait for it.
    let health = env.health_file();
    wait_until(|| health.exists(), "the daemon to write the health file");

    // Running + healthy: notify is silent and exits 0.
    let healthy = env.vard(&["notify"]);
    assert_eq!(
        code(&healthy),
        0,
        "a running, healthy daemon must make notify exit 0; stdout={:?}",
        stdout(&healthy)
    );
    assert!(
        stdout(&healthy).trim().is_empty(),
        "healthy notify must be silent, got: {:?}",
        stdout(&healthy)
    );
    // The healthy JSON is an empty array, not silence.
    let healthy_json = env.vard(&["notify", "--format", "json"]);
    assert_eq!(code(&healthy_json), 0);
    assert_eq!(stdout(&healthy_json).trim(), "[]");

    // Inject a problem into the health file (the daemon is idle, so it stays).
    // This exercises notify's running-daemon read path against a real lock.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        &health,
        format!(
            "version = 1\nwritten_at = {now}\n\n[[problem]]\nwatch = \"vault\"\nstate = \"conflicted\"\nsummary = \"a sync conflict is blocking progress\"\nsince = {}\n",
            now - 7200
        ),
    )
    .unwrap();

    let troubled = env.vard(&["notify"]);
    assert_eq!(code(&troubled), 1, "a problem must make notify exit 1");
    let line = stdout(&troubled);
    assert!(line.contains("'vault' conflicted"), "got: {line}");
    // `since` is 2h ago, so elapsed renders as "2h" (plus a few seconds of the
    // read delay: "2h", "2h3s", …) — the "2h" prefix is stable.
    assert!(line.contains("for 2h"), "elapsed must render, got: {line}");

    let troubled_json = env.vard(&["notify", "--format", "json"]);
    assert_eq!(code(&troubled_json), 1);
    let js = stdout(&troubled_json);
    assert!(js.contains("\"watch\":\"vault\""), "got: {js}");
    // elapsed_seconds is `now_at_read - since`; the seconds that pass between
    // writing the file and reading it push it a hair past 7200, so allow the
    // small delay rather than pinning an exact value (a real clock race).
    let elapsed = elapsed_seconds_from_json(&js);
    assert!(
        (7200..7210).contains(&elapsed),
        "elapsed_seconds should be ~7200, got {elapsed} in {js}"
    );

    // Clean shutdown: SIGTERM. The daemon removes the health file and releases
    // the lock, so notify falls back to the daemon-not-running line.
    let term = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .expect("spawn kill");
    assert!(term.success(), "kill -TERM failed");
    let exit = wait_for_exit(&mut daemon);
    assert!(exit.success(), "daemon must exit cleanly on SIGTERM");

    assert!(
        !health.exists(),
        "clean shutdown must remove the health file"
    );
    let stopped = env.vard(&["notify"]);
    assert_eq!(
        code(&stopped),
        1,
        "a stopped daemon must make notify exit 1"
    );
    assert!(
        stdout(&stopped).contains("daemon not running"),
        "got: {}",
        stdout(&stopped)
    );
}

/// Pulls the integer value of `"elapsed_seconds":N` out of a one-object JSON
/// array, without a JSON dependency.
fn elapsed_seconds_from_json(js: &str) -> i64 {
    let key = "\"elapsed_seconds\":";
    let start = js.find(key).expect("elapsed_seconds present") + key.len();
    let digits: String = js[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().expect("elapsed_seconds is a number")
}

/// Waits for a spawned child to exit, returning its status. Kills it on timeout
/// so a hung daemon fails the test rather than the whole suite.
fn wait_for_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("daemon did not exit within the timeout");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
