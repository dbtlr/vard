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
//! The probe + read + version-check is shared with `vard status` (VRD-17) and
//! lives in [`health::collect`]; this module owns only the presentation.
//!
//! # What it prints
//!
//! - Every watch healthy ⇒ nothing, exit 0.
//! - One line per troubled watch ⇒ exit 1.
//! - The daemon not running is itself one reported line (it *replaces* any stale
//!   per-watch entries) ⇒ exit 1.
//! - The daemon running but its health document not yet readable — the startup
//!   or shutdown window — is an honest "starting or stopping" line ⇒ exit 1,
//!   never a silent all-clear.
//! - A running daemon whose document has gone stale (older than
//!   [`health::STALE_AFTER_SECS`]) ⇒ a "health data is stale" line, exit 1,
//!   rather than trusting a document a wedged daemon stopped refreshing.
//! - An operational failure (an unsupported health schema version, an
//!   unresolvable state dir) ⇒ exit 2.
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
use crate::command;
use crate::health::{self, HealthProblem, HealthReport};
use crate::output::glyphs::{self, Glyph};
use crate::output::palette::{self, Palette};
use crate::output::primitives::clean_line;
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
    // Resolve the state directory once (rather than through two separate XDG
    // resolutions) and derive both files the hot path touches.
    let state_dir = paths::state_dir().map_err(|e| e.to_string())?;
    let lock_file = state_dir.join("vard.lock");
    let health_file = state_dir.join("health");

    let is_tty = io::stdout().is_terminal();
    let palette = palette::resolve_with_tty(color, is_tty);
    let ascii = glyphs::use_ascii();
    // Human lines are the default even when piped (the prompt-hook case); only
    // an explicit flag selects a machine shape.
    let out_format = format.unwrap_or(OutputFormat::Records);

    let problems = collect(&lock_file, &health_file)?;
    render(&problems, out_format, &palette, ascii)?;

    // 0 healthy, 1 anything to report (a troubled watch, a stopped/starting/
    // stale daemon).
    Ok(if problems.is_empty() { 0 } else { 1 })
}

/// One reportable line: a troubled watch, or a synthetic daemon-level line.
enum NotifyProblem {
    /// A troubled watch, from the health document.
    Watch {
        /// The watch's name.
        watch: String,
        /// The status token (a health-vocabulary spelling).
        state: String,
        /// The stable machine classifier.
        kind: String,
        /// The human summary with action guidance.
        summary: String,
        /// Unix seconds the watch entered the state.
        since: u64,
    },
    /// No daemon is running. `last_write` is the leftover health file's mtime,
    /// when present, for a staleness suffix.
    DaemonNotRunning {
        /// The leftover health file's mtime (unix seconds), if any.
        last_write: Option<u64>,
    },
    /// A daemon is running but its health document is not yet readable — the
    /// startup window (before the first write) or the shutdown window (after the
    /// clear, before the lock releases).
    Starting,
    /// A running daemon's document has gone stale; `written_at` is its age
    /// anchor.
    Stale {
        /// When the daemon last wrote the document (unix seconds).
        written_at: u64,
    },
}

/// Gathers the reportable problems from the shared [`health::collect`] picture,
/// applying notify's own staleness policy on top of a running daemon's doc.
fn collect(
    lock_file: &std::path::Path,
    health_file: &std::path::Path,
) -> Result<Vec<NotifyProblem>, String> {
    let now = health::now_secs();
    match health::collect(lock_file, health_file)? {
        HealthReport::NotRunning { last_write } => {
            Ok(vec![NotifyProblem::DaemonNotRunning { last_write }])
        }
        HealthReport::Starting => Ok(vec![NotifyProblem::Starting]),
        HealthReport::Running {
            problems,
            written_at,
        } => {
            // A document the daemon stopped refreshing is not trustworthy: report
            // staleness instead of a possibly-frozen problem set.
            if now.saturating_sub(written_at) > health::STALE_AFTER_SECS {
                return Ok(vec![NotifyProblem::Stale { written_at }]);
            }
            Ok(problems
                .into_iter()
                .map(NotifyProblem::from_health)
                .collect())
        }
    }
}

impl NotifyProblem {
    fn from_health(p: HealthProblem) -> NotifyProblem {
        NotifyProblem::Watch {
            watch: p.watch,
            state: p.state,
            kind: p.kind,
            summary: p.summary,
            since: p.since,
        }
    }
}

/// Renders the problems in the resolved format. Records is the human,
/// one-line-per-problem form; JSON/JSONL emit the stable machine shape (an
/// empty array when healthy).
fn render(
    problems: &[NotifyProblem],
    format: OutputFormat,
    palette: &Palette,
    ascii: bool,
) -> Result<(), String> {
    let now = health::now_secs();
    let mut w = io::stdout().lock();
    let res = match format {
        OutputFormat::Records => {
            // Silent when healthy: the loop simply writes nothing.
            let mut res = Ok(());
            for problem in problems {
                if let Err(e) = writeln!(w, "{}", human_line(problem, palette, now, ascii)) {
                    res = Err(e);
                    break;
                }
            }
            res
        }
        OutputFormat::Json => record::render_json(&mut w, &records(problems, now)),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, &records(problems, now)),
    };
    // Reuse the shared writer fold (a broken pipe — a prompt that read one line
    // and closed — is success, not an error).
    command::finish_write(res).map_err(|e| e.message().to_string())
}

/// The human line for one problem. Every file-derived string is flattened to a
/// single line (a multi-line failure reason must not break a prompt) and stripped
/// of control characters via [`clean_line`] so a crafted watch name or summary
/// cannot inject terminal escapes.
fn human_line(problem: &NotifyProblem, palette: &Palette, now: u64, ascii: bool) -> String {
    let raw = glyphs::render(Glyph::Warn, ascii);
    let glyph = format!(
        "{}{raw}{}",
        palette.warning.render(),
        palette.warning.render_reset()
    );
    match problem {
        NotifyProblem::Watch {
            watch,
            state,
            summary,
            since,
            ..
        } => {
            let watch = clean_line(watch);
            let state = clean_line(state);
            let summary = clean_line(summary);
            let elapsed = format_duration(Duration::from_secs(now.saturating_sub(*since)));
            format!("{glyph} vard: '{watch}' {state} (for {elapsed}) — {summary}")
        }
        NotifyProblem::DaemonNotRunning { last_write } => {
            let staleness = last_write
                .map(|written| {
                    format!(
                        " (last health update {} ago)",
                        format_duration(Duration::from_secs(now.saturating_sub(written)))
                    )
                })
                .unwrap_or_default();
            format!("{glyph} vard: daemon not running — start it with `vard run`{staleness}")
        }
        NotifyProblem::Starting => {
            format!("{glyph} vard: daemon is starting or stopping; health not yet available")
        }
        NotifyProblem::Stale { written_at } => {
            let age = format_duration(Duration::from_secs(now.saturating_sub(*written_at)));
            format!(
                "{glyph} vard: health data is stale ({age}) — the daemon may be stuck or \
                 unable to write"
            )
        }
    }
}

/// Builds the machine-form records: one object per problem with a stable field
/// set. `elapsed_seconds` is derived so a consumer need not know the current
/// time; both timestamps are bare JSON numbers (or `null`). JSON escaping makes
/// flattening unnecessary here — the machine shape carries the raw text.
fn records(problems: &[NotifyProblem], now: u64) -> Vec<Record> {
    problems.iter().map(|p| record_for(p, now)).collect()
}

fn record_for(problem: &NotifyProblem, now: u64) -> Record {
    let (watch, state, kind, summary, since) = match problem {
        NotifyProblem::Watch {
            watch,
            state,
            kind,
            summary,
            since,
        } => (
            Some(watch.clone()),
            state.clone(),
            kind.clone(),
            summary.clone(),
            Some(*since),
        ),
        NotifyProblem::DaemonNotRunning { last_write } => (
            None,
            "daemon-not-running".to_string(),
            "daemon-not-running".to_string(),
            "the vard daemon is not running; your watches are not being snapshotted".to_string(),
            *last_write,
        ),
        NotifyProblem::Starting => (
            None,
            "starting".to_string(),
            "starting".to_string(),
            "the vard daemon is starting or stopping; health not yet available".to_string(),
            None,
        ),
        NotifyProblem::Stale { written_at } => (
            None,
            "stale".to_string(),
            "stale".to_string(),
            "the vard daemon's health data is stale; it may be stuck or unable to write"
                .to_string(),
            Some(*written_at),
        ),
    };
    let elapsed = since.map(|s| now.saturating_sub(s) as i64);
    Record {
        header: None,
        fields: vec![
            RecordField::opt("watch", watch),
            RecordField::str("state", state.as_str()),
            RecordField::str("kind", kind.as_str()),
            RecordField::str("summary", summary.as_str()),
            RecordField::opt_int("since", since.map(|s| s as i64)),
            RecordField::opt_int("elapsed_seconds", elapsed),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch_problem(state: &str, summary: &str, since: u64) -> NotifyProblem {
        NotifyProblem::Watch {
            watch: "vault".to_string(),
            state: state.to_string(),
            kind: state.to_string(),
            summary: summary.to_string(),
            since,
        }
    }

    #[test]
    fn human_line_for_a_watch_carries_state_elapsed_and_summary() {
        let p = watch_problem("conflicted", "a sync conflict is blocking progress", 1000);
        // now = since + 2h.
        let line = human_line(&p, &Palette::off(), 1000 + 7200, false);
        assert_eq!(
            line,
            "⚠ vard: 'vault' conflicted (for 2h) — a sync conflict is blocking progress"
        );
    }

    #[test]
    fn human_line_for_a_stopped_daemon_names_the_fix_and_staleness() {
        let p = NotifyProblem::DaemonNotRunning {
            last_write: Some(1000),
        };
        let line = human_line(&p, &Palette::off(), 1000 + 300, false);
        assert!(line.contains("daemon not running"), "got: {line}");
        assert!(line.contains("vard run"), "must name the fix: {line}");
        assert!(line.contains("last health update 5m ago"), "got: {line}");
    }

    #[test]
    fn human_line_for_a_stopped_daemon_without_a_file_omits_staleness() {
        let p = NotifyProblem::DaemonNotRunning { last_write: None };
        let line = human_line(&p, &Palette::off(), 5000, false);
        assert!(line.contains("daemon not running"), "got: {line}");
        assert!(!line.contains("last health update"), "got: {line}");
    }

    #[test]
    fn human_line_for_the_startup_window_is_honest_not_healthy() {
        let line = human_line(&NotifyProblem::Starting, &Palette::off(), 5000, false);
        assert!(line.contains("starting or stopping"), "got: {line}");
        assert!(line.contains("health not yet available"), "got: {line}");
    }

    #[test]
    fn human_line_for_stale_data_names_the_age_and_cause() {
        let p = NotifyProblem::Stale { written_at: 1000 };
        let line = human_line(&p, &Palette::off(), 1000 + 600, false);
        assert!(line.contains("stale (10m)"), "got: {line}");
        assert!(line.contains("stuck or unable to write"), "got: {line}");
    }

    #[test]
    fn human_line_flattens_a_multiline_summary_to_one_line() {
        let p = watch_problem(
            "snapshots-failing",
            "git commit failed:\nfatal: boom\n\nline two",
            10,
        );
        let line = human_line(&p, &Palette::off(), 20, false);
        assert!(
            !line.contains('\n'),
            "a prompt line must be single-line: {line:?}"
        );
        assert!(
            line.contains("git commit failed:; fatal: boom; line two"),
            "got: {line}"
        );
    }

    #[test]
    fn human_line_sanitizes_control_characters_from_file_fields() {
        // A crafted watch name / summary must not inject terminal escapes.
        let p = NotifyProblem::Watch {
            watch: "na\x1bme".to_string(),
            state: "attention".to_string(),
            kind: "attention".to_string(),
            summary: "evil\x1b[31mred\x07".to_string(),
            since: 10,
        };
        let line = human_line(&p, &Palette::off(), 20, false);
        assert!(!line.contains('\x1b'), "raw ESC must not survive: {line:?}");
        assert!(!line.contains('\x07'), "raw BEL must not survive: {line:?}");
        assert!(
            line.contains('\u{fffd}'),
            "expected replacement char: {line:?}"
        );
    }

    #[test]
    fn ascii_fallback_glyph_when_requested() {
        // The ascii flag is injected, not read from a process-global env var, so
        // this test cannot race a sibling reading VARD_ASCII.
        let p = watch_problem("blocked", "repository is in an unsafe state", 0);
        let line = human_line(&p, &Palette::off(), 60, /* ascii */ true);
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
        assert!(s.contains(r#""kind":"attention""#), "got: {s}");
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
        let problems = vec![NotifyProblem::DaemonNotRunning { last_write: None }];
        let recs = records(&problems, 5);
        let mut out = Vec::new();
        record::render_json(&mut out, &recs).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""watch":null"#), "got: {s}");
        assert!(s.contains(r#""state":"daemon-not-running""#), "got: {s}");
    }
}
