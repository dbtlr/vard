//! `vard log <name|path> [--since "2h"]` — show a watch's snapshot history.
//!
//! Read-only: reads the backend log directly, takes no lock, and mutates
//! nothing, so it is safe against a watch the daemon is actively snapshotting.

use std::process::ExitCode;
use std::time::SystemTime;

use vard_core::{LogFilter, Snapshot, VcsBackend};

use super::timefmt;
use super::{
    CmdError, CmdPaths, CmdResult, OutCtx, emit_records, load_config, open_backend, select_one,
};
use crate::cli::{ColorWhen, LogArgs, OutputFormat};
use crate::output::record::{Record, RecordField};

/// Entry point for `vard log`.
pub(crate) fn run(args: LogArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

fn run_inner(args: LogArgs, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let paths = CmdPaths::from_xdg().map_err(|e| CmdError::err(e.to_string()))?;
    let out = OutCtx::resolve(color, format);

    let config = load_config(&paths.config_file)?;
    let rw = select_one(&config, &args.target)?;
    let backend = open_backend(&rw.spec)?;

    let since = match &args.since {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };
    let filter = LogFilter {
        since,
        until: None,
        limit: None,
    };
    let snapshots = backend
        .log(&filter)
        .map_err(|e| CmdError::err(format!("reading log for {:?}: {e}", rw.spec.name())))?;

    let records: Vec<Record> = snapshots.iter().map(snapshot_record).collect();
    emit_records(&out, &records, "snapshots")
}

/// Parses a `--since` duration ("2h", "3d") into the cutoff instant that far in
/// the past, saturating at the epoch. Shares [`timefmt::duration_back_from_now`]
/// with `restore --at`, so the two flags interpret a duration identically.
fn parse_since(raw: &str) -> Result<SystemTime, CmdError> {
    let duration = vard_core::parse_duration(raw).map_err(|e| CmdError::err(e.to_string()))?;
    Ok(timefmt::duration_back_from_now(duration, SystemTime::now()))
}

/// Builds the display record for one snapshot: full id, RFC 3339 UTC time,
/// subject, and the trigger (absent when the trailer was missing).
fn snapshot_record(snapshot: &Snapshot) -> Record {
    Record {
        header: None,
        fields: vec![
            RecordField::str("id", snapshot.id.as_str()),
            RecordField::str("time", timefmt::format_rfc3339_utc(snapshot.time)),
            RecordField::str("subject", &snapshot.subject),
            RecordField::opt("trigger", snapshot.trigger.map(|t| t.to_string())),
        ],
    }
}
