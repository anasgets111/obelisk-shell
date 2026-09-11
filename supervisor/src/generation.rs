//! A generation's authoritative identity and process handle, sibling Renderer resolution, exit
//! classification/reporting, and `RestartBrake`. `main.rs` consults the brake before replacement.
//! Promotion and retirement stay in its `select!` loop (ADR-0037), where the arm shares loop state.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// ADR-0058 decision 3: three deaths in a minute lets transient OOM/GPU-reset crashes recover,
/// while stopping a config that kills every Renderer before the lock screen flickers forever.
pub(super) const RESTART_LIMIT: usize = 3;
pub(super) const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Installed Renderer filename. `renderer` is too generic for a user's `$PATH`; `cargo install`
/// puts every binary in one directory.
pub(crate) const RENDERER_BINARY: &str = "obelisk-renderer";

/// Resolves the Renderer as a sibling of the running Supervisor.
///
/// Keeps the Renderer off `$PATH` in an install with no code behind it. On Linux `current_exe`
/// reads symlink-resolved `/proc/self/exe`, so `$PREFIX/bin/obelisk -> ../lib/obelisk/obelisk` finds
/// `$PREFIX/lib/obelisk/obelisk-renderer`; users get one command on `$PATH` and the pair stays
/// together.
///
/// The sibling rule makes `cargo run` a trap: it rebuilds one half and launches the other's stale
/// binary. `just run` builds both.
pub(crate) fn renderer_binary_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name(RENDERER_BINARY))
}

/// One generation's identity and process handle while authoritative; replaced wholesale on swap.
pub(super) struct Authoritative {
    pub(super) generation_id: u32,
    pub(super) child: tokio::process::Child,
}

/// How the authoritative Renderer ended (ADR-0058 decision 2). `Clean` is not a crash, so
/// `main`'s shutdown reap is never reported as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RendererDeparture {
    Clean,
    Failed { code: i32 },
    Signalled { signal: i32 },
}

/// Check the signal first: a signalled child has no exit code, so `code()` first returns `None` and
/// loses what happened.
pub(super) fn classify_departure(status: std::process::ExitStatus) -> RendererDeparture {
    use std::os::unix::process::ExitStatusExt;

    if let Some(signal) = status.signal() {
        return RendererDeparture::Signalled { signal };
    }
    match status.code() {
        Some(0) => RendererDeparture::Clean,
        Some(code) => RendererDeparture::Failed { code },
        // Linux `wait(2)` produces neither a code nor a signal only outside its normal shapes. A
        // panic here would kill the process that can still recover it, so return `-1` instead.
        None => RendererDeparture::Failed { code: -1 },
    }
}

/// ADR-0058 decision 3: at most `limit` restarts inside `window`. Without the brake, one dead bar
/// turns into a lock screen flickering every few hundred milliseconds.
///
/// Sliding window, not total count: a Renderer dying once a day for a month is a logged bug, not a
/// restart loop; a total would eventually refuse a shell healthy since the last reboot.
pub(super) struct RestartBrake {
    limit: usize,
    window: Duration,
    /// Restart instants in the current window, oldest first; bounded by `limit`.
    recent: std::collections::VecDeque<std::time::Instant>,
}

impl RestartBrake {
    pub(super) fn new(limit: usize, window: Duration) -> Self {
        RestartBrake { limit, window, recent: std::collections::VecDeque::new() }
    }

    /// Records an attempt at `now` and says whether it may proceed. Injecting `now` makes the
    /// window testable without a test that takes an hour.
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

/// Human-readable Renderer departure with lock state (ADR-0058 decision 2). The compositor does
/// not unlock when a lock client dies, so losing a Renderer holding `ext_session_lock_v1` costs the
/// session, not just the bar.
pub(super) fn departure_report(departure: RendererDeparture, generation_id: u32, lock_active: bool) -> String {
    let what = match departure {
        RendererDeparture::Clean => "exited cleanly".to_string(),
        RendererDeparture::Failed { code } => format!("exited with code {code}"),
        RendererDeparture::Signalled { signal } => format!("was killed by signal {signal}"),
    };
    let lock = if lock_active {
        ", and it held the session lock: the compositor does not unlock when a lock client dies, so the session stays \
         locked until a replacement takes the lock over (ADR-0058)"
    } else {
        ""
    };
    format!("generation {generation_id}'s renderer {what}{lock}")
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// Raw `wait(2)` status for normal exit `code`; keeps `<< 8` in one place.
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
        // A signalled child has no exit code; calling `code()` first returns `None` and loses this.
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

        // An hour later is a new incident; counting it would refuse a shell healthy all day.
        assert!(brake.allow(start + Duration::from_secs(3600)), "the window has long passed");
    }

    #[test]
    fn a_slow_crash_loop_never_trips_the_brake() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::new(3, Duration::from_secs(60));

        // One crash per window forever is a logged bug, not a restart loop.
        for attempt in 0..10 {
            assert!(
                brake.allow(start + Duration::from_secs(attempt * 61)),
                "crash {attempt} stands alone in its window"
            );
        }
    }
}
