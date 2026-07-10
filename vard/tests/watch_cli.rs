//! End-to-end tests for `vard watch add/remove/list/pause/resume`, driving the
//! real binary against tempdir-isolated config, state, and HOME — nothing here
//! touches the developer's real environment. Repositories are created with a
//! throwaway global git config so commits and inits are deterministic in CI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// A fully isolated environment for one test: its own XDG dirs, HOME, and git
/// global config.
struct Env {
    _root: TempDir,
    config_home: PathBuf,
    state_home: PathBuf,
    home: PathBuf,
    git_config: PathBuf,
}

impl Env {
    fn new() -> Env {
        let root = TempDir::new().unwrap();
        let base = root.path();
        let env = Env {
            config_home: base.join("config"),
            state_home: base.join("state"),
            home: base.join("home"),
            git_config: base.join("gitconfig"),
            _root: root,
        };
        std::fs::create_dir_all(&env.home).unwrap();
        // Deterministic git identity for any repo `watch add --init` creates.
        run_git(
            &env.git_config,
            &[
                "config",
                "--file",
                env.git_config.to_str().unwrap(),
                "user.email",
                "vard-test@example.com",
            ],
        );
        run_git(
            &env.git_config,
            &[
                "config",
                "--file",
                env.git_config.to_str().unwrap(),
                "user.name",
                "Vard Test",
            ],
        );
        env
    }

    /// Runs `vard <args>` in this environment with stdin closed (non-interactive).
    fn vard(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_vard"))
            .args(args)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR_FORCE")
            .stdin(Stdio::null())
            .output()
            .expect("spawn vard")
    }

    fn config_path(&self) -> PathBuf {
        self.config_home.join("vard").join("config.toml")
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(self.config_path()).unwrap_or_default()
    }
}

fn run_git(git_config: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", git_config)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed");
}

/// A directory that is already a git repository on branch `main`.
fn repo(env: &Env, name: &str) -> PathBuf {
    let path = env._root.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    let out = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_GLOBAL", &env.git_config)
        .output()
        .expect("git init");
    assert!(
        out.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // canonicalize so assertions compare against what vard stores.
    std::fs::canonicalize(&path).unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn add_existing_repo_registers_and_lists() {
    let env = Env::new();
    let path = repo(&env, "notes");
    let path_str = path.to_str().unwrap();

    let out = env.vard(&["watch", "add", path_str, "--name", "notes"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    let list = env.vard(&["--format", "json", "watch", "list"]);
    let json = stdout(&list);
    assert!(json.contains("\"name\":\"notes\""), "got: {json}");
    assert!(json.contains("\"paused\":false"), "got: {json}");
    // The stored path is the canonical one.
    assert!(json.contains(path_str), "got: {json}");
}

#[test]
fn add_non_repo_without_init_fails_noninteractively() {
    let env = Env::new();
    let dir = env._root.path().join("plain");
    std::fs::create_dir_all(&dir).unwrap();

    let out = env.vard(&["watch", "add", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("--init"), "stderr: {}", stderr(&out));
    // Nothing was written.
    assert!(env.config_text().is_empty());
}

#[test]
fn add_non_repo_with_init_creates_repository() {
    let env = Env::new();
    let dir = env._root.path().join("fresh");
    std::fs::create_dir_all(&dir).unwrap();

    let out = env.vard(&[
        "watch",
        "add",
        dir.to_str().unwrap(),
        "--init",
        "--branch",
        "main",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"initialized\":true"),
        "got: {}",
        stdout(&out)
    );
    // A real repository now exists.
    assert!(dir.join(".git").exists());
}

#[test]
fn add_seeds_git_excludes_idempotently() {
    let env = Env::new();
    let path = repo(&env, "proj");
    let exclude = path.join(".git").join("info").join("exclude");

    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "proj"]);
    let first = std::fs::read_to_string(&exclude).unwrap();
    assert!(first.contains("node_modules/"));
    assert!(first.contains(".env"));
    assert!(first.contains("*.pem"));

    // Re-add: the managed block must not be duplicated.
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "proj"]);
    let second = std::fs::read_to_string(&exclude).unwrap();
    assert_eq!(
        second.matches(">>> vard managed excludes >>>").count(),
        1,
        "exclude block duplicated on re-add"
    );
}

#[test]
fn pause_and_resume_preserve_comments() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    // Inject a hand-written comment ahead of the config.
    let cfg = env.config_path();
    let original = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("# keep this note\n{original}")).unwrap();

    let out = env.vard(&["watch", "pause", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let paused = env.config_text();
    assert!(
        paused.contains("# keep this note"),
        "comment lost: {paused}"
    );
    assert!(paused.contains("paused = true"), "got: {paused}");

    let out = env.vard(&["watch", "resume", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let resumed = env.config_text();
    assert!(
        resumed.contains("# keep this note"),
        "comment lost: {resumed}"
    );
    assert!(
        !resumed.contains("paused"),
        "paused key not cleared: {resumed}"
    );
}

#[test]
fn remove_leaves_repository_intact() {
    let env = Env::new();
    let path = repo(&env, "notes");
    std::fs::write(path.join("a.txt"), "hi").unwrap();
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "remove", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // Config no longer names it; the repo and file are untouched.
    assert!(!env.config_text().contains("name = \"notes\""));
    assert!(path.join(".git").exists());
    assert!(path.join("a.txt").exists());
}

#[test]
fn remove_by_path_selects_the_watch() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "remove", path.to_str().unwrap()]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let list = env.vard(&["--format", "json", "watch", "list"]);
    assert_eq!(stdout(&list).trim(), "[]");
}

#[test]
fn readd_existing_name_at_new_path_relinks() {
    let env = Env::new();
    let first = repo(&env, "one");
    let second = repo(&env, "two");
    env.vard(&["watch", "add", first.to_str().unwrap(), "--name", "w"]);

    let out = env.vard(&["watch", "add", second.to_str().unwrap(), "--name", "w"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"relinked\":true"),
        "got: {}",
        stdout(&out)
    );

    // Exactly one watch, now pointing at the second path.
    let list = env.vard(&["--format", "json", "watch", "list"]);
    let json = stdout(&list);
    assert_eq!(json.matches("\"name\":\"w\"").count(), 1, "got: {json}");
    assert!(json.contains(second.to_str().unwrap()), "got: {json}");
    assert!(
        !json.contains(first.to_str().unwrap()),
        "old path lingered: {json}"
    );
}

#[test]
fn remove_unknown_watch_is_an_error() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "remove", "ghost"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("ghost"), "stderr: {}", stderr(&out));
}

#[test]
fn list_with_no_config_is_empty() {
    let env = Env::new();
    let out = env.vard(&["--format", "json", "watch", "list"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "[]");
}
