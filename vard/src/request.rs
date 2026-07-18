//! The request-file schema shared by the CLI (the writer) and the daemon (the
//! reader), so the two agree on one serde-owned contract instead of a hand-built
//! TOML string on one side and a parser on the other.
//!
//! # The contract (ADR 0004, spec §11)
//!
//! The CLI and daemon do not share memory; they rendezvous through files under
//! [`paths::request_dir`](crate::paths::request_dir). Each file is a small TOML
//! document:
//!
//! ```toml
//! kind = "snapshot"          # or "sync"
//! watch = "vault"            # optional; omitted means "every watch"
//! requested_at = 1752000000  # unix seconds, stamped by the writer
//! ```
//!
//! `requested_at` lets the daemon discard a request that has been sitting in the
//! queue too long — a machine asleep for hours should not wake to a burst of
//! stale manual snapshots (see [`STALE_AFTER`]).
//!
//! Writers create a request atomically (via [`write()`](fn@write)): serialize to a temp file in
//! the same directory and `rename(2)` it to its final `*.toml` name. The daemon
//! consumes only *settled* names, so a request mid-write is never half-read.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A request older than this when the daemon drains it is discarded with a
/// warning rather than applied: a stale manual snapshot from hours ago is not
/// what the user wants acted on now. Matches the journal's 15-minute
/// operation-window rationale ([`MAX_OP_WINDOW`](crate::journal::MAX_OP_WINDOW)).
///
/// This gate is about the *request file's* age and is orthogonal to the per-watch
/// operation lock (VRD-37): a fresh request is merely injected as a
/// `Trigger::Manual`, and it is the engine worker — not this drain — that later
/// takes the op lock when it actually snapshots. So the op lock changes nothing
/// about this gate's soundness.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// A request file: what operation, for which watch, and when it was queued.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    /// The operation requested.
    pub kind: RequestKind,
    /// The target watch; `None` (the key omitted) means every watch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<String>,
    /// When the request was written, in unix seconds — the daemon's staleness
    /// gate reads this. Required: a request without it is malformed.
    pub requested_at: u64,
}

/// The operations a request file can name.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RequestKind {
    /// Take a snapshot now.
    Snapshot,
    /// Sync with the remote (not yet implemented).
    Sync,
}

impl Request {
    /// A snapshot request for `watch` (or every watch when `None`), stamped
    /// `requested_at = now`.
    pub(crate) fn snapshot(watch: Option<String>) -> Request {
        Request {
            kind: RequestKind::Snapshot,
            watch,
            requested_at: now_epoch_secs(),
        }
    }

    /// A sync request for `watch` (or every sync-enabled watch when `None`),
    /// stamped `requested_at = now`.
    pub(crate) fn sync(watch: Option<String>) -> Request {
        Request {
            kind: RequestKind::Sync,
            watch,
            requested_at: now_epoch_secs(),
        }
    }

    /// How long ago this request was written, relative to `now`. Zero when the
    /// stamp is in the future (a clock skew), so a skewed request is treated as
    /// fresh rather than as impossibly old. A stamp too large to represent as a
    /// `SystemTime` (corruption or tampering — it still parses as a valid
    /// `u64`, so the poison path never sees it) reports `Duration::MAX` so the
    /// staleness gate discards it instead of panicking the daemon.
    pub(crate) fn age(&self, now: SystemTime) -> Duration {
        let Some(stamped) = UNIX_EPOCH.checked_add(Duration::from_secs(self.requested_at)) else {
            return Duration::MAX;
        };
        now.duration_since(stamped).unwrap_or(Duration::ZERO)
    }
}

/// Whether `name` is a *settled* request file the daemon may consume: a plain
/// `*.toml` name that is not hidden (no leading dot). Writers create requests by
/// writing to a temp or dot name in the same directory and `rename(2)`-ing to the
/// final `*.toml` name (atomic on POSIX; see the [module docs](self)), so a name
/// matching this predicate is always a complete file. Temp suffixes (`.tmp`,
/// `.partial`, anything else) fail the `*.toml` requirement.
///
/// This is the naming policy shared by the two readers of the request dir: the
/// daemon's drain (which consumes settled files and leaves unsettled ones "not
/// ours to touch") and `vard doctor` (which conversely flags *unsettled*
/// leftovers a crashed writer stranded). Keeping the predicate here — one source
/// of truth — stops the two from disagreeing on what a settled name is.
pub(crate) fn is_settled_request_name(name: &str) -> bool {
    name.ends_with(".toml") && !name.starts_with('.')
}

/// Parses a request file's TOML text, returning a human-readable error string on
/// any malformation (missing/unknown `kind`, a missing `requested_at`, unknown
/// fields, or invalid TOML) so a poison file can be logged and dropped.
pub(crate) fn parse(text: &str) -> Result<Request, String> {
    toml::from_str(text).map_err(|err| err.to_string())
}

/// Serializes `request` and writes it as a settled `*.toml` file in
/// `request_dir`, atomically (temp + `rename`, via [`atomic`](crate::atomic)),
/// so the daemon only ever reads a complete file. The filename is unique per
/// writer and instant. Returns a human-readable error on any failure.
pub(crate) fn write(request_dir: &Path, request: &Request) -> Result<(), String> {
    let text = toml::to_string(request).map_err(|e| format!("encoding request: {e}"))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let settled = request_dir.join(format!("req-{pid}-{nanos}.toml"));
    crate::atomic::write(&settled, text.as_bytes())
        .map_err(|e| format!("writing request file: {e}"))
}

/// The current unix time in whole seconds, or 0 before the epoch (impossible in
/// practice).
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_toml_names_are_settled() {
        assert!(is_settled_request_name("req.toml"));
        assert!(is_settled_request_name("snapshot-1234.toml"));
        // Hidden files are a writer's staging name, never consumed.
        assert!(!is_settled_request_name(".req.toml"));
        assert!(!is_settled_request_name(".hidden"));
        // Temp suffixes and non-toml names are not settled.
        assert!(!is_settled_request_name("req.toml.tmp"));
        assert!(!is_settled_request_name("req.toml.partial"));
        assert!(!is_settled_request_name("req.tmp"));
        assert!(!is_settled_request_name("notes.txt"));
        assert!(!is_settled_request_name("toml"));
    }

    #[test]
    fn round_trips_through_toml() {
        let req = Request {
            kind: RequestKind::Snapshot,
            watch: Some("vault".to_string()),
            requested_at: 1_752_000_000,
        };
        let text = toml::to_string(&req).unwrap();
        assert_eq!(parse(&text).unwrap(), req);
    }

    #[test]
    fn an_absent_watch_fans_out() {
        let text = "kind = \"snapshot\"\nrequested_at = 1752000000\n";
        let req = parse(text).unwrap();
        assert_eq!(req.watch, None);
    }

    #[test]
    fn missing_requested_at_is_rejected() {
        // Without the stamp the daemon cannot age the request; it is malformed.
        assert!(parse("kind = \"snapshot\"\n").is_err());
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(parse("kind = \"snapshot\"\nrequested_at = 1\nwhen = \"now\"\n").is_err());
    }

    #[test]
    fn unknown_kind_is_rejected() {
        assert!(parse("kind = \"restore\"\nrequested_at = 1\n").is_err());
    }

    #[test]
    fn age_is_zero_for_a_future_stamp() {
        let req = Request {
            kind: RequestKind::Snapshot,
            watch: None,
            requested_at: 2_000_000_000,
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        assert_eq!(req.age(now), Duration::ZERO);
    }

    #[test]
    fn age_measures_elapsed_seconds() {
        let req = Request {
            kind: RequestKind::Snapshot,
            watch: None,
            requested_at: 1_000,
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_600);
        assert_eq!(req.age(now), Duration::from_secs(600));
    }

    #[test]
    fn age_of_an_unrepresentable_stamp_is_max_not_a_panic() {
        // A u64 stamp beyond SystemTime's range parses cleanly (the poison
        // path never sees it), so age() must degrade to "infinitely old" —
        // the staleness gate then discards the request — rather than panic
        // the daemon's poll loop with the file still on disk (crash loop).
        let req = Request {
            kind: RequestKind::Snapshot,
            watch: None,
            requested_at: u64::MAX,
        };
        assert_eq!(req.age(SystemTime::now()), Duration::MAX);
    }

    #[test]
    fn write_produces_a_settled_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::snapshot(Some("w".to_string()));
        write(dir.path(), &req).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "one settled file: {entries:?}");
        let name = &entries[0];
        assert!(
            name.ends_with(".toml") && !name.starts_with('.'),
            "got: {name}"
        );
        // It parses back to the same request.
        let text = std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(parse(&text).unwrap(), req);
    }
}
