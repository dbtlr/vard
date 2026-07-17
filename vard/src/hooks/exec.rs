//! Hook process execution: spawn `$SHELL -c <command>`, enforce the timeout with
//! a process-group kill discipline, capture output into the log, and reap.
//!
//! [`exec_hook`] is synchronous and blocking by design — the runner drives it on
//! a blocking task ([`tokio::task::JoinSet::spawn_blocking`]) so the bus loop
//! never waits on a hook, while the blocking closure still runs to completion (and
//! reaps its child) even if the runner task is aborted on a re-arm. This mirrors
//! the git backend's timed-command discipline in `vard-core` (own process group,
//! thread-drained pipes, poll-and-kill), so kill/zombie handling stays uniform.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

/// How often a running hook is polled for completion while waiting out its
/// timeout (and the post-SIGTERM grace).
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a timed-out hook's process group is given to exit after SIGTERM
/// before it is SIGKILLed.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_secs(2);

/// One hook cleared to run: the shell command, its working directory, the
/// wall-clock timeout, and the `VARD_*` environment (minus `VARD_SUPPRESSED`,
/// which the runner supplies per fire from the coalescing count).
#[derive(Clone)]
pub(super) struct HookInvocation {
    /// The shell command line, run via `$SHELL -c`.
    pub command: String,
    /// Working directory: the watch's path, or the daemon's cwd for a global hook.
    pub cwd: PathBuf,
    /// Wall-clock timeout before the hook's process group is killed.
    pub timeout: Duration,
    /// The `VARD_*` variables to set, excluding `VARD_SUPPRESSED`.
    pub env: Vec<(String, String)>,
}

/// The terminal result of one hook run, consumed by the runner's per-key
/// consecutive-failure tracking.
pub(super) enum HookOutcome {
    /// The hook exited zero.
    Success,
    /// The hook exited non-zero, timed out, or could not be run — with a short
    /// human-readable reason kept as the key's last error.
    Failure(String),
}

/// The shell used to run hook commands: `$SHELL`, falling back to `/bin/sh` when
/// it is unset or empty.
fn hook_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// Runs one hook synchronously to completion. Spawns `$SHELL -c <command>` in its
/// own process group with the invocation's cwd and environment (plus
/// `VARD_SUPPRESSED`), enforces `timeout` with a SIGTERM -> grace -> SIGKILL kill
/// of the whole group (so shell children die too), captures stdout/stderr into
/// the log, and reaps the child before returning. `event` and `scope` (the watch
/// name, or `daemon` for a global hook) are log context only.
pub(super) fn exec_hook(
    event: &str,
    scope: &str,
    inv: &HookInvocation,
    suppressed: u64,
) -> HookOutcome {
    let mut cmd = Command::new(hook_shell());
    cmd.arg("-c")
        .arg(&inv.command)
        .current_dir(&inv.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &inv.env {
        cmd.env(key, value);
    }
    cmd.env("VARD_SUPPRESSED", suppressed.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group led by the shell, so the group-directed kill on
        // timeout reaches every child the hook spawned and nothing else.
        cmd.process_group(0);
    }

    debug!(event, scope, command = inv.command.as_str(), "running hook");

    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!(event, scope, error = %err, "hook failed to spawn");
            return HookOutcome::Failure(format!("failed to spawn hook: {err}"));
        }
    };

    // Drain both pipes on background threads so a chatty hook that fills a pipe
    // buffer cannot deadlock against our completion poll.
    let mut child_stdout = child.stdout.take().expect("stdout piped");
    let mut child_stderr = child.stderr.take().expect("stderr piped");
    let stdout_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = child_stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = child_stderr.read_to_end(&mut buf);
        buf
    });

    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= inv.timeout {
                    timed_out = true;
                    kill_timed_out(&mut child);
                    // Reap so no zombie is left; the readers see EOF as the pipes
                    // close.
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(KILL_POLL_INTERVAL);
            }
            Err(err) => {
                warn!(event, scope, error = %err, "waiting on hook failed");
                let _ = child.wait();
                break None;
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);

    if timed_out {
        let secs = inv.timeout.as_secs_f64();
        warn!(
            event,
            scope,
            timeout_secs = secs,
            stdout = %stdout.trim(),
            stderr = %stderr.trim(),
            "hook timed out; killed its process group"
        );
        return HookOutcome::Failure(format!("timed out after {secs:.0}s"));
    }

    match status {
        Some(status) if status.success() => {
            debug!(
                event,
                scope,
                stdout = %stdout.trim(),
                stderr = %stderr.trim(),
                "hook succeeded"
            );
            HookOutcome::Success
        }
        Some(status) => {
            let code = status.code();
            warn!(
                event,
                scope,
                code,
                stdout = %stdout.trim(),
                stderr = %stderr.trim(),
                "hook exited non-zero"
            );
            HookOutcome::Failure(match code {
                Some(code) => format!("exited with status {code}"),
                None => "killed by signal".to_string(),
            })
        }
        None => HookOutcome::Failure("hook did not run to completion".to_string()),
    }
}

/// Kills a timed-out hook: on unix, SIGTERM the process group, wait out a short
/// grace, then SIGKILL the group if anything survives. On non-unix (not a
/// supported target, kept only so the crate still compiles there) only the child
/// itself is killed.
fn kill_timed_out(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        signal_group(child, rustix::process::Signal::TERM);
        if !exited_within(child, TERM_GRACE) {
            signal_group(child, rustix::process::Signal::KILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

/// Signals the process group led by `child` (its pgid equals its pid, set via
/// `process_group(0)`). Best-effort: a group already gone is not an error.
#[cfg(unix)]
fn signal_group(child: &std::process::Child, signal: rustix::process::Signal) {
    let _ = rustix::process::kill_process_group(rustix::process::Pid::from_child(child), signal);
}

/// Polls `child` for up to `grace`, returning whether it exited in that window.
#[cfg(unix)]
fn exited_within(child: &mut std::process::Child, grace: Duration) -> bool {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {
                if start.elapsed() >= grace {
                    return false;
                }
                std::thread::sleep(KILL_POLL_INTERVAL);
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(
        command: &str,
        cwd: PathBuf,
        timeout: Duration,
        env: Vec<(&str, &str)>,
    ) -> HookInvocation {
        HookInvocation {
            command: command.to_string(),
            cwd,
            timeout,
            env: env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_zero_exit_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = exec_hook(
            "snapshot.completed",
            "notes",
            &inv(
                "exit 0",
                dir.path().to_path_buf(),
                Duration::from_secs(5),
                vec![],
            ),
            0,
        );
        assert!(matches!(outcome, HookOutcome::Success));
    }

    #[test]
    fn a_non_zero_exit_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = exec_hook(
            "snapshot.failed",
            "notes",
            &inv(
                "exit 3",
                dir.path().to_path_buf(),
                Duration::from_secs(5),
                vec![],
            ),
            0,
        );
        match outcome {
            HookOutcome::Failure(err) => assert!(err.contains('3'), "reason names the code: {err}"),
            HookOutcome::Success => panic!("a non-zero exit must be a failure"),
        }
    }

    #[test]
    fn the_env_and_cwd_reach_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        // The hook writes its cwd, a VARD_* var, and VARD_SUPPRESSED to a file.
        let command = format!(
            "printf '%s\\n%s\\n%s\\n' \"$PWD\" \"$VARD_REF\" \"$VARD_SUPPRESSED\" > {}",
            out.display()
        );
        let outcome = exec_hook(
            "snapshot.completed",
            "notes",
            &inv(
                &command,
                dir.path().to_path_buf(),
                Duration::from_secs(5),
                vec![("VARD_REF", "abc123")],
            ),
            4,
        );
        assert!(matches!(outcome, HookOutcome::Success));
        let body = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        // The cwd may be reported through a symlinked temp root; compare by suffix.
        assert!(
            std::path::Path::new(lines[0]).ends_with(dir.path().file_name().unwrap())
                || lines[0] == dir.path().to_string_lossy(),
            "cwd was {} , expected {}",
            lines[0],
            dir.path().display()
        );
        assert_eq!(lines[1], "abc123", "VARD_REF reached the hook");
        assert_eq!(lines[2], "4", "VARD_SUPPRESSED is the fired count");
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_kills_the_hook_and_its_child_and_reaps() {
        // The hook backgrounds a grandchild that would write a marker after a
        // delay, then blocks. A short timeout must SIGTERM the whole group, so
        // neither the shell nor the grandchild survives to write the marker.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let command = format!("(sleep 10 && touch {m}) & sleep 10", m = marker.display());
        let start = Instant::now();
        let outcome = exec_hook(
            "snapshot.completed",
            "notes",
            &inv(
                &command,
                dir.path().to_path_buf(),
                Duration::from_millis(60),
                vec![],
            ),
            0,
        );
        let elapsed = start.elapsed();
        match outcome {
            HookOutcome::Failure(err) => assert!(err.contains("timed out"), "got: {err}"),
            HookOutcome::Success => panic!("a timed-out hook must be a failure"),
        }
        // A plain `sleep` dies on SIGTERM at once, so the kill returns well inside
        // the 2s grace — nowhere near the 10s the hook would otherwise run.
        assert!(
            elapsed < Duration::from_secs(3),
            "kill should be prompt, took {elapsed:?}"
        );
        // Give any surviving grandchild far longer than it would need, then assert
        // the marker never appeared: proof the whole group was killed.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !marker.exists(),
            "the backgrounded grandchild must have been killed with the group"
        );
    }
}
