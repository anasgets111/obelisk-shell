//! A generation's identity and process handle while it is authoritative (`Authoritative`),
//! spawned as a sibling of this Supervisor binary (`renderer_binary_path`), and how its exit is
//! classified and reported (`classify_departure`, `departure_report`). `RestartBrake` is the
//! crash-loop stop condition `main.rs`'s exit-handling arm consults before spawning a
//! replacement. The promote/retire transition itself -- reassigning which generation is
//! authoritative -- stays in `main.rs`'s `select!` loop (docs/adr/0037): it is one arm among many
//! sharing that loop's own state, not a step this module could run on its own.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// docs/adr/0058 decision 3: three deaths inside a minute is enough that a transient crash (an
/// OOM that has since passed, a GPU reset) recovers without a human, and few enough that a config
/// that kills every Renderer stops after three rather than flickering the lock screen forever.
pub(super) const RESTART_LIMIT: usize = 3;
pub(super) const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Resolves the Renderer binary as a sibling of the running Supervisor binary (the standard
/// same-workspace cargo layout). No packaging or install-path configuration exists yet
/// (docs/adr/0025) -- this is the only assumption available until one does.
pub(super) fn renderer_binary_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name("renderer"))
}

/// One generation's identity and process handle while it's authoritative. Reassigned wholesale
/// on a successful swap.
pub(super) struct Authoritative {
    pub(super) generation_id: u32,
    pub(super) child: tokio::process::Child,
}

/// How the authoritative Renderer's process ended (docs/adr/0058 decision 2). `Clean` is not a
/// crash: `main`'s own shutdown reap must never be reported as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RendererDeparture {
    Clean,
    Failed { code: i32 },
    Signalled { signal: i32 },
}

/// Signal before code: a signalled child has no exit code at all, so asking `code()` first
/// returns `None` and throws away the only fact that says what happened.
pub(super) fn classify_departure(status: std::process::ExitStatus) -> RendererDeparture {
    use std::os::unix::process::ExitStatusExt;

    if let Some(signal) = status.signal() {
        return RendererDeparture::Signalled { signal };
    }
    match status.code() {
        Some(0) => RendererDeparture::Clean,
        Some(code) => RendererDeparture::Failed { code },
        // Neither a code nor a signal is not a shape `wait(2)` produces on Linux. Named rather than
        // `unreachable!()`: this runs on the crash-handling path, and a panic here would kill the
        // process that can still recover it.
        None => RendererDeparture::Failed { code: -1 },
    }
}

/// docs/adr/0058 decision 3's stop condition: at most `limit` restarts inside any `window`. An
/// unbraked loop turns one dead bar into a lock screen that flickers every few hundred
/// milliseconds, harder to escape than the dead shell it was meant to fix.
///
/// A sliding window, not a total count: a Renderer that dies once a day for a month is a bug to
/// chase in the log, not a loop to stop restarting, and a total count would eventually refuse to
/// restart a shell that had been healthy since the last reboot.
pub(super) struct RestartBrake {
    limit: usize,
    window: Duration,
    /// Restart instants inside the current window, oldest first. Bounded by `limit`.
    recent: std::collections::VecDeque<std::time::Instant>,
}

impl RestartBrake {
    pub(super) fn new(limit: usize, window: Duration) -> Self {
        RestartBrake { limit, window, recent: std::collections::VecDeque::new() }
    }

    /// Records a restart attempt at `now` and answers whether it may proceed. `now` is a
    /// parameter, not read from the clock, so the window is testable without a test that takes
    /// an hour.
    pub(super) fn allow(&mut self, now: std::time::Instant) -> bool {
        while self.recent.front().is_some_and(|at| now.duration_since(*at) >= self.window) {
            self.recent.pop_front();
        }
        if self.recent.len() >= self.limit {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// The line a human reads when the Renderer goes away, carrying whether a lock was live
/// (docs/adr/0058 decision 2). The compositor does not unlock when a lock client dies, so a
/// Renderer that dies holding `ext_session_lock_v1` costs the session now, not just a bar.
pub(super) fn departure_report(departure: RendererDeparture, generation_id: u32, lock_active: bool) -> String {
    let what = match departure {
        RendererDeparture::Clean => "exited cleanly".to_string(),
        RendererDeparture::Failed { code } => format!("exited with code {code}"),
        RendererDeparture::Signalled { signal } => format!("was killed by signal {signal}"),
    };
    let lock = if lock_active {
        ", and it held the session lock: the compositor does not unlock when a lock client dies, so the session stays \
         locked until a replacement takes the lock over (docs/adr/0058)"
    } else {
        ""
    };
    format!("generation {generation_id}'s renderer {what}{lock}")
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// The raw `wait(2)` status for a normal exit with `code`. Written out so `<< 8` appears once.
    fn exited(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code << 8)
    }

    fn killed_by(signal: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(signal)
    }

    #[test]
    fn a_renderer_that_exits_zero_is_not_a_crash() {
        assert_eq!(classify_departure(exited(0)), RendererDeparture::Clean);
    }

    #[test]
    fn a_renderer_that_exits_nonzero_carries_its_code() {
        assert_eq!(classify_departure(exited(101)), RendererDeparture::Failed { code: 101 });
    }

    #[test]
    fn a_renderer_killed_by_a_signal_reports_the_signal_not_an_exit_code() {
        // A signalled child has no exit code at all, so a classifier reaching for `code()` first
        // reports `None` and loses the only fact that says what happened.
        assert_eq!(classify_departure(killed_by(9)), RendererDeparture::Signalled { signal: 9 });
    }

    #[test]
    fn a_departure_while_locked_says_the_session_stays_locked() {
        let report = departure_report(RendererDeparture::Signalled { signal: 9 }, 3, true);

        assert!(report.contains("signal 9"), "the signal has to survive into the message: {report}");
        assert!(
            report.contains("session stays locked"),
            "a Renderer that died holding the lock is a different emergency from one that died without it, and the \
             message is the only place that distinction reaches a human: {report}"
        );
    }

    #[test]
    fn a_departure_while_unlocked_does_not_mention_the_lock() {
        let report = departure_report(RendererDeparture::Failed { code: 101 }, 3, false);

        assert!(report.contains("code 101"), "{report}");
        assert!(!report.contains("locked"), "an unlocked crash must not cry lock: {report}");
    }

    #[test]
    fn the_brake_allows_restarts_up_to_its_limit() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::new(3, Duration::from_secs(60));

        for attempt in 0..3 {
            assert!(brake.allow(start + Duration::from_secs(attempt)), "restart {attempt} is within the limit");
        }
    }

    #[test]
    fn the_brake_stops_a_crash_loop_once_the_limit_is_reached_inside_the_window() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::new(3, Duration::from_secs(60));
        for attempt in 0..3 {
            brake.allow(start + Duration::from_secs(attempt));
        }

        assert!(
            !brake.allow(start + Duration::from_secs(4)),
            "a config that kills every Renderer it is handed must stop being handed Renderers"
        );
    }

    #[test]
    fn the_brake_forgets_restarts_older_than_its_window() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::new(3, Duration::from_secs(60));
        for attempt in 0..3 {
            brake.allow(start + Duration::from_secs(attempt));
        }

        // An hour later is not the same incident; counting it as one would refuse to restart a
        // shell that had been healthy all day.
        assert!(brake.allow(start + Duration::from_secs(3600)), "the window has long passed");
    }

    #[test]
    fn a_slow_crash_loop_never_trips_the_brake() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::new(3, Duration::from_secs(60));

        // One crash per window, forever: a bug to chase in the log, not a loop to stop restarting.
        for attempt in 0..10 {
            assert!(brake.allow(start + Duration::from_secs(attempt * 61)), "crash {attempt} stands alone in its window");
        }
    }
}
