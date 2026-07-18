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

use super::{OpEnv, PreflightOutcome};

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

/// `vard service start`: load the service, or kick an already-loaded one, then
/// verify. A missing plist advises `vard service install`. Pre-flights the
/// daemon config first (VRD-58): a config `vard run` could not start refuses
/// before any launchctl call.
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

    let dom = domain(uid);
    let plist_str = plist.to_string_lossy();
    let plist_str = plist_str.as_ref();

    let out = env
        .runner
        .run("launchctl", &["bootstrap", dom.as_str(), plist_str]);
    if !out.success() {
        // Already loaded: kick it so a running-but-idle service is (re)started.
        let kick = env
            .runner
            .run("launchctl", &["kickstart", service_target(uid).as_str()]);
        if !kick.success() {
            return Err(CmdError::err(format!(
                "launchctl could not start the service: {}",
                kick.detail()
            )));
        }
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

/// `vard service restart`: kickstart with `-k` (kill and restart), loading the
/// service first if it is not yet loaded, then verify. This is how macOS
/// re-execs the daemon — launchd has no reload signal. Pre-flights the daemon
/// config first (VRD-58): a config `vard run` could not start refuses before any
/// launchctl call.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn restart(
    env: &OpEnv,
    uid: u32,
    plist: &Path,
    preflight: &PreflightOutcome,
) -> Result<Vec<String>, CmdError> {
    // Refuse before touching launchd if `vard run` itself could not start.
    preflight.require_startable()?;

    let target = service_target(uid);
    let out = env
        .runner
        .run("launchctl", &["kickstart", "-k", target.as_str()]);
    if !out.success() {
        // Not loaded: bootstrap it (the plist must exist to load).
        if !plist.exists() {
            return Err(CmdError::err(
                "no vard service is installed — run `vard service install` first",
            ));
        }
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
    fn start_kicks_when_already_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        let runner = FakeRunner::new(vec![fail("already bootstrapped"), ok()]); // bootstrap fails, kickstart ok
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        start(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert!(calls[1].starts_with("launchctl kickstart gui/501/com.dbtlr.vard"));
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
    fn restart_kickstarts_k_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        restart(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert_eq!(calls[0], "launchctl kickstart -k gui/501/com.dbtlr.vard");
    }

    #[test]
    fn restart_bootstraps_when_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.dbtlr.vard.plist");
        fs::write(&plist, "x").unwrap();
        let runner = FakeRunner::new(vec![fail("No such process"), ok()]); // kickstart fails, bootstrap ok
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        restart(&e, 501, &plist, &startable()).unwrap();
        let calls = runner.calls();
        assert!(calls[1].starts_with("launchctl bootstrap gui/501"));
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
}
