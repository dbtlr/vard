//! Resolve the effective output format from the global `--format` flag and the
//! output destination.
//!
//! The settled convention (replacing a per-command `--json`): an explicit
//! `--format` always wins; absent it, output auto-detects by destination —
//! human-readable records on a TTY, machine-readable JSON when piped. The
//! read/list commands (VRD-15+) consume this at the point they emit output, so
//! the resolver is foundation-only for now and carries an `allow(dead_code)`.
#![allow(dead_code)]

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
}
