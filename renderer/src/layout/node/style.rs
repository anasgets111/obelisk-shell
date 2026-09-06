//! Box-model and paint-adjacent parsers. `table_number` is shared by `toplevel`'s size hints,
//! popup offsets, and anchor rectangles.

use std::collections::HashMap;

use cursor_icon::CursorIcon;
use mlua::Value;

use super::*;

/// `"NN%"` (`^\d+(\.\d+)?%$`) as `SizeMode::Percent`. Not a confirmed spec syntax: § 5.1's base
/// property table only documents integer/`"Fill"` for width/height, though § 3.1 names
/// `Percent(f32)` as a size class with no literal Lua form given. See ADR-0023.
fn parse_percent(s: &str) -> Option<f32> {
    let digits = s.strip_suffix('%')?;
    let mut parts = digits.splitn(2, '.');
    let int_part = parts.next()?;
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(frac_part) = parts.next()
        && (frac_part.is_empty() || !frac_part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    digits.parse::<f32>().ok().map(|n| n / 100.0)
}

/// Numeric sizes use § 5.1's `[0, 8192]` range (ADR-0021). `properties` is already a
/// [`resolve_properties`] result, so an absent key covers both omission and a signal resolving to
/// `nil`.
pub fn parse_size_mode(properties: &HashMap<String, Value>, property: &str) -> Result<SizeMode, LayoutError> {
    // Deferred on the evaluation pass: § 6's `width`/`height` are live layer-shell `set_size`
    // fields (ADR-0038 decision 2), so `App::apply_spec_change` re-derives them each pass.
    if is_deferred_signal(properties, property) {
        return Ok(SizeMode::Content);
    }
    let Some(value) = properties.get(property) else {
        return Ok(SizeMode::Content);
    };
    if let Some(n) = value_as_f32(property, value)? {
        if !(0.0..=8192.0).contains(&n) {
            return Err(invalid(property, format!("must be within [0, 8192], got {n}")));
        }
        return Ok(SizeMode::Pixels(n));
    }
    if let Value::String(s) = value {
        let s = checked_string(property, s)?;
        if s == "Fill" {
            return Ok(SizeMode::Fill);
        }
        if let Some(pct) = parse_percent(&s) {
            return Ok(SizeMode::Percent(pct));
        }
    }
    Err(invalid(
        property,
        format!(
            "expected a number, \"Fill\", or a \"NN%\" string (Content sizing has no literal -- omit the property instead), got {}",
            preview_for_error(value)
        ),
    ))
}

/// `max_width`/`max_height` cap a `Content`-sized node in pixels, leaving overflow for `scroll`.
/// Percent and `"Fill"` ceilings add no meaning beyond a fixed size.
pub fn parse_max_size(properties: &HashMap<String, Value>, property: &str) -> Result<Option<f32>, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(None);
    };
    match value_as_f32(property, value)? {
        Some(n) if (0.0..=8192.0).contains(&n) => Ok(Some(n)),
        Some(n) => Err(invalid(property, format!("must be within [0, 8192], got {n}"))),
        None => Err(invalid(property, format!("expected a number of pixels, got {}", preview_for_error(value)))),
    }
}

/// Reads one numeric field from a table-valued property. `None` means absent; callers choose
/// whether that defaults to 0 or is required. Nested `Signal`s are refused and errors name
/// `{property}.{key}`.
pub(super) fn table_number(property: &str, table: &mlua::Table, key: &str) -> Result<Option<f32>, LayoutError> {
    let v: Value = table.get(key).map_err(|e| invalid(property, e.to_string()))?;
    match v {
        Value::Nil => Ok(None),
        Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!("{property}.{key}"))),
        other => value_as_f32(property, &other)?
            .ok_or_else(|| invalid(property, format!("`{key}` must be a number, got {}", preview_for_error(&other))))
            .map(Some),
    }
}

/// `table.get` is metamethod-aware. This used to run once per consumer, producing 16 `__index`
/// calls for one child's margin, a row measured 18 wide, and its 10-wide child at 16..26. Parsing
/// each geometry property once per node/pass fixes that. The reads also escaped ADR-0021's 5ms
/// getter budget: `CpuBudget` hooked `Signal::get_value` and dropped the hook on return, covering
/// only a signal getter's body; a `__index` loop of 200 million iterations made `Scene::apply` take
/// 26.10s and
/// return `Ok(())` with no `Signal`, on the VM's thread (ADR-0039). `LayoutPassBudget` now holds
/// the hook for the whole pass and refuses it in 2s with `PassBudgetExceeded`. Scalar shorthand is
/// shared by `margin`/`padding`/`border_width`; only the last keeps a range check.
pub fn parse_edge_insets(properties: &HashMap<String, Value>, property: &str) -> Result<EdgeInsets, LayoutError> {
    // Deferred on the evaluation pass: a panel root's `margin` is the live layer-shell anchor
    // offset (`set_margin`, ADR-0038 decision 2), so zero is the absent-key placeholder.
    if is_deferred_signal(properties, property) {
        return Ok(EdgeInsets::default());
    }
    let Some(value) = properties.get(property) else {
        return Ok(EdgeInsets::default());
    };
    if let Some(n) = value_as_f32(property, value)? {
        return Ok(EdgeInsets { top: n, right: n, bottom: n, left: n });
    }
    let Value::Table(table) = value else {
        return Err(invalid(property, format!("expected a number or a table, got {}", preview_for_error(value))));
    };
    // An absent edge is 0; [`table_number`] rejects nested `Signal`s.
    let edge = |key: &str| -> Result<f32, LayoutError> { Ok(table_number(property, table, key)?.unwrap_or(0.0)) };
    Ok(EdgeInsets { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? })
}

/// `rect.background` (§ 5.2 item 1). Absent is `None`, not transparent black: `fill_rect` skips
/// it, while `#RRGGBBAA` with `AA = 00` remains an explicit transparent fill.
pub fn parse_background(properties: &HashMap<String, Value>) -> Result<Option<Rgba>, LayoutError> {
    let Some(value) = properties.get("background") else {
        return Ok(None);
    };
    let Value::String(s) = value else {
        return Err(invalid("background", format!("expected a string, got {}", preview_for_error(value))));
    };
    let s = checked_string("background", s)?;
    Ok(Some(parse_hex_color("background", &s)?))
}

/// `radius` and `border_width` share `[0, 8192]` with `width`/`height` (§ 5.1). In femtovg 0.26,
/// `radius = -4` silently squares corners (`path.rs:458` treats values under 0.1 as unrounded),
/// while `border_width = -4` clamps to 0 and clears paint alpha. Above roughly 8.4e6,
/// `curve_divisions` (`path/cache.rs:911`) divides by
/// `acos(1.0) == 0.0`; `inf as u32` becomes `u32::MAX`, causing billions of iterations and tens
/// of GB of vertices on the Wayland dispatch thread. `margin`/`padding` stay unrestricted because
/// the solver treats negative margin as CSS layout math, not a femtovg stroke input.
fn check_geometry_range(property: &str, n: f32) -> Result<(), LayoutError> {
    if !(0.0..=8192.0).contains(&n) {
        return Err(invalid(property, format!("must be within [0, 8192], got {n}")));
    }
    Ok(())
}

/// `rect.radius` (§ 5.2 item 1), defaulting to 0.
pub fn parse_radius(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("radius") else {
        return Ok(0.0);
    };
    let n = value_as_f32("radius", value)?
        .ok_or_else(|| invalid("radius", format!("expected a number, got {}", preview_for_error(value))))?;
    check_geometry_range("radius", n)?;
    Ok(n)
}

/// Whether a node clips children to its box or lets `radius` shape the clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipShape {
    /// The node's rectangle with square corners.
    #[default]
    Box,
    /// The node's rounded shape, using the same arc as its background fill.
    Rounded,
}

/// `rect.clip` (§ 5.2 item 1) defaults to [`ClipShape::Box`] and is opt-in because rounded clipping
/// needs an offscreen target and composite, while a square clip is a free GPU scissor. QML's
/// `Item.clip` likewise ignores `radius`; Quickshell's `ClippingRectangle` spends two targets.
pub fn parse_clip(properties: &HashMap<String, Value>) -> Result<ClipShape, LayoutError> {
    let Some(value) = properties.get("clip") else {
        return Ok(ClipShape::Box);
    };
    let Value::String(s) = value else {
        return Err(invalid("clip", format!("must be a string, got {}", preview_for_error(value))));
    };
    match checked_string("clip", s)?.as_str() {
        "Box" => Ok(ClipShape::Box),
        "Rounded" => Ok(ClipShape::Rounded),
        other => Err(invalid("clip", format!("must be \"Box\" or \"Rounded\", got {other:?}"))),
    }
}

/// `rect.border_color` (§ 5.2 item 1), one colour per edge. `None` means "not painted", the same
/// absence [`parse_background`] returns for a missing fill: an edge at width 0 needs no colour,
/// and one with a colour at width 0 still paints nothing, so the drawing pass gets the same answer
/// either way. § 5.2 gives the table form no per-edge default, so an absent edge takes `None`
/// rather than an invented one.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BorderColor {
    pub top: Option<Rgba>,
    pub right: Option<Rgba>,
    pub bottom: Option<Rgba>,
    pub left: Option<Rgba>,
}

pub fn parse_border_color(properties: &HashMap<String, Value>) -> Result<BorderColor, LayoutError> {
    let Some(value) = properties.get("border_color") else {
        return Ok(BorderColor::default());
    };
    if let Value::String(s) = value {
        let s = checked_string("border_color", s)?;
        let color = Some(parse_hex_color("border_color", &s)?);
        return Ok(BorderColor { top: color, right: color, bottom: color, left: color });
    }
    let Value::Table(table) = value else {
        return Err(invalid("border_color", format!("expected a string or a table, got {}", preview_for_error(value))));
    };
    // Metamethod-aware, but parsed once per node by `paint_style` (ADR-0068).
    let edge = |key: &str| -> Result<Option<Rgba>, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("border_color", e.to_string()))?;
        // Name the edge as well as the property; the shared string/color parsers only know the
        // property.
        let name_edge = |e: LayoutError| match e {
            LayoutError::InvalidProperty { property, detail } => {
                LayoutError::InvalidProperty { property, detail: format!("`{key}`: {detail}") }
            }
            other => other,
        };
        match v {
            Value::Nil => Ok(None),
            // Reject nested signals rather than misreporting them as bad hex.
            Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!("border_color.{key}"))),
            Value::String(s) => {
                let s = checked_string("border_color", &s).map_err(name_edge)?;
                Ok(Some(parse_hex_color("border_color", &s).map_err(name_edge)?))
            }
            other => Err(invalid(
                "border_color",
                format!("`{key}` must be a hex colour string, got {}", preview_for_error(&other)),
            )),
        }
    };
    Ok(BorderColor { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? })
}

/// `rect.border_width` (§ 5.2 item 1), using [`EdgeInsets`] and adding its `[0, 8192]` range check
/// after [`parse_edge_insets`]. `margin`/`padding` deliberately do not use that check.
pub fn parse_border_width(properties: &HashMap<String, Value>) -> Result<EdgeInsets, LayoutError> {
    let insets = parse_edge_insets(properties, "border_width")?;
    for n in [insets.top, insets.right, insets.bottom, insets.left] {
        check_geometry_range("border_width", n)?;
    }
    Ok(insets)
}

pub fn parse_align(properties: &HashMap<String, Value>, property: &str) -> Result<Align, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(Align::Start);
    };
    let Value::String(s) = value else {
        return Err(invalid(property, format!("expected a string, got {}", preview_for_error(value))));
    };
    match checked_string(property, s)?.as_str() {
        "Start" => Ok(Align::Start),
        "Center" => Ok(Align::Center),
        "End" => Ok(Align::End),
        "Stretch" => Ok(Align::Stretch),
        other => Err(invalid(property, format!("unknown alignment `{other}`"))),
    }
}

/// `list.direction` (§ 5.2 item 7), defaulting to `"Vertical"`; returns the borrowed `row` or
/// `column` kind rather than adding a third layout arm.
pub fn parse_list_direction(properties: &HashMap<String, Value>) -> Result<&'static str, LayoutError> {
    let Some(value) = properties.get("direction") else {
        return Ok("column");
    };
    let Value::String(s) = value else {
        return Err(invalid("direction", format!("expected a string, got {}", preview_for_error(value))));
    };
    match checked_string("direction", s)?.as_str() {
        "Vertical" => Ok("column"),
        "Horizontal" => Ok("row"),
        other => Err(invalid("direction", format!("unknown direction `{other}`, expected `Vertical` or `Horizontal`"))),
    }
}

/// `opacity` (`oblisk-idl-api-specs.md` § 5.1) belongs to every kind, including non-painting lists,
/// and is inherited by multiplication on `ResolvedNode`. It does not replace `visible`: a fully
/// transparent node still lays out, occupies space, and hit-tests. Values outside `[0, 1]` error
/// rather than clamp (ADR-0068), matching the reference config's use of this property in 32 files.
pub fn parse_opacity(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("opacity") else {
        return Ok(1.0);
    };
    let Some(n) = value_as_f32("opacity", value)? else {
        return Err(invalid("opacity", format!("must be a number, got {}", preview_for_error(value))));
    };
    if !(0.0..=1.0).contains(&n) {
        return Err(invalid("opacity", format!("must be within [0, 1], got {n}")));
    }
    Ok(n)
}

pub fn parse_visible(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    let Some(value) = properties.get("visible") else {
        return Ok(true);
    };
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(invalid("visible", format!("expected a boolean, got {}", preview_for_error(other)))),
    }
}

/// § 5.1 `cursor`: CSS names such as `"pointer"`, `"text"`, `"grab"`, and resize edges, or `None`
/// for the default rule (ADR-0107; `layout::hit::cursor_under`). `cursor_icon` and
/// `wp_cursor_shape_v1` use the same names, so the compositor reads the config string directly.
pub fn parse_cursor(properties: &HashMap<String, Value>) -> Result<Option<CursorIcon>, LayoutError> {
    let Some(value) = properties.get("cursor") else {
        return Ok(None);
    };
    let Value::String(name) = value else {
        return Err(invalid("cursor", format!("must be a cursor name string, got {}", preview_for_error(value))));
    };
    let name = name.to_str().map_err(|_| invalid("cursor", "must be UTF-8"))?;
    name.parse::<CursorIcon>()
        .map(Some)
        .map_err(|_| invalid("cursor", format!("unknown cursor name {name:?}; the names are CSS's, like \"pointer\"")))
}

pub fn parse_spacing(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("spacing") else {
        return Ok(0.0);
    };
    value_as_f32("spacing", value)?
        .ok_or_else(|| invalid("spacing", format!("expected a number, got {}", preview_for_error(value))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::nodes::deserialize_lua_table;

    fn lua() -> mlua::Lua {
        mlua::Lua::new()
    }

    fn props_from_table(table: &mlua::Table) -> HashMap<String, Value> {
        deserialize_lua_table(table).unwrap().properties
    }

    #[test]
    fn width_absent_is_content() {
        let props = HashMap::new();
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Content);
    }

    #[test]
    fn width_integer_is_pixels() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = 32 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Pixels(32.0));
    }

    #[test]
    fn width_fill_string_is_fill() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = "Fill" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Fill);
    }

    #[test]
    fn width_percent_string_divides_by_100() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = "50%" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Percent(0.5));
    }

    #[test]
    fn width_above_the_8192_ceiling_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = 8193 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(parse_size_mode(&props, "width").unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn a_negative_width_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = -5 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(parse_size_mode(&props, "width").unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn width_garbage_string_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = "banana" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(parse_size_mode(&props, "width").unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn height_content_error_names_omission_as_the_spelling() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", height = "Content" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_size_mode(&props, "height").unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "height" && detail.contains("omit the property")),
            "must name omission as how Content sizing is spelled: {err}"
        );
    }

    #[test]
    fn margin_reads_named_edges_defaulting_absent_ones_to_zero() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", margin = { top = 4, left = 2 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let insets = parse_edge_insets(&props, "margin").unwrap();
        assert_eq!(insets, EdgeInsets { top: 4.0, right: 0.0, bottom: 0.0, left: 2.0 });
    }

    #[test]
    fn padding_reads_named_edges_defaulting_absent_ones_to_zero() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", padding = { top = 4, left = 2 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let insets = parse_edge_insets(&props, "padding").unwrap();
        assert_eq!(insets, EdgeInsets { top: 4.0, right: 0.0, bottom: 0.0, left: 2.0 });
    }

    #[test]
    fn margin_scalar_broadcasts_to_all_four_edges() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", margin = 10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "margin").unwrap(),
            EdgeInsets { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 }
        );
    }

    #[test]
    fn padding_scalar_broadcasts_to_all_four_edges() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", padding = 10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "padding").unwrap(),
            EdgeInsets { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 }
        );
    }

    #[test]
    fn margin_negative_value_is_accepted() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", margin = -10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "margin").unwrap(),
            EdgeInsets { top: -10.0, right: -10.0, bottom: -10.0, left: -10.0 }
        );
    }

    #[test]
    fn padding_negative_value_is_accepted() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", padding = -10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "padding").unwrap(),
            EdgeInsets { top: -10.0, right: -10.0, bottom: -10.0, left: -10.0 }
        );
    }

    #[test]
    fn align_h_parses_all_four_variants() {
        for (text, expected) in
            [("Start", Align::Start), ("Center", Align::Center), ("End", Align::End), ("Stretch", Align::Stretch)]
        {
            let lua = lua();
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "rect", align_h = "{text}" }}"#)).eval().unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_align(&props, "align_h").unwrap(), expected);
        }
    }

    #[test]
    fn visible_absent_defaults_true() {
        let props = HashMap::new();
        assert!(parse_visible(&props).unwrap());
    }

    #[test]
    fn a_signal_userdata_in_a_geometry_slot_resolves_to_its_current_value() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal =
            crate::lua::signal::Signal::new_live(Value::Boolean(false), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("visible", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let resolved = resolve_properties(&node.properties, "rect", &lua).unwrap();
        assert!(!parse_visible(&resolved).unwrap(), "must read the signal's current value, not error on the handle");
    }

    #[test]
    fn spacing_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "row", spacing = 1e300 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_spacing(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "spacing"
        ));
    }

    #[test]
    fn background_absent_is_none() {
        let props = HashMap::new();
        assert_eq!(parse_background(&props).unwrap(), None);
    }

    #[test]
    fn background_six_digit_hex_is_opaque() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#336699" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_background(&props).unwrap(),
            Some(Rgba { r: 0x33 as f32 / 255.0, g: 0x66 as f32 / 255.0, b: 0x99 as f32 / 255.0, a: 1.0 })
        );
    }

    #[test]
    fn background_eight_digit_hex_carries_its_own_alpha() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#33669980" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_background(&props).unwrap(),
            Some(Rgba {
                r: 0x33 as f32 / 255.0,
                g: 0x66 as f32 / 255.0,
                b: 0x99 as f32 / 255.0,
                a: 0x80 as f32 / 255.0,
            })
        );
    }

    #[test]
    fn background_without_a_leading_hash_is_rejected_naming_the_property() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", background = "336699" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("must start with `#`")),
            "must name the missing `#`, not just some invalid-property error: {err}"
        );
    }

    #[test]
    fn background_with_the_wrong_digit_count_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#369" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 3")),
            "must be the digit-count rule specifically, naming 3 digits: {err}"
        );
    }

    #[test]
    fn background_with_non_hex_characters_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#zzzzzz" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("only hex digits")),
            "must be the hex-digit rule specifically, not the digit-count rule: {err}"
        );
    }

    #[test]
    fn a_non_ascii_colour_string_gets_the_hex_digit_diagnosis_not_a_byte_count() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#日本語" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("only hex digits") && !detail.contains("got 9")),
            "non-ASCII input must get the hex-digit diagnosis, not a byte-length count: {err}"
        );
    }

    #[test]
    fn background_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", background = {} }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("expected a string")),
            "{err}"
        );
    }

    #[test]
    fn uppercase_hex_parses_the_same_as_lowercase() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#FF0000" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_background(&props).unwrap(), Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }));
    }

    #[test]
    fn a_seven_digit_hex_is_rejected_naming_the_digit_count() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#1234567" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 7")),
            "{err}"
        );
    }

    #[test]
    fn a_bare_hash_is_rejected_for_wrong_digit_count() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 0")),
            "{err}"
        );
    }

    #[test]
    fn clip_absent_defaults_to_the_nodes_box() {
        let props = HashMap::new();
        assert_eq!(parse_clip(&props).unwrap(), ClipShape::Box);
    }

    #[test]
    fn clip_reads_both_shapes() {
        for (declared, expected) in [("Box", ClipShape::Box), ("Rounded", ClipShape::Rounded)] {
            let lua = mlua::Lua::new();
            let src = format!(r#"return {{ kind = "rect", clip = "{declared}" }}"#);
            let table: mlua::Table = lua.load(&src).eval().unwrap();
            let props = deserialize_lua_table(&table).unwrap().properties;
            assert_eq!(parse_clip(&props).unwrap(), expected, "clip = {declared:?}");
        }
    }

    /// The whole point of a named shape over a boolean: `clip = true` would have to mean something,
    /// and the two shapes are not on/off, a node clips either way.
    #[test]
    fn an_unknown_clip_shape_is_rejected_naming_both() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", clip = "Circle" }"#).eval().unwrap();
        let props = deserialize_lua_table(&table).unwrap().properties;
        let err = parse_clip(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "clip" && detail.contains("\"Box\" or \"Rounded\"")),
            "got {err:?}"
        );

        let table: mlua::Table = lua.load(r#"return { kind = "rect", clip = true }"#).eval().unwrap();
        let props = deserialize_lua_table(&table).unwrap().properties;
        assert!(
            matches!(parse_clip(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "clip")
        );
    }

    #[test]
    fn radius_absent_defaults_to_zero() {
        let props = HashMap::new();
        assert_eq!(parse_radius(&props).unwrap(), 0.0);
    }

    #[test]
    fn radius_reads_the_number() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = 6 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_radius(&props).unwrap(), 6.0);
    }

    #[test]
    fn radius_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("expected a number")),
            "{err}"
        );
    }

    #[test]
    fn a_negative_radius_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = -4 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn radius_above_8192_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = 8193 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_absent_defaults_to_all_zero() {
        let props = HashMap::new();
        assert_eq!(parse_border_width(&props).unwrap(), EdgeInsets::default());
    }

    #[test]
    fn border_width_scalar_broadcasts_to_all_four_edges() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = 3 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_border_width(&props).unwrap(), EdgeInsets { top: 3.0, right: 3.0, bottom: 3.0, left: 3.0 });
    }

    #[test]
    fn border_width_table_sets_edges_independently() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", border_width = { top = 2, left = 5 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_border_width(&props).unwrap(), EdgeInsets { top: 2.0, right: 0.0, bottom: 0.0, left: 5.0 });
    }

    #[test]
    fn border_width_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("expected a number or a table")),
            "{err}"
        );
    }

    #[test]
    fn a_negative_border_width_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = -4 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_above_8192_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = 8193 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_table_form_out_of_range_edge_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = { top = 8193 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn a_signal_nested_in_a_margin_edge_table_is_rejected_naming_the_edge() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Integer(4), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        let margin = lua.create_table().unwrap();
        margin.set("top", signal).unwrap();
        table.set("margin", margin).unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_edge_insets(&props, "margin").unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "margin.top"
        ));
    }

    #[test]
    fn border_color_absent_is_all_none() {
        let props = HashMap::new();
        assert_eq!(parse_border_color(&props).unwrap(), BorderColor::default());
    }

    #[test]
    fn border_color_scalar_string_broadcasts_to_all_four_edges() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", border_color = "#ff0000" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let red = Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 });
        assert_eq!(parse_border_color(&props).unwrap(), BorderColor { top: red, right: red, bottom: red, left: red });
    }

    #[test]
    fn border_color_table_sets_edges_independently_leaving_absent_edges_none() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", border_color = { top = "#ff0000", left = "#00ff00" } }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_border_color(&props).unwrap(),
            BorderColor {
                top: Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
                right: None,
                bottom: None,
                left: Some(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            }
        );
    }

    #[test]
    fn border_color_malformed_hex_in_a_table_is_rejected_naming_the_edge() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", border_color = { top = "not-a-color" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_border_color(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("top") && detail.contains("must start with `#`")),
            "must name the failing edge, not just `border_color`: {err}"
        );
    }

    #[test]
    fn a_malformed_hex_on_a_non_top_edge_names_that_edge() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", border_color = { right = "not-a-color" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_border_color(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("right")),
            "must name `right`, the edge that actually failed: {err}"
        );
    }

    #[test]
    fn a_signal_nested_in_a_border_color_edge_table_is_rejected_naming_the_edge() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let hex = lua.create_string("#ff0000").unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(hex), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        let border_color = lua.create_table().unwrap();
        border_color.set("top", signal).unwrap();
        table.set("border_color", border_color).unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_border_color(&props).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "border_color.top"
        ));
    }

    #[test]
    fn border_color_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_color = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_border_color(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("expected a string or a table")),
            "{err}"
        );
    }
}
