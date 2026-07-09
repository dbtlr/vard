//! The interval scheduler: turn a per-watch period into a tick signal.
//!
//! A schedule fires one [`SchedulerSignal::Tick`] each time its watch's
//! interval comes due (the engine passes each watch's
//! [`interval`](crate::WatchSpec::interval)). The tick is the scheduler's
//! whole job: it says "this watch's interval came due", and the snapshot
//! engine consumes it. Whether that tick actually produces a snapshot —
//! skipping the work when the tree is clean — is the engine's decision
//! through the VCS layer, not the scheduler's. The scheduler just ticks.
//!
//! This is the twin of the [watcher](crate::watcher): where the watcher signals
//! "activity, then quiet", the scheduler signals "interval elapsed", per watch.
//! Both feed the same snapshot queue.
//!
//! # First tick after a full interval, never at arm
//!
//! The first tick comes one full interval *after* arming, not immediately (see
//! [`Scheduler::arm`]). Arming happens at daemon start, across every configured
//! watch at once; an at-arm tick would stampede every watch into a snapshot
//! simultaneously the instant the daemon comes up. Deferring the first tick by
//! one interval spreads that first wave out to whenever each watch's period
//! naturally falls due.
//!
//! # Missed ticks skip; queued ticks are the consumer's
//!
//! Ticks are level-triggered and queue: the signal channel is unbounded and
//! the tick task never waits for the consumer, so a consumer that stops
//! draining for five intervals finds five queued `Tick`s when it resumes.
//! Collapsing a queued run of identical ticks for the same watch is the
//! consuming engine's job, not the scheduler's.
//!
//! The timer itself, though, never fires a backlog. When the tick *task* is
//! the late party — the executor was starved, or the task was blocked long
//! enough for several deadlines to pass in monotonic time — at most one tick
//! fires for the whole missed window, and the cadence resumes aligned to the
//! original period grid. This is [`MissedTickBehavior::Skip`] on the
//! underlying [`tokio::time::interval`].
//!
//! Laptop sleep needs neither mechanism: the timer runs on the monotonic
//! clock (`CLOCK_MONOTONIC` on Linux, `mach_absolute_time` on macOS), which
//! pauses during system suspend. After wake the timer has seen almost no
//! elapsed time, so no backlog ever exists to coalesce — a watch that slept
//! through six intervals of wall-clock time just finishes elapsing its
//! current (paused) interval and ticks once, never six times.
//!
//! # Trigger mode is not the scheduler's concern
//!
//! [`arm`](Scheduler::arm) does **not** consult the watch's
//! [`TriggerMode`](crate::TriggerMode): it arms a schedule for whatever name
//! and period it is handed. Which components a watch arms per its mode —
//! events, interval, or both — is the engine's decision; the engine simply
//! does not call `arm` for a watch whose mode excludes the interval trigger.
//! Keeping the mode check out of here leaves the scheduler a pure per-watch
//! timer.
//!
//! # Trouble reporting
//!
//! A schedule task that dies abnormally is reported as
//! [`SchedulerSignal::Trouble`] by a supervisor; a deliberate disarm (handle
//! drop) is not abnormal and reports nothing. Trouble travels the same
//! channel as ticks, so the report reaches the consumer only while the
//! [`SchedulerRx`] is alive — keeping that receiver alive and drained is the
//! host's concern.
//!
//! # One scheduler per purpose
//!
//! A `Scheduler` carries one kind of tick. A second timed purpose — the
//! pull-driven sync interval expected later, with its own period and jitter —
//! arms a second `Scheduler` instance with its own receiver; the separate
//! channel is the routing, so a tick never needs a purpose label.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Instant, MissedTickBehavior, interval_at};

/// What the scheduler reports on its one stream: an elapsed interval or trouble.
///
/// `Tick` is emitted once each time a watch's interval comes due — the first
/// one interval after arming, then on a steady cadence (see the
/// [module docs](self) for how a late timer task and a lagging consumer
/// differ).
///
/// `Trouble` means the schedule needs attention: its task died abnormally. A
/// schedule that emits `Trouble` is no longer ticking, so the consumer should
/// surface the condition and, if it still wants interval snapshots for that
/// watch, re-arm it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchedulerSignal {
    /// A watch's interval came due.
    Tick {
        /// Stable name of the watch whose interval elapsed.
        watch: String,
    },
    /// A schedule hit a condition that needs attention.
    Trouble {
        /// Stable name of the watch.
        watch: String,
        /// Human-readable description of the condition.
        detail: String,
    },
}

/// The receiving end of a [`Scheduler`]'s signal stream, returned by
/// [`Scheduler::new`].
///
/// Every armed schedule feeds its [`SchedulerSignal`]s into this one receiver.
/// Call `recv().await` to take the next signal. The channel is unbounded:
/// signals are low-rate (one `Tick` per interval per watch, plus rare
/// `Trouble`), so senders never block and the consumer sees every signal in
/// emission order.
pub type SchedulerRx = mpsc::UnboundedReceiver<SchedulerSignal>;

/// Everything that can go wrong arming a schedule.
#[derive(Debug)]
#[non_exhaustive]
pub enum SchedulerError {
    /// The requested period was zero. A zero period cannot be scheduled — the
    /// underlying timer panics on one — so the schedule is refused here
    /// rather than spawning a task that would die. A period read off a
    /// [`WatchSpec`](crate::WatchSpec) never trips this (its builder rejects
    /// zero durations); a raw duration from any other source can.
    ZeroInterval {
        /// Stable name of the watch whose period was zero.
        watch: String,
    },
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerError::ZeroInterval { watch } => {
                write!(f, "watch {watch:?}: interval must be non-zero to schedule")
            }
        }
    }
}

impl std::error::Error for SchedulerError {}

/// The per-schedule tick loop: one instance runs per armed schedule.
///
/// Ticks on `period`, with the first tick one full `period` after the loop
/// starts (`interval_at(now + period, period)`), and missed deadlines skipped
/// so a late-polling task fires at most one tick for the whole missed window
/// before the cadence resumes on the period grid. Each tick sends one
/// [`SchedulerSignal::Tick`]; the loop ends when the send fails, which means
/// the consumer dropped its [`SchedulerRx`] and there is no one left to tick
/// for.
async fn run_schedule(
    watch: String,
    period: Duration,
    signal_tx: mpsc::UnboundedSender<SchedulerSignal>,
) {
    // First tick one interval out, not at arm: arming happens across every
    // watch at daemon start, and an at-arm tick would stampede them all into a
    // snapshot at once.
    let mut ticker = interval_at(Instant::now() + period, period);
    // Skip (not Burst): if this task polls late — executor starvation, a
    // long-blocked runtime — at most one tick fires for the whole missed
    // window, then the cadence resumes aligned to the period grid, never a
    // backlog of one tick per missed deadline.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        // A send failure means the consumer dropped its receiver: nothing left
        // to tick for, so end cleanly (a normal end, not trouble).
        if signal_tx
            .send(SchedulerSignal::Tick {
                watch: watch.clone(),
            })
            .is_err()
        {
            break;
        }
    }
}

/// Watches a schedule task and reports an abnormal end as
/// [`SchedulerSignal::Trouble`], so an abnormal death is surfaced rather than
/// silent — for as long as the consumer holds its [`SchedulerRx`]. A
/// deliberate abort (disarm) is not abnormal and reports nothing.
fn supervise(
    watch: String,
    task: JoinHandle<()>,
    signal_tx: mpsc::UnboundedSender<SchedulerSignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = task.await
            && !err.is_cancelled()
        {
            let _ = signal_tx.send(SchedulerSignal::Trouble {
                watch,
                detail: format!("schedule task ended abnormally: {err}"),
            });
        }
    })
}

/// An interval scheduler that reports per-watch interval ticks and trouble.
///
/// Construct with [`new`](Scheduler::new), then [`arm`](Scheduler::arm) each
/// watch; arming and disarming are dynamic, so schedules can be added and
/// removed while the scheduler runs. Every armed schedule feeds the single
/// [`SchedulerRx`] returned alongside the scheduler.
///
/// [`arm`](Scheduler::arm) takes `&self`, so one `Scheduler` value serves the
/// whole process.
pub struct Scheduler {
    signal_tx: mpsc::UnboundedSender<SchedulerSignal>,
}

impl Scheduler {
    /// Creates a scheduler and the receiver for every schedule's signals.
    pub fn new() -> (Scheduler, SchedulerRx) {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        (Scheduler { signal_tx }, signal_rx)
    }

    /// Arms a schedule ticking as `watch` every `period`, and returns its
    /// handle.
    ///
    /// `watch` is the tick's stable identity and `period` its cadence; for a
    /// snapshot-interval schedule the engine passes the watch's
    /// [`name()`](crate::WatchSpec::name) and
    /// [`interval()`](crate::WatchSpec::interval). The first tick fires one
    /// full period later, not now (see the [module docs](self) for why);
    /// thereafter it ticks on that period, skipping — never batching — missed
    /// deadlines.
    ///
    /// This does **not** consult the watch's
    /// [`TriggerMode`](crate::TriggerMode): a schedule is armed for whatever
    /// name and period are passed. The engine decides which watches get an
    /// interval schedule per their mode and simply does not arm the ones
    /// whose mode excludes it.
    ///
    /// Fails with [`SchedulerError::ZeroInterval`] if `period` is zero (a
    /// zero period cannot be scheduled).
    ///
    /// # No same-watch deduplication
    ///
    /// Arming the same watch twice is not detected here: both schedules arm,
    /// and both tick — every tick is doubled. The engine owns arming each
    /// watch's schedule exactly once.
    ///
    /// # Runtime
    ///
    /// Must be called from within a Tokio runtime: it spawns the schedule's
    /// tick task and its supervisor.
    pub fn arm(
        &self,
        watch: impl Into<String>,
        period: Duration,
    ) -> Result<ScheduleHandle, SchedulerError> {
        let watch = watch.into();
        if period.is_zero() {
            return Err(SchedulerError::ZeroInterval { watch });
        }

        let task = tokio::spawn(run_schedule(watch.clone(), period, self.signal_tx.clone()));
        let abort = task.abort_handle();
        supervise(watch, task, self.signal_tx.clone());

        Ok(ScheduleHandle { task: abort })
    }
}

/// A live schedule. Dropping it disarms the schedule (see
/// [`disarm`](Self::disarm)).
pub struct ScheduleHandle {
    task: AbortHandle,
}

impl ScheduleHandle {
    /// Disarms the schedule: ends its tick task so it emits no further ticks.
    ///
    /// This is exactly what dropping the handle does; call it to disarm
    /// explicitly and read as intent.
    pub fn disarm(self) {
        // Consumes `self`; `Drop` does the work.
    }
}

impl Drop for ScheduleHandle {
    fn drop(&mut self) {
        // A deliberate abort: the supervisor treats a cancelled task as a
        // normal disarm and reports no trouble.
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawns a bare tick loop with its own channel, returning the signal
    /// receiver and the task handle (whose `abort` models disarm).
    fn spawn_schedule(watch: &str, period: Duration) -> (SchedulerRx, JoinHandle<()>) {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_schedule(watch.to_string(), period, signal_tx));
        (signal_rx, task)
    }

    /// Lets the spawned tick task make progress without advancing the paused
    /// clock (which `yield_now` does not touch).
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// Asserts the next pending signal is `Tick` for `watch`.
    fn expect_tick(rx: &mut SchedulerRx, watch: &str) {
        match rx.try_recv().expect("expected a pending signal") {
            SchedulerSignal::Tick { watch: w } => assert_eq!(w, watch),
            other => panic!("expected Tick, got {other:?}"),
        }
    }

    // --- tick cadence (paused time) ------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn first_tick_comes_one_interval_after_arm_not_immediately() {
        let period = Duration::from_secs(60);
        let (mut rx, _task) = spawn_schedule("w", period);

        // Nothing at arm.
        settle().await;
        assert!(rx.try_recv().is_err(), "must not tick at arm");

        // Nothing one tick short of a full interval.
        tokio::time::advance(period - Duration::from_secs(1)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "must not tick before a full interval elapses"
        );

        // Exactly one tick at the interval.
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        expect_tick(&mut rx, "w");
        assert!(rx.try_recv().is_err(), "exactly one tick per interval");
    }

    #[tokio::test(start_paused = true)]
    async fn steady_cadence_one_tick_per_interval() {
        let period = Duration::from_secs(60);
        let (mut rx, _task) = spawn_schedule("w", period);
        // Let the task park on its first tick before advancing, so its deadline
        // is anchored at the un-advanced clock.
        settle().await;

        for _ in 0..5 {
            tokio::time::advance(period).await;
            settle().await;
            expect_tick(&mut rx, "w");
            assert!(rx.try_recv().is_err(), "one tick per interval, no backlog");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn missed_ticks_coalesce_into_one_then_cadence_resumes() {
        let period = Duration::from_secs(60);
        let (mut rx, _task) = spawn_schedule("w", period);
        settle().await;

        // Model the tick task polling late: five periods of monotonic time
        // pass before it next runs (executor starvation, a long-blocked
        // runtime).
        tokio::time::advance(period * 5).await;
        settle().await;
        expect_tick(&mut rx, "w");
        assert!(
            rx.try_recv().is_err(),
            "a missed window collapses to one tick, not a backlog of five"
        );

        // Cadence resumes aligned: the next tick is one period after the
        // coalesced one, not five periods of catch-up.
        tokio::time::advance(period).await;
        settle().await;
        expect_tick(&mut rx, "w");
        assert!(
            rx.try_recv().is_err(),
            "cadence resumes at one tick per period"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn skipped_ticks_realign_to_the_period_grid_not_the_late_poll() {
        let period = Duration::from_secs(60);
        let (mut rx, _task) = spawn_schedule("w", period);
        settle().await;

        // From the on-grid start, 2.5 periods pass in one jump: the deadlines
        // at 1P and 2P were both missed when the task next polls, at t=2.5P.
        tokio::time::advance(period * 5 / 2).await;
        settle().await;
        expect_tick(&mut rx, "w");
        assert!(rx.try_recv().is_err(), "the missed window is one tick");

        // Skip realigns to the grid: the next tick lands at t=3P, half a
        // period away — not a full period after the late poll (t=3.5P, which
        // is what Delay would give).
        tokio::time::advance(period / 2 - Duration::from_secs(1)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "no tick just short of the grid point"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        expect_tick(&mut rx, "w");
    }

    #[tokio::test(start_paused = true)]
    async fn ticks_queue_for_a_lagging_consumer_they_are_not_coalesced() {
        let period = Duration::from_secs(60);
        let (mut rx, _task) = spawn_schedule("w", period);
        settle().await;

        // The timer task keeps running on cadence while the consumer drains
        // nothing: ticks are level-triggered and queue on the unbounded
        // channel. Deduplicating a queued run is the consuming engine's job.
        for _ in 0..5 {
            tokio::time::advance(period).await;
            settle().await;
        }
        for _ in 0..5 {
            expect_tick(&mut rx, "w");
        }
        assert!(rx.try_recv().is_err(), "exactly the five emitted ticks");
    }

    #[tokio::test(start_paused = true)]
    async fn two_watches_with_different_intervals_tick_independently() {
        let (mut rx_fast, _f) = spawn_schedule("fast", Duration::from_secs(10));
        let (mut rx_slow, _s) = spawn_schedule("slow", Duration::from_secs(30));
        settle().await;

        // At t=10 the fast schedule ticks; the slow one has not come due.
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        expect_tick(&mut rx_fast, "fast");
        assert!(
            rx_slow.try_recv().is_err(),
            "the slow schedule has not reached its interval"
        );

        // At t=20 the fast schedule ticks again; the slow one still has not.
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        expect_tick(&mut rx_fast, "fast");
        assert!(
            rx_slow.try_recv().is_err(),
            "still short of the slow interval"
        );

        // At t=30 both come due, each on its own stream.
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        expect_tick(&mut rx_fast, "fast");
        assert!(rx_fast.try_recv().is_err());
        expect_tick(&mut rx_slow, "slow");
        assert!(rx_slow.try_recv().is_err());
    }

    // --- arm / disarm through the public surface (paused time) ---------------

    #[tokio::test(start_paused = true)]
    async fn armed_schedule_is_silent_until_one_full_period_then_ticks() {
        let period = Duration::from_secs(15 * 60);
        let (scheduler, mut rx) = Scheduler::new();
        let _handle = scheduler.arm("w", period).unwrap();

        // Nothing at arm: arming at daemon start must not stampede a
        // snapshot across every watch at once.
        settle().await;
        assert!(rx.try_recv().is_err(), "must not tick at arm");

        tokio::time::advance(period - Duration::from_secs(1)).await;
        settle().await;
        assert!(
            rx.try_recv().is_err(),
            "must not tick before one full period elapses"
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        expect_tick(&mut rx, "w");
        assert!(rx.try_recv().is_err(), "exactly one tick at the period");
    }

    #[tokio::test(start_paused = true)]
    async fn zero_period_is_refused_at_arm() {
        let (scheduler, _rx) = Scheduler::new();
        match scheduler.arm("w", Duration::ZERO) {
            Err(SchedulerError::ZeroInterval { watch }) => assert_eq!(watch, "w"),
            other => panic!("expected ZeroInterval, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_disarms_and_stops_ticks() {
        let period = Duration::from_secs(60);
        let (scheduler, mut rx) = Scheduler::new();
        let handle = scheduler.arm("w", period).unwrap();
        settle().await;

        // One tick lands while armed.
        tokio::time::advance(period).await;
        settle().await;
        expect_tick(&mut rx, "w");

        // Dropping the handle disarms; no further ticks, ever, and no trouble.
        drop(handle);
        settle().await;
        tokio::time::advance(period * 10).await;
        settle().await;
        assert!(rx.try_recv().is_err(), "no signal after disarm");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_armed_produces_no_ticks() {
        let (_scheduler, mut rx) = Scheduler::new();

        tokio::time::advance(Duration::from_secs(3600)).await;
        settle().await;
        assert!(rx.try_recv().is_err(), "an idle scheduler never ticks");
    }

    // --- supervisor ----------------------------------------------------------

    #[tokio::test]
    async fn panicking_schedule_task_is_reported_as_trouble() {
        let (signal_tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async { panic!("schedule bug") });
        supervise("w".to_string(), task, signal_tx)
            .await
            .expect("supervisor itself must not die");

        match rx.try_recv() {
            Ok(SchedulerSignal::Trouble { watch, .. }) => assert_eq!(watch, "w"),
            other => panic!("expected Trouble after a task panic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliberately_aborted_schedule_task_is_not_trouble() {
        let (signal_tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        let supervisor = supervise("w".to_string(), task, signal_tx);
        abort.abort();
        supervisor.await.expect("supervisor must end cleanly");

        assert!(
            rx.try_recv().is_err(),
            "a deliberate abort (disarm) must not be reported as trouble"
        );
    }
}
