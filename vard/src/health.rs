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
//! # The document (versioned)
//!
//! A [`HealthDoc`] is a TOML document (reusing the binary's existing serde+TOML
//! stack, exactly as [`crate::request`] does — no new dependency for one small
//! file):
//!
//! ```toml
//! version = 1
//! written_at = 1752000000     # unix seconds of this write, for staleness
//!
//! [[problem]]
//! watch = "vault"
//! state = "conflicted"        # paused | conflicted | sync-error | attention
//! summary = "a sync conflict is blocking progress"
//! since = 1751990000          # unix seconds the state was entered
//! ```
//!
//! Only *problem* watches contribute an entry; a healthy watch adds nothing, so
//! a healthy daemon writes a document with an empty `problem` list. The
//! `version` field lets the shape evolve — a notify built against a newer schema
//! can refuse an unknown version rather than misread it.
//!
//! Timestamps are epoch seconds so notify renders *elapsed* time ("for 2h"),
//! never a wall-clock instant that would lie across timezones.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vard_core::{Event, WatchState};

/// The current health-document schema version. Bump on any breaking shape
/// change so a reader can reject what it cannot parse.
pub(crate) const VERSION: u32 = 1;

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
}

/// One troubled watch's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HealthProblem {
    /// The watch's stable name.
    pub watch: String,
    /// The problem state, in the spec's status vocabulary: `paused`,
    /// `conflicted`, `sync-error`, or `attention` (mapped from [`WatchState`]).
    pub state: String,
    /// A human-readable one-line summary of the problem.
    pub summary: String,
    /// Unix seconds at which the watch entered this state, so a reader can
    /// render elapsed time.
    pub since: u64,
}

/// The daemon-side accumulator that turns the stream of
/// [`Event::WatchStateChanged`] into the current problem set. Keyed by watch
/// name in a [`BTreeMap`] so the written document is deterministically ordered.
#[derive(Debug, Default)]
pub(crate) struct HealthTracker {
    problems: BTreeMap<String, HealthProblem>,
}

impl HealthTracker {
    /// A tracker with no known problems.
    pub(crate) fn new() -> HealthTracker {
        HealthTracker::default()
    }

    /// Folds one event into the problem set, returning `true` when the set
    /// changed (so the caller rewrites the file). Only
    /// [`Event::WatchStateChanged`] carries health-relevant information; every
    /// other event returns `false` and leaves the set untouched.
    ///
    /// A transition **to** [`WatchState::Ok`] clears the watch's problem (a
    /// recovery); a transition to any other state records or replaces it with
    /// `since = now`. The file always reflects *current* problem state, not a
    /// log, so a recovery must remove the entry.
    pub(crate) fn observe(&mut self, event: &Event, now: u64) -> bool {
        let Event::WatchStateChanged {
            watch, to, reason, ..
        } = event
        else {
            return false;
        };

        match problem_state(*to) {
            None => self.problems.remove(watch).is_some(),
            Some(state) => {
                let problem = HealthProblem {
                    watch: watch.clone(),
                    state,
                    summary: summary_for(*to, reason.as_deref()),
                    since: now,
                };
                self.problems.insert(watch.clone(), problem);
                // Every problem transition is a genuine state change the engine
                // only emits on a real edge, and it refreshes `since` to now, so
                // the file is always worth rewriting.
                true
            }
        }
    }

    /// Snapshots the current problems into a document stamped `written_at =
    /// now`.
    pub(crate) fn doc(&self, now: u64) -> HealthDoc {
        HealthDoc {
            version: VERSION,
            written_at: now,
            problems: self.problems.values().cloned().collect(),
        }
    }
}

/// The status-vocabulary string for a non-healthy [`WatchState`], or `None` for
/// [`WatchState::Ok`] (which is not a problem). The strings come straight from
/// `WatchState`'s `Display`, so they never drift from the engine's vocabulary.
fn problem_state(state: WatchState) -> Option<String> {
    match state {
        WatchState::Ok => None,
        other => Some(other.to_string()),
    }
}

/// The human summary for a problem: the transition's own `reason` when the
/// engine gave one, else a fixed fallback per state so the line is never empty.
fn summary_for(state: WatchState, reason: Option<&str>) -> String {
    if let Some(reason) = reason.map(str::trim).filter(|r| !r.is_empty()) {
        return reason.to_string();
    }
    match state {
        WatchState::Paused => "watch is paused",
        WatchState::Conflicted => "a sync conflict is blocking progress",
        WatchState::SyncError => "a sync operation is failing",
        WatchState::Attention => "needs attention",
        // Ok is filtered out before this is reached.
        WatchState::Ok => "healthy",
        _ => "needs attention",
    }
    .to_string()
}

/// Serializes `doc` and installs it atomically at `path` (temp + fsync +
/// rename, via [`crate::atomic`]), so a concurrent notify read sees either the
/// old document or the whole new one, never a torn write.
pub(crate) fn write(path: &Path, doc: &HealthDoc) -> Result<(), String> {
    let text = toml::to_string(doc).map_err(|e| format!("encoding health document: {e}"))?;
    crate::atomic::write(path, text.as_bytes())
        .map_err(|e| format!("writing health file {}: {e}", path.display()))
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

/// Removes the health file, best-effort — the clean-shutdown clear. A missing
/// file is success; any other error is ignored (the daemon is exiting anyway,
/// and notify's daemon-not-running probe does not depend on the file).
pub(crate) fn clear(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The current unix time in whole seconds (0 before the epoch, impossible in
/// practice).
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vard_core::TroubleKind;

    fn changed(watch: &str, to: WatchState, reason: Option<&str>) -> Event {
        Event::WatchStateChanged {
            watch: watch.to_string(),
            from: WatchState::Ok,
            to,
            reason: reason.map(str::to_string),
            trouble: None,
        }
    }

    #[test]
    fn a_problem_transition_is_recorded_with_since_now() {
        let mut t = HealthTracker::new();
        assert!(t.observe(
            &changed("vault", WatchState::Conflicted, Some("merge stuck")),
            1000
        ));
        let doc = t.doc(2000);
        assert_eq!(doc.version, VERSION);
        assert_eq!(doc.written_at, 2000);
        assert_eq!(doc.problems.len(), 1);
        let p = &doc.problems[0];
        assert_eq!(p.watch, "vault");
        assert_eq!(p.state, "conflicted");
        assert_eq!(p.summary, "merge stuck");
        assert_eq!(p.since, 1000);
    }

    #[test]
    fn recovery_to_ok_clears_the_problem() {
        let mut t = HealthTracker::new();
        t.observe(&changed("vault", WatchState::Attention, None), 10);
        assert_eq!(t.doc(0).problems.len(), 1);
        // Returning to Ok removes the entry — the file is state, not a log.
        assert!(t.observe(
            &Event::WatchStateChanged {
                watch: "vault".to_string(),
                from: WatchState::Attention,
                to: WatchState::Ok,
                reason: None,
                trouble: None,
            },
            20
        ));
        assert!(t.doc(0).problems.is_empty());
    }

    #[test]
    fn a_recovery_with_no_prior_problem_reports_no_change() {
        let mut t = HealthTracker::new();
        // An Ok→Ok (or first-seen Ok) transition changes nothing.
        assert!(!t.observe(&changed("vault", WatchState::Ok, None), 10));
    }

    #[test]
    fn missing_reason_falls_back_to_a_per_state_summary() {
        let mut t = HealthTracker::new();
        t.observe(&changed("w", WatchState::SyncError, None), 1);
        assert_eq!(t.doc(0).problems[0].summary, "a sync operation is failing");
        t.observe(&changed("w", WatchState::Paused, Some("   ")), 1);
        assert_eq!(t.doc(0).problems[0].summary, "watch is paused");
    }

    #[test]
    fn non_state_events_do_not_touch_the_set() {
        let mut t = HealthTracker::new();
        assert!(!t.observe(&Event::DaemonStarted, 1));
        assert!(!t.observe(
            &Event::SnapshotCompleted {
                watch: "w".to_string(),
                snapshot: "abc".to_string(),
                files_changed: 1,
                trigger: vard_core::Trigger::Event,
            },
            1
        ));
        assert!(t.doc(0).problems.is_empty());
    }

    #[test]
    fn problems_are_ordered_by_watch_name() {
        let mut t = HealthTracker::new();
        t.observe(&changed("zebra", WatchState::Attention, None), 1);
        t.observe(&changed("apple", WatchState::Paused, None), 1);
        let names: Vec<_> = t.doc(0).problems.iter().map(|p| p.watch.clone()).collect();
        assert_eq!(names, vec!["apple", "zebra"]);
    }

    #[test]
    fn document_round_trips_through_toml() {
        let mut t = HealthTracker::new();
        t.observe(&changed("vault", WatchState::Conflicted, Some("x")), 100);
        let doc = t.doc(200);
        let text = toml::to_string(&doc).unwrap();
        let back: HealthDoc = toml::from_str(&text).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn write_then_read_round_trips_and_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health");
        assert_eq!(read(&path).unwrap(), None, "missing file reads as None");

        let mut t = HealthTracker::new();
        // A trouble transition still keys the state off `to`, not the kind.
        t.observe(
            &Event::WatchStateChanged {
                watch: "w".to_string(),
                from: WatchState::Ok,
                to: WatchState::Attention,
                reason: Some("backend died".to_string()),
                trouble: Some(TroubleKind::SourceDied),
            },
            5,
        );
        let doc = t.doc(9);
        write(&path, &doc).unwrap();
        assert_eq!(read(&path).unwrap(), Some(doc));
    }

    #[test]
    fn a_corrupt_document_is_an_error_not_silently_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health");
        std::fs::write(&path, "this = is = not = toml").unwrap();
        assert!(read(&path).is_err());
    }
}
