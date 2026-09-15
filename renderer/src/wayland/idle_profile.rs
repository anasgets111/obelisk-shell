//! Reports what wakes the Renderer's poll loop, what work it does, and its CPU cost under
//! `obelisk --profile`. It replaces ad-hoc probes, where each question meant a temporary
//! `eprintln!`, one reading, then deletion. ADR-0124 made polling timeout-free, so a wake with no
//! work means an unnecessary re-arm and a spin. All figures here are release-build measurements.
//! Idle costs 0.34% of a core; `dev` resolves roughly four times slower, so compare like builds.
//!
//! Off costs two predicted branches per turn and no clock reads, since the profiler is an `Option`.

use std::time::{Duration, Instant};

use nix::sys::resource::{UsageWho, getrusage};

/// Work the loop already computed for one turn; the profiler measures none of these fields.
#[derive(Clone, Copy, Default)]
pub struct Turn {
    /// `dispatch_pending` handed handlers a Wayland event.
    pub dispatched: bool,
    /// `re_resolve_if_dirty` rebuilt the tree, or a tween tick relaid it out.
    pub re_resolved: bool,
    /// A compositor frame callback advanced a tween (ADR-0145); a subset of `re_resolved`.
    pub ticked: bool,
    /// A keystroke changed a text field.
    pub typed: bool,
    /// A background image decode landed.
    pub decoded: bool,
    /// `ActivateDraw` nonces serviced this turn.
    pub draws: usize,
    /// The turn reached `repaint_mapped_surfaces`.
    pub painted: bool,
    /// Surfaces that repaint actually drew and swapped. `painted` can exceed this because
    /// `App::paint_surface` builds every mapped surface's list, then skips unchanged lists:
    /// `resolve=18 drawn=0` means eighteen whole-scene re-resolves moved no pixels, the cost of
    /// ADR-0044 decision 2's single global dirty flag.
    pub drawn: usize,
}

impl Turn {
    /// A woken turn with `false` is the spin signature.
    fn did_work(self) -> bool {
        self.dispatched || self.re_resolved || self.typed || self.decoded || self.draws > 0
    }
}

/// Measured phase times. Each turn costs three `clock_gettime` calls when enabled; with the profile
/// off, `mark` branches on `None` and the loop touches no clock. The split explains `drawn=0`, so
/// it shares the main switch rather than adding another environment variable.
#[derive(Clone, Copy, Default)]
pub struct Phases {
    /// Current phase start, or `None` when profiling is off.
    at: Option<Instant>,
    resolve: Duration,
    surface_state: Duration,
    repaint: Duration,
}

impl Phases {
    /// Starts timing when `profile.is_some()`.
    pub fn start(on: bool) -> Self {
        Self { at: on.then(Instant::now), ..Self::default() }
    }

    /// Closes the current phase and opens the next. Called after every phase, including skipped
    /// ones, so a skipped phase reports zero instead of charging its neighbor.
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

/// Readiness of the two polled fds. Both or neither can be ready; zero or an error is retried and
/// counted as neither.
#[derive(Clone, Copy)]
pub struct Wake {
    pub wayland: bool,
    pub waker: bool,
}

/// One report window, separate from [`IdleProfile`] so [`render`] stays pure and testable without
/// waiting ten seconds.
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
    ticked: u64,
    typed: u64,
    decoded: u64,
    draws: u64,
    painted: u64,
    drawn: u64,
    resolve: Duration,
    surface_state: Duration,
    repaint: Duration,
    dispatch_cpu: Duration,
    focus_turns: u64,
    focus_searched: u64,
    focus_redundant: u64,
    focus_cpu: Duration,
    focus_redundant_cpu: Duration,
}

impl Counters {
    fn focus(&mut self, cpu: Duration, searched: bool, redundant: bool) {
        self.focus_turns += 1;
        self.focus_cpu += cpu;
        self.focus_searched += u64::from(searched);
        if redundant {
            self.focus_redundant += 1;
            self.focus_redundant_cpu += cpu;
        }
    }
}

/// CPU seconds for the whole process and this thread. Their gap is shaping, the socket thread, or
/// tokio; a much larger `process` value points outside this loop.
#[derive(Clone, Copy, Default)]
struct Cpu {
    process: f64,
    thread: f64,
}

impl Cpu {
    /// `getrusage` for both scopes, or zeros if the kernel refuses; only CPU columns are lost.
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

/// Accumulates turns and wakes, printing every `interval`.
pub struct IdleProfile {
    interval: Duration,
    window_started: Instant,
    cpu_at_window_start: Cpu,
    counters: Counters,
}

impl IdleProfile {
    /// `Some` only under `--profile`.
    pub fn from_env() -> Option<Self> {
        let interval = shared::profile_interval()?;
        eprintln!("[obelisk-renderer] idle profile on, reporting every {}s", interval.as_secs());
        Some(Self {
            interval,
            window_started: Instant::now(),
            cpu_at_window_start: Cpu::now(),
            counters: Counters::default(),
        })
    }

    /// Records the poll wake for the turn that runs next, which reports the work it caused.
    pub fn wake(&mut self, wake: Wake) {
        match (wake.wayland, wake.waker) {
            (true, true) => self.counters.wake_both += 1,
            (true, false) => self.counters.wake_wayland += 1,
            (false, true) => self.counters.wake_waker += 1,
            (false, false) => self.counters.wake_neither += 1,
        }
    }

    /// Thread CPU spent inside `dispatch_pending`. Timed on its own because the phase timers start
    /// after it, so no `ms` column contains it.
    pub fn dispatch(&mut self, cpu: Duration) {
        self.counters.dispatch_cpu += cpu;
    }

    /// Thread CPU spent in end-of-turn focus maintenance, split by how much of it a tighter gate
    /// could remove.
    ///
    /// `searched` is a turn that reached the tree-cloning scope search rather than returning at
    /// the `keyboard_focus`/armed-field guards.
    ///
    /// `redundant` narrows that to turns the candidate gate would have skipped: no re-resolve, no
    /// `field_input_changed`, and an unchanged `App::focus_key`. Nothing gates on it yet, so this
    /// measures the proposal rather than the result of adopting it. It runs slightly optimistic:
    /// a reload that commits a tree whose follow-up resolve then fails also has to force arming,
    /// and that case is not sampled here.
    pub fn focus(&mut self, cpu: Duration, searched: bool, redundant: bool) {
        self.counters.focus(cpu, searched, redundant);
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
        c.ticked += u64::from(turn.ticked);
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
        eprintln!("[obelisk-renderer] {}", render(elapsed, &self.counters, cpu.since(self.cpu_at_window_start)));
        self.window_started = Instant::now();
        self.cpu_at_window_start = cpu;
        self.counters = Counters::default();
    }
}

/// Pure report formatting. `SPIN` needs at least 100 turns and more idle than busy turns: a few
/// ignored Wayland events are ordinary; a hundred idle turns per second is the bug.
fn render(window: Duration, c: &Counters, cpu: Cpu) -> String {
    let secs = window.as_secs_f64().max(f64::MIN_POSITIVE);
    let percent = |seconds: f64| seconds / secs * 100.0;
    let spinning = c.turns >= 100 && c.idle_turns * 2 > c.turns;
    format!(
        "idle {:.1}s: turns={} idle={} cpu proc={:.2}% main={:.2}% | wake wl={} wake={} both={} none={} \
         | work dispatch={} resolve={} tick={} type={} decode={} draw={} paint={} drawn={} \
         | ms resolve={:.1} surfstate={:.1} repaint={:.1} dispatch={:.1} \
         | focus turns={} searched={} redundant={} ms={:.1} redundant={:.1} ({:.2}% of a core){}",
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
        c.ticked,
        c.typed,
        c.decoded,
        c.draws,
        c.painted,
        c.drawn,
        c.resolve.as_secs_f64() * 1000.0,
        c.surface_state.as_secs_f64() * 1000.0,
        c.repaint.as_secs_f64() * 1000.0,
        c.dispatch_cpu.as_secs_f64() * 1000.0,
        c.focus_turns,
        c.focus_searched,
        c.focus_redundant,
        c.focus_cpu.as_secs_f64() * 1000.0,
        c.focus_redundant_cpu.as_secs_f64() * 1000.0,
        percent(c.focus_redundant_cpu.as_secs_f64()),
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
        assert!(!render(Duration::from_secs(10), &counters(10, 9), Cpu::default()).contains("SPIN"));
        assert!(render(Duration::from_secs(10), &counters(1000, 900), Cpu::default()).contains("SPIN"));
    }

    #[test]
    fn a_window_that_resolved_without_drawing_says_so_in_both_columns() {
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
        let mut phases = Phases::start(true);
        std::thread::sleep(Duration::from_millis(5));
        phases.mark_resolve();
        phases.mark_surface_state();
        assert!(phases.resolve >= Duration::from_millis(5), "{:?}", phases.resolve);
        assert!(phases.surface_state < Duration::from_millis(5), "{:?}", phases.surface_state);
    }

    /// The caller classifies each turn; this is the split it gets. Every turn counts toward the
    /// total whether or not it searched, and only a removable one is charged twice.
    #[test]
    fn focus_cpu_is_split_between_every_turn_and_the_removable_ones() {
        let mut c = Counters::default();
        // Guards returned early: no tree was cloned, so it is neither searched nor removable.
        c.focus(Duration::from_micros(1), false, false);
        // Searched, but the arming followed a re-resolve that may well have needed it.
        c.focus(Duration::from_micros(200), true, false);
        // Searched with nothing re-resolved: the candidate for a tighter gate.
        c.focus(Duration::from_micros(300), true, true);

        assert_eq!((c.focus_turns, c.focus_searched, c.focus_redundant), (3, 2, 1));
        assert_eq!(c.focus_cpu, Duration::from_micros(501), "every turn's cost counts once");
        assert_eq!(c.focus_redundant_cpu, Duration::from_micros(300), "only the removable turn's cost");
    }

    /// The report has to name the removable share as a percentage of a core, because that is the
    /// number the threshold is stated in.
    #[test]
    fn the_focus_columns_report_the_removable_share_of_a_core() {
        let c = Counters {
            focus_turns: 40,
            focus_searched: 30,
            focus_redundant: 25,
            focus_cpu: Duration::from_millis(60),
            focus_redundant_cpu: Duration::from_millis(45),
            ..Counters::default()
        };
        let line = render(Duration::from_secs(30), &c, Cpu::default());
        assert!(
            // Includes the separator: a folded line continuation reads as a run of spaces here.
            line.contains(
                "dispatch=0.0 | focus turns=40 searched=30 redundant=25 ms=60.0 redundant=45.0 (0.15% of a core)"
            ),
            "{line}"
        );
    }

    #[test]
    fn a_zero_length_window_reports_rather_than_dividing_by_zero() {
        let line = render(Duration::ZERO, &counters(1, 0), Cpu { process: 0.0, thread: 0.0 });
        assert!(line.contains("turns=1"), "{line}");
    }
}
