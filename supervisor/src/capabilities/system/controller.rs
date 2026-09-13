//! [`SystemController`] feeds `obelisk.system.time` from one
//! wall-clock-aligned task, refreshed every second.
//!
//! `system.time` has no interval argument, so it ticks unconditionally from construction to shutdown,
//! aligning its first wake to the wall-clock second boundary ([`time_until_next_second`]).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;

/// `obelisk.system`'s Lua-visible fields, with their
/// `StateSnapshot` JSON keys unchanged.
#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct SystemState {
    /// Unix epoch seconds, not milliseconds. `os.date` expects seconds;
    /// milliseconds would be wrong by 1000x.
    pub time: i64,
    /// Whole seconds since this controller was built, which is the first time a config asked for
    /// `system`. Only a difference means anything; take durations from this rather than from `time`,
    /// which `settimeofday` and an NTP step move underneath a deadline.
    ///
    /// ponytail: `Instant` is `CLOCK_MONOTONIC` on Linux, so a suspend does not count toward an
    /// elapsed reading. Suspend-inclusive timing wants `CLOCK_BOOTTIME` as a second field.
    pub monotonic: i64,
}

/// Wakes `main.rs`'s `select!` for a fresh `StateSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSignal {
    Changed,
}

/// Whether a sampled state is worth pushing. Required by ADR-0044: every `StateSnapshot` dirties
/// the Renderer and triggers full re-resolve/repaint, so unconditional pushes repaint unchanged
/// scenes.
///
/// Compares the whole state, not the wall second alone: a clock correction that kept resampling one
/// second would otherwise stall `monotonic` for as long as it ran, and a countdown armed from a
/// stalled reading fires the moment it unsticks.
pub fn should_emit(last_emitted: Option<&SystemState>, current: &SystemState) -> bool {
    last_emitted != Some(current)
}

/// `SystemTime::now()`'s epoch truncated to whole seconds for `time`; one pinned seam.
pub fn epoch_seconds(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0)
}

/// Duration from `elapsed_since_epoch` to the next whole-second boundary. Pure over `Duration` for
/// clock-free tests.
///
/// ponytail: aligns once at startup, then uses a steady 1-second `tokio::time::interval` on
/// monotonic `Instant`. It does not track wall-clock drift. With no NTP step or suspend/resume,
/// both clocks run closely enough for this shell's sessions; a step (`settimeofday`, NTP slew,
/// resume) leaves the old boundary until restart. Upgrade by re-deriving alignment each tick and
/// rebuilding the interval after a discrepancy of a few milliseconds; not worth a check in this
/// 1Hz loop until observed.
pub fn time_until_next_second(elapsed_since_epoch: Duration) -> Duration {
    Duration::from_secs(1) - Duration::from_nanos(u64::from(elapsed_since_epoch.subsec_nanos()))
}

pub struct SystemController {
    state: Arc<Mutex<SystemState>>,
}

impl SystemController {
    /// Seeds `time` immediately, so an early client sees the current second, not stale zero; the
    /// ticking task takes over afterward.
    pub fn new(signal_tx: UnboundedSender<SystemSignal>) -> Self {
        let started = Instant::now();
        let state = Arc::new(Mutex::new(SystemState { time: epoch_seconds(SystemTime::now()), monotonic: 0 }));

        tokio::spawn(run_clock_task(Arc::clone(&state), signal_tx, started));

        Self { state }
    }

    /// Current state for `main.rs`'s signal-channel `select!` snapshot push.
    pub fn snapshot(&self) -> SystemState {
        self.state.lock().expect("system state mutex poisoned").clone()
    }
}

/// Aligns its first wake to the next wall-clock second, then ticks a steady one-second interval.
/// The held state is its own record of what was last pushed, so the tick seeded by `new` does not
/// double-push.
async fn run_clock_task(state: Arc<Mutex<SystemState>>, signal_tx: UnboundedSender<SystemSignal>, started: Instant) {
    let delay = time_until_next_second(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default());
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + delay, Duration::from_secs(1));

    loop {
        ticker.tick().await;
        let sampled =
            SystemState { time: epoch_seconds(SystemTime::now()), monotonic: started.elapsed().as_secs() as i64 };
        let mut held = state.lock().expect("system state mutex poisoned");
        if !should_emit(Some(&held), &sampled) {
            continue;
        }
        *held = sampled;
        drop(held); // the consumer's `snapshot` takes this same lock.
        if signal_tx.send(SystemSignal::Changed).is_err() {
            return; // main.rs's select! loop is gone; nothing left to notify
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampled(time: i64, monotonic: i64) -> SystemState {
        SystemState { time, monotonic }
    }

    #[test]
    fn should_emit_is_false_for_an_unchanged_sample() {
        assert!(!should_emit(Some(&sampled(1_700_000_000, 5)), &sampled(1_700_000_000, 5)));
    }

    #[test]
    fn should_emit_is_true_for_a_new_second() {
        assert!(should_emit(Some(&sampled(1_700_000_000, 5)), &sampled(1_700_000_001, 6)));
    }

    /// A clock correction landing on the second already sampled must not stall `monotonic`: a
    /// countdown armed from a stalled reading fires as soon as the corrections stop.
    #[test]
    fn should_emit_is_true_when_the_wall_second_repeats_but_monotonic_advanced() {
        assert!(should_emit(Some(&sampled(1_700_000_000, 5)), &sampled(1_700_000_000, 6)));
    }

    #[test]
    fn should_emit_is_true_before_anything_has_ever_been_emitted() {
        assert!(should_emit(None, &sampled(1_700_000_000, 0)));
    }

    #[test]
    fn epoch_seconds_is_pinned_against_a_known_instant() {
        let known = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(epoch_seconds(known), 1_700_000_000, "must be exact -- Lua reads this straight as an integer");
    }

    #[test]
    fn epoch_seconds_is_seconds_not_milliseconds() {
        // Millis "now" would be roughly 1_700_000_000_000, three orders larger.
        let known = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let seconds = epoch_seconds(known);
        assert!((1_000_000_000..2_000_000_000).contains(&seconds), "plausible unix epoch seconds range, got {seconds}");
    }

    #[test]
    fn time_until_next_second_is_the_complement_of_the_subsecond_remainder() {
        assert_eq!(time_until_next_second(Duration::from_millis(300)), Duration::from_millis(700));
        assert_eq!(time_until_next_second(Duration::new(10, 999_000_000)), Duration::from_millis(1));
    }

    #[test]
    fn time_until_next_second_is_a_full_second_when_already_on_the_boundary() {
        assert_eq!(time_until_next_second(Duration::from_secs(5)), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn new_seeds_time_synchronously_before_any_tick_fires() {
        // The first tick is a second away; clients in that second must read real epoch, not zero.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SystemController::new(tx);

        let seeded = controller.snapshot();
        assert!(seeded.time > 1_700_000_000, "seeded from the real clock, not defaulted");
        assert_eq!(seeded.monotonic, 0, "zero is the epoch, not missing data");
    }
}
