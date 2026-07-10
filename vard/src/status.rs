//! `vard status [name|path]` — daemon liveness and per-watch state (spec §8).
//!
//! The on-demand companion to [`notify`](crate::notify): where notify is a
//! silent-when-healthy prompt hook, status always reports — the daemon's
//! liveness on the first line, then every configured watch's current state,
//! healthy and paused ones included. It is strictly read-only: it probes the
//! single-instance lock, reads the health file, and reads the config's watch
//! list, and it never takes a lock, runs git, or mutates anything.
//!
//! # State projection
//!
//! The watch list comes from [`Config::resolve_all`] (config order, config-paused
//! watches included) and is joined with the health projection
//! ([`health::collect`]): a watch present in the health problems shows its health
//! state; a config-paused watch shows `paused`; an unpaused watch shows `ok` when
//! a daemon is monitoring it, or `unknown` when nothing is (the daemon is not
//! running or still starting). A health problem whose watch is no longer in the
//! config (removed or renamed before a reload) is not dropped — it is appended as
//! its own row, marked as not configured, so it still counts.
//!
//! # Staleness
//!
//! Like notify, status treats a running daemon whose health file has gone stale
//! (older than [`health::STALE_AFTER_SECS`]) as stale and says so on the daemon
//! line. Unlike notify — which suppresses per-watch detail on the hot path —
//! status still shows the last-known per-watch states beneath that warning, since
//! a diagnostic view is more useful surfacing the last truth than hiding it; the
//! stale daemon line is the honest caveat.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use anstyle::Style;

use crate::cli::{ColorWhen, OutputFormat, StatusArgs};
use crate::command::{self, CmdError, CmdResult, OutCtx};
use crate::config::{Config, ResolvedWatch};
use crate::health::{self, HealthProblem, HealthReport};
use crate::output::glyphs::{self, Glyph};
use crate::output::palette::Palette;
use crate::output::primitives::clean_line;
use crate::output::record::{self, Record, RecordField, format_duration};
use crate::paths;
use crate::watch::select;

/// Entry point for `vard status`. Returns the health-derived exit code (0
/// healthy, 1 attention), or exit 2 on an operational error (an unresolvable
/// state dir, an unsupported health schema, an invalid config, or an
/// unresolvable selector).
pub(crate) fn run(args: StatusArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    match run_inner(args, color, format) {
        Ok(code) => ExitCode::from(code),
        // An operational error prints `vard: <message>` and exits 2 through the
        // shared command-outcome layer.
        Err(err) => command::finish(Err(err)),
    }
}

fn run_inner(
    args: StatusArgs,
    color: ColorWhen,
    format: Option<OutputFormat>,
) -> Result<u8, CmdError> {
    let lock_file = paths::lock_file().map_err(|e| CmdError::err(e.to_string()))?;
    let health_file = paths::health_file().map_err(|e| CmdError::err(e.to_string()))?;
    let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
    let out = OutCtx::resolve(color, format);
    report(
        &out,
        &lock_file,
        &health_file,
        &config_file,
        args.target.as_deref(),
    )
}

/// The injectable core: resolve the health picture and config from explicit
/// paths, join them, render, and return the exit code. Split from [`run_inner`]
/// (which resolves the production XDG paths) so a test can drive it against a
/// tempdir.
fn report(
    out: &OutCtx,
    lock_file: &std::path::Path,
    health_file: &std::path::Path,
    config_file: &std::path::Path,
    target: Option<&str>,
) -> Result<u8, CmdError> {
    let ascii = glyphs::use_ascii();
    let now = health::now_secs();

    // Probe the daemon and read its health projection. An unsupported health
    // schema is the one operational error `collect` surfaces (exit 2).
    let report = health::collect(lock_file, health_file).map_err(CmdError::err)?;
    let daemon = DaemonStatus::from_report(&report, now);

    // The watch list is the config's, in config order; a missing config file is
    // simply an empty list. A present-but-invalid config is an operational error.
    let config = Config::load_optional(config_file).map_err(|e| CmdError::err(e.to_string()))?;
    let watches = match &config {
        Some(cfg) => cfg
            .resolve_all()
            .map_err(|e| CmdError::err(e.to_string()))?,
        None => Vec::new(),
    };
    // Every configured watch name, lowercased, so an orphaned problem can be told
    // from a case-differing match (the same identity the config/select layers use).
    let configured_names: HashSet<String> = watches
        .iter()
        .map(|w| w.spec.name().to_lowercase())
        .collect();

    // A selector narrows to one watch (and reports not-found / ambiguous as an
    // operational error); with no selector every configured watch is reported.
    let reported: Vec<ResolvedWatch> = match target {
        Some(sel) => {
            let cfg = config
                .as_ref()
                .ok_or_else(|| CmdError::err(format!("no watch named or rooted at {sel:?}")))?;
            let index = select::select_watch(cfg, sel).map_err(|e| CmdError::err(e.to_string()))?;
            // `select_watch` indexes `config.watches`; `resolve_all` preserves
            // that order, so the same index selects the resolved watch.
            vec![
                watches
                    .into_iter()
                    .nth(index)
                    .expect("selector index is within the resolved watch list"),
            ]
        }
        None => watches,
    };

    // Join: overlay the health problems onto the reported watches and, for the
    // whole-fleet view, append any orphaned problems. Orphans are only meaningful
    // without a selector — a selector already narrows to one named config watch.
    let rows = join_rows(&report, &reported, &configured_names, target.is_none());

    // Exit code: daemon-level attention always folds in; the per-watch component
    // is only the reported watches (a selector narrows it). Paused, ok, and
    // unknown are not attention.
    let mut worst = 0u8;
    if daemon.attention {
        worst = worst.max(1);
    }
    for row in &rows {
        if row.is_problem() {
            worst = worst.max(1);
        }
    }

    render(out, &daemon, &rows, now, ascii)?;
    Ok(worst)
}

/// Overlays the health problems onto the reported config watches, then — for the
/// whole-fleet view (`include_orphans`) — appends a row for each health problem
/// whose watch is not in the config, so a removed/renamed watch's problem is
/// still surfaced and still counts toward the exit code.
fn join_rows(
    report: &HealthReport,
    reported: &[ResolvedWatch],
    configured_names: &HashSet<String>,
    include_orphans: bool,
) -> Vec<WatchRow> {
    let problems = problems_map(report);
    // A running daemon (even under a stale file) is monitoring; a starting or
    // stopped daemon is not, so an unpaused watch is `unknown`, not `ok`.
    let monitoring = matches!(report, HealthReport::Running { .. });
    let mut rows: Vec<WatchRow> = reported
        .iter()
        .map(|rw| WatchRow::project(rw, &problems, monitoring))
        .collect();

    if include_orphans && let HealthReport::Running { problems, .. } = report {
        for p in problems {
            if !configured_names.contains(&p.watch.to_lowercase()) {
                rows.push(WatchRow::orphan(p));
            }
        }
    }
    rows
}

/// Indexes the health problems by their watch name, lowercased on both insert
/// and lookup so the join is case-insensitive — matching the config/select
/// duplicate-detection identity. Empty unless a daemon is running.
fn problems_map(report: &HealthReport) -> HashMap<String, &HealthProblem> {
    match report {
        HealthReport::Running { problems, .. } => problems
            .iter()
            .map(|p| (p.watch.to_lowercase(), p))
            .collect(),
        HealthReport::Starting | HealthReport::NotRunning { .. } => HashMap::new(),
    }
}

/// The daemon-liveness line: its display state, a human summary, an optional
/// timestamp anchor, and whether it counts as attention for the exit code.
struct DaemonStatus {
    /// `running`, `not-running`, `starting`, or `stale`.
    state: &'static str,
    /// Human summary shown after `daemon:` on the first line.
    summary: String,
    /// The timestamp anchor (health `written_at`, or a leftover file mtime), if
    /// any, for the machine `since` / `elapsed_seconds` fields.
    since: Option<u64>,
    /// Whether the daemon condition needs attention (folds 1 into the exit code).
    attention: bool,
}

impl DaemonStatus {
    fn from_report(report: &HealthReport, now: u64) -> DaemonStatus {
        match report {
            HealthReport::Running { written_at, .. } => {
                let age = now.saturating_sub(*written_at);
                if age > health::STALE_AFTER_SECS {
                    DaemonStatus {
                        state: "stale",
                        summary: format!(
                            "running, but health data is stale ({}) — the daemon may be stuck \
                             or unable to write",
                            format_duration(Duration::from_secs(age))
                        ),
                        since: Some(*written_at),
                        attention: true,
                    }
                } else {
                    DaemonStatus {
                        state: "running",
                        summary: "running".to_string(),
                        since: Some(*written_at),
                        attention: false,
                    }
                }
            }
            HealthReport::Starting => DaemonStatus {
                state: "starting",
                summary: "starting or stopping; health not yet available".to_string(),
                since: None,
                attention: true,
            },
            HealthReport::NotRunning { last_write } => DaemonStatus {
                state: "not-running",
                summary: "not running — start it with `vard run`".to_string(),
                since: *last_write,
                attention: true,
            },
        }
    }
}

/// One reported watch's projected status.
struct WatchRow {
    /// The watch's stable name.
    name: String,
    /// The state token: `ok`, `paused`, `unknown`, or a health-vocabulary word.
    state: String,
    /// The machine classifier for a problem state, else `None`.
    kind: Option<String>,
    /// The human summary for a problem state, else `None`.
    summary: Option<String>,
    /// Unix seconds the problem state was entered, else `None`.
    since: Option<u64>,
}

impl WatchRow {
    /// Joins one config watch with the health projection: a health problem wins,
    /// then a config pause, then `ok` when a daemon is monitoring, else `unknown`
    /// (nothing is watching it).
    fn project(
        rw: &ResolvedWatch,
        problems: &HashMap<String, &HealthProblem>,
        monitoring: bool,
    ) -> WatchRow {
        let name = rw.spec.name();
        if let Some(p) = problems.get(&name.to_lowercase()) {
            WatchRow::from_problem(name.to_string(), p)
        } else if rw.paused {
            WatchRow::plain(name.to_string(), "paused")
        } else if monitoring {
            WatchRow::plain(name.to_string(), "ok")
        } else {
            WatchRow::plain(name.to_string(), "unknown")
        }
    }

    /// A row for a health problem whose watch is no longer in the config, marked
    /// so a reader can tell it apart from a configured watch's problem.
    fn orphan(p: &HealthProblem) -> WatchRow {
        WatchRow {
            name: p.watch.clone(),
            state: p.state.clone(),
            kind: Some(p.kind.clone()),
            summary: Some(format!("{} (watch not in the current config)", p.summary)),
            since: Some(p.since),
        }
    }

    /// A row carrying a health problem's fields.
    fn from_problem(name: String, p: &HealthProblem) -> WatchRow {
        WatchRow {
            name,
            state: p.state.clone(),
            kind: Some(p.kind.clone()),
            summary: Some(p.summary.clone()),
            since: Some(p.since),
        }
    }

    /// A plain state row (`ok` / `paused` / `unknown`): no problem detail.
    fn plain(name: String, state: &str) -> WatchRow {
        WatchRow {
            name,
            state: state.to_string(),
            kind: None,
            summary: None,
            since: None,
        }
    }

    /// Whether the row counts toward the attention exit code. `ok`, a deliberate
    /// `paused`, and a neutral `unknown` do not; every health-vocabulary state
    /// does.
    fn is_problem(&self) -> bool {
        !matches!(self.state.as_str(), "ok" | "paused" | "unknown")
    }
}

/// Renders status in the resolved format: human lines on a terminal, a stable
/// JSON/JSONL array (the daemon carries a null watch name) when piped. JSON/JSONL
/// and broken-pipe handling route through the shared command emit layer.
fn render(
    out: &OutCtx,
    daemon: &DaemonStatus,
    rows: &[WatchRow],
    now: u64,
    ascii: bool,
) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => render_human(&mut w, daemon, rows, &out.palette, now, ascii),
        OutputFormat::Json => record::render_json(&mut w, &records(daemon, rows, now)),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, &records(daemon, rows, now)),
    };
    command::finish_write(res)
}

/// The human form: the daemon line, then one line per watch (or a note when no
/// watches are configured).
fn render_human(
    w: &mut dyn Write,
    daemon: &DaemonStatus,
    rows: &[WatchRow],
    palette: &Palette,
    now: u64,
    ascii: bool,
) -> io::Result<()> {
    let daemon_style = if daemon.attention {
        &palette.warning
    } else {
        &palette.success
    };
    let daemon_glyph = if daemon.attention {
        Glyph::Warn
    } else {
        Glyph::Pass
    };
    writeln!(
        w,
        "{} daemon: {}",
        paint(glyphs::render(daemon_glyph, ascii), daemon_style),
        clean_line(&daemon.summary)
    )?;

    if rows.is_empty() {
        return writeln!(w, "{}", paint("no watches configured", &palette.dim));
    }

    for row in rows {
        writeln!(w, "{}", human_line(row, palette, now, ascii))?;
    }
    Ok(())
}

/// One watch's human line: a state-colored glyph, the name and state, plus the
/// elapsed time and summary for a problem state.
fn human_line(row: &WatchRow, palette: &Palette, now: u64, ascii: bool) -> String {
    let (style, glyph) = match row.state.as_str() {
        "ok" => (&palette.success, Glyph::Pass),
        // A deliberate pause and an unmonitored `unknown` are both neutral.
        "paused" | "unknown" => (&palette.dim, Glyph::Sep),
        _ => (&palette.warning, Glyph::Warn),
    };
    let mut line = format!(
        "{} {}: {}",
        paint(glyphs::render(glyph, ascii), style),
        clean_line(&row.name),
        clean_line(&row.state)
    );
    if let Some(since) = row.since {
        let elapsed = format_duration(Duration::from_secs(now.saturating_sub(since)));
        line.push_str(&format!(" (for {elapsed})"));
    }
    if let Some(summary) = &row.summary {
        line.push_str(&format!(" — {}", clean_line(summary)));
    }
    line
}

/// The machine records: the daemon row first (null watch name, `daemon: true`),
/// then one per reported watch. Every row carries the same stable field set, so
/// the human and JSON forms can never drift in *which* fields they report.
fn records(daemon: &DaemonStatus, rows: &[WatchRow], now: u64) -> Vec<Record> {
    let mut recs = Vec::with_capacity(rows.len() + 1);
    recs.push(daemon_record(daemon, now));
    recs.extend(rows.iter().map(|row| watch_record(row, now)));
    recs
}

fn daemon_record(daemon: &DaemonStatus, now: u64) -> Record {
    row_record(
        None,
        daemon.state,
        // The daemon row is not a watch: its `kind` is null and a dedicated
        // boolean flags it, so `kind` stays the health-classifier vocabulary the
        // watch rows use.
        None,
        Some(daemon.summary.as_str()),
        daemon.since,
        now,
        true,
    )
}

fn watch_record(row: &WatchRow, now: u64) -> Record {
    row_record(
        Some(row.name.as_str()),
        row.state.as_str(),
        row.kind.as_deref(),
        row.summary.as_deref(),
        row.since,
        now,
        false,
    )
}

/// Builds one fixed-shape record. `elapsed_seconds` is derived so a consumer
/// need not know the current time; timestamps are bare numbers (or null). The
/// `daemon` boolean distinguishes the liveness row from a watch row.
#[allow(clippy::too_many_arguments)]
fn row_record(
    name: Option<&str>,
    state: &str,
    kind: Option<&str>,
    summary: Option<&str>,
    since: Option<u64>,
    now: u64,
    is_daemon: bool,
) -> Record {
    let elapsed = since.map(|s| now.saturating_sub(s) as i64);
    Record {
        header: None,
        fields: vec![
            RecordField::opt("name", name.map(str::to_string)),
            RecordField::str("state", state),
            RecordField::opt("kind", kind.map(str::to_string)),
            RecordField::opt("summary", summary.map(str::to_string)),
            RecordField::opt_int("since", since.map(|s| s as i64)),
            RecordField::opt_int("elapsed_seconds", elapsed),
            RecordField::bool("daemon", is_daemon),
        ],
    }
}

/// Wraps `text` in a style's SGR codes (a no-op when color is off).
fn paint(text: &str, style: &Style) -> String {
    format!("{}{text}{}", style.render(), style.render_reset())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(watch: &str, state: &str, since: u64) -> HealthProblem {
        HealthProblem {
            watch: watch.to_string(),
            state: state.to_string(),
            kind: state.to_string(),
            summary: format!("{state} summary"),
            since,
        }
    }

    fn resolved(name: &str, paused: bool) -> ResolvedWatch {
        let spec = vard_core::WatchSpec::builder(name, std::path::Path::new("/tmp/x"))
            .build()
            .unwrap();
        ResolvedWatch { spec, paused }
    }

    fn map_of(problems: &[HealthProblem]) -> HashMap<String, &HealthProblem> {
        problems
            .iter()
            .map(|p| (p.watch.to_lowercase(), p))
            .collect()
    }

    #[test]
    fn project_prefers_a_health_problem_over_paused_and_ok() {
        let p = problem("vault", "conflicted", 100);
        let map = map_of(std::slice::from_ref(&p));
        let row = WatchRow::project(&resolved("vault", true), &map, true);
        assert_eq!(row.state, "conflicted");
        assert_eq!(row.kind.as_deref(), Some("conflicted"));
        assert!(row.is_problem());
    }

    #[test]
    fn project_joins_a_problem_case_insensitively() {
        // Health names the watch "Vault"; the config names it "vault".
        let p = problem("Vault", "conflicted", 100);
        let map = map_of(std::slice::from_ref(&p));
        let row = WatchRow::project(&resolved("vault", false), &map, true);
        assert_eq!(row.state, "conflicted", "a case difference must still join");
    }

    #[test]
    fn project_shows_paused_when_no_problem() {
        let map: HashMap<String, &HealthProblem> = HashMap::new();
        let row = WatchRow::project(&resolved("notes", true), &map, true);
        assert_eq!(row.state, "paused");
        assert!(!row.is_problem(), "a deliberate pause is not attention");
    }

    #[test]
    fn project_shows_ok_when_healthy_and_monitored() {
        let map: HashMap<String, &HealthProblem> = HashMap::new();
        let row = WatchRow::project(&resolved("work", false), &map, true);
        assert_eq!(row.state, "ok");
        assert!(!row.is_problem());
    }

    #[test]
    fn project_shows_unknown_when_nothing_is_monitoring() {
        // Daemon not running / starting: an unpaused watch is `unknown`, not `ok`.
        let map: HashMap<String, &HealthProblem> = HashMap::new();
        let row = WatchRow::project(&resolved("work", false), &map, /* monitoring */ false);
        assert_eq!(row.state, "unknown");
        assert!(!row.is_problem(), "unknown is neutral, not attention");
        // A config-paused watch still shows paused even when nothing monitors it.
        let paused = WatchRow::project(&resolved("nap", true), &map, false);
        assert_eq!(paused.state, "paused");
    }

    #[test]
    fn join_appends_orphaned_problems_and_folds_them_into_attention() {
        // A problem whose watch is not in the config must not be dropped.
        let report = HealthReport::Running {
            problems: vec![problem("ghost", "conflicted", 100)],
            written_at: 1000,
        };
        let configured: HashSet<String> = ["kept".to_string()].into_iter().collect();
        let rows = join_rows(&report, &[resolved("kept", false)], &configured, true);
        assert_eq!(rows.len(), 2, "the kept watch plus the orphan");
        let orphan = rows.iter().find(|r| r.name == "ghost").expect("orphan row");
        assert_eq!(orphan.state, "conflicted");
        assert!(
            orphan
                .summary
                .as_deref()
                .unwrap()
                .contains("not in the current config"),
            "the orphan must be marked: {:?}",
            orphan.summary
        );
        assert!(
            orphan.is_problem(),
            "an orphan problem folds into the exit code"
        );
    }

    #[test]
    fn join_skips_orphans_when_a_selector_is_in_play() {
        let report = HealthReport::Running {
            problems: vec![problem("ghost", "conflicted", 100)],
            written_at: 1000,
        };
        let configured: HashSet<String> = ["kept".to_string()].into_iter().collect();
        let rows = join_rows(&report, &[resolved("kept", false)], &configured, false);
        assert_eq!(rows.len(), 1, "no orphans appended under a selector");
        assert_eq!(rows[0].name, "kept");
    }

    #[test]
    fn daemon_running_fresh_is_not_attention() {
        let report = HealthReport::Running {
            problems: vec![],
            written_at: 1000,
        };
        let d = DaemonStatus::from_report(&report, 1000 + 5);
        assert_eq!(d.state, "running");
        assert!(!d.attention);
    }

    #[test]
    fn daemon_running_stale_is_attention() {
        let report = HealthReport::Running {
            problems: vec![],
            written_at: 1000,
        };
        let d = DaemonStatus::from_report(&report, 1000 + health::STALE_AFTER_SECS + 1);
        assert_eq!(d.state, "stale");
        assert!(d.attention);
        assert!(d.summary.contains("stale"), "got: {}", d.summary);
    }

    #[test]
    fn daemon_not_running_is_attention_and_names_the_fix() {
        let d = DaemonStatus::from_report(&HealthReport::NotRunning { last_write: None }, 5);
        assert_eq!(d.state, "not-running");
        assert!(d.attention);
        assert!(d.summary.contains("vard run"), "got: {}", d.summary);
    }

    #[test]
    fn daemon_starting_is_attention() {
        let d = DaemonStatus::from_report(&HealthReport::Starting, 5);
        assert_eq!(d.state, "starting");
        assert!(d.attention);
    }

    #[test]
    fn human_line_for_a_problem_carries_elapsed_and_summary() {
        let row = WatchRow {
            name: "vault".to_string(),
            state: "conflicted".to_string(),
            kind: Some("conflicted".to_string()),
            summary: Some("a sync conflict is blocking progress".to_string()),
            since: Some(1000),
        };
        let line = human_line(&row, &Palette::off(), 1000 + 7200, false);
        assert!(line.contains("vault: conflicted"), "got: {line}");
        assert!(line.contains("(for 2h)"), "got: {line}");
        assert!(line.contains("blocking progress"), "got: {line}");
    }

    #[test]
    fn human_line_for_ok_is_terse() {
        let row = WatchRow::plain("notes".to_string(), "ok");
        let line = human_line(&row, &Palette::off(), 100, false);
        assert!(line.ends_with("notes: ok"), "got: {line}");
    }

    #[test]
    fn human_line_sanitizes_control_characters() {
        let row = WatchRow {
            name: "na\x1bme".to_string(),
            state: "attention".to_string(),
            kind: Some("attention".to_string()),
            summary: Some("evil\x1b[31m\x07".to_string()),
            since: Some(10),
        };
        let line = human_line(&row, &Palette::off(), 20, false);
        assert!(!line.contains('\x1b'), "raw ESC must not survive: {line:?}");
        assert!(!line.contains('\x07'), "raw BEL must not survive: {line:?}");
    }

    #[test]
    fn machine_records_lead_with_the_daemon_row_and_stable_fields() {
        let daemon = DaemonStatus::from_report(&HealthReport::NotRunning { last_write: None }, 5);
        let rows = vec![WatchRow::plain("vault".to_string(), "ok")];
        let recs = records(&daemon, &rows, 5);
        let mut out = Vec::new();
        record::render_json(&mut out, &recs).unwrap();
        let s = String::from_utf8(out).unwrap();
        // Daemon row: null watch name, null kind, a `daemon: true` flag, its state.
        assert!(s.contains(r#""name":null"#), "got: {s}");
        assert!(s.contains(r#""kind":null"#), "got: {s}");
        assert!(s.contains(r#""state":"not-running""#), "got: {s}");
        assert!(s.contains(r#""daemon":true"#), "got: {s}");
        // Watch row: real name, ok state, a `daemon: false` flag.
        assert!(s.contains(r#""name":"vault""#), "got: {s}");
        assert!(s.contains(r#""state":"ok""#), "got: {s}");
        assert!(s.contains(r#""daemon":false"#), "got: {s}");
    }

    #[test]
    fn empty_watch_list_still_reports_the_daemon() {
        let daemon = DaemonStatus::from_report(&HealthReport::Starting, 5);
        let recs = records(&daemon, &[], 5);
        assert_eq!(recs.len(), 1, "just the daemon row");
    }
}
