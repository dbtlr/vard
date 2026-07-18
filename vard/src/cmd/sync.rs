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

use vard_core::{
    Engine, GitBackend, SYNC_MAX_ATTEMPTS, SharedGate, SyncOutcome, VcsBackend, WatchSpec,
    sync_no_remote_reason,
};

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

/// A runnable sync target: the resolved watch plus its **already-opened**
/// backend. The open is the per-watch vetting AND the backend the engine gets
/// injected (see [`run_cycles`]), so `Engine::build` never re-opens — one
/// broken repository can only ever fail its own row.
type Candidate = (ResolvedWatch, GitBackend);

/// The disposition of a named sync target's repository — classified ONCE
/// (open, then the remote probe) and rendered by both dispatch paths, so the
/// in-process rows and the daemon-present refusals can never diverge on what a
/// given repository state means.
///
/// The one deliberate asymmetry is [`ProbeFailed`](Self::ProbeFailed): the
/// in-process path treats it as runnable — the engine's own live remote probe
/// is the authority, so a transient classify-time error (e.g. `.git/config`
/// contention) gets a real sync attempt and only a persistent error fails the
/// cycle honestly — while the daemon-request path refuses up front, because a
/// fire-and-forget request has no cycle row to fall through to.
enum RepoDisposition {
    /// The repository opened and defines the configured remote: runnable, with
    /// the opened backend kept for injection.
    Runnable(GitBackend),
    /// The repository could not be opened at all — a real fault (exit 2).
    Unopenable(vard_core::VcsError),
    /// The repository opened but the remote probe itself failed (an unreadable
    /// config). Never masked as "no remote"; see the type docs for the
    /// per-path handling asymmetry. Carries the opened backend so the
    /// in-process path can still run the cycle on it.
    ProbeFailed {
        /// The opened backend (usable; only the probe failed).
        backend: GitBackend,
        /// The probe's error, for the daemon-request refusal message.
        error: vard_core::VcsError,
    },
    /// The repository opened but does not define the configured remote — the
    /// attention-class refusal (exit 1) for a named watch.
    NoRemote,
}

/// Classifies a named target's repository (see [`RepoDisposition`]).
fn classify_repo(spec: &WatchSpec) -> RepoDisposition {
    match vard_core::open_git_backend(spec) {
        Err(e) => RepoDisposition::Unopenable(e),
        Ok(backend) => match backend.has_remote() {
            Ok(true) => RepoDisposition::Runnable(backend),
            Ok(false) => RepoDisposition::NoRemote,
            Err(error) => RepoDisposition::ProbeFailed { backend, error },
        },
    }
}

/// Routes a named target's [`RepoDisposition`] on the IN-PROCESS path: either
/// a runnable candidate or a pre-decided row with its exit-code contribution.
/// [`ProbeFailed`](RepoDisposition::ProbeFailed) becomes a candidate — the
/// cycle's own live probe is the authority (see the disposition's docs) — so a
/// transient probe error never yields a definitive failed row without a sync
/// even being attempted.
fn route_named(
    rw: ResolvedWatch,
    disposition: RepoDisposition,
) -> (Vec<Candidate>, Vec<(Record, u8)>) {
    let name = rw.spec.name().to_string();
    match disposition {
        RepoDisposition::Runnable(backend) | RepoDisposition::ProbeFailed { backend, .. } => {
            (vec![(rw, backend)], Vec::new())
        }
        RepoDisposition::Unopenable(e) => (Vec::new(), vec![(open_failed_record(&name, &e), 2)]),
        RepoDisposition::NoRemote => {
            let row = no_remote_record(&name, rw.spec.remote());
            (Vec::new(), vec![(row, 1)])
        }
    }
}

/// Entry point for `vard sync`.
pub(crate) fn run(args: SyncArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

/// What a sync dispatch decided to print, kept separate from the *doing* so a
/// caller can render it — or fold it into another document — rather than having
/// the dispatch emit inline. `vard sync` (via [`run_inner`]) renders it exactly
/// as before; `watch add --sync` nests the records in its single JSON object.
pub(crate) enum SyncEmit {
    /// Per-watch result rows under the `syncs` noun (the in-process path, and
    /// the empty/error rows the no-daemon path pre-decides).
    Rows(Vec<Record>),
    /// A single action — a human sentence plus one record — for a daemon
    /// hand-off (fire-and-forget).
    Action {
        /// The human-readable line (records form).
        human: String,
        /// The single result record (machine forms).
        record: Record,
    },
    /// Nothing to print (a lock-contention or path-resolution error carried
    /// entirely on stderr via the returned [`CmdError`]).
    Nothing,
}

impl SyncEmit {
    /// The result rows, for folding into another document. A daemon-hand-off
    /// action becomes a single-element array; `Nothing` is empty.
    pub(crate) fn into_records(self) -> Vec<Record> {
        match self {
            SyncEmit::Rows(rows) => rows,
            SyncEmit::Action { record, .. } => vec![record],
            SyncEmit::Nothing => Vec::new(),
        }
    }
}

/// Renders a [`SyncEmit`] the way `vard sync` always has: a `syncs` list, a
/// single action, or nothing.
pub(crate) fn emit_sync(out: &OutCtx, emit: &SyncEmit) -> CmdResult {
    match emit {
        SyncEmit::Rows(records) => emit_records(out, records, "syncs"),
        SyncEmit::Action { human, record } => emit_action(out, human, record),
        SyncEmit::Nothing => Ok(()),
    }
}

/// Runs one `vard sync` invocation and returns its outcome without mapping to an
/// exit code. Shared with the `watch sync` opt-in gesture, which reuses the
/// exact same dispatch (daemon request when a daemon runs, in-process cycle
/// otherwise) as the confirmation cycle for a just-enabled watch.
pub(crate) fn run_inner(
    args: SyncArgs,
    color: ColorWhen,
    format: Option<OutputFormat>,
) -> CmdResult {
    let out = OutCtx::resolve(color, format);
    let (result, emit) = collect(&args);
    emit_sync(&out, &emit)?;
    result
}

/// Runs the sync dispatch and returns its outcome plus what to print, WITHOUT
/// emitting — so a caller (`watch add --sync`) can fold the records into a
/// single document. [`run_inner`] renders the [`SyncEmit`] as `vard sync`
/// always has.
pub(crate) fn collect(args: &SyncArgs) -> (CmdResult, SyncEmit) {
    let paths = match CmdPaths::from_xdg() {
        Ok(paths) => paths,
        Err(e) => return (Err(CmdError::err(e.to_string())), SyncEmit::Nothing),
    };

    // The same instance-lock role discrimination `snapshot` uses:
    //   * we hold it        ⇒ no daemon ⇒ sync in-process while holding it
    //   * a daemon holds it  ⇒ hand it a request
    //   * a peer CLI holds it past the budget ⇒ honest "another command is running"
    match InstanceLock::acquire_for_cli(&paths.lock_file, CLI_LOCK_BUDGET) {
        Ok(CliLock::Acquired(lock)) => {
            let reported = in_process(&paths, args);
            // Hold the lock across the whole in-process sync; drop it only now.
            drop(lock);
            reported
        }
        Ok(CliLock::DaemonHeld) => via_request(&paths, args),
        Ok(CliLock::BusyPeerCli) => (
            Err(CmdError::err(
                "another vard command is running; retry in a moment",
            )),
            SyncEmit::Nothing,
        ),
        Err(e) => (
            Err(CmdError::err(format!("acquiring instance lock: {e}"))),
            SyncEmit::Nothing,
        ),
    }
}

/// In-process path (no daemon): run one sync cycle per targeted, sync-enabled
/// watch under the held instance lock. Returns the overall outcome plus what to
/// print, WITHOUT emitting — the caller renders or folds it.
fn in_process(paths: &CmdPaths, args: &SyncArgs) -> (CmdResult, SyncEmit) {
    match in_process_collect(paths, args) {
        // A path that decided rows prints them — including the legitimately
        // *empty* result sets (the no-selector zero-sync-enabled case and an
        // engine-start failure), which emitted `[]` / `0 syncs` before the
        // dispatch/render split and must keep doing so.
        Ok((result, rows)) => (result, SyncEmit::Rows(rows)),
        // An error decided BEFORE dispatch (config load failure, an unresolved
        // selector) printed NOTHING before this split, and must stay silent:
        // emitting an empty `[]` / `0 syncs` here would let a JSON consumer
        // misread a hard failure (exit 2) as a benign "zero syncs".
        Err(e) => (Err(e), SyncEmit::Nothing),
    }
}

/// The fallible core of [`in_process`]: `?` short-circuits the row-less errors,
/// while the paths that have decided rows return them alongside the outcome.
fn in_process_collect(
    paths: &CmdPaths,
    args: &SyncArgs,
) -> Result<(CmdResult, Vec<Record>), CmdError> {
    let config = load_config(&paths.config_file)?;
    let named = args.target.is_some();

    // Resolve targets into cycle candidates plus pre-decided rows (each with its
    // exit-code contribution).
    //
    // * A NAMED target that has syncing disabled or is **paused** (the same
    //   refusal the daemon path makes — pausing a watch suspends it everywhere,
    //   not just under a daemon) is an attention row, exit 1; one whose
    //   repository cannot be opened is a failed row, exit 2; the no-remote
    //   fast-path check refuses with an attention row, exit 1.
    // * With NO selector every sync-enabled watch gets a row: a paused one is an
    //   informational `paused` row (exit 0 — accurate, and matching the
    //   daemon-present path's exit 0), a remote-less one is answered by the
    //   engine's live gate as an informational `disabled` row, and an
    //   unopenable one is a failed row (exit 2) that never blocks the rest.
    //
    // Each runnable candidate keeps its already-opened backend: that ONE open is
    // both the vetting and THE backend the engine uses (`run_cycles` injects it),
    // so `Engine::build` cannot re-fail on an open the CLI performed.
    let (candidates, pre_rows): (Vec<Candidate>, Vec<(Record, u8)>) = match &args.target {
        Some(t) => {
            let rw = select_one(&config, t)?;
            let name = rw.spec.name().to_string();
            if !rw.spec.sync() {
                (Vec::new(), vec![(disabled_record(&name), 1)])
            } else if rw.paused {
                (Vec::new(), vec![(paused_record(&name), 1)])
            } else {
                // One classification shared with the daemon-present path (see
                // [`classify_repo`]); [`route_named`] renders it for this path.
                let disposition = classify_repo(&rw.spec);
                route_named(rw, disposition)
            }
        }
        None => {
            let all = resolve_all(&config)?;
            let mut candidates = Vec::new();
            let mut pre_rows = Vec::new();
            let mut any_sync_enabled = false;
            for rw in all.into_iter().filter(|rw| rw.spec.sync()) {
                any_sync_enabled = true;
                if rw.paused {
                    // Informational: the watch exists and syncs, it is just
                    // paused right now — never an error on the everything
                    // path (and never a silently missing row).
                    pre_rows.push((paused_record(rw.spec.name()), 0));
                    continue;
                }
                // Per-watch isolation: one unopenable repository becomes an
                // honest failed row while every other watch still syncs —
                // Engine::build gets the vetted backends injected, so it
                // cannot re-fail on opens.
                match vard_core::open_git_backend(&rw.spec) {
                    Ok(backend) => candidates.push((rw, backend)),
                    Err(e) => {
                        pre_rows.push((open_failed_record(rw.spec.name(), &e), 2));
                    }
                }
            }
            if !any_sync_enabled {
                // An empty `syncs` list plus an attention outcome — the caller
                // still renders the (empty) rows.
                return Ok((
                    Err(CmdError::attention("no sync-enabled watches configured")),
                    Vec::new(),
                ));
            }
            (candidates, pre_rows)
        }
    };

    let results = if candidates.is_empty() {
        Vec::new()
    } else {
        let reconcile_dir =
            paths::reconcile_dir().map_err(|e: HomeNotFound| CmdError::err(e.to_string()))?;
        match run_cycles(&paths.journal_dir, &reconcile_dir, &candidates) {
            Ok(results) => results,
            Err(e) => {
                // The engine could not run at all (a runtime/start failure):
                // the rows already decided are still the truth — return them
                // with the error, never silently discard them.
                let records: Vec<Record> = pre_rows.into_iter().map(|(r, _)| r).collect();
                return Ok((Err(CmdError::err(e)), records));
            }
        }
    };

    let mut records = Vec::with_capacity(results.len() + pre_rows.len());
    let mut worst = 0u8;
    for ((rw, _backend), outcome) in candidates.iter().zip(results) {
        let (record, code) = result_record(rw.spec.name(), rw.spec.remote(), &outcome, named);
        worst = CmdError::worse(worst, code);
        records.push(record);
    }
    for (record, code) in pre_rows {
        worst = CmdError::worse(worst, code);
        records.push(record);
    }
    let result = match worst {
        0 => Ok(()),
        1 => Err(CmdError::attention(
            "one or more watches need attention (see above)",
        )),
        _ => Err(CmdError::err("one or more syncs failed (see above)")),
    };
    Ok((result, records))
}

/// Builds a minimal engine over `targets` — injecting each watch's
/// **already-opened** backend, so `Engine::build` cannot re-fail on an open the
/// caller vetted (per-watch isolation lives at the call site) — requests one
/// **acknowledged** sync per watch, drains the engine (running every queued
/// cycle to completion), and reads each cycle's terminal [`SyncOutcome`] off
/// its acknowledgement in `targets` order. The two local no-answer cases (an
/// ack whose sender was dropped, a request the engine did not accept) are
/// synthesized as [`SyncOutcome::NotRun`], so callers fold ONE representation.
/// Runs the async work on a scoped runtime.
fn run_cycles(
    journal_dir: &Path,
    reconcile_dir: &Path,
    targets: &[Candidate],
) -> Result<Vec<SyncOutcome>, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting async runtime: {e}"))?;

    runtime.block_on(async move {
        let mut builder = Engine::builder()
            .event_capacity((targets.len() * 4).max(256))
            .sync_network_timeout(SYNC_NETWORK_TIMEOUT)
            .shutdown_drain_timeout(SYNC_DRAIN_TIMEOUT);
        for (rw, backend) in targets {
            let scratch = reconcile_dir.join(journal::journal_file_name(rw.spec.path()));
            let spec: WatchSpec = rw.spec.clone().with_scratch_dir(scratch);
            let gate: SharedGate =
                Arc::new(JournalOpGate::for_repo_in_dir(journal_dir, rw.spec.path()));
            builder = builder.watch_with_backend_and_gate(spec, Arc::new(backend.clone()), gate);
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
        // acknowledgement carries its real terminal outcome.
        let acks: Vec<_> = targets
            .iter()
            .map(|(rw, _)| handle.request_sync_ack(rw.spec.name()))
            .collect();
        handle.shutdown().await;

        let mut outcomes = Vec::with_capacity(acks.len());
        for ack in acks {
            let outcome = match ack {
                Some(rx) => rx.await.unwrap_or_else(|_| {
                    SyncOutcome::NotRun(
                        "the sync did not run to completion before the engine stopped".to_string(),
                    )
                }),
                None => {
                    SyncOutcome::NotRun("the engine did not accept the sync request".to_string())
                }
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    })
}

/// Request path (a daemon is running): hand the sync to the daemon via a request
/// file. The outcome is asynchronous, so the command reports only that the
/// request was queued. Refusals and I/O errors carry only a stderr message
/// ([`SyncEmit::Nothing`]); a queued request reports a single action.
fn via_request(paths: &CmdPaths, args: &SyncArgs) -> (CmdResult, SyncEmit) {
    match via_request_inner(paths, args) {
        Ok((human, record)) => (Ok(()), SyncEmit::Action { human, record }),
        Err(e) => (Err(e), SyncEmit::Nothing),
    }
}

/// The fallible core of [`via_request`]: `?`/refusals short-circuit to the
/// stderr-only error; a queued request returns the human line and record to
/// print.
fn via_request_inner(paths: &CmdPaths, args: &SyncArgs) -> Result<(String, Record), CmdError> {
    // Resolve a selector to the watch's *name* (the daemon routes by name) and
    // pre-check a NAMED target's eligibility here as fast-path UX. The daemon
    // answers every request honestly on its own — an ineligible watch's cycle
    // terminates as a logged no-op (`sync.skipped` for a missing remote) and a
    // remote added after daemon start is picked up live — but this request path
    // is fire-and-forget: the user would see only "requested" and have to read
    // the daemon log for the outcome. For a watch the user explicitly named,
    // a request that is doomed *right now* (syncing disabled, paused so the
    // daemon does not supervise it, no configured remote) fails fast with an
    // actionable message instead. The no-selector path sends the request as-is.
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
            // One classification shared with the in-process path (see
            // [`classify_repo`]): real faults exit 2, a missing remote is the
            // attention-class refusal, with identical wording either way.
            match classify_repo(&rw.spec) {
                RepoDisposition::Runnable(_) => {}
                RepoDisposition::Unopenable(e) => {
                    return Err(CmdError::err(format!(
                        "watch {}: cannot open repository: {e}",
                        rw.spec.name()
                    )));
                }
                // Fire-and-forget has no cycle row to fall through to (the
                // asymmetry documented on [`RepoDisposition`]): refuse up front.
                RepoDisposition::ProbeFailed { error, .. } => {
                    return Err(CmdError::err(format!(
                        "watch {}: cannot probe the repository's remotes: {error}",
                        rw.spec.name()
                    )));
                }
                RepoDisposition::NoRemote => {
                    return Err(CmdError::attention(format!(
                        "sync is enabled for watch {} but {}",
                        rw.spec.name(),
                        sync_no_remote_reason(rw.spec.remote())
                    )));
                }
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
    Ok((human, record))
}

/// Builds the per-watch result record and its exit code from the engine's
/// terminal [`SyncOutcome`]. `remote` is the watch's configured remote name
/// (for the no-remote detail); `named` says whether the user asked for this
/// watch explicitly (a selector was given) — a few outcomes are informational
/// on the everything path but an attention-worthy refusal when named.
fn result_record(name: &str, remote: &str, outcome: &SyncOutcome, named: bool) -> (Record, u8) {
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
        // A sync-disabled watch is filtered out before the cycle runs, so this is
        // defensive; report it honestly rather than as success.
        SyncOutcome::Disabled => (
            record(name, "disabled", Some("sync is not enabled"), None, None),
            1,
        ),
        // The live remote gate found the configured remote missing. A NAMED
        // watch reaching it (the remote vanished between the fast-path
        // pre-check and the cycle) is the same attention-class refusal the
        // pre-check makes — exit 1. On the no-selector path the row is
        // informational and does not force a non-zero exit.
        SyncOutcome::NoRemote => (no_remote_record(name, remote), u8::from(named)),
        // The request never ran — the engine's drain gave up on a persistently
        // busy op gate, or the CLI's local no-answer cases (see `run_cycles`).
        SyncOutcome::NotRun(reason) => (record(name, "did not run", Some(reason), None, None), 2),
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

/// The row for a named watch that is paused: pausing suspends the watch
/// everywhere (the daemon does not supervise it, and the in-process path
/// refuses it identically), so an explicit sync of it is an attention-class
/// refusal, never a silent cycle.
fn paused_record(name: &str) -> Record {
    record(
        name,
        "paused",
        Some("the watch is paused; resume it to sync"),
        None,
        None,
    )
}

/// The row for a sync-enabled watch whose repository does not define its
/// configured remote (the shared [`sync_no_remote_reason`] wording — naming the
/// remote — so this row, the engine's `sync.skipped` log line, and the
/// cycle-outcome row can never drift).
fn no_remote_record(name: &str, remote: &str) -> Record {
    record(
        name,
        "disabled",
        Some(&sync_no_remote_reason(remote)),
        None,
        None,
    )
}

/// The row for a watch whose repository could not be opened at all: a real
/// failure (exit 2), isolated per watch so one broken repository never blocks
/// the others from syncing.
fn open_failed_record(name: &str, error: &vard_core::VcsError) -> Record {
    record(
        name,
        "failed",
        Some(&format!("cannot open repository: {error}")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_no_remote_outcome_is_attention_when_named_and_informational_otherwise() {
        // A NAMED watch can reach the cycle's NoRemote outcome (the remote was
        // removed between the fast-path pre-check and the cycle): that is the
        // same attention-class refusal the pre-check makes, exit 1. On the
        // no-selector path the row is informational, exit 0. The detail names
        // the missing remote either way.
        let (record, code) = result_record("w", "backup", &SyncOutcome::NoRemote, true);
        assert_eq!(code, 1, "named + NoRemote is an attention refusal");
        let rendered = format!("{record:?}");
        assert!(
            rendered.contains("backup"),
            "the detail names the missing remote: {rendered}"
        );
        let (_, code) = result_record("w", "backup", &SyncOutcome::NoRemote, false);
        assert_eq!(code, 0, "no-selector NoRemote rows are informational");
    }

    #[test]
    fn an_engine_not_run_outcome_maps_to_the_did_not_run_row() {
        let outcome = SyncOutcome::NotRun("the gate stayed busy".into());
        let (record, code) = result_record("w", "origin", &outcome, true);
        assert_eq!(code, 2);
        let rendered = format!("{record:?}");
        assert!(
            rendered.contains("did not run") && rendered.contains("the gate stayed busy"),
            "got: {rendered}"
        );
    }

    #[test]
    fn a_probe_error_falls_through_to_the_cycle_in_process() {
        // Finding 3 (round 6): a transient classify-time probe error must not
        // yield a definitive failed row with no sync even attempted — the
        // in-process path routes ProbeFailed as a runnable candidate, and the
        // engine's own live probe (the authority) decides honestly inside the
        // cycle. (The daemon-request path still refuses up front: fire-and-forget
        // has no cycle row to fall through to.)
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let backend = vard_core::GitBackend::init(&repo, Some("main")).unwrap();
        let spec = vard_core::WatchSpec::builder("w", &repo)
            .trigger(vard_core::TriggerMode::Interval)
            .interval(std::time::Duration::from_secs(3600))
            .sync(true)
            .build()
            .unwrap();
        let rw = ResolvedWatch {
            spec,
            paused: false,
            hooks: std::collections::HashMap::new(),
            hook_timeout: crate::config::DEFAULT_HOOK_TIMEOUT,
            hook_rate_limit: crate::config::DEFAULT_HOOK_RATE_LIMIT,
        };
        let disposition = RepoDisposition::ProbeFailed {
            backend,
            error: vard_core::VcsError::CommandFailed {
                op: "config".into(),
                status: Some(128),
                stderr: "transient contention".into(),
            },
        };
        let (candidates, rows) = route_named(rw, disposition);
        assert_eq!(
            candidates.len(),
            1,
            "a probe error is routed to the cycle, not a pre-decided row"
        );
        assert!(rows.is_empty(), "no failed row without a sync attempt");
    }

    #[test]
    fn run_cycles_survives_a_repository_deleted_after_vetting() {
        // The pre-check's open IS the backend the engine uses: `run_cycles`
        // injects it, so `Engine::build` never re-opens the repository. A repo
        // deleted between the pre-check and the build therefore cannot abort
        // the whole engine (which previously dropped every decided row) — the
        // build succeeds and the vanished repo surfaces as that one watch's
        // failed cycle outcome.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&repo)
            .output()
            .expect("spawn git");
        assert!(init.status.success());

        let spec = vard_core::WatchSpec::builder("w", &repo)
            .trigger(vard_core::TriggerMode::Interval)
            .interval(std::time::Duration::from_secs(3600))
            .sync(true)
            .build()
            .unwrap();
        let backend = vard_core::open_git_backend(&spec).expect("the repo opens while present");
        let rw = ResolvedWatch {
            spec,
            paused: false,
            hooks: std::collections::HashMap::new(),
            hook_timeout: crate::config::DEFAULT_HOOK_TIMEOUT,
            hook_rate_limit: crate::config::DEFAULT_HOOK_RATE_LIMIT,
        };

        // The repository vanishes AFTER the vetting open.
        std::fs::remove_dir_all(&repo).unwrap();

        let journal_dir = dir.path().join("journal");
        let reconcile_dir = dir.path().join("reconcile");
        let outcomes = run_cycles(&journal_dir, &reconcile_dir, &[(rw, backend)])
            .expect("the engine builds on the injected backend; no wholesale abort");
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(
                &outcomes[0],
                SyncOutcome::Failed(_) | SyncOutcome::NotRun(_)
            ),
            "the vanished repo is a per-watch outcome, got {:?}",
            outcomes[0]
        );
    }
}
