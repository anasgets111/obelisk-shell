//! Typed property parsing for the layout engine (build-steps.md Phase 12,
//! `docs/oblisk-idl-api-specs.md` § 5.1). `renderer/src/lua/nodes.rs`'s `VirtualNode` deliberately
//! left every property as a raw `mlua::Value` -- this module is the "actual consumer that needs
//! typed, validated properties" that file's own doc comment named as Phase 12's job.
//!
//! Most parsers here resolve a `Value::UserData` holding a `Signal` (§ 1.2) instead of rejecting
//! it (build-steps.md Phase 19 item 1, ADR-0044 decision 1, `CONTEXT.md`'s Signal resolution
//! entry): `resolve_property` reads the signal's current value through
//! [`crate::lua::signal::Signal::get_value`] and the caller then applies its own type's usual
//! rules to that value, exactly as it would to a literal. This resolves exactly once -- if the
//! result is itself a `Signal` (a fresh `Value::UserData`), that's an error rather than a second
//! read. That guard only stops a signal resolving directly to another signal; it is not a
//! recursion bound, and it does nothing for a computed signal whose getter returns a fresh table
//! on every call (e.g. a `children` signal that builds new node tables each read), which still
//! recurses as deep as the getter wants to go, through `resolve_and_reconcile` and
//! `deserialize_lua_table`, until the process aborts on a stack overflow rather than returning a
//! `LayoutError`. A bounded depth cap is build-steps.md Phase 19 item 3's job, not this one's.
//!
//! [`SurfaceTopology`]'s four fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/
//! `parse_monitor`) are the one carve-out and keep rejecting a `Signal` outright -- see
//! [`reject_signal_in_topology_field`]'s doc comment for why.

use std::collections::HashMap;

use mlua::{Lua, Value};

use crate::lua::marshal;
use crate::lua::nodes::{VirtualNode, deserialize_lua_table};
use crate::lua::signal::Signal;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeMode {
    Pixels(f32),
    Percent(f32),
    Content,
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EdgeInsets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl EdgeInsets {
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("unsupported node kind `{0}`")]
    UnsupportedNodeKind(String),
    #[error("invalid value for `{property}`: {detail}")]
    InvalidProperty { property: String, detail: String },
    #[error(
        "`{0}` is a Signal handle, not a plain value -- read it via :get() before returning it from shell.lua"
    )]
    UnsupportedSignalProperty(String),
    /// build-steps.md Phase 19 item 3: `resolve_and_reconcile`'s recursion, bounded at
    /// `layout::scene::MAX_TREE_DEPTH`. Covers both a literal cyclic tree (`r.children = { r }`)
    /// and a computed `children` signal that generates fresh depth on every read -- both recurse
    /// through the same Rust call, so one cap catches both (see that constant's doc comment).
    ///
    /// `max` is the number of levels actually admitted and `depth` is the 1-based level that was
    /// refused, so `depth` is always `max + 1` -- the message states the limit the code enforces,
    /// not one adjacent to it.
    #[error(
        "node tree exceeds the maximum depth of {max} levels (at `{kind}`, level {depth}) -- a node holding itself in `children`?"
    )]
    TreeTooDeep { kind: String, depth: u32, max: u32 },
}

fn invalid(property: &str, detail: impl Into<String>) -> LayoutError {
    LayoutError::InvalidProperty {
        property: property.to_string(),
        detail: detail.into(),
    }
}

/// Runs a numeric `Value` through the marshalling boundary (`lua::marshal`, ADR-0044 decision 1)
/// before this parser's own application-level range checks (e.g. `parse_size_mode`'s `[0, 8192]`)
/// ever see it -- catches a NaN/Inf `f64` `Number` or an out-of-2^53-range `Integer`, whether it
/// arrived as a literal or came out of resolving a `Signal` via [`resolve_property`]: both are
/// equally Lua-authored values crossing into Rust, exactly what `marshal::check_number`/
/// `check_integer` were written to guard (`renderer/src/lua/marshal.rs`'s module doc comment).
///
/// `marshal::check_number` alone isn't sufficient here, though: it only guards the `f64`
/// representation, and a finite `f64` like `1e300` sails through it and then overflows to
/// `f32::INFINITY` on the narrowing cast below. A caller with no further range check (e.g.
/// `parse_spacing`) would otherwise hand that `Inf` straight into layout arithmetic -- `inf * 0.0`
/// is `NaN`, and `snap_to_physical`'s final `as i32` silently saturates a `NaN` rect to `0` instead
/// of ever raising an error. So the finiteness check re-runs after the cast, on the `f32`, naming
/// the same property a non-finite literal would.
fn value_as_f32(property: &str, value: &Value) -> Result<Option<f32>, LayoutError> {
    match value {
        Value::Integer(i) => {
            let checked = marshal::check_integer(*i).map_err(|e| invalid(property, e.to_string()))?;
            Ok(Some(checked as f32))
        }
        Value::Number(n) => {
            let checked = marshal::check_number(*n).map_err(|e| invalid(property, e.to_string()))?;
            let narrowed = checked as f32;
            if !narrowed.is_finite() {
                return Err(invalid(
                    property,
                    format!("must be finite, got {checked} which overflows f32 to {narrowed}"),
                ));
            }
            Ok(Some(narrowed))
        }
        _ => Ok(None),
    }
}

/// Runs a Lua string through `marshal::check_string`'s 64KB cap and returns the owned `String` --
/// same "literal and resolved-`Signal` values share one check" reasoning as [`value_as_f32`].
fn checked_string(property: &str, s: &mlua::LuaString) -> Result<String, LayoutError> {
    let s = s.to_string_lossy();
    marshal::check_string(&s).map_err(|e| invalid(property, e.to_string()))?;
    Ok(s)
}

/// Resolves `properties[property]` (build-steps.md Phase 19 item 1, ADR-0044 decision 1,
/// `CONTEXT.md`'s Signal resolution entry): a `Value::UserData` wrapping a `Signal` is read
/// through `Signal::get_value` and the *result* returned in its place, under the same rules the
/// caller would then apply to a literal; every other value passes through unchanged.
///
/// Resolves exactly once. If the result is itself a `Signal`, that's an error rather than a
/// second read. That only stops a signal resolving directly to another signal, not a getter that
/// recurses some other way (see this module's doc comment); a bounded recursion cap is
/// build-steps.md Phase 19 item 3's job, not this function's.
///
/// `None` means the property was absent; callers keep their own default-handling for that case,
/// same as every caller did before this function existed.
fn resolve_property(
    properties: &HashMap<String, Value>,
    property: &str,
    lua: &Lua,
) -> Result<Option<Value>, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(None);
    };
    let Value::UserData(ud) = value else {
        return Ok(Some(value.clone()));
    };
    let signal = ud
        .borrow::<Signal>()
        .map_err(|_| invalid(property, format!("expected a plain value or a Signal, got {value:?}")))?;
    let resolved = signal
        .get_value(lua)
        .map_err(|e| invalid(property, format!("Signal getter failed: {e}")))?;
    if matches!(resolved, Value::UserData(_)) {
        return Err(invalid(
            property,
            "a Signal resolved to another Signal -- resolution happens exactly once, not to a fixed point",
        ));
    }
    Ok(Some(resolved))
}

/// The one carve-out from decision 1's "parsers resolve a `Signal`" rule: [`SurfaceTopology`]'s
/// four fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/`parse_monitor`) keep rejecting
/// one outright, the same way every parser used to (this function used to be named
/// `reject_signal` and back every one of them).
///
/// `surface_topology` runs on every `Scene::apply` so `renderer/src/socket.rs`'s
/// `handle_reevaluate` can diff it against `applied_topology` and choose swap-versus-in-place
/// (ADR-0001). A `Signal` in one of these four fields would resolve once for that comparison and
/// then be free to change inside the live generation afterwards: a surface could move layer or
/// monitor with no swap, and the swap-versus-in-place decision would already have been made
/// against a value that no longer holds by the time anything acted on it. ADR-0044 decision 1
/// doesn't carve this out explicitly -- it's a gap in the ADR, not a case the ADR considered and
/// rejected.
fn reject_signal_in_topology_field(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

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
pub fn parse_size_mode(
    properties: &HashMap<String, Value>,
    property: &str,
    lua: &Lua,
) -> Result<SizeMode, LayoutError> {
    let Some(value) = resolve_property(properties, property, lua)? else {
        return Ok(SizeMode::Content);
    };
    if let Some(n) = value_as_f32(property, &value)? {
        if !(0.0..=8192.0).contains(&n) {
            return Err(invalid(
                property,
                format!("must be within [0, 8192], got {n}"),
            ));
        }
        return Ok(SizeMode::Pixels(n));
    }
    if let Value::String(s) = &value {
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
        format!("expected a number, \"Fill\", or a \"NN%\" string, got {value:?}"),
    ))
}

pub fn parse_edge_insets(
    properties: &HashMap<String, Value>,
    property: &str,
    lua: &Lua,
) -> Result<EdgeInsets, LayoutError> {
    let Some(value) = resolve_property(properties, property, lua)? else {
        return Ok(EdgeInsets::default());
    };
    let Value::Table(table) = &value else {
        return Err(invalid(
            property,
            format!("expected a table, got {value:?}"),
        ));
    };
    let edge = |key: &str| -> Result<f32, LayoutError> {
        let v: Value = table
            .get(key)
            .map_err(|e| invalid(property, e.to_string()))?;
        match v {
            Value::Nil => Ok(0.0),
            other => value_as_f32(property, &other)?.ok_or_else(|| {
                invalid(property, format!("`{key}` must be a number, got {other:?}"))
            }),
        }
    };
    Ok(EdgeInsets {
        top: edge("top")?,
        right: edge("right")?,
        bottom: edge("bottom")?,
        left: edge("left")?,
    })
}

pub fn parse_align(
    properties: &HashMap<String, Value>,
    property: &str,
    lua: &Lua,
) -> Result<Align, LayoutError> {
    let Some(value) = resolve_property(properties, property, lua)? else {
        return Ok(Align::Start);
    };
    let Value::String(s) = &value else {
        return Err(invalid(
            property,
            format!("expected a string, got {value:?}"),
        ));
    };
    match checked_string(property, s)?.as_str() {
        "Start" => Ok(Align::Start),
        "Center" => Ok(Align::Center),
        "End" => Ok(Align::End),
        "Stretch" => Ok(Align::Stretch),
        other => Err(invalid(property, format!("unknown alignment `{other}`"))),
    }
}

pub fn parse_visible(properties: &HashMap<String, Value>, lua: &Lua) -> Result<bool, LayoutError> {
    let Some(value) = resolve_property(properties, "visible", lua)? else {
        return Ok(true);
    };
    match value {
        Value::Boolean(b) => Ok(b),
        other => Err(invalid(
            "visible",
            format!("expected a boolean, got {other:?}"),
        )),
    }
}

pub fn parse_spacing(properties: &HashMap<String, Value>, lua: &Lua) -> Result<f32, LayoutError> {
    let Some(value) = resolve_property(properties, "spacing", lua)? else {
        return Ok(0.0);
    };
    value_as_f32("spacing", &value)?
        .ok_or_else(|| invalid("spacing", format!("expected a number, got {value:?}")))
}

pub fn parse_content(properties: &HashMap<String, Value>, lua: &Lua) -> Result<String, LayoutError> {
    let Some(value) = resolve_property(properties, "content", lua)? else {
        return Err(invalid("content", "text node requires `content`"));
    };
    match &value {
        Value::String(s) => checked_string("content", s),
        other => Err(invalid(
            "content",
            format!("expected a string, got {other:?}"),
        )),
    }
}

pub fn parse_font_size(properties: &HashMap<String, Value>, lua: &Lua) -> Result<f32, LayoutError> {
    let Some(value) = resolve_property(properties, "font_size", lua)? else {
        return Ok(12.0);
    };
    value_as_f32("font_size", &value)?
        .ok_or_else(|| invalid("font_size", format!("expected a number, got {value:?}")))
}

pub fn parse_icon_size(properties: &HashMap<String, Value>, lua: &Lua) -> Result<f32, LayoutError> {
    let Some(value) = resolve_property(properties, "size", lua)? else {
        return Err(invalid("size", "icon node requires `size`"));
    };
    value_as_f32("size", &value)?.ok_or_else(|| invalid("size", format!("expected a number, got {value:?}")))
}

/// Shared shape behind [`parse_surface_id`]/[`parse_layer`]/[`parse_monitor`]: fetch `property`,
/// reject a `Signal`, require it to be a string. `default` supplies the value when the property
/// is absent; `None` makes it required, erroring instead (Standards review, docs/adr/0024).
fn parse_string_property(properties: &HashMap<String, Value>, property: &str, default: Option<&str>) -> Result<String, LayoutError> {
    let value = match properties.get(property) {
        Some(value) => value,
        None => match default {
            Some(default) => return Ok(default.to_string()),
            None => return Err(invalid(property, format!("surface node requires `{property}`"))),
        },
    };
    reject_signal_in_topology_field(property, value)?;
    match value {
        Value::String(s) => Ok(s.to_string_lossy()),
        other => Err(invalid(property, format!("expected a string, got {other:?}"))),
    }
}

pub fn parse_surface_id(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "id", None)
}

/// § 6.1's `layer` (`"Background"`/`"Bottom"`/`"Top"`/`"Overlay"`). Required, same shape as
/// [`parse_surface_id`] -- every existing fixture in this repo already sets it. Stored as a raw
/// string, not a validated enum: Phase 13 only needs it for topology-diff equality (`CONTEXT.md`,
/// Topology change), not for binding a real `zwlr_layer_surface_v1` yet -- see docs/adr/0024.
pub fn parse_layer(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "layer", None)
}

/// § 6.1's `anchor` table (`{ top, bottom, left, right }` edge booleans). Same default-to-zero
/// shape as [`EdgeInsets`], booleans instead of floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Anchor {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

pub fn parse_anchor(properties: &HashMap<String, Value>) -> Result<Anchor, LayoutError> {
    let Some(value) = properties.get("anchor") else {
        return Ok(Anchor::default());
    };
    reject_signal_in_topology_field("anchor", value)?;
    let Value::Table(table) = value else {
        return Err(invalid("anchor", format!("expected a table, got {value:?}")));
    };
    let edge = |key: &str| -> Result<bool, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("anchor", e.to_string()))?;
        match v {
            Value::Nil => Ok(false),
            Value::Boolean(b) => Ok(b),
            other => Err(invalid("anchor", format!("`{key}` must be a boolean, got {other:?}"))),
        }
    };
    Ok(Anchor { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? })
}

/// § 6.1's `monitor` (a specific output EDID, or `"All"`). Absent defaults to `"All"` -- an
/// unqualified surface targets every monitor, matching the IDL's own documented meaning for that
/// value rather than treating the property as required.
pub fn parse_monitor(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "monitor", Some("All"))
}

/// A surface's topology-relevant fields (`CONTEXT.md`, Topology change: "adds, removes, or
/// changes the layer, anchor, or monitor target of a top-level `surface` node"). Structural
/// equality on `Vec<SurfaceTopology>` (order-sensitive) is the Renderer's own topology diff --
/// see `renderer/src/socket.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceTopology {
    pub id: String,
    pub layer: String,
    pub anchor: Anchor,
    pub monitor: String,
}

pub fn surface_topology(properties: &HashMap<String, Value>) -> Result<SurfaceTopology, LayoutError> {
    Ok(SurfaceTopology {
        id: parse_surface_id(properties)?,
        layer: parse_layer(properties)?,
        anchor: parse_anchor(properties)?,
        monitor: parse_monitor(properties)?,
    })
}

/// A single-node property (`surface.child`), converted from its raw table via
/// `lua::nodes::deserialize_lua_table` -- not re-implemented here.
pub fn parse_single_child(
    properties: &HashMap<String, Value>,
    property: &str,
    lua: &Lua,
) -> Result<Option<VirtualNode>, LayoutError> {
    let Some(value) = resolve_property(properties, property, lua)? else {
        return Ok(None);
    };
    let Value::Table(table) = &value else {
        return Err(invalid(
            property,
            format!("expected a node table, got {value:?}"),
        ));
    };
    let node = deserialize_lua_table(table).map_err(|e| invalid(property, e.to_string()))?;
    Ok(Some(node))
}

/// An array-of-nodes property (`rect`/`row`/`column`/`button.children`).
pub fn parse_children(
    properties: &HashMap<String, Value>,
    lua: &Lua,
) -> Result<Vec<VirtualNode>, LayoutError> {
    let Some(value) = resolve_property(properties, "children", lua)? else {
        return Ok(Vec::new());
    };
    let Value::Table(table) = &value else {
        return Err(invalid(
            "children",
            format!("expected an array table, got {value:?}"),
        ));
    };
    let mut children = Vec::new();
    for entry in table.sequence_values::<mlua::Table>() {
        let entry = entry.map_err(|e| invalid("children", e.to_string()))?;
        let node = deserialize_lua_table(&entry).map_err(|e| invalid("children", e.to_string()))?;
        children.push(node);
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> mlua::Lua {
        mlua::Lua::new()
    }

    fn props_from_table(table: &mlua::Table) -> HashMap<String, Value> {
        deserialize_lua_table(table).unwrap().properties
    }

    #[test]
    fn width_absent_is_content() {
        let props = HashMap::new();
        assert_eq!(parse_size_mode(&props, "width", &lua()).unwrap(), SizeMode::Content);
    }

    #[test]
    fn width_integer_is_pixels() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = 32 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_size_mode(&props, "width", &lua).unwrap(),
            SizeMode::Pixels(32.0)
        );
    }

    #[test]
    fn width_fill_string_is_fill() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "Fill" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_size_mode(&props, "width", &lua).unwrap(), SizeMode::Fill);
    }

    #[test]
    fn width_percent_string_divides_by_100() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "50%" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_size_mode(&props, "width", &lua).unwrap(),
            SizeMode::Percent(0.5)
        );
    }

    #[test]
    fn width_above_the_8192_ceiling_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = 8193 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width", &lua).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn a_negative_width_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = -5 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width", &lua).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn width_garbage_string_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "banana" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width", &lua).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn margin_reads_named_edges_defaulting_absent_ones_to_zero() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", margin = { top = 4, left = 2 } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let insets = parse_edge_insets(&props, "margin", &lua).unwrap();
        assert_eq!(
            insets,
            EdgeInsets {
                top: 4.0,
                right: 0.0,
                bottom: 0.0,
                left: 2.0
            }
        );
    }

    #[test]
    fn align_h_parses_all_four_variants() {
        for (text, expected) in [
            ("Start", Align::Start),
            ("Center", Align::Center),
            ("End", Align::End),
            ("Stretch", Align::Stretch),
        ] {
            let lua = lua();
            let table: mlua::Table = lua
                .load(format!(r#"return {{ kind = "rect", align_h = "{text}" }}"#))
                .eval()
                .unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_align(&props, "align_h", &lua).unwrap(), expected);
        }
    }

    #[test]
    fn visible_absent_defaults_true() {
        let props = HashMap::new();
        assert!(parse_visible(&props, &lua()).unwrap());
    }

    #[test]
    fn a_signal_userdata_in_a_geometry_slot_resolves_to_its_current_value() {
        // Replaces the old "rejected" test (docs/adr/0044 decision 1): `visible` is not a
        // topology field, so it now resolves a `Signal` instead of erroring on one.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(false)).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("visible", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(
            !parse_visible(&node.properties, &lua).unwrap(),
            "must read the signal's current value, not error on the handle"
        );
    }

    #[test]
    fn text_content_is_required() {
        let props = HashMap::new();
        assert!(matches!(
            parse_content(&props, &lua()).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn a_signal_resolving_to_a_string_satisfies_content() {
        // ADR-0044 decision 1, step 1: a Signal wrapping "hello" parses as "hello".
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let hello = lua.create_string("hello").unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(hello)).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("content", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(parse_content(&node.properties, &lua).unwrap(), "hello");
    }

    #[test]
    fn a_signal_resolving_to_a_table_reports_the_same_error_a_literal_table_would() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();

        let literal_table: mlua::Table = lua.load(r#"return { kind = "text", content = {} }"#).eval().unwrap();
        let literal_props = props_from_table(&literal_table);
        let literal_err = parse_content(&literal_props, &lua).unwrap_err();

        let signal = crate::lua::signal::Signal::new_live(Value::Table(lua.create_table().unwrap())).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("content", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let signal_err = parse_content(&node.properties, &lua).unwrap_err();

        for err in [&literal_err, &signal_err] {
            assert!(matches!(
                err,
                LayoutError::InvalidProperty { property, detail }
                    if property == "content" && detail.starts_with("expected a string")
            ));
        }
    }

    #[test]
    fn a_signal_resolving_to_another_signal_is_an_error() {
        // ADR-0044 decision 1's "resolve exactly once": a Signal whose value is itself a Signal
        // userdata is an error, not a second read to a fixed point.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let inner = crate::lua::signal::Signal::new_live(Value::Integer(5)).0;
        let inner_userdata = lua.create_userdata(inner).unwrap();
        let outer = crate::lua::signal::Signal::new_live(Value::UserData(inner_userdata)).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", outer).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(
            parse_font_size(&node.properties, &lua).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    #[test]
    fn spacing_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        // CONFIRMED finding: marshal::check_number(1e300) is Ok (1e300 is a finite f64), but the
        // very next line's `as f32` saturates it to f32::INFINITY. `spacing` has no range check
        // of its own (unlike `parse_size_mode`'s `[0, 8192]`), so without this fix the Inf sails
        // through to `intrinsic_content_size`'s `spacing * visible.len().saturating_sub(1) as f32`
        // -- `inf * 0.0` is `NaN`, silently producing NaN geometry instead of a LayoutError.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "row", spacing = 1e300 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_spacing(&props, &lua).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "spacing"
        ));
    }

    #[test]
    fn font_size_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        // Same defeated-guard bug as spacing, on a different unranged property: `font_size`
        // reaching `shaping.shape` as Inf would compute `line_height = inf * 1.2` instead of
        // erroring.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "text", font_size = 1e300 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_font_size(&props, &lua).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    #[test]
    fn font_size_absent_defaults_to_twelve() {
        let props = HashMap::new();
        assert_eq!(parse_font_size(&props, &lua()).unwrap(), 12.0);
    }

    #[test]
    fn a_signal_resolving_to_a_number_satisfies_font_size_through_marshals_check_number() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Number(18.0)).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(parse_font_size(&node.properties, &lua).unwrap(), 18.0);
    }

    #[test]
    fn icon_size_is_required() {
        let props = HashMap::new();
        assert!(matches!(
            parse_icon_size(&props, &lua()).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn parse_children_walks_nested_node_tables() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "row", children = { { kind = "text", content = "a" }, { kind = "text", content = "b" } } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let children = parse_children(&props, &lua).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].kind, "text");
        assert_eq!(
            children[1]
                .properties
                .get("content")
                .unwrap()
                .as_string()
                .unwrap()
                .to_string_lossy(),
            "b"
        );
    }

    #[test]
    fn parse_single_child_converts_the_child_table() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "surface", child = { kind = "rect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let child = parse_single_child(&props, "child", &lua).unwrap();
        assert_eq!(child.unwrap().kind, "rect");
    }

    #[test]
    fn parse_single_child_absent_is_none() {
        let props = HashMap::new();
        assert!(parse_single_child(&props, "child", &lua()).unwrap().is_none());
    }

    #[test]
    fn layer_is_required() {
        let props = HashMap::new();
        assert!(matches!(parse_layer(&props).unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn layer_reads_the_string() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", layer = "Top" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_layer(&props).unwrap(), "Top");
    }

    #[test]
    fn anchor_absent_defaults_all_false() {
        let props = HashMap::new();
        assert_eq!(parse_anchor(&props).unwrap(), Anchor::default());
    }

    #[test]
    fn anchor_reads_named_edges_defaulting_absent_ones_to_false() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", anchor = { top = true, left = true } }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_anchor(&props).unwrap(), Anchor { top: true, right: false, bottom: false, left: true });
    }

    #[test]
    fn monitor_absent_defaults_to_all() {
        let props = HashMap::new();
        assert_eq!(parse_monitor(&props).unwrap(), "All");
    }

    #[test]
    fn monitor_reads_the_string() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", monitor = "eDP-1" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_monitor(&props).unwrap(), "eDP-1");
    }

    #[test]
    fn surface_topology_combines_id_layer_anchor_and_monitor() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "surface", id = "bar", layer = "Top", anchor = { top = true }, monitor = "eDP-1" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let topology = surface_topology(&props).unwrap();
        assert_eq!(
            topology,
            SurfaceTopology {
                id: "bar".to_string(),
                layer: "Top".to_string(),
                anchor: Anchor { top: true, right: false, bottom: false, left: false },
                monitor: "eDP-1".to_string(),
            }
        );
    }

    #[test]
    fn a_signal_userdata_in_layer_is_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true)).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "surface").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(parse_layer(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer"));
    }
}
