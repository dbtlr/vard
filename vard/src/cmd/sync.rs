//! `vard sync [name|path]` — reconcile a watch with its remote now.
//!
//! Dispatch mirrors [`snapshot`](super::snapshot) exactly: the single-instance
//! lock plus its role discriminator decides *who* does the work. If we acquire
//! the lock, no daemon is running and the sync cycle runs in-process while the
//! lock is held; if a *daemon* holds it, the daemon owns the repositories and
//! the sync is handed to it as a request file; if a *peer CLI* holds it we wait
//! a bounded spell and then report honestly rather than orphan a request.
//!
//! The in-process cycle is not a reimplementation: it builds a minimal
//! [`Engine`] over the targeted watches — the same workers,
//! the same [`request_sync`](vard_core::EngineHandle::request_sync) cycle the
//! daemon drives — requests one sync per watch, and drains the engine, which
//! runs every queued cycle to completion. The sync events emitted during the
//! run are folded into a per-watch result. Each watch is given the same
//! op-lock-and-journal gate the daemon injects and the same reconcile scratch
//! directory recovery prunes, so an in-process sync is crash-recoverable and
//! never collides with anything.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use vard_core::{Engine, Event, EventReceiver, SharedGate, WatchSpec};

use super::{
    CmdError, CmdPaths, CmdResult, OutCtx, emit_action, emit_records, load_config, resolve_all,
    select_one,
};
use crate::cli::{ColorWhen, OutputFormat, SyncArgs};
use crate::config::ResolvedWatch;
use crate::instance::{CliLock, InstanceLock};
use crate::journal::{self, JournalOpGate};
use crate::output::record::{Record, RecordField};
use crate::paths::{self, HomeNotFound};
use crate::request::{self, Request};

/// How long a CLI `sync` waits out a *peer CLI* lock holder before reporting
/// that another command is running. A daemon holder short-circuits to the
/// request path immediately; only a transient peer is waited on. Matches
/// `snapshot`'s budget.
const CLI_LOCK_BUDGET: Duration = Duration::from_secs(10);

/// Per-step network timeout for the in-process sync cycle (fetch, push). A
/// manual `vard sync` is an interactive command, so a generous bound is fine.
const SYNC_NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the engine drain waits for the in-flight sync cycle to finish
/// before aborting. Must comfortably exceed the two network steps' combined
/// budget so a genuinely slow (but progressing) fetch+push is never cut short.
const SYNC_DRAIN_TIMEOUT: Duration = Duration::from_secs(SYNC_NETWORK_TIMEOUT.as_secs() * 2 + 30);

/// Entry point for `vard sync`.
pub(crate) fn run(args: SyncArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

fn run_inner(args: SyncArgs, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let paths = CmdPaths::from_xdg().map_err(|e| CmdError::err(e.to_string()))?;
    let out = OutCtx::resolve(color, format);

    // The same instance-lock role discrimination `snapshot` uses:
    //   * we hold it        ⇒ no daemon ⇒ sync in-process while holding it
    //   * a daemon holds it  ⇒ hand it a request
    //   * a peer CLI holds it past the budget ⇒ honest "another command is running"
    match InstanceLock::acquire_for_cli(&paths.lock_file, CLI_LOCK_BUDGET) {
        Ok(CliLock::Acquired(lock)) => {
            let result = in_process(&paths, &out, &args);
            // Hold the lock across the whole in-process sync; drop it only now.
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

/// In-process path (no daemon): run one sync cycle per targeted, sync-enabled
/// watch under the held instance lock, and report per watch.
fn in_process(paths: &CmdPaths, out: &OutCtx, args: &SyncArgs) -> CmdResult {
    let config = load_config(&paths.config_file)?;

    // Resolve targets. A named target that exists but has syncing disabled is
    // reported as such (attention); with no selector we sync every sync-enabled
    // watch and simply skip the rest.
    let (syncable, mut disabled_rows) = match &args.target {
        Some(t) => {
            let rw = select_one(&config, t)?;
            if rw.spec.sync() {
                (vec![rw], Vec::new())
            } else {
                (Vec::new(), vec![disabled_record(rw.spec.name())])
            }
        }
        None => {
            let all = resolve_all(&config)?;
            let syncable: Vec<ResolvedWatch> =
                all.into_iter().filter(|rw| rw.spec.sync()).collect();
            (syncable, Vec::new())
        }
    };

    if syncable.is_empty() {
        // Nothing to sync: either the named watch has syncing off (its disabled
        // row is shown), or no watch has syncing enabled at all.
        emit_records(out, &disabled_rows, "syncs")?;
        return Err(CmdError::attention(match &args.target {
            Some(_) => "sync is not enabled for that watch",
            None => "no sync-enabled watches configured",
        }));
    }

    let reconcile_dir =
        paths::reconcile_dir().map_err(|e: HomeNotFound| CmdError::err(e.to_string()))?;
    let results =
        run_cycles(&paths.journal_dir, &reconcile_dir, &syncable).map_err(CmdError::err)?;

    let mut records = Vec::with_capacity(results.len() + disabled_rows.len());
    let mut worst = 0u8;
    for (rw, outcome) in syncable.iter().zip(results) {
        let (record, code) = result_record(rw.spec.name(), &outcome);
        worst = CmdError::worse(worst, code);
        records.push(record);
    }
    // A named non-sync target contributes an attention row and code.
    worst = disabled_rows
        .iter()
        .fold(worst, |w, _| CmdError::worse(w, 1));
    records.append(&mut disabled_rows);
    emit_records(out, &records, "syncs")?;

    match worst {
        0 => Ok(()),
        1 => Err(CmdError::attention(
            "one or more watches need attention (see above)",
        )),
        _ => Err(CmdError::err("one or more syncs failed (see above)")),
    }
}

/// Builds a minimal engine over `targets`, requests one sync per watch, drains
/// the engine (running every queued cycle to completion), and folds the emitted
/// sync events into a per-watch [`SyncOutcome`] in `targets` order. Runs the
/// async work on a scoped runtime.
fn run_cycles(
    journal_dir: &Path,
    reconcile_dir: &Path,
    targets: &[ResolvedWatch],
) -> Result<Vec<SyncOutcome>, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting async runtime: {e}"))?;

    runtime.block_on(async move {
        let mut builder = Engine::builder()
            // Room for every watch's sync events without a lagging subscriber.
            .event_capacity((targets.len() * 4).max(256))
            .sync_network_timeout(SYNC_NETWORK_TIMEOUT)
            .shutdown_drain_timeout(SYNC_DRAIN_TIMEOUT);
        for rw in targets {
            let scratch = reconcile_dir.join(journal::journal_file_name(rw.spec.path()));
            let spec: WatchSpec = rw.spec.clone().with_scratch_dir(scratch);
            let gate: SharedGate =
                Arc::new(JournalOpGate::for_repo_in_dir(journal_dir, rw.spec.path()));
            builder = builder.watch_with_gate(spec, gate);
        }
        let engine = builder
            .build()
            .map_err(|e| format!("building engine: {e}"))?;
        let mut events = engine.subscribe();
        let handle = engine
            .start()
            .await
            .map_err(|e| format!("starting engine: {e}"))?;

        // Queue one sync per watch, then drain: the drain runs every queued
        // cycle to completion before the workers exit, so once shutdown returns
        // the absence of a sync event for a watch is a definitive "nothing to
        // do", not a race.
        for rw in targets {
            handle.request_sync(rw.spec.name());
        }
        handle.shutdown().await;

        let emitted = drain_events(&mut events);
        Ok(targets
            .iter()
            .map(|rw| fold_outcome(rw.spec.name(), &emitted))
            .collect())
    })
}

/// Collects every event currently buffered on the subscriber. Called after
/// `shutdown` has joined every task, so the stream is complete and closed.
fn drain_events(events: &mut EventReceiver) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(ev) = events.try_recv() {
        out.push(ev);
    }
    out
}

/// The per-watch disposition of one sync cycle, derived from the events emitted
/// for that watch during the drained run.
enum SyncOutcome {
    /// The fetch found nothing to pull, the tree was clean, nothing to push.
    UpToDate,
    /// Local commits were pushed (with the count) and/or remote commits pulled.
    Moved { pushed: Option<usize>, pulled: bool },
    /// A reconcile conflict latched the watch `conflicted`.
    Conflict,
    /// A network/auth/reconcile step failed (message ready to surface).
    Failed(String),
}

/// Folds every event for `watch` into one [`SyncOutcome`]. A failure or a
/// conflict dominates a movement; movement dominates up-to-date.
fn fold_outcome(watch: &str, events: &[Event]) -> SyncOutcome {
    let mut pushed: Option<usize> = None;
    let mut pulled = false;
    for ev in events {
        match ev {
            Event::SyncFailed { watch: w, error } if w == watch => {
                return SyncOutcome::Failed(error.clone());
            }
            Event::SyncConflict { watch: w } if w == watch => return SyncOutcome::Conflict,
            Event::SyncPushed {
                watch: w, commits, ..
            } if w == watch => pushed = Some(*commits),
            Event::SyncPulled { watch: w, .. } if w == watch => pulled = true,
            _ => {}
        }
    }
    if pushed.is_some() || pulled {
        SyncOutcome::Moved { pushed, pulled }
    } else {
        SyncOutcome::UpToDate
    }
}

/// Request path (a daemon is running): hand the sync to the daemon via a request
/// file. The outcome is asynchronous, so the command reports only that the
/// request was queued.
fn via_request(paths: &CmdPaths, out: &OutCtx, args: &SyncArgs) -> CmdResult {
    // Resolve a selector to the watch's *name* (the daemon routes by name) and
    // refuse a watch with syncing disabled here: the daemon would accept the
    // request and do nothing, which would look like success.
    let watch_name = match &args.target {
        Some(t) => {
            let config = load_config(&paths.config_file)?;
            let rw = select_one(&config, t)?;
            if !rw.spec.sync() {
                return Err(CmdError::attention(format!(
                    "sync is not enabled for watch {}; enable it in the config first",
                    rw.spec.name()
                )));
            }
            Some(rw.spec.name().to_string())
        }
        None => None,
    };

    request::write(&paths.request_dir, &Request::sync(watch_name.clone()))
        .map_err(CmdError::err)?;

    let (human, record) = match &watch_name {
        Some(w) => (
            format!("sync request for {w} handed to the running daemon"),
            requested_record(w),
        ),
        None => (
            "sync request for all sync-enabled watches handed to the running daemon".to_string(),
            requested_record("(all)"),
        ),
    };
    emit_action(out, &human, &record)
}

/// Builds the per-watch result record and its exit code from a [`SyncOutcome`].
fn result_record(name: &str, outcome: &SyncOutcome) -> (Record, u8) {
    match outcome {
        SyncOutcome::UpToDate => (record(name, "up to date", None, None, None), 0),
        SyncOutcome::Moved { pushed, pulled } => {
            let status = match (pushed.is_some(), pulled) {
                (true, true) => "synced",
                (true, false) => "pushed",
                (false, true) => "pulled",
                // Movement with neither a push nor a pull cannot occur, but stay
                // truthful rather than assert.
                (false, false) => "up to date",
            };
            (
                record(name, status, None, pushed.map(|c| c as i64), None),
                0,
            )
        }
        SyncOutcome::Conflict => (
            record(
                name,
                "conflict",
                Some("a sync conflict needs resolution before syncing can continue"),
                None,
                None,
            ),
            1,
        ),
        SyncOutcome::Failed(error) => (record(name, "failed", Some(error), None, None), 2),
    }
}

/// The row for a watch that exists but has syncing disabled.
fn disabled_record(name: &str) -> Record {
    record(
        name,
        "disabled",
        Some("sync is not enabled for this watch"),
        None,
        None,
    )
}

/// The row for a sync handed to a running daemon (fire-and-forget).
fn requested_record(name: &str) -> Record {
    record(name, "requested", None, None, None)
}

/// A fixed-shape result record so the JSON contract carries stable keys across
/// every outcome (unused fields render as `null` / `—`).
fn record(
    name: &str,
    status: &str,
    detail: Option<&str>,
    commits: Option<i64>,
    reference: Option<&str>,
) -> Record {
    Record {
        header: None,
        fields: vec![
            RecordField::str("name", name),
            RecordField::str("status", status),
            RecordField::opt("detail", detail.map(str::to_string)),
            RecordField::opt_int("commits", commits),
            RecordField::opt("ref", reference.map(str::to_string)),
        ],
    }
}
