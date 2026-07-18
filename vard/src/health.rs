//! The health document — the daemon's one-way status channel to `vard notify`.
//!
//! # Why a file, not a query
//!
//! `vard notify` is a shell-prompt hook (spec §8): it runs on every prompt, so
//! it must be sub-millisecond and must never talk to the daemon or shell out to
//! git — anything slower gets ripped out of a `.zshrc` within a week. The daemon
//! therefore *pushes* its current problem state into a small file that notify
//! opens, reads, and prints. The daemon is the only writer; notify is a
//! read-only reader.
//!
//! # The document is a projection, never an accumulator
//!
//! The document is **regenerated** from the engine's own truth
//! ([`EngineHandle::watch_states`](vard_core::EngineHandle::watch_states)) on
//! every relevant change, after every rebuild/reload, and on a periodic
//! heartbeat — it is never incrementally patched from a shadow copy of the event
//! stream. That makes it self-healing: a dropped `WatchStateChanged` (a slow
//! subscriber lagging the bus), a rebuild that renamed a watch, or a restart all
//! reconcile on the next regeneration instead of leaving a stale entry forever.
//!
//! # The health vocabulary (mapping engine state → what a user reads)
//!
//! The engine's [`WatchState`]/[`TroubleKind`] truth is mapped into a small
//! user-facing vocabulary by [`classify`]:
//!
//! | engine state / trouble            | health `state`     | `kind`             |
//! |-----------------------------------|--------------------|--------------------|
//! | `Ok`                              | *(not a problem)*  | —                  |
//! | `Paused` (unsafe-repo auto-pause) | `blocked`          | `unsafe-pause`     |
//! | `Attention` + `SnapshotsFailing` | `snapshots-failing`| `snapshots-failing`|
//! | `Attention` + `SourceDied`       | `attention`        | `source-died`      |
//! | `Attention` (other)              | `attention`        | `attention`        |
//! | `Conflicted`                     | `conflicted`       | `conflicted`       |
//! | `SyncError`                      | `sync-error`       | `sync-error`       |
//! | *(pre-engine: repo failed to open)* | `attention`      | `unopenable`       |
//! | *(runner: hook failing ×3)*         | `attention`      | `hook-failing`     |
//!
//! The last two rows are not derived from an engine [`WatchState`] at all. A
//! watch whose repository cannot be opened is skipped *before* it ever reaches
//! the engine (VRD-41 per-watch isolation in `daemon.rs`), so there is no
//! [`WatchStatus`] to classify. The daemon — the sole owner of that skip
//! decision — synthesizes the problem itself via [`unopenable_problem`] and
//! hands it to [`doc_from_states`] alongside the engine's own projection, so a
//! skipped watch is never silently reported as `ok`. A `hook-failing` problem is
//! likewise daemon-synthesized (VRD-21): the hooks runner is a bus subscriber
//! the engine knows nothing about, so the daemon projects its
//! [`snapshot`](crate::hooks::HooksRunnerHandle::snapshot) into
//! [`hook_failing_problem`]s and folds them in via [`doc_with_hooks`].
//!
//! Two vocabulary decisions are deliberate:
//!
//! - **The word `paused` is reserved for a pause a *user chose*.** The engine's
//!   only `Paused` state is the automatic pause it applies when a repository is
//!   in an unsafe state (mid-merge/rebase); that renders as **`blocked`** so a
//!   user never confuses "vard is deliberately idle because I asked" with "vard
//!   cannot make progress". (A *config-paused* watch never reaches the engine at
//!   all — the daemon filters paused watches out before building specs — so it
//!   can never appear here.)
//! - **Config-paused watches are not notify problems.** A watch a user
//!   deliberately paused must not nag on every shell prompt. `vard status`
//!   (VRD-17) is where a user reviews paused watches on demand; notify reports
//!   only conditions that need attention.
//!
//! # The document (versioned)
//!
//! A [`HealthDoc`] is a TOML document (reusing the binary's existing serde+TOML
//! stack, exactly as [`crate::request`] does — no new dependency for one small
//! file):
//!
//! ```toml
//! version = 2
//! written_at = 1752000000     # unix seconds of this write, for staleness
//!
//! [[problem]]
//! watch = "vault"
//! state = "blocked"           # blocked | snapshots-failing | attention | ...
//! kind = "unsafe-pause"       # the stable machine classifier
//! summary = "repository is in an unsafe state ..."
//! since = 1751990000          # unix seconds the state was entered
//!
//! [[problem]]
//! watch = "notes"
//! state = "attention"
//! kind = "hook-failing"       # a hook has failed 3+ times running (VRD-21)
//! summary = "hook for snapshot.completed has failed 3 times ..."
//! since = 1752000000
//!
//! [[suppression]]
//! watch = "notes"             # pure telemetry, never a problem (VRD-21)
//! count = 12                  # hook events coalesced (delayed, never dropped)
//! ```
//!
//! Only *problem* watches contribute a `[[problem]]` entry; a healthy watch adds
//! nothing, so a healthy daemon writes a document with an empty `problem` list. A
//! `[[suppression]]` entry is telemetry, not a problem: it records how many hook
//! events a watch coalesced and never makes the watch unhealthy or makes notify
//! speak. The `version` field lets the shape evolve — a notify built against a
//! newer schema can refuse an unknown version rather than misread it.
//!
//! Timestamps are epoch seconds so notify renders *elapsed* time ("for 2h"),
//! never a wall-clock instant that would lie across timezones. `since` comes
//! from the engine-local [`WatchStatus::entered_at`](vard_core::WatchStatus),
//! which is not persisted — so a daemon restart resets it (a watch blocked for
//! hours reads as freshly entered after a restart).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vard_core::{TroubleKind, WatchState, WatchStatus};

use crate::instance::{self, DaemonProbe};

/// The current health-document schema version. Bump on any breaking shape
/// change so a reader can reject what it cannot parse. Bumped to `2` for the
/// hooks-runner projection (VRD-21): the `hook-failing` problem kind and the
/// per-watch `[[suppression]]` telemetry table.
pub(crate) const VERSION: u32 = 2;

/// How often the daemon rewrites the health file even when nothing changed, to
/// refresh `written_at`. `vard notify` uses a multiple of this
/// ([`STALE_AFTER_SECS`]) to decide the document is stale.
pub(crate) const HEARTBEAT_INTERVAL_SECS: u64 = 60;

/// How old a running daemon's health document may be before `vard notify`
/// treats it as stale (the daemon may be wedged or unable to write). Three
/// heartbeats: a single missed write is tolerated, a sustained gap is not.
pub(crate) const STALE_AFTER_SECS: u64 = 3 * HEARTBEAT_INTERVAL_SECS;

/// The whole health document: schema version, the time it was written (for
/// staleness reporting), and the current per-watch problems.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HealthDoc {
    /// Schema version (see [`VERSION`]).
    pub version: u32,
    /// Unix seconds at which the daemon last wrote this document.
    pub written_at: u64,
    /// The current problems, one per troubled watch. Empty when every watch is
    /// healthy. Renamed to `problem` in the TOML so the array reads as
    /// `[[problem]]`.
    #[serde(default, rename = "problem")]
    pub problems: Vec<HealthProblem>,
    /// Per-watch hook suppression telemetry (VRD-21): how many hook events each
    /// watch has coalesced. Pure telemetry, never a problem — an entry here
    /// never makes a watch unhealthy or makes `vard notify` speak. Empty when no
    /// watch has coalesced a hook. Renamed to `suppression` so the array reads
    /// as `[[suppression]]`.
    #[serde(default, rename = "suppression")]
    pub suppressions: Vec<HookSuppression>,
}

/// One troubled watch's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HealthProblem {
    /// The watch's stable name.
    pub watch: String,
    /// The problem state in the health vocabulary: `blocked`,
    /// `snapshots-failing`, `attention`, `conflicted`, or `sync-error` (see the
    /// module docs for the mapping from [`WatchState`]).
    pub state: String,
    /// The stable machine classifier for the problem, distinct from `state`
    /// where the display token collapses several kinds (e.g. `attention` covers
    /// both `source-died` and a generic degraded condition).
    #[serde(default)]
    pub kind: String,
    /// A human-readable one-line summary carrying action guidance.
    pub summary: String,
    /// Unix seconds at which the watch entered this state, so a reader can
    /// render elapsed time.
    pub since: u64,
}

/// One watch's hook suppression total (VRD-21): the number of hook events the
/// runner has coalesced (delayed, never dropped) for it. Telemetry only — it is
/// carried in its own `[[suppression]]` table, never in `[[problem]]`, so it can
/// never fold into an attention state. Counts reset on daemon restart (the
/// runner's in-memory tally is projected wholesale, never accumulated in the
/// file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HookSuppression {
    /// The watch's stable name.
    pub watch: String,
    /// The coalesced-event total for this watch (always nonzero — a zero count
    /// is omitted from the projection entirely).
    pub count: u64,
}

/// The closed set of problem conditions the health vocabulary reports. Derived
/// from the engine's `(WatchState, TroubleKind)` truth by [`classify`]; a local
/// enum (not [`WatchState`], which is `#[non_exhaustive]`) so the display token,
/// machine kind, and summary are each matched *exhaustively* here — a new kind
/// cannot be added without updating all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProblemKind {
    /// The repository is in an unsafe state (mid-merge/rebase) and the engine
    /// auto-paused snapshots. Rendered `blocked`, never `paused`.
    Blocked,
    /// Snapshots are hard-failing (a failed commit or safe-state probe).
    SnapshotsFailing,
    /// A watch's signal source died; the daemon rebuilds it automatically.
    SourceDied,
    /// Any other attention condition (a degraded backend, a channel overflow).
    Attention,
    /// A sync conflict blocks progress (sync lands in a later task).
    Conflicted,
    /// A sync operation is failing.
    SyncError,
    /// A watch's repository could not be opened, so it was skipped before it
    /// ever reached the engine (VRD-41). Never produced by [`classify`] — the
    /// daemon synthesizes it directly via [`unopenable_problem`].
    Unopenable,
    /// A hook has failed on a key `FAILURE_THRESHOLD` consecutive times (VRD-21):
    /// non-zero exit or timeout, either counts. Never produced by [`classify`]
    /// (the engine does not know hooks exist) — the daemon synthesizes it from
    /// the runner's snapshot via [`hook_failing_problem`]. Self-clearing: the
    /// key's next success drops it from the runner's state, and the next
    /// projection omits it.
    HookFailing,
}

impl ProblemKind {
    /// The display token notify prints as the watch's state word.
    fn state_token(self) -> &'static str {
        match self {
            ProblemKind::Blocked => "blocked",
            ProblemKind::SnapshotsFailing => "snapshots-failing",
            ProblemKind::SourceDied | ProblemKind::Attention => "attention",
            ProblemKind::Conflicted => "conflicted",
            ProblemKind::SyncError => "sync-error",
            ProblemKind::Unopenable => "attention",
            ProblemKind::HookFailing => "attention",
        }
    }

    /// The stable machine classifier stored in the doc's `kind` field.
    fn kind_token(self) -> &'static str {
        match self {
            ProblemKind::Blocked => "unsafe-pause",
            ProblemKind::SnapshotsFailing => "snapshots-failing",
            ProblemKind::SourceDied => "source-died",
            ProblemKind::Attention => "attention",
            ProblemKind::Conflicted => "conflicted",
            ProblemKind::SyncError => "sync-error",
            ProblemKind::Unopenable => "unopenable",
            ProblemKind::HookFailing => "hook-failing",
        }
    }

    /// The one-line summary with action guidance. `reason` is the engine's
    /// transition reason, folded in where it adds detail (the error text for a
    /// failing snapshot); otherwise fixed guidance keeps the line actionable.
    fn summary(self, reason: Option<&str>) -> String {
        let reason = reason.map(str::trim).filter(|r| !r.is_empty());
        match self {
            ProblemKind::Blocked => "repository is in an unsafe state (mid-merge/rebase?); \
                 resolve it to resume snapshots"
                .to_string(),
            ProblemKind::SnapshotsFailing => match reason {
                Some(err) => format!("snapshots are failing: {err}"),
                None => "snapshots are failing".to_string(),
            },
            ProblemKind::SourceDied => {
                "watcher died; the daemon is rebuilding it automatically".to_string()
            }
            ProblemKind::Attention => reason
                .map(str::to_string)
                .unwrap_or_else(|| "needs attention".to_string()),
            ProblemKind::Conflicted => reason
                .map(str::to_string)
                .unwrap_or_else(|| "a sync conflict is blocking progress".to_string()),
            ProblemKind::SyncError => reason
                .map(str::to_string)
                .unwrap_or_else(|| "a sync operation is failing".to_string()),
            ProblemKind::Unopenable => match reason {
                Some(err) => {
                    format!("repository cannot be opened: {err}; fix it — a reload picks it up")
                }
                None => "repository cannot be opened; fix it — a reload picks it up".to_string(),
            },
            // The full detail (event, command, count, last error) is composed by
            // `hook_failing_problem` and handed in as `reason`; the fallback is
            // only for a defensively-empty detail.
            ProblemKind::HookFailing => reason
                .map(str::to_string)
                .unwrap_or_else(|| "a hook is failing repeatedly".to_string()),
        }
    }
}

/// Maps one engine [`WatchState`] (plus any [`TroubleKind`]) into a health
/// [`ProblemKind`], or `None` for a healthy watch. See the module-level table.
fn classify(state: WatchState, trouble: Option<TroubleKind>) -> Option<ProblemKind> {
    match state {
        WatchState::Ok => None,
        WatchState::Paused => Some(ProblemKind::Blocked),
        WatchState::Conflicted => Some(ProblemKind::Conflicted),
        WatchState::SyncError => Some(ProblemKind::SyncError),
        WatchState::Attention => Some(match trouble {
            Some(TroubleKind::SnapshotsFailing) => ProblemKind::SnapshotsFailing,
            Some(TroubleKind::SourceDied) => ProblemKind::SourceDied,
            _ => ProblemKind::Attention,
        }),
        // `WatchState` is `#[non_exhaustive]`: a future state defaults to a
        // generic attention problem until it is classified explicitly.
        _ => Some(ProblemKind::Attention),
    }
}

/// Projects one watch's engine status into a health problem, or `None` when the
/// watch is healthy.
fn problem_from_status(status: &WatchStatus) -> Option<HealthProblem> {
    let kind = classify(status.state, status.trouble)?;
    Some(HealthProblem {
        watch: status.name.clone(),
        state: kind.state_token().to_string(),
        kind: kind.kind_token().to_string(),
        summary: kind.summary(status.reason.as_deref()),
        since: systemtime_secs(status.entered_at),
    })
}

/// Builds a health problem for a watch skipped at engine-build time because its
/// repository could not be opened (VRD-41 per-watch isolation). The daemon
/// owns the skip decision and calls this right where it makes it — nothing
/// here re-probes the repository; it only applies the health vocabulary
/// (`state = "attention"`, `kind = "unopenable"`) to the daemon's own facts.
pub(crate) fn unopenable_problem(watch: &str, error: &str, since: u64) -> HealthProblem {
    HealthProblem {
        watch: watch.to_string(),
        state: ProblemKind::Unopenable.state_token().to_string(),
        kind: ProblemKind::Unopenable.kind_token().to_string(),
        summary: ProblemKind::Unopenable.summary(Some(error)),
        since,
    }
}

/// Builds a health problem for a hook that has failed on its key
/// `FAILURE_THRESHOLD` consecutive times (VRD-21), synthesized from the runner's
/// snapshot exactly as [`unopenable_problem`] is synthesized from the daemon's
/// own skip decision — the engine never sees hooks. `watch` is the hook's watch,
/// or `None` for a daemon-global (`[hooks]`) hook: a global hook has no watch, so
/// its problem carries an **empty** `watch` field, the honest marker `status`
/// and `notify` render as a daemon-scoped hook line (a real watch name is never
/// empty). The command is truncated if long so one pathological one-liner cannot
/// bloat the health file or a prompt. `since` is the moment the streak first
/// crossed the failure threshold (the runner's stable timestamp), so the
/// problem's age stays honest across heartbeats and reloads rather than resetting
/// on every write; the consecutive count in the summary is the primary signal.
pub(crate) fn hook_failing_problem(
    watch: Option<&str>,
    event: &str,
    command: &str,
    consecutive: u64,
    last_error: &str,
    since: u64,
) -> HealthProblem {
    let command = truncate_command(command);
    let last = last_error.trim();
    let detail = if last.is_empty() {
        format!("hook for {event} has failed {consecutive} times; command: {command}")
    } else {
        format!(
            "hook for {event} has failed {consecutive} times (last error: {last}); \
             command: {command}"
        )
    };
    HealthProblem {
        watch: watch.unwrap_or("").to_string(),
        state: ProblemKind::HookFailing.state_token().to_string(),
        kind: ProblemKind::HookFailing.kind_token().to_string(),
        summary: ProblemKind::HookFailing.summary(Some(&detail)),
        since,
    }
}

/// Truncates a hook command for a health-file summary, on a char boundary so a
/// multibyte command never splits mid-codepoint, appending an ellipsis when cut.
fn truncate_command(command: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut chars = command.chars();
    let head: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Regenerates the whole health document from a point-in-time projection of the
/// engine's per-watch truth plus the current engine generation's `skipped`
/// watches (those whose repository could not be opened and so never reached
/// the engine at all — see [`unopenable_problem`]), stamped `written_at = now`.
/// This is the only way the document is produced — it is never patched
/// incrementally. Engine-derived problems come first, skipped problems after;
/// an empty `skipped` slice reproduces the pre-VRD-41-fix behavior exactly.
pub(crate) fn doc_from_states(
    states: &[WatchStatus],
    skipped: &[HealthProblem],
    now: u64,
) -> HealthDoc {
    let mut problems: Vec<HealthProblem> = states.iter().filter_map(problem_from_status).collect();
    problems.extend(skipped.iter().cloned());
    HealthDoc {
        version: VERSION,
        written_at: now,
        problems,
        suppressions: Vec::new(),
    }
}

/// Regenerates the whole health document from the engine's per-watch truth and
/// the `skipped` set (via [`doc_from_states`]), then folds in the hooks runner's
/// projection (VRD-21): `hook_problems` (each a `hook-failing` [`HealthProblem`],
/// appended after the engine and skipped problems) and per-watch `suppressions`
/// telemetry. Both come from the daemon's projection of
/// [`HooksRunnerHandle::snapshot`](crate::hooks::HooksRunnerHandle::snapshot),
/// keeping this module decoupled from the runner's own types. Like every other
/// path this is a pure wholesale projection — no accumulation, so a hook that
/// recovered or a suppression counter that reset simply does not appear in the
/// next document.
pub(crate) fn doc_with_hooks(
    states: &[WatchStatus],
    skipped: &[HealthProblem],
    hook_problems: Vec<HealthProblem>,
    suppressions: Vec<HookSuppression>,
    now: u64,
) -> HealthDoc {
    let mut doc = doc_from_states(states, skipped, now);
    doc.problems.extend(hook_problems);
    doc.suppressions = suppressions;
    doc
}

/// What `vard notify` learned about the daemon and its health, resolved by
/// [`collect`]. Hoisted here (rather than living in `notify`) so `vard status`
/// (VRD-17) can reuse the same probe+read+version-check; notify keeps only its
/// own presentation.
pub(crate) enum HealthReport {
    /// A daemon is running and its document parsed: these are its current
    /// problems (empty when healthy), and `written_at` is when it last wrote.
    Running {
        /// The current per-watch problems.
        problems: Vec<HealthProblem>,
        /// Per-watch hook suppression telemetry (VRD-21). Read by `vard status`
        /// (rendered alongside a watch's state); deliberately ignored by `vard
        /// notify`, which reports only `problems` and so stays silent when a
        /// watch has only coalesced hooks.
        suppressions: Vec<HookSuppression>,
        /// When the daemon last wrote the document (for staleness).
        written_at: u64,
    },
    /// A daemon is running but there is no readable, parseable document yet —
    /// the startup window (before the first write) or the shutdown window (after
    /// the clear, before the lock releases). Honest, not silently healthy.
    Starting,
    /// No daemon is running. `last_write` is the leftover document's file mtime
    /// in unix seconds, if any, for a staleness nicety.
    NotRunning {
        /// The leftover health file's mtime (unix seconds), if it exists.
        last_write: Option<u64>,
    },
}

/// Resolves the health picture for `vard notify` (and `vard status`): probe the
/// instance lock, then read the document only when a daemon holds it.
///
/// - No daemon ⇒ [`HealthReport::NotRunning`], peeking only the file *mtime*
///   (never a parse) for the staleness nicety.
/// - Daemon running, document present and a supported version ⇒
///   [`HealthReport::Running`].
/// - Daemon running, document missing *or unparseable* ⇒
///   [`HealthReport::Starting`] (the startup/shutdown window).
/// - Daemon running, document parses but its version is unsupported ⇒ `Err`
///   (an operational error the caller surfaces as "upgrade vard", exit 2).
pub(crate) fn collect(lock_file: &Path, health_file: &Path) -> Result<HealthReport, String> {
    match instance::probe_daemon(lock_file).map_err(|e| format!("probing the daemon lock: {e}"))? {
        DaemonProbe::NotRunning => Ok(HealthReport::NotRunning {
            last_write: file_mtime_secs(health_file),
        }),
        DaemonProbe::Running => match read(health_file) {
            Ok(Some(doc)) if doc.version == VERSION => Ok(HealthReport::Running {
                problems: doc.problems,
                suppressions: doc.suppressions,
                written_at: doc.written_at,
            }),
            Ok(Some(doc)) => Err(format!(
                "health file schema version {} is not supported by this vard \
                 (expected {}); upgrade vard",
                doc.version, VERSION
            )),
            // Missing or unparseable while the daemon holds the lock: the
            // startup or shutdown window (the daemon clears the file before it
            // releases the lock). An honest "starting or stopping" line, never a
            // silent healthy read of a half-written or absent document.
            Ok(None) | Err(_) => Ok(HealthReport::Starting),
        },
    }
}

/// Serializes `doc` and installs it atomically at `path` (temp + rename, via a
/// same-directory temp file), so a concurrent notify read sees either the old
/// document or the whole new one, never a torn write.
///
/// Unlike [`crate::atomic::write`] this does **not** `fsync` the file or its
/// parent directory: the health file is best-effort, regenerable runtime state
/// (the daemon rewrites it on the next event or heartbeat), so paying two
/// `fsync`s on a frequently-rewritten control-plane file buys durability the
/// file does not need. Atomicity (never a torn read) is kept; durability across
/// a power cut is not.
pub(crate) fn write(path: &Path, doc: &HealthDoc) -> Result<(), String> {
    let text = toml::to_string(doc).map_err(|e| format!("encoding health document: {e}"))?;
    write_atomic_no_fsync(path, text.as_bytes())
        .map_err(|e| format!("writing health file {}: {e}", path.display()))
}

/// Temp-file + `rename(2)` install without any `fsync` (see [`write()`]).
fn write_atomic_no_fsync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "health".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Reads and parses the health document at `path`. `Ok(None)` when the file is
/// absent (no daemon has written one yet); `Err` when it exists but cannot be
/// read or parsed (a corrupt document a reader should not silently treat as
/// healthy).
pub(crate) fn read(path: &Path) -> Result<Option<HealthDoc>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading health file {}: {e}", path.display())),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| format!("parsing health file {}: {e}", path.display()))
}

/// The health file's modification time in unix seconds, or `None` when it is
/// missing or its mtime is unreadable. Used by notify for the daemon-not-running
/// staleness suffix — a bare `stat`, never a TOML parse, on the hot path.
pub(crate) fn file_mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Removes the health file, best-effort — the clean-shutdown clear and the
/// startup crash-leftover removal. A missing file is success; any other error
/// is ignored (the daemon is exiting or just starting, and notify's
/// daemon-probe does not depend on the file existing).
pub(crate) fn clear(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The current unix time in whole seconds (0 before the epoch, impossible in
/// practice).
pub(crate) fn now_secs() -> u64 {
    systemtime_secs(SystemTime::now())
}

/// A [`SystemTime`] as whole unix seconds (0 if before the epoch).
pub(crate) fn systemtime_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn status(
        name: &str,
        state: WatchState,
        trouble: Option<TroubleKind>,
        reason: &str,
    ) -> WatchStatus {
        WatchStatus {
            name: name.to_string(),
            state,
            trouble,
            reason: Some(reason.to_string()),
            entered_at: UNIX_EPOCH + Duration::from_secs(1000),
        }
    }

    #[test]
    fn healthy_watches_contribute_no_problems() {
        let states = vec![status("vault", WatchState::Ok, None, "")];
        let doc = doc_from_states(&states, &[], 2000);
        assert_eq!(doc.version, VERSION);
        assert_eq!(doc.written_at, 2000);
        assert!(doc.problems.is_empty());
    }

    #[test]
    fn unsafe_pause_renders_as_blocked_with_action_guidance() {
        let states = vec![status(
            "vault",
            WatchState::Paused,
            None,
            "a merge is in progress",
        )];
        let p = &doc_from_states(&states, &[], 2000).problems[0];
        assert_eq!(p.watch, "vault");
        assert_eq!(
            p.state, "blocked",
            "the word 'paused' is reserved for user pauses"
        );
        assert_eq!(p.kind, "unsafe-pause");
        assert!(p.summary.contains("unsafe state"), "got: {}", p.summary);
        assert!(
            p.summary.contains("resolve it"),
            "action guidance: {}",
            p.summary
        );
        assert_eq!(p.since, 1000);
    }

    #[test]
    fn snapshots_failing_carries_the_error_reason() {
        let states = vec![status(
            "vault",
            WatchState::Attention,
            Some(TroubleKind::SnapshotsFailing),
            "git commit failed (exit 1): boom",
        )];
        let p = &doc_from_states(&states, &[], 2000).problems[0];
        assert_eq!(p.state, "snapshots-failing");
        assert_eq!(p.kind, "snapshots-failing");
        assert!(
            p.summary.contains("boom"),
            "the error reason must be surfaced: {}",
            p.summary
        );
    }

    #[test]
    fn source_died_names_the_automatic_rebuild() {
        let states = vec![status(
            "vault",
            WatchState::Attention,
            Some(TroubleKind::SourceDied),
            "watch task ended abnormally",
        )];
        let p = &doc_from_states(&states, &[], 2000).problems[0];
        assert_eq!(p.state, "attention");
        assert_eq!(p.kind, "source-died");
        assert!(p.summary.contains("rebuilding"), "got: {}", p.summary);
    }

    #[test]
    fn generic_attention_falls_back_to_its_reason() {
        let states = vec![status(
            "vault",
            WatchState::Attention,
            Some(TroubleKind::Degraded),
            "inotify queue overflowed",
        )];
        let p = &doc_from_states(&states, &[], 2000).problems[0];
        assert_eq!(p.state, "attention");
        assert_eq!(p.kind, "attention");
        assert_eq!(p.summary, "inotify queue overflowed");
    }

    #[test]
    fn problems_follow_the_engine_watch_order() {
        let states = vec![
            status("zebra", WatchState::Attention, None, ""),
            status("apple", WatchState::Paused, None, "merge"),
        ];
        let names: Vec<_> = doc_from_states(&states, &[], 0)
            .problems
            .iter()
            .map(|p| p.watch.clone())
            .collect();
        // The projection preserves the engine's configured order (not sorted).
        assert_eq!(names, vec!["zebra", "apple"]);
    }

    #[test]
    fn unopenable_problem_uses_the_attention_state_and_unopenable_kind() {
        let p = unopenable_problem("broken", "not a git repository", 500);
        assert_eq!(p.watch, "broken");
        assert_eq!(p.state, "attention");
        assert_eq!(p.kind, "unopenable");
        assert!(
            p.summary.contains("not a git repository"),
            "the open error is surfaced: {}",
            p.summary
        );
        assert!(
            p.summary.contains("reload"),
            "action guidance points at a reload: {}",
            p.summary
        );
        assert_eq!(p.since, 500);
    }

    #[test]
    fn doc_from_states_appends_skipped_problems_after_engine_problems() {
        let states = vec![status("zebra", WatchState::Attention, None, "trouble")];
        let skipped = vec![unopenable_problem("broken", "no such repo", 10)];
        let doc = doc_from_states(&states, &skipped, 2000);
        let names: Vec<_> = doc.problems.iter().map(|p| p.watch.clone()).collect();
        assert_eq!(
            names,
            vec!["zebra", "broken"],
            "engine problems come first, skipped problems follow"
        );
        assert_eq!(doc.problems[1].kind, "unopenable");
    }

    #[test]
    fn an_empty_skipped_slice_reproduces_the_pre_vrd41_fix_behavior() {
        let states = vec![status(
            "vault",
            WatchState::Attention,
            Some(TroubleKind::SnapshotsFailing),
            "boom",
        )];
        let with_empty_skipped = doc_from_states(&states, &[], 2000);
        assert_eq!(with_empty_skipped.problems.len(), 1);
        assert_eq!(with_empty_skipped.problems[0].watch, "vault");
    }

    #[test]
    fn document_round_trips_through_toml() {
        let states = vec![status(
            "vault",
            WatchState::Attention,
            Some(TroubleKind::SnapshotsFailing),
            "x",
        )];
        let doc = doc_from_states(&states, &[], 200);
        let text = toml::to_string(&doc).unwrap();
        let back: HealthDoc = toml::from_str(&text).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn write_then_read_round_trips_and_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health");
        assert_eq!(read(&path).unwrap(), None, "missing file reads as None");

        let doc = doc_from_states(
            &[status(
                "w",
                WatchState::Attention,
                Some(TroubleKind::SourceDied),
                "backend died",
            )],
            &[],
            9,
        );
        write(&path, &doc).unwrap();
        assert_eq!(read(&path).unwrap(), Some(doc));
        // A temp+rename write leaves no stray temp behind.
        let temps: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(temps.is_empty(), "temp left behind: {temps:?}");
    }

    #[test]
    fn a_corrupt_document_is_an_error_not_silently_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health");
        std::fs::write(&path, "this = is = not = toml").unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn file_mtime_is_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(file_mtime_secs(&dir.path().join("nope")), None);
        let path = dir.path().join("health");
        std::fs::write(&path, "x").unwrap();
        assert!(file_mtime_secs(&path).is_some());
    }

    #[test]
    fn hook_failing_problem_for_a_watch_carries_the_detail_and_attention_state() {
        let p = hook_failing_problem(
            Some("notes"),
            "snapshot.completed",
            "dotfiles-apply",
            3,
            "exited with status 1",
            2000,
        );
        assert_eq!(p.watch, "notes");
        assert_eq!(p.state, "attention");
        assert_eq!(p.kind, "hook-failing");
        assert!(
            p.summary.contains("snapshot.completed"),
            "got: {}",
            p.summary
        );
        assert!(
            p.summary.contains("3 times"),
            "count is surfaced: {}",
            p.summary
        );
        assert!(
            p.summary.contains("exited with status 1"),
            "last error is surfaced: {}",
            p.summary
        );
        assert!(
            p.summary.contains("dotfiles-apply"),
            "command is surfaced: {}",
            p.summary
        );
        assert_eq!(p.since, 2000);
    }

    #[test]
    fn a_global_hook_failure_uses_an_empty_watch_as_the_daemon_marker() {
        // A real watch name is never empty, so an empty `watch` is the honest
        // marker for a daemon-global `[hooks]` hook (no watch to attach to).
        let p = hook_failing_problem(None, "daemon.started", "notify-up", 5, "timed out", 10);
        assert_eq!(p.watch, "", "a global hook carries no watch name");
        assert_eq!(p.kind, "hook-failing");
        assert!(p.summary.contains("daemon.started"), "got: {}", p.summary);
    }

    #[test]
    fn a_long_hook_command_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(200);
        let p = hook_failing_problem(Some("w"), "sync.pulled", &long, 3, "boom", 0);
        assert!(
            p.summary.contains('…'),
            "a long command is truncated: {}",
            p.summary
        );
        // No panic on a multibyte boundary is the real assertion; the ellipsis
        // proves the cut happened.
    }

    #[test]
    fn doc_with_hooks_appends_hook_problems_after_engine_problems_and_carries_suppression() {
        let states = vec![status("vault", WatchState::Attention, None, "trouble")];
        let hook_problems = vec![hook_failing_problem(
            Some("notes"),
            "snapshot.completed",
            "apply",
            3,
            "exit 1",
            2000,
        )];
        let suppressions = vec![HookSuppression {
            watch: "notes".to_string(),
            count: 12,
        }];
        let doc = doc_with_hooks(&states, &[], hook_problems, suppressions, 2000);
        assert_eq!(doc.version, VERSION);
        let names: Vec<_> = doc.problems.iter().map(|p| p.watch.clone()).collect();
        assert_eq!(
            names,
            vec!["vault", "notes"],
            "engine problems come first, hook problems follow"
        );
        assert_eq!(doc.problems[1].kind, "hook-failing");
        assert_eq!(doc.suppressions.len(), 1);
        assert_eq!(doc.suppressions[0].count, 12);
    }

    #[test]
    fn a_document_with_suppression_round_trips_through_toml() {
        let doc = doc_with_hooks(
            &[status("v", WatchState::Ok, None, "")],
            &[],
            vec![hook_failing_problem(
                Some("v"),
                "sync.pulled",
                "cmd",
                4,
                "boom",
                5,
            )],
            vec![HookSuppression {
                watch: "v".to_string(),
                count: 3,
            }],
            5,
        );
        let text = toml::to_string(&doc).unwrap();
        let back: HealthDoc = toml::from_str(&text).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn suppression_defaults_to_empty_when_absent_from_the_file() {
        // A v2 doc with no [[suppression]] table parses with an empty vec, so a
        // healthy daemon's document need not carry the section at all.
        let doc: HealthDoc = toml::from_str("version = 2\nwritten_at = 1\n").unwrap();
        assert!(doc.suppressions.is_empty());
        assert!(doc.problems.is_empty());
    }
}
