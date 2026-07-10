//! `vard snapshot [name|path] [-m msg]` — take a manual snapshot now.
//!
//! Dispatch hinges on the single-instance flock as a race-free discriminator
//! (see the [module docs](super)): acquiring it proves no daemon is running, so
//! the snapshot is taken in-process while the lock is *held* — a daemon that
//! starts concurrently cannot collide, because it would fail to take the lock.
//! If the lock is already held, a daemon owns the repositories and the snapshot
//! is handed to it as a request file instead.

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use vard_core::{
    CommitMessage, SafeState, SnapshotOutcome, SnapshotRequest, Trigger, VcsBackend, VcsError,
};

use super::{
    CmdError, CmdPaths, CmdResult, OutCtx, emit_action, emit_records, journaled_snapshot,
    load_config, open_backend, resolve_all, select_one,
};
use crate::cli::{ColorWhen, OutputFormat, SnapshotArgs};
use crate::config::ResolvedWatch;
use crate::instance::{InstanceLock, LockError};
use crate::output::record::{Record, RecordField};

/// Entry point for `vard snapshot`.
pub(crate) fn run(args: SnapshotArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

fn run_inner(args: SnapshotArgs, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let paths = CmdPaths::from_xdg().map_err(|e| CmdError::err(e.to_string()))?;
    let out = OutCtx::resolve(color, format);

    // TRY the instance lock. Acquired ⇒ no daemon ⇒ snapshot in-process while
    // holding it. Held ⇒ a daemon owns the repos ⇒ hand it a request.
    match InstanceLock::acquire_at(&paths.lock_file) {
        Ok(lock) => {
            let result = in_process(&paths, &out, &args);
            // Hold the lock across the whole in-process snapshot; drop it only
            // now, once every targeted watch is done.
            drop(lock);
            result
        }
        Err(LockError::Held { .. }) => via_request(&paths, &out, &args),
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
                result_record(name, "unsafe", Some(&reason.to_string()), None, None),
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

    let req = SnapshotRequest {
        trigger: Trigger::Manual,
        user_text: message.map(str::to_string),
        extra_trailers: Vec::new(),
    };

    match journaled_snapshot(&paths.journal_dir, name, &backend, &req) {
        Ok(Some(outcome)) => (committed_record(name, &outcome), 0),
        Ok(None) => (result_record(name, "no changes", None, None, None), 0),
        Err(VcsError::UnsafeState(reason)) => (
            result_record(name, "unsafe", Some(&reason.to_string()), None, None),
            1,
        ),
        Err(VcsError::LockContended { .. }) => (
            result_record(
                name,
                "busy",
                Some("a git lock is held by another process; retry later"),
                None,
                None,
            ),
            1,
        ),
        Err(e) => (
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
    // name (see `crate::daemon::apply_request`).
    let watch_name = match &args.target {
        Some(t) => {
            let config = load_config(&paths.config_file)?;
            Some(select_one(&config, t)?.spec.name().to_string())
        }
        None => None,
    };

    // The request-file contract carries no message field, so `-m` cannot reach
    // the daemon. Say so rather than silently dropping it.
    if args.message.is_some() {
        eprintln!("vard: note: -m/--message is ignored when a running daemon takes the snapshot");
    }

    write_request(&paths.request_dir, watch_name.as_deref())?;

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

/// Writes a settled snapshot request into the request dir per the daemon's
/// contract: a temp (dotfile) name written first, then `rename(2)`-d to a
/// `*.toml` name — atomic on POSIX, so the daemon only ever reads a complete
/// file. `watch` is `None` for an all-watches request.
fn write_request(request_dir: &Path, watch: Option<&str>) -> CmdResult {
    fs::create_dir_all(request_dir)
        .map_err(|e| CmdError::err(format!("creating request dir: {e}")))?;

    let mut text = String::from("kind = \"snapshot\"\n");
    if let Some(w) = watch {
        text.push_str(&format!("watch = \"{}\"\n", toml_basic_escape(w)));
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    // A dotfile temp name is "unsettled" per the contract, so the daemon
    // ignores it while it is being written.
    let tmp = request_dir.join(format!(".snapshot-{pid}-{nanos}.tmp"));
    let settled = request_dir.join(format!("snapshot-{pid}-{nanos}.toml"));

    fs::write(&tmp, text.as_bytes())
        .map_err(|e| CmdError::err(format!("writing request file: {e}")))?;
    fs::rename(&tmp, &settled).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        CmdError::err(format!("finalizing request file: {e}"))
    })?;
    Ok(())
}

/// Escapes a value for a TOML basic string. Watch names are validated to a safe
/// charset, so escaping the two structural characters is sufficient.
fn toml_basic_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Builds the result record for a committed snapshot: full id and the commit's
/// change-summary subject.
fn committed_record(name: &str, outcome: &SnapshotOutcome) -> Record {
    let subject =
        CommitMessage::new(outcome.summary.clone(), Trigger::Manual, None, Vec::new()).subject();
    result_record(
        name,
        "committed",
        None,
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
