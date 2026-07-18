//! `vard service install|uninstall|start|stop|restart` — manage the vard daemon
//! as a login-session service (VRD-24).
//!
//! `vard run` is the foreground daemon; this module wraps it in the login
//! session's service manager so it starts at login and respawns on failure — a
//! macOS **LaunchAgent** ([`launchd`]) or a Linux **systemd user unit**
//! ([`systemd`]). The unit only execs `vard run`, so all watching and
//! snapshotting still happens there.
//!
//! # Shape
//!
//! The backend is chosen at [`dispatch`] by `#[cfg(target_os = …)]`; each
//! backend exposes the same operation verbs. The unit-file **renderers**
//! ([`launchd::render_plist`], [`systemd::render_unit`]) and the linger-consent
//! decision ([`systemd::decide_linger`]) are pure functions compiled and
//! golden-tested on every platform, so a macOS build still checks the systemd
//! unit text and vice versa.
//!
//! Every side effect the operation flows have — running a subprocess, waiting
//! for the daemon to come up, prompting the user — goes through an injectable
//! seam ([`Runner`], [`Liveness`], [`Prompt`], bundled in [`OpEnv`]), so the
//! flows are unit-tested against a fake runner that records invocations and
//! simulates failures without touching the real service manager.
//!
//! # Output and exit codes
//!
//! Service verbs are **text-only** (the same class as `vard logs`): they print
//! human status lines and reject an explicit `--format json`/`jsonl`. Exit codes
//! follow the system-wide convention through [`CmdError`]: `0` success (a
//! stop-when-stopped or uninstall-when-absent is an idempotent success), `1`
//! when the unit is in place but the daemon did not come up, `2` for an
//! operational error (unsupported platform, a launchctl/systemctl/loginctl
//! failure, an unresolvable home path).

pub(crate) mod launchd;
pub(crate) mod systemd;

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::{ColorWhen, OutputFormat, ServiceCommand};
use crate::command::{self, CmdError, CmdResult, OutCtx};
use crate::instance::{self, DaemonProbe};
use crate::paths;

use std::process::ExitCode;

/// Wall-clock bound on each service-manager subprocess (launchctl, systemctl,
/// loginctl), so a wedged login session cannot hang the command.
const RUN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to poll for the daemon to take the instance lock after starting the
/// service before declaring the start unverified.
const VERIFY_BUDGET: Duration = Duration::from_secs(5);

/// Interval between liveness probes while waiting out [`VERIFY_BUDGET`].
const VERIFY_POLL: Duration = Duration::from_millis(100);

/// Poll interval while waiting for a timed-out subprocess to die.
const KILL_POLL: Duration = Duration::from_millis(20);

/// Production entry point for `vard service <subcommand>`.
pub(crate) fn run(cmd: ServiceCommand, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    command::finish(run_inner(cmd, color, format))
}

fn run_inner(cmd: ServiceCommand, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let out = OutCtx::resolve(color, format);

    // Text-only, like `logs` and `diff`: reject an explicit machine format.
    if matches!(
        out.raw_format,
        Some(OutputFormat::Json) | Some(OutputFormat::Jsonl)
    ) {
        return Err(CmdError::err(
            "service prints human status lines and is text-only; --format json/jsonl is not \
             supported",
        ));
    }

    // The linger prompt (Linux install) only makes sense when both ends are a
    // terminal — a piped stdin has no one to answer it.
    let is_tty = out.is_tty && io::stdin().is_terminal();

    let env = OpEnv {
        runner: &SystemRunner,
        liveness: &DaemonLiveness,
        prompt: &StdinPrompt,
    };

    let lines = dispatch(cmd, &env, is_tty)?;
    emit_lines(&lines)
}

/// Writes the operation's human status lines to stdout, tolerating a broken pipe
/// (a reader that went away) the way the shared emitters do.
fn emit_lines(lines: &[String]) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = (|| -> io::Result<()> {
        for line in lines {
            writeln!(w, "{line}")?;
        }
        Ok(())
    })();
    command::finish_write(res)
}

// --- backend selection ----------------------------------------------------

#[cfg(target_os = "macos")]
fn dispatch(cmd: ServiceCommand, env: &OpEnv, _is_tty: bool) -> Result<Vec<String>, CmdError> {
    let uid = rustix::process::getuid().as_raw();
    let plist = launchd::plist_path()?;
    match cmd {
        ServiceCommand::Install(args) => {
            let bin = resolve_service_binary()?;
            launchd::install(env, uid, &plist, &bin, args.dry_run)
        }
        ServiceCommand::Uninstall => launchd::uninstall(env, uid, &plist),
        ServiceCommand::Start => launchd::start(env, uid, &plist),
        ServiceCommand::Stop => launchd::stop(env, uid),
        ServiceCommand::Restart => launchd::restart(env, uid, &plist),
    }
}

#[cfg(target_os = "linux")]
fn dispatch(cmd: ServiceCommand, env: &OpEnv, is_tty: bool) -> Result<Vec<String>, CmdError> {
    let unit = systemd::unit_path()?;
    match cmd {
        ServiceCommand::Install(args) => {
            let bin = resolve_service_binary()?;
            if args.dry_run {
                // Dry run touches nothing and needs no reachable session.
                return systemd::install(
                    env,
                    &unit,
                    &bin,
                    true,
                    args.linger,
                    args.no_linger,
                    is_tty,
                );
            }
            systemd::ensure_reachable(env)?;
            systemd::install(env, &unit, &bin, false, args.linger, args.no_linger, is_tty)
        }
        ServiceCommand::Uninstall => {
            systemd::ensure_reachable(env)?;
            systemd::uninstall(env, &unit)
        }
        ServiceCommand::Start => {
            systemd::ensure_reachable(env)?;
            systemd::start(env, &unit)
        }
        ServiceCommand::Stop => {
            systemd::ensure_reachable(env)?;
            systemd::stop(env)
        }
        ServiceCommand::Restart => {
            systemd::ensure_reachable(env)?;
            systemd::restart(env)
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn dispatch(_cmd: ServiceCommand, _env: &OpEnv, _is_tty: bool) -> Result<Vec<String>, CmdError> {
    Err(CmdError::err(
        "vard service is supported on macOS (launchd) and Linux (systemd) only; run `vard run` \
         under your platform's own supervisor",
    ))
}

// --- injectable seams -----------------------------------------------------

/// The bundle of side-effect seams the operation flows run through, so the flows
/// are unit-tested against fakes.
pub(crate) struct OpEnv<'a> {
    /// Runs a service-manager subprocess (launchctl / systemctl / loginctl).
    pub(crate) runner: &'a dyn Runner,
    /// Waits for the daemon to come up after the service is started.
    pub(crate) liveness: &'a dyn Liveness,
    /// Asks the user a yes/no question (the Linux linger consent prompt).
    pub(crate) prompt: &'a dyn Prompt,
}

/// The captured result of a service-manager subprocess.
pub(crate) struct RunOutput {
    /// Whether the program launched at all (`false` when the binary is missing).
    pub(crate) spawned: bool,
    /// The process exit code, or `None` when it was killed or never launched.
    pub(crate) code: Option<i32>,
    /// Captured stdout. The service verbs themselves never need it (their
    /// commands are actioned by exit code alone), but doctor's service-context
    /// probes (`loginctl show-user --value`, `systemctl --user
    /// show-environment`, `launchctl print`) parse it, so [`run_bounded`]
    /// captures it unconditionally rather than growing a second variant.
    pub(crate) stdout: String,
    /// Captured stderr (used to summarize a failure).
    pub(crate) stderr: String,
    /// Whether the process was killed for exceeding [`RUN_TIMEOUT`].
    pub(crate) timed_out: bool,
}

impl RunOutput {
    /// A clean, on-time, zero-exit run.
    pub(crate) fn success(&self) -> bool {
        self.spawned && !self.timed_out && self.code == Some(0)
    }

    /// The first non-blank stderr line, for a sanitized one-line failure detail.
    pub(crate) fn detail(&self) -> String {
        if self.timed_out {
            return format!("timed out after {RUN_TIMEOUT:?}");
        }
        // A spawn failure and a nonzero exit both summarize to the first stderr
        // line (the OS error text and the tool's own message, respectively).
        first_line(&self.stderr)
    }
}

/// Runs a service-manager subprocess. The single seam every backend shells out
/// through, so operation flows inject a fake.
pub(crate) trait Runner {
    fn run(&self, program: &str, args: &[&str]) -> RunOutput;
}

/// Waits for the daemon to become live after a start.
pub(crate) trait Liveness {
    /// Polls until the daemon is observed holding the instance lock, or the
    /// budget expires. Returns whether it came up.
    fn wait_until_up(&self) -> bool;
}

/// Asks the user a yes/no question.
pub(crate) trait Prompt {
    /// Prints `question` and reads a line; `true` only on an affirmative answer.
    fn confirm(&self, question: &str) -> bool;
}

/// The production [`Runner`]: a timeout-bounded subprocess that drains both
/// pipes and kills the process group on the deadline (the bounded-probe pattern
/// vard-core's `git_output_timed` uses).
struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> RunOutput {
        run_bounded(program, args, RUN_TIMEOUT)
    }
}

/// The production [`Liveness`]: polls the instance lock the same way `vard
/// status`/`notify` do — the single daemon-liveness signal, not a parallel one.
struct DaemonLiveness;

impl Liveness for DaemonLiveness {
    fn wait_until_up(&self) -> bool {
        let lock = match paths::lock_file() {
            Ok(lock) => lock,
            Err(_) => return false,
        };
        let deadline = Instant::now() + VERIFY_BUDGET;
        loop {
            if let Ok(DaemonProbe::Running) = instance::probe_daemon(&lock) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(VERIFY_POLL);
        }
    }
}

/// The production [`Prompt`]: reads one line from stdin, `true` on `y`/`yes`.
struct StdinPrompt;

impl Prompt for StdinPrompt {
    fn confirm(&self, question: &str) -> bool {
        print!("{question} ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// Runs `program args` with a wall-clock bound, draining both pipes concurrently
/// and killing the child's process group on the deadline so a wedged transport
/// cannot outlive it. Shared by the service verbs (launchctl/systemctl/loginctl)
/// and doctor's own service-context probes — the single bounded-subprocess seam
/// for every login-session tool vard shells out to.
pub(crate) fn run_bounded(program: &str, args: &[&str], timeout: Duration) -> RunOutput {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return RunOutput {
                spawned: false,
                code: None,
                stdout: String::new(),
                stderr: e.to_string(),
                timed_out: false,
            };
        }
    };

    let mut child_stderr = child.stderr.take().expect("stderr was piped");
    let mut child_stdout = child.stdout.take().expect("stdout was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_stderr.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let mut wait_error = None;
    let (code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_group(&mut child);
                    let _ = child.wait();
                    break (None, true);
                }
                std::thread::sleep(KILL_POLL);
            }
            Err(e) => {
                // try_wait itself failed (e.g. a wait()-family syscall error).
                // The child may still be alive: kill and reap it the same way
                // the timeout branch does, so the pipe-reader threads below —
                // which block until the child's stdout/stderr fds close — are
                // not left waiting on a still-running process.
                wait_error = Some(e.to_string());
                kill_group(&mut child);
                let _ = child.wait();
                break (None, false);
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
    if let Some(err) = wait_error {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!("waiting on the process failed: {err}"));
    }
    RunOutput {
        spawned: true,
        code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr,
        timed_out,
    }
}

/// Kills the timed-out child's process group (it leads its own group via
/// `process_group(0)`), so a transport helper it spawned dies with it.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_child(child),
            rustix::process::Signal::KILL,
        );
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

// --- binary-path resolution -----------------------------------------------

/// Resolves the path to record as the service's ExecStart: the path `vard` was
/// invoked through, symlinks deliberately preserved (a Homebrew shim keeps
/// `/opt/homebrew/bin/vard`, not a versioned Cellar path). Falls back to
/// `current_exe` when argv0 is unresolvable.
pub(crate) fn resolve_service_binary() -> Result<PathBuf, CmdError> {
    let argv0 = std::env::args_os().next();
    let cwd = std::env::current_dir().ok();
    if let (Some(argv0), Some(cwd)) = (argv0.as_deref(), cwd.as_deref())
        && let Some(path) =
            resolve_from_argv0(argv0, std::env::var_os("PATH").as_deref(), cwd, |p| {
                p.exists()
            })
    {
        return Ok(path);
    }
    std::env::current_exe()
        .map_err(|e| CmdError::err(format!("could not resolve the vard binary path: {e}")))
}

/// Pure core of [`resolve_service_binary`]: an argv0 that carries a path
/// separator is absolutized against `cwd` (never canonicalized, so a symlink is
/// preserved); a bare name is looked up on `path_var`. Returns the first
/// candidate `exists` accepts, or `None`.
fn resolve_from_argv0(
    argv0: &OsStr,
    path_var: Option<&OsStr>,
    cwd: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let p = Path::new(argv0);
    let has_separator = p.parent().is_some_and(|par| !par.as_os_str().is_empty());
    if has_separator {
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        return exists(&candidate).then_some(candidate);
    }

    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(argv0);
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

// --- shared path helpers --------------------------------------------------

/// An absolute `$HOME`, or `None` — the base for the launchd LaunchAgents path
/// (which is deliberately non-XDG) and the systemd XDG config resolution.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Pure XDG config-base resolution (`$XDG_CONFIG_HOME`, default `~/.config`),
/// mirroring `paths::resolve` but stopping at the base (systemd's unit dir is
/// `<base>/systemd/user`, not `<base>/vard`).
fn config_base(var_value: Option<OsString>, home: Option<&Path>) -> Option<PathBuf> {
    let xdg = var_value.map(PathBuf::from).filter(|p| p.is_absolute());
    xdg.or_else(|| home.filter(|h| h.is_absolute()).map(|h| h.join(".config")))
}

/// The first non-empty, trimmed line of `text`, so a multi-line launchctl or
/// systemctl stderr is summarized to its most meaningful line. Mirrors doctor's
/// `first_line`.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no details)")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake [`Runner`] that returns programmed responses in order and records
    /// every invocation as `program arg arg …`.
    pub(crate) struct FakeRunner {
        responses: RefCell<Vec<RunOutput>>,
        pub(crate) calls: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        pub(crate) fn new(responses: Vec<RunOutput>) -> Self {
            FakeRunner {
                responses: RefCell::new(responses),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> RunOutput {
            let mut record = String::from(program);
            for a in args {
                record.push(' ');
                record.push_str(a);
            }
            self.calls.borrow_mut().push(record);
            self.responses
                .borrow_mut()
                .drain(..1)
                .next()
                .expect("FakeRunner ran out of programmed responses")
        }
    }

    pub(crate) fn ok() -> RunOutput {
        RunOutput {
            spawned: true,
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    pub(crate) fn fail(stderr: &str) -> RunOutput {
        RunOutput {
            spawned: true,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    pub(crate) fn not_spawned() -> RunOutput {
        RunOutput {
            spawned: false,
            code: None,
            stdout: String::new(),
            stderr: "No such file or directory (os error 2)".to_string(),
            timed_out: false,
        }
    }

    /// A [`Liveness`] fake with a fixed answer.
    pub(crate) struct FakeLiveness(pub(crate) bool);
    impl Liveness for FakeLiveness {
        fn wait_until_up(&self) -> bool {
            self.0
        }
    }

    /// A [`Prompt`] fake with a fixed answer.
    pub(crate) struct FakePrompt(pub(crate) bool);
    impl Prompt for FakePrompt {
        fn confirm(&self, _question: &str) -> bool {
            self.0
        }
    }

    #[test]
    fn argv0_with_separator_absolutizes_against_cwd_without_canonicalizing() {
        let got = resolve_from_argv0(
            OsStr::new("./bin/vard"),
            None,
            Path::new("/work/dir"),
            |_| true,
        );
        assert_eq!(got, Some(PathBuf::from("/work/dir/./bin/vard")));
    }

    #[test]
    fn absolute_argv0_is_used_verbatim() {
        let got = resolve_from_argv0(
            OsStr::new("/opt/homebrew/bin/vard"),
            None,
            Path::new("/anywhere"),
            |_| true,
        );
        assert_eq!(got, Some(PathBuf::from("/opt/homebrew/bin/vard")));
    }

    #[test]
    fn separator_argv0_that_does_not_exist_is_none() {
        let got = resolve_from_argv0(
            OsStr::new("/nope/vard"),
            None,
            Path::new("/anywhere"),
            |_| false,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn bare_name_is_looked_up_on_path_and_first_hit_wins() {
        let got = resolve_from_argv0(
            OsStr::new("vard"),
            Some(OsStr::new("/a:/b:/c")),
            Path::new("/cwd"),
            |p| p == Path::new("/b/vard"),
        );
        assert_eq!(got, Some(PathBuf::from("/b/vard")));
    }

    #[test]
    fn bare_name_not_on_path_is_none() {
        let got = resolve_from_argv0(
            OsStr::new("vard"),
            Some(OsStr::new("/a:/b")),
            Path::new("/cwd"),
            |_| false,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn bare_name_with_no_path_var_is_none() {
        let got = resolve_from_argv0(OsStr::new("vard"), None, Path::new("/cwd"), |_| true);
        assert_eq!(got, None);
    }

    #[test]
    fn run_bounded_captures_a_clean_exit() {
        let out = run_bounded("sh", &["-c", "exit 0"], Duration::from_secs(5));
        assert!(out.spawned);
        assert!(out.success());
        assert!(!out.timed_out);
    }

    #[test]
    fn run_bounded_kills_and_reaps_on_timeout() {
        // Regression coverage for the try_wait loop: a process that outlives
        // its budget is killed, reaped, and the pipe readers still join —
        // exactly the sequence the try_wait-Err branch now also follows.
        let out = run_bounded("sh", &["-c", "sleep 5"], Duration::from_millis(50));
        assert!(out.spawned);
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(out.detail().starts_with("timed out after"));
    }

    #[test]
    fn config_base_prefers_absolute_xdg() {
        let base = config_base(Some(OsString::from("/x/cfg")), Some(Path::new("/home/u")));
        assert_eq!(base, Some(PathBuf::from("/x/cfg")));
    }

    #[test]
    fn config_base_falls_back_to_home_dot_config() {
        let base = config_base(None, Some(Path::new("/home/u")));
        assert_eq!(base, Some(PathBuf::from("/home/u/.config")));
    }

    #[test]
    fn config_base_ignores_relative_xdg() {
        let base = config_base(Some(OsString::from("rel/cfg")), Some(Path::new("/home/u")));
        assert_eq!(base, Some(PathBuf::from("/home/u/.config")));
    }

    #[test]
    fn config_base_none_without_home_or_xdg() {
        assert_eq!(config_base(None, None), None);
    }

    #[test]
    fn first_line_takes_first_nonblank_trimmed() {
        assert_eq!(first_line("\n  boom: nope\nmore\n"), "boom: nope");
        assert_eq!(first_line("   \n\n"), "(no details)");
    }
}
