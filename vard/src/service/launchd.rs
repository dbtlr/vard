//! macOS LaunchAgent backend for `vard service` (VRD-24).
//!
//! The service is a per-user **LaunchAgent** — a plist at
//! `~/Library/LaunchAgents/com.dbtlr.vard.plist` loaded into the user's GUI
//! login domain (`gui/<uid>`) with `launchctl`. The plist's `RunAtLoad` starts
//! the daemon at login and `KeepAlive { SuccessfulExit = false }` respawns it on
//! a crash while leaving a clean `SIGTERM` exit down.
//!
//! [`render_plist`] is pure and golden-tested on every platform; the operation
//! flows shell out through the injected [`Runner`](super::Runner) so they are
//! tested against a fake.

use std::fs;
use std::path::{Path, PathBuf};

use crate::atomic;
use crate::command::CmdError;

use super::{OpEnv, PreflightOutcome, RunOutput, first_line};

/// The LaunchAgent label and plist basename stem.
const LABEL: &str = "com.dbtlr.vard";

/// `~/Library/LaunchAgents/com.dbtlr.vard.plist` — the LaunchAgents location is
/// dictated by launchd and is deliberately non-XDG.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn plist_path() -> Result<PathBuf, CmdError> {
    let home = super::home().ok_or_else(|| {
        CmdError::err("HOME is not set to an absolute path; cannot locate ~/Library/LaunchAgents")
    })?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Escapes the five XML metacharacters so a binary path with a `&` or a quote in
/// it cannot break the plist.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Renders the LaunchAgent plist for a daemon exec'd from `bin`. Pure: compiled
/// and golden-tested on every platform.
///
/// `RunAtLoad` starts at login; `KeepAlive { SuccessfulExit = false }` respawns
/// only on a failure exit (a clean `SIGTERM` stays down); `ProcessType
/// Background` and `ThrottleInterval 10` keep the daemon out of a tight crash
/// loop (it exits 2 on lock contention). Stdout/stderr are deliberately not
/// redirected — the daemon writes its own rotated logfile.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn render_plist(bin: &str) -> String {
    let label = LABEL;
    let bin = xml_escape(bin);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{bin}</string>
		<string>run</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Background</string>
	<key>ThrottleInterval</key>
	<integer>10</integer>
</dict>
</plist>
"#
    )
}

fn domain(uid: u32) -> String {
    format!("gui/{uid}")
}

/// `gui/<uid>/<label>` — the launchd service target `launchctl print`,
/// `bootout`, and `kickstart` all address the LaunchAgent by. Shared with
/// doctor's `service-agent` check, which prints the same target to read the
/// service's actual loaded/running state.
pub(crate) fn service_target(uid: u32) -> String {
    format!("gui/{uid}/{LABEL}")
}

/// What `launchctl print gui/<uid>/<label>` reported about the loaded
/// LaunchAgent. The single parse of that output — shared by the `start` verb's
/// state probe (VRD-59) and doctor's `service-agent` check — so the two can
/// never disagree on what a given `launchctl print` means.
#[derive(Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum LaunchctlPrint {
    /// A `pid = ` line is present — the daemon is running.
    Running,
    /// No `pid = ` line, but a `last exit code = N` (nonzero) is — the service
    /// is loaded but not running, having exited on its own.
    Exited { code: i32 },
    /// `launchctl print` exited nonzero — the label is not loaded.
    NotLoaded,
    /// The output does not match a recognized shape. Carries a short detail.
    Unparsed(String),
}

/// Parses a captured `launchctl print` run for the shapes callers care about.
/// Pure over the runner's output, so it is unit-tested against representative
/// fixtures without a real launchd. `launchctl` failing to spawn or timing out
/// is a *probe* failure, not evidence the service is unloaded, so only a *ran
/// and exited nonzero* result is read as [`LaunchctlPrint::NotLoaded`] —
/// everything else that cannot be confidently interpreted is
/// [`LaunchctlPrint::Unparsed`], never a guess. (The `start` verb treats every
/// non-`Running` outcome, `Unparsed` included, as "recover it", so a probe that
/// cannot run never becomes a refusal.)
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_launchctl_print(out: &RunOutput) -> LaunchctlPrint {
    if !out.spawned || out.timed_out {
        return LaunchctlPrint::Unparsed(out.detail());
    }
    if !out.success() {
        return LaunchctlPrint::NotLoaded;
    }
    let running = out.stdout.lines().any(|l| {
        l.trim()
            .strip_prefix("pid = ")
            .is_some_and(|rest| rest.trim().parse::<u64>().is_ok())
    });
    if running {
        return LaunchctlPrint::Running;
    }
    let exited = out.stdout.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("last exit code = ")?;
        rest.trim().parse::<i32>().ok()
    });
    match exited {
        Some(code) if code != 0 => LaunchctlPrint::Exited { code },
        _ => {
            let sample = if out.stdout.trim().is_empty() {
                &out.stderr
            } else {
                &out.stdout
            };
            LaunchctlPrint::Unparsed(first_line(sample))
        }
    }
}

/// The failure message shared by install/start/restart when the unit is in place
/// but the daemon never took the instance lock.
fn verify_failed() -> CmdError {
    CmdError::attention(
        "the service was loaded and started, but the daemon did not come up — run `vard run` in \
         the foreground to see why",
    )
}

/// Waits for the daemon to come up, appending a confirmation line on success.
fn finish_with_verify(env: &OpEnv, mut lines: Vec<String>) -> Result<Vec<String>, CmdError> {
    if env.liveness.wait_until_up() {
        lines.push("The vard daemon is up.".to_string());
        Ok(lines)
    } else {
        Err(verify_failed())
    }
}

/// `vard service install`: render and write the plist, bootstrap it into the
/// login domain (unloading and retrying once for idempotency), and verify the
/// daemon came up. `--dry-run` prints the plan and touches nothing.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn install(
    env: &OpEnv,
    uid: u32,
    plist: &Path,
    bin: &Path,
    dry_run: bool,
    preflight: &PreflightOutcome,
) -> Result<Vec<String>, CmdError> {
    let content = render_plist(&bin.to_string_lossy());

    if dry_run {
        let mut lines = vec![
            "Dry run — nothing was written.".to_string(),
            format!("Binary:    {}", bin.display()),
            format!("Unit file: {}", plist.display()),
            String::new(),
            "Rendered LaunchAgent:".to_string(),
        ];
        lines.extend(content.lines().map(|l| format!("  {l}")));
        lines.push(String::new());
        lines.push(format!(
            "Would write the plist, bootstrap it into {}, and verify the daemon came up.",
            domain(uid)
        ));
        if let Some(warning) = preflight.dry_run_warning() {
            lines.push(String::new());
            lines.push(warning);
        }
        return Ok(lines);
    }

    // Refuse before writing the plist or touching launchd if `vard run` itself
    // could not start (VRD-58).
    preflight.require_startable()?;

    atomic::write(plist, content.as_bytes())
        .map_err(|e| CmdError::err(format!("writing {}: {e}", plist.display())))?;

    let dom = domain(uid);
    let plist_str = plist.to_string_lossy();
    let plist_str = plist_str.as_ref();

    let out = env
        .runner
        .run("launchctl", &["bootstrap", dom.as_str(), plist_str]);
    if !out.success() {
        // Already bootstrapped (or a stale prior load): unload and retry once so
        // a re-install is idempotent.
        let _ = env
            .runner
            .run("launchctl", &["bootout", service_target(uid).as_str()]);
        let retry = env
            .runner
            .run("launchctl", &["bootstrap", dom.as_str(), plist_str]);
        if !retry.success() {
            return Err(CmdError::err(format!(
                "launchctl bootstrap failed: {}",
                retry.detail()
            )));
        }
    }

    finish_with_verify(
        env,
        vec![
            format!("Wrote LaunchAgent {}", plist.display()),
            format!("Loaded and started {LABEL}."),
        ],
    )
}

/// `vard service uninstall`: unload the service (ignoring not-found) and remove
/// the plist (ignoring missing). Uninstalling when nothing is installed is an
/// idempotent success.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn uninstall(env: &OpEnv, uid: u32, plist: &Path) -> Result<Vec<String>, CmdError> {
    let out = env
        .runner
        .run("launchctl", &["bootout", service_target(uid).as_str()]);
    let was_loaded = out.success();

    let plist_existed = plist.exists();
    if plist_existed {
        fs::remove_file(plist)
            .map_err(|e| CmdError::err(format!("removing {}: {e}", plist.display())))?;
    }

    if !was_loaded && !plist_existed {
        return Ok(vec![
            "The vard service was not installed; nothing to do.".to_string(),
        ]);
    }

    let mut lines = Vec::new();
    if was_loaded {
        lines.push(format!("Unloaded {LABEL}."));
    }
    if plist_existed {
        lines.push(format!("Removed {}", plist.display()));
    }
    Ok(lines)
}

/// `vard service start`: bring the installed service up, verifying it came up.
/// A missing plist advises `vard service install`. Otherwise the real launchd
/// state is probed first (`launchctl print`, parsed by
/// [`parse_launchctl_print`]): a confirmed **Running** service is an exit-0
/// no-op (the documented idempotency); every other outcome — not loaded,
/// throttled, exited, or a probe that could not even run — is recovered by the
/// same unconditional `bootout`-then-`bootstrap` sequence `install` uses, which
/// is safe from every state. Probe failure therefore never yields a refusal
/// (VRD-59).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn start(
    env: &OpEnv,
    uid: u32,
    plist: &Path,
    preflight: &PreflightOutcome,
) -> Result<Vec<String>, CmdError> {
    // Refuse before touching launchd if `vard run` itself could not start.
    preflight.require_startable()?;

    if !plist.exists() {
        return Err(CmdError::err(
            "no vard service is installed — run `vard service install` first",
        ));
    }

    let target = service_target(uid);
    let probe = env.runner.run("launchctl", &["print", target.as_str()]);
    if let LaunchctlPrint::Running = parse_launchctl_print(&probe) {
        return Ok(vec![format!("{LABEL} is already running.")]);
    }

    // Any non-running (or unreadable) state: recover with install's proven
    // bootout → bootstrap sequence, ignoring the bootout result (it fails
    // harmlessly when nothing is loaded).
    let _ = env.runner.run("launchctl", &["bootout", target.as_str()]);
    let dom = domain(uid);
    let plist_str = plist.to_string_lossy();
    let boot = env.runner.run(
        "launchctl",
        &["bootstrap", dom.as_str(), plist_str.as_ref()],
    );
    if !boot.success() {
        return Err(CmdError::err(format!(
            "launchctl could not start the service: {}",
            boot.detail()
        )));
    }

    finish_with_verify(env, vec![format!("Started {LABEL}.")])
}

/// `vard service stop`: unload the service. Stopping an already-stopped service
/// is an idempotent success. The installed plist's `RunAtLoad` re-arms it at the
/// next login — `uninstall` removes it for good.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn stop(env: &OpEnv, uid: u32) -> Result<Vec<String>, CmdError> {
    let out = env
        .runner
        .run("launchctl", &["bootout", service_target(uid).as_str()]);
    if out.success() {
        Ok(vec![
            format!("Stopped {LABEL}."),
            "Its RunAtLoad plist re-arms it at your next login; run `vard service uninstall` to \
             remove it."
                .to_string(),
        ])
    } else {
        Ok(vec!["The vard service was already stopped.".to_string()])
    }
}

/// `vard service restart`: re-exec the daemon (launchd has no reload signal). A
/// missing plist advises `vard service install`. Otherwise run `install`'s
/// proven sequence **unconditionally** — `bootout` (result ignored) →
/// `bootstrap` → liveness verify — which is correct from every state (running,
/// stopped, throttled, not loaded) and sidesteps the loaded-state inference that
/// misfired against a bootstrapped-but-throttled service (VRD-59).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn restart(
    env: &OpEnv,
    uid: u32,
    plist: &Path,
    preflight: &PreflightOutcome,
) -> Result<Vec<String>, CmdError> {
    // Refuse before touching launchd if `vard run` itself could not start.
    preflight.require_startable()?;

    if !plist.exists() {
        return Err(CmdError::err(
            "no vard service is installed — run `vard service install` first",
        ));
    }

    let target = service_target(uid);
    // Bootout first to clear any prior (possibly throttled) load; a not-loaded
    // service boots out harmlessly, so the result is deliberately ignored.
    let _ = env.runner.run("launchctl", &["bootout", target.as_str()]);
    let dom = domain(uid);
    let plist_str = plist.to_string_lossy();
    let boot = env.runner.run(
        "launchctl",
        &["bootstrap", dom.as_str(), plist_str.as_ref()],
    );
    if !boot.success() {
        return Err(CmdError::err(format!(
            "launchctl bootstrap failed: {}",
            boot.detail()
        )));
    }

    finish_with_verify(env, vec![format!("Restarted {LABEL}.")])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::OpEnv;
    use crate::service::tests::{FakeLiveness, FakePrompt, FakeRunner, fail, ok};

    fn env<'a>(
        runner: &'a FakeRunner,
        live: &'a FakeLiveness,
        prompt: &'a FakePrompt,
    ) -> OpEnv<'a> {
        OpEnv {
            runner,
            liveness: live,
            prompt,
        }
    }

    /// A pre-flight that lets the verb proceed — the default for flows that are
    /// not exercising the VRD-58 refusal.
    fn startable() -> PreflightOutcome {
        PreflightOutcome::Startable
    }

    /// A pre-flight that refuses, carrying a recognizable advice message.
    fn refused() -> PreflightOutcome {
        PreflightOutcome::Refused(
            "nothing to watch — add a watch with `vard watch add`".to_string(),
        )
    }

    /// A successful, zero-exit `launchctl print` run with the given stdout — the
    /// shape [`crate::service::run_bounded`] returns for a clean probe.
    fn print_ok(stdout: &str) -> RunOutput {
        RunOutput {
            spawned: true,
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    #[test]
    fn render_plist_is_stable_and_escapes() {
        let got = render_plist("/opt/homebrew/bin/vard");
        let expected = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.dbtlr.vard</string>
	<key>ProgramArguments</key>
	<array>
		<string>/opt/homebrew/bin/vard</string>
		<string>run</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Background</string>
	<key>ThrottleInterval</key>
	<integer>10</integer>
</dict>
</plist>
"#;
        assert_eq!(got, expected);
    }

    #[test]
    fn render_plist_xml_escapes_the_binary_path() {
        let got = render_plist("/Users/a&b/<vard>");
        assert!(got.contains("<string>/Users/a&amp;b/&lt;vard&gt;</string>"));
        assert!(!got.contains("a&b"));
    }

    #[test]
    fn install_writes_plist_bootstraps_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        let runner = FakeRunner::new(vec![ok()]); // bootstrap succeeds
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            501,
            &plist,
            Path::new("/opt/homebrew/bin/vard"),
            false,
            &startable(),
        )
        .unwrap();
        assert!(plist.exists(), "plist should be written");
        assert_eq!(
            runner.calls(),
            vec!["launchctl bootstrap gui/501 ".to_string() + plist.to_str().unwrap()]
        );
        assert!(lines.iter().any(|l| l.contains("daemon is up")));
    }

    #[test]
    fn install_reboots_when_already_bootstrapped() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        // First bootstrap fails (already loaded), bootout ok, second bootstrap ok.
        let runner = FakeRunner::new(vec![fail("service already bootstrapped"), ok(), ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        install(
            &e,
            501,
            &plist,
            Path::new("/usr/local/bin/vard"),
            false,
            &startable(),
        )
        .unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].starts_with("launchctl bootout gui/501/com.dbtlr.vard"));
        assert!(calls[2].starts_with("launchctl bootstrap"));
    }

    #[test]
    fn install_reports_attention_when_daemon_does_not_come_up() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(false); // never comes up
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = install(
            &e,
            501,
            &plist,
            Path::new("/usr/local/bin/vard"),
            false,
            &startable(),
        )
        .unwrap_err();
        assert!(err.message().contains("did not come up"));
    }

    #[test]
    fn install_second_bootstrap_failure_is_operational_error() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        let runner = FakeRunner::new(vec![fail("boom"), ok(), fail("still broken")]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = install(
            &e,
            501,
            &plist,
            Path::new("/usr/local/bin/vard"),
            false,
            &startable(),
        )
        .unwrap_err();
        assert!(err.message().contains("bootstrap failed"));
    }

    #[test]
    fn dry_run_writes_nothing_and_prints_plan() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        let runner = FakeRunner::new(vec![]); // must never run
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            501,
            &plist,
            Path::new("/opt/homebrew/bin/vard"),
            true,
            &startable(),
        )
        .unwrap();
        assert!(!plist.exists(), "dry run must not write the plist");
        assert!(runner.calls().is_empty(), "dry run must not shell out");
        let text = lines.join("\n");
        assert!(text.contains("Dry run"));
        assert!(text.contains("/opt/homebrew/bin/vard"));
        assert!(text.contains("gui/501"));
        assert!(text.contains("<key>Label</key>"));
    }

    #[test]
    fn install_refuses_when_preflight_fails_before_touching_launchd() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        let runner = FakeRunner::new(vec![]); // must never run
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = install(
            &e,
            501,
            &plist,
            Path::new("/opt/homebrew/bin/vard"),
            false,
            &refused(),
        )
        .unwrap_err();
        assert!(err.message().contains("vard watch add"));
        assert!(!plist.exists(), "refusal must not write the plist");
        assert!(
            runner.calls().is_empty(),
            "refusal must not shell out to launchctl"
        );
    }

    #[test]
    fn dry_run_warns_when_preflight_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        let runner = FakeRunner::new(vec![]); // dry run never shells out
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            501,
            &plist,
            Path::new("/opt/homebrew/bin/vard"),
            true,
            &refused(),
        )
        .unwrap();
        assert!(!plist.exists(), "dry run must not write the plist");
        let text = lines.join("\n");
        assert!(text.contains("Dry run"));
        assert!(
            text.contains("WARNING: install would refuse"),
            "dry run must surface the pre-flight warning, got: {text}"
        );
        assert!(text.contains("vard watch add"));
    }

    #[test]
    fn uninstall_when_nothing_installed_is_idempotent_success() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist"); // does not exist
        let runner = FakeRunner::new(vec![fail("Boot-out failed: 3: No such process")]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = uninstall(&e, 501, &plist).unwrap();
        assert!(lines.iter().any(|l| l.contains("nothing to do")));
    }

    #[test]
    fn uninstall_removes_plist_and_unloads() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = uninstall(&e, 501, &plist).unwrap();
        assert!(!plist.exists());
        assert!(lines.iter().any(|l| l.contains("Unloaded")));
        assert!(lines.iter().any(|l| l.contains("Removed")));
    }

    #[test]
    fn start_without_plist_advises_install() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist"); // missing
        let runner = FakeRunner::new(vec![]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = start(&e, 501, &plist, &startable()).unwrap_err();
        assert!(err.message().contains("vard service install"));
    }

    #[test]
    fn start_refuses_when_preflight_fails_before_any_launchctl() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        let runner = FakeRunner::new(vec![]); // must never run
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = start(&e, 501, &plist, &refused()).unwrap_err();
        assert!(err.message().contains("vard watch add"));
        assert!(
            runner.calls().is_empty(),
            "refusal must precede every launchctl call, even the state probe"
        );
    }

    #[test]
    fn start_is_a_no_op_when_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        // The probe reports a running daemon (a `pid = ` line).
        let runner = FakeRunner::new(vec![print_ok(
            "com.dbtlr.vard = {\n\tpid = 4242\n\tstate = running\n}\n",
        )]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = start(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "a running service is probed and left alone");
        assert!(calls[0].starts_with("launchctl print gui/501/com.dbtlr.vard"));
        assert!(lines.iter().any(|l| l.contains("already running")));
    }

    #[test]
    fn start_recovers_from_a_throttled_or_unloaded_state() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        // Probe reports a nonzero last exit (throttled/crash-looping), then the
        // bootout+bootstrap recovery both succeed.
        let runner = FakeRunner::new(vec![
            print_ok("com.dbtlr.vard = {\n\tlast exit code = 2\n\tstate = not running\n}\n"),
            ok(), // bootout
            ok(), // bootstrap
        ]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        start(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].starts_with("launchctl print gui/501/com.dbtlr.vard"));
        assert!(calls[1].starts_with("launchctl bootout gui/501/com.dbtlr.vard"));
        assert!(calls[2].starts_with("launchctl bootstrap gui/501"));
    }

    #[test]
    fn start_recovers_even_when_the_probe_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        // A probe that fails to interpret (here: launchctl print itself errored)
        // must fall through to recovery, never a refusal.
        let runner = FakeRunner::new(vec![
            fail("Could not find service in domain"), // probe -> NotLoaded
            ok(),                                     // bootout
            ok(),                                     // bootstrap
        ]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        start(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[2].starts_with("launchctl bootstrap gui/501"));
    }

    #[test]
    fn stop_already_stopped_is_success() {
        let runner = FakeRunner::new(vec![fail("Boot-out failed: 3: No such process")]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = stop(&e, 501).unwrap();
        assert!(lines.iter().any(|l| l.contains("already stopped")));
    }

    #[test]
    fn stop_when_loaded_notes_relogin_rearm() {
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = stop(&e, 501).unwrap();
        assert!(lines.iter().any(|l| l.contains("re-arms")));
    }

    #[test]
    fn restart_runs_the_unconditional_bootout_bootstrap_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        // From a bootstrapped-but-throttled state, bootout succeeds then
        // bootstrap succeeds — the sequence install proved live.
        let runner = FakeRunner::new(vec![ok(), ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        restart(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], "launchctl bootout gui/501/com.dbtlr.vard");
        assert!(calls[1].starts_with("launchctl bootstrap gui/501"));
    }

    #[test]
    fn restart_bootstraps_even_when_bootout_fails_because_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        // bootout fails harmlessly (nothing loaded); its result is ignored and
        // bootstrap still runs — proving the sequence works from every state.
        let runner = FakeRunner::new(vec![fail("Boot-out failed: 3: No such process"), ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        restart(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].starts_with("launchctl bootstrap gui/501"));
    }

    #[test]
    fn restart_without_plist_advises_install() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist"); // missing
        let runner = FakeRunner::new(vec![]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = restart(&e, 501, &plist, &startable()).unwrap_err();
        assert!(err.message().contains("vard service install"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn restart_refuses_when_preflight_fails_before_any_launchctl() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        let runner = FakeRunner::new(vec![]); // must never run
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = restart(&e, 501, &plist, &refused()).unwrap_err();
        assert!(err.message().contains("vard watch add"));
        assert!(runner.calls().is_empty());
    }

    // --- launchctl print parser (moved from doctor; single source of truth) ---

    #[test]
    fn launchctl_print_running_from_pid_line() {
        let out = print_ok(
            "com.dbtlr.vard = {\n\tactive count = 1\n\tpid = 4242\n\tstate = running\n}\n",
        );
        assert!(matches!(
            parse_launchctl_print(&out),
            LaunchctlPrint::Running
        ));
    }

    #[test]
    fn launchctl_print_nonzero_last_exit_without_pid() {
        let out = print_ok("com.dbtlr.vard = {\n\tlast exit code = 2\n\tstate = not running\n}\n");
        match parse_launchctl_print(&out) {
            LaunchctlPrint::Exited { code } => assert_eq!(code, 2),
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    #[test]
    fn launchctl_print_not_loaded_on_command_failure() {
        let out = RunOutput {
            spawned: true,
            code: Some(113),
            stdout: String::new(),
            stderr: "Could not find service \"com.dbtlr.vard\" in domain for port".to_string(),
            timed_out: false,
        };
        assert!(matches!(
            parse_launchctl_print(&out),
            LaunchctlPrint::NotLoaded
        ));
    }

    #[test]
    fn launchctl_print_missing_binary_is_unparsed_not_not_loaded() {
        // launchctl itself failed to spawn — a probe failure, not evidence the
        // service is unloaded.
        let out = RunOutput {
            spawned: false,
            code: None,
            stdout: String::new(),
            stderr: "No such file or directory (os error 2)".to_string(),
            timed_out: false,
        };
        assert!(matches!(
            parse_launchctl_print(&out),
            LaunchctlPrint::Unparsed(_)
        ));
    }

    #[test]
    fn launchctl_print_timeout_is_unparsed_not_not_loaded() {
        let out = RunOutput {
            spawned: true,
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        };
        assert!(matches!(
            parse_launchctl_print(&out),
            LaunchctlPrint::Unparsed(_)
        ));
    }

    #[test]
    fn launchctl_print_clean_last_exit_with_no_pid_is_unparsed() {
        let out = print_ok("com.dbtlr.vard = {\n\tlast exit code = 0\n\tstate = not running\n}\n");
        assert!(matches!(
            parse_launchctl_print(&out),
            LaunchctlPrint::Unparsed(_)
        ));
    }

    #[test]
    fn launchctl_print_unrecognized_shape_is_unparsed_with_detail() {
        let out = print_ok("something unexpected\n");
        match parse_launchctl_print(&out) {
            LaunchctlPrint::Unparsed(detail) => assert_eq!(detail, "something unexpected"),
            other => panic!("expected Unparsed, got {other:?}"),
        }
    }
}
