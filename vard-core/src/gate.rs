//! The operation gate: the injected seam that makes "one writer per watch" a
//! structural invariant rather than a convention.
//!
//! # Why a gate
//!
//! Every mutating operation on a watch (a snapshot commit, a restore) must be
//! the *only* one touching that watch's repository and its durable operation
//! record at a time. The engine already serializes a single watch's own passes
//! (one worker task), but a watch can be mutated from *outside* the engine too —
//! a second engine armed briefly during a daemon reload, or a CLI `restore`
//! running while the daemon owns the repository. The gate is the one seam all of
//! those pass through, so the invariant is enforced by an RAII guard, not by
//! everyone remembering to cooperate.
//!
//! # The shape (mirrors the [`VcsBackend`](crate::VcsBackend) seam)
//!
//! [`OpGate`] is a small, dyn-compatible, synchronous trait a host implements.
//! [`begin`](OpGate::begin) tries to admit an operation: on success it returns an
//! [`OpGuard`] whose *creation already recorded the operation's start* (the host
//! acquired its per-watch lock and wrote its journal `begin`), and whose
//! [`complete`](OpGuard::complete) records the clean close and releases the lock.
//! On contention it returns [`None`] (busy) — another holder owns the watch right
//! now — which the engine treats exactly like a contended index lock: requeue and
//! retry on the next trigger.
//!
//! # The drop-without-complete contract (load-bearing)
//!
//! Dropping an [`OpGuard`] **without** calling [`complete`](OpGuard::complete) is
//! *release-only*: it releases the lock but MUST NOT record a clean close. An
//! operation that unwound before completing (a panicked backend call, a killed
//! process) may have left a git `index.lock` behind, and the host's dangling
//! `begin` record is the *only* evidence that lets recovery prove that lock stale
//! and ours. Writing a close on drop would destroy that evidence and wedge the
//! repository. So the guard's `Drop` releases the lock and nothing else; only an
//! explicit [`complete`](OpGuard::complete) records the close.
//!
//! # The standalone default
//!
//! `vard-core` ships [`NoOpGate`], which admits every operation and whose guard
//! does nothing. It is the default for a watch added without an injected gate, so
//! the embeddable engine (and its tests) run standalone with no host lock or
//! journal. The `vard` binary injects an op-lock-backed gate for the daemon and
//! CLI paths.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// A shared, per-watch operation gate. `Arc<dyn OpGate>` so the same gate value
/// can be cloned into a worker task; the trait is `Send + Sync` because the
/// worker holds it across `.await` points.
pub type SharedGate = Arc<dyn OpGate>;

/// A per-watch gate that admits at most one mutating operation at a time.
///
/// Synchronous and dyn-compatible, like [`VcsBackend`](crate::VcsBackend): the
/// engine holds one `Arc<dyn OpGate>` per watch and calls
/// [`begin`](Self::begin) from its worker before every commit. `begin`'s work is
/// a non-blocking lock attempt plus a small record write, so it is called
/// directly from async context without a `spawn_blocking` hop (it must not
/// block the runtime — a busy gate returns [`None`] immediately rather than
/// waiting). [`OpGuard::complete`] is likewise called directly from async: its
/// work is a small journal *compaction* (a file truncate) plus the lock release,
/// both cheap synchronous I/O in the same class as `begin`'s record write, never
/// a blocking wait. The one heavy step — the git commit between them — is the
/// only part the engine hands to `spawn_blocking` (see the engine's
/// `run_snapshot_under_guard`, which keeps the guard coupled to that blocking
/// work so an async abort cannot release the lock while git is mid-write).
pub trait OpGate: Send + Sync {
    /// Tries to begin operation `op` (e.g. `"snapshot"`) for this watch:
    ///
    /// - `Ok(Some(guard))` — admitted. The guard's creation already recorded the
    ///   operation's start; the caller holds the exclusive right to mutate this
    ///   watch until the guard is [`complete`](OpGuard::complete)d or dropped.
    /// - `Ok(None)` — busy. Another holder owns this watch right now; the caller
    ///   should requeue and retry later (the engine treats this like a contended
    ///   index lock).
    /// - `Err(_)` — the gate could not be evaluated (host I/O trouble).
    fn begin(&self, op: &str) -> Result<Option<Box<dyn OpGuard>>, OpGateError>;

    /// A cheap, side-effect-free admission probe: whether a [`begin`](Self::begin)
    /// issued right now would likely be admitted (`false` means a live holder
    /// owns the watch). **Advisory only** — a holder can arrive between the probe
    /// and the real `begin`, which then simply reports busy; callers must never
    /// treat `true` as a held lock. The engine's sync cycle probes this *before*
    /// its network fetch so a busy gate defers pre-network instead of paying for
    /// a fetch it cannot use. Implementations should try-acquire and immediately
    /// release their lock **without** recording anything (no journal write). The
    /// default is optimistic (`true`), correct for gates with no contention
    /// concept ([`NoOpGate`]); a probe that cannot be evaluated should also
    /// return `true` and let `begin` surface the real error.
    fn available(&self) -> bool {
        true
    }
}

/// The RAII half of [`OpGate`]: a live operation admitted by the gate.
///
/// Created = the host's per-watch lock is held and its operation start is
/// recorded. [`complete`](Self::complete) records the clean close and releases;
/// a plain `Drop` releases WITHOUT recording a close — deliberately leaving the
/// start record as recovery evidence for an unwound operation (see the [module
/// docs](self)). `Send` because the engine holds it across the backend commit's
/// `.await`.
pub trait OpGuard: Send {
    /// Records the operation's clean completion (the host compacts its journal)
    /// and releases the lock. Consumes the guard. Not calling this — dropping the
    /// guard instead — is the release-only path that preserves recovery evidence.
    ///
    /// A crashed operation leaves only the `begin` record as evidence; recovery
    /// is never surgery on the user's files. A dangling **sync** record's cleanup
    /// prunes the vard-owned scratch worktree and nothing else — the crashed
    /// tree is fully committed at worst mid-checkout, and the next sync cycle
    /// self-heals (dirty check → pre-sync snapshot → fresh reconcile → advance).
    fn complete(self: Box<Self>);
}

/// The default gate: admits every operation, records nothing, locks nothing. The
/// standalone SDK default (see the [module docs](self)).
pub struct NoOpGate;

impl OpGate for NoOpGate {
    fn begin(&self, _op: &str) -> Result<Option<Box<dyn OpGuard>>, OpGateError> {
        Ok(Some(Box::new(NoOpGuard)))
    }
}

/// The guard [`NoOpGate`] hands out: completing or dropping it both do nothing.
struct NoOpGuard;

impl OpGuard for NoOpGuard {
    fn complete(self: Box<Self>) {}
}

/// A [`SharedGate`] wrapping the [`NoOpGate`] default.
pub(crate) fn default_gate() -> SharedGate {
    Arc::new(NoOpGate)
}

/// A host-side failure evaluating an [`OpGate`]. Opaque: it carries a
/// human-readable description a host formatted from its own error (an op-lock or
/// journal I/O failure), keeping the trait free of any host error type.
#[derive(Debug)]
pub struct OpGateError {
    detail: String,
}

impl OpGateError {
    /// Wraps a human-readable failure description.
    pub fn new(detail: impl Into<String>) -> OpGateError {
        OpGateError {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for OpGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl Error for OpGateError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_op_gate_admits_and_its_guard_completes() {
        let gate = NoOpGate;
        let guard = gate.begin("snapshot").unwrap();
        assert!(guard.is_some(), "the no-op gate always admits");
        guard.unwrap().complete();
    }

    #[test]
    fn op_gate_is_dyn_compatible() {
        fn _takes_dyn(_: &dyn OpGate) {}
        let _shared: SharedGate = default_gate();
    }
}
