//! Resolve the effective output format from the global `--format` flag and the
//! output destination.
//!
//! The settled convention (replacing a per-command `--json`): an explicit
//! `--format` always wins; absent it, output auto-detects by destination —
//! human-readable records on a TTY, machine-readable JSON when piped. The
//! `vard watch list` command consumes this at the point it emits output.
//!
//! # Single-value surfaces
//!
//! One exception class: **single-value surfaces** — commands whose output is a
//! lone scalar, today exactly `vard config get` and `vard config path`. For
//! these, absent an explicit `--format`, the bare value is emitted regardless of
//! destination (TTY or pipe). A lone scalar's bare line is itself a machine
//! format — a parallel TEXT response type, chosen because it is simpler to
//! consume in automation (`$(vard config path)`) than JSON, not as a human
//! courtesy. An explicit `--format json`/`jsonl` still emits the enveloped
//! object. A future single-value command joins the class by resolving its format
//! through [`resolve_single_value`] instead of [`resolve`].

use crate::cli::OutputFormat;

/// Resolve the effective format: the explicit flag if present, else records on
/// a TTY and JSON when piped.
pub fn resolve(flag: Option<OutputFormat>, stdout_is_tty: bool) -> OutputFormat {
    match flag {
        Some(f) => f,
        None if stdout_is_tty => OutputFormat::Records,
        None => OutputFormat::Json,
    }
}

/// Resolve the effective format for a single-value surface: the explicit flag if
/// present, else the bare value (records) regardless of destination. See the
/// module docs for why a lone scalar's bare line is its own machine format.
pub fn resolve_single_value(flag: Option<OutputFormat>) -> OutputFormat {
    flag.unwrap_or(OutputFormat::Records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_flag_wins_on_tty() {
        assert_eq!(resolve(Some(OutputFormat::Json), true), OutputFormat::Json);
        assert_eq!(
            resolve(Some(OutputFormat::Jsonl), true),
            OutputFormat::Jsonl
        );
    }

    #[test]
    fn explicit_flag_wins_when_piped() {
        assert_eq!(
            resolve(Some(OutputFormat::Records), false),
            OutputFormat::Records
        );
    }

    #[test]
    fn auto_detects_records_on_tty() {
        assert_eq!(resolve(None, true), OutputFormat::Records);
    }

    #[test]
    fn auto_detects_json_when_piped() {
        assert_eq!(resolve(None, false), OutputFormat::Json);
    }

    #[test]
    fn single_value_defaults_to_records_regardless_of_destination() {
        assert_eq!(resolve_single_value(None), OutputFormat::Records);
    }

    #[test]
    fn single_value_explicit_flag_still_wins() {
        assert_eq!(
            resolve_single_value(Some(OutputFormat::Json)),
            OutputFormat::Json
        );
        assert_eq!(
            resolve_single_value(Some(OutputFormat::Jsonl)),
            OutputFormat::Jsonl
        );
    }
}
