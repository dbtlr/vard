//! The interval scheduler: turn a per-watch period into a tick signal.
//!
//! A schedule fires exactly one [`SchedulerSignal::Tick`] each time its watch's
//! interval elapses (default [`WatchSpec::interval`]). The tick is the
//! scheduler's whole job: it says "this watch's interval came due", and the
//! snapshot engine consumes it. Whether that tick actually produces a
//! snapshot — skipping the work when the tree is clean — is the engine's
//! decision through the VCS layer, not the scheduler's. The scheduler just
//! ticks.
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
//! # Missed-tick coalescing
//!
//! After the laptop sleeps and wakes — or a stalled consumer stops draining
//! ticks — the wall-clock time for several intervals may pass at once. The
//! scheduler collapses that missed window into a *single* tick and then resumes
//! the normal cadence, aligned to the period; it never fires a backlog of one
//! tick per missed interval. A watch that slept through six intervals wants one
//! snapshot of where the tree landed, not six identical ones. This is
//! [`MissedTickBehavior::Skip`] on the underlying [`tokio::time::interval`].
//!
//! # Trigger mode is not the scheduler's concern
//!
//! [`arm`](Scheduler::arm) does **not** consult
//! [`WatchSpec::trigger`](crate::TriggerMode): it arms a schedule for any watch
//! handed to it. Which components a watch arms per its
//! [`TriggerMode`](crate::TriggerMode) — events, interval, or both — is the
//! engine's decision; the engine simply does not call `arm` for a watch whose
//! mode excludes the interval trigger. Keeping the mode check out of here leaves
//! the scheduler a pure per-watch timer.
//!
//! # Trouble reporting
//!
//! A schedule task that dies abnormally is reported as
//! [`SchedulerSignal::Trouble`] by a supervisor, so a schedule can never
//! silently turn into a zombie that looks armed but never ticks. A deliberate
//! disarm (handle drop) is not abnormal and reports nothing.
//!
//! # One schedule per purpose
//!
//! A watch gets one schedule per timed purpose: today only the snapshot
//! interval, but a second pull-driven sync schedule (a different period, with
//! jitter) is expected later and would arm as its own independent schedule over
//! the same watch, distinguished by a label on the tick.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::config::WatchSpec;

/// What the scheduler reports on its one stream: an elapsed interval or trouble.
///
/// `Tick` is emitted once each time a watch's interval comes due — the first
/// one interval after arming, then on a steady cadence, with a slept-through
/// window collapsed to a single tick (see the [module docs](self)).
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
    /// The watch's interval was zero. A zero period cannot be scheduled — the
    /// underlying timer rejects it — so the schedule is refused here rather
    /// than spawning a task that would panic. [`WatchSpec`]'s builder already
    /// rejects a zero interval, so this is defense in depth against a period of
    /// zero reaching the timer regardless of how the spec was obtained.
    ZeroInterval {
        /// Stable name of the watch whose interval was zero.
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
/// starts (`interval_at(now + period, period)`), and missed ticks skipped so a
/// slept-through window collapses to one tick before the cadence resumes. Each
/// tick sends one [`SchedulerSignal::Tick`]; the loop ends when the send fails,
/// which means the consumer dropped its [`SchedulerRx`] and there is no one
/// left to tick for.
async fn run_schedule(
    watch: String,
    period: Duration,
    signal_tx: mpsc::UnboundedSender<SchedulerSignal>,
) {
    // First tick one interval out, not at arm: arming happens across every
    // watch at daemon start, and an at-arm tick would stampede them all into a
    // snapshot at once.
    let mut ticker = interval_at(Instant::now() + period, period);
    // Skip (not Burst): after suspend/resume or a stalled consumer, at most one
    // tick fires for the whole missed window, then the cadence resumes aligned
    // to the period — never a backlog of one tick per missed interval.
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
/// [`SchedulerSignal::Trouble`], so a schedule can never die silently. A
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
/// The scheduler is cheap to clone-by-handle: [`arm`](Scheduler::arm) takes
/// `&self`, so one `Scheduler` value serves the whole process.
pub struct Scheduler {
    signal_tx: mpsc::UnboundedSender<SchedulerSignal>,
}

impl Scheduler {
    /// Creates a scheduler and the receiver for every schedule's signals.
    pub fn new() -> (Scheduler, SchedulerRx) {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        (Scheduler { signal_tx }, signal_rx)
    }

    /// Arms an interval schedule for `spec` and returns its handle.
    ///
    /// Uses [`spec.name()`](WatchSpec::name) as the tick's stable identity and
    /// [`spec.interval()`](WatchSpec::interval) as the period. The first tick
    /// fires one full interval later, not now (see the [module docs](self) for
    /// why); thereafter it ticks on that period, coalescing missed ticks.
    ///
    /// This does **not** consult [`spec.trigger()`](crate::TriggerMode): a
    /// schedule is armed for whatever watch is passed. The engine decides which
    /// watches get an interval schedule per their
    /// [`TriggerMode`](crate::TriggerMode) and simply does not arm the ones
    /// that exclude it.
    ///
    /// Fails with [`SchedulerError::ZeroInterval`] if the interval is zero (a
    /// zero period cannot be scheduled). [`WatchSpec`]'s builder already rejects
    /// that, so a spec from the builder never trips it.
    ///
    /// # Runtime
    ///
    /// Must be called from within a Tokio runtime: it spawns the schedule's
    /// tick task and its supervisor.
    pub fn arm(&self, spec: &WatchSpec) -> Result<ScheduleHandle, SchedulerError> {
        let watch = spec.name().to_string();
        let period = spec.interval();
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

    /// A watch spec with a given interval over a nominal path. The path is
    /// never touched — the scheduler is a pure timer — so it need not exist.
    fn spec(name: &str, interval: Duration) -> WatchSpec {
        WatchSpec::builder(name, "/nonexistent/vard-scheduler-test")
            .interval(interval)
            .build()
            .unwrap()
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

        // Model laptop sleep or a stalled consumer: several intervals of
        // wall-clock time pass in a single jump.
        tokio::time::advance(period * 5).await;
        settle().await;
        expect_tick(&mut rx, "w");
        assert!(
            rx.try_recv().is_err(),
            "a slept-through window collapses to one tick, not a backlog of five"
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
    async fn armed_schedule_ticks_on_its_interval() {
        let period = Duration::from_secs(15 * 60);
        let (scheduler, mut rx) = Scheduler::new();
        let handle = scheduler.arm(&spec("w", period)).unwrap();
        settle().await;

        tokio::time::advance(period).await;
        settle().await;
        expect_tick(&mut rx, "w");

        drop(handle);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_disarms_and_stops_ticks() {
        let period = Duration::from_secs(60);
        let (scheduler, mut rx) = Scheduler::new();
        let handle = scheduler.arm(&spec("w", period)).unwrap();
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
