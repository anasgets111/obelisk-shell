//! `animate`: per-property tweens on a retained node, the engine's answer to QML's `Behavior on x {
//! NumberAnimation { ... } }` (ADR-0145). A node names the properties it wants eased and how long;
//! when a pass resolves a different target for one of them, the node's [`Tween`] carries the
//! displayed value from where it was to where it is going, and `layout::scene::Scene::tick`
//! advances it between passes without running any Lua.
//!
//! This module owns the parsing and the arithmetic. Where the tween lives, when one starts and
//! what a tick relays out are `layout::scene`'s.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Lua, Value};

use super::style::{axis_default, parse_percent, range_of};
use super::{LayoutError, Rgba, invalid, parse_hex_color, preview_for_error, value_as_f32};

/// The one thing a hex colour has to look like to reach `parse_hex_color` again next pass.
fn hex_of(color: Rgba) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", byte(color.r), byte(color.g), byte(color.b), byte(color.a))
}

/// QML's `Easing.Type` names, spelled the same so a
/// `Behavior on width { NumberAnimation { easing.type: Easing.OutCubic } }` ports by dropping the
/// prefix, plus CSS's two curves QML has no name for: an arbitrary cubic Bezier and a step
/// function (ADR-0151).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Easing {
    Linear,
    InQuad,
    OutQuad,
    /// The default: the one the reference config wrote most often, and the one that reads as
    /// motion rather than a snap when nobody chose.
    #[default]
    InOutQuad,
    InCubic,
    OutCubic,
    InOutCubic,
    InQuart,
    OutQuart,
    InOutQuart,
    InQuint,
    OutQuint,
    InOutQuint,
    InSine,
    OutSine,
    InOutSine,
    InExpo,
    OutExpo,
    InOutExpo,
    InCirc,
    OutCirc,
    InOutCirc,
    /// The three that overshoot past the target and settle. The tween clamps the result to the
    /// property's legal range, so a `width` easing to `0` never goes negative into the parser.
    InBack,
    OutBack,
    InOutBack,
    InElastic,
    OutElastic,
    InOutElastic,
    InBounce,
    OutBounce,
    InOutBounce,
    /// CSS `cubic-bezier(x1, y1, x2, y2)`: the two control points of a curve from `(0, 0)` to
    /// `(1, 1)`. Written as `easing = { x1, y1, x2, y2 }`.
    Bezier {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
    /// CSS `steps(n)`: `n` equal jumps, the last landing on the target. Written as
    /// `easing = { steps = n }`.
    Steps(u32),
}

impl Easing {
    const NAMES: &[(&str, Easing)] = &[
        ("Linear", Easing::Linear),
        ("InQuad", Easing::InQuad),
        ("OutQuad", Easing::OutQuad),
        ("InOutQuad", Easing::InOutQuad),
        ("InCubic", Easing::InCubic),
        ("OutCubic", Easing::OutCubic),
        ("InOutCubic", Easing::InOutCubic),
        ("InQuart", Easing::InQuart),
        ("OutQuart", Easing::OutQuart),
        ("InOutQuart", Easing::InOutQuart),
        ("InQuint", Easing::InQuint),
        ("OutQuint", Easing::OutQuint),
        ("InOutQuint", Easing::InOutQuint),
        ("InSine", Easing::InSine),
        ("OutSine", Easing::OutSine),
        ("InOutSine", Easing::InOutSine),
        ("InExpo", Easing::InExpo),
        ("OutExpo", Easing::OutExpo),
        ("InOutExpo", Easing::InOutExpo),
        ("InCirc", Easing::InCirc),
        ("OutCirc", Easing::OutCirc),
        ("InOutCirc", Easing::InOutCirc),
        ("InBack", Easing::InBack),
        ("OutBack", Easing::OutBack),
        ("InOutBack", Easing::InOutBack),
        ("InElastic", Easing::InElastic),
        ("OutElastic", Easing::OutElastic),
        ("InOutElastic", Easing::InOutElastic),
        ("InBounce", Easing::InBounce),
        ("OutBounce", Easing::OutBounce),
        ("InOutBounce", Easing::InOutBounce),
    ];

    /// Back's overshoot constant and Elastic's period, Penner's originals, the numbers QML and
    /// every CSS easing cheat sheet use. Named so the arms below read as the shape and not the
    /// arithmetic. Each family's `InOut` uses a wider constant than its `In` and `Out` do, which
    /// is why those two arms are written out rather than reflected.
    const BACK: f32 = 1.70158;
    const BACK_IN_OUT: f32 = Self::BACK * 1.525;
    const ELASTIC: f32 = 2.0 * std::f32::consts::PI / 3.0;
    const ELASTIC_IN_OUT: f32 = 2.0 * std::f32::consts::PI / 4.5;

    fn parse(name: &str) -> Option<Self> {
        Self::NAMES.iter().find(|(spelling, _)| *spelling == name).map(|(_, easing)| *easing)
    }

    /// One curve's `Out` from its `In`, and its `InOut` from both: reflection through the centre,
    /// which is how Penner defined the families and why only the `In` arm below is written out.
    fn out_of(inward: impl Fn(f32) -> f32, t: f32) -> f32 {
        1.0 - inward(1.0 - t)
    }

    fn in_out_of(inward: impl Fn(f32) -> f32 + Copy, t: f32) -> f32 {
        if t < 0.5 { 0.5 * inward(2.0 * t) } else { 0.5 + 0.5 * Self::out_of(inward, 2.0 * t - 1.0) }
    }

    /// Progress `t` in `[0, 1]` to the eased fraction of the distance covered. May leave `[0, 1]`
    /// for the overshooting families; [`Animatable::lerp`] clamps to the property's own range.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        let power = |p: i32| move |t: f32| t.powi(p);
        let sine = |t: f32| 1.0 - (t * std::f32::consts::FRAC_PI_2).cos();
        let expo = |t: f32| if t <= 0.0 { 0.0 } else { (10.0 * (t - 1.0)).exp2() };
        let circ = |t: f32| 1.0 - (1.0 - t * t).max(0.0).sqrt();
        let back = |t: f32| (Self::BACK + 1.0) * t.powi(3) - Self::BACK * t.powi(2);
        let elastic = |t: f32| {
            if t <= 0.0 || t >= 1.0 {
                return t;
            }
            -(10.0 * (t - 1.0)).exp2() * ((t * 10.0 - 10.75) * Self::ELASTIC).sin()
        };
        match self {
            Easing::Linear => t,
            Easing::InQuad => power(2)(t),
            Easing::OutQuad => Self::out_of(power(2), t),
            Easing::InOutQuad => Self::in_out_of(power(2), t),
            Easing::InCubic => power(3)(t),
            Easing::OutCubic => Self::out_of(power(3), t),
            Easing::InOutCubic => Self::in_out_of(power(3), t),
            Easing::InQuart => power(4)(t),
            Easing::OutQuart => Self::out_of(power(4), t),
            Easing::InOutQuart => Self::in_out_of(power(4), t),
            Easing::InQuint => power(5)(t),
            Easing::OutQuint => Self::out_of(power(5), t),
            Easing::InOutQuint => Self::in_out_of(power(5), t),
            Easing::InSine => sine(t),
            Easing::OutSine => Self::out_of(sine, t),
            Easing::InOutSine => Self::in_out_of(sine, t),
            Easing::InExpo => expo(t),
            Easing::OutExpo => Self::out_of(expo, t),
            Easing::InOutExpo => Self::in_out_of(expo, t),
            Easing::InCirc => circ(t),
            Easing::OutCirc => Self::out_of(circ, t),
            Easing::InOutCirc => Self::in_out_of(circ, t),
            Easing::InBack => back(t),
            Easing::OutBack => Self::out_of(back, t),
            Easing::InOutBack => {
                let c = Self::BACK_IN_OUT;
                if t < 0.5 {
                    (2.0 * t).powi(2) * ((c + 1.0) * 2.0 * t - c) / 2.0
                } else {
                    ((2.0 * t - 2.0).powi(2) * ((c + 1.0) * (2.0 * t - 2.0) + c) + 2.0) / 2.0
                }
            }
            Easing::InElastic => elastic(t),
            Easing::OutElastic => Self::out_of(elastic, t),
            Easing::InOutElastic => {
                if t <= 0.0 || t >= 1.0 {
                    return t;
                }
                let swing = ((t * 20.0 - 11.125) * Self::ELASTIC_IN_OUT).sin();
                if t < 0.5 {
                    -((20.0 * t - 10.0).exp2() * swing) / 2.0
                } else {
                    (-20.0f32).mul_add(t, 10.0).exp2() * swing / 2.0 + 1.0
                }
            }
            Easing::InBounce => bounce_in(t),
            Easing::OutBounce => Self::out_of(bounce_in, t),
            Easing::InOutBounce => Self::in_out_of(bounce_in, t),
            Easing::Bezier { x1, y1, x2, y2 } => bezier_at(x1, y1, x2, y2, t),
            // CSS `steps(n, jump-end)`: the value holds through each step and the last lands on
            // the target, so `t == 1` is the only progress that reaches it.
            Easing::Steps(n) => (t * n as f32).floor() / n as f32,
        }
    }
}

/// Penner's bounce, written `In` so the reflections above build the other two. Four parabolas of
/// shrinking height, the constants his originals use.
fn bounce_in(t: f32) -> f32 {
    const N: f32 = 7.5625;
    const D: f32 = 2.75;
    let t = 1.0 - t;
    let out = if t < 1.0 / D {
        N * t * t
    } else if t < 2.0 / D {
        let t = t - 1.5 / D;
        N * t * t + 0.75
    } else if t < 2.5 / D {
        let t = t - 2.25 / D;
        N * t * t + 0.9375
    } else {
        let t = t - 2.625 / D;
        N * t * t + 0.984375
    };
    1.0 - out
}

/// CSS `cubic-bezier`: the curve's `y` at the progress whose `x` is `t`. The `x` component is
/// strictly increasing over `[0, 1]` because both control `x` are held there, so a bisection
/// finds the parameter.
// ponytail: bisection, not Newton. Twenty halvings pin the parameter to under 1e-6 of `t`, which
// is finer than a frame of a 60 s animation, and it cannot diverge the way Newton does on a curve
// with a near-flat segment. Swap in Newton with a bisection fallback if a profile ever shows this.
fn bezier_at(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let curve = |a: f32, b: f32, p: f32| {
        let inv = 1.0 - p;
        3.0 * inv * inv * p * a + 3.0 * inv * p * p * b + p * p * p
    };
    // Both ends are on the curve exactly. Bisecting toward one lands a parameter about 1e-6 away
    // instead, which a control point far outside the unit square turns into a visible jump: the
    // `y` of a bezier is unbounded, so `{ 0, 1000000, 1, 1000000 }` starts a tween half again past
    // its target rather than on its source.
    if t <= 0.0 || t >= 1.0 {
        return t.clamp(0.0, 1.0);
    }
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = 0.5 * (low + high);
        if curve(x1, x2, mid) < t { low = mid } else { high = mid }
    }
    curve(y1, y2, 0.5 * (low + high))
}

/// How one property eases: `animate = { width = 200 }` or
/// `animate = { width = { duration = 200, easing = "OutCubic", from = 0 } }`.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimationSpec {
    /// What kind of motion this is, and the only place its timing lives.
    pub motion: Motion,
    /// How long the property holds still before the motion starts (ADR-0153). Zero when absent.
    /// It offsets a sequence's whole run, loops and all, rather than each cycle.
    pub delay: Duration,
    /// Where a node that has never displayed this property starts: a fresh node's entry, or a
    /// property it did not carry last pass. Absent means the value is taken as it is.
    pub from: Option<Animatable>,
}

/// How a property gets where it is going. The three are exclusive, and which one an entry names
/// decides which of its other keys mean anything -- a `duration` beside `keyframes` is the frames'
/// default, and beside a `spring` it is nothing at all, which the parser refuses rather than let
/// this enum carry a dead field (ADR-0154).
#[derive(Debug, Clone, PartialEq)]
pub enum Motion {
    /// One pass along a curve of progress, from what the node displays to what a pass resolved
    /// (ADR-0145).
    Eased { duration: Duration, easing: Easing },
    /// The property walks a list of values and reads nothing resolved for it (ADR-0152).
    Sequence(Sequence),
    /// A mass on a spring: no duration, and it carries its velocity through a change of target
    /// (ADR-0154).
    Spring(Spring),
}

/// Which closed-form solution a spring's constants put it in. Underdamped rings past the target,
/// overdamped crawls in without reaching it, and the boundary between them is its own formula
/// because both of the others divide by the distance to it.
enum Regime {
    /// Underdamped, carrying the frequency it rings at.
    Ringing(f32),
    /// Overdamped, carrying its two decay rates, the faster first.
    Crawling(f32, f32),
    /// Critically damped.
    Critical,
}

/// A mass on a spring, in units of the displacement it has left to cross: it starts one
/// displacement from the target and settles on it, so one scalar drives a number, a percent, a
/// colour and an edge table alike, and the rest threshold below is dimensionless rather than
/// needing to know pixels from opacity.
///
/// Solved in closed form rather than integrated per frame. [`Tween::at`] has to be a pure
/// function of elapsed time -- a pass and a tick both call it, and the value carries no state
/// across reconciliation (ADR-0152) -- so stepping a velocity forward per frame would be a
/// second source of truth and would drift with the frame rate. The closed form also hands over
/// an exact rate when the target moves, which is the whole reason a spring is here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    /// The pull toward the target, `k/m`, per second squared.
    pub stiffness: f32,
    /// The drag on the way, `c/m`, per second. `2 * sqrt(stiffness)` is critical damping: below
    /// it the spring overshoots and rings, above it it crawls in without ever crossing.
    pub damping: f32,
    /// Where the displacement is already heading when this run begins, as a fraction of that
    /// displacement per second. Zero for a spring starting at rest; a retarget hands the running
    /// spring's own rate over here, which is how the motion keeps its velocity through a change
    /// of target instead of restarting from still.
    pub velocity: f32,
    /// When the displacement is inside [`Spring::REST`] for good, computed once at parse from a
    /// bound on the envelope. Conservative on purpose: too long only keeps a tween that is
    /// already sitting on its target, while too short would drop it mid-flight.
    settles: Duration,
}

impl Spring {
    /// How close to the target counts as arrived, as a fraction of the original displacement.
    /// A thousandth of the distance is under half a pixel on anything this shell lays out and
    /// under a step of 8-bit colour.
    const REST: f32 = 1e-3;

    /// No spring may ask for frames for longer than this, whatever its constants say.
    const LONGEST: f32 = 60.0;

    /// The most velocity a hand-over may carry, in displacements per second. A target that lands
    /// almost where the value already is makes the projection below enormous; without a bound the
    /// next run would shoot away from a target it was arriving at.
    const FASTEST: f32 = 100.0;

    pub fn new(stiffness: f32, damping: f32, velocity: f32) -> Self {
        let mut spring = Self { stiffness, damping, velocity, settles: Duration::ZERO };
        spring.settles = Duration::from_secs_f32(spring.settle_time());
        spring
    }

    /// `zeta * omega0`, the rate the envelope decays at, and `omega0^2 - (zeta * omega0)^2`, whose
    /// sign says which of the three solutions applies. Both fall out of `damping` and `stiffness`
    /// alone, so every arm below reads them rather than recomputing the algebra.
    fn decay_and_gap(&self) -> (f32, f32) {
        let half_damping = self.damping / 2.0;
        (half_damping, self.stiffness - half_damping * half_damping)
    }

    /// Displacement left, as a fraction of the original: `1` when the run begins and `0` on the
    /// target. It may pass through zero and come back, which is the overshoot, and the caller
    /// clamps that to the property's own range exactly as it does `OutBack`'s.
    fn displacement(&self, seconds: f32) -> f32 {
        let (decay, gap) = self.decay_and_gap();
        // `s(0) = 1` and `s'(0) = -velocity` in all three arms: the run starts one displacement
        // out and is already closing at `velocity` displacements per second.
        let slope = decay - self.velocity;
        match self.regime(gap) {
            Regime::Ringing(ringing) => {
                (-decay * seconds).exp() * ((ringing * seconds).cos() + slope / ringing * (ringing * seconds).sin())
            }
            Regime::Crawling(fast, slow) => {
                let (near, far) = self.overdamped_weights(fast, slow);
                near * (fast * seconds).exp() + far * (slow * seconds).exp()
            }
            Regime::Critical => (1.0 + slope * seconds) * (-decay * seconds).exp(),
        }
    }

    /// The rate `displacement` is changing at, which is negative while the spring closes on its
    /// target. What a retarget hands to the next run.
    fn rate(&self, seconds: f32) -> f32 {
        let (decay, gap) = self.decay_and_gap();
        let slope = decay - self.velocity;
        match self.regime(gap) {
            // Differentiating `exp(-decay t) * (cos(w t) + (slope / w) sin(w t))` and collecting
            // the two terms; at `t = 0` it is `slope - decay`, which is `-velocity`.
            Regime::Ringing(ringing) => {
                let (cos, sin) = ((ringing * seconds).cos(), (ringing * seconds).sin());
                (-decay * seconds).exp() * ((slope - decay) * cos - (ringing + slope * decay / ringing) * sin)
            }
            Regime::Crawling(fast, slow) => {
                let (near, far) = self.overdamped_weights(fast, slow);
                near * fast * (fast * seconds).exp() + far * slow * (slow * seconds).exp()
            }
            Regime::Critical => (slope - decay * (1.0 + slope * seconds)) * (-decay * seconds).exp(),
        }
    }

    /// Which of the three solutions applies. The comparison is against a fraction of `stiffness`
    /// rather than a fixed number because `gap` is in units of stiffness: an absolute threshold
    /// would call a soft spring critical and a stiff one never.
    fn regime(&self, gap: f32) -> Regime {
        if gap.abs() <= self.stiffness * 1e-6 {
            return Regime::Critical;
        }
        if gap > 0.0 {
            return Regime::Ringing(gap.sqrt());
        }
        let (decay, _) = self.decay_and_gap();
        let spread = (-gap).sqrt();
        // The far root is the sum of two terms of one sign, so it loses nothing. The near one is
        // their difference, and for a heavily overdamped spring they agree to every bit an `f32`
        // has: `stiffness = 0.0001, damping = 10000` gives `spread` exactly `decay`, a near root
        // of exactly zero, and a displacement that never changes -- the value would sit still for
        // a minute and then jump. The roots multiply to `stiffness`, so the near one comes from
        // the far one instead of from a subtraction.
        let far = -decay - spread;
        Regime::Crawling(far, self.stiffness / far)
    }

    /// How the starting displacement and rate split between those two rates.
    fn overdamped_weights(&self, fast: f32, slow: f32) -> (f32, f32) {
        let near = (-self.velocity - slow) / (fast - slow);
        (near, 1.0 - near)
    }

    /// A bound on how long the displacement takes to fall inside [`Self::REST`] and stay there.
    /// Every arm bounds the solution above by `amplitude * exp(-rate * t)` and inverts that, so
    /// the answer is never early. The critically damped arm carries a linear factor that no plain
    /// exponential bounds, so it is charged to half the decay and the factor's own maximum.
    fn settle_time(&self) -> f32 {
        let (decay, gap) = self.decay_and_gap();
        let slope = decay - self.velocity;
        let (amplitude, rate) = match self.regime(gap) {
            Regime::Ringing(ringing) => ((1.0 + (slope / ringing).powi(2)).sqrt(), decay),
            Regime::Crawling(fast, slow) => {
                let (near, far) = self.overdamped_weights(fast, slow);
                // The slower root is the one still moving once the other has gone.
                (near.abs() + far.abs(), -slow)
            }
            Regime::Critical => {
                let half = decay / 2.0;
                // `(1 + slope * t) * exp(-half * t)` peaks where its derivative vanishes; before
                // that point it has not yet grown, so the value at `t = 0` stands.
                let peak = (1.0 / half - 1.0 / slope.abs()).max(0.0);
                ((1.0 + slope.abs() * peak) * (-half * peak).exp(), half)
            }
        };
        if rate <= 0.0 {
            return Self::LONGEST;
        }
        ((amplitude / Self::REST).max(1.0).ln() / rate).clamp(0.0, Self::LONGEST)
    }

    /// Progress toward the target: `0` at the start, `1` on it, and past `1` while it overshoots.
    /// Pinned exactly to `1` once settled so the property lands on the value a pass resolved
    /// rather than a thousandth away from it.
    fn at(&self, elapsed: Duration) -> f32 {
        if elapsed >= self.settles {
            return 1.0;
        }
        1.0 - self.displacement(elapsed.as_secs_f32())
    }

    fn done(&self, elapsed: Duration) -> bool {
        elapsed >= self.settles
    }

    /// The pair a config wrote, apart from the velocity a retarget handed this one. Two springs
    /// agreeing here are the same spring at different points of the same motion.
    fn constants(&self) -> (f32, f32) {
        (self.stiffness, self.damping)
    }

    /// This spring's constants, started at the rate `running` had reached rather than at rest.
    ///
    /// Both runs read `value = to + s * (from - to)`, so the value's own rate is `s'` times the
    /// displacement it is measured against. Matching the two across the hand-over gives the new
    /// run's starting rate as the old one's projected onto the new displacement, which is exact
    /// for a single number and the closest one scalar comes for a colour or an edge table whose
    /// components are not moving in step.
    ///
    /// ponytail: one scalar for every shape, so a colour crossing a hue keeps its speed but not
    /// its direction per channel. Carrying an `Animatable`-shaped velocity would fix that, and is
    /// worth doing when something animates a colour by spring and the difference shows.
    fn handed(self, running: &Tween, displayed: Animatable, target: Animatable, now: Instant) -> Self {
        let Motion::Spring(prior) = &running.spec.motion else { return self };
        // The run's whole displacement, not what is left of it. `displayed - running.to` is that
        // whole displacement already scaled by how much remains, so projecting it would hand over
        // the true velocity times the fraction still to cross: near zero for a retarget late in a
        // run that is still moving briskly, and sign-flipped once an overshoot has carried the
        // value past its target.
        let was = running.from.delta(running.to);
        let becomes = displayed.delta(target);
        let square: f32 = becomes.iter().map(|axis| axis * axis).sum();
        if square <= f32::EPSILON {
            return self;
        }
        let projected: f32 = was.iter().zip(becomes).map(|(old, new)| old * new).sum::<f32>() / square;
        // `rate` is negative while a spring closes, and `velocity` counts the same motion as
        // positive, hence the sign. Bounded so that a hand-over onto a displacement of almost
        // nothing cannot fling the next run across the screen.
        let carried = -prior.rate(running.progressed(now).as_secs_f32()) * projected;
        Self::new(self.stiffness, self.damping, carried.clamp(-Self::FASTEST, Self::FASTEST))
    }
}

/// One stop in a keyframe list: a value, and how the segment arriving at it is timed. The first
/// frame's own `duration` and `easing` are never read -- nothing eases into a beginning.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyframe {
    pub value: Animatable,
    pub duration: Duration,
    pub easing: Easing,
}

/// A property walking a list of values, some number of times (ADR-0152), which is QML's
/// `SequentialAnimation on <property>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sequence {
    /// At least two: the value it starts on, then one per segment. Shared rather than owned
    /// because the retained tree is cloned whole every frame and this list never changes after it
    /// is parsed -- a reference count moves instead of every keyframe in every running sequence.
    pub frames: Rc<[Keyframe]>,
    /// `None` is `Animation.Infinite`: it repeats for as long as the entry is there.
    pub loops: Option<u32>,
    /// One time through, which is every segment but the first frame's. Summed once here because
    /// both `at` and `done` want it on every tick of every running sequence, and the frames it
    /// sums are fixed when the entry is parsed.
    cycle: Duration,
}

impl Sequence {
    /// `None` when one time through would take no time, which is a list that is all jumps. Endless
    /// it would ask the compositor for a frame forever while showing one still value; counted it
    /// is a snap, which a config writes by leaving `animate` off the property (ADR-0152).
    fn new(frames: Vec<Keyframe>, loops: Option<u32>) -> Option<Self> {
        let cycle: Duration = frames.iter().skip(1).map(|frame| frame.duration).sum();
        (!cycle.is_zero()).then_some(Self { frames: frames.into(), loops, cycle })
    }

    /// The value `elapsed` into the run: the segment holding that instant, eased. A segment of no
    /// duration is a jump rather than a stop, so it is stepped over and its value shows only as
    /// the start of whatever follows -- which is what QML's `PropertyAction` does between two
    /// `PauseAnimation`s.
    fn at(&self, elapsed: Duration, property: &str) -> Animatable {
        let last = self.frames.last().expect("a parsed sequence has frames").value;
        if let Some(loops) = self.loops
            && elapsed >= self.cycle * loops
        {
            return last;
        }
        // The phase and the walk across segments stay in whole nanoseconds, and only the chosen
        // segment's fraction of itself becomes a float. An endless sequence runs for the life of
        // the process, and an `f32`'s 24-bit mantissa loses a millisecond of resolution after
        // about two hours of elapsed time and a whole 100 ms cycle after a fortnight, at which
        // point the phase stops advancing and the animation freezes and jumps. Subtracting the
        // segments in floats has the smaller version of the same fault: `0.4 - 0.3 < 0.1` holds in
        // `f32`, so an instant landing exactly on a boundary reads as just short of it and a jump
        // scheduled there waits for the next frame.
        let mut at = Duration::from_nanos((elapsed.as_nanos() % self.cycle.as_nanos()) as u64);
        for pair in self.frames.windows(2) {
            let (start, end) = (&pair[0], &pair[1]);
            if at < end.duration {
                let progress = end.easing.apply(at.as_secs_f32() / end.duration.as_secs_f32());
                return start.value.lerp(end.value, progress, property);
            }
            at -= end.duration;
        }
        last
    }

    /// Whether a run that started `elapsed` ago has played out. An infinite one never has.
    fn done(&self, elapsed: Duration) -> bool {
        self.loops.is_some_and(|loops| elapsed >= self.cycle * loops)
    }
}

/// `animate`'s table, resolved: which properties ease and how. Absent means none. The table
/// itself may be a signal, resolved like any other property; entries inside it are plain values.
/// A name `kind` does not accept is refused, so a misspelling fails the pass instead of silently
/// snapping; what the value is decides whether it can tween ([`Animatable::from_value`]), the way
/// Qt registers interpolators by type rather than by property.
pub fn parse_animate(
    kind: &str,
    properties: &HashMap<String, Value>,
) -> Result<HashMap<String, AnimationSpec>, LayoutError> {
    let Some(value) = properties.get("animate") else {
        return Ok(HashMap::new());
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            "animate",
            format!("expected a table of property names to durations, got {}", preview_for_error(value)),
        ));
    };
    let mut out = HashMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, entry) = pair.map_err(|e| invalid("animate", e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid("animate", format!("keys are property names, got {}", preview_for_error(&key))));
        };
        let property = key.to_str().map_err(|e| invalid("animate", e.to_string()))?.to_string();
        // The one key that is not a property name (ADR-0150). Checked here rather than only when
        // the node departs, so a typo in the block is refused while the node is still in the tree.
        if property == "exit" {
            parse_exit(kind, &entry)?;
            continue;
        }
        if property == "animate" || !crate::lua::nodes::accepts(kind, &property) {
            return Err(invalid("animate", format!("`{property}` is not a property of a `{kind}` node")));
        }
        out.insert(property.clone(), parse_spec(&property, &entry)?);
    }
    Ok(out)
}

/// One entry's spec: a bare duration, or `{ duration, easing, from }`, or those beside a
/// `keyframes` list and a `loops` count (ADR-0152), or a `spring` instead of any timing at all
/// (ADR-0154). `from` is read as a value of `property`.
fn parse_spec(property: &str, entry: &Value) -> Result<AnimationSpec, LayoutError> {
    let field = format!("animate.{property}");
    // The bare form says the duration and nothing else: `animate = { width = 200 }`.
    let Value::Table(spec) = entry else {
        let duration = parse_millis(&field, "duration", entry, 1)?
            .ok_or_else(|| invalid(&field, format!("expected a duration in ms, got {}", preview_for_error(entry))))?;
        let motion = Motion::Eased { duration, easing: Easing::default() };
        return Ok(AnimationSpec { motion, delay: Duration::ZERO, from: None });
    };
    let get = |key: &str| -> Result<Value, LayoutError> { spec.get(key).map_err(|e| invalid(&field, e.to_string())) };

    let from = match get("from")? {
        Value::Nil => None,
        from => Some(Animatable::from_value(property, Some(&from))?.ok_or_else(|| {
            invalid(&field, format!("`from` must be a value a tween can carry, got {}", preview_for_error(&from)))
        })?),
    };
    let delay = parse_millis(&field, "delay", &get("delay")?, 0)?.unwrap_or(Duration::ZERO);

    // Which motion this is decides which of the timing fields are read at all, so the ones
    // belonging to another are refused rather than parsed and dropped. That is the rule ADR-0152
    // already applied to `from` beside `keyframes`: an `easing` silently ignored beside a spring
    // is a config that believes it tuned something.
    let spring = parse_spring(&field, spec)?;
    let keyframes = get("keyframes")?;
    if spring.is_some() && !keyframes.is_nil() {
        return Err(invalid(
            &field,
            "`spring` and `keyframes` are two different motions: a spring settles on one target, a sequence walks a list"
                .to_string(),
        ));
    }
    if spring.is_some() {
        for name in ["duration", "easing", "loops"] {
            if !get(name)?.is_nil() {
                return Err(invalid(
                    &field,
                    format!("a `spring` has no `{name}`: what it does is decided by its stiffness and damping"),
                ));
            }
        }
    } else if keyframes.is_nil() && !get("loops")?.is_nil() {
        return Err(invalid(
            &field,
            "`loops` counts the walks of a `keyframes` list, and this entry has none".to_string(),
        ));
    }

    let motion = match spring {
        Some(spring) => Motion::Spring(spring),
        None => {
            let duration = get("duration")?;
            let duration = parse_millis(&field, "duration", &duration, 1)?.ok_or_else(|| {
                invalid(&field, format!("expected a duration in ms, got {}", preview_for_error(&duration)))
            })?;
            let easing = parse_easing(&field, &get("easing")?)?;
            match parse_sequence(property, &field, spec, &keyframes, duration, easing)? {
                Some(sequence) => {
                    if from.is_some() {
                        return Err(invalid(
                            &field,
                            "`from` and `keyframes` say the same thing twice: a sequence starts on its own first frame"
                                .to_string(),
                        ));
                    }
                    Motion::Sequence(sequence)
                }
                None => Motion::Eased { duration, easing },
            }
        }
    };
    Ok(AnimationSpec { motion, delay, from })
}

/// `image.transition` (ADR-0181): how a `retain`ing image crosses from the picture it is holding to
/// the one that has just landed. Duration and easing, and nothing else yet -- a cross-dissolve is
/// the whole of it until the masks arrive with a shader stage, and `effect` is the key that will
/// name them.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpec {
    pub duration: Duration,
    pub easing: Easing,
    /// A fragment shader to cross with, instead of the built-in dissolve (ADR-0184). The config's
    /// own file: `layout::image_shader` compiles it and owns nothing about what it draws.
    pub shader: Option<PathBuf>,
    /// `params` as `(uniform name, value)`, sorted, so two runs of one shader compare equal when
    /// they are the same. A name the compiled shader has no uniform for is ignored, because a
    /// shader may declare one and never use it.
    pub params: Vec<(String, f32)>,
}

/// `transition = { duration = 700, easing = "InOutCubic" }` on an `image`. The `duration` is
/// required: a dissolve with no length is a snap, and `retain` on its own is already that.
///
/// Unknown keys are refused rather than ignored, so a typo in a field name is an error the config
/// sees rather than a setting that silently does nothing.
pub fn parse_transition(properties: &HashMap<String, Value>) -> Result<Option<TransitionSpec>, LayoutError> {
    let Some(value) = properties.get("transition") else { return Ok(None) };
    let Value::Table(table) = value else {
        return Err(invalid(
            "transition",
            format!("expected a table of transition fields, got {}", preview_for_error(value)),
        ));
    };
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| invalid("transition", e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid("transition", format!("keys are field names, got {}", preview_for_error(&key))));
        };
        let key = key.to_str().map_err(|e| invalid("transition", e.to_string()))?;
        if !matches!(&*key, "duration" | "easing" | "shader" | "params") {
            return Err(invalid(
                "transition",
                format!("`{key}` is not a field of a transition; it takes `duration`, `easing`, `shader` and `params`"),
            ));
        }
    }
    let duration: Value = table.get("duration").map_err(|e| invalid("transition.duration", e.to_string()))?;
    let duration = parse_millis("transition.duration", "duration", &duration, 1)?
        .ok_or_else(|| invalid("transition", "a transition needs a `duration` in ms"))?;
    let easing: Value = table.get("easing").map_err(|e| invalid("transition.easing", e.to_string()))?;
    let easing = parse_easing("transition.easing", &easing)?;
    let shader: Value = table.get("shader").map_err(|e| invalid("transition.shader", e.to_string()))?;
    let shader = match shader {
        Value::Nil => None,
        Value::String(path) => {
            let path = path.to_str().map_err(|e| invalid("transition.shader", e.to_string()))?;
            // Absolute, the way `image.source` is: a config names its own files through
            // `obelisk.config_dir`, and a relative path would resolve against whatever directory
            // the Renderer happens to have been started in.
            if !path.starts_with('/') {
                return Err(invalid("transition.shader", format!("expected an absolute path, got `{path}`")));
            }
            Some(PathBuf::from(&*path))
        }
        other => {
            return Err(invalid(
                "transition.shader",
                format!("expected a path to a fragment shader, got {}", preview_for_error(&other)),
            ));
        }
    };
    let params: Value = table.get("params").map_err(|e| invalid("transition.params", e.to_string()))?;
    let params = parse_shader_params(&params)?;
    if shader.is_none() && !params.is_empty() {
        return Err(invalid("transition.params", "there is no `shader` for these to reach"));
    }
    Ok(Some(TransitionSpec { duration, easing, shader, params }))
}

/// `params = { softness = 0.1 }`: uniform names to numbers, which is every type a config can hand a
/// shader (ADR-0184). Sorted, so the list is a value two runs can compare.
fn parse_shader_params(value: &Value) -> Result<Vec<(String, f32)>, LayoutError> {
    let table = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(table) => table,
        other => {
            return Err(invalid(
                "transition.params",
                format!("expected a table of uniform names to numbers, got {}", preview_for_error(other)),
            ));
        }
    };
    let mut out = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| invalid("transition.params", e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid(
                "transition.params",
                format!("keys are uniform names, got {}", preview_for_error(&key)),
            ));
        };
        let name = key.to_str().map_err(|e| invalid("transition.params", e.to_string()))?.to_string();
        let field = format!("transition.params.{name}");
        let number = value_as_f32(&field, &value)?
            .filter(|number| number.is_finite())
            .ok_or_else(|| invalid(&field, format!("expected a finite number, got {}", preview_for_error(&value))))?;
        out.push((name, number));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// One cross-dissolve in flight on an `image` (ADR-0181), started by the frame the incoming texture
/// landed on and dropped the moment its duration is up.
#[derive(Debug, Clone, PartialEq)]
pub struct Dissolve {
    /// The source being crossed away from: what the node displayed when the incoming was first
    /// drawn. `ResolvedNode::displayed_source` has already moved on to the incoming by then, so
    /// the outgoing has nowhere else to live.
    pub from: String,
    /// The source being crossed to. Held rather than read off the node, because a pass may resolve
    /// a third source while this run is still going and a run whose destination moved under it
    /// drops the picture it was halfway to (ADR-0183). The successor waits for this run to end.
    pub to: String,
    pub started: Instant,
    pub spec: TransitionSpec,
    /// Eased 0..1 as of the last advance, and what `layout::paint` draws the incoming at. Held
    /// rather than read off the clock at paint time for the reason a tween writes its value into
    /// `properties`: the display list is built once and compared for equality, so the number in it
    /// has to be a number a pass decided, not one that moves under the comparison.
    pub progress: f32,
}

impl Dissolve {
    pub fn start(from: String, to: String, spec: TransitionSpec, now: Instant) -> Self {
        Self { from, to, started: now, spec, progress: 0.0 }
    }

    /// Advances to `now`. `false` once the dissolve is over, which is the caller's cue to drop it
    /// and leave the node drawing the source it named.
    pub fn advance(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.spec.duration {
            return false;
        }
        // Clamped, unlike a tween's value: `Easing::apply` clamps its input and not its output, so
        // Back, Elastic and Bounce all leave [0, 1], and this number is drawn as an alpha rather
        // than handed to a property parser that would refuse it (ADR-0183).
        let eased = self.spec.easing.apply(elapsed.as_secs_f32() / self.spec.duration.as_secs_f32());
        self.progress = eased.clamp(0.0, 1.0);
        true
    }
}

/// One `duration` or `delay`, in whole milliseconds: `None` when the field is absent, an error
/// when it is there and is not a number.
///
/// The two are told apart here rather than by `value_as_f32`, which answers `None` for a string as
/// readily as for `nil` and would let a typo read as an omission and take the default. `least` is
/// the smallest the field may round to, which is `1` wherever zero means no motion at all: a
/// `duration` of `0.1` clears any bound written in floats and then rounds to nothing, leaving a
/// tween that reports itself finished the instant it starts. Whole milliseconds because
/// `from_secs_f32` would carry `200` as `200.000003ms`.
fn parse_millis(field: &str, what: &str, value: &Value, least: u64) -> Result<Option<Duration>, LayoutError> {
    if value.is_nil() {
        return Ok(None);
    }
    let millis = value_as_f32(field, value)?
        .ok_or_else(|| invalid(field, format!("expected a {what} in ms, got {}", preview_for_error(value))))?;
    let rounded = millis.round() as u64;
    if !(0.0..=60_000.0).contains(&millis) || rounded < least {
        return Err(invalid(field, format!("{what} must be within [{least}, 60000] ms, got {millis}")));
    }
    Ok(Some(Duration::from_millis(rounded)))
}

/// An entry's `spring`, if it has one: `{ stiffness, damping }`, both required and both positive.
/// No defaults, because a spring whose constants are implicit is a mystery to read and to tune,
/// and no `mass`: it divides out of both constants, so naming it would be a third number that
/// only rescales the two above.
fn parse_spring(field: &str, spec: &mlua::Table) -> Result<Option<Spring>, LayoutError> {
    let spring: Value = spec.get("spring").map_err(|e| invalid(field, e.to_string()))?;
    let Value::Table(spring) = spring else {
        return match spring {
            Value::Nil => Ok(None),
            other => Err(invalid(
                field,
                format!("`spring` is a table of `stiffness` and `damping`, got {}", preview_for_error(&other)),
            )),
        };
    };
    let read = |name: &str, highest: f32| -> Result<f32, LayoutError> {
        let at = format!("{field}.spring.{name}");
        let value: Value = spring.get(name).map_err(|e| invalid(&at, e.to_string()))?;
        match value_as_f32(&at, &value)? {
            Some(number) if number > 0.0 && number <= highest => Ok(number),
            _ => Err(invalid(
                &at,
                format!("`{name}` is a number within (0, {highest}], got {}", preview_for_error(&value)),
            )),
        }
    };
    let stiffness = read("stiffness", 100_000.0)?;
    let damping = read("damping", 10_000.0)?;
    // A run that a pass starts fresh is at rest; `retarget` is the only thing that begins one
    // already moving, and it rebuilds the spring to say so.
    Ok(Some(Spring::new(stiffness, damping, 0.0)))
}

/// An entry's `keyframes` and `loops`, if it has them. A frame is a bare value, or a table naming
/// its own `duration` and `easing` in place of the entry's; the first frame is where the property
/// starts and the timing on it is never read. `loops` is a count or `"Infinite"`, one by default.
fn parse_sequence(
    property: &str,
    field: &str,
    spec: &mlua::Table,
    keyframes: &Value,
    duration: Duration,
    easing: Easing,
) -> Result<Option<Sequence>, LayoutError> {
    let Value::Table(keyframes) = keyframes else {
        return match keyframes {
            Value::Nil => Ok(None),
            other => Err(invalid(field, format!("`keyframes` is a list of values, got {}", preview_for_error(other)))),
        };
    };
    let mut frames = Vec::new();
    for (index, frame) in keyframes.sequence_values::<Value>().enumerate() {
        let frame = frame.map_err(|e| invalid(field, e.to_string()))?;
        let at = format!("{field}.keyframes[{}]", index + 1);
        let (value, duration, easing) = match frame {
            // A frame that names nothing of its own is still a table when the value is one, so an
            // explicit `value` key is what tells the two apart.
            Value::Table(table) if table.contains_key("value").unwrap_or(false) => {
                let value: Value = table.get("value").map_err(|e| invalid(&at, e.to_string()))?;
                let own: Value = table.get("duration").map_err(|e| invalid(&at, e.to_string()))?;
                // Absent takes the entry's. Zero is allowed where the entry's own is not: a
                // segment that takes no time is the jump QML writes as `PropertyAction`.
                let own = parse_millis(&at, "duration", &own, 0)?.unwrap_or(duration);
                let named: Value = table.get("easing").map_err(|e| invalid(&at, e.to_string()))?;
                let named = if named.is_nil() { easing } else { parse_easing(&at, &named)? };
                (value, own, named)
            }
            plain => (plain, duration, easing),
        };
        let value = Animatable::from_value(property, Some(&value))?.ok_or_else(|| {
            invalid(&at, format!("must be a value a tween can carry, got {}", preview_for_error(&value)))
        })?;
        frames.push(Keyframe { value, duration, easing });
    }
    if frames.len() < 2 {
        return Err(invalid(field, format!("`keyframes` needs at least two values, got {}", frames.len())));
    }
    // A list is read from index 1 until the first hole, so `{ [1] = 0, [2] = 1, [4] = 0 }` would
    // quietly become two frames. Count what the table actually holds and refuse the mismatch: a
    // config that miscounted its own loop should fail the pass, like every other typo here.
    let mut entries = 0usize;
    for pair in keyframes.pairs::<Value, Value>() {
        pair.map_err(|e| invalid(field, e.to_string()))?;
        entries += 1;
    }
    if entries != frames.len() {
        return Err(invalid(
            field,
            format!("`keyframes` is a list; it holds {entries} entries but only {} run from index 1", frames.len()),
        ));
    }
    let loops: Value = spec.get("loops").map_err(|e| invalid(field, e.to_string()))?;
    let loops = match &loops {
        Value::Nil => Some(1),
        Value::String(name) if name.to_str().is_ok_and(|name| name == "Infinite") => None,
        counted => match value_as_f32(field, counted)? {
            Some(count) if (1.0..=10_000.0).contains(&count) && count.fract() == 0.0 => Some(count as u32),
            _ => {
                return Err(invalid(
                    field,
                    format!(
                        "`loops` is a whole count in [1, 10000] or \"Infinite\", got {}",
                        preview_for_error(counted)
                    ),
                ));
            }
        },
    };
    Sequence::new(frames, loops).map(Some).ok_or_else(|| {
        invalid(field, "every `keyframes` segment lasts no time: a sequence that takes none is a jump".to_string())
    })
}

/// The shared spec and every `(property, target)` pair of one `animate.exit` block.
type ExitBlock = (AnimationSpec, Vec<(String, Animatable)>);

/// A spec's `easing`: a name, a four-number table read as CSS `cubic-bezier(x1, y1, x2, y2)`, or
/// `{ steps = n }` (ADR-0151). Absent is `InOutQuad`.
fn parse_easing(field: &str, value: &Value) -> Result<Easing, LayoutError> {
    match value {
        Value::Nil => Ok(Easing::default()),
        Value::String(name) => {
            let name = name.to_str().map_err(|e| invalid(field, e.to_string()))?;
            Easing::parse(&name).ok_or_else(|| {
                let known: Vec<&str> = Easing::NAMES.iter().map(|(n, _)| *n).collect();
                invalid(field, format!("unknown easing `{name}`; one of {}", known.join(", ")))
            })
        }
        Value::Table(table) => {
            let steps: Value = table.get("steps").map_err(|e| invalid(field, e.to_string()))?;
            if !steps.is_nil() {
                let steps = value_as_f32(field, &steps)?
                    .ok_or_else(|| invalid(field, format!("`steps` is a count, got {}", preview_for_error(&steps))))?;
                if steps < 1.0 || steps > 1000.0 || steps.fract() != 0.0 {
                    return Err(invalid(field, format!("`steps` must be a whole count in [1, 1000], got {steps}")));
                }
                return Ok(Easing::Steps(steps as u32));
            }
            let mut points = [0.0f32; 4];
            for (index, slot) in points.iter_mut().enumerate() {
                let point: Value = table.get(index + 1).map_err(|e| invalid(field, e.to_string()))?;
                *slot = value_as_f32(field, &point)?.ok_or_else(|| {
                    invalid(field, "a table easing is `{ x1, y1, x2, y2 }` or `{ steps = n }`".to_string())
                })?;
            }
            // Only the control `x` are bounded, and CSS bounds them for the same reason: outside
            // `[0, 1]` the curve doubles back and one progress has several answers. The `y` are
            // free, which is what lets a Bezier overshoot the way `OutBack` does.
            if !(0.0..=1.0).contains(&points[0]) || !(0.0..=1.0).contains(&points[2]) {
                return Err(invalid(
                    field,
                    format!("a Bezier's `x1` and `x2` must be within [0, 1], got {} and {}", points[0], points[2]),
                ));
            }
            Ok(Easing::Bezier { x1: points[0], y1: points[1], x2: points[2], y2: points[3] })
        }
        other => Err(invalid(
            field,
            format!("easing is a name, `{{ x1, y1, x2, y2 }}` or `{{ steps = n }}`, got {}", preview_for_error(other)),
        )),
    }
}

/// `animate.exit`'s block, resolved: `{ duration, easing, <property> = <target>, ... }`, one spec
/// for every named target. The targets are what the node eases to once the tree no longer holds
/// it (ADR-0150), the way QML's `ViewTransition` on `remove` runs after the model row is gone.
/// A block naming no target is a no-op and needs no duration, so it resolves to `None`.
fn parse_exit(kind: &str, block: &Value) -> Result<Option<ExitBlock>, LayoutError> {
    let Value::Table(exit) = block else {
        return match block {
            Value::Nil => Ok(None),
            other => Err(invalid("animate.exit", format!("expected a table, got {}", preview_for_error(other)))),
        };
    };
    let mut out = Vec::new();
    for pair in exit.pairs::<Value, Value>() {
        let (key, target) = pair.map_err(|e| invalid("animate.exit", e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid("animate.exit", format!("keys are property names, got {}", preview_for_error(&key))));
        };
        let property = key.to_str().map_err(|e| invalid("animate.exit", e.to_string()))?.to_string();
        if matches!(property.as_str(), "duration" | "delay" | "easing" | "spring") {
            continue;
        }
        if property == "animate" || !crate::lua::nodes::accepts(kind, &property) {
            return Err(invalid("animate.exit", format!("`{property}` is not a property of a `{kind}` node")));
        }
        let field = format!("animate.exit.{property}");
        let target = Animatable::from_value(&property, Some(&target))?.ok_or_else(|| {
            invalid(&field, format!("must be a value a tween can carry, got {}", preview_for_error(&target)))
        })?;
        out.push((property, target));
    }
    // Last, so an empty block stays legal while one with targets must say how long they take.
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some((parse_spec("exit", block)?, out)))
}

/// Starts a removed node's exit tweens (ADR-0150): each `animate.exit` target from the value the
/// node displays, or from the property's identity when it never set one (an absent `opacity` is
/// `1`, an absent `scale` is `1`, anything else `0`), over the block's shared `duration` and
/// `easing`. Every tween the node was already running is dropped where it stood, so the exit
/// block alone decides how long the node lives. Returns whether anything is now in flight: a node
/// with nothing to ease is dropped at once.
pub fn depart(
    kind: &str,
    tweens: &mut Vec<Tween>,
    properties: &mut HashMap<String, Value>,
    now: Instant,
    lua: &Lua,
) -> Result<bool, LayoutError> {
    let Some(Value::Table(animate)) = properties.get("animate") else { return Ok(false) };
    let block: Value = animate.get("exit").map_err(|e| invalid("animate.exit", e.to_string()))?;
    let Some((spec, targets)) = parse_exit(kind, &block)? else { return Ok(false) };
    // Everything already in flight stops here, at the value it had reached. The exit owns the
    // node's motion from now on, so its lifetime is the block's duration and not that plus
    // whatever an interrupted entry animation had left to run.
    tweens.clear();
    for (property, target) in targets {
        let from =
            Animatable::from_value(&property, properties.get(&property))?.unwrap_or_else(|| target.identity(&property));
        let tween =
            Tween { property: property.clone(), from, to: target, started: now, spec: spec.clone(), resting: false };
        properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
        tweens.push(tween);
    }
    Ok(!tweens.is_empty())
}

/// A value a tween can sit between, told apart by shape rather than by which property holds it.
/// `Percent` is a `"NN%"` size held as a fraction; `Fields` is a table of numbers under one of
/// two key sets, the edges `{ top, right, bottom, left }` or the axes `{ x, y }`, an absent key
/// reading as the property's default (`0`, or `1` for a `scale`). Two different shapes snap, so
/// a fill that switches between `"45%"` and `"Fill"` or a margin that switches between a number
/// and a table takes the new value at once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Animatable {
    Number(f32),
    Percent(f32),
    Color(Rgba),
    Fields { keys: &'static [&'static str], values: [f32; 4] },
}

const EDGES: &[&str] = &["top", "right", "bottom", "left"];
const AXES: &[&str] = &["x", "y"];

impl Animatable {
    /// `property`'s value when it is not set, in this value's shape: `1` for `opacity` and
    /// `scale`, `0` otherwise, per axis or edge for a table. A percent or a colour has no identity
    /// to speak of and stays where it is.
    fn identity(self, property: &str) -> Self {
        match self {
            Self::Number(_) => Self::Number(if property == "opacity" { 1.0 } else { axis_default(property) }),
            Self::Fields { keys, .. } => Self::Fields { keys, values: [axis_default(property); 4] },
            // An unset size is nothing, and an unset colour paints nothing, which is that colour
            // at zero alpha rather than a second hue to cross on the way out.
            Self::Percent(_) => Self::Percent(0.0),
            Self::Color(colour) => Self::Color(Rgba { a: 0.0, ..colour }),
        }
    }

    /// The typed reading of `property`'s current value, or `None` when the value is a shape no
    /// tween carries (`"Fill"`, a boolean, a table of colours, absent): the caller snaps then. A
    /// `#` string that fails its colour parse is an error, the same one the property's own parser
    /// raises.
    pub fn from_value(property: &str, value: Option<&Value>) -> Result<Option<Self>, LayoutError> {
        let Some(value) = value else { return Ok(None) };
        match value {
            Value::String(s) => {
                let s = s.to_str().map_err(|e| invalid(property, e.to_string()))?;
                if s.starts_with('#') {
                    return Ok(Some(Self::Color(parse_hex_color(property, &s)?)));
                }
                Ok(parse_percent(&s).map(Self::Percent))
            }
            Value::Table(table) => {
                let has = |key: &str| table.contains_key(key).unwrap_or(false);
                let keys = if has("x") || has("y") { AXES } else { EDGES };
                let mut values = [axis_default(property); 4];
                for (slot, key) in values.iter_mut().zip(keys) {
                    let field: Value = table.get(*key).map_err(|e| invalid(property, e.to_string()))?;
                    if field.is_nil() {
                        continue;
                    }
                    let Some(n) = value_as_f32(property, &field)? else { return Ok(None) };
                    *slot = n;
                }
                Ok(Some(Self::Fields { keys, values }))
            }
            _ => Ok(value_as_f32(property, value)?.map(Self::Number)),
        }
    }

    /// This value minus `to`, component by component, zero-padded to a fixed width so the four
    /// shapes compare as one vector. Two shapes that cannot mix have no displacement between them
    /// and answer zero, which is the same thing [`Self::lerp`] does with such a pair: snap.
    fn delta(self, to: Self) -> [f32; 4] {
        let mut out = [0.0; 4];
        match (self, to) {
            (Self::Number(a), Self::Number(b)) | (Self::Percent(a), Self::Percent(b)) => out[0] = a - b,
            (Self::Fields { keys, values: a }, Self::Fields { keys: other, values: b }) if keys == other => {
                for ((slot, x), y) in out.iter_mut().zip(a).zip(b) {
                    *slot = x - y;
                }
            }
            (Self::Color(a), Self::Color(b)) => out = [a.r - b.r, a.g - b.g, a.b - b.b, a.a - b.a],
            _ => {}
        }
        out
    }

    fn lerp(self, to: Self, t: f32, property: &str) -> Self {
        match (self, to) {
            (Self::Number(a), Self::Number(b)) => {
                let (low, high) = range_of(property);
                Self::Number((a + (b - a) * t).clamp(low, high))
            }
            (Self::Percent(a), Self::Percent(b)) => Self::Percent((a + (b - a) * t).max(0.0)),
            (Self::Fields { keys, values: a }, Self::Fields { keys: other, values: b }) if keys == other => {
                let (low, high) = range_of(property);
                let mut values = [0.0; 4];
                for ((slot, x), y) in values.iter_mut().zip(a).zip(b) {
                    *slot = (x + (y - x) * t).clamp(low, high);
                }
                Self::Fields { keys, values }
            }
            (Self::Color(a), Self::Color(b)) => {
                let mix = |x: f32, y: f32| (x + (y - x) * t).clamp(0.0, 1.0);
                Self::Color(Rgba { r: mix(a.r, b.r), g: mix(a.g, b.g), b: mix(a.b, b.b), a: mix(a.a, b.a) })
            }
            // The two shapes come from the same property, so this pair cannot be mixed; snap to
            // the target rather than guess if it ever is.
            _ => to,
        }
    }

    /// The value written back into a resolved property map for the parsers to read.
    pub fn to_value(self, lua: &Lua) -> mlua::Result<Value> {
        Ok(match self {
            Self::Number(n) => Value::Number(f64::from(n)),
            // Three decimals: enough that a 147ms tween over a 6px meter never repeats a frame,
            // and the shape `parse_percent` reads (`^\d+(\.\d+)?%$`, no exponent, no sign).
            Self::Percent(p) => Value::String(lua.create_string(format!("{:.3}%", p * 100.0))?),
            Self::Color(color) => Value::String(lua.create_string(hex_of(color))?),
            Self::Fields { keys, values } => {
                let table = lua.create_table_with_capacity(0, keys.len())?;
                for (key, value) in keys.iter().zip(values) {
                    table.set(*key, value)?;
                }
                Value::Table(table)
            }
        })
    }
}

/// One property of one retained node, in flight from `from` to `to`. `started` is when the pass
/// that saw the target change ran; progress is elapsed time over the spec's duration (ADR-0130
/// decision 2), never accumulated frame deltas.
#[derive(Debug, Clone, PartialEq)]
pub struct Tween {
    pub property: String,
    /// Where the motion began, and where it is bound for. Both are ignored under
    /// [`Motion::Sequence`] -- a sequence reads its own frames -- and hold its first and last for
    /// a reader.
    pub from: Animatable,
    pub to: Animatable,
    pub started: Instant,
    pub spec: AnimationSpec,
    /// A finite sequence that has played out (ADR-0152). It stays in the list so a pass does not
    /// start it over, holding the property at its last frame, but it no longer asks for frames.
    /// Always false for a plain tween, which is dropped the moment it arrives.
    pub resting: bool,
}

impl Tween {
    /// How far into the motion itself `now` is: time since the tween started, less the spec's
    /// `delay`. Saturating, so the whole delay window reads as zero and both callers below hold
    /// at the beginning without a branch of their own -- every easing answers 0 at 0, and a
    /// sequence's first frame is where it starts.
    fn progressed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started).saturating_sub(self.spec.delay)
    }

    pub fn at(&self, now: Instant) -> Animatable {
        let elapsed = self.progressed(now);
        let progress = match &self.spec.motion {
            // The lead-in holds the value the run opens on. `progressed` saturates to zero through
            // it, which is where every easing and every spring starts anyway, but a sequence
            // opening on a jump plays that jump at zero, and would spend the whole delay showing
            // the value after it rather than the one before (ADR-0153).
            Motion::Sequence(sequence) if now.saturating_duration_since(self.started) < self.spec.delay => {
                return sequence.frames[0].value;
            }
            Motion::Sequence(sequence) => return sequence.at(elapsed, &self.property),
            Motion::Eased { duration, easing } => {
                easing.apply((elapsed.as_secs_f32() / duration.as_secs_f32()).min(1.0))
            }
            Motion::Spring(spring) => spring.at(elapsed),
        };
        self.from.lerp(self.to, progress, &self.property)
    }

    pub fn done(&self, now: Instant) -> bool {
        let elapsed = self.progressed(now);
        match &self.spec.motion {
            Motion::Sequence(sequence) => sequence.done(elapsed),
            Motion::Eased { duration, .. } => elapsed >= *duration,
            Motion::Spring(spring) => spring.done(elapsed),
        }
    }
}

/// Reconciles a node's tweens against the targets a pass just resolved, and writes the displayed
/// value of each into `properties` for the parsers to read. `retained` is the node this one was
/// matched to, as the tweens it carried and the properties it last displayed; `None` is a new
/// node, which takes its targets as they are (QML's `Behavior` does not animate a first value
/// either).
///
/// A target that differs from the retained target starts a tween from the value on screen: the
/// one the retained map holds, which is what the last pass or tick painted, whether that was a
/// resting value or the middle of an earlier tween. A target that matches keeps the running
/// tween, so a pass that re-resolves for some unrelated signal does not restart motion. A property
/// `animate` stopped naming loses its tween and snaps. A property nothing displayed yet, on a new
/// node or one that lacked it, starts from the spec's `from` when there is one.
pub fn retarget(
    kind: &str,
    retained: Option<(&[Tween], &HashMap<String, Value>)>,
    properties: &mut HashMap<String, Value>,
    now: Instant,
    lua: &Lua,
) -> Result<Vec<Tween>, LayoutError> {
    let specs = parse_animate(kind, properties)?;
    let (running, shown) = retained.map_or((&[][..], None), |(running, shown)| (running, Some(shown)));
    let mut tweens = Vec::with_capacity(specs.len());
    for (property, spec) in specs {
        let running = running.iter().find(|t| t.property == property);
        // A sequence drives the property rather than easing to it (ADR-0152), so it needs no
        // target and reads nothing the pass resolved. The same list going round again is the same
        // run, played out or not; a different list is a new one, from its first frame.
        if let Motion::Sequence(sequence) = &spec.motion {
            let carried = running.filter(|prior| prior.spec.motion == spec.motion);
            let mut tween = match carried {
                Some(prior) => Tween { spec, ..prior.clone() },
                None => Tween {
                    property: property.clone(),
                    from: sequence.frames[0].value,
                    to: sequence.frames.last().expect("a parsed sequence has frames").value,
                    started: now,
                    spec,
                    resting: false,
                },
            };
            // Against `now`, not against what the carried run was resting on: `advance` is the only
            // other place this is decided, and a pass can both finish a run it never ticked and
            // hand a played-out one a fresh `delay`. Carrying the old flag through either of those
            // leaves the tree disagreeing with the clock -- a finished run still asking for frame
            // callbacks, or a re-delayed one resting so hard that `animating` never asks for the
            // first.
            tween.resting = tween.done(now);
            properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
            tweens.push(tween);
            continue;
        }
        let Some(target) = Animatable::from_value(&property, properties.get(&property))? else {
            continue;
        };
        let displayed = match shown {
            Some(shown) => Animatable::from_value(&property, shown.get(&property))?,
            None => None,
        };
        let Some(displayed) = displayed.or(spec.from) else { continue };
        let retained_target = running.map_or(displayed, |tween| tween.to);
        let tween = match running {
            _ if retained_target != target => {
                // A spring that is already moving hands its rate to the run replacing it, so a
                // target that changes mid-flight bends the motion instead of restarting it from
                // still (ADR-0154). Every other motion starts over, which is what a curve of
                // progress can do.
                let spec = match (spec.motion, running) {
                    (Motion::Spring(spring), Some(running)) => {
                        AnimationSpec { motion: Motion::Spring(spring.handed(running, displayed, target, now)), ..spec }
                    }
                    (motion, _) => AnimationSpec { motion, ..spec },
                };
                Tween { property: property.clone(), from: displayed, to: target, started: now, spec, resting: false }
            }
            Some(running) if !running.done(now) => {
                // A spring's `velocity` is the rate the last retarget handed it, not a number the
                // config wrote, and re-parsing the entry always yields one at rest. Taking the
                // fresh spec whole would stop a moving spring dead on the first pass any unrelated
                // signal caused, so one whose constants still match carries the spring across.
                // Only the spring: the rest of the entry is re-read, so an edited `delay` lands on
                // a spring already moving. Editing a constant takes the new spring at its parsed
                // rest -- either way the run continues, it is the motion under it that changed.
                let mut spec = spec;
                if let (Motion::Spring(fresh), Motion::Spring(prior)) = (&spec.motion, &running.spec.motion)
                    && fresh.constants() == prior.constants()
                {
                    spec.motion = Motion::Spring(*prior);
                }
                Tween { spec, ..running.clone() }
            }
            _ => continue,
        };
        properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
        tweens.push(tween);
    }
    Ok(tweens)
}

/// The properties a tween can move without asking the solver anything: what they change is what a
/// node paints, never the box it was given. `layout::scene::taffy_style` reads none of them, and
/// `layout::scene::measure_for` reads a text's content, size, family and wrapping but not its
/// colour, so a tick whose every running tween names one of these can re-derive the paint in place
/// and leave the taffy pass out entirely (`layout::scene::Scene::tick`).
///
/// `opacity` is in `LayoutStyle` and still belongs here: the solver never receives it, `finish`
/// only copies it onto the node, and `layout::paint` multiplies it down the subtree.
///
/// The transform properties are deliberately absent even though the solver ignores them too.
/// ADR-0149 maps the pointer back through a node's inverse transform, so moving one changes what
/// the pointer hits, and the input regions have to be rebuilt with it. They stay on the layout
/// path until something rebuilds those regions without a full pass.
const PAINT_ONLY: &[&str] = &["opacity", "background", "border_color", "foreground", "radius"];

/// Whether a tween on `property` can be advanced by a paint-only tick; see [`PAINT_ONLY`].
pub fn is_paint_only(property: &str) -> bool {
    PAINT_ONLY.contains(&property)
}

/// Advances every tween in `tweens` to `now`, writing the displayed values into `properties` and
/// dropping the ones that have arrived. A sequence that has played out is kept instead, resting on
/// its last frame, because the list alone is what a pass has to tell a finished run from one it
/// has never started (ADR-0152).
pub fn advance(
    tweens: &mut Vec<Tween>,
    properties: &mut HashMap<String, Value>,
    now: Instant,
    lua: &Lua,
) -> Result<(), LayoutError> {
    for tween in tweens.iter_mut() {
        if tween.resting {
            continue;
        }
        // `retarget` wrote the key when it started the tween, so no insert and no key clone.
        *properties.get_mut(&tween.property).expect("a tween's property is in the map it was started from") =
            tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?;
        tween.resting = matches!(tween.spec.motion, Motion::Sequence(_)) && tween.done(now);
    }
    tweens.retain(|tween| matches!(tween.spec.motion, Motion::Sequence(_)) || !tween.done(now));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    impl AnimationSpec {
        /// The eased pair, for the tests that only care about timing. Panics on the other two
        /// motions, which is what a test asserting a duration wants when it is handed a sequence.
        fn eased(&self) -> (Duration, Easing) {
            match &self.motion {
                Motion::Eased { duration, easing } => (*duration, *easing),
                other => panic!("expected an eased motion, got {other:?}"),
            }
        }

        fn sequence(&self) -> Option<Sequence> {
            match &self.motion {
                Motion::Sequence(sequence) => Some(sequence.clone()),
                _ => None,
            }
        }
    }

    fn props(lua: &Lua, src: &str) -> HashMap<String, Value> {
        let table: mlua::Table = lua.load(src).eval().unwrap();
        table.pairs::<String, Value>().map(|p| p.unwrap()).collect()
    }

    /// The spec `src` declares for `width`, and the message refusing `src`: between them, what
    /// every parser test below asks.
    fn spec(lua: &Lua, src: &str) -> AnimationSpec {
        parse_animate("rect", &props(lua, src)).unwrap().remove("width").unwrap()
    }

    fn refused(lua: &Lua, src: &str) -> String {
        parse_animate("rect", &props(lua, src)).unwrap_err().to_string()
    }

    #[test]
    fn every_easing_starts_at_zero_and_ends_at_one() {
        for (name, easing) in Easing::NAMES {
            assert!((easing.apply(0.0)).abs() < 1e-6, "{name} at 0");
            assert!((easing.apply(1.0) - 1.0).abs() < 1e-5, "{name} at 1");
        }
    }

    #[test]
    fn in_out_quad_is_symmetric_about_the_midpoint() {
        assert!((Easing::InOutQuad.apply(0.5) - 0.5).abs() < 1e-6);
        assert!((Easing::InOutQuad.apply(0.25) + Easing::InOutQuad.apply(0.75) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn out_back_overshoots_and_the_number_clamp_catches_it() {
        assert!(Easing::OutBack.apply(0.7) > 1.0);
        let from = Animatable::Number(40.0);
        let to = Animatable::Number(0.0);
        assert_eq!(from.lerp(to, Easing::OutBack.apply(0.7), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(to, Easing::OutBack.apply(0.7), "margin"), Animatable::Number(n) if n < 0.0));
    }

    /// Every named curve at a quarter, a midpoint and three quarters, against values computed from
    /// Qt's `QEasingCurve` -- which is what QML's `Easing.Type` actually runs -- and not against
    /// this module's own other arms. Checking a family's `Out` against a reflection of its own
    /// `In` is a tautology: it passed while `InOutBack` and `InOutElastic` were both wrong, because
    /// those two widen their constant for the `InOut` arm (`s * 1.525`, a period of `0.3 * 1.5`)
    /// and a plain reflection does not.
    #[test]
    fn every_named_curve_matches_qts_own_at_a_quarter_a_half_and_three_quarters() {
        #[rustfmt::skip]
        let reference: &[(Easing, [f32; 3])] = &[
            (Easing::Linear,       [0.250000, 0.500000, 0.750000]),
            (Easing::InQuad,       [0.062500, 0.250000, 0.562500]),
            (Easing::OutQuad,      [0.437500, 0.750000, 0.937500]),
            (Easing::InOutQuad,    [0.125000, 0.500000, 0.875000]),
            (Easing::InCubic,      [0.015625, 0.125000, 0.421875]),
            (Easing::OutCubic,     [0.578125, 0.875000, 0.984375]),
            (Easing::InOutCubic,   [0.062500, 0.500000, 0.937500]),
            (Easing::InQuart,      [0.003906, 0.062500, 0.316406]),
            (Easing::OutQuart,     [0.683594, 0.937500, 0.996094]),
            (Easing::InOutQuart,   [0.031250, 0.500000, 0.968750]),
            (Easing::InQuint,      [0.000977, 0.031250, 0.237305]),
            (Easing::OutQuint,     [0.762695, 0.968750, 0.999023]),
            (Easing::InOutQuint,   [0.015625, 0.500000, 0.984375]),
            (Easing::InSine,       [0.076120, 0.292893, 0.617317]),
            (Easing::OutSine,      [0.382683, std::f32::consts::FRAC_1_SQRT_2, 0.923880]),
            (Easing::InOutSine,    [0.146447, 0.500000, 0.853553]),
            (Easing::InExpo,       [0.005524, 0.031250, 0.176777]),
            (Easing::OutExpo,      [0.823223, 0.968750, 0.994476]),
            (Easing::InOutExpo,    [0.015625, 0.500000, 0.984375]),
            (Easing::InCirc,       [0.031754, 0.133975, 0.338562]),
            (Easing::OutCirc,      [0.661438, 0.866025, 0.968246]),
            (Easing::InOutCirc,    [0.066987, 0.500000, 0.933013]),
            (Easing::InBack,       [-0.064137, -0.087698, 0.182590]),
            (Easing::OutBack,      [0.817410, 1.087697, 1.064137]),
            (Easing::InOutBack,    [-0.099682, 0.500000, 1.099682]),
            (Easing::InElastic,    [-0.005524, -0.015625, 0.088388]),
            (Easing::OutElastic,   [0.911612, 1.015625, 1.005524]),
            (Easing::InOutElastic, [0.011969, 0.500000, 0.988031]),
            (Easing::InBounce,     [0.027344, 0.234375, 0.527344]),
            (Easing::OutBounce,    [0.472656, 0.765625, 0.972656]),
            (Easing::InOutBounce,  [0.117188, 0.500000, 0.882812]),
        ];
        assert_eq!(reference.len(), Easing::NAMES.len(), "every name has a row");
        for (easing, expected) in reference {
            for (t, want) in [0.25f32, 0.5, 0.75].into_iter().zip(expected) {
                let got = easing.apply(t);
                assert!((got - want).abs() < 1e-5, "{easing:?} at {t}: got {got}, Qt says {want}");
            }
        }
    }

    /// The overshooting families leave `[0, 1]` on purpose; the property's own range is what pulls
    /// them back, so a `width` easing to `0` never hands the parser a negative.
    #[test]
    fn back_elastic_and_bounce_overshoot_and_the_range_clamp_catches_them() {
        assert!(Easing::InBack.apply(0.3) < 0.0, "Back winds up before it moves");
        assert!(Easing::OutElastic.apply(0.4) > 1.0, "Elastic rings past the target");
        // `InBack` winds backwards, so the clamp bites at the start of a growing width rather than
        // at the end of a shrinking one the way `OutBack`'s does above.
        let (from, to) = (Animatable::Number(0.0), Animatable::Number(40.0));
        assert_eq!(from.lerp(to, Easing::InBack.apply(0.3), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(to, Easing::InBack.apply(0.3), "margin"), Animatable::Number(n) if n < 0.0));
        assert!(Easing::InBounce.apply(0.5) >= 0.0 && Easing::OutBounce.apply(0.5) <= 1.0, "Bounce stays inside");
    }

    /// A four-number table is CSS `cubic-bezier`, solved for `y` at the parameter whose `x` is the
    /// progress. The identity control points are exactly `Linear`, which is the cheapest proof the
    /// solve is not off by a parameter.
    #[test]
    fn a_four_number_easing_is_a_cubic_bezier() {
        let lua = Lua::new();
        let parsed = |src: &str| {
            parse_animate("rect", &props(&lua, src)).unwrap().remove("width").expect("width has a spec").eased().1
        };
        let linear = parsed("return { animate = { width = { duration = 1, easing = { 0, 0, 1, 1 } } } }");
        assert_eq!(linear, Easing::Bezier { x1: 0.0, y1: 0.0, x2: 1.0, y2: 1.0 });
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            assert!((linear.apply(t) - t).abs() < 1e-4, "the identity curve is Linear, off at {t}");
        }
        // CSS `ease-in-out`, whose control points are symmetric, so the curve is too.
        let ease = parsed("return { animate = { width = { duration = 1, easing = { 0.42, 0, 0.58, 1 } } } }");
        assert!((ease.apply(0.5) - 0.5).abs() < 1e-4);
        assert!((ease.apply(0.25) + ease.apply(0.75) - 1.0).abs() < 1e-4);
        assert!(ease.apply(0.25) < 0.25, "it starts slower than linear");
    }

    /// `{ steps = n }` holds each value and lands on the target only at the end, which is what a
    /// blinking or ticking indicator wants instead of a smooth ramp.
    #[test]
    fn a_steps_easing_jumps_and_only_the_last_step_reaches_the_target() {
        let lua = Lua::new();
        let steps = parse_animate(
            "rect",
            &props(&lua, "return { animate = { width = { duration = 1, easing = { steps = 4 } } } }"),
        )
        .unwrap()
        .remove("width")
        .expect("width has a spec")
        .eased()
        .1;
        assert_eq!(steps, Easing::Steps(4));
        assert_eq!(steps.apply(0.0), 0.0);
        assert_eq!(steps.apply(0.1), 0.0);
        assert_eq!(steps.apply(0.3), 0.25);
        assert_eq!(steps.apply(0.99), 0.75);
        assert_eq!(steps.apply(1.0), 1.0);
    }

    #[test]
    fn a_table_easing_that_is_neither_shape_is_refused() {
        let lua = Lua::new();

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = { 2, 0, 0.5, 1 } } } }");
        assert!(text.contains("`x1` and `x2`") && text.contains("2"), "{text}");

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = { steps = 0 } } } }");
        assert!(text.contains("steps") && text.contains("[1, 1000]"), "{text}");

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = { 0.5, 0.5 } } } }");
        assert!(text.contains("x1, y1, x2, y2") && text.contains("steps"), "{text}");

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = 4 } } }");
        assert!(text.contains("easing is a name"), "{text}");
    }

    #[test]
    fn a_bare_number_is_a_duration_with_the_default_easing() {
        let lua = Lua::new();
        let specs = parse_animate("rect", &props(&lua, "return { animate = { width = 200 } }")).unwrap();
        assert_eq!(
            specs["width"],
            AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_millis(200), easing: Easing::InOutQuad },
                delay: Duration::ZERO,
                from: None,
            }
        );
    }

    #[test]
    fn a_table_names_its_easing() {
        let lua = Lua::new();
        let specs = parse_animate(
            "rect",
            &props(
                &lua,
                r##"return { animate = { background = { duration = 150, easing = "OutCubic", from = "#000000" } } }"##,
            ),
        )
        .unwrap();
        assert_eq!(specs["background"].eased().1, Easing::OutCubic);
        assert_eq!(specs["background"].from, Some(Animatable::Color(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 })));
    }

    #[test]
    fn an_unknown_easing_is_refused_naming_the_known_ones() {
        let lua = Lua::new();
        let err = parse_animate(
            "rect",
            &props(&lua, r#"return { animate = { width = { duration = 1, easing = "Bouncy" } } }"#),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("animate.width") && text.contains("Bouncy") && text.contains("OutBack"), "{text}");
    }

    #[test]
    fn a_property_the_kind_does_not_have_is_refused_by_name() {
        let lua = Lua::new();
        let err = parse_animate("rect", &props(&lua, "return { animate = { widht = 200 } }")).unwrap_err();
        assert!(err.to_string().contains("`widht`") && err.to_string().contains("`rect`"), "{err}");
        // A real property whose value is not a tween shape is fine to name; it snaps.
        assert!(parse_animate("rect", &props(&lua, "return { animate = { visible = 200 } }")).is_ok());
    }

    /// ADR-0150: the exit block is checked by the same live pass that checks the rest of
    /// `animate`, so a typo is refused while the node is still in the tree to be named, not on the
    /// pass that removed it.
    /// ADR-0152: a bare value takes the entry's timing, a table names its own, and the first
    /// frame is only a starting point -- nothing eases into it, so its timing is never read.
    #[test]
    fn a_keyframe_list_takes_the_entrys_timing_unless_a_frame_names_its_own() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &props(
                &lua,
                r#"return { animate = { opacity = { duration = 200, easing = "Linear", loops = "Infinite",
                    keyframes = { 1, 0.4, { value = 1, duration = 50, easing = "OutCubic" } } } } }"#,
            ),
        )
        .unwrap()
        .remove("opacity")
        .expect("opacity has a spec");
        let sequence = spec.sequence().expect("the entry named keyframes");
        assert_eq!(sequence.loops, None, "\"Infinite\" is no count at all");
        assert_eq!(sequence.frames.len(), 3);
        assert_eq!(sequence.frames[1].duration, Duration::from_millis(200), "the entry's duration");
        assert_eq!(sequence.frames[2].duration, Duration::from_millis(50), "its own");
        assert_eq!(sequence.frames[2].easing, Easing::OutCubic);
        assert_eq!(sequence.cycle, Duration::from_millis(250), "the first frame's timing is not in it");
    }

    /// The walk itself: each segment eases between its own two frames, an exhausted run holds the
    /// last one, and an endless run wraps rather than stopping.
    #[test]
    fn a_sequence_walks_its_frames_and_wraps_only_when_it_loops() {
        let lua = Lua::new();
        let sequence = |src: &str| {
            parse_animate("rect", &props(&lua, src)).unwrap().remove("opacity").unwrap().sequence().unwrap()
        };
        // Compared with a tolerance: the wrap is an `f32` remainder, so 250 ms into a 200 ms cycle
        // lands a hair under 50 rather than on it.
        let at = |sequence: &Sequence, millis: u64| {
            let Animatable::Number(value) = sequence.at(Duration::from_millis(millis), "opacity") else {
                panic!("an opacity sequence carries numbers")
            };
            value
        };

        let once = sequence(
            r#"return { animate = { opacity = { duration = 100, easing = "Linear", keyframes = { 0, 1, 0 } } } }"#,
        );
        for (millis, want, why) in [
            (0, 0.0, "the first frame"),
            (50, 0.5, "halfway up"),
            (100, 1.0, "the first segment's end is the second's start"),
            (150, 0.5, "halfway back down"),
            (200, 0.0, "played out, holding the last frame"),
            (5_000, 0.0, "and holding it however long after"),
        ] {
            assert!((at(&once, millis) - want).abs() < 1e-5, "{why}: got {}", at(&once, millis));
        }
        assert!(once.done(Duration::from_millis(200)) && !once.done(Duration::from_millis(199)));

        let endless = sequence(
            r#"return { animate = { opacity = { duration = 100, easing = "Linear", loops = "Infinite",
                keyframes = { 0, 1, 0 } } } }"#,
        );
        assert!((at(&endless, 250) - 0.5).abs() < 1e-5, "back round the first segment: got {}", at(&endless, 250));
        assert!(!endless.done(Duration::from_secs(3_600)));
    }

    /// A frame of no duration is a jump, not a stop: it is stepped over, and its value shows as
    /// the start of whatever follows. That is QML's `PropertyAction` between two `PauseAnimation`s,
    /// which is how the reference config flashes a battery that was just plugged in.
    #[test]
    fn a_frame_with_no_duration_jumps_and_the_frame_after_it_holds() {
        let lua = Lua::new();
        let sequence = parse_animate(
            "rect",
            &props(
                &lua,
                r#"return { animate = { opacity = { duration = 100, loops = 2, keyframes = {
                    0, { value = 0, duration = 100 }, { value = 1, duration = 0 },
                    { value = 1, duration = 100 } } } } }"#,
            ),
        )
        .unwrap()
        .remove("opacity")
        .unwrap()
        .sequence()
        .unwrap();
        let at = |millis: u64| sequence.at(Duration::from_millis(millis), "opacity");
        assert_eq!(sequence.cycle, Duration::from_millis(200), "the jump costs no time");
        assert_eq!(at(50), Animatable::Number(0.0), "held dark");
        assert_eq!(at(100), Animatable::Number(1.0), "the jump lands and the hold at 1 begins");
        assert_eq!(at(150), Animatable::Number(1.0));
        assert_eq!(at(250), Animatable::Number(0.0), "second time round");
        assert_eq!(at(400), Animatable::Number(1.0), "two loops done, holding the last frame");
    }

    #[test]
    fn a_malformed_keyframe_list_is_refused() {
        let lua = Lua::new();
        let cases: [(&str, &[&str]); 7] = [
            ("keyframes = { 1 }", &["at least two"]),
            ("keyframes = 3", &["`keyframes` is a list"]),
            (r#"keyframes = { 1, "Fill" }"#, &["keyframes[2]"]),
            ("loops = 0, keyframes = { 1, 0 }", &["`loops`", "Infinite"]),
            ("keyframes = { 1, { value = 0, duration = -5 } }", &["keyframes[2]", "[0, 60000]"]),
            // A hole truncates the read at index 3, so the two frames that survive would have
            // passed the length check while the config quietly lost one.
            ("keyframes = { [1] = 1, [2] = 0, [4] = 1 }", &["3 entries", "index 1"]),
            ("from = 0, keyframes = { 1, 0 }", &["`from` and `keyframes`"]),
        ];
        for (entry, wanted) in cases {
            let text = refused(&lua, &format!("return {{ animate = {{ opacity = {{ duration = 1, {entry} }} }} }}"));
            assert!(wanted.iter().all(|want| text.contains(want)), "{entry}: {text}");
        }
    }

    /// An endless sequence runs as long as the shell does, so the phase has to come from whole
    /// nanoseconds rather than seconds in an `f32`, whose mantissa is out of milliseconds after a
    /// couple of hours and out of whole cycles after a fortnight.
    #[test]
    fn an_endless_sequence_keeps_its_phase_after_a_fortnight() {
        let lua = Lua::new();
        let sequence = parse_animate(
            "rect",
            &props(
                &lua,
                r#"return { animate = { opacity = { duration = 100, easing = "Linear", loops = "Infinite",
                    keyframes = { 0, 1 } } } }"#,
            ),
        )
        .unwrap()
        .remove("opacity")
        .unwrap()
        .sequence()
        .unwrap();
        let at = |elapsed: Duration| match sequence.at(elapsed, "opacity") {
            Animatable::Number(value) => value,
            other => panic!("an opacity sequence carries numbers, got {other:?}"),
        };
        let fortnight = Duration::from_secs(14 * 24 * 60 * 60);
        for (offset, want) in [(0, 0.0), (25, 0.25), (50, 0.5), (75, 0.75)] {
            let elapsed = fortnight + Duration::from_millis(offset);
            assert!((at(elapsed) - want).abs() < 1e-5, "a fortnight and {offset} ms in: got {}", at(elapsed));
        }
        assert!(at(fortnight) != at(fortnight + Duration::from_millis(1)), "and it still moves per millisecond");
    }

    #[test]
    fn a_bad_exit_block_is_refused_on_a_live_pass() {
        let lua = Lua::new();

        let text = refused(&lua, "return { animate = { exit = 200 } }");
        assert!(text.contains("animate.exit") && text.contains("expected a table"), "{text}");

        let text = refused(&lua, "return { animate = { exit = { duration = 100, widht = 0 } } }");
        assert!(text.contains("`widht`") && text.contains("`rect`"), "{text}");

        let text = refused(&lua, r#"return { animate = { exit = { duration = 100, opacity = "gone" } } }"#);
        assert!(text.contains("animate.exit.opacity"), "{text}");

        let text = refused(&lua, "return { animate = { exit = { opacity = 0 } } }");
        assert!(text.contains("animate.exit") && text.contains("duration"), "{text}");

        // A block naming nothing has nothing to time, so it stays legal and simply never runs.
        assert!(parse_animate("rect", &props(&lua, "return { animate = { exit = {} } }")).is_ok());
    }

    /// A departing node eases from what it displays, and from the property's own identity when it
    /// never set one: an absent `opacity` is `1`, not the `0` a bare number would default to.
    #[test]
    fn departing_starts_each_target_at_the_displayed_value_or_the_property_identity() {
        let lua = Lua::new();
        let mut properties =
            props(&lua, r#"return { width = 40, animate = { exit = { duration = 100, width = 0, opacity = 0 } } }"#);
        let mut tweens = Vec::new();
        let now = Instant::now();
        assert!(depart("rect", &mut tweens, &mut properties, now, &lua).unwrap());
        let started: HashMap<&str, &Tween> = tweens.iter().map(|t| (t.property.as_str(), t)).collect();
        assert_eq!(started["width"].from, Animatable::Number(40.0), "the displayed width");
        assert_eq!(started["opacity"].from, Animatable::Number(1.0), "an absent opacity is opaque");
        assert_eq!(started["opacity"].to, Animatable::Number(0.0));

        // Nothing to ease: the caller drops the node instead of holding it for a frame.
        let mut nothing = props(&lua, "return { width = 40 }");
        assert!(!depart("rect", &mut Vec::new(), &mut nothing, now, &lua).unwrap());
    }

    /// Departing is the end of everything else the node was doing. Otherwise a card interrupted
    /// mid-entry outlives its own exit block: the leftover tween keeps `advance_leaving` from
    /// dropping it, and keeps painting a transition nobody coordinated with the exit.
    #[test]
    fn departing_replaces_every_tween_the_node_was_already_running() {
        let lua = Lua::new();
        let mut properties = props(
            &lua,
            r##"return { width = 40, background = "#ff0000",
                animate = { background = 5000, exit = { duration = 100, opacity = 0 } } }"##,
        );
        let now = Instant::now();
        let mut tweens = vec![Tween {
            property: "background".to_string(),
            from: Animatable::Color(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
            to: Animatable::Color(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            started: now,
            spec: AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_secs(5), easing: Easing::Linear },
                delay: Duration::ZERO,
                from: None,
            },
            resting: false,
        }];
        assert!(depart("rect", &mut tweens, &mut properties, now, &lua).unwrap());
        let properties: Vec<&str> = tweens.iter().map(|t| t.property.as_str()).collect();
        assert_eq!(properties, ["opacity"], "the five-second background tween does not outlive the exit");
    }

    /// An unset percent is nothing and an unset colour paints nothing, so both have a real value
    /// to leave from. Falling through to the target instead would tween a value to itself and
    /// hold the node for the whole duration showing no motion at all.
    #[test]
    fn an_unset_percent_or_colour_departs_from_nothing_rather_than_from_the_target() {
        let lua = Lua::new();
        let mut properties = props(
            &lua,
            r##"return { animate = { exit = { duration = 100, width = "0%", background = "#3366ff" } } }"##,
        );
        let mut tweens = Vec::new();
        assert!(depart("rect", &mut tweens, &mut properties, Instant::now(), &lua).unwrap());
        let started: HashMap<&str, &Tween> = tweens.iter().map(|t| (t.property.as_str(), t)).collect();
        assert_eq!(started["width"].from, Animatable::Percent(0.0));
        assert_eq!(started["background"].from, Animatable::Color(Rgba { r: 0.2, g: 0.4, b: 1.0, a: 0.0 }));
    }

    #[test]
    fn an_edge_table_halfway_is_the_per_edge_midpoint_and_absent_edges_are_zero() {
        let lua = Lua::new();
        let table = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("margin", Some(&value)).unwrap().unwrap()
        };
        let mid = table("return { top = 10, left = -20 }").lerp(table("return { top = 20, right = 8 }"), 0.5, "margin");
        assert_eq!(mid, Animatable::Fields { keys: EDGES, values: [15.0, 4.0, 0.0, -10.0] });
        let Value::Table(back) = mid.to_value(&lua).unwrap() else { panic!("edges write back as a table") };
        assert_eq!(back.get::<f32>("left").unwrap(), -10.0);
        let colours: Value = lua.load(r##"return { top = "#ff0000" }"##).eval().unwrap();
        assert_eq!(Animatable::from_value("border_color", Some(&colours)).unwrap(), None, "colour edges snap");
    }

    #[test]
    fn an_axis_table_tweens_on_x_and_y_and_an_absent_scale_axis_is_one() {
        let lua = Lua::new();
        let table = |src: &str, property: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value(property, Some(&value)).unwrap().unwrap()
        };
        let mid = table("return { x = 1 }", "scale").lerp(table("return { x = 2, y = 3 }", "scale"), 0.5, "scale");
        assert_eq!(mid, Animatable::Fields { keys: AXES, values: [1.5, 2.0, 1.0, 1.0] });
        let Value::Table(back) = mid.to_value(&lua).unwrap() else { panic!("axes write back as a table") };
        assert_eq!((back.get::<f32>("x").unwrap(), back.get::<f32>("y").unwrap()), (1.5, 2.0));
        assert!(!back.contains_key("top").unwrap());
        // A number against a table snaps: a `scale = 2` meeting `scale = { x = 2 }`.
        let snapped = Animatable::Number(2.0).lerp(table("return { x = 2 }", "scale"), 0.5, "scale");
        assert_eq!(snapped, table("return { x = 2 }", "scale"));
    }

    #[test]
    fn a_duration_that_rounds_to_no_milliseconds_is_refused() {
        let lua = Lua::new();
        for src in ["return { animate = { width = 0 } }", "return { animate = { width = 0.1 } }"] {
            // A bound written in floats lets `0.1` through and rounding then makes it nothing, so
            // the bound is on the milliseconds the tween actually gets: a tween of none reports
            // itself finished on the frame it starts and never moves.
            assert!(refused(&lua, src).contains("[1, 60000]"), "{src}");
        }
    }

    #[test]
    fn a_colour_halfway_is_the_channel_midpoint_and_round_trips_as_hex() {
        let lua = Lua::new();
        let black = Animatable::from_value("background", Some(&Value::String(lua.create_string("#000000").unwrap())))
            .unwrap()
            .unwrap();
        let white = Animatable::from_value("background", Some(&Value::String(lua.create_string("#ffffff").unwrap())))
            .unwrap()
            .unwrap();
        let mid = black.lerp(white, 0.5, "background");
        let Value::String(hex) = mid.to_value(&lua).unwrap() else { panic!("a colour writes back as a string") };
        assert_eq!(hex.to_str().unwrap(), "#808080ff");
    }

    #[test]
    fn a_percent_halfway_is_the_midpoint_and_round_trips_as_a_percent_string() {
        let lua = Lua::new();
        let pct = |s: &str| {
            Animatable::from_value("width", Some(&Value::String(lua.create_string(s).unwrap()))).unwrap().unwrap()
        };
        let mid = pct("40%").lerp(pct("60%"), 0.5, "width");
        let Value::String(text) = mid.to_value(&lua).unwrap() else { panic!("a percent writes back as a string") };
        assert_eq!(text.to_str().unwrap(), "50.000%");
        let fill = Value::String(lua.create_string("Fill").unwrap());
        assert_eq!(Animatable::from_value("width", Some(&fill)).unwrap(), None, "`Fill` is not an endpoint");
    }

    /// The three regimes and one hand-over, against a Runge-Kutta integration of
    /// `s'' + damping * s' + stiffness * s = 0` done outside this module -- not against another
    /// arm of the same `match`, which is the tautology the easing table was rewritten to avoid.
    #[test]
    fn every_spring_regime_matches_an_integration_of_the_equation_it_solves() {
        /// A spring's constants, its starting velocity, and what the integration says its
        /// displacement and rate are at three instants.
        struct Row {
            name: &'static str,
            stiffness: f32,
            damping: f32,
            velocity: f32,
            at: [(f32, f32, f32); 3],
        }
        #[rustfmt::skip]
        let reference = &[
            Row { name: "underdamped",   stiffness: 200.0, damping: 10.0, velocity: 0.0, at: [
                (0.05,  0.795370, -7.232_43), (0.10,  0.371074, -8.88951), (0.25, -0.300436,  0.714015)] },
            Row { name: "critical",      stiffness: 100.0, damping: 20.0, velocity: 0.0, at: [
                (0.05,  0.909796, -3.032653), (0.10,  0.735759, -3.678794), (0.25,  0.287297, -2.052125)] },
            Row { name: "overdamped",    stiffness: 100.0, damping: 40.0, velocity: 0.0, at: [
                (0.05,  0.930295, -2.0781),   (0.10,  0.822263, -2.139091), (0.25,  0.551353, -1.477107)] },
            Row { name: "with velocity", stiffness: 200.0, damping: 10.0, velocity: 3.0, at: [
                (0.05,  0.686884, -8.533672), (0.10,  0.237731, -8.669304), (0.25, -0.289726,  1.508_22)] },
        ];
        for Row { name, stiffness, damping, velocity, at } in reference {
            let spring = Spring::new(*stiffness, *damping, *velocity);
            for (seconds, displacement, rate) in at {
                let got = spring.displacement(*seconds);
                assert!(
                    (got - displacement).abs() < 1e-4,
                    "{name} displacement at {seconds}s: got {got}, want {displacement}"
                );
                let got = spring.rate(*seconds);
                assert!((got - rate).abs() < 1e-3, "{name} rate at {seconds}s: got {got}, want {rate}");
            }
        }
    }

    #[test]
    fn a_spring_starts_on_its_source_ends_on_its_target_and_is_allowed_to_overshoot() {
        let spring = Spring::new(200.0, 10.0, 0.0);
        assert!(spring.at(Duration::ZERO).abs() < 1e-6, "no progress before it moves");
        assert!(spring.at(Duration::from_millis(250)) > 1.0, "an underdamped spring passes its target");
        assert_eq!(spring.at(spring.settles), 1.0, "and is pinned exactly on it once settled");
        assert_eq!(spring.at(spring.settles * 2), 1.0);

        // The overshoot is the property's own range to absorb, exactly as `OutBack`'s is.
        let past = spring.at(Duration::from_millis(250));
        assert_eq!(
            Animatable::Number(1.0).lerp(Animatable::Number(0.0), past, "opacity"),
            Animatable::Number(0.0),
            "opacity cannot go negative"
        );
    }

    /// Every spring has to stop asking for frames, and every spring has to move. The second half
    /// is what the extreme pairs are here for: a heavily overdamped one used to compute its near
    /// root as the difference of two `f32` that agreed to the last bit, get exactly zero, and hold
    /// its starting value for the full minute before snapping. Asserting only that it settles
    /// passed that happily.
    #[test]
    fn every_spring_settles_within_a_minute_and_none_settles_before_it_arrives() {
        for (stiffness, damping) in [
            (200.0, 10.0),
            (100.0, 20.0),
            (100.0, 40.0),
            (1.0, 0.01),
            (100_000.0, 10_000.0),
            (0.0001, 10_000.0),
            (100_000.0, 0.0001),
            (0.0001, 0.0001),
        ] {
            let spring = Spring::new(stiffness, damping, 0.0);
            // Sampled across the whole window rather than at one instant: a barely damped spring
            // is still ringing at its midpoint and may be heading either way there, so no single
            // reading says whether it is alive. What every one of these must not be is *still* --
            // a displacement of exactly `1` at every instant, with a rate of exactly `0`, which is
            // the shape the cancellation produced. A spring in treacle is allowed to be slow, and
            // one of these pairs really does need a hundred million seconds.
            let mut moved = false;
            for step in 0..=20 {
                let seconds = spring.settles.as_secs_f32() * step as f32 / 20.0;
                let (displacement, rate) = (spring.displacement(seconds), spring.rate(seconds));
                assert!(displacement.is_finite() && rate.is_finite(), "{stiffness}/{damping}: {displacement}, {rate}");
                moved |= displacement != 1.0;
            }
            assert!(moved, "{stiffness}/{damping} holds its starting value at every instant");
        }
        for (stiffness, damping) in [(200.0, 10.0), (100.0, 20.0), (100.0, 40.0), (1.0, 0.01), (100_000.0, 10_000.0)] {
            let spring = Spring::new(stiffness, damping, 0.0);
            assert!(spring.settles <= Duration::from_secs_f32(Spring::LONGEST), "{stiffness}/{damping} never stops");
            assert!(spring.done(spring.settles), "{stiffness}/{damping} is not done when it says it is");
            // Whatever it says, it really is within `REST` of the target by then -- the bound is
            // allowed to be late, never early.
            let displacement = spring.displacement(spring.settles.as_secs_f32());
            assert!(
                displacement.abs() <= Spring::REST || spring.settles.as_secs_f32() >= Spring::LONGEST,
                "{stiffness}/{damping} calls itself settled {displacement} out"
            );
        }
    }

    /// The whole reason a spring is here: a target that moves mid-flight bends the motion instead
    /// of restarting it. An eased tween has no way to do this, so it is the comparison.
    #[test]
    fn a_retargeted_spring_keeps_the_speed_it_had_where_an_easing_would_start_over() {
        let lua = Lua::new();
        let sprung = spec(&lua, "return { animate = { width = { spring = { stiffness = 200, damping = 10 } } } }");
        let started = Instant::now();
        let running = Tween {
            property: "width".into(),
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec: sprung.clone(),
            resting: false,
        };
        let midway = started + Duration::from_millis(50);
        let displayed = running.at(midway);
        assert!(matches!(displayed, Animatable::Number(n) if n > 0.0 && n < 100.0), "{displayed:?}");

        // The target moves further out in the same direction; the new run must already be moving.
        let Motion::Spring(fresh) = sprung.motion else { panic!("parsed a spring") };
        let handed = fresh.handed(&running, displayed, Animatable::Number(200.0), midway);
        assert!(handed.velocity > 0.0, "the hand-over carries the speed it had, got {}", handed.velocity);
        assert!(
            handed.rate(0.0) < fresh.rate(0.0),
            "and so closes faster at its first instant than a run starting from still"
        );

        // A target that moves the other way hands over a velocity pointing away from it, which is
        // what makes the value swing through rather than snap back.
        let backwards = fresh.handed(&running, displayed, Animatable::Number(-50.0), midway);
        assert!(backwards.velocity < 0.0, "got {}", backwards.velocity);
    }

    /// The hand-over's magnitude, not just its sign. Matching the value's own rate across the
    /// swap gives `-rate(t) * (D_old . D_new) / |D_new|^2`, and the run's displacement there is
    /// the whole of it -- `from - to`, not `displayed - to`, which is that same displacement
    /// already scaled by how much is left to cross. Scaling it twice hands over almost nothing
    /// exactly when the hand-over matters most: late in a run, or once an overshoot has taken the
    /// value past its target and the leftover has changed sign.
    #[test]
    fn the_handed_velocity_matches_the_rate_the_value_was_actually_moving_at() {
        let spring = Spring::new(200.0, 10.0, 0.0);
        let started = Instant::now();
        let running = Tween {
            property: "width".into(),
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec: AnimationSpec { motion: Motion::Spring(spring), delay: Duration::ZERO, from: None },
            resting: false,
        };
        // 220 ms in, an underdamped spring of these constants is past its target and coming back,
        // so the displacement left has the opposite sign to the one it started with.
        for millis in [20, 60, 220] {
            let now = started + Duration::from_millis(millis);
            let displayed = running.at(now);
            let target = Animatable::Number(300.0);
            let handed = spring.handed(&running, displayed, target, now);

            let old_span = -100.0_f32;
            let Animatable::Number(shown) = displayed else { panic!("a number") };
            let new_span = shown - 300.0;
            let want = -spring.rate(Duration::from_millis(millis).as_secs_f32()) * old_span / new_span;
            assert!((handed.velocity - want).abs() < 1e-3, "at {millis} ms: handed {}, want {want}", handed.velocity);
        }
    }

    /// A pass that re-resolves for some unrelated signal keeps the running tween, and used to keep
    /// it with the freshly parsed spec. For every other motion that is harmless -- the curve is
    /// the same curve -- but a spring's `velocity` is the rate the last retarget handed it rather
    /// than anything the config wrote, and parsing yields one at rest. Any unrelated change would
    /// stop a moving spring dead, which is the one thing the hand-over exists to prevent.
    /// A bezier's `y` is deliberately unbounded, so a curve that overshoots hard makes the
    /// endpoint error visible: bisection stops about 1e-6 of a parameter short of the ends, and
    /// the curve's steep opening multiplies that into a value nowhere near where the tween starts.
    #[test]
    fn a_bezier_begins_on_its_source_and_ends_on_its_target_however_far_its_controls_reach() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &props(&lua, "return { animate = { width = { duration = 100, easing = { 0, 1000000, 1, 1000000 } } } }"),
        )
        .unwrap()
        .remove("width")
        .unwrap();
        let started = Instant::now();
        let tween = Tween {
            property: "width".into(),
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec,
            resting: false,
        };
        assert_eq!(tween.at(started), Animatable::Number(0.0), "it starts where it starts");
        assert_eq!(tween.at(started + Duration::from_millis(100)), Animatable::Number(100.0), "and lands on target");
    }

    #[test]
    fn a_moving_spring_survives_a_pass_that_changes_nothing_about_it() {
        let lua = Lua::new();
        let source = "return { width = 300, animate = { width = { spring = { stiffness = 200, damping = 10 } } } }";
        let started = Instant::now();
        let carried = Spring::new(200.0, 10.0, 40.0);
        let running = [Tween {
            property: "width".into(),
            from: Animatable::Number(0.0),
            to: Animatable::Number(300.0),
            started,
            spec: AnimationSpec { motion: Motion::Spring(carried), delay: Duration::ZERO, from: None },
            resting: false,
        }];
        let shown: HashMap<String, Value> = HashMap::from([("width".to_string(), Value::Number(120.0))]);

        let mut properties = props(&lua, source);
        let now = started + Duration::from_millis(30);
        let tweens = retarget("rect", Some((&running[..], &shown)), &mut properties, now, &lua).unwrap();
        let Motion::Spring(kept) = tweens[0].spec.motion else { panic!("still a spring") };
        assert_eq!(kept.velocity, 40.0, "the handed velocity survives an unrelated resolve");
        assert_eq!(tweens[0].started, started, "and the run is the same run, not a fresh one");

        // Only the spring is carried, not the entry around it: an edited `delay` beside untouched
        // constants still lands, where taking the running spec whole would have swallowed it.
        let waiting = [Tween {
            spec: AnimationSpec { motion: Motion::Spring(carried), delay: Duration::from_millis(1000), from: None },
            ..running[0].clone()
        }];
        let mut properties = props(&lua, source);
        let tweens = retarget("rect", Some((&waiting[..], &shown)), &mut properties, now, &lua).unwrap();
        let Motion::Spring(kept) = tweens[0].spec.motion else { panic!("still a spring") };
        assert_eq!(kept.velocity, 40.0, "the handed rate carries");
        assert_eq!(tweens[0].spec.delay, Duration::ZERO, "and the edited delay lands on it");

        // Editing either constant is a config change: the new spring arrives at its parsed rest.
        // The run it lands in is still the one already going -- nothing here restarts a tween whose
        // target never moved, so `started` and `from` are the running one's.
        let stiffer = "return { width = 300, animate = { width = { spring = { stiffness = 400, damping = 10 } } } }";
        let mut properties = props(&lua, stiffer);
        let tweens = retarget("rect", Some((&running[..], &shown)), &mut properties, now, &lua).unwrap();
        let Motion::Spring(fresh) = tweens[0].spec.motion else { panic!("still a spring") };
        assert_eq!((fresh.stiffness, fresh.velocity), (400.0, 0.0), "an edited constant is a new spring");
        assert_eq!(tweens[0].started, started, "carried by the run already going");
        assert_eq!(tweens[0].from, Animatable::Number(0.0), "which keeps the value it set out from");
    }

    /// `duration` and `keyframes` beside a spring were already refused; `easing` was parsed and
    /// then dropped on the floor, so a config could tune a curve that never ran. A field the
    /// chosen motion does not read is a config that believes something it is not getting.
    #[test]
    fn a_field_belonging_to_another_motion_is_refused_rather_than_ignored() {
        let lua = Lua::new();
        let spring = "spring = { stiffness = 200, damping = 10 }";
        for beside in ["easing = \"Linear\"", "duration = 200", "duration = \"oops\"", "loops = 3"] {
            let text = refused(&lua, &format!("return {{ animate = {{ width = {{ {spring}, {beside} }} }} }}"));
            assert!(text.contains("a `spring` has no"), "{beside}: {text}");
        }
        // `loops` without a list to walk was read by nobody at all, typo and count alike.
        let text = refused(&lua, "return { animate = { width = { duration = 10, loops = 3 } } }");
        assert!(text.contains("`loops`"), "{text}");
    }

    /// One time through takes no time only when every segment is a jump. Counted, that is a snap a
    /// config writes by leaving `animate` off the property; endless, it asks the compositor for a
    /// frame forever while showing one still value, which is a wakelock with nothing to show.
    #[test]
    fn a_sequence_whose_every_segment_is_a_jump_is_refused() {
        let lua = Lua::new();
        let text = refused(
            &lua,
            r#"return { animate = { width = { duration = 100, loops = "Infinite",
                keyframes = { 0, { value = 1, duration = 0 } } } } }"#,
        );
        assert!(text.contains("takes none is a jump"), "{text}");
    }

    /// The lead-in holds where the run opens. Every easing and every spring read elapsed zero as
    /// their own start, but a sequence opening on a jump plays that jump at zero, so the delay
    /// used to be spent showing the value after it.
    #[test]
    fn a_delay_before_a_sequence_holds_its_first_frame_rather_than_its_first_jump() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &props(
                &lua,
                r#"return { animate = { width = { duration = 100, delay = 1000,
                    keyframes = { 40, { value = 0, duration = 0 }, 40 } } } }"#,
            ),
        )
        .unwrap()
        .remove("width")
        .unwrap();
        let started = Instant::now();
        let Motion::Sequence(ref sequence) = spec.motion else { panic!("a sequence") };
        let first = sequence.frames[0].value;
        let tween =
            Tween { property: "width".into(), from: first, to: first, started, spec: spec.clone(), resting: false };
        assert_eq!(tween.at(started), Animatable::Number(40.0), "the lead-in holds the first frame");
        assert_eq!(tween.at(started + Duration::from_millis(999)), Animatable::Number(40.0), "for all of it");
        assert_eq!(tween.at(started + Duration::from_millis(1000)), Animatable::Number(0.0), "then the jump lands");
    }

    /// The phase walk stays in whole nanoseconds. In `f32` the segment subtraction rounds against
    /// the comparison -- `0.4 - 0.3 < 0.1` holds -- so an instant landing exactly on a boundary
    /// reads as a hair short of it and picks the segment that has just ended.
    #[test]
    fn an_instant_exactly_on_a_segment_boundary_belongs_to_the_segment_it_begins() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &props(
                &lua,
                r#"return { animate = { width = { duration = 100, easing = "Linear",
                    keyframes = { 0, { value = 10, duration = 300 }, { value = 20, duration = 100 },
                        { value = 99, duration = 0 }, { value = 5, duration = 100 } } } } }"#,
            ),
        )
        .unwrap()
        .remove("width")
        .unwrap();
        let Motion::Sequence(sequence) = spec.motion else { panic!("a sequence") };
        // 300 + 100 lands exactly where the second segment ends and a jump to 99 fires. In floats
        // the walk arrives with 0.099999994 left of a 0.1 segment and reports itself still inside
        // it, a hair short of a jump that should already have happened.
        assert_eq!(sequence.at(Duration::from_millis(400), "width"), Animatable::Number(99.0));
    }

    #[test]
    fn a_spring_is_two_positive_numbers_and_says_nothing_about_duration() {
        let lua = Lua::new();

        let Motion::Spring(spring) =
            spec(&lua, "return { animate = { width = { spring = { stiffness = 220, damping = 26 } } } }").motion
        else {
            panic!("expected a spring")
        };
        assert_eq!((spring.stiffness, spring.damping, spring.velocity), (220.0, 26.0, 0.0));

        let cases: [(&str, &[&str]); 6] = [
            ("duration = 10, spring = { stiffness = 1, damping = 1 }", &["has no `duration`"]),
            ("spring = { stiffness = 1, damping = 1 }, keyframes = { 0, 1 }", &["two different motions"]),
            ("spring = { damping = 26 }", &["stiffness", "(0, 100000]"]),
            ("spring = { stiffness = 220, damping = 0 }", &["damping", "(0, 10000]"]),
            ("spring = 220", &["table of `stiffness` and `damping`"]),
            // Without a spring the duration is still required, so lifting it is scoped to the one.
            (r#"easing = "Linear""#, &["expected a duration in ms"]),
        ];
        for (entry, wanted) in cases {
            let text = refused(&lua, &format!("return {{ animate = {{ width = {{ {entry} }} }} }}"));
            assert!(wanted.iter().all(|want| text.contains(want)), "{entry}: {text}");
        }
    }

    #[test]
    fn a_delay_holds_the_start_value_then_runs_the_whole_duration() {
        let started = Instant::now();
        let tween = Tween {
            property: "width".into(),
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec: AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_millis(100), easing: Easing::Linear },
                delay: Duration::from_millis(50),
                from: None,
            },
            resting: false,
        };
        assert_eq!(tween.at(started), Animatable::Number(0.0));
        assert_eq!(
            tween.at(started + Duration::from_millis(50)),
            Animatable::Number(0.0),
            "still held at the hand-off"
        );
        assert_eq!(tween.at(started + Duration::from_millis(100)), Animatable::Number(50.0), "halfway, 50 ms late");
        assert_eq!(tween.at(started + Duration::from_millis(150)), Animatable::Number(100.0));
        // The delay is added to the life of the tween, not taken out of it.
        assert!(!tween.done(started + Duration::from_millis(100)));
        assert!(tween.done(started + Duration::from_millis(150)));
    }

    /// A delay offsets a sequence's whole run once, not each time round: the phase is measured
    /// from the moment the first frame is left, so an endless loop keeps its cycle.
    #[test]
    fn a_delay_offsets_a_sequence_once_rather_than_every_cycle() {
        let lua = Lua::new();
        let specs = parse_animate(
            "rect",
            &props(&lua, "return { animate = { opacity = { duration = 100, delay = 40, keyframes = { 0, 1 }, loops = \"Infinite\" } } }"),
        )
        .unwrap();
        let started = Instant::now();
        let tween = Tween {
            property: "opacity".into(),
            from: Animatable::Number(0.0),
            to: Animatable::Number(1.0),
            started,
            spec: specs["opacity"].clone(),
            resting: false,
        };
        let at = |ms| match tween.at(started + Duration::from_millis(ms)) {
            Animatable::Number(n) => n,
            other => panic!("{other:?}"),
        };
        assert!(at(0) < 1e-6 && at(40) < 1e-6, "held on the first frame through the delay");
        assert!((at(90) - 0.5).abs() < 1e-3, "half a cycle in, 40 ms late");
        assert!((at(190) - 0.5).abs() < 1e-3, "and half of the next one, still 40 ms late");
    }

    #[test]
    fn a_delay_is_a_whole_number_of_milliseconds_within_a_minute() {
        let lua = Lua::new();
        let specs =
            parse_animate("rect", &props(&lua, "return { animate = { width = { duration = 10, delay = 40 } } }"))
                .unwrap();
        assert_eq!(specs["width"].delay, Duration::from_millis(40));
        let bare = parse_animate("rect", &props(&lua, "return { animate = { width = 10 } }")).unwrap();
        assert_eq!(bare["width"].delay, Duration::ZERO, "absent is no delay");
        let zeroed =
            parse_animate("rect", &props(&lua, "return { animate = { width = { duration = 10, delay = 0 } } }"))
                .unwrap();
        assert_eq!(zeroed["width"].delay, Duration::ZERO, "zero is the default written out, not a refusal");

        let cases: [(&str, &[&str]); 5] = [
            ("delay = 60001", &["delay", "[0, 60000]"]),
            ("delay = -1", &["delay", "[0, 60000]"]),
            // A value that is not a number at all is a typo, not a zero: `value_as_f32` cannot
            // tell a string from an absent key, so a silent `Duration::ZERO` would drop the
            // lead-in without saying so, while the same typo on `duration` fails the pass.
            (r#"delay = "50""#, &["expected a delay in ms"]),
            ("delay = {}", &["expected a delay in ms"]),
            (r#"keyframes = { 0, { value = 1, duration = "5" } }"#, &["expected a duration in ms"]),
        ];
        for (entry, wanted) in cases {
            let text = refused(&lua, &format!("return {{ animate = {{ width = {{ duration = 10, {entry} }} }} }}"));
            assert!(wanted.iter().all(|want| text.contains(want)), "{entry}: {text}");
        }
    }

    #[test]
    fn a_tween_reads_from_at_its_start_and_to_at_its_end() {
        let started = Instant::now();
        let tween = Tween {
            property: "width".into(),
            from: Animatable::Number(40.0),
            to: Animatable::Number(90.0),
            started,
            spec: AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_millis(100), easing: Easing::Linear },
                delay: Duration::ZERO,
                from: None,
            },
            resting: false,
        };
        assert_eq!(tween.at(started), Animatable::Number(40.0));
        assert_eq!(tween.at(started + Duration::from_millis(50)), Animatable::Number(65.0));
        assert_eq!(tween.at(started + Duration::from_millis(500)), Animatable::Number(90.0));
        assert!(tween.done(started + Duration::from_millis(100)));
    }

    /// ADR-0181. A dissolve is a clock and a curve: nothing to write back, nothing to refuse, and
    /// it reports its own end rather than resting at 1.0 the way a played-out sequence does.
    #[test]
    fn a_dissolve_eases_across_its_duration_and_reports_when_it_is_over() {
        let spec = TransitionSpec {
            duration: Duration::from_millis(400),
            easing: Easing::Linear,
            shader: None,
            params: Vec::new(),
        };
        let started = Instant::now();
        let mut dissolve = Dissolve::start("/tmp/a.png".into(), "/tmp/b.png".into(), spec.clone(), started);
        assert_eq!(dissolve.progress, 0.0, "it opens on the outgoing picture");

        assert!(dissolve.advance(started + Duration::from_millis(100)));
        assert!((dissolve.progress - 0.25).abs() < 1e-5);
        assert!(dissolve.advance(started + Duration::from_millis(300)));
        assert!((dissolve.progress - 0.75).abs() < 1e-5);

        assert!(!dissolve.advance(started + Duration::from_millis(400)), "the end is the end, not a rest at 1.0");

        // An overshooting curve is clamped before it is stored: this number is drawn as an alpha,
        // and `Easing::apply` clamps its input, not its output (ADR-0183).
        let overshoot = TransitionSpec {
            duration: Duration::from_millis(400),
            easing: Easing::OutBack,
            shader: None,
            params: Vec::new(),
        };
        let mut dissolve = Dissolve::start("/tmp/a.png".into(), "/tmp/b.png".into(), overshoot, started);
        for millis in [40, 120, 200, 280, 360] {
            assert!(dissolve.advance(started + Duration::from_millis(millis)));
            assert!((0.0..=1.0).contains(&dissolve.progress), "{millis}ms gave {}", dissolve.progress);
        }
        assert!(!dissolve.advance(started + Duration::from_secs(9)));

        // A clock that has gone backwards saturates rather than wrapping into a huge progress.
        let mut dissolve = Dissolve::start("/tmp/a.png".into(), "/tmp/b.png".into(), spec.clone(), started);
        assert!(dissolve.advance(started - Duration::from_millis(50)));
        assert_eq!(dissolve.progress, 0.0);
    }
}
