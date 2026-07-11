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

use vard_core::{Engine, SYNC_MAX_ATTEMPTS, SharedGate, SyncOutcome, VcsBackend, WatchSpec};

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

/// How long the engine drain waits for the in-flight sync cycle to finish before
/// aborting. Derived from the engine's own convergence bound — a cycle re-runs
/// `fetch → … → push` up to [`SYNC_MAX_ATTEMPTS`] times, each with two
/// network steps of [`SYNC_NETWORK_TIMEOUT`] — plus a margin, so a genuinely slow
/// (but progressing) cycle is never cut short and misreported.
const SYNC_DRAIN_MARGIN: Duration = Duration::from_secs(30);
const SYNC_DRAIN_TIMEOUT: Duration = Duration::from_secs(
    SYNC_MAX_ATTEMPTS as u64 * SYNC_NETWORK_TIMEOUT.as_secs() * 2 + SYNC_DRAIN_MARGIN.as_secs(),
);

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

    // Resolve targets.
    //
    // * A NAMED target that exists but has syncing disabled, or (as a fast-path
    //   UX check) whose repository has no configured remote, is reported as a
    //   disabled row and exits 1 — the user asked for that one watch explicitly.
    // * With NO selector we run every sync-enabled watch and do NOT pre-filter on
    //   the remote (finding 5): the engine's live remote gate answers a
    //   remote-less watch with a `NoRemote` outcome, which renders as an
    //   informational disabled row that does NOT force a non-zero exit — so every
    //   sync-enabled watch gets a row and the remote-having ones still sync.
    let (syncable, mut disabled_rows) = match &args.target {
        Some(t) => {
            let rw = select_one(&config, t)?;
            if !rw.spec.sync() {
                (Vec::new(), vec![disabled_record(rw.spec.name())])
            } else if !spec_has_remote(&rw.spec) {
                let row = no_remote_record(rw.spec.name(), rw.spec.remote());
                (Vec::new(), vec![row])
            } else {
                (vec![rw], Vec::new())
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
        // Nothing to sync: either the named watch cannot sync (its disabled row
        // explains why), or no watch has syncing enabled at all.
        emit_records(out, &disabled_rows, "syncs")?;
        return Err(CmdError::attention(match &args.target {
            Some(_) => "that watch is not syncing (see the row above)",
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

/// The per-watch disposition the CLI reports: the engine's terminal
/// [`SyncOutcome`] for a cycle that ran, or [`Outcome::NotRun`] when it did not
/// (a request the worker could not complete before the engine stopped, or a
/// watch the engine did not know). Inferred outcomes from event silence are gone
/// — a busy or shut-down cycle reports honestly, never a false "up to date".
enum Outcome {
    /// The cycle ran to a terminal outcome.
    Ran(SyncOutcome),
    /// The cycle did not run to completion; the reason is ready to surface.
    NotRun(String),
}

/// Builds a minimal engine over `targets`, requests one **acknowledged** sync per
/// watch, drains the engine (running every queued cycle to completion), and reads
/// each cycle's terminal outcome off its acknowledgement in `targets` order. Runs
/// the async work on a scoped runtime.
fn run_cycles(
    journal_dir: &Path,
    reconcile_dir: &Path,
    targets: &[ResolvedWatch],
) -> Result<Vec<Outcome>, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting async runtime: {e}"))?;

    runtime.block_on(async move {
        let mut builder = Engine::builder()
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
        let handle = engine
            .start()
            .await
            .map_err(|e| format!("starting engine: {e}"))?;

        // Queue one acknowledged sync per watch, then drain: the drain runs every
        // queued cycle to completion before the workers exit, so each cycle's
        // acknowledgement carries its real terminal outcome. A watch the engine
        // does not know yields `None`; a cycle that never completed (an unfreed op
        // gate, a cut-short drain) drops its sender, and the receiver resolving to
        // `Err` is reported as "did not run".
        let acks: Vec<_> = targets
            .iter()
            .map(|rw| handle.request_sync_ack(rw.spec.name()))
            .collect();
        handle.shutdown().await;

        let mut outcomes = Vec::with_capacity(acks.len());
        for ack in acks {
            let outcome = match ack {
                Some(rx) => match rx.await {
                    Ok(outcome) => Outcome::Ran(outcome),
                    Err(_) => Outcome::NotRun(
                        "the sync did not run to completion before the engine stopped".to_string(),
                    ),
                },
                None => Outcome::NotRun("the engine did not accept the sync request".to_string()),
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    })
}

/// Whether `spec`'s repository defines its configured remote — a cheap,
/// non-network probe. A repository that cannot be opened, or whose remote lookup
/// errors, counts as "no remote".
fn spec_has_remote(spec: &WatchSpec) -> bool {
    vard_core::open_git_backend(spec)
        .ok()
        .and_then(|backend| backend.has_remote().ok())
        .unwrap_or(false)
}

/// Request path (a daemon is running): hand the sync to the daemon via a request
/// file. The outcome is asynchronous, so the command reports only that the
/// request was queued.
fn via_request(paths: &CmdPaths, out: &OutCtx, args: &SyncArgs) -> CmdResult {
    // Resolve a selector to the watch's *name* (the daemon routes by name) and
    // pre-check eligibility here, mirroring `snapshot`: the daemon would accept a
    // request for an ineligible watch and silently do nothing, which would look
    // like success. A watch with syncing disabled, a paused watch (the daemon does
    // not sync it), and a repository with no configured remote (the daemon left
    // its sync disabled) are all refused up front.
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
            if rw.paused {
                return Err(CmdError::attention(format!(
                    "watch {} is paused; the daemon will not sync it — resume it first",
                    rw.spec.name()
                )));
            }
            if !spec_has_remote(&rw.spec) {
                return Err(CmdError::attention(format!(
                    "sync is enabled for watch {} but its repository has no remote {:?}; \
                     add the remote first",
                    rw.spec.name(),
                    rw.spec.remote()
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

/// Builds the per-watch result record and its exit code from an [`Outcome`].
fn result_record(name: &str, outcome: &Outcome) -> (Record, u8) {
    match outcome {
        Outcome::Ran(SyncOutcome::UpToDate) => (record(name, "up to date", None, None, None), 0),
        Outcome::Ran(SyncOutcome::Moved { pushed, pulled }) => {
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
        Outcome::Ran(SyncOutcome::Conflict) => (
            record(
                name,
                "conflict",
                Some("a sync conflict needs resolution before syncing can continue"),
                None,
                None,
            ),
            1,
        ),
        Outcome::Ran(SyncOutcome::Failed(error)) => {
            (record(name, "failed", Some(error), None, None), 2)
        }
        // A sync-disabled watch is filtered out before the cycle runs, so this is
        // defensive; report it honestly rather than as success.
        Outcome::Ran(SyncOutcome::Disabled) => (
            record(name, "disabled", Some("sync is not enabled"), None, None),
            1,
        ),
        // The live remote gate found no configured remote. On the no-selector
        // path this is an informational row (the watch was not named), so it does
        // NOT force a non-zero exit — a named remote-less watch is already caught
        // by the fast-path pre-check and never reaches here.
        Outcome::Ran(SyncOutcome::NoRemote) => (
            record(
                name,
                "disabled",
                Some("the repository has no configured remote"),
                None,
                None,
            ),
            0,
        ),
        Outcome::NotRun(reason) => (record(name, "did not run", Some(reason), None, None), 2),
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

/// The row for a sync-enabled watch whose repository has no configured remote.
fn no_remote_record(name: &str, remote: &str) -> Record {
    record(
        name,
        "disabled",
        Some(&format!("no remote {remote:?} in the repository")),
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
