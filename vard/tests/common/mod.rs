//! Shared integration-test harness.
//!
//! A fully isolated environment — its own XDG config/state dirs, HOME, and a
//! throwaway global git config — so no test's assertions can leak out to (or in
//! from) the developer's real environment. Every command runs with color forced
//! off-detection scrubbed (`NO_COLOR`/`CLICOLOR_FORCE` removed) and any inherited
//! `$EDITOR`/`$VISUAL` cleared, the strictest scrubbing any suite needs.
//!
//! Each `tests/*.rs` binary compiles this module separately (the standard
//! `tests/common` pattern via `mod common;`), so a crate-level `allow(dead_code)`
//! covers the helpers a given suite does not exercise.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// A fully isolated environment for one test.
pub struct Env {
    /// The tempdir root; held so it outlives the test.
    pub root: TempDir,
    pub config_home: PathBuf,
    pub state_home: PathBuf,
    pub home: PathBuf,
    pub git_config: PathBuf,
}

impl Env {
    pub fn new() -> Env {
        let root = TempDir::new().unwrap();
        let base = root.path();
        let env = Env {
            config_home: base.join("config"),
            state_home: base.join("state"),
            home: base.join("home"),
            git_config: base.join("gitconfig"),
            root,
        };
        std::fs::create_dir_all(&env.home).unwrap();
        // Deterministic git identity for any repository a test creates.
        env.set_git_config("user.email", "vard-test@example.com");
        env.set_git_config("user.name", "Vard Test");
        env.set_git_config("init.defaultBranch", "main");
        env
    }

    /// A `vard` command with the isolated environment applied but not yet run, so
    /// a test can layer on `.env("EDITOR", …)`, custom stdio, or `.spawn()`.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vard"));
        cmd.args(args)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env_remove("EDITOR")
            .env_remove("VISUAL")
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR_FORCE");
        cmd
    }

    /// Runs `vard <args>` to completion with stdin closed (non-interactive).
    pub fn vard(&self, args: &[&str]) -> Output {
        self.command(args)
            .stdin(Stdio::null())
            .output()
            .expect("spawn vard")
    }

    /// `$XDG_CONFIG_HOME/vard`.
    pub fn config_dir(&self) -> PathBuf {
        self.config_home.join("vard")
    }

    /// `$XDG_CONFIG_HOME/vard/config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    /// `$XDG_STATE_HOME/vard/health`.
    pub fn health_file(&self) -> PathBuf {
        self.state_home.join("vard").join("health")
    }

    /// `$XDG_STATE_HOME/vard/journal`.
    pub fn journal_dir(&self) -> PathBuf {
        self.state_home.join("vard").join("journal")
    }

    /// Writes `contents` as the config file, creating the config dir.
    pub fn write_config(&self, contents: &str) {
        std::fs::create_dir_all(self.config_dir()).unwrap();
        std::fs::write(self.config_path(), contents).unwrap();
    }

    /// Reads the config file, panicking if it is absent.
    pub fn read_config(&self) -> String {
        std::fs::read_to_string(self.config_path()).unwrap()
    }

    /// Reads the config file, or the empty string when it is absent.
    pub fn config_text(&self) -> String {
        std::fs::read_to_string(self.config_path()).unwrap_or_default()
    }

    /// Sets a key in the throwaway global git config.
    pub fn set_git_config(&self, key: &str, value: &str) {
        let out = self.run_git(&[
            "config",
            "--file",
            self.git_config.to_str().unwrap(),
            key,
            value,
        ]);
        assert!(out.status.success(), "git config {key} failed");
    }

    /// Runs `git <args>` against the throwaway global git config, returning the
    /// raw output (the caller asserts on success).
    pub fn run_git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .output()
            .expect("spawn git")
    }

    /// Installs an executable editor script that overwrites the file it is given
    /// with `new_contents`, and returns its path. Named by a stable hash of the
    /// payload so two distinct editors (a `$VISUAL` and a `$EDITOR`) never collide
    /// on one file.
    pub fn editor_writing(&self, new_contents: &str) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        new_contents.hash(&mut hasher);
        let script = self
            .root
            .path()
            .join(format!("fake-editor-{:x}.sh", hasher.finish()));
        // The editor is invoked as `script <file>`; write the payload to $1.
        let body = format!("#!/bin/sh\ncat > \"$1\" <<'VARD_EOF'\n{new_contents}\nVARD_EOF\n");
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        script
    }
}

/// A real `vard run` daemon spawned against an [`Env`], killed (and reaped) on
/// drop so a failing assertion can never leak the process.
pub struct DaemonGuard(std::process::Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Env {
    /// Spawns the real daemon detached from the test's stdio and waits until it
    /// is demonstrably up — the health file written, the instance lock held —
    /// so a subsequent CLI command takes the daemon-present dispatch path.
    /// Panics if the daemon never comes up. Drop the guard to stop it.
    pub fn spawn_daemon(&self) -> DaemonGuard {
        let child = self
            .command(&["run"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vard run");
        let guard = DaemonGuard(child);
        let health = self.health_file();
        for _ in 0..100 {
            if health.exists() {
                return guard;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("the daemon never started (no health file)");
    }
}

/// The process exit code, or a panic if it was signalled.
pub fn code(out: &Output) -> i32 {
    out.status.code().expect("process exited via a signal")
}

/// Stdout as a lossy UTF-8 string.
pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Stderr as a lossy UTF-8 string.
pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
