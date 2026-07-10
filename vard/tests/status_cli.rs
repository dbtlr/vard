//! End-to-end tests for `vard status` (VRD-17), driving the real binary against
//! tempdir-isolated XDG dirs. These are deterministic and need no daemon: with
//! nothing holding the instance lock, status reports the daemon as not running
//! and projects the config's watch list (paused and healthy watches included).
//! The running-daemon health read path is covered by the shared `health::collect`
//! and `notify` lifecycle test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// A fully isolated environment: its own XDG dirs and HOME.
struct Env {
    root: TempDir,
    config_home: PathBuf,
    state_home: PathBuf,
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let root = TempDir::new().unwrap();
        let base = root.path();
        let env = Env {
            config_home: base.join("config"),
            state_home: base.join("state"),
            home: base.join("home"),
            root,
        };
        std::fs::create_dir_all(&env.home).unwrap();
        env
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vard"));
        cmd.args(args)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("HOME", &self.home)
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR_FORCE");
        cmd
    }

    fn vard(&self, args: &[&str]) -> Output {
        self.command(args)
            .stdin(Stdio::null())
            .output()
            .expect("spawn vard")
    }

    /// Writes `contents` as the config file, creating the config dir.
    fn write_config(&self, contents: &str) {
        let dir = self.config_home.join("vard");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents).unwrap();
    }
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("process exited via a signal")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn no_config_no_daemon_reports_the_stopped_daemon() {
    let env = Env::new();
    // No config, no daemon: status must exit 1 (daemon not running folds in) and
    // report the daemon line, even with no watches to list.
    let out = env.vard(&["--format", "records", "status"]);
    assert_eq!(code(&out), 1, "no daemon must exit 1, got {out:?}");
    assert!(
        stdout(&out).contains("daemon: not running"),
        "expected the daemon-not-running line, got: {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("no watches configured"),
        "expected the no-watches note, got: {}",
        stdout(&out)
    );
}

#[test]
fn paused_and_healthy_watches_render_their_states() {
    let env = Env::new();
    let active = env.root.path().join("active");
    let sleeping = env.root.path().join("sleeping");
    std::fs::create_dir_all(&active).unwrap();
    std::fs::create_dir_all(&sleeping).unwrap();
    env.write_config(&format!(
        "version = 1\n\n[[watch]]\nname = \"active\"\npath = {:?}\n\n\
         [[watch]]\nname = \"sleeping\"\npath = {:?}\npaused = true\n",
        active, sleeping
    ));

    let out = env.vard(&["--format", "records", "status"]);
    // Daemon not running ⇒ exit 1, but the per-watch projection still renders.
    assert_eq!(code(&out), 1);
    let s = stdout(&out);
    // With no daemon, an unpaused watch is `unknown` (nothing is monitoring it),
    // not `ok`; a config-paused watch still shows `paused`.
    assert!(
        s.contains("active: unknown"),
        "unmonitored watch state, got: {s}"
    );
    assert!(
        s.contains("sleeping: paused"),
        "paused watch state, got: {s}"
    );
}

#[test]
fn json_shape_leads_with_a_null_named_daemon_row() {
    let env = Env::new();
    let active = env.root.path().join("active");
    std::fs::create_dir_all(&active).unwrap();
    env.write_config(&format!(
        "version = 1\n\n[[watch]]\nname = \"active\"\npath = {:?}\n",
        active
    ));

    let out = env.vard(&["--format", "json", "status"]);
    assert_eq!(code(&out), 1);
    let s = stdout(&out);
    // A JSON array a status bar can parse.
    assert!(s.trim_start().starts_with('['), "got: {s}");
    // The daemon row: null watch name, null kind, a `daemon: true` flag, its state.
    assert!(s.contains(r#""name":null"#), "got: {s}");
    assert!(s.contains(r#""kind":null"#), "got: {s}");
    assert!(s.contains(r#""daemon":true"#), "got: {s}");
    assert!(s.contains(r#""state":"not-running""#), "got: {s}");
    // The watch row: real name, `unknown` state (no daemon), a `daemon: false` flag.
    assert!(s.contains(r#""name":"active""#), "got: {s}");
    assert!(s.contains(r#""state":"unknown""#), "got: {s}");
    assert!(s.contains(r#""daemon":false"#), "got: {s}");
    assert!(
        s.contains(r#""elapsed_seconds""#),
        "stable field set, got: {s}"
    );
}

#[test]
fn selector_narrows_to_one_watch() {
    let env = Env::new();
    let a = env.root.path().join("a");
    let b = env.root.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    env.write_config(&format!(
        "version = 1\n\n[[watch]]\nname = \"a\"\npath = {:?}\n\n\
         [[watch]]\nname = \"b\"\npath = {:?}\n",
        a, b
    ));

    let out = env.vard(&["--format", "records", "status", "a"]);
    let s = stdout(&out);
    assert!(s.contains("a: unknown"), "got: {s}");
    assert!(!s.contains("b: unknown"), "selector must narrow, got: {s}");
}

#[test]
fn unknown_selector_is_an_operational_error() {
    let env = Env::new();
    let a = env.root.path().join("a");
    std::fs::create_dir_all(&a).unwrap();
    env.write_config(&format!(
        "version = 1\n\n[[watch]]\nname = \"a\"\npath = {:?}\n",
        a
    ));

    let out = env.vard(&["status", "ghost"]);
    assert_eq!(code(&out), 2, "not-found selector must exit 2");
    assert!(
        stderr(&out).contains("no watch named or rooted at"),
        "got: {}",
        stderr(&out)
    );
}

#[test]
fn ambiguous_path_selector_is_an_operational_error() {
    let env = Env::new();
    let real = env.root.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = env.root.path().join("link");
    symlink(&real, &link);

    // Two watches at textually-distinct paths that canonicalize to the same
    // directory: a path selector matches both ⇒ ambiguous.
    env.write_config(&format!(
        "version = 1\n\n[[watch]]\nname = \"a\"\npath = {:?}\n\n\
         [[watch]]\nname = \"b\"\npath = {:?}\n",
        real, link
    ));

    let out = env.vard(&["status", real.to_str().unwrap()]);
    assert_eq!(code(&out), 2, "ambiguous selector must exit 2");
    assert!(
        stderr(&out).contains("matches multiple watches"),
        "got: {}",
        stderr(&out)
    );
}

fn symlink(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(not(unix))]
    panic!("symlink test requires a unix platform");
}
