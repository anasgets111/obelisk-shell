//! The box-model and paint-adjacent parsers: size, margin/padding, alignment, visibility,
//! spacing, background, radius, and border color/width. Every parser here reads `&HashMap<String,
//! Value>` and returns a typed value or a [`LayoutError`] naming the offending property.
//!
//! `table_number` is `pub(super)`: `toplevel`'s size-hint and popup-offset/anchor-rect parsers
//! reuse the same one-field-out-of-a-table helper this module defines.

use std::collections::HashMap;

use mlua::Value;

use super::*;

/// `"NN%"` (`^\d+(\.\d+)?%$`) as `SizeMode::Percent`. Not a confirmed spec syntax -- § 5.1's base
/// property table only documents integer/`"Fill"` for width/height even though § 3.1 names
/// `Percent(f32)` as a size class without giving it a literal Lua form. See docs/adr/0023.
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

/// An explicit pixel value is range-checked against § 5.1's base property table
/// (`[0, 8192]`) -- ADR-0021 item 5 named this phase's layout engine as the "actual
/// consumer that needs typed, validated properties" that range check waits on.
///
/// `properties` is a [`resolve_properties`] result, here and in every parser below: an absent key
/// covers both a property the config omitted and one whose signal read `nil`, which is why none of
/// them takes a `&Lua` or handles a `Value::Nil` of its own.
pub fn parse_size_mode(properties: &HashMap<String, Value>, property: &str) -> Result<SizeMode, LayoutError> {
    // Deferred on the evaluation-time pass ([`is_deferred_signal`]): § 6.1's `width`/`height` are a
    // layer-shell `set_size`, which docs/adr/0038 decision 2 lists among the requests that are valid
    // on a live surface, so `crate::wayland::App::apply_spec_change` re-derives both from the
    // resolved tree on every pass. Same placeholder an absent property gets. The guard cannot fire
    // below a surface root, where this parser is also used: `resolve_properties` has already
    // replaced every `Signal` in an inner node's map.
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

/// One numeric field out of a table-valued property (`margin.top`, `anchor_rect.width`,
/// `offset.x`, `min_size.height`). `Ok(None)` means the key is absent, which every caller answers
/// differently: [`parse_edge_insets`] defaults an edge to 0, `toplevel::parse_anchor_rect` defaults an
/// origin to 0 but refuses an absent extent, and `toplevel::parse_size_hint` refuses either axis.
///
/// A `Signal` only resolves at the top level of the property map ([`resolve_properties`] never
/// looks inside a table value), so one surviving into a nested slot is refused outright rather than
/// misreported as "must be a number, got AnyUserData(Ref(0x...))" -- an opaque pointer and a wrong
/// claim about the type. `UnsupportedSignalProperty` already carries the right advice (read it via
/// `:get()` first); `{property}.{key}` names both the property and which field.
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

/// The four `table.get` calls below are metamethod-aware, so a resolved table carrying a
/// side-effecting `__index` answers per read rather than per node. That used to matter: this
/// parser ran once per *consumer* of the property, so a child's `margin` was parsed by its
/// parent's child loop, by both `intrinsic_content_size` folds and by `position_children`, four
/// answers to one question with nothing making them agree. Measured then: 16 `__index`
/// invocations for one child in one pass, and a row that measured itself 18 wide and placed its
/// 10-wide child spanning 16..26.
///
/// Closed by build-steps.md Phase 19 item 5's second half. `layout::scene`'s `LayoutStyle` parses
/// every geometry property once per node per pass, in the parent's child loop, and the sizing and
/// positioning passes read that struct. This function still runs the metamethod, once, which is
/// what any parser reading a Lua table has to do; what changed is that nothing calls it twice for
/// the same node.
///
/// Those reads also used to run entirely outside ADR-0021's 5ms cap, and so did
/// `surface::parse_anchor`'s: `CpuBudget` installed its Lua hook inside `Signal::get_value` and
/// dropped it on return, so the only Lua a budget ever covered was a signal getter's own body.
/// A `margin` table whose `__index` spins 200 million iterations made one `Scene::apply` take
/// 26.10 seconds and return `Ok(())`, reachable from a config using no `Signal` at all, on the
/// thread that also answers `configure` and runs the VM (docs/adr/0039). `lua::signal`'s
/// `LayoutPassBudget` now holds the hook for the whole pass, so the same config is refused in
/// 2 seconds with a `LayoutError::PassBudgetExceeded`.
///
/// Scalar shorthand -- a bare number broadcasts to all four edges -- shared by `margin`, `padding`
/// and `border_width` (docs/build-steps.md Phase 19 item 15). Carries no range check of its own:
/// see [`check_geometry_range`]'s doc comment for why `border_width` keeps a bound this function
/// does not apply to `margin`/`padding`.
pub fn parse_edge_insets(properties: &HashMap<String, Value>, property: &str) -> Result<EdgeInsets, LayoutError> {
    // Deferred on the evaluation-time pass, for [`parse_size_mode`]'s reason: on a `panel` root
    // `margin` is the layer-shell anchor offset, which `set_margin` changes on a live surface
    // (docs/adr/0038 decision 2). Zero insets are the placeholder an absent `margin` already takes,
    // and the same "cannot fire below a root" note applies.
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
    // An absent edge is 0, which is [`table_number`]'s `None` -- see that function for the nested
    // `Signal` rejection every table-valued property shares.
    let edge = |key: &str| -> Result<f32, LayoutError> { Ok(table_number(property, table, key)?.unwrap_or(0.0)) };
    Ok(EdgeInsets { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? })
}

/// `rect.background` (§ 5.2 item 1). Absent is `None`, not transparent black -- `layout::paint`'s
/// `fill_rect` has to be able to skip the fill entirely rather than paint an invisible one, and
/// `#RRGGBBAA` with `AA = 00` already covers "explicitly transparent" as a distinct config choice.
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

/// Shared `[0, 8192]` bound for `radius` and `border_width` -- the same range [`parse_size_mode`]
/// already enforces for `width`/`height` (§ 5.1's base property table). Traced in femtovg 0.26:
/// `radius = -4` silently draws square corners (`path.rs:458` treats anything under 0.1 as
/// unrounded) and `border_width = -4` clamps to 0.0 and multiplies paint alpha by zero, so both
/// negative ends fail silently rather than raising. The upper end matters most: above roughly
/// 8.4e6, `curve_divisions` (`path/cache.rs:911`) computes `acos(1.0) == 0.0`, divides by it, and
/// `inf as u32` saturates to `u32::MAX` as a stroke-loop bound in `round_join`/`round_cap_start` --
/// billions of iterations and tens of gigabytes of vertices on the Wayland dispatch thread.
///
/// This bound stays private to `radius` and `border_width`, not extended to `margin`/`padding`:
/// § 5.1 gives `margin`/`padding` no "Valid Range" entry at all, unlike `width`/`height`, and
/// `layout::scene`'s `position_children` reads a negative margin the same way CSS does -- subtracted
/// into a child's footprint and slot size, so `margin = -8` deliberately pulls a child closer to (or
/// over) its neighbor. That is layout math, not a femtovg stroke input, with no crash mode like
/// `border_width`'s curve-divisions blowup.
fn check_geometry_range(property: &str, n: f32) -> Result<(), LayoutError> {
    if !(0.0..=8192.0).contains(&n) {
        return Err(invalid(property, format!("must be within [0, 8192], got {n}")));
    }
    Ok(())
}

/// `rect.radius` (§ 5.2 item 1). Absent defaults to 0, an unrounded rectangle -- same shape as
/// [`parse_spacing`]/[`parse_font_size`] for an unranged numeric property with no documented
/// default of its own beyond "no effect when omitted".
pub fn parse_radius(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("radius") else {
        return Ok(0.0);
    };
    let n = value_as_f32("radius", value)?
        .ok_or_else(|| invalid("radius", format!("expected a number, got {}", preview_for_error(value))))?;
    check_geometry_range("radius", n)?;
    Ok(n)
}

/// `rect.border_color` (§ 5.2 item 1), one colour per edge. `None` on an edge means "not painted",
/// the same absence [`parse_background`] returns for a missing fill and the same zero
/// [`parse_border_width`] defaults an edge to -- an edge with width 0 needs no colour, and an edge
/// with a colour but width 0 still paints nothing, so the drawing pass can read either field first
/// and get the same answer. § 5.2 gives the table form no per-edge default colour to fall back to,
/// so an absent edge takes `None` rather than an invented default.
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
    // `table.get` is metamethod-aware here too. Unlike margin/padding this was never read twice --
    // docs/adr/0068 already had `paint_style` parse it once per node -- so all it needed was a
    // budget to run under, which `lua::signal`'s `LayoutPassBudget` now provides for the whole
    // pass. See [`parse_edge_insets`] for the measurements.
    let edge = |key: &str| -> Result<Option<Rgba>, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("border_color", e.to_string()))?;
        // Every error this closure raises names the edge, `key`, not just the property --
        // see the `Value::UserData` arm below and `name_edge` for why the String arm needs help
        // to do that too, since `checked_string`/`parse_hex_color` only know the property.
        let name_edge = |e: LayoutError| match e {
            LayoutError::InvalidProperty { property, detail } => {
                LayoutError::InvalidProperty { property, detail: format!("`{key}`: {detail}") }
            }
            other => other,
        };
        match v {
            Value::Nil => Ok(None),
            // Same hole as `parse_edge_insets`'s `edge` closure, same fix: a Signal only resolves
            // at the top level of the property map, so one nested here is refused outright rather
            // than falling into the `other` arm and being misreported as a bad hex string.
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

/// `rect.border_width` (§ 5.2 item 1), reusing [`EdgeInsets`] rather than a new per-edge type since
/// the shape (four `f32`, default 0) is already exactly that. Both the scalar and table forms
/// (docs/build-steps.md Phase 19 item 15) come straight from [`parse_edge_insets`], which also
/// defaults an absent edge to 0; the one thing this wrapper still adds is the `[0, 8192]` range
/// check, run on every edge of the result. That check stays here rather than moving into
/// `parse_edge_insets` itself -- see [`check_geometry_range`]'s doc comment for why `margin`/
/// `padding` don't get it.
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

/// `list.direction` (§ 5.2 item 7): `"Vertical"` (the default) or `"Horizontal"`.
///
/// Returns the kind whose layout a `list` borrows, because that is all the property does --
/// `layout::scene` has one `row` arm and one `column` arm, and a `list` is routed to whichever the
/// direction names rather than growing a third. `"Vertical"` is the default because it was the only
/// behaviour before this existed, so no config that predates it changes shape.
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

/// `opacity` (`oblisk-idl-api-specs.md` § 5.1): how much of this node and everything under it
/// reaches the screen, 0 for invisible and 1 for solid. Absent defaults to 1.
///
/// § 5.1 and not § 5.2, because this belongs to every kind rather than to the ones that draw a box.
/// A `list` paints nothing itself and still has to fade what is inside it, which is also why the
/// value lives on `ResolvedNode` rather than inside `PaintStyle`.
///
/// **Inherited, and multiplied.** The value parsed here is one node's own contribution;
/// `layout::paint::build_node` multiplies it into whatever its ancestors already applied, the same
/// way it intersects a clip rather than replacing one. That is what makes fading a whole panel one
/// property instead of a walk over its children.
///
/// **Not `visible = false`.** A fully transparent node still lays out, still occupies space in its
/// parent's flow, and still hit-tests, because `layout::hit` gates descent on `visible` alone. That
/// is what lets a fade run without the layout jumping under it, and it matches what the reference
/// config expects from the property it uses in 32 files.
///
/// Refused rather than clamped outside `[0, 1]`, matching every other paint property since
/// docs/adr/0068: a config that writes `opacity = 50` meaning percent should hear about it while
/// applying, not stare at an invisible panel.
pub fn parse_opacity(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("opacity") else {
        return Ok(1.0);
    };
    let Some(n) = value_as_f32("opacity", value)? else {
        return Err(invalid("opacity", format!("must be a number, got {}", preview_for_error(value))));
    };
    // `NaN` and the infinities never reach this: `value_as_f32` goes through `marshal::check_number`
    // first, which refuses a non-finite before any range test would have to decide what
    // `(0.0..=1.0).contains(&NaN)` ought to mean.
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
