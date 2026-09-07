//! `animate` (`docs/oblisk-idl-api-specs.md` § 5.1): per-property tweens on a retained node, the
//! engine's answer to QML's `Behavior on x { NumberAnimation { ... } }` (ADR-0145). A node names
//! the properties it wants eased and how long; when a pass resolves a different target for one of
//! them, the node's [`Tween`] carries the displayed value from where it was to where it is going,
//! and `layout::scene::Scene::tick` advances it between passes without running any Lua.
//!
//! This module owns the parsing and the arithmetic. Where the tween lives, when one starts and
//! what a tick relays out are `layout::scene`'s.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use mlua::{Lua, Value};

use super::{LayoutError, Rgba, invalid, parse_hex_color, preview_for_error, value_as_f32};

/// The one thing a hex colour has to look like to reach `parse_hex_color` again next pass.
fn hex_of(color: Rgba) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", byte(color.r), byte(color.g), byte(color.b), byte(color.a))
}

/// QML's `Easing.Type` names, the subset a shell config reaches for. Spelled the same so a
/// `Behavior on width { NumberAnimation { easing.type: Easing.OutCubic } }` ports by dropping the
/// prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    /// Overshoots past the target and settles. The tween clamps the result to the property's
    /// legal range, so a `width` easing to `0` never goes negative into the parser.
    OutBack,
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
        ("OutBack", Easing::OutBack),
    ];

    fn parse(name: &str) -> Option<Self> {
        Self::NAMES.iter().find(|(spelling, _)| *spelling == name).map(|(_, easing)| *easing)
    }

    /// Progress `t` in `[0, 1]` to the eased fraction of the distance covered.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        let in_out = |power: i32| {
            if t < 0.5 { 0.5 * (2.0 * t).powi(power) } else { 1.0 - 0.5 * (2.0 - 2.0 * t).powi(power) }
        };
        match self {
            Easing::Linear => t,
            Easing::InQuad => t * t,
            Easing::OutQuad => 1.0 - (1.0 - t).powi(2),
            Easing::InOutQuad => in_out(2),
            Easing::InCubic => t.powi(3),
            Easing::OutCubic => 1.0 - (1.0 - t).powi(3),
            Easing::InOutCubic => in_out(3),
            Easing::OutBack => {
                const C1: f32 = 1.70158;
                const C3: f32 = C1 + 1.0;
                1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
            }
        }
    }
}

/// How one property eases: `animate = { width = 200 }` or
/// `animate = { width = { duration = 200, easing = "OutCubic" } }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimationSpec {
    pub duration: Duration,
    pub easing: Easing,
}

/// Properties a tween can carry, by value shape. A property outside this list is refused by
/// [`parse_animate`] so a misspelling fails the pass instead of silently snapping.
///
/// ponytail: numbers and colours only. `margin`, `padding` and `border_width` animate in their
/// bare-number form; their edge-table form and `border_color`'s snap, because writing a table
/// back means building one per frame. Upgrade path: an `Animatable::Edges([f32; 4])` arm.
const NUMBERS: &[&str] = &[
    "width",
    "height",
    "max_width",
    "max_height",
    "margin",
    "padding",
    "spacing",
    "radius",
    "border_width",
    "opacity",
    "font_size",
    "size",
];
const COLORS: &[&str] = &["background", "border_color", "foreground"];

/// The range an eased number is clamped into, so an overshooting easing lands inside what the
/// property's parser accepts. `margin` accepts a negative; nothing else does.
fn range_of(property: &str) -> (f32, f32) {
    match property {
        "opacity" => (0.0, 1.0),
        "margin" => (-8192.0, 8192.0),
        _ => (0.0, 8192.0),
    }
}

/// `animate`'s table, resolved: which properties ease and how. Absent means none. The table
/// itself may be a signal, resolved like any other property; entries inside it are plain values.
pub fn parse_animate(properties: &HashMap<String, Value>) -> Result<HashMap<String, AnimationSpec>, LayoutError> {
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
        if !NUMBERS.contains(&property.as_str()) && !COLORS.contains(&property.as_str()) {
            return Err(invalid(
                "animate",
                format!("`{property}` is not a property a tween can carry (numbers and colours only)"),
            ));
        }
        let field = format!("animate.{property}");
        let (duration, easing) = match &entry {
            Value::Table(spec) => {
                let duration: Value = spec.get("duration").map_err(|e| invalid(&field, e.to_string()))?;
                let easing: Value = spec.get("easing").map_err(|e| invalid(&field, e.to_string()))?;
                let easing = match easing {
                    Value::Nil => Easing::default(),
                    Value::String(name) => {
                        let name = name.to_str().map_err(|e| invalid(&field, e.to_string()))?;
                        Easing::parse(&name).ok_or_else(|| {
                            let known: Vec<&str> = Easing::NAMES.iter().map(|(n, _)| *n).collect();
                            invalid(&field, format!("unknown easing `{name}`; one of {}", known.join(", ")))
                        })?
                    }
                    other => {
                        return Err(invalid(
                            &field,
                            format!("easing must be a name, got {}", preview_for_error(&other)),
                        ));
                    }
                };
                (duration, easing)
            }
            number => (number.clone(), Easing::default()),
        };
        let millis = value_as_f32(&field, &duration)?.ok_or_else(|| {
            invalid(&field, format!("expected a duration in ms, got {}", preview_for_error(&duration)))
        })?;
        if millis <= 0.0 || millis > 60_000.0 {
            return Err(invalid(&field, format!("duration must be within (0, 60000] ms, got {millis}")));
        }
        // Whole milliseconds: `from_secs_f32` would carry `200` as `200.000003ms`.
        out.insert(property, AnimationSpec { duration: Duration::from_millis(millis.round() as u64), easing });
    }
    Ok(out)
}

/// A value a tween can sit between: the two shapes [`parse_animate`] admits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Animatable {
    Number(f32),
    Color(Rgba),
}

impl Animatable {
    /// The typed reading of `property`'s current value, or `None` when the value is a shape no
    /// tween carries (`"Fill"`, an edge table, absent): the caller snaps then. A value of the
    /// right shape that fails its parse is an error, the same one the property's own parser
    /// raises.
    pub fn from_value(property: &str, value: Option<&Value>) -> Result<Option<Self>, LayoutError> {
        let Some(value) = value else { return Ok(None) };
        if COLORS.contains(&property) {
            return match value {
                Value::String(s) => {
                    let s = s.to_str().map_err(|e| invalid(property, e.to_string()))?;
                    Ok(Some(Self::Color(parse_hex_color(property, &s)?)))
                }
                _ => Ok(None),
            };
        }
        Ok(value_as_f32(property, value)?.map(Self::Number))
    }

    fn lerp(self, to: Self, t: f32, property: &str) -> Self {
        match (self, to) {
            (Self::Number(a), Self::Number(b)) => {
                let (low, high) = range_of(property);
                Self::Number((a + (b - a) * t).clamp(low, high))
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
            Self::Color(color) => Value::String(lua.create_string(hex_of(color))?),
        })
    }
}

/// One property of one retained node, in flight from `from` to `to`. `started` is when the pass
/// that saw the target change ran; progress is elapsed time over the spec's duration (ADR-0130
/// decision 2), never accumulated frame deltas.
#[derive(Debug, Clone, PartialEq)]
pub struct Tween {
    pub property: String,
    pub from: Animatable,
    pub to: Animatable,
    pub started: Instant,
    pub spec: AnimationSpec,
}

impl Tween {
    pub fn at(&self, now: Instant) -> Animatable {
        let elapsed = now.saturating_duration_since(self.started);
        let t = (elapsed.as_secs_f32() / self.spec.duration.as_secs_f32()).min(1.0);
        self.from.lerp(self.to, self.spec.easing.apply(t), &self.property)
    }

    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.spec.duration
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
/// `animate` stopped naming loses its tween and snaps.
pub fn retarget(
    retained: Option<(&[Tween], &HashMap<String, Value>)>,
    properties: &mut HashMap<String, Value>,
    now: Instant,
    lua: &Lua,
) -> Result<Vec<Tween>, LayoutError> {
    let specs = parse_animate(properties)?;
    let Some((running, displayed)) = retained else {
        return Ok(Vec::new());
    };
    let mut tweens = Vec::with_capacity(specs.len());
    for (property, spec) in specs {
        let Some(target) = Animatable::from_value(&property, properties.get(&property))? else {
            continue;
        };
        let running = running.iter().find(|t| t.property == property);
        let Some(displayed) = Animatable::from_value(&property, displayed.get(&property))? else { continue };
        let retained_target = running.map_or(displayed, |tween| tween.to);
        let tween = match running {
            _ if retained_target != target => {
                Tween { property: property.clone(), from: displayed, to: target, started: now, spec }
            }
            Some(running) if !running.done(now) => Tween { spec, ..running.clone() },
            _ => continue,
        };
        properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
        tweens.push(tween);
    }
    Ok(tweens)
}

/// Advances every tween in `tweens` to `now`, writing the displayed values into `properties` and
/// dropping the ones that have arrived.
pub fn advance(
    tweens: &mut Vec<Tween>,
    properties: &mut HashMap<String, Value>,
    now: Instant,
    lua: &Lua,
) -> Result<(), LayoutError> {
    for tween in tweens.iter() {
        // `retarget` wrote the key when it started the tween, so no insert and no key clone.
        *properties.get_mut(&tween.property).expect("a tween's property is in the map it was started from") =
            tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?;
    }
    tweens.retain(|tween| !tween.done(now));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(lua: &Lua, src: &str) -> HashMap<String, Value> {
        let table: mlua::Table = lua.load(src).eval().unwrap();
        table.pairs::<String, Value>().map(|p| p.unwrap()).collect()
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

    #[test]
    fn a_bare_number_is_a_duration_with_the_default_easing() {
        let lua = Lua::new();
        let specs = parse_animate(&props(&lua, "return { animate = { width = 200 } }")).unwrap();
        assert_eq!(specs["width"], AnimationSpec { duration: Duration::from_millis(200), easing: Easing::InOutQuad });
    }

    #[test]
    fn a_table_names_its_easing() {
        let lua = Lua::new();
        let specs = parse_animate(&props(
            &lua,
            r#"return { animate = { background = { duration = 150, easing = "OutCubic" } } }"#,
        ))
        .unwrap();
        assert_eq!(specs["background"].easing, Easing::OutCubic);
    }

    #[test]
    fn an_unknown_easing_is_refused_naming_the_known_ones() {
        let lua = Lua::new();
        let err =
            parse_animate(&props(&lua, r#"return { animate = { width = { duration = 1, easing = "Bouncy" } } }"#))
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("animate.width") && text.contains("Bouncy") && text.contains("OutBack"), "{text}");
    }

    #[test]
    fn a_property_no_tween_carries_is_refused() {
        let lua = Lua::new();
        let err = parse_animate(&props(&lua, "return { animate = { visible = 200 } }")).unwrap_err();
        assert!(err.to_string().contains("`visible`"), "{err}");
    }

    #[test]
    fn a_zero_duration_is_refused() {
        let lua = Lua::new();
        let err = parse_animate(&props(&lua, "return { animate = { width = 0 } }")).unwrap_err();
        assert!(err.to_string().contains("(0, 60000]"), "{err}");
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
    fn a_tween_reads_from_at_its_start_and_to_at_its_end() {
        let started = Instant::now();
        let tween = Tween {
            property: "width".into(),
            from: Animatable::Number(40.0),
            to: Animatable::Number(90.0),
            started,
            spec: AnimationSpec { duration: Duration::from_millis(100), easing: Easing::Linear },
        };
        assert_eq!(tween.at(started), Animatable::Number(40.0));
        assert_eq!(tween.at(started + Duration::from_millis(50)), Animatable::Number(65.0));
        assert_eq!(tween.at(started + Duration::from_millis(500)), Animatable::Number(90.0));
        assert!(tween.done(started + Duration::from_millis(100)));
    }
}
