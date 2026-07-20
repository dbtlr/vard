//! Text + machine renderers for `vard self-update`, driven by the global
//! `--format`: a human block on a terminal, a stable one-object JSON/JSONL array
//! when piped (matching every other list surface — one record).

use std::io::{self, Write};

use crate::cli::OutputFormat;
use crate::command::{self, CmdResult, OutCtx};
use crate::output::palette::Palette;
use crate::output::primitives::{self, sanitize_controls};
use crate::output::record::{self, Record, RecordField};
use crate::self_update::resolve::Action;
use crate::self_update::{PostSwap, SelfUpdateReport};

/// Renders `report` in the resolved format.
pub(crate) fn render(out: &OutCtx, report: &SelfUpdateReport) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => render_human(&mut w, &out.palette, report),
        OutputFormat::Json => record::render_json(&mut w, &records(report)),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, &records(report)),
    };
    command::finish_write(res)
}

/// The machine record: a single fixed-shape object, so the JSON contract carries
/// stable keys across every action (the asset fields render `null` on a no-op).
/// The phase-2 outcome fields (`restart_attempted`, `verify_outcome`,
/// `running_version`) are appended after the phase-1 keys, which stay stable.
fn records(report: &SelfUpdateReport) -> Vec<Record> {
    // The post-swap outcome, projected to the JSON contract. `restart_attempted`
    // is true only when a loaded unit was actually restarted; `verify_outcome` is
    // a stable token, `null` when nothing was swapped (dry run / no-op).
    let (restart_attempted, verify_outcome) = match &report.post_swap {
        Some(PostSwap::Verified) => (true, Some("verified")),
        Some(PostSwap::Failed { .. }) => (true, Some("failed")),
        Some(PostSwap::NoUnit) => (false, Some("no_unit")),
        None => (false, None),
    };
    vec![Record {
        header: None,
        fields: vec![
            RecordField::str("action", report.action.token()),
            RecordField::bool("update_available", report.update_available),
            RecordField::str("current_version", &report.current_version),
            RecordField::str("latest_version", &report.latest_version),
            RecordField::str("target_version", &report.target_version),
            RecordField::str("target_triple", &report.target_triple),
            RecordField::str("install_path", &report.install_path),
            RecordField::opt("asset_url", report.asset_url.clone()),
            RecordField::opt("asset_sha256", report.asset_sha256.clone()),
            RecordField::bool("dry_run", report.dry_run),
            RecordField::bool("unit_installed", report.unit_installed),
            RecordField::bool("restart_attempted", restart_attempted),
            RecordField::opt("verify_outcome", verify_outcome.map(str::to_string)),
            RecordField::opt("running_version", report.running_version.clone()),
        ],
    }]
}

/// The human form: a headline, the resolved plan as aligned fields, the outcome
/// line, and — when a swap actually happened — the daemon-restart caveat.
fn render_human(out: &mut dyn Write, p: &Palette, report: &SelfUpdateReport) -> io::Result<()> {
    primitives::status_headline(out, p, "vard self-update")?;

    field(out, "current", &report.current_version)?;
    field(out, "latest", &report.latest_version)?;
    field(out, "target", &report.target_version)?;
    field(out, "triple", &report.target_triple)?;
    field(out, "install path", &report.install_path)?;
    if let Some(url) = &report.asset_url {
        field(out, "asset url", url)?;
    }
    if let Some(sha) = &report.asset_sha256 {
        field(out, "asset sha256", sha)?;
    }

    let target = sanitize_controls(&report.target_version);
    let current = sanitize_controls(&report.current_version);
    match report.action {
        Action::WouldUpdate => {
            writeln!(
                out,
                "Dry run — would update {current} → {target} (nothing downloaded)"
            )?;
            // The plan's post-swap step, so a dry run says what a real run would do.
            if report.unit_installed {
                writeln!(
                    out,
                    "Then it would restart the vard service and verify {target} is running."
                )?;
            } else {
                writeln!(
                    out,
                    "No vard service is loaded, so you would restart your daemon \
                     (`vard run`) to pick up {target}."
                )?;
            }
        }
        Action::WouldNoOp => writeln!(out, "Dry run — already on {current}; up to date")?,
        Action::NoOp => writeln!(out, "Already on {current}; up to date")?,
        Action::Updated => {
            writeln!(out, "Updated vard {current} → {target}")?;
            render_post_swap(out, report, &current, &target)?;
        }
    }
    Ok(())
}

/// The post-swap tail on a real update: the confirmed restart, the
/// no-service-loaded hint, or the failed-verify recovery gesture. Never blames
/// watch state (ADR 0017): a failed verify asserts only that the restarted
/// daemon's heartbeat did not confirm the new version, and points at the exact
/// pinned-downgrade command plus the installer fallback.
fn render_post_swap(
    out: &mut dyn Write,
    report: &SelfUpdateReport,
    current: &str,
    target: &str,
) -> io::Result<()> {
    match &report.post_swap {
        Some(PostSwap::Verified) => {
            writeln!(out, "Restarted the vard service; {target} is now running.")
        }
        Some(PostSwap::NoUnit) | None => writeln!(
            out,
            "No vard service is loaded — the binary is swapped; restart your daemon \
             (`vard run`) to pick up {target}."
        ),
        Some(PostSwap::Failed { reason }) => {
            let reason = sanitize_controls(reason);
            writeln!(
                out,
                "The binary was swapped, but the restart could not be verified: {reason}."
            )?;
            writeln!(
                out,
                "The new binary is in place; the daemon may still be on {current}."
            )?;
            writeln!(out, "To go back to {current}:")?;
            writeln!(out, "    vard self-update --version {current}")?;
            writeln!(out, "or reinstall via the installer:")?;
            writeln!(
                out,
                "    curl --proto '=https' --tlsv1.2 -LsSf \
                 https://github.com/dbtlr/vard/releases/latest/download/vard-installer.sh | sh"
            )
        }
    }
}

/// One aligned `  {label}:  {value}` row, with the value sanitized so a crafted
/// manifest value cannot inject terminal escapes.
fn field(out: &mut dyn Write, label: &str, value: &str) -> io::Result<()> {
    writeln!(out, "  {label:<13} {}", sanitize_controls(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SelfUpdateReport {
        SelfUpdateReport {
            update_available: true,
            current_version: "0.1.0".to_string(),
            latest_version: "0.2.0".to_string(),
            target_version: "0.2.0".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            install_path: "/opt/homebrew/bin/vard".to_string(),
            asset_url: Some(
                "https://example/download/v0.2.0/vard-aarch64-apple-darwin.tar.xz".to_string(),
            ),
            asset_sha256: Some("abc123".to_string()),
            dry_run: true,
            action: Action::WouldUpdate,
            unit_installed: true,
            post_swap: None,
            running_version: None,
        }
    }

    #[test]
    fn json_carries_stable_keys() {
        let mut buf = Vec::new();
        record::render_json(&mut buf, &records(&sample())).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with('['), "json is an array: {s}");
        assert!(s.contains(r#""action":"would_update""#));
        assert!(s.contains(r#""update_available":true"#));
        assert!(s.contains(r#""current_version":"0.1.0""#));
        assert!(s.contains(r#""target_version":"0.2.0""#));
        assert!(s.contains(r#""asset_sha256":"abc123""#));
        assert!(s.contains(r#""dry_run":true"#));
    }

    #[test]
    fn json_nulls_asset_fields_on_no_op() {
        let mut report = sample();
        report.action = Action::NoOp;
        report.update_available = false;
        report.dry_run = false;
        report.asset_url = None;
        report.asset_sha256 = None;
        let mut buf = Vec::new();
        record::render_json(&mut buf, &records(&report)).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains(r#""asset_url":null"#), "got: {s}");
        assert!(s.contains(r#""asset_sha256":null"#), "got: {s}");
    }

    #[test]
    fn human_dry_run_shows_versions_and_marker() {
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &sample()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("0.1.0"), "current missing: {s}");
        assert!(s.contains("0.2.0"), "target missing: {s}");
        assert!(s.contains("Dry run"), "dry-run marker missing: {s}");
    }

    #[test]
    fn json_carries_the_post_swap_outcome_fields() {
        let mut report = sample();
        report.action = Action::Updated;
        report.dry_run = false;
        report.post_swap = Some(PostSwap::Verified);
        report.running_version = Some("0.2.0".to_string());
        let mut buf = Vec::new();
        record::render_json(&mut buf, &records(&report)).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains(r#""restart_attempted":true"#), "got: {s}");
        assert!(s.contains(r#""verify_outcome":"verified""#), "got: {s}");
        assert!(s.contains(r#""running_version":"0.2.0""#), "got: {s}");
        assert!(s.contains(r#""unit_installed":true"#), "got: {s}");
    }

    #[test]
    fn json_nulls_the_post_swap_fields_on_a_dry_run() {
        // Nothing was swapped, so there is no restart or verify outcome.
        let s = {
            let mut buf = Vec::new();
            record::render_json(&mut buf, &records(&sample())).unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert!(s.contains(r#""restart_attempted":false"#), "got: {s}");
        assert!(s.contains(r#""verify_outcome":null"#), "got: {s}");
        assert!(s.contains(r#""running_version":null"#), "got: {s}");
    }

    #[test]
    fn human_dry_run_names_the_post_swap_plan() {
        // Unit loaded → restart+verify plan; no unit → manual restart plan.
        let mut with_unit = sample();
        with_unit.unit_installed = true;
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &with_unit).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("restart the vard service"), "got: {s}");

        let mut no_unit = sample();
        no_unit.unit_installed = false;
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &no_unit).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("No vard service is loaded"), "got: {s}");
    }

    #[test]
    fn human_verified_names_the_running_version() {
        let mut report = sample();
        report.action = Action::Updated;
        report.dry_run = false;
        report.post_swap = Some(PostSwap::Verified);
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &report).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Updated vard 0.1.0 → 0.2.0"), "got: {s}");
        assert!(s.contains("0.2.0 is now running"), "got: {s}");
    }

    #[test]
    fn human_no_unit_says_swapped_restart_your_daemon() {
        let mut report = sample();
        report.action = Action::Updated;
        report.dry_run = false;
        report.unit_installed = false;
        report.post_swap = Some(PostSwap::NoUnit);
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &report).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("restart your daemon"), "got: {s}");
    }

    #[test]
    fn human_failed_verify_prints_the_recovery_gesture_without_blaming_watch_state() {
        let mut report = sample();
        report.action = Action::Updated;
        report.dry_run = false;
        report.post_swap = Some(PostSwap::Failed {
            reason: "the daemon's health file did not report 0.2.0 running within 5s".to_string(),
        });
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &report).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // (a) the swap itself succeeded.
        assert!(s.contains("Updated vard 0.1.0 → 0.2.0"), "got: {s}");
        assert!(s.contains("binary was swapped"), "got: {s}");
        // (c) the exact recovery gesture: pinned downgrade + installer fallback.
        assert!(
            s.contains("vard self-update --version 0.1.0"),
            "recovery pin missing: {s}"
        );
        assert!(
            s.contains("vard-installer.sh"),
            "installer fallback missing: {s}"
        );
        // (b) it never blames watch state.
        assert!(!s.contains("watch"), "must not mention watch state: {s}");
    }

    #[test]
    fn human_sanitizes_control_characters_in_values() {
        let mut report = sample();
        report.target_version = "0.2.0\x1b[31m".to_string();
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &report).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'), "raw ESC must not survive: {s:?}");
    }
}
