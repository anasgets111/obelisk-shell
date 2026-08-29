//! `SystemController`: one wall-clock-aligned ticking task feeding `oblisk.system`'s two fields
//! (docs/oblisk-idl-api-specs.md §2.11) -- `time`, refreshed every second, and `state`, the
//! parsed `state.json` dictionary, read once at construction and never again (see `state.rs`'s
//! doc comment for why). Structurally the sibling of `hardware::sysinfo::controller`
//! (`SysinfoController`): a `Mutex`-guarded state struct, one signal channel, one spawned task.
//! It departs from that template in two ways, both because `system` has exactly one thing to
//! tick and sysinfo has three independently-configurable ones:
//!
//! - No `poll_mode`/dormant-vs-ticking split and no `watch::Sender` interval. There is nothing
//!   to configure yet (§2.11 names no interval argument, and `system:configure` does not exist
//!   in the IDL the way `sysinfo:configure` does), so the task just ticks, unconditionally, from
//!   construction to shutdown.
//! - The tick is aligned to the wall-clock second boundary (see [`time_until_next_second`]),
//!   which sysinfo's tasks have no reason to do -- a CPU percentage does not visibly jitter by
//!   400ms, but a clock reading `14:32` a third of a second after every other clock on screen
//!   already moved to `14:33` looks broken next to them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;

/// `oblisk.system`'s two Lua-visible fields (docs/oblisk-idl-api-specs.md §2.11). Field names
/// are the `StateSnapshot` payload's JSON keys verbatim, same convention as `SysinfoState`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SystemState {
    /// Unix epoch seconds, not milliseconds -- §2.11 calls it "system time epoch" with no unit
    /// stated, and seconds is the granularity the same sentence promises ("updated at 1-second
    /// intervals"): a millisecond value would carry precision the update cadence never delivers,
    /// and a config reading it as `os.date` input (which wants seconds) would be silently wrong
    /// by a factor of 1000.
    pub time: i64,
    /// The parsed contents of `state.json`, or an empty object -- see `state::load_state`.
    /// Read-only from Lua's side: this capability has no `dispatch` (no write action is
    /// implemented; `system:write_state`, §3.2, is a separate, unbuilt write path).
    pub state: serde_json::Value,
}

/// Wakes `main.rs`'s `select!` to push a fresh `StateSnapshot` -- matches `SysinfoSignal`/
/// `PrivacySignal`'s single-variant shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSignal {
    Changed,
}

/// Whether a freshly-sampled epoch second is worth pushing. This is the filter ADR-0044 makes
/// mandatory rather than optional: every `StateSnapshot` marks the Renderer's scene dirty and
/// drives a full re-resolve and repaint of every surface (docs/adr/0044), so a task that ticks
/// and pushes unconditionally would repaint the whole scene once per internal tick even when
/// nothing a config could observe changed. Pure over "the last second actually emitted" and "the
/// second just sampled" rather than wall-clock time itself, so the decision is testable without
/// a real clock or a sleep.
///
/// `last_emitted` is `None` only before the very first tick; every real comparison after that is
/// `Some`.
pub fn should_emit(last_emitted: Option<i64>, current: i64) -> bool {
    last_emitted != Some(current)
}

/// `SystemTime::now()`'s epoch, truncated to whole seconds -- §2.11's `time` field. A thin
/// wrapper over `duration_since(UNIX_EPOCH)` exists as its own function (rather than inlined at
/// both call sites below) so the truncation itself -- seconds, not millis -- is one pinned seam
/// instead of two places that could drift apart.
pub fn epoch_seconds(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0)
}

/// How long to sleep from `elapsed_since_epoch` (a `SystemTime::now()` reading, already
/// `duration_since(UNIX_EPOCH)`'d by the caller) until the next whole-second boundary. Pure over
/// a `Duration` rather than over `SystemTime` itself, so the boundary arithmetic is testable
/// without a real clock.
///
/// ponytail: this aligns once, at task startup, and then ticks a plain steady 1-second
/// `tokio::time::interval` -- it does not re-align on every tick or track wall-clock drift
/// against the monotonic clock `tokio::time::Instant` is built on. The two clocks run at the
/// same rate under normal operation (no NTP step, no suspend/resume) closely enough that a
/// display clock does not visibly drift within any session length this shell runs for. A step
/// (NTP slew, `settimeofday`, wake from suspend) would leave the tick boundary wherever it was
/// before the step until the next restart. The upgrade path, if that is ever observed, is
/// re-deriving `time_until_next_second` after every tick instead of only before the first one
/// and rebuilding the `interval` when it disagrees with the ticker by more than a few
/// milliseconds -- not built here because it adds a re-alignment check to a 1Hz hot loop for a
/// clock skew this shell has no other reason to care about.
pub fn time_until_next_second(elapsed_since_epoch: Duration) -> Duration {
    Duration::from_secs(1) - Duration::from_nanos(u64::from(elapsed_since_epoch.subsec_nanos()))
}

pub struct SystemController {
    state: Arc<Mutex<SystemState>>,
}

impl SystemController {
    /// `home`/`xdg_state_home` are the caller's already-resolved roots (real defaults: `$HOME`
    /// and `$XDG_STATE_HOME`, read by whoever wires this into `main.rs`) -- this module never
    /// calls `std::env` itself, the same "roots are constructor parameters" shape
    /// `PrivacyController::new`'s `proc_root`/`video4linux_root` use, chosen for the same
    /// reason: a controller that read the environment internally could not be constructed
    /// against a fixture in a test.
    ///
    /// `state.json` is read synchronously, here, once, before the ticking task is spawned --
    /// see `state.rs`'s doc comment for why it is never read again. `time` is seeded to the
    /// current second immediately, so a client that connects and hydrates from
    /// `last_snapshots["system"]` before the first tick fires still sees a real clock value, not
    /// a stale zero.
    pub fn new(home: PathBuf, xdg_state_home: Option<PathBuf>, signal_tx: UnboundedSender<SystemSignal>) -> Self {
        let path = super::paths::resolve_state_path(&home, xdg_state_home.as_deref());
        let loaded = super::state::load_state(&path);
        let now = epoch_seconds(SystemTime::now());
        let state = Arc::new(Mutex::new(SystemState { time: now, state: loaded }));

        tokio::spawn(run_clock_task(Arc::clone(&state), signal_tx, now));

        Self { state }
    }

    /// The current combined state -- what `main.rs`'s signal-channel `select!` arm clones and
    /// pushes as a fresh `StateSnapshot` (mirrors `sysinfo`/`privacy`'s own `snapshot()`).
    pub fn snapshot(&self) -> SystemState {
        self.state.lock().expect("system state mutex poisoned").clone()
    }
}

/// The one ticking task. Aligns its first wakeup to the next wall-clock second boundary
/// ([`time_until_next_second`]), then ticks a plain steady one-second `tokio::time::interval`
/// forever -- there is no dormant/ticking split to race against (unlike `sysinfo`'s three tasks)
/// because nothing can reconfigure this one yet.
///
/// `last_emitted` starts at the second `SystemController::new` already seeded into `state`, so
/// the first real tick -- one second later, at the aligned boundary -- compares against that
/// seed rather than against `None`, and [`should_emit`] correctly declines to push again for the
/// same second construction already reported.
async fn run_clock_task(state: Arc<Mutex<SystemState>>, signal_tx: UnboundedSender<SystemSignal>, mut last_emitted: i64) {
    let delay = time_until_next_second(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default());
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + delay, Duration::from_secs(1));

    loop {
        ticker.tick().await;
        let now = epoch_seconds(SystemTime::now());
        if should_emit(Some(last_emitted), now) {
            state.lock().expect("system state mutex poisoned").time = now;
            last_emitted = now;
            if signal_tx.send(SystemSignal::Changed).is_err() {
                return; // main.rs's select! loop is gone; nothing left to notify
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_emit_is_false_for_the_same_second_fed_twice() {
        assert!(!should_emit(Some(1_700_000_000), 1_700_000_000));
    }

    #[test]
    fn should_emit_is_true_for_a_new_second() {
        assert!(should_emit(Some(1_700_000_000), 1_700_000_001));
    }

    #[test]
    fn should_emit_is_true_before_anything_has_ever_been_emitted() {
        assert!(should_emit(None, 1_700_000_000));
    }

    #[test]
    fn epoch_seconds_is_pinned_against_a_known_instant() {
        let known = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(epoch_seconds(known), 1_700_000_000, "must be exact -- Lua reads this straight as an integer");
    }

    #[test]
    fn epoch_seconds_is_seconds_not_milliseconds() {
        // A millis value for "now" would be roughly 1_700_000_000_000: three orders of
        // magnitude larger. Pin the magnitude so a future `as_millis()` typo fails loudly here
        // instead of silently shipping a clock that is wrong by 1000x.
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
    async fn new_seeds_time_and_state_synchronously_before_any_tick_fires() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("oblisk")).unwrap();
        std::fs::write(dir.path().join("oblisk").join("state.json"), r#"{"theme": "dark"}"#).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = SystemController::new(PathBuf::from("/nonexistent-home"), Some(dir.path().to_path_buf()), tx);
        let snapshot = controller.snapshot();

        assert_eq!(snapshot.state, serde_json::json!({"theme": "dark"}));
        let now = epoch_seconds(SystemTime::now());
        assert!((now - 2..=now).contains(&snapshot.time), "seeded time must be the real current second, not a stale zero");
    }

    #[tokio::test]
    async fn new_degrades_to_an_empty_state_object_when_no_state_json_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = SystemController::new(dir.path().to_path_buf(), None, tx);

        assert_eq!(controller.snapshot().state, serde_json::json!({}));
    }
}
