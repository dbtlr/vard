//! Text + machine renderers for `vard self-update`, driven by the global
//! `--format`: a human block on a terminal, a stable one-object JSON/JSONL array
//! when piped (matching every other list surface — one record).

use std::io::{self, Write};

use crate::cli::OutputFormat;
use crate::command::{self, CmdResult, OutCtx};
use crate::output::palette::Palette;
use crate::output::primitives::{self, sanitize_controls};
use crate::output::record::{self, Record, RecordField};
use crate::self_update::SelfUpdateReport;
use crate::self_update::resolve::Action;

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
fn records(report: &SelfUpdateReport) -> Vec<Record> {
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
        Action::WouldUpdate => writeln!(
            out,
            "Dry run — would update {current} → {target} (nothing downloaded)"
        )?,
        Action::WouldNoOp => writeln!(out, "Dry run — already on {current}; up to date")?,
        Action::NoOp => writeln!(out, "Already on {current}; up to date")?,
        Action::Updated => {
            writeln!(out, "Updated vard {current} → {target}")?;
            writeln!(
                out,
                "A running daemon keeps the old binary until it is restarted — \
                 run `vard service restart` (or restart `vard run`) to pick up {target}."
            )?;
        }
    }
    Ok(())
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
    fn human_updated_names_the_restart_caveat() {
        let mut report = sample();
        report.action = Action::Updated;
        report.dry_run = false;
        let mut buf = Vec::new();
        render_human(&mut buf, &Palette::off(), &report).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Updated vard 0.1.0 → 0.2.0"), "got: {s}");
        assert!(s.contains("restart"), "restart caveat missing: {s}");
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
