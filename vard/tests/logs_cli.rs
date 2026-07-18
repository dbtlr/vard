//! Integration coverage for `vard logs` (VRD-23): the CLI surface that reads the
//! daemon's rotated logfile set. The cross-file `-n` assembly and rotation-aware
//! follow logic are unit-tested in `cmd::logs`; here we exercise the real binary
//! end to end against a synthetic logs directory and a live daemon.

mod common;

use common::{Env, code, stderr, stdout};

/// Seeds the daemon's logs directory with one file of `body` under the given
/// `vard.log.<date>` name, as if the daemon had written it.
fn seed_logfile(env: &Env, date: &str, body: &str) {
    let dir = env.logs_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("vard.log.{date}")), body).unwrap();
}

#[test]
fn no_logfile_reports_cleanly_and_exits_one() {
    let env = Env::new();
    let out = env.vard(&["logs"]);
    assert_eq!(
        code(&out),
        1,
        "no-logfile case must exit 1: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).is_empty(),
        "nothing should be printed to stdout: {}",
        stdout(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("no daemon logfile yet"),
        "expected the friendly no-logfile message, got: {err}"
    );
}

#[test]
fn tail_defaults_to_last_50_lines() {
    let env = Env::new();
    // 60 numbered lines; the default -n 50 must show 11..=60.
    let body: String = (1..=60).map(|i| format!("line {i}\n")).collect();
    seed_logfile(&env, "2026-07-18", &body);

    let out = env.vard(&["logs"]);
    assert_eq!(code(&out), 0, "logs failed: {}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 50, "expected 50 lines, got {}", lines.len());
    assert_eq!(lines.first(), Some(&"line 11"));
    assert_eq!(lines.last(), Some(&"line 60"));
}

#[test]
fn n_flag_spans_rotated_files() {
    let env = Env::new();
    // Newest day has 2 lines; a request for 4 must reach into the previous day.
    seed_logfile(&env, "2026-07-17", "a1\na2\na3\n");
    seed_logfile(&env, "2026-07-18", "b1\nb2\n");

    let out = env.vard(&["logs", "-n", "4"]);
    assert_eq!(code(&out), 0, "logs -n failed: {}", stderr(&out));
    assert_eq!(stdout(&out), "a2\na3\nb1\nb2\n");
}

#[test]
fn text_only_rejects_explicit_json_format() {
    let env = Env::new();
    seed_logfile(&env, "2026-07-18", "hello\n");

    let out = env.vard(&["--format", "json", "logs"]);
    assert_eq!(
        code(&out),
        2,
        "explicit --format json must be rejected: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("text-only"),
        "expected the text-only rejection message, got: {}",
        stderr(&out)
    );
}

#[test]
fn logs_from_a_live_daemon() {
    let env = Env::new();
    // A watch so the daemon has something to supervise and starts cleanly.
    let repo = env.root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let add = env.vard(&[
        "watch",
        "add",
        repo.to_str().unwrap(),
        "--name",
        "notes",
        "--init",
    ]);
    assert_eq!(code(&add), 0, "watch add failed: {}", stderr(&add));

    let _daemon = env.spawn_daemon();
    // The daemon writes its logfile at startup; give it a moment to appear.
    let dir = env.logs_dir();
    let mut appeared = false;
    for _ in 0..50 {
        if dir
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
        {
            appeared = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        appeared,
        "daemon never wrote a logfile under {}",
        dir.display()
    );

    let out = env.vard(&["logs", "-n", "20"]);
    assert_eq!(code(&out), 0, "logs failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("vard::daemon"),
        "expected the daemon's log target in the output, got: {}",
        stdout(&out)
    );
}
