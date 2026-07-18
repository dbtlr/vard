//! `vard snapshot [name|path] [-m msg]` — take a manual snapshot now.
//!
//! Dispatch hinges on the single-instance lock plus its role discriminator (see
//! [`crate::instance`]): if we acquire the lock, no daemon is running and the
//! snapshot is taken in-process while the lock is *held* — a daemon that starts
//! concurrently cannot collide, because it would fail to take the lock. If a
//! *daemon* holds it, the daemon owns the repositories and the snapshot is
//! handed to it as a request file. If a *peer CLI* holds it, we wait a bounded
//! spell for it to finish and then either take the lock or report honestly that
//! another command is running — never write a request no daemon will drain.

use std::process::ExitCode;
use std::time::Duration;

use vard_core::{
    CommitMessage, SafeState, SecretMatch, SnapshotOutcome, SnapshotReport, SnapshotRequest,
    Trigger, UnsafeReason, VcsBackend, VcsError,
};

use super::{
    CmdError, CmdPaths, CmdResult, Gated, OutCtx, emit_action, emit_records, journaled_snapshot,
    load_config, open_backend, resolve_all, select_one,
};
use crate::cli::{ColorWhen, OutputFormat, SnapshotArgs};
use crate::config::ResolvedWatch;
use crate::instance::{CliLock, InstanceLock};
use crate::output::record::{Record, RecordField};
use crate::request::{self, Request};

/// How long a CLI `snapshot` waits out a *peer CLI* lock holder before
/// reporting that another command is running. A daemon holder short-circuits to
/// the request path immediately; only a transient peer is waited on.
const CLI_LOCK_BUDGET: Duration = Duration::from_secs(10);

/// Entry point for `vard snapshot`.
pub(crate) fn run(args: SnapshotArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

fn run_inner(args: SnapshotArgs, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let paths = CmdPaths::from_xdg().map_err(|e| CmdError::err(e.to_string()))?;
    let out = OutCtx::resolve(color, format);

    // Acquire the instance lock, distinguishing *who* holds it if we cannot:
    //   * we hold it       ⇒ no daemon ⇒ snapshot in-process while holding it
    //   * a daemon holds it ⇒ hand it a request
    //   * a peer CLI holds it past the budget ⇒ honest "another command is
    //     running" (never a false success, never an orphaned request)
    match InstanceLock::acquire_for_cli(&paths.lock_file, CLI_LOCK_BUDGET) {
        Ok(CliLock::Acquired(lock)) => {
            let result = in_process(&paths, &out, &args);
            // Hold the lock across the whole in-process snapshot; drop it only
            // now, once every targeted watch is done.
            drop(lock);
            result
        }
        Ok(CliLock::DaemonHeld) => via_request(&paths, &out, &args),
        Ok(CliLock::BusyPeerCli) => Err(CmdError::err(
            "another vard command is running; retry in a moment",
        )),
        Err(e) => Err(CmdError::err(format!("acquiring instance lock: {e}"))),
    }
}

/// In-process path (no daemon): snapshot each targeted watch under the held
/// instance lock, journaling each operation, and report per watch.
fn in_process(paths: &CmdPaths, out: &OutCtx, args: &SnapshotArgs) -> CmdResult {
    let config = load_config(&paths.config_file)?;
    let targets: Vec<ResolvedWatch> = match &args.target {
        Some(t) => vec![select_one(&config, t)?],
        None => resolve_all(&config)?,
    };

    if targets.is_empty() {
        emit_records(out, &[], "snapshots")?;
        return Err(CmdError::attention(
            "no watches configured; nothing to snapshot",
        ));
    }

    let mut records = Vec::with_capacity(targets.len());
    let mut worst = 0u8;
    for rw in &targets {
        let (record, code) = snapshot_one(paths, rw, args.message.as_deref());
        worst = CmdError::worse(worst, code);
        records.push(record);
    }
    emit_records(out, &records, "snapshots")?;

    match worst {
        0 => Ok(()),
        1 => Err(CmdError::attention(
            "one or more watches need attention (see above)",
        )),
        _ => Err(CmdError::err("one or more snapshots failed (see above)")),
    }
}

/// Snapshots one watch in-process, returning its result record and an exit code
/// (0 committed / no-op, 1 attention, 2 error). A paused watch is snapshotted
/// too — a manual snapshot is explicit intent — but an unsafe repository state
/// is reported and skipped.
fn snapshot_one(paths: &CmdPaths, rw: &ResolvedWatch, message: Option<&str>) -> (Record, u8) {
    let spec = &rw.spec;
    let name = spec.name();

    let backend = match open_backend(spec) {
        Ok(b) => b,
        Err(e) => {
            return (
                result_record(name, "error", Some(e.message()), None, None),
                2,
            );
        }
    };

    // Report an unsafe repository per watch (exit 1) rather than committing into
    // an in-progress operation. `snapshot` re-checks this itself, but checking
    // first keeps the message clean and skips the journal bracket for a doomed
    // op.
    match backend.is_safe_state() {
        Ok(SafeState::Unsafe(reason)) => {
            return (
                result_record(name, "unsafe", Some(&unsafe_detail(&reason)), None, None),
                1,
            );
        }
        Ok(SafeState::Safe) => {}
        Err(e) => {
            return (
                result_record(name, "error", Some(&e.to_string()), None, None),
                2,
            );
        }
    }

    // Compile the watch's secret scanner (VRD-22) just as the daemon does, so an
    // in-process manual snapshot quarantines newly-added secrets exactly as a
    // daemon pass would. A bad `secret_patterns` fails the watch with the config
    // error rather than silently snapshotting unscanned.
    let scanner = match super::compile_scanner(spec) {
        Ok(scanner) => scanner,
        Err(e) => {
            return (
                result_record(name, "error", Some(e.message()), None, None),
                2,
            );
        }
    };

    let req = SnapshotRequest {
        trigger: Trigger::Manual,
        user_text: message.map(str::to_string),
        extra_trailers: Vec::new(),
        scanner: Some(scanner),
    };

    match journaled_snapshot(&paths.journal_dir, spec.path(), name, &backend, &req) {
        // Another operation holds the watch's op lock (a concurrent restore, say).
        Gated::Busy => (
            result_record(
                name,
                "busy",
                Some("another vard operation holds this watch's lock; retry later"),
                None,
                None,
            ),
            1,
        ),
        // In-process snapshot is a sole vard actor, so `with_op_gate` never fails
        // closed here; handle it defensively as an error rather than panicking.
        Gated::LockFailed(detail) => (result_record(name, "error", Some(&detail), None, None), 2),
        Gated::Ran(Ok(report)) => snapshot_result(name, report),
        Gated::Ran(Err(VcsError::UnsafeState(reason))) => (
            result_record(name, "unsafe", Some(&unsafe_detail(&reason)), None, None),
            1,
        ),
        Gated::Ran(Err(VcsError::LockContended { .. })) => (
            result_record(
                name,
                "busy",
                Some("a git lock is held by another process; retry later"),
                None,
                None,
            ),
            1,
        ),
        Gated::Ran(Err(e)) => (
            result_record(name, "error", Some(&e.to_string()), None, None),
            2,
        ),
    }
}

/// Request path (a daemon is running): hand the snapshot to the daemon via a
/// request file. The outcome is asynchronous, so the command reports only that
/// the request was queued — never a commit result it cannot know.
fn via_request(paths: &CmdPaths, out: &OutCtx, args: &SnapshotArgs) -> CmdResult {
    // Resolve a selector to the watch's *name* — the daemon routes requests by
    // name (see `crate::daemon::apply_request`). A paused watch is refused here:
    // the daemon will not snapshot it, so a request would be a silent no-op that
    // looks like success. (In-process snapshotting of a paused watch — when no
    // daemon runs — is still allowed; a manual snapshot is explicit intent.)
    let watch_name = match &args.target {
        Some(t) => {
            let config = load_config(&paths.config_file)?;
            let rw = select_one(&config, t)?;
            if rw.paused {
                return Err(CmdError::attention(format!(
                    "watch {} is paused; the daemon will not snapshot it — resume it, \
                     or stop the daemon to snapshot in-process",
                    rw.spec.name()
                )));
            }
            Some(rw.spec.name().to_string())
        }
        None => None,
    };

    // The request-file contract carries no message field, so `-m` cannot reach
    // the daemon. Say so rather than silently dropping it.
    if args.message.is_some() {
        eprintln!("vard: note: -m/--message is ignored when a running daemon takes the snapshot");
    }

    request::write(&paths.request_dir, &Request::snapshot(watch_name.clone()))
        .map_err(CmdError::err)?;

    let (human, record) = match &watch_name {
        Some(w) => (
            format!("snapshot request for {w} handed to the running daemon"),
            result_record(w, "requested", None, None, None),
        ),
        None => (
            "snapshot request for all watches handed to the running daemon".to_string(),
            result_record("(all)", "requested", None, None, None),
        ),
    };
    emit_action(out, &human, &record)
}

/// The detail shown when a watch is skipped for an unsafe repository: the reason
/// plus guidance. A running daemon defers *its* manual snapshots the same way,
/// so the advice is to finish the in-progress operation and re-run either path.
fn unsafe_detail(reason: &UnsafeReason) -> String {
    format!(
        "{reason}; finish the merge/rebase (or leave the wrong branch) and re-run — \
         a running daemon likewise defers manual snapshots until the repo is safe again"
    )
}

/// Maps a completed in-process snapshot [`SnapshotReport`] to its result record
/// and exit code (VRD-22). Quarantine is a **warning, not a failure**: a pass
/// that withheld secrets still exits `0` if the snapshot itself succeeded, but it
/// is never silent — the withheld paths and why are printed to stderr (an
/// output-class warning, distinct from the stdout result record). The three
/// dispositions:
///
/// * a commit (with or without a withhold beside it) → `committed`;
/// * nothing committed but something withheld → `quarantined` (not `no changes`,
///   which would hide that a change *was* seen and held back);
/// * a clean tree with nothing withheld → `no changes`.
fn snapshot_result(name: &str, report: SnapshotReport) -> (Record, u8) {
    let withheld = report.quarantined.len();
    if !report.quarantined.is_empty() {
        warn_quarantined(name, &report.quarantined);
    }
    match report.committed {
        Some(outcome) => (committed_record(name, &outcome, withheld), 0),
        None if withheld > 0 => (
            result_record(
                name,
                "quarantined",
                Some(&withheld_detail(withheld)),
                None,
                None,
            ),
            0,
        ),
        None => (result_record(name, "no changes", None, None, None), 0),
    }
}

/// Prints the quarantine warning to **stderr** (an output-class warning, kept off
/// the stdout result stream): the count, then each withheld path and its reason
/// family, then how to include one anyway. It names paths and reason families
/// only — never any file content (a [`SecretMatch`] carries none).
///
/// Shared with `restore`'s protective snapshot ([`super::restore`]) so a
/// withhold during a pre-restore snapshot is surfaced identically, never
/// silently dropped.
pub(super) fn warn_quarantined(name: &str, withheld: &[SecretMatch]) {
    eprintln!(
        "vard: {name}: {} — each stays on disk, uncommitted:",
        withheld_detail(withheld.len())
    );
    for m in withheld {
        eprintln!("vard:   {} — {}", m.path.display(), m.reason);
    }
    eprintln!(
        "vard: to snapshot one anyway, move it out of the watch, or set `secret_scan = false` \
         for this watch to turn scanning off"
    );
}

/// The one-line "N file(s) withheld as likely secret(s)" phrasing shared by the
/// result record's `detail` and the stderr warning's lead line.
fn withheld_detail(count: usize) -> String {
    if count == 1 {
        "withheld 1 newly-added file as a likely secret".to_string()
    } else {
        format!("withheld {count} newly-added files as likely secrets")
    }
}

/// Builds the result record for a committed snapshot: full id and the commit's
/// change-summary subject. `withheld` is the count of files quarantined beside
/// the commit (VRD-22); when nonzero it is noted in `detail` so the machine
/// shape carries the fact the stderr warning also reports.
fn committed_record(name: &str, outcome: &SnapshotOutcome, withheld: usize) -> Record {
    let subject =
        CommitMessage::new(outcome.summary.clone(), Trigger::Manual, None, Vec::new()).subject();
    let detail = (withheld > 0).then(|| withheld_detail(withheld));
    result_record(
        name,
        "committed",
        detail.as_deref(),
        Some(outcome.id.as_str()),
        Some(&subject),
    )
}

/// A fixed-shape result record so the JSON contract carries stable keys across
/// every outcome (unused fields render as `null` / `—`).
fn result_record(
    name: &str,
    status: &str,
    detail: Option<&str>,
    id: Option<&str>,
    subject: Option<&str>,
) -> Record {
    Record {
        header: None,
        fields: vec![
            RecordField::str("name", name),
            RecordField::str("status", status),
            RecordField::opt("detail", detail.map(str::to_string)),
            RecordField::opt("id", id.map(str::to_string)),
            RecordField::opt("subject", subject.map(str::to_string)),
        ],
    }
}
