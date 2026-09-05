//! What the Renderer's poll loop actually did, printed on an interval, behind `OBLISK_PROFILE_IDLE`.
//!
//! Every performance question asked of this loop so far has been answered by adding a temporary
//! `eprintln!`, taking a reading, and deleting it again: how often it wakes, what wakes it, how
//! much of a core it costs, and whether a turn that woke did any work at all. That last one is the
//! bug shape this exists to catch. Since ADR-0124 the loop polls with no timeout, so a turn that
//! wakes and finds nothing to do means something re-armed a wakeup for no reason -- a spin, and
//! the difference between the 0.34% of a core this process costs idle and a hot laptop. That
//! figure, and every other number here, is a release build: a `dev` build resolves roughly four
//! times slower, so a debug reading compared against a release one invents a regression that is
//! not there. Compare like with like, or the profile lies to you.
//!
//! Off unless `OBLISK_PROFILE_IDLE` is set, and off is genuinely free: the whole struct is behind
//! an `Option` the loop checks with `if let`, so a build with it unset does two branch-predicted
//! tests per turn and touches no clock. Set it to a report interval in seconds
//! (`OBLISK_PROFILE_IDLE=10`); anything unparseable falls back to [`DEFAULT_INTERVAL_SECS`] rather
//! than refusing to start, since the point is a diagnostic that turns on when asked.

use std::time::{Duration, Instant};

use nix::sys::resource::{UsageWho, getrusage};

/// The report interval used when `OBLISK_PROFILE_IDLE` is set to something that isn't a positive
/// number of seconds. Ten seconds is long enough that the idle case reports single-digit turns,
/// which is what makes an unexpected hundred obvious at a glance.
const DEFAULT_INTERVAL_SECS: u64 = 10;

/// A turn's worth of work, filled in by the loop body once it knows what happened. Every field is
/// something the loop already computed to decide what to do; nothing here is measured for the
/// profile's sake.
#[derive(Clone, Copy, Default)]
pub struct Turn {
    /// `dispatch_pending` handed the handlers at least one Wayland event.
    pub dispatched: bool,
    /// `re_resolve_if_dirty` rebuilt the tree.
    pub re_resolved: bool,
    /// A keystroke changed a text field.
    pub typed: bool,
    /// At least one background image decode landed.
    pub decoded: bool,
    /// How many `ActivateDraw` nonces this turn serviced.
    pub draws: usize,
    /// The turn reached `repaint_mapped_surfaces`.
    pub painted: bool,
    /// How many surfaces that repaint actually drew and swapped. The gap between this and
    /// `painted` is the point: `App::paint_surface` builds a display list for every mapped
    /// surface and then declines the ones whose list is unchanged, so a window reporting
    /// `resolve=18 drawn=0` is eighteen whole-scene re-resolves that moved not one pixel --
    /// ADR-0044 decision 2's single global dirty flag, costing exactly what it was always going
    /// to cost. Still a count the paint path decides for its own reasons, not for this one's.
    pub drawn: usize,
}

impl Turn {
    /// Whether this turn did anything. A woken turn that answers `false` is the spin signature.
    fn did_work(self) -> bool {
        self.dispatched || self.re_resolved || self.typed || self.decoded || self.draws > 0
    }
}

/// Where a turn's time went. Unlike [`Turn`], these *are* measured for the profile's sake -- three
/// `clock_gettime` calls a turn -- which is why [`Phases::start`] takes the switch: with the
/// profile off every `mark` below is a branch on a `None` and nothing else, and the loop touches
/// no clock. The split is the answer to what a `drawn=0` window provokes, so it is deliberately
/// not a second environment variable: nobody has ever wanted one of these two without the other.
#[derive(Clone, Copy, Default)]
pub struct Phases {
    /// Start of the phase currently being timed, or `None` when the profile is off.
    at: Option<Instant>,
    resolve: Duration,
    surface_state: Duration,
    repaint: Duration,
}

impl Phases {
    /// Begins a turn's timing, or doesn't. Pass `profile.is_some()`.
    pub fn start(on: bool) -> Self {
        Self { at: on.then(Instant::now), ..Self::default() }
    }

    /// Closes the current phase and opens the next. Called unconditionally after each phase's
    /// block, including a block that didn't run: a skipped phase reporting zero is the truth, and
    /// it keeps the next phase from being credited with time it didn't spend.
    fn split(&mut self) -> Duration {
        let Some(at) = self.at else { return Duration::ZERO };
        let now = Instant::now();
        self.at = Some(now);
        now.duration_since(at)
    }

    pub fn mark_resolve(&mut self) {
        self.resolve = self.split();
    }

    pub fn mark_surface_state(&mut self) {
        self.surface_state = self.split();
    }

    pub fn mark_repaint(&mut self) {
        self.repaint = self.split();
    }
}

/// Which of the two polled fds was ready. Both can be, and neither can: `poll` returning zero or
/// an error leaves the loop to try again, which is itself worth counting.
#[derive(Clone, Copy)]
pub struct Wake {
    pub wayland: bool,
    pub waker: bool,
}

/// Counts for one report window. Separated from the [`IdleProfile`] that owns the clock so
/// [`render`] is a pure function of a window's worth of facts, testable without waiting ten
/// seconds for one.
#[derive(Default, Clone, Copy)]
pub struct Counters {
    turns: u64,
    idle_turns: u64,
    wake_wayland: u64,
    wake_waker: u64,
    wake_both: u64,
    wake_neither: u64,
    dispatched: u64,
    re_resolved: u64,
    typed: u64,
    decoded: u64,
    draws: u64,
    painted: u64,
    drawn: u64,
    resolve: Duration,
    surface_state: Duration,
    repaint: Duration,
}

/// CPU consumed over a window, in seconds: the whole process against this one thread. The gap
/// between them is the shaping worker, the socket thread and tokio, so a report where `process`
/// is much larger than `main` says to go and look at a thread this loop doesn't own.
#[derive(Clone, Copy, Default)]
struct Cpu {
    process: f64,
    thread: f64,
}

impl Cpu {
    /// `getrusage` for both scopes, or zeros if the kernel refuses. A refused reading costs the
    /// CPU columns, not the report.
    fn now() -> Self {
        let seconds = |who| {
            getrusage(who).map_or(0.0, |usage| {
                let user = usage.user_time();
                let system = usage.system_time();
                let secs = |t: nix::sys::time::TimeVal| t.tv_sec() as f64 + t.tv_usec() as f64 / 1_000_000.0;
                secs(user) + secs(system)
            })
        };
        Self { process: seconds(UsageWho::RUSAGE_SELF), thread: seconds(UsageWho::RUSAGE_THREAD) }
    }

    fn since(self, earlier: Self) -> Self {
        Self { process: self.process - earlier.process, thread: self.thread - earlier.thread }
    }
}

/// The loop's own accumulator: counts turns and wakes, and prints a line every `interval`.
pub struct IdleProfile {
    interval: Duration,
    window_started: Instant,
    cpu_at_window_start: Cpu,
    counters: Counters,
}

impl IdleProfile {
    /// `Some` only when `OBLISK_PROFILE_IDLE` is set. The one call site is the loop's setup, so
    /// reading the environment here rather than at the call site keeps the whole feature in one
    /// file.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("OBLISK_PROFILE_IDLE").ok()?;
        let secs = raw.trim().parse::<u64>().ok().filter(|s| *s > 0).unwrap_or(DEFAULT_INTERVAL_SECS);
        eprintln!("[oblisk-renderer] idle profile on, reporting every {secs}s");
        Some(Self {
            interval: Duration::from_secs(secs),
            window_started: Instant::now(),
            cpu_at_window_start: Cpu::now(),
            counters: Counters::default(),
        })
    }

    /// Records which fds woke the loop. Called at the poll site, so it attributes the wake that
    /// ends this turn to the turn that runs next -- the same turn that will report the work the
    /// wake caused.
    pub fn wake(&mut self, wake: Wake) {
        match (wake.wayland, wake.waker) {
            (true, true) => self.counters.wake_both += 1,
            (true, false) => self.counters.wake_wayland += 1,
            (false, true) => self.counters.wake_waker += 1,
            (false, false) => self.counters.wake_neither += 1,
        }
    }

    /// Records one turn's work and prints the report when the window is up.
    pub fn turn(&mut self, turn: Turn, phases: Phases) {
        let c = &mut self.counters;
        c.turns += 1;
        if !turn.did_work() {
            c.idle_turns += 1;
        }
        c.dispatched += u64::from(turn.dispatched);
        c.re_resolved += u64::from(turn.re_resolved);
        c.typed += u64::from(turn.typed);
        c.decoded += u64::from(turn.decoded);
        c.draws += turn.draws as u64;
        c.painted += u64::from(turn.painted);
        c.drawn += turn.drawn as u64;
        c.resolve += phases.resolve;
        c.surface_state += phases.surface_state;
        c.repaint += phases.repaint;

        let elapsed = self.window_started.elapsed();
        if elapsed < self.interval {
            return;
        }
        let cpu = Cpu::now();
        eprintln!("[oblisk-renderer] {}", render(elapsed, &self.counters, cpu.since(self.cpu_at_window_start)));
        self.window_started = Instant::now();
        self.cpu_at_window_start = cpu;
        self.counters = Counters::default();
    }
}

/// One report line. Pure, so the shape below is a test rather than something to squint at in a
/// log. `SPIN` is appended when most turns did nothing and there were enough of them to mean it:
/// a handful of idle turns is ordinary (a Wayland event this client ignores), a hundred a second
/// is the bug.
fn render(window: Duration, c: &Counters, cpu: Cpu) -> String {
    let secs = window.as_secs_f64().max(f64::MIN_POSITIVE);
    let percent = |seconds: f64| seconds / secs * 100.0;
    let spinning = c.turns >= 100 && c.idle_turns * 2 > c.turns;
    format!(
        "idle {:.1}s: turns={} idle={} cpu proc={:.2}% main={:.2}% | wake wl={} wake={} both={} none={} \
         | work dispatch={} resolve={} type={} decode={} draw={} paint={} drawn={} \
         | ms resolve={:.1} surfstate={:.1} repaint={:.1}{}",
        secs,
        c.turns,
        c.idle_turns,
        percent(cpu.process),
        percent(cpu.thread),
        c.wake_wayland,
        c.wake_waker,
        c.wake_both,
        c.wake_neither,
        c.dispatched,
        c.re_resolved,
        c.typed,
        c.decoded,
        c.draws,
        c.painted,
        c.drawn,
        c.resolve.as_secs_f64() * 1000.0,
        c.surface_state.as_secs_f64() * 1000.0,
        c.repaint.as_secs_f64() * 1000.0,
        if spinning { " SPIN" } else { "" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(turns: u64, idle_turns: u64) -> Counters {
        Counters { turns, idle_turns, ..Counters::default() }
    }

    #[test]
    fn a_turn_that_only_painted_still_counts_as_idle() {
        // `painted` is a consequence of the other flags, never a cause, so a turn carrying it
        // alone did not happen; counting it as work would hide exactly the spin this looks for.
        assert!(!Turn { painted: true, ..Turn::default() }.did_work());
        assert!(Turn { re_resolved: true, ..Turn::default() }.did_work());
        assert!(Turn { draws: 1, ..Turn::default() }.did_work());
    }

    #[test]
    fn cpu_seconds_read_as_a_percentage_of_one_core_over_the_window() {
        let line = render(Duration::from_secs(10), &counters(11, 0), Cpu { process: 0.034, thread: 0.031 });
        assert!(line.contains("cpu proc=0.34% main=0.31%"), "{line}");
    }

    #[test]
    fn a_mostly_idle_window_is_marked_only_once_it_is_busy_enough_to_mean_something() {
        // Nine turns out of ten idle, but only ten turns: an ordinary quiet window.
        assert!(!render(Duration::from_secs(10), &counters(10, 9), Cpu::default()).contains("SPIN"));
        // The same ratio at a hundred times the rate is the loop eating a core for nothing.
        assert!(render(Duration::from_secs(10), &counters(1000, 900), Cpu::default()).contains("SPIN"));
    }

    #[test]
    fn a_window_that_resolved_without_drawing_says_so_in_both_columns() {
        // The shape the whole phase split exists to make visible: every turn re-resolved the
        // scene, the resolve was nearly all of the time, and not one surface reached the GPU.
        let c = Counters {
            turns: 18,
            re_resolved: 18,
            painted: 18,
            drawn: 0,
            resolve: Duration::from_micros(21_600),
            surface_state: Duration::from_micros(1_400),
            repaint: Duration::from_micros(500),
            ..Counters::default()
        };
        let line = render(Duration::from_secs(10), &c, Cpu::default());
        assert!(line.contains("paint=18 drawn=0"), "{line}");
        assert!(line.contains("ms resolve=21.6 surfstate=1.4 repaint=0.5"), "{line}");
    }

    #[test]
    fn phases_with_the_profile_off_read_no_clock_and_report_nothing() {
        let mut phases = Phases::start(false);
        phases.mark_resolve();
        phases.mark_surface_state();
        phases.mark_repaint();
        assert!(phases.at.is_none());
        assert_eq!((phases.resolve, phases.surface_state, phases.repaint), Default::default());
    }

    #[test]
    fn each_phase_is_credited_only_with_its_own_span() {
        // A skipped phase reports zero rather than handing its neighbour the time, which is what
        // makes `surfstate=0.0` on a turn that took the `re_resolved` branch a real signal.
        let mut phases = Phases::start(true);
        std::thread::sleep(Duration::from_millis(5));
        phases.mark_resolve();
        phases.mark_surface_state();
        assert!(phases.resolve >= Duration::from_millis(5), "{:?}", phases.resolve);
        assert!(phases.surface_state < Duration::from_millis(5), "{:?}", phases.surface_state);
    }

    #[test]
    fn a_zero_length_window_reports_rather_than_dividing_by_zero() {
        let line = render(Duration::ZERO, &counters(1, 0), Cpu { process: 0.0, thread: 0.0 });
        assert!(line.contains("turns=1"), "{line}");
    }
}
