//! Linux systemd **user unit** backend for `vard service` (VRD-24).
//!
//! The service is a systemd user unit at
//! `$XDG_CONFIG_HOME/systemd/user/vard.service` (default
//! `~/.config/systemd/user/vard.service`), managed with `systemctl --user`. The
//! unit's `Restart=on-failure` respawns the daemon on a crash, and its
//! `ExecReload=/bin/kill -HUP $MAINPID` lets `systemctl --user reload` push a
//! config reload without a full restart. A user unit stops at logout unless
//! **lingering** is enabled — install handles that consent.
//!
//! [`render_unit`], [`should_prompt`], and [`decide_linger`] are pure and
//! tested on every platform; the operation flows shell out through the injected
//! [`Runner`](super::Runner).

use std::fs;
use std::path::{Path, PathBuf};

use crate::atomic;
use crate::command::CmdError;

use super::{OpEnv, PreflightOutcome, RunOutput};

/// The systemd user unit name.
const UNIT: &str = "vard.service";

/// `$XDG_CONFIG_HOME/systemd/user/vard.service` (default
/// `~/.config/systemd/user/vard.service`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn unit_path() -> Result<PathBuf, CmdError> {
    let base = super::config_base(
        std::env::var_os("XDG_CONFIG_HOME"),
        super::home().as_deref(),
    )
    .ok_or_else(|| {
        CmdError::err(
            "neither XDG_CONFIG_HOME nor an absolute HOME is set; cannot locate the systemd \
                 user unit directory",
        )
    })?;
    Ok(base.join("systemd/user").join(UNIT))
}

/// Quotes an ExecStart binary path for systemd if it contains whitespace
/// (systemd splits the command line on spaces).
fn quote_exec(bin: &str) -> String {
    if bin.chars().any(char::is_whitespace) {
        format!("\"{bin}\"")
    } else {
        bin.to_string()
    }
}

/// Renders the systemd user unit for a daemon exec'd from `bin`. Pure: compiled
/// and golden-tested on every platform. `Restart=on-failure` respawns only on a
/// failure exit; `ExecReload` sends `SIGHUP` for a config reload.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn render_unit(bin: &str) -> String {
    let exec = quote_exec(bin);
    format!(
        "[Unit]
Description=vard — automatic directory snapshots into version control
Documentation=man:vard(1)

[Service]
Type=simple
ExecStart={exec} run
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"
    )
}

/// What to do about lingering after `enable --now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum LingerDecision {
    /// Run `loginctl enable-linger`.
    Enable,
    /// Leave lingering off, saying nothing (an explicit `--no-linger` or a
    /// declined prompt).
    SkipQuiet,
    /// Leave lingering off and print a one-line notice (non-interactive, no
    /// flag).
    SkipNotice,
}

/// Whether the linger consent needs an interactive prompt: no flag either way,
/// and a TTY to ask on. Pure.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn should_prompt(linger: bool, no_linger: bool, is_tty: bool) -> bool {
    !linger && !no_linger && is_tty
}

/// Decides the linger action from the flags, TTY-ness, and the prompt answer (if
/// one was shown). Pure — the decision table is unit-tested exhaustively.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn decide_linger(
    linger: bool,
    no_linger: bool,
    is_tty: bool,
    answer: Option<bool>,
) -> LingerDecision {
    if linger {
        return LingerDecision::Enable;
    }
    if no_linger {
        return LingerDecision::SkipQuiet;
    }
    if is_tty {
        match answer {
            Some(true) => LingerDecision::Enable,
            _ => LingerDecision::SkipQuiet,
        }
    } else {
        LingerDecision::SkipNotice
    }
}

/// `systemctl --user <args>`.
fn systemctl(env: &OpEnv, args: &[&str]) -> RunOutput {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push("--user");
    full.extend_from_slice(args);
    env.runner.run("systemctl", &full)
}

/// Confirms a reachable systemd user session, or fails with the day-one
/// systemd-only guidance. Absent `systemctl` or an unreachable `--user` bus both
/// exit 2.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn ensure_reachable(env: &OpEnv) -> Result<(), CmdError> {
    let out = systemctl(env, &["show-environment"]);
    if !out.spawned {
        return Err(CmdError::err(
            "systemctl was not found; vard service is systemd-only day one — run `vard run` under \
             your own supervisor",
        ));
    }
    if !out.success() {
        return Err(CmdError::err(format!(
            "the systemd user session is unreachable ({}); vard service is systemd-only day one — \
             run `vard run` under your own supervisor",
            out.detail()
        )));
    }
    Ok(())
}

fn verify_failed() -> CmdError {
    CmdError::attention(
        "the unit was installed and the service started, but the daemon did not come up — run \
         `vard run` in the foreground to see why",
    )
}

fn finish_with_verify(env: &OpEnv, mut lines: Vec<String>) -> Result<Vec<String>, CmdError> {
    if env.liveness.wait_until_up() {
        lines.push("The vard daemon is up.".to_string());
        Ok(lines)
    } else {
        Err(verify_failed())
    }
}

/// `vard service install`: write the unit, `daemon-reload`, `enable --now`, run
/// the linger consent flow, and verify the daemon came up. `--dry-run` prints
/// the plan and touches nothing (and needs no reachable session).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn install(
    env: &OpEnv,
    unit: &Path,
    bin: &Path,
    dry_run: bool,
    linger: bool,
    no_linger: bool,
    is_tty: bool,
    preflight: &PreflightOutcome,
) -> Result<Vec<String>, CmdError> {
    let content = render_unit(&bin.to_string_lossy());

    if dry_run {
        let mut lines = vec![
            "Dry run — nothing was written.".to_string(),
            format!("Binary:    {}", bin.display()),
            format!("Unit file: {}", unit.display()),
            String::new(),
            "Rendered systemd user unit:".to_string(),
        ];
        lines.extend(content.lines().map(|l| format!("  {l}")));
        lines.push(String::new());
        lines.push(
            "Would write the unit, `systemctl --user daemon-reload`, `enable --now vard.service`, \
             and verify the daemon came up."
                .to_string(),
        );
        lines.push(format!(
            "Linger: {}",
            linger_dry_summary(linger, no_linger, is_tty)
        ));
        if let Some(warning) = preflight.dry_run_warning() {
            lines.push(String::new());
            lines.push(warning);
        }
        return Ok(lines);
    }

    // Refuse before writing the unit or touching systemd if `vard run` itself
    // could not start (VRD-58). On the Linux path dispatch has already gated
    // this before the reachability probe; this keeps the backend self-contained.
    preflight.require_startable()?;

    atomic::write(unit, content.as_bytes())
        .map_err(|e| CmdError::err(format!("writing {}: {e}", unit.display())))?;

    let reload = systemctl(env, &["daemon-reload"]);
    if !reload.success() {
        return Err(CmdError::err(format!(
            "systemctl --user daemon-reload failed: {}",
            reload.detail()
        )));
    }

    let enable = systemctl(env, &["enable", "--now", UNIT]);
    if !enable.success() {
        return Err(CmdError::err(format!(
            "systemctl --user enable --now {UNIT} failed: {}",
            enable.detail()
        )));
    }

    let mut lines = vec![
        format!("Wrote systemd user unit {}", unit.display()),
        format!("Enabled and started {UNIT}."),
    ];

    // Linger consent.
    let answer = if should_prompt(linger, no_linger, is_tty) {
        Some(env.prompt.confirm(
            "User services stop at logout. Enable lingering so vard survives logout? [y/N]",
        ))
    } else {
        None
    };
    match decide_linger(linger, no_linger, is_tty, answer) {
        LingerDecision::Enable => {
            let out = env.runner.run("loginctl", &["enable-linger"]);
            if out.success() {
                lines.push("Enabled lingering (the service survives logout).".to_string());
            } else {
                // Linger is a convenience, not the install's success condition:
                // surface the failure but do not fail the whole install.
                lines.push(format!(
                    "Could not enable lingering ({}); the service will stop at logout. Run \
                     `loginctl enable-linger` to fix it.",
                    out.detail()
                ));
            }
        }
        LingerDecision::SkipQuiet => {}
        LingerDecision::SkipNotice => {
            lines.push(
                "Lingering is off, so the service stops at logout; run with `--linger` (or \
                 `loginctl enable-linger`) to keep it running."
                    .to_string(),
            );
        }
    }

    finish_with_verify(env, lines)
}

/// A one-line description of what install's linger step would do, for `--dry-run`.
fn linger_dry_summary(linger: bool, no_linger: bool, is_tty: bool) -> &'static str {
    if linger {
        "would enable lingering (loginctl enable-linger)"
    } else if no_linger {
        "would leave lingering off"
    } else if is_tty {
        "would prompt to enable lingering"
    } else {
        "would leave lingering off (non-interactive)"
    }
}

/// `vard service uninstall`: `disable --now` (ignoring not-loaded), remove the
/// unit (ignoring missing), and `daemon-reload`. Idempotent when nothing is
/// installed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn uninstall(env: &OpEnv, unit: &Path) -> Result<Vec<String>, CmdError> {
    let disable = systemctl(env, &["disable", "--now", UNIT]);
    let was_enabled = disable.success();

    let unit_existed = unit.exists();
    if unit_existed {
        fs::remove_file(unit)
            .map_err(|e| CmdError::err(format!("removing {}: {e}", unit.display())))?;
    }

    // Reload so systemd forgets the removed unit; ignore its result (nothing to
    // reload is fine).
    let _ = systemctl(env, &["daemon-reload"]);

    if !was_enabled && !unit_existed {
        return Ok(vec![
            "The vard service was not installed; nothing to do.".to_string(),
        ]);
    }

    let mut lines = Vec::new();
    if was_enabled {
        lines.push(format!("Disabled and stopped {UNIT}."));
    }
    if unit_existed {
        lines.push(format!("Removed {}", unit.display()));
    }
    Ok(lines)
}

/// `vard service start`: `start` the unit and verify. A missing unit advises
/// `vard service install`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn start(
    env: &OpEnv,
    unit: &Path,
    preflight: &PreflightOutcome,
) -> Result<Vec<String>, CmdError> {
    preflight.require_startable()?;
    if !unit.exists() {
        return Err(CmdError::err(
            "no vard service is installed — run `vard service install` first",
        ));
    }
    let out = systemctl(env, &["start", UNIT]);
    if !out.success() {
        return Err(CmdError::err(format!(
            "systemctl --user start {UNIT} failed: {}",
            out.detail()
        )));
    }
    finish_with_verify(env, vec![format!("Started {UNIT}.")])
}

/// `vard service stop`: `stop` the unit. Idempotent — stopping an already-stopped
/// unit is a success (`systemctl stop` is itself idempotent). The unit stays
/// enabled, so it starts again at the next login until `uninstall`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn stop(env: &OpEnv) -> Result<Vec<String>, CmdError> {
    let out = systemctl(env, &["stop", UNIT]);
    if !out.success() {
        return Err(CmdError::err(format!(
            "systemctl --user stop {UNIT} failed: {}",
            out.detail()
        )));
    }
    Ok(vec![
        format!("Stopped {UNIT}."),
        "The unit stays enabled and starts again at your next login; run `vard service \
         uninstall` to remove it."
            .to_string(),
    ])
}

/// `vard service restart`: refuse up front if `vard run` itself could not start
/// (the VRD-58 pre-flight), then run [`restart_unchecked`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn restart(env: &OpEnv, preflight: &PreflightOutcome) -> Result<Vec<String>, CmdError> {
    preflight.require_startable()?;
    restart_unchecked(env)
}

/// The `restart` + verify mechanics *without* the VRD-58 config pre-flight.
/// `systemctl --user restart` is state-agnostic (it starts a stopped unit and
/// restarts a running one), so no state inference is needed here — the VRD-59
/// launchd fix has no systemd parallel. `vard service restart` calls this behind
/// its pre-flight gate; `vard self-update`'s post-swap restart
/// ([`crate::service::restart_installed`]) calls it directly, because per ADR
/// 0017 the updater verifies and reports and must never surface a watch-state
/// refusal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn restart_unchecked(env: &OpEnv) -> Result<Vec<String>, CmdError> {
    let out = systemctl(env, &["restart", UNIT]);
    if !out.success() {
        return Err(CmdError::err(format!(
            "systemctl --user restart {UNIT} failed: {}",
            out.detail()
        )));
    }
    finish_with_verify(env, vec![format!("Restarted {UNIT}.")])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::OpEnv;
    use crate::service::tests::{
        AlwaysWait, FakeLiveness, FakePrompt, FakeRunner, fail, not_spawned, ok,
    };

    fn env<'a>(
        runner: &'a FakeRunner,
        live: &'a FakeLiveness,
        prompt: &'a FakePrompt,
    ) -> OpEnv<'a> {
        OpEnv {
            runner,
            liveness: live,
            prompt,
            // systemd flows never bootout→settle; the waiter is never consulted.
            settle: &AlwaysWait,
        }
    }

    /// A pre-flight that lets the verb proceed — the default for flows not
    /// exercising the VRD-58 refusal.
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
    fn render_unit_is_stable() {
        let got = render_unit("/usr/bin/vard");
        let expected = "[Unit]
Description=vard — automatic directory snapshots into version control
Documentation=man:vard(1)

[Service]
Type=simple
ExecStart=/usr/bin/vard run
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
";
        assert_eq!(got, expected);
    }

    #[test]
    fn render_unit_quotes_a_path_with_spaces() {
        let got = render_unit("/home/a b/bin/vard");
        assert!(got.contains("ExecStart=\"/home/a b/bin/vard\" run"));
    }

    #[test]
    fn linger_decision_table() {
        use LingerDecision::*;
        // --linger always enables.
        assert_eq!(decide_linger(true, false, false, None), Enable);
        assert_eq!(decide_linger(true, false, true, Some(false)), Enable);
        // --no-linger always skips quietly.
        assert_eq!(decide_linger(false, true, true, Some(true)), SkipQuiet);
        // Interactive, yes.
        assert_eq!(decide_linger(false, false, true, Some(true)), Enable);
        // Interactive, no.
        assert_eq!(decide_linger(false, false, true, Some(false)), SkipQuiet);
        // Non-interactive, no flag → notice.
        assert_eq!(decide_linger(false, false, false, None), SkipNotice);
    }

    #[test]
    fn should_prompt_only_when_no_flag_and_tty() {
        assert!(should_prompt(false, false, true));
        assert!(!should_prompt(false, false, false));
        assert!(!should_prompt(true, false, true));
        assert!(!should_prompt(false, true, true));
    }

    #[test]
    fn ensure_reachable_reports_missing_systemctl() {
        let runner = FakeRunner::new(vec![not_spawned()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);
        let err = ensure_reachable(&e).unwrap_err();
        assert!(err.message().contains("systemd-only"));
    }

    #[test]
    fn ensure_reachable_reports_unreachable_bus() {
        let runner = FakeRunner::new(vec![fail("Failed to connect to bus")]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);
        let err = ensure_reachable(&e).unwrap_err();
        assert!(err.message().contains("unreachable"));
    }

    #[test]
    fn install_writes_reloads_enables_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        // daemon-reload ok, enable --now ok.
        let runner = FakeRunner::new(vec![ok(), ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            false,
            false,
            true,
            false,
            &startable(),
        )
        .unwrap();
        assert!(unit.exists());
        let calls = runner.calls();
        assert_eq!(calls[0], "systemctl --user daemon-reload");
        assert_eq!(calls[1], "systemctl --user enable --now vard.service");
        assert!(lines.iter().any(|l| l.contains("daemon is up")));
    }

    #[test]
    fn install_with_linger_runs_loginctl() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        // daemon-reload, enable, loginctl enable-linger.
        let runner = FakeRunner::new(vec![ok(), ok(), ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            false,
            true,
            false,
            false,
            &startable(),
        )
        .unwrap();
        let calls = runner.calls();
        assert_eq!(calls[2], "loginctl enable-linger");
        assert!(lines.iter().any(|l| l.contains("Enabled lingering")));
    }

    #[test]
    fn install_prompt_yes_enables_linger() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![ok(), ok(), ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(true); // user answers yes
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            false,
            false,
            false,
            true,
            &startable(),
        )
        .unwrap();
        assert_eq!(runner.calls()[2], "loginctl enable-linger");
        assert!(lines.iter().any(|l| l.contains("Enabled lingering")));
    }

    #[test]
    fn install_non_tty_no_flag_prints_notice_and_skips_linger() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![ok(), ok()]); // no loginctl call
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            false,
            false,
            false,
            false,
            &startable(),
        )
        .unwrap();
        assert_eq!(runner.calls().len(), 2, "loginctl must not run");
        assert!(lines.iter().any(|l| l.contains("Lingering is off")));
    }

    #[test]
    fn install_enable_failure_is_operational_error() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![ok(), fail("Unit vard.service failed")]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            false,
            false,
            true,
            false,
            &startable(),
        )
        .unwrap_err();
        assert!(err.message().contains("enable --now"));
    }

    #[test]
    fn install_attention_when_daemon_does_not_come_up() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![ok(), ok(), ok()]); // reload, enable, loginctl
        let live = FakeLiveness(false);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            false,
            true,
            false,
            false,
            &startable(),
        )
        .unwrap_err();
        assert!(err.message().contains("did not come up"));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            true,
            false,
            false,
            true,
            &startable(),
        )
        .unwrap();
        assert!(!unit.exists());
        assert!(runner.calls().is_empty());
        let text = lines.join("\n");
        assert!(text.contains("Dry run"));
        assert!(text.contains("ExecStart=/usr/bin/vard run"));
        assert!(text.contains("would prompt to enable lingering"));
    }

    #[test]
    fn uninstall_when_nothing_installed_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service"); // missing
        // disable fails (not loaded), daemon-reload ok.
        let runner = FakeRunner::new(vec![fail("Failed to disable: not loaded"), ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = uninstall(&e, &unit).unwrap();
        assert!(lines.iter().any(|l| l.contains("nothing to do")));
    }

    #[test]
    fn uninstall_disables_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        fs::write(&unit, "x").unwrap();
        let runner = FakeRunner::new(vec![ok(), ok()]); // disable, daemon-reload
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = uninstall(&e, &unit).unwrap();
        assert!(!unit.exists());
        assert!(lines.iter().any(|l| l.contains("Disabled")));
        assert!(lines.iter().any(|l| l.contains("Removed")));
    }

    #[test]
    fn start_without_unit_advises_install() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);
        let err = start(&e, &unit, &startable()).unwrap_err();
        assert!(err.message().contains("vard service install"));
    }

    #[test]
    fn start_runs_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        fs::write(&unit, "x").unwrap();
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        start(&e, &unit, &startable()).unwrap();
        assert_eq!(runner.calls()[0], "systemctl --user start vard.service");
    }

    #[test]
    fn start_refuses_when_preflight_fails_before_any_systemctl() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        fs::write(&unit, "x").unwrap();
        let runner = FakeRunner::new(vec![]); // must never run
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = start(&e, &unit, &refused()).unwrap_err();
        assert!(err.message().contains("vard watch add"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn stop_runs_stop_and_notes_still_enabled() {
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = stop(&e).unwrap();
        assert_eq!(runner.calls()[0], "systemctl --user stop vard.service");
        assert!(lines.iter().any(|l| l.contains("stays enabled")));
    }

    #[test]
    fn restart_runs_restart_and_verifies() {
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        restart(&e, &startable()).unwrap();
        assert_eq!(runner.calls()[0], "systemctl --user restart vard.service");
    }

    #[test]
    fn restart_attention_when_daemon_does_not_come_up() {
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(false);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = restart(&e, &startable()).unwrap_err();
        assert!(err.message().contains("did not come up"));
    }

    #[test]
    fn restart_unchecked_skips_preflight_and_never_judges_watch_state() {
        // The post-swap reuse seam (`vard self-update` → restart_installed →
        // restart_unchecked) restarts *below* the VRD-58 pre-flight gate: it
        // takes no PreflightOutcome, so no config is consulted and no watch-state
        // refusal can surface (ADR 0017). Just `systemctl --user restart` + verify.
        let runner = FakeRunner::new(vec![ok()]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = restart_unchecked(&e).unwrap();
        assert_eq!(runner.calls()[0], "systemctl --user restart vard.service");
        assert!(lines.iter().any(|l| l.contains("Restarted")));
        assert!(
            !lines.iter().any(|l| l.to_lowercase().contains("watch")),
            "post-swap restart must never judge watch state: {lines:?}"
        );
    }

    #[test]
    fn restart_refuses_when_preflight_fails_before_any_systemctl() {
        let runner = FakeRunner::new(vec![]); // must never run
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = restart(&e, &refused()).unwrap_err();
        assert!(err.message().contains("vard watch add"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn install_refuses_when_preflight_fails_before_touching_systemd() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![]); // must never run
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let err = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            false,
            false,
            true,
            false,
            &refused(),
        )
        .unwrap_err();
        assert!(err.message().contains("vard watch add"));
        assert!(!unit.exists(), "refusal must not write the unit");
        assert!(runner.calls().is_empty(), "refusal must not shell out");
    }

    #[test]
    fn dry_run_warns_when_preflight_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("vard.service");
        let runner = FakeRunner::new(vec![]);
        let live = FakeLiveness(true);
        let prompt = FakePrompt(false);
        let e = env(&runner, &live, &prompt);

        let lines = install(
            &e,
            &unit,
            Path::new("/usr/bin/vard"),
            true,
            false,
            false,
            true,
            &refused(),
        )
        .unwrap();
        assert!(!unit.exists());
        let text = lines.join("\n");
        assert!(text.contains("Dry run"));
        assert!(
            text.contains("WARNING: install would refuse"),
            "dry run must surface the pre-flight warning, got: {text}"
        );
        assert!(text.contains("vard watch add"));
    }
}
