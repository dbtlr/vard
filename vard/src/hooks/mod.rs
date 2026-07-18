//! The hooks runner: a binary-level bus subscriber that runs user shell commands
//! in reaction to engine [`Event`]s, guarded against loops by a per-key
//! trailing-edge coalescing limiter.
//!
//! # Shape
//!
//! - [`HooksConfig`] is the runner's arming: the resolved global (`[hooks]`) and
//!   per-watch (`[watch.hooks]`) command maps, each with its scope's timeout,
//!   rate limit, and working directory. [`HooksConfig::build`] returns `None`
//!   when no hooks are configured anywhere, so the daemon skips the runner
//!   entirely.
//! - [`spawn`] subscribes the runner to the engine bus (its **own** subscription,
//!   never shared with the health-writer loop) and starts its task, returning a
//!   [`HooksRunnerHandle`]. Dropping the handle aborts the task — this is how the
//!   daemon re-arms cleanly on an engine rebuild or config reload. The re-armed
//!   runner is not born empty: [`HooksRunnerHandle::carryover`] hands the old
//!   runner's [`Carryover`] (limiter per-key phase, pending trailing slots,
//!   failure streaks, and suppression totals) into the new [`spawn`], keyed by
//!   the reload-stable `(scope, event, command)` and pruned to the new config.
//!
//! # State across a re-arm
//!
//! Because the state survives a reload/rebuild:
//! - A coalesced trailing event that had not yet fired stays pending and fires
//!   when its window opens — "delayed, never dropped" holds across reloads too.
//!   Process shutdown is the honest boundary: a pending slot does not survive the
//!   daemon exiting.
//! - A key's consecutive-failure streak keeps counting across reloads, so a
//!   BackoffRebuild loop cannot silently reset a failing hook below its threshold.
//!   Its `since` (the moment the streak crossed the reporting threshold) rides
//!   along too, so the projected health problem's age is stable across reloads.
//! - A run that was *in flight* at the re-arm cannot be handed to the successor
//!   (whose `JoinSet` has no task for it), so the carried key collapses from
//!   `Running` to `Cooldown` anchored at its real run-start. The old blocking
//!   pool still reaps that child; its outcome is merely unreportable. Debounce and
//!   the trailing pending slot are both preserved, so the key never wedges.
//! - `daemon.started`, which every new engine generation re-emits, hits a limiter
//!   that remembers the previous fire: a reload storm debounces to at most one hook
//!   run per cooldown window. This is deliberate — there is no first-generation-only
//!   lifecycle delivery. `daemon.started` therefore fires at startup and again on
//!   each config reload/engine rebuild, rate-limited by the loop guard;
//!   `daemon.stopped` fires only at true shutdown, since a reload aborts the old
//!   runner before the old engine drains its `daemon.stopped`. That asymmetry is an
//!   accepted trade. At true shutdown the daemon gives the runner a short bounded
//!   drain ([`HooksRunnerHandle::drain`]) to observe `daemon.stopped` and dispatch
//!   its hook before it is dropped; delivery is nonetheless **best-effort** — a
//!   mid-hook runner is not waited on past the drain budget, and a SIGKILL or crash
//!   still loses `daemon.stopped`.
//! - A command changed in the config is a *different* key, so the old pending slot
//!   for the previous command is pruned rather than fired — the honest behavior.
//! - The [`limiter`] holds the pure `idle -> running -> cooldown` decision core;
//!   [`exec`] runs the process with the SIGTERM/SIGKILL process-group discipline.
//!
//! # Loop guard
//!
//! Each hook key is `(scope, dotted event name, command)`. A key runs
//! single-flight with one latest-wins pending slot: an event while idle runs at
//! once; an event while running or cooling down is coalesced (the pending slot is
//! replaced and a suppressed counter bumped) and the *last* such event is
//! guaranteed to run once the cooldown window (anchored at the run's start)
//! elapses. Suppression only delays the trailing event, never drops it. See
//! [`limiter`] for the state machine and its property tests.
//!
//! # Env contract
//!
//! Each hook receives an enumerated `VARD_*` environment (stdin stays closed):
//! `VARD_EVENT` and `VARD_SUPPRESSED` always; `VARD_WATCH`/`VARD_PATH` for
//! watch-scoped events; and per-payload `VARD_REF`, `VARD_PREV_REF`,
//! `VARD_FILES_CHANGED`, `VARD_ERROR`, `VARD_QUARANTINED_COUNT`. An unset
//! variable is absent from the environment, never an empty string. See
//! [`hook_env`].
//!
//! # State for observability
//!
//! The runner accumulates per-watch suppression totals and per-key
//! consecutive-failure counts. [`HooksRunnerHandle::snapshot`] is a cheap, pure
//! read of that state ([`RunnerSnapshot`]) — the projection the daemon folds into
//! the health file (`hook-failing` problems and per-watch suppression telemetry,
//! see [`crate::health`]). This module builds the state; it does not report it.

mod exec;
mod limiter;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::task::{Id, JoinSet};
use tracing::{trace, warn};
use vard_core::{Event, EventReceiver, RecvError};

use crate::config::HookMap;
use exec::{HookInvocation, HookOutcome, exec_hook};
use limiter::{Fire, Limiter};

/// Consecutive-failure count at which a hook key is surfaced as a health problem.
/// A constant — a configurable knob waits for demand. The daemon projects every
/// [`FailingHook`] at or beyond this count into a `hook-failing` health problem.
pub(crate) const FAILURE_THRESHOLD: u64 = 3;

/// A hook's scope: a specific watch, or the daemon-global `[hooks]` section.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Scope {
    /// The top-level `[hooks]` section (daemon-level events).
    Global,
    /// A watch's `[watch.hooks]`, named by the watch.
    Watch(String),
}

impl Scope {
    /// The log/label token for this scope.
    fn label(&self) -> &str {
        match self {
            Scope::Global => "daemon",
            Scope::Watch(name) => name,
        }
    }
}

/// One hook's identity: `(scope, dotted event name, command)`. Single-flight and
/// failure tracking are per key.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct HookKey {
    scope: Scope,
    event: String,
    command: String,
}

/// The hook commands and execution parameters for one scope.
struct ScopeHooks {
    /// Dotted event name -> shell command.
    hooks: HookMap,
    /// This scope's hook command timeout.
    timeout: Duration,
    /// This scope's cooldown window (`hook_rate_limit`).
    rate_limit: Duration,
    /// Working directory for hooks in this scope.
    cwd: PathBuf,
}

/// One watch's hook arming, passed to [`HooksConfig::build`].
pub(crate) struct WatchHooks {
    /// The watch's stable name.
    pub name: String,
    /// The watch's repository path (hook working directory).
    pub path: PathBuf,
    /// Dotted event name -> command, from `[watch.hooks]`.
    pub hooks: HookMap,
    /// The watch's effective `hook_timeout`.
    pub timeout: Duration,
    /// The watch's effective `hook_rate_limit`.
    pub rate_limit: Duration,
}

/// The runner's complete arming: global hooks plus a per-watch map.
pub(crate) struct HooksConfig {
    global: ScopeHooks,
    watches: HashMap<String, ScopeHooks>,
}

impl HooksConfig {
    /// Assembles the arming from the resolved global hooks (with the daemon-level
    /// timeout/rate-limit and cwd) and the active watches. Returns `None` when no
    /// hooks are configured anywhere, so the daemon can skip spawning the runner.
    /// Watches with an empty hook map contribute nothing.
    pub(crate) fn build(
        global: HookMap,
        global_timeout: Duration,
        global_rate_limit: Duration,
        watches: impl IntoIterator<Item = WatchHooks>,
    ) -> Option<HooksConfig> {
        let mut map = HashMap::new();
        for watch in watches {
            if watch.hooks.is_empty() {
                continue;
            }
            map.insert(
                watch.name,
                ScopeHooks {
                    hooks: watch.hooks,
                    timeout: watch.timeout,
                    rate_limit: watch.rate_limit,
                    cwd: watch.path,
                },
            );
        }
        if global.is_empty() && map.is_empty() {
            return None;
        }
        let daemon_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Some(HooksConfig {
            global: ScopeHooks {
                hooks: global,
                timeout: global_timeout,
                rate_limit: global_rate_limit,
                cwd: daemon_cwd,
            },
            watches: map,
        })
    }

    /// Every hook key this config arms: the identities carried state must be
    /// pruned to on a re-arm, so a pending slot or failure streak for a hook the
    /// new config no longer defines is dropped rather than resurrected.
    fn hook_keys(&self) -> impl Iterator<Item = HookKey> + '_ {
        let global = self.global.hooks.iter().map(|(event, command)| HookKey {
            scope: Scope::Global,
            event: event.clone(),
            command: command.clone(),
        });
        let watches = self.watches.iter().flat_map(|(name, scope)| {
            scope.hooks.iter().map(move |(event, command)| HookKey {
                scope: Scope::Watch(name.clone()),
                event: event.clone(),
                command: command.clone(),
            })
        });
        global.chain(watches)
    }
}

/// A cheap, point-in-time read of the runner's accumulated state — the pure
/// projection the daemon folds into the health file (`hook-failing` problems and
/// per-watch suppression telemetry). Building it is a lock, clone, and unlock.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunnerSnapshot {
    /// Total coalesced (suppressed) event counts per watch name.
    pub suppressed_by_watch: HashMap<String, u64>,
    /// Hook keys at or beyond [`FAILURE_THRESHOLD`] consecutive failures, sorted
    /// for stable rendering.
    pub failing: Vec<FailingHook>,
}

/// One persistently-failing hook, for the health projection.
#[derive(Debug, Clone)]
pub(crate) struct FailingHook {
    /// The watch name, or `None` for a daemon-global hook.
    pub watch: Option<String>,
    /// The dotted event name the hook fires on.
    pub event: String,
    /// The shell command.
    pub command: String,
    /// Consecutive failures (`>= FAILURE_THRESHOLD`).
    pub consecutive: u64,
    /// The most recent failure's reason.
    pub last_error: String,
    /// When this key's streak first reached [`FAILURE_THRESHOLD`] — the stable
    /// moment the health projection stamps as the problem's `since`, so its age
    /// does not reset on every heartbeat or unrelated health write. Carried
    /// across a re-arm with the rest of the failure state.
    pub since: SystemTime,
}

/// The runner's mutable state, shared between the runner task (writer) and
/// [`HooksRunnerHandle::snapshot`] (reader), and carried across a re-arm (see
/// [`Carryover`]). Holds the coalescing [`Limiter`] itself so a reload preserves
/// each key's phase and pending trailing slot, not just the observability tallies.
#[derive(Clone)]
struct RunnerState {
    /// Coalesced-event totals per watch name.
    suppressed_by_watch: HashMap<String, u64>,
    /// Per-key failure tracking; a key is absent once it succeeds.
    failures: HashMap<HookKey, Failure>,
    /// The per-key `idle -> running -> cooldown` decision core. Lives here so it
    /// survives a re-arm; only the runner task mutates it (plus the drain that
    /// builds a [`Carryover`]).
    limiter: Limiter<HookKey, HookInvocation>,
}

impl Default for RunnerState {
    fn default() -> Self {
        RunnerState {
            suppressed_by_watch: HashMap::new(),
            failures: HashMap::new(),
            limiter: Limiter::new(),
        }
    }
}

/// One key's consecutive-failure tally, last error, and the moment the streak
/// first crossed [`FAILURE_THRESHOLD`].
#[derive(Clone)]
struct Failure {
    consecutive: u64,
    last_error: String,
    /// `Some` once `consecutive` first reaches [`FAILURE_THRESHOLD`]; the stable
    /// `since` for the health projection. `None` while the streak is still below
    /// the threshold (not yet a reported problem).
    since: Option<SystemTime>,
}

/// A runner's state captured for handoff to its re-armed successor: the limiter's
/// per-key phase and pending slots, the failure streaks, and the suppression
/// totals. Cloned from the old runner (which keeps running until its handle is
/// dropped) and pruned to the new config in [`spawn`].
pub(crate) struct Carryover {
    state: RunnerState,
}

/// A handle to a running hooks runner. Dropping it aborts the runner task
/// (the daemon's re-arm on engine rebuild/reload), while any hook already
/// executing on a blocking task still runs to completion and reaps its child.
pub(crate) struct HooksRunnerHandle {
    // Read by `snapshot`, which the daemon calls to build the health projection;
    // the runner task writes it throughout.
    state: Arc<Mutex<RunnerState>>,
    task: tokio::task::JoinHandle<()>,
}

impl HooksRunnerHandle {
    /// Captures this runner's state for handoff to a re-armed runner. Cloned, not
    /// drained: the old runner keeps serving its bus until the daemon drops its
    /// handle, and on a *failed* rebuild it is kept as the live runner — draining
    /// would leave it (or a bad-config fallback) amnesiac. On a successful rebuild
    /// the old runner is aborted right after, so the clone cannot double-fire a
    /// pending slot except within the brief both-armed rebuild window the daemon
    /// already tolerates.
    pub(crate) fn carryover(&self) -> Carryover {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = state.clone();
        // The successor's JoinSet has no task for a run that is in flight right
        // now, so it could never call `on_finished` on a carried `Running` key —
        // that key would coalesce forever without firing. Collapse any in-flight
        // run to cooldown (anchored at its real run-start): the old blocking pool
        // still reaps the child, and the pending slot fires at the window edge.
        state.limiter.collapse_running_to_cooldown();
        Carryover { state }
    }

    /// Best-effort drain at true shutdown: waits up to `budget` for the runner
    /// task to end on its own — the engine bus closing (see
    /// [`EngineHandle::shutdown`](vard_core::EngineHandle)) ends the loop cleanly,
    /// so a `daemon.stopped` hook it already dispatched gets a chance to start —
    /// then aborts whatever is left. Consumes the handle; `Drop` aborts the task
    /// on the way out (a no-op once it has ended). Delivery is best-effort: a mid-
    /// hook runner is not waited on beyond `budget`, and a SIGKILL/crash still
    /// loses `daemon.stopped`.
    pub(crate) async fn drain(mut self, budget: Duration) {
        if tokio::time::timeout(budget, &mut self.task).await.is_err() {
            warn!("hooks runner: shutdown drain timed out; aborting the runner");
        }
    }

    /// A cheap, pure read of the runner's state for the health projection.
    pub(crate) fn snapshot(&self) -> RunnerSnapshot {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut failing: Vec<FailingHook> = state
            .failures
            .iter()
            .filter(|(_, f)| f.consecutive >= FAILURE_THRESHOLD)
            .map(|(key, f)| FailingHook {
                watch: match &key.scope {
                    Scope::Watch(name) => Some(name.clone()),
                    Scope::Global => None,
                },
                event: key.event.clone(),
                command: key.command.clone(),
                consecutive: f.consecutive,
                last_error: f.last_error.clone(),
                // Set the moment the streak crossed the threshold, which every
                // `failing` entry has by definition; fall back defensively.
                since: f.since.unwrap_or_else(SystemTime::now),
            })
            .collect();
        failing.sort_by(|a, b| {
            (&a.watch, &a.event, &a.command).cmp(&(&b.watch, &b.event, &b.command))
        });
        RunnerSnapshot {
            suppressed_by_watch: state.suppressed_by_watch.clone(),
            failing,
        }
    }
}

impl Drop for HooksRunnerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Subscribes the runner to `events` (its own bus subscription, taken *before*
/// the engine starts so `daemon.started` is seen) and starts its task with
/// `config`. `carry`, when present, is the previous generation's [`Carryover`]:
/// its state resumes here, pruned to the keys and watches `config` still arms so a
/// removed or command-changed hook cannot fire a stale pending slot. The returned
/// handle's drop aborts the task.
pub(crate) fn spawn(
    events: EventReceiver,
    config: HooksConfig,
    carry: Option<Carryover>,
) -> HooksRunnerHandle {
    let mut state = carry.map(|c| c.state).unwrap_or_default();
    prune_to_config(&mut state, &config);
    let state = Arc::new(Mutex::new(state));
    let task = tokio::spawn(run(events, config, Arc::clone(&state)));
    HooksRunnerHandle { state, task }
}

/// Prunes carried state to what `config` still arms: a failure streak or pending
/// slot for a `(scope, event, command)` the new config dropped is discarded, and a
/// suppression total for a watch the new config no longer arms goes with it. A
/// single retain per collection — the keys are stable across generations, so a
/// surviving key keeps its state untouched.
fn prune_to_config(state: &mut RunnerState, config: &HooksConfig) {
    let valid: HashSet<HookKey> = config.hook_keys().collect();
    state.failures.retain(|key, _| valid.contains(key));
    state.limiter.retain(|key| valid.contains(key));
    let watches: HashSet<&str> = config.watches.keys().map(String::as_str).collect();
    state
        .suppressed_by_watch
        .retain(|watch, _| watches.contains(watch.as_str()));
}

/// The runner task: coalesce bus events through the limiter, fire cleared hooks
/// on blocking tasks, and advance the limiter as runs finish and cooldowns
/// elapse. Ends cleanly when the bus closes.
async fn run(mut events: EventReceiver, config: HooksConfig, state: Arc<Mutex<RunnerState>>) {
    // Tracks the key of every in-flight blocking task by its join id, so a task
    // that panics (a bare `JoinError` carries no payload, hence no key) can still
    // be attributed, recorded as a failure, and its key re-armed. Populated on
    // spawn, drained on completion (success, failure, or panic).
    let mut in_flight: HashMap<Id, HookKey> = HashMap::new();
    let mut joins: JoinSet<(HookKey, HookOutcome)> = JoinSet::new();
    loop {
        // The soonest a pending cooldown is due; `None` when no key is cooling.
        let sleep = {
            let st = state.lock().unwrap_or_else(|e| e.into_inner());
            st.limiter
                .next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
        };
        tokio::select! {
            received = events.recv() => match received {
                Ok(event) => {
                    if let Some((key, rate_limit, invocation)) = route(&config, &event) {
                        let fire = {
                            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                            match st.limiter.on_event(key.clone(), rate_limit, invocation, Instant::now()) {
                                Some(fire) => Some(fire),
                                None => {
                                    // Coalesced: bump the per-watch total (global
                                    // suppression is not tracked). Delayed, not lost.
                                    if let Scope::Watch(name) = &key.scope {
                                        *st.suppressed_by_watch.entry(name.clone()).or_insert(0) += 1;
                                    }
                                    None
                                }
                            }
                        };
                        if let Some(fire) = fire {
                            spawn_hook(&mut joins, &mut in_flight, key, fire);
                        }
                    } else {
                        // Hot path: the common no-hook event stays near-free at trace.
                        trace!(event = event.name(), "hooks runner: no configured hook for event");
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    // Coalescing makes lag benign: the next event of each key
                    // supersedes what was missed. Log and continue.
                    warn!(skipped, "hooks runner: event bus lagged; some hooks may be skipped");
                }
                Err(RecvError::Closed) => break,
            },
            Some(done) = joins.join_next_with_id(), if !joins.is_empty() => {
                let fire = match done {
                    Ok((id, (key, outcome))) => {
                        in_flight.remove(&id);
                        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                        record_outcome(&mut st, &key, &outcome);
                        st.limiter.on_finished(&key, Instant::now()).map(|f| (key, f))
                    }
                    Err(join_err) => {
                        // A hook task panicked (or was cancelled). Recover its key so
                        // the limiter re-arms — otherwise the key wedges in Running
                        // forever, silently dropping every future event for it.
                        match in_flight.remove(&join_err.id()) {
                            Some(key) => {
                                warn!(
                                    event = %key.event,
                                    scope = key.scope.label(),
                                    error = %join_err,
                                    "hooks runner: hook task failed to join; recording a failure and re-arming"
                                );
                                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                                record_outcome(
                                    &mut st,
                                    &key,
                                    &HookOutcome::Failure("hook task panicked".to_string()),
                                );
                                st.limiter.on_finished(&key, Instant::now()).map(|f| (key, f))
                            }
                            None => {
                                warn!(error = %join_err, "hooks runner: a hook task failed to join with no tracked key");
                                None
                            }
                        }
                    }
                };
                if let Some((key, fire)) = fire {
                    spawn_hook(&mut joins, &mut in_flight, key, fire);
                }
            }
            _ = tokio::time::sleep(sleep.unwrap_or_default()), if sleep.is_some() => {
                let fired = {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    st.limiter.poll(Instant::now())
                };
                for (key, fire) in fired {
                    spawn_hook(&mut joins, &mut in_flight, key, fire);
                }
            }
        }
    }
}

/// Dispatches a cleared hook onto a blocking task, tagged with its key so the
/// runner can advance the limiter when it completes. The task's join id is
/// recorded in `in_flight` so a panic (whose `JoinError` carries no payload) is
/// still attributable to its key.
fn spawn_hook(
    joins: &mut JoinSet<(HookKey, HookOutcome)>,
    in_flight: &mut HashMap<Id, HookKey>,
    key: HookKey,
    fire: Fire<HookInvocation>,
) {
    let invocation = fire.payload;
    let suppressed = fire.suppressed;
    let event = key.event.clone();
    let scope = key.scope.label().to_string();
    let task_key = key.clone();
    let handle = joins.spawn_blocking(move || {
        let outcome = exec_hook(&event, &scope, &invocation, suppressed);
        (task_key, outcome)
    });
    in_flight.insert(handle.id(), key);
}

/// Folds one run's outcome into the key's consecutive-failure tally: a failure
/// increments and records its reason, a success clears the key.
fn record_outcome(state: &mut RunnerState, key: &HookKey, outcome: &HookOutcome) {
    match outcome {
        HookOutcome::Success => {
            state.failures.remove(key);
        }
        HookOutcome::Failure(error) => {
            let failure = state.failures.entry(key.clone()).or_insert(Failure {
                consecutive: 0,
                last_error: String::new(),
                since: None,
            });
            failure.consecutive += 1;
            failure.last_error = error.clone();
            // Stamp `since` the instant the streak first reaches the reporting
            // threshold, and never again — so the projected problem's age stays
            // honest across heartbeats and reloads.
            if failure.since.is_none() && failure.consecutive >= FAILURE_THRESHOLD {
                failure.since = Some(SystemTime::now());
            }
        }
    }
}

/// Resolves the single hook (if any) an event fires: its key, its scope's rate
/// limit, and the invocation to run. Watch-scoped events consult the watch's map;
/// daemon-level events the global map — the two are disjoint by scope.
fn route(config: &HooksConfig, event: &Event) -> Option<(HookKey, Duration, HookInvocation)> {
    let name = event.name();
    match event_watch(event) {
        Some(watch) => {
            let scope = config.watches.get(watch)?;
            let command = scope.hooks.get(name)?;
            Some((
                HookKey {
                    scope: Scope::Watch(watch.to_string()),
                    event: name.to_string(),
                    command: command.clone(),
                },
                scope.rate_limit,
                HookInvocation {
                    command: command.clone(),
                    cwd: scope.cwd.clone(),
                    timeout: scope.timeout,
                    env: hook_env(event, Some(&scope.cwd)),
                },
            ))
        }
        None => {
            let command = config.global.hooks.get(name)?;
            Some((
                HookKey {
                    scope: Scope::Global,
                    event: name.to_string(),
                    command: command.clone(),
                },
                config.global.rate_limit,
                HookInvocation {
                    command: command.clone(),
                    cwd: config.global.cwd.clone(),
                    timeout: config.global.timeout,
                    env: hook_env(event, None),
                },
            ))
        }
    }
}

/// The watch an event carries, or `None` for a daemon-level event. Kept
/// wildcard-guarded (`Event` is `#[non_exhaustive]`) so a new variant defaults to
/// daemon-level until routed deliberately.
fn event_watch(event: &Event) -> Option<&str> {
    match event {
        Event::SnapshotStarted { watch, .. }
        | Event::SnapshotCompleted { watch, .. }
        | Event::SnapshotFailed { watch, .. }
        | Event::SnapshotSkipped { watch, .. }
        | Event::SnapshotQuarantined { watch, .. }
        | Event::SyncPushed { watch, .. }
        | Event::SyncPulled { watch, .. }
        | Event::SyncConflict { watch, .. }
        | Event::SyncResolved { watch, .. }
        | Event::SyncFailed { watch, .. }
        | Event::SyncSkipped { watch, .. }
        | Event::RestoreCompleted { watch, .. }
        | Event::WatchStateChanged { watch, .. } => Some(watch),
        Event::DaemonStarted | Event::DaemonStopped | Event::UpdateAvailable { .. } => None,
        _ => None,
    }
}

/// Builds the enumerated `VARD_*` environment for an event (all but
/// `VARD_SUPPRESSED`, which the runner adds per fire). `watch_path` is the watch's
/// directory for watch-scoped events. Only the variables an event actually
/// carries are set — an unset one is absent, never empty.
fn hook_env(event: &Event, watch_path: Option<&Path>) -> Vec<(String, String)> {
    let mut env = vec![("VARD_EVENT".to_string(), event.name().to_string())];
    if let Some(watch) = event_watch(event) {
        env.push(("VARD_WATCH".to_string(), watch.to_string()));
        if let Some(path) = watch_path {
            env.push(("VARD_PATH".to_string(), path.to_string_lossy().into_owned()));
        }
    }
    match event {
        Event::SnapshotCompleted {
            snapshot,
            files_changed,
            ..
        } => {
            env.push(("VARD_REF".to_string(), snapshot.clone()));
            env.push(("VARD_FILES_CHANGED".to_string(), files_changed.to_string()));
        }
        Event::SnapshotFailed { error, .. } => {
            env.push(("VARD_ERROR".to_string(), error.clone()));
        }
        Event::SnapshotQuarantined { count, .. } => {
            // Count-only per the VRD-21/VRD-22 contract: never the paths or any
            // secret bytes.
            env.push(("VARD_QUARANTINED_COUNT".to_string(), count.to_string()));
        }
        Event::SyncPushed { new_ref, .. } => {
            env.push(("VARD_REF".to_string(), new_ref.clone()));
        }
        Event::SyncPulled {
            prev_ref, new_ref, ..
        } => {
            env.push(("VARD_REF".to_string(), new_ref.clone()));
            env.push(("VARD_PREV_REF".to_string(), prev_ref.clone()));
        }
        Event::SyncFailed { error, .. } => {
            env.push(("VARD_ERROR".to_string(), error.clone()));
        }
        Event::RestoreCompleted {
            restored_to,
            prev_ref,
            ..
        } => {
            env.push(("VARD_REF".to_string(), restored_to.clone()));
            env.push(("VARD_PREV_REF".to_string(), prev_ref.clone()));
        }
        // Everything else (UpdateAvailable, the daemon lifecycle events, and the
        // watch events with no extra scalar payload) sets only the always/watch
        // variables above.
        _ => {}
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use vard_core::{EventBus, Trigger};

    fn find<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    // --- env contract --------------------------------------------------------

    #[test]
    fn snapshot_completed_sets_watch_ref_and_files_changed() {
        let event = Event::SnapshotCompleted {
            watch: "notes".to_string(),
            snapshot: "abc123".to_string(),
            files_changed: 7,
            trigger: Trigger::Event,
        };
        let env = hook_env(&event, Some(Path::new("/home/u/notes")));
        assert_eq!(find(&env, "VARD_EVENT"), Some("snapshot.completed"));
        assert_eq!(find(&env, "VARD_WATCH"), Some("notes"));
        assert_eq!(find(&env, "VARD_PATH"), Some("/home/u/notes"));
        assert_eq!(find(&env, "VARD_REF"), Some("abc123"));
        assert_eq!(find(&env, "VARD_FILES_CHANGED"), Some("7"));
        // No error/prev-ref for a completed snapshot.
        assert_eq!(find(&env, "VARD_ERROR"), None);
        assert_eq!(find(&env, "VARD_PREV_REF"), None);
        // VARD_SUPPRESSED is added by the runner per fire, not here.
        assert_eq!(find(&env, "VARD_SUPPRESSED"), None);
    }

    #[test]
    fn sync_pulled_sets_both_refs() {
        let event = Event::SyncPulled {
            watch: "notes".to_string(),
            prev_ref: "old".to_string(),
            new_ref: "new".to_string(),
        };
        let env = hook_env(&event, Some(Path::new("/w")));
        assert_eq!(find(&env, "VARD_REF"), Some("new"));
        assert_eq!(find(&env, "VARD_PREV_REF"), Some("old"));
    }

    #[test]
    fn snapshot_failed_sets_error() {
        let event = Event::SnapshotFailed {
            watch: "notes".to_string(),
            trigger: Trigger::Interval,
            error: "boom".to_string(),
        };
        let env = hook_env(&event, Some(Path::new("/w")));
        assert_eq!(find(&env, "VARD_ERROR"), Some("boom"));
        assert_eq!(find(&env, "VARD_REF"), None);
    }

    #[test]
    fn snapshot_quarantined_sets_only_the_count() {
        // VRD-22: the quarantine hook is count-only — never the paths or bytes.
        let event = Event::SnapshotQuarantined {
            watch: "notes".to_string(),
            count: 3,
        };
        let env = hook_env(&event, Some(Path::new("/home/u/notes")));
        assert_eq!(find(&env, "VARD_EVENT"), Some("snapshot.quarantined"));
        assert_eq!(find(&env, "VARD_WATCH"), Some("notes"));
        assert_eq!(find(&env, "VARD_PATH"), Some("/home/u/notes"));
        assert_eq!(find(&env, "VARD_QUARANTINED_COUNT"), Some("3"));
        // No ref/error/files-changed leak into the quarantine env.
        assert_eq!(find(&env, "VARD_REF"), None);
        assert_eq!(find(&env, "VARD_ERROR"), None);
        assert_eq!(find(&env, "VARD_FILES_CHANGED"), None);
    }

    #[test]
    fn a_daemon_event_carries_no_watch_variables() {
        let env = hook_env(&Event::DaemonStarted, None);
        assert_eq!(find(&env, "VARD_EVENT"), Some("daemon.started"));
        assert_eq!(find(&env, "VARD_WATCH"), None);
        assert_eq!(find(&env, "VARD_PATH"), None);
    }

    #[test]
    fn update_available_sets_only_the_always_variables() {
        // Deliberate per the checkpoint spec: UpdateAvailable gets no extra vars.
        let env = hook_env(
            &Event::UpdateAvailable {
                version: "1.2.3".to_string(),
            },
            None,
        );
        assert_eq!(find(&env, "VARD_EVENT"), Some("update.available"));
        assert_eq!(env.len(), 1, "only VARD_EVENT is set: {env:?}");
    }

    // --- routing -------------------------------------------------------------

    fn config_with(watch: &str, event: &str, command: &str) -> HooksConfig {
        let mut hooks = HookMap::new();
        hooks.insert(event.to_string(), command.to_string());
        HooksConfig::build(
            HookMap::new(),
            Duration::from_secs(60),
            Duration::from_secs(300),
            vec![WatchHooks {
                name: watch.to_string(),
                path: PathBuf::from("/w"),
                hooks,
                timeout: Duration::from_secs(30),
                rate_limit: Duration::from_secs(120),
            }],
        )
        .expect("hooks configured")
    }

    #[test]
    fn build_returns_none_when_nothing_is_configured() {
        let config = HooksConfig::build(
            HookMap::new(),
            Duration::from_secs(60),
            Duration::from_secs(300),
            vec![WatchHooks {
                name: "empty".to_string(),
                path: PathBuf::from("/w"),
                hooks: HookMap::new(),
                timeout: Duration::from_secs(30),
                rate_limit: Duration::from_secs(120),
            }],
        );
        assert!(config.is_none(), "no hooks anywhere means no runner");
    }

    #[test]
    fn a_matching_watch_event_routes_to_its_command_and_rate_limit() {
        let config = config_with("notes", "snapshot.completed", "echo hi");
        let event = Event::SnapshotCompleted {
            watch: "notes".to_string(),
            snapshot: "r".to_string(),
            files_changed: 0,
            trigger: Trigger::Event,
        };
        let (key, rate, inv) = route(&config, &event).expect("routes");
        assert_eq!(key.scope, Scope::Watch("notes".to_string()));
        assert_eq!(key.event, "snapshot.completed");
        assert_eq!(inv.command, "echo hi");
        assert_eq!(rate, Duration::from_secs(120), "the watch's rate limit");
        assert_eq!(inv.timeout, Duration::from_secs(30), "the watch's timeout");
    }

    #[test]
    fn an_event_with_no_matching_hook_does_not_route() {
        let config = config_with("notes", "snapshot.completed", "echo hi");
        // A different event on the same watch: no hook.
        let event = Event::SnapshotFailed {
            watch: "notes".to_string(),
            trigger: Trigger::Event,
            error: "e".to_string(),
        };
        assert!(route(&config, &event).is_none());
        // A matching event on an unconfigured watch: no hook.
        let other = Event::SnapshotCompleted {
            watch: "other".to_string(),
            snapshot: "r".to_string(),
            files_changed: 0,
            trigger: Trigger::Event,
        };
        assert!(route(&config, &other).is_none());
    }

    #[test]
    fn a_daemon_event_routes_to_the_global_scope() {
        let mut global = HookMap::new();
        global.insert("daemon.started".to_string(), "echo up".to_string());
        let config = HooksConfig::build(
            global,
            Duration::from_secs(60),
            Duration::from_secs(300),
            std::iter::empty(),
        )
        .expect("global hooks configured");
        let (key, rate, inv) = route(&config, &Event::DaemonStarted).expect("routes");
        assert_eq!(key.scope, Scope::Global);
        assert_eq!(inv.command, "echo up");
        assert_eq!(rate, Duration::from_secs(300), "the global rate limit");
    }

    #[test]
    fn event_watch_partitions_the_whole_event_catalog_by_scope() {
        // One instance of every event, split the way the config catalog splits
        // them: every WATCH_HOOK_EVENTS event must carry a watch (so `route` can
        // reach a `[watch.hooks]` map), and every DAEMON_HOOK_EVENTS event must
        // not. A new allowlisted event whose `event_watch` arm was forgotten
        // (defaulting to daemon-level) then fails here instead of misrouting live.
        use vard_core::{Resolver, SkipReason, WatchState};

        let watch_events = [
            Event::SnapshotStarted {
                watch: "w".to_string(),
                trigger: Trigger::Manual,
            },
            Event::SnapshotCompleted {
                watch: "w".to_string(),
                snapshot: "r".to_string(),
                files_changed: 0,
                trigger: Trigger::Manual,
            },
            Event::SnapshotFailed {
                watch: "w".to_string(),
                trigger: Trigger::Manual,
                error: "e".to_string(),
            },
            Event::SnapshotSkipped {
                watch: "w".to_string(),
                trigger: Trigger::Manual,
                reason: SkipReason::Clean,
            },
            Event::SnapshotQuarantined {
                watch: "w".to_string(),
                count: 1,
            },
            Event::SyncPushed {
                watch: "w".to_string(),
                new_ref: "r".to_string(),
                commits: 0,
            },
            Event::SyncPulled {
                watch: "w".to_string(),
                prev_ref: "a".to_string(),
                new_ref: "b".to_string(),
            },
            Event::SyncConflict {
                watch: "w".to_string(),
            },
            Event::SyncResolved {
                watch: "w".to_string(),
                resolver: Resolver::Human,
            },
            Event::SyncFailed {
                watch: "w".to_string(),
                error: "e".to_string(),
            },
            Event::SyncSkipped {
                watch: "w".to_string(),
                reason: "r".to_string(),
            },
            Event::RestoreCompleted {
                watch: "w".to_string(),
                restored_to: "a".to_string(),
                prev_ref: "b".to_string(),
            },
            Event::WatchStateChanged {
                watch: "w".to_string(),
                from: WatchState::Ok,
                to: WatchState::Paused,
                reason: None,
                trouble: None,
            },
        ];
        let daemon_events = [
            Event::DaemonStarted,
            Event::DaemonStopped,
            Event::UpdateAvailable {
                version: "1.0.0".to_string(),
            },
        ];

        assert_eq!(
            watch_events.len(),
            crate::config::WATCH_HOOK_EVENTS.len(),
            "one watch-event instance per WATCH_HOOK_EVENTS entry"
        );
        assert_eq!(
            daemon_events.len(),
            crate::config::DAEMON_HOOK_EVENTS.len(),
            "one daemon-event instance per DAEMON_HOOK_EVENTS entry"
        );
        for event in &watch_events {
            assert_eq!(
                event_watch(event),
                Some("w"),
                "{} must route watch-scoped",
                event.name()
            );
        }
        for event in &daemon_events {
            assert_eq!(
                event_watch(event),
                None,
                "{} must route daemon-scoped",
                event.name()
            );
        }
    }

    // --- runner wiring -------------------------------------------------------

    /// Waits until `path` exists (a hook side effect), or panics. Real time, tiny
    /// budget; the hooks under test complete in single-digit milliseconds.
    async fn wait_for(path: &Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("hook side effect {} never appeared", path.display());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_idle_event_fires_its_hook_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("fired");
        let mut hooks = HookMap::new();
        hooks.insert(
            "snapshot.completed".to_string(),
            format!("touch {}", marker.display()),
        );
        let config = HooksConfig::build(
            HookMap::new(),
            Duration::from_secs(60),
            Duration::from_secs(300),
            vec![WatchHooks {
                name: "notes".to_string(),
                path: dir.path().to_path_buf(),
                hooks,
                timeout: Duration::from_secs(5),
                rate_limit: Duration::from_secs(300),
            }],
        )
        .unwrap();

        let bus = EventBus::default();
        let runner = spawn(bus.subscribe(), config, None);
        bus.emit(Event::SnapshotCompleted {
            watch: "notes".to_string(),
            snapshot: "r".to_string(),
            files_changed: 1,
            trigger: Trigger::Event,
        });
        wait_for(&marker).await;
        drop(runner);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_hook_reaches_the_failure_threshold_and_a_success_clears_it() {
        // A tiny rate limit so coalesced retries fire quickly; each run appends to
        // a counter file so the command's exit can be flipped from the test.
        let dir = tempfile::tempdir().unwrap();
        let toggle = dir.path().join("ok");
        // Fails (exit 1) until `toggle` exists, then succeeds.
        let command = format!("test -f {}", toggle.display());
        let mut hooks = HookMap::new();
        hooks.insert("snapshot.completed".to_string(), command);
        let config = HooksConfig::build(
            HookMap::new(),
            Duration::from_secs(60),
            Duration::from_secs(300),
            vec![WatchHooks {
                name: "notes".to_string(),
                path: dir.path().to_path_buf(),
                hooks,
                timeout: Duration::from_secs(5),
                rate_limit: Duration::from_millis(10),
            }],
        )
        .unwrap();

        let bus = EventBus::default();
        let runner = spawn(bus.subscribe(), config, None);
        let emit = || {
            bus.emit(Event::SnapshotCompleted {
                watch: "notes".to_string(),
                snapshot: "r".to_string(),
                files_changed: 0,
                trigger: Trigger::Event,
            });
        };

        // Drive at least FAILURE_THRESHOLD failing runs. Each event either runs
        // (idle) or coalesces into the pending run; the tiny rate limit means the
        // trailing runs fire promptly.
        for _ in 0..12 {
            emit();
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let failing = loop_until(&runner, |snap| {
            snap.failing
                .iter()
                .any(|f| f.watch.as_deref() == Some("notes") && f.consecutive >= FAILURE_THRESHOLD)
        })
        .await;
        assert!(failing, "the hook must reach the failure threshold");

        // Flip the command to succeed and drive one more run: the key clears.
        std::fs::write(&toggle, b"").unwrap();
        for _ in 0..12 {
            emit();
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let cleared = loop_until(&runner, |snap| snap.failing.is_empty()).await;
        assert!(cleared, "a successful run must clear the failure state");
        drop(runner);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lagging_bus_is_logged_and_the_runner_keeps_going() {
        // A capacity-2 bus overflowed before the runner drains it must not panic
        // the runner; the trailing event still fires its hook.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("fired");
        let mut hooks = HookMap::new();
        hooks.insert(
            "snapshot.completed".to_string(),
            format!("touch {}", marker.display()),
        );
        let config = HooksConfig::build(
            HookMap::new(),
            Duration::from_secs(60),
            Duration::from_secs(300),
            vec![WatchHooks {
                name: "notes".to_string(),
                path: dir.path().to_path_buf(),
                hooks,
                timeout: Duration::from_secs(5),
                rate_limit: Duration::from_millis(10),
            }],
        )
        .unwrap();

        let bus = EventBus::new(2);
        let runner = spawn(bus.subscribe(), config, None);
        // Flood well past capacity so the subscriber lags, then let it catch up.
        for i in 0..50 {
            bus.emit(Event::SnapshotCompleted {
                watch: "notes".to_string(),
                snapshot: format!("r{i}"),
                files_changed: 0,
                trigger: Trigger::Event,
            });
        }
        // The runner logged the lag and continued; the last event's hook still
        // runs (immediately if it caught the tail idle, or as the trailing run).
        wait_for(&marker).await;
        drop(runner);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn carryover_of_a_running_key_with_a_pending_fires_it_and_does_not_wedge() {
        // A reload that lands while a hook is mid-run must not wedge the key. The
        // hook touches a per-`VARD_SUPPRESSED` marker immediately, then holds the
        // run open (sleep) so the key is still `Running` at carryover: marker.0 is
        // the first run, marker.1 is the coalesced trailing run the successor must
        // fire once the cooldown window opens.
        let dir = tempfile::tempdir().unwrap();
        let command = format!(
            "touch {d}/marker.$VARD_SUPPRESSED; sleep 2",
            d = dir.path().display()
        );
        let build = || {
            let mut hooks = HookMap::new();
            hooks.insert("snapshot.completed".to_string(), command.clone());
            HooksConfig::build(
                HookMap::new(),
                Duration::from_secs(60),
                Duration::from_secs(300),
                vec![WatchHooks {
                    name: "notes".to_string(),
                    path: dir.path().to_path_buf(),
                    hooks,
                    timeout: Duration::from_secs(5),
                    rate_limit: Duration::from_millis(50),
                }],
            )
            .unwrap()
        };

        let bus = EventBus::default();
        let runner = spawn(bus.subscribe(), build(), None);
        let emit = || {
            bus.emit(Event::SnapshotCompleted {
                watch: "notes".to_string(),
                snapshot: "r".to_string(),
                files_changed: 0,
                trigger: Trigger::Event,
            });
        };

        // First event fires immediately: the run touches marker.0 then sleeps, so
        // the key sits in `Running`.
        emit();
        wait_for(&dir.path().join("marker.0")).await;
        // A second event lands mid-run and coalesces into the pending slot.
        emit();
        let coalesced = loop_until(&runner, |snap| {
            snap.suppressed_by_watch.get("notes").copied().unwrap_or(0) >= 1
        })
        .await;
        assert!(
            coalesced,
            "the second event must coalesce while the run is in flight"
        );

        // Hand the state to a successor and abort the old runner. Its in-flight
        // sleep keeps running on the blocking pool (and never re-fires the pending,
        // since its `on_finished` is unreachable), so marker.1 can only come from
        // the successor.
        let carry = runner.carryover();
        drop(runner);
        let runner2 = spawn(bus.subscribe(), build(), Some(carry));

        // The successor fires the carried pending at the cooldown edge — the bug
        // wedged the key in `Running` forever, so marker.1 would never appear.
        wait_for(&dir.path().join("marker.1")).await;
        drop(runner2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_runner_drains_promptly_when_the_bus_closes_at_shutdown() {
        // At true shutdown the engine emits daemon.stopped and drops the bus. The
        // runner loop must end on that clean close so `drain` returns well inside
        // its budget rather than spending the whole timeout and aborting.
        let mut global = HookMap::new();
        global.insert("daemon.started".to_string(), "true".to_string());
        let config = HooksConfig::build(
            global,
            Duration::from_secs(60),
            Duration::from_secs(300),
            std::iter::empty(),
        )
        .unwrap();

        let bus = EventBus::default();
        let runner = spawn(bus.subscribe(), config, None);
        // Close the bus, as `EngineHandle::shutdown` does after the final event.
        drop(bus);
        let start = Instant::now();
        runner.drain(Duration::from_secs(2)).await;
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the runner must end cleanly on bus close, took {:?}",
            start.elapsed()
        );
    }

    /// Polls the runner's snapshot until `pred` holds or a short budget elapses.
    async fn loop_until(
        runner: &HooksRunnerHandle,
        pred: impl Fn(&RunnerSnapshot) -> bool,
    ) -> bool {
        for _ in 0..200 {
            if pred(&runner.snapshot()) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }
}
