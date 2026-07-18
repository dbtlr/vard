//! The per-key coalescing limiter: the hook loop guard's decision core.
//!
//! Each hook key runs **single-flight** with one latest-wins pending slot,
//! cycling `idle -> running -> cooldown`. An event that arrives while a key is
//! idle runs immediately; one that arrives while the key is running or inside
//! its cooldown window replaces the pending slot (latest event wins) and bumps a
//! suppressed counter. When the run finishes *and* the cooldown window (measured
//! from the run's **start**) has elapsed, the pending event executes carrying the
//! accumulated suppressed count. The trailing event is therefore always
//! eventually delivered — suppression only ever *delays* it, never drops it.
//!
//! This module is deliberately pure: it never spawns a process, never reads a
//! clock, and never blocks. Every decision is driven by an [`Instant`] the caller
//! injects, so the whole state machine is exercised deterministically in the unit
//! tests below without touching real time.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// A payload cleared to run, paired with the number of same-key events coalesced
/// into it since that key's previous run started (`0` for an immediate idle run).
pub(super) struct Fire<P> {
    /// The event payload to execute (the latest one seen for the key).
    pub payload: P,
    /// Events coalesced since the key's previous run began.
    pub suppressed: u64,
}

/// A single key's position in the `idle -> running -> cooldown` cycle. `Idle` is
/// represented explicitly (rather than by absence) so a key that has quiesced can
/// re-fire immediately without re-inserting its config.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Running,
    Cooldown,
}

/// One key's coalescing state.
#[derive(Clone)]
struct KeyState<P> {
    phase: Phase,
    /// The cooldown length for this key (its scope's `hook_rate_limit`).
    rate_limit: Duration,
    /// When the current (or most recent) run began; the cooldown window anchors
    /// here, so `deadline = run_start + rate_limit`.
    run_start: Instant,
    /// Events coalesced since `run_start`'s run began; delivered as the fired
    /// payload's suppressed count and then reset.
    suppressed: u64,
    /// The latest coalesced payload awaiting the cooldown window (latest wins).
    pending: Option<P>,
}

impl<P> KeyState<P> {
    /// The instant the cooldown window opens: cooldown anchors at run-start.
    fn deadline(&self) -> Instant {
        self.run_start + self.rate_limit
    }
}

/// The coalescing limiter over an opaque key `K` and payload `P`.
#[derive(Clone)]
pub(super) struct Limiter<K, P> {
    keys: HashMap<K, KeyState<P>>,
}

impl<K: Eq + Hash + Clone, P> Limiter<K, P> {
    /// A limiter with no keys yet seen.
    pub(super) fn new() -> Self {
        Limiter {
            keys: HashMap::new(),
        }
    }

    /// Feeds an event for `key` (with its scope's `rate_limit` and `payload`) at
    /// `now`. Returns `Some(Fire)` when the event runs immediately (the key was
    /// idle); `None` when it was coalesced into the pending slot (the key was
    /// running or cooling down), with the suppressed counter bumped.
    pub(super) fn on_event(
        &mut self,
        key: K,
        rate_limit: Duration,
        payload: P,
        now: Instant,
    ) -> Option<Fire<P>> {
        if let Some(st) = self.keys.get_mut(&key) {
            // A cooldown that has elapsed with nothing pending is really idle;
            // collapse it so this event can run at once rather than waiting for a
            // poll that would only transition it and drop the event to pending.
            if st.phase == Phase::Cooldown && st.pending.is_none() && now >= st.deadline() {
                st.phase = Phase::Idle;
            }
            match st.phase {
                Phase::Idle => {
                    st.phase = Phase::Running;
                    st.run_start = now;
                    st.suppressed = 0;
                    st.pending = None;
                    st.rate_limit = rate_limit;
                    Some(Fire {
                        payload,
                        suppressed: 0,
                    })
                }
                Phase::Running | Phase::Cooldown => {
                    st.pending = Some(payload);
                    st.suppressed += 1;
                    None
                }
            }
        } else {
            self.keys.insert(
                key,
                KeyState {
                    phase: Phase::Running,
                    rate_limit,
                    run_start: now,
                    suppressed: 0,
                    pending: None,
                },
            );
            Some(Fire {
                payload,
                suppressed: 0,
            })
        }
    }

    /// Records that `key`'s in-flight run finished at `now`. Returns `Some(Fire)`
    /// when a pending event is due immediately (the cooldown window had already
    /// elapsed by the time the run finished); otherwise `None` — the key drops to
    /// cooldown (a pending event will fire at [`next_deadline`](Self::next_deadline))
    /// or straight back to idle when nothing is pending and the window has passed.
    pub(super) fn on_finished(&mut self, key: &K, now: Instant) -> Option<Fire<P>> {
        let st = self.keys.get_mut(key)?;
        // Defensive: only a running key finishes.
        if st.phase != Phase::Running {
            return None;
        }
        let elapsed = now >= st.deadline();
        match (st.pending.is_some(), elapsed) {
            (true, true) => {
                let payload = st.pending.take().expect("pending present");
                let suppressed = st.suppressed;
                st.run_start = now;
                st.suppressed = 0;
                st.phase = Phase::Running;
                Some(Fire {
                    payload,
                    suppressed,
                })
            }
            (true, false) => {
                st.phase = Phase::Cooldown;
                None
            }
            (false, true) => {
                st.phase = Phase::Idle;
                None
            }
            (false, false) => {
                st.phase = Phase::Cooldown;
                None
            }
        }
    }

    /// The earliest cooldown deadline across all keys, if any — the instant the
    /// caller should next call [`poll`](Self::poll). Only keys in cooldown carry a
    /// deadline; running keys wait on their completion instead.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.keys
            .values()
            .filter(|st| st.phase == Phase::Cooldown)
            .map(KeyState::deadline)
            .min()
    }

    /// Drops every key the predicate rejects — used on a re-arm to prune keys
    /// (`(scope, event, command)`) the new config no longer arms, so a stale
    /// pending slot for a removed or command-changed hook cannot fire.
    pub(super) fn retain(&mut self, keep: impl Fn(&K) -> bool) {
        self.keys.retain(|k, _| keep(k));
    }

    /// Collapses every `Running` key to `Cooldown`, anchored at its recorded
    /// run-start (the pending slot and suppressed count are preserved).
    ///
    /// Used when handing this limiter's state to a re-armed runner. The successor
    /// starts with an empty `JoinSet`, so it can never receive the `on_finished`
    /// for a run that was in flight at handoff — a carried `Running` key would
    /// then coalesce future events forever without ever firing. Treating the run
    /// as finished-at-handoff is honest: the in-flight process runs to completion
    /// on the old blocking pool (its outcome is merely unreportable), and the
    /// cooldown anchored at the real run-start preserves both debounce (the window
    /// still measures from when the run began) and delivery (a pending slot fires
    /// at the window edge via [`poll`](Self::poll)).
    pub(super) fn collapse_running_to_cooldown(&mut self) {
        for st in self.keys.values_mut() {
            if st.phase == Phase::Running {
                st.phase = Phase::Cooldown;
            }
        }
    }

    /// Fires every key whose cooldown window has opened by `now`: a key with a
    /// pending event re-runs (carrying its suppressed count), an empty one drops
    /// back to idle. Returns the payloads to execute, keyed.
    pub(super) fn poll(&mut self, now: Instant) -> Vec<(K, Fire<P>)> {
        let mut fired = Vec::new();
        for (key, st) in self.keys.iter_mut() {
            if st.phase != Phase::Cooldown || now < st.deadline() {
                continue;
            }
            if let Some(payload) = st.pending.take() {
                let suppressed = st.suppressed;
                st.run_start = now;
                st.suppressed = 0;
                st.phase = Phase::Running;
                fired.push((
                    key.clone(),
                    Fire {
                        payload,
                        suppressed,
                    },
                ));
            } else {
                st.phase = Phase::Idle;
            }
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: Duration = Duration::from_secs(300);

    /// Feeds an event and asserts it fired immediately, returning the payload and
    /// suppressed count.
    fn expect_fire<P: std::fmt::Debug>(fire: Option<Fire<P>>) -> (P, u64) {
        let fire = fire.expect("expected an immediate fire");
        (fire.payload, fire.suppressed)
    }

    #[test]
    fn an_idle_key_runs_immediately_with_zero_suppressed() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        let (payload, suppressed) = expect_fire(lim.on_event("k", RATE, "a", t0));
        assert_eq!(payload, "a");
        assert_eq!(suppressed, 0, "an immediate idle run coalesces nothing");
    }

    #[test]
    fn a_single_event_during_a_run_defers_to_exactly_one_trailing_run() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        // Arrives mid-run: coalesced, not run.
        assert!(lim.on_event("k", RATE, "b", t0 + ms(10)).is_none());
        // Finishing before the window: no immediate run, a deadline is armed.
        assert!(lim.on_finished(&"k", t0 + ms(20)).is_none());
        assert_eq!(lim.next_deadline(), Some(t0 + RATE));
        // Nothing is due before the window opens.
        assert!(lim.poll(t0 + RATE - ms(1)).is_empty());
        // At the window the single deferred event fires, carrying suppressed = 1.
        let fired = lim.poll(t0 + RATE);
        assert_eq!(fired.len(), 1, "exactly one trailing run");
        assert_eq!(fired[0].0, "k");
        assert_eq!(fired[0].1.payload, "b", "the latest payload");
        assert_eq!(fired[0].1.suppressed, 1);
    }

    #[test]
    fn a_burst_coalesces_to_the_latest_payload_with_the_full_suppressed_count() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        // Three events land mid-run; only the last survives in the pending slot.
        assert!(lim.on_event("k", RATE, "b", t0 + ms(1)).is_none());
        assert!(lim.on_event("k", RATE, "c", t0 + ms(2)).is_none());
        assert!(lim.on_event("k", RATE, "d", t0 + ms(3)).is_none());
        lim.on_finished(&"k", t0 + ms(4));
        let fired = lim.poll(t0 + RATE);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].1.payload, "d", "latest wins");
        assert_eq!(
            fired[0].1.suppressed, 3,
            "all three coalesced events counted"
        );
    }

    #[test]
    fn a_run_finishing_after_the_window_fires_the_pending_immediately() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        assert!(lim.on_event("k", RATE, "b", t0 + ms(10)).is_none());
        // The run outlasts the cooldown window: on finish the pending is due now.
        let fire = lim.on_finished(&"k", t0 + RATE + ms(5));
        let (payload, suppressed) = expect_fire(fire);
        assert_eq!(payload, "b");
        assert_eq!(suppressed, 1);
        // Having just re-fired, the key is running again — no deadline pending.
        assert_eq!(lim.next_deadline(), None);
    }

    #[test]
    fn events_arriving_during_cooldown_are_coalesced_not_run() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        // Finish quickly with nothing pending -> cooldown (window still open).
        assert!(lim.on_finished(&"k", t0 + ms(5)).is_none());
        assert_eq!(lim.next_deadline(), Some(t0 + RATE));
        // An event mid-cooldown defers rather than running immediately.
        assert!(lim.on_event("k", RATE, "b", t0 + ms(50)).is_none());
        let fired = lim.poll(t0 + RATE);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].1.payload, "b");
        assert_eq!(fired[0].1.suppressed, 1);
    }

    #[test]
    fn an_empty_cooldown_returns_to_idle_and_the_next_event_runs_at_once() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        lim.on_finished(&"k", t0 + ms(5));
        // Poll past the window with nothing pending: the key goes idle.
        assert!(lim.poll(t0 + RATE).is_empty());
        assert_eq!(lim.next_deadline(), None);
        // The next event runs immediately with a clean suppressed count.
        let (payload, suppressed) = expect_fire(lim.on_event("k", RATE, "b", t0 + RATE + ms(1)));
        assert_eq!(payload, "b");
        assert_eq!(suppressed, 0);
    }

    #[test]
    fn an_event_after_an_elapsed_empty_cooldown_runs_immediately() {
        // Same as above but the caller never polled: on_event itself must treat an
        // elapsed, empty cooldown as idle and run at once.
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        lim.on_finished(&"k", t0 + ms(5));
        let (payload, suppressed) = expect_fire(lim.on_event("k", RATE, "b", t0 + RATE + ms(1)));
        assert_eq!(payload, "b");
        assert_eq!(suppressed, 0);
    }

    #[test]
    fn distinct_keys_are_fully_independent() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        // k1 runs and gets a pending; k2 runs independently.
        expect_fire(lim.on_event("k1", RATE, "a1", t0));
        expect_fire(lim.on_event("k2", RATE, "a2", t0));
        assert!(lim.on_event("k1", RATE, "b1", t0 + ms(1)).is_none());
        // k2 finishing and going idle does not disturb k1's pending run: at the
        // window k2 idles and k1 is still running, so nothing fires yet.
        lim.on_finished(&"k2", t0 + ms(2));
        assert!(lim.poll(t0 + RATE).is_empty());
        lim.on_finished(&"k1", t0 + ms(3));
        let fired = lim.poll(t0 + RATE);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, "k1");
        assert_eq!(fired[0].1.payload, "b1");
    }

    #[test]
    fn the_trailing_event_survives_repeated_bursts() {
        // Every window carries the last event of the preceding burst; nothing is
        // ever lost across many cycles.
        let mut lim: Limiter<&str, u32> = Limiter::new();
        let mut now = Instant::now();
        expect_fire(lim.on_event("k", RATE, 0, now));
        for round in 1..=5u32 {
            // A burst lands during the run; the last value is `round * 10 + 2`.
            for step in 0..3u32 {
                let v = round * 10 + step;
                assert!(
                    lim.on_event("k", RATE, v, now + ms(1 + step as u64))
                        .is_none()
                );
            }
            let last = round * 10 + 2;
            lim.on_finished(&"k", now + ms(10));
            let fired = lim.poll(now + RATE);
            assert_eq!(fired.len(), 1, "round {round}: one trailing run");
            assert_eq!(fired[0].1.payload, last, "round {round}: latest survives");
            assert_eq!(
                fired[0].1.suppressed, 3,
                "round {round}: full burst counted"
            );
            // Advance to the just-started run's own window for the next round.
            now += RATE;
        }
    }

    #[test]
    fn a_second_event_while_running_never_yields_a_second_concurrent_run() {
        // The limiter only ever hands back a fire for an idle key or a due
        // pending; a mid-run event can only coalesce, so same-key runs cannot
        // overlap.
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        for i in 0..10 {
            assert!(
                lim.on_event("k", RATE, "x", t0 + ms(i)).is_none(),
                "no fire may be issued while the key is still running"
            );
        }
    }

    #[test]
    fn collapsing_a_running_key_with_a_pending_fires_it_at_the_window_edge() {
        // A run in flight (Running) with a coalesced pending event: collapsing to
        // cooldown (the re-arm handoff) must not lose the pending — it fires at the
        // window anchored at the original run-start, never wedging.
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        assert!(lim.on_event("k", RATE, "b", t0 + ms(10)).is_none());
        // Handoff while the run is still in flight: no on_finished will ever come.
        lim.collapse_running_to_cooldown();
        // The key is now cooling with its pending intact; the window still anchors
        // at the original run-start, so it opens at t0 + RATE.
        assert_eq!(lim.next_deadline(), Some(t0 + RATE));
        assert!(lim.poll(t0 + RATE - ms(1)).is_empty());
        let fired = lim.poll(t0 + RATE);
        assert_eq!(fired.len(), 1, "the carried pending fires, not a wedge");
        assert_eq!(fired[0].1.payload, "b");
        assert_eq!(fired[0].1.suppressed, 1);
    }

    #[test]
    fn collapsing_a_running_key_with_no_pending_returns_to_idle_at_the_window() {
        let mut lim: Limiter<&str, &str> = Limiter::new();
        let t0 = Instant::now();
        expect_fire(lim.on_event("k", RATE, "a", t0));
        lim.collapse_running_to_cooldown();
        // Empty cooldown: at the window it drops to idle, then the next event runs.
        assert!(lim.poll(t0 + RATE).is_empty());
        assert_eq!(lim.next_deadline(), None);
        let (payload, suppressed) = expect_fire(lim.on_event("k", RATE, "c", t0 + RATE + ms(1)));
        assert_eq!(payload, "c");
        assert_eq!(suppressed, 0);
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
}
