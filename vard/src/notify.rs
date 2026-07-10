//! `vard notify` — the shell-prompt health hook (spec §8).
//!
//! The constraint *is* the feature: notify runs on every shell prompt, so it
//! must be sub-millisecond and must never talk to the daemon or run git —
//! anything slower gets ripped out of a `.zshrc` within a week. It therefore
//! does exactly two cheap things: a non-blocking probe of the instance
//! [`flock`](crate::instance) (microseconds) to learn whether a daemon is
//! running, and a read of the small [`health`] file the daemon
//! keeps current. No config parse, no repository access, no background chatter.
//!
//! # What it prints
//!
//! - Every watch healthy ⇒ nothing, exit 0.
//! - One line per troubled watch ⇒ exit 1.
//! - The daemon not running is itself one reported line (it *replaces* any stale
//!   per-watch entries — a stopped daemon's leftover file is not current) ⇒
//!   exit 1.
//! - An operational failure (unreadable/corrupt health file while the daemon
//!   runs, an unresolvable state dir) ⇒ exit 2.
//!
//! These exit codes make it usable from a prompt, tmux, starship, or cron: a
//! caller can branch on the status without parsing the text.
//!
//! # Format
//!
//! Human lines are the default *regardless of destination* — a prompt hook runs
//! notify in a command substitution (piped, not a TTY) yet wants the human
//! line, so notify does not apply the global records-on-TTY / JSON-when-piped
//! auto-detect. `--format json` (or `jsonl`) opts into a stable machine shape:
//! an array of problem objects, `[]` when healthy, so a status-bar program gets
//! a contract instead of silence.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use crate::cli::{ColorWhen, OutputFormat};
use crate::health::{self, HealthProblem};
use crate::instance::{self, DaemonProbe};
use crate::output::glyphs::{self, Glyph};
use crate::output::palette::{self, Palette};
use crate::output::primitives::sanitize_controls;
use crate::output::record::{self, Record, RecordField, format_duration};
use crate::paths;

/// Entry point for `vard notify`.
pub(crate) fn run(color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    match run_inner(color, format) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("vard: {message}");
            // 2 = operational error (distinct from 1 = problems reported).
            ExitCode::from(2)
        }
    }
}

fn run_inner(color: ColorWhen, format: Option<OutputFormat>) -> Result<u8, String> {
    let lock_file = paths::lock_file().map_err(|e| e.to_string())?;
    let health_file = paths::health_file().map_err(|e| e.to_string())?;

    let is_tty = io::stdout().is_terminal();
    let palette = palette::resolve_with_tty(color, is_tty);
    // Human lines are the default even when piped (the prompt-hook case); only
    // an explicit flag selects a machine shape.
    let out_format = format.unwrap_or(OutputFormat::Records);

    let problems = collect(&lock_file, &health_file)?;
    render(&problems, out_format, &palette)?;

    // 0 healthy, 1 anything to report (including a stopped daemon).
    Ok(if problems.is_empty() { 0 } else { 1 })
}

/// One reportable line: a troubled watch, or the synthetic daemon-not-running
/// entry (which carries no `watch`).
struct NotifyProblem {
    /// The watch's name, or `None` for the daemon-not-running line.
    watch: Option<String>,
    /// The status token: a `WatchState` spelling, or `daemon-not-running`.
    state: String,
    /// A human summary of the problem.
    summary: String,
    /// Unix seconds the state was entered (for a watch) or the health file was
    /// last written (for the daemon-not-running line); `None` when unknown.
    since: Option<u64>,
}

impl NotifyProblem {
    fn from_health(p: HealthProblem) -> NotifyProblem {
        NotifyProblem {
            watch: Some(p.watch),
            state: p.state,
            summary: p.summary,
            since: Some(p.since),
        }
    }

    /// The daemon-not-running line. `last_write` is the health file's
    /// `written_at`, when a leftover file exists, so the line can say how stale
    /// the last real status was.
    fn daemon_not_running(last_write: Option<u64>) -> NotifyProblem {
        NotifyProblem {
            watch: None,
            state: "daemon-not-running".to_string(),
            summary: "the vard daemon is not running; your watches are not being snapshotted"
                .to_string(),
            since: last_write,
        }
    }
}

/// Gathers the reportable problems, choosing the source by the lock probe: a
/// running daemon's live health file, or the single daemon-not-running line.
fn collect(
    lock_file: &std::path::Path,
    health_file: &std::path::Path,
) -> Result<Vec<NotifyProblem>, String> {
    match instance::probe_daemon(lock_file).map_err(|e| format!("probing the daemon lock: {e}"))? {
        // No daemon: the not-running line REPLACES any leftover per-watch
        // problems (a stopped daemon's file is not current). Peek the file only
        // to report how stale it is, ignoring any read/parse error — staleness
        // is a nicety, never a reason to fail this path.
        DaemonProbe::NotRunning => {
            let last_write = health::read(health_file)
                .ok()
                .flatten()
                .map(|doc| doc.written_at);
            Ok(vec![NotifyProblem::daemon_not_running(last_write)])
        }
        // A daemon is running: its health file is authoritative.
        DaemonProbe::Running => match health::read(health_file)? {
            // No file yet (daemon just started, nothing troubled): healthy.
            None => Ok(Vec::new()),
            Some(doc) => {
                if doc.version != health::VERSION {
                    return Err(format!(
                        "health file schema version {} is newer than this vard understands \
                         (expected {}); upgrade vard",
                        doc.version,
                        health::VERSION
                    ));
                }
                Ok(doc
                    .problems
                    .into_iter()
                    .map(NotifyProblem::from_health)
                    .collect())
            }
        },
    }
}

/// Renders the problems in the resolved format. Records is the human,
/// one-line-per-problem form; JSON/JSONL emit the stable machine shape (an
/// empty array when healthy).
fn render(
    problems: &[NotifyProblem],
    format: OutputFormat,
    palette: &Palette,
) -> Result<(), String> {
    let now = health::now_secs();
    let mut w = io::stdout().lock();
    let res = match format {
        OutputFormat::Records => {
            // Silent when healthy: the loop simply writes nothing.
            for problem in problems {
                if let Err(e) = writeln!(w, "{}", human_line(problem, palette, now)) {
                    return finish(Err(e));
                }
            }
            Ok(())
        }
        OutputFormat::Json => record::render_json(&mut w, &records(problems, now)),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, &records(problems, now)),
    };
    finish(res)
}

/// The human line for one problem. Every file-derived string is passed through
/// [`sanitize_controls`] so a crafted watch name or summary cannot inject
/// terminal escapes into a prompt.
fn human_line(problem: &NotifyProblem, palette: &Palette, now: u64) -> String {
    let glyph = glyphs::render(Glyph::Warn, glyphs::use_ascii());
    let glyph = format!(
        "{}{glyph}{}",
        palette.warning.render(),
        palette.warning.render_reset()
    );
    match &problem.watch {
        Some(watch) => {
            let watch = sanitize_controls(watch);
            let state = sanitize_controls(&problem.state);
            let summary = sanitize_controls(&problem.summary);
            let elapsed = problem
                .since
                .map(|since| format_duration(Duration::from_secs(now.saturating_sub(since))))
                .unwrap_or_else(|| "an unknown time".to_string());
            format!("{glyph} vard: '{watch}' {state} (for {elapsed}) — {summary}")
        }
        None => {
            let staleness = problem
                .since
                .map(|written| {
                    format!(
                        " (last health update {} ago)",
                        format_duration(Duration::from_secs(now.saturating_sub(written)))
                    )
                })
                .unwrap_or_default();
            format!("{glyph} vard: daemon not running — start it with `vard run`{staleness}")
        }
    }
}

/// Builds the machine-form records: one object per problem with a stable field
/// set. `elapsed_seconds` is derived so a consumer need not know the current
/// time; both timestamps are bare JSON numbers (or `null`).
fn records(problems: &[NotifyProblem], now: u64) -> Vec<Record> {
    problems
        .iter()
        .map(|problem| {
            let elapsed = problem.since.map(|since| now.saturating_sub(since) as i64);
            Record {
                header: None,
                fields: vec![
                    RecordField::opt("watch", problem.watch.clone()),
                    RecordField::str("state", problem.state.as_str()),
                    RecordField::str("summary", problem.summary.as_str()),
                    RecordField::opt_int("since", problem.since.map(|s| s as i64)),
                    RecordField::opt_int("elapsed_seconds", elapsed),
                ],
            }
        })
        .collect()
}

/// Folds a write result, treating a broken pipe (a prompt that read one line
/// and closed) as success rather than an error.
fn finish(res: io::Result<()>) -> Result<(), String> {
    match res {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(format!("writing output: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch_problem(state: &str, summary: &str, since: u64) -> NotifyProblem {
        NotifyProblem {
            watch: Some("vault".to_string()),
            state: state.to_string(),
            summary: summary.to_string(),
            since: Some(since),
        }
    }

    #[test]
    fn human_line_for_a_watch_carries_state_elapsed_and_summary() {
        let p = watch_problem("conflicted", "a sync conflict is blocking progress", 1000);
        // now = since + 2h.
        let line = human_line(&p, &Palette::off(), 1000 + 7200);
        assert_eq!(
            line,
            "⚠ vard: 'vault' conflicted (for 2h) — a sync conflict is blocking progress"
        );
    }

    #[test]
    fn human_line_for_a_stopped_daemon_names_the_fix_and_staleness() {
        let p = NotifyProblem::daemon_not_running(Some(1000));
        let line = human_line(&p, &Palette::off(), 1000 + 300);
        assert!(line.contains("daemon not running"), "got: {line}");
        assert!(line.contains("vard run"), "must name the fix: {line}");
        assert!(line.contains("last health update 5m ago"), "got: {line}");
    }

    #[test]
    fn human_line_for_a_stopped_daemon_without_a_file_omits_staleness() {
        let p = NotifyProblem::daemon_not_running(None);
        let line = human_line(&p, &Palette::off(), 5000);
        assert!(line.contains("daemon not running"), "got: {line}");
        assert!(!line.contains("last health update"), "got: {line}");
    }

    #[test]
    fn human_line_sanitizes_control_characters_from_file_fields() {
        // A crafted watch name / summary must not inject terminal escapes.
        let p = watch_problem("attention", "evil\x1b[31mred\x07", 10);
        let line = human_line(
            &NotifyProblem {
                watch: Some("na\x1bme".to_string()),
                ..p
            },
            &Palette::off(),
            20,
        );
        assert!(!line.contains('\x1b'), "raw ESC must not survive: {line:?}");
        assert!(!line.contains('\x07'), "raw BEL must not survive: {line:?}");
        assert!(
            line.contains('\u{fffd}'),
            "expected replacement char: {line:?}"
        );
    }

    #[test]
    fn ascii_fallback_glyph_when_requested() {
        // SAFETY: single-threaded test; sets and restores the env var.
        unsafe { std::env::set_var("VARD_ASCII", "1") };
        let p = watch_problem("paused", "watch is paused", 0);
        let line = human_line(&p, &Palette::off(), 60);
        unsafe { std::env::remove_var("VARD_ASCII") };
        assert!(line.starts_with("[warn]"), "expected ASCII glyph: {line}");
    }

    #[test]
    fn json_records_carry_the_stable_field_set_with_numeric_times() {
        let problems = vec![watch_problem("attention", "needs attention", 1000)];
        let recs = records(&problems, 1000 + 60);
        let mut out = Vec::new();
        record::render_json(&mut out, &recs).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""watch":"vault""#), "got: {s}");
        assert!(s.contains(r#""state":"attention""#), "got: {s}");
        assert!(s.contains(r#""since":1000"#), "got: {s}");
        assert!(s.contains(r#""elapsed_seconds":60"#), "got: {s}");
    }

    #[test]
    fn empty_json_is_an_empty_array_not_silence() {
        let mut out = Vec::new();
        record::render_json(&mut out, &records(&[], 0)).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[]\n");
    }

    #[test]
    fn daemon_not_running_line_has_null_watch_in_json() {
        let problems = vec![NotifyProblem::daemon_not_running(None)];
        let recs = records(&problems, 5);
        let mut out = Vec::new();
        record::render_json(&mut out, &recs).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""watch":null"#), "got: {s}");
        assert!(s.contains(r#""state":"daemon-not-running""#), "got: {s}");
    }
}
