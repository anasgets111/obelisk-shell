//! Typed property parsing for the layout engine (`docs/oblisk-idl-api-specs.md` § 5.1).
//! `lua::nodes::VirtualNode` leaves every property as a raw `mlua::Value`; this module turns it
//! into typed, validated properties.
//! A `Value::UserData` holding a `Signal` (§ 1.2) is resolved rather than rejected (ADR-0044
//! decision 1), and every such read happens in exactly one place, [`resolve_properties`], run once
//! per node per pass. The parsers below take plain values and no `&Lua`, so two parsers reading
//! the same property in the same pass see the same answer. That guarantee covers a `Signal` and
//! nothing else: a plain Lua table with an `__index` metamethod is copied in as-is, and every
//! `table.get` a parser makes still runs it afresh; see [`parse_edge_insets`]'s `ponytail:` for
//! the hole this leaves. A signal resolving to another `Signal` is an error, not a second read:
//! not a recursion bound, since a computed signal's getter returning fresh depth on every call
//! recurses through `layout::scene::prepare` and `deserialize_lua_table` as deep as it wants.
//! `layout::scene::MAX_TREE_DEPTH` caps that and raises [`LayoutError::TreeTooDeep`].
//! [`SurfaceTopology`]'s five fields and every node's optional `id` (ADR-0045 decision 1) still
//! reject a `Signal` outright; see [`reject_signal_in_structural_field`]. A `panel`'s remaining
//! § 6.1 properties are not carve-outs, since layer-shell accepts each on a live surface.

mod content;
mod paint_style;
mod spec;
mod style;
mod surface;
mod toplevel;

// The paint-only parsers are imported, not re-exported: [`paint_style`] is now their only caller
// (ADR-0068, replacing `layout::paint` itself), reached via `super::*` in `paint_style.rs`.
use content::{
    parse_elide, parse_fit, parse_font_size, parse_foreground, parse_icon_name, parse_image_source,
    parse_mask_character, parse_max_lines, parse_optional_foreground, parse_placeholder, parse_text_align, parse_wrap,
};
use spec::parse_secure_submit;
use style::{parse_background, parse_border_color, parse_border_width, parse_clip, parse_radius};

pub use content::{
    Elide, StyleRun, TextAlign, Wrap, font_runs, parse_content, parse_icon_size, parse_node_id, parse_surface_id,
    segments,
};
pub use paint_style::{PaintStyle, paint_style};
pub use spec::{
    SecureSubmitTarget, SurfaceFingerprint, SurfaceSpec, lock_spec, parse_children, parse_list_children,
    parse_single_child,
};
// `wayland::tests`' and `instance::tests`' fixtures name it `node::LockSpec`; nothing else does.
#[cfg(test)]
pub use spec::LockSpec;
pub use style::{
    BorderColor, ClipShape, parse_align, parse_cursor, parse_edge_insets, parse_list_direction, parse_opacity,
    parse_size_mode, parse_spacing, parse_visible,
};
pub use surface::{Anchor, Exclusive, KeyboardInteractivity, LayerKind, PanelSpec, SurfaceTopology, panel_spec};
pub use toplevel::{ConstraintAdjustment, PopupAnchor, PopupSpec, SizeHint, WindowSpec, popup_spec, window_spec};
// `wayland::tests`' fixtures name it `node::PopupOffset`, same reason as `LockSpec` above.
#[cfg(test)]
pub use toplevel::PopupOffset;

use std::collections::HashMap;

use mlua::{Lua, Value};

use crate::lua::marshal;
use crate::lua::signal::{self, is_signal};

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

/// A parsed colour, four channels in `0.0..=1.0`. `f32`, not `u8`: femtovg's `Color` (read by
/// `layout::paint`) already stores channels as `f32`. `Color::rgbaf` takes them as-is, while
/// `Color::rgba` takes `u8` and divides by 255.0 to reach that form. Storing `f32` here means the
/// drawing pass copies straight into `Color`, no `u8` round-trip to undo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("unsupported node kind `{0}`")]
    UnsupportedNodeKind(String),
    #[error("invalid value for `{property}`: {detail}")]
    InvalidProperty { property: String, detail: String },
    #[error("`{0}` is a Signal handle, not a plain value -- read it via :get() before returning it from shell.lua")]
    UnsupportedSignalProperty(String),
    /// `layout::scene::prepare`'s recursion, bounded at `layout::scene::MAX_TREE_DEPTH`. Covers
    /// both a literal cyclic tree (`r.children = { r }`) and a computed `children` signal
    /// generating fresh depth on every read, since both recurse through the same Rust call.
    /// `max` is the number of levels actually admitted; `depth`, the 1-based level refused, is
    /// always `max + 1`, so the message states the limit the code enforces, not one adjacent to it.
    #[error(
        "node tree exceeds the maximum depth of {max} levels (at `{kind}`, level {depth}) -- a node holding itself in `children`?"
    )]
    TreeTooDeep { kind: String, depth: u32, max: u32 },
    /// One whole `Scene::apply` ran past `lua::signal`'s `LAYOUT_PASS_CAP`. Distinct from the
    /// `InvalidProperty` a blown per-getter budget produces: only this one can be
    /// reached with no `Signal` in the config at all, since a resolved table's `__index` metamethod
    /// is Lua the pass runs outside any signal evaluation.
    #[error(
        "the layout pass exceeded its CPU budget -- a property getter or an `__index` metamethod that does not return?"
    )]
    PassBudgetExceeded,
}

/// `pub(crate)` rather than private: `layout::scene`'s `Scene::apply_one_instance` raises an
/// `id`-scoped error for an instance naming an undeclared surface, so every `InvalidProperty` in
/// this crate is built here rather than by hand.
pub(crate) fn invalid(property: &str, detail: impl Into<String>) -> LayoutError {
    LayoutError::InvalidProperty { property: property.to_string(), detail: detail.into() }
}

/// Longest prefix of a rejected value's `Debug` form this file will ever put in an error message.
/// 200 bytes, not `marshal::MAX_STRING_BYTES` (64KB): that cap answers how much of a *string
/// property* is a legitimate value, while this one bounds a line of `rescue`'s `error_log`
/// (§ 2.10), enough to recognize the value, not a paste buffer.
const MAX_ERROR_VALUE_PREVIEW_BYTES: usize = 200;

/// `pub(crate)` since `layout::scene`'s `list` node rejects a bad `source`/`itemfn`/`key` value
/// from outside this module and needs the same bounded preview. Renders a `Value` for an
/// [`invalid`] detail without ever formatting its `Debug` form in full first:
/// `format!("{value:?}")` on an oversized `Value::String` allocates and escapes the whole thing
/// before any truncation could run, so `rect { radius = string.rep("x", 20 * 1024 * 1024) }`
/// would format 20 MB on the Wayland dispatch thread before the error reaches `rescue`.
/// `marshal::check_string`'s 64KB cap never runs here; it lives in [`checked_string`], which a
/// value rejected for the wrong *type* never reaches (only `Value::String` has unbounded `Debug`
/// in mlua 0.12; everything else derives `Debug` over a fixed-width `ValueRef` pointer). Measured
/// here, formatting a Lua string of each size against this function:
///
/// | size | `format!("{value:?}")` | this function |
/// |---|---|---|
/// | 1 MB | 1.64 ms | 0.0088 ms |
/// | 20 MB | 23.96 ms | 0.0057 ms |
/// | 100 MB | 93.88 ms | 0.0061 ms |
///
/// Cost is a function of the cap, not the input: 23.96 ms is more than a whole frame at 60fps,
/// worth fixing while `layout::paint` still validated a `background` or `radius` while drawing.
/// It no longer does (ADR-0068), so the cap buys a bounded `rescue` message, not a bounded frame.
/// The regression test
/// `oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix` asserts a
/// naive format-then-truncate cannot report the value's true length.
pub(crate) fn preview_for_error(value: &Value) -> String {
    let Value::String(s) = value else {
        return format!("{value:?}");
    };
    // `as_bytes()` borrows the Lua string's own buffer, no copy or escaping, so measuring its
    // length is an O(1) check that must run before any formatting decision.
    let bytes = s.as_bytes();
    let total_len = bytes.len();
    if total_len <= MAX_ERROR_VALUE_PREVIEW_BYTES {
        return format!("{value:?}");
    }
    // Slice first, format second: only the bounded prefix reaches a `Debug`-style formatter, so a
    // 20 MB string costs O(200 bytes), not O(len). Rendered lossily on purpose (a log preview, not
    // an equality key): slicing raw bytes at a fixed offset can land mid-codepoint.
    let prefix = String::from_utf8_lossy(&bytes[..MAX_ERROR_VALUE_PREVIEW_BYTES]);
    format!(
        "String({prefix:?}...) -- {total_len} bytes total, truncated to the first {MAX_ERROR_VALUE_PREVIEW_BYTES} here"
    )
}

/// Runs a numeric `Value` through the marshalling boundary (`lua::marshal`, ADR-0044 decision 1)
/// before this parser's own range checks (e.g. `parse_size_mode`'s `[0, 8192]`), catching a
/// NaN/Inf `f64` or an out-of-2^53-range `Integer`, whether literal or resolved via
/// [`resolve_properties`]. `marshal::check_number` alone is not enough: a finite `f64` like
/// `1e300` sails through it and overflows to `f32::INFINITY` on the narrowing cast below, and a
/// caller with no further range check (e.g. `parse_spacing`) would hand that `Inf` straight into
/// layout arithmetic. `inf * 0.0` is `NaN`, and `snap_to_physical`'s final `as i32` silently
/// saturates a `NaN` rect to `0` instead of raising an error, so the finiteness check re-runs
/// after the cast, on the `f32`, naming the same property a non-finite literal would.
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

/// Runs a Lua string through `marshal::check_string`'s 64KB cap and returns the owned `String`,
/// same reasoning as [`value_as_f32`].
fn checked_string(property: &str, s: &mlua::LuaString) -> Result<String, LayoutError> {
    let s = s.to_string_lossy();
    marshal::check_string(&s).map_err(|e| invalid(property, e.to_string()))?;
    Ok(s)
}

/// Strict `#RRGGBB` / `#RRGGBBAA` hex colour parsing (§ 5.2's `rect.background`, `border_color`,
/// `text.foreground`). No 3-digit shorthand, no named colours, no bare digits without `#`: § 5.2
/// specifies none of them.
fn parse_hex_color(property: &str, s: &str) -> Result<Rgba, LayoutError> {
    let Some(digits) = s.strip_prefix('#') else {
        return Err(invalid(property, format!("hex colour must start with `#`, got `{s}`")));
    };
    // Digit check before length check, deliberately: every multi-byte UTF-8 byte is >= 0x80 and
    // fails `is_ascii_hexdigit`, giving non-ASCII input (`"#日本語"`) the accurate diagnosis. After
    // it, the string is known ASCII, so `.len()` is a character count, not the wrong byte count
    // (9 bytes, 3 characters) `"#日本語"` would otherwise report.
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(property, format!("hex colour must contain only hex digits, got `{s}`")));
    }
    if digits.len() != 6 && digits.len() != 8 {
        return Err(invalid(
            property,
            format!("hex colour must have 6 or 8 hex digits after `#`, got {} in `{s}`", digits.len()),
        ));
    }
    let channel = |range: std::ops::Range<usize>| -> f32 {
        u8::from_str_radix(&digits[range], 16).expect("digits validated as hex above") as f32 / 255.0
    };
    let a = if digits.len() == 8 { channel(6..8) } else { 1.0 };
    Ok(Rgba { r: channel(0..2), g: channel(2..4), b: channel(4..6), a })
}

/// Whether `property` is one [`resolve_properties`] copies through untouched on a node of this
/// `kind`, so [`reject_signal_in_structural_field`] still sees a raw `Value::UserData` and can
/// refuse it. Resolving then rejecting is unimplementable: once read, a signal's value is
/// indistinguishable from a literal. Kind-aware because a skip is only sound where a parser runs
/// to do the rejecting. `id` is
/// skipped on every kind ([`parse_node_id`]/[`parse_surface_id`] read it wherever it appears).
/// `layer`/`anchor`/`monitor`/`namespace` are read only by `surface::surface_topology` on
/// top-level surfaces; skipping them on a `rect` (where no parser reads them) would leak a live
/// `Signal` into `layout::scene::RetainedNode::properties`, breaking `ResolvedNode::properties`'s
/// "never a `Signal`" invariant. Below a surface these resolve like any ordinary property.
/// `namespace` joins the carve-out for the same protocol reason as `monitor`:
/// `zwlr_layer_shell_v1::get_layer_surface` fixes a namespace at creation and no request changes
/// it on a live surface. The in-place `panel` fields (`keyboard_interactivity`, `exclusive`,
/// `margin`, `width`/`height`) are deliberately *not* here: layer-shell permits changing each on a
/// live surface (ADR-0044 decision 1). `window`,
/// `popup` and `lock` add nothing, by the same live-object test: a `window`'s
/// `set_title`/`set_app_id`/`set_min_size`/`set_max_size` are all valid requests on a mapped
/// toplevel; a `popup`'s whole `xdg_positioner` is rebuilt on every open (ADR-0049 decision 1), so
/// `parent`/`anchor_rect`/`anchor`/`gravity` are meant to carry a `Signal`; a `lock`'s § 6.4
/// property list is only `id` and `child`, already the universal arm's as a reconcile identity
/// rather than a protocol field. `hover` joins it there on any kind (ADR-0062 decision 3): it
/// names the signal the pointer handler writes, and a resolved `hover` would arrive as the
/// boolean `false`, saying nothing about *which* signal that is.
fn is_structural_property(kind: &str, property: &str) -> bool {
    property == "id"
        || property == "hover"
        // ADR-0069 decision 4: the positioning pass reads this signal's number and writes the
        // clamped one back, so it needs the handle, not a snapshot.
        || property == "scroll"
        || (kind == "panel" && matches!(property, "layer" | "anchor" | "monitor" | "namespace"))
}

/// One node's raw property map with every `Signal` replaced by its current value (ADR-0044
/// decision 1). Called once per node per pass, as that node enters reconciliation; everything
/// downstream (this module's parsers, `layout::scene`'s sizing/positioning passes,
/// `RetainedNode::properties`) reads the result, not the raw map. Once, and once is load-bearing:
/// `Signal::get_value` runs a `computed` signal's Lua closure, and a closure that is not a pure
/// function of unchanged state (`os.clock()`, `math.random`, an accumulator upvalue) answers
/// differently on every call, so one read per property makes the resolved tree a snapshot of one
/// pass and stops ADR-0021's per-`get_value` 5ms budget being paid four times over for one
/// property. The snapshot covers the *signals* only: a plain table with an `__index` metamethod
/// is copied through as-is, and each `table.get` a parser makes still runs it again; see
/// [`parse_edge_insets`]'s `ponytail:`. Nor is this ADR-0044 decision 3's rejected memoization,
/// which caches *across* pushes and needs an invalidation rule no push has. Per entry: a key
/// [`is_structural_property`] names for this node's `kind` is copied through
/// raw, signal and all. A `Value::UserData` holding a `Signal` is read through `Signal::get_value`
/// and the *result* stored in its place; a result that is itself a `Signal` is an error naming
/// the property, not a second read, avoiding an unbounded loop on a cyclic construction. A result
/// of `Value::Nil` **omits the key entirely**: ADR-0044 decision 1's amendment ("a signal
/// resolving to nil means the property is absent") falls out of the map rather than being
/// re-checked in every parser. Matters at boot: `RendererClient::run_startup_evaluation` runs
/// before the poll loop drains any inbound frame, so every `shared::Capability::ALL` signal still
/// reads `nil` at the first `Scene::apply`, and a bare capability binding must not fail layout
/// there; also consistent with a Lua table's own inability to store `nil`, so `visible = nil`
/// reads the same. Everything else, including a `UserData` that is not a `Signal`, is copied
/// through unchanged, for whichever parser reads it. Every property resolves, including ones no
/// parser reads today: the resolved map is what the
/// paint stage reads a colour or radius straight off (`ResolvedNode::properties`), and § 5.1 puts
/// no property out of a `Signal`'s reach, so there is no subset safe to skip. A getter that raises
/// fails the whole apply, even for a property nothing downstream looked at: deferring would mean
/// keeping the getter around to re-run later, the second read this function prevents.
///
/// ponytail: one fresh `HashMap` per node per pass, not reusing the retained node's allocation
/// across passes. Costs more now that ADR-0044 decision 2's dirty flag makes a pass a per-push,
/// not per-config-edit, event on the dispatch thread. Upgrade path: resolve in place over the
/// retained map, reordering the reconcile match before resolution.
///
/// ponytail: every property *holding a `Signal`* is evaluated every pass, paint-only ones
/// included (`background`, `color`, `radius`), each buying its own ADR-0021 5ms budget (§ 1.2), so
/// four signal-bound paint properties cost four budgets in a pass ADR-0044 decision 2 now runs per
/// capability push. Upgrade path: [`parse_edge_insets`]'s `ponytail:` whole-pass budget.
pub fn resolve_properties(
    properties: &HashMap<String, Value>,
    kind: &str,
    lua: &Lua,
) -> Result<HashMap<String, Value>, LayoutError> {
    let mut resolved = HashMap::with_capacity(properties.len());
    // Sorted, and the sort is the point: `properties` is a `HashMap` with per-process randomised
    // iteration order, so two failing properties on one node used to name whichever the hash seed
    // reached first, differing across runs. `renderer/src/socket.rs` puts this message in the
    // `rescue` global's `error_log` for a human to read (§ 2.10, ADR-0024), so which property a
    // broken config names must be a function of the config alone. Do not "optimise" this into a
    // bare `for (property, value) in properties`.
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort_unstable();
    for property in names {
        let value = &properties[property];
        if is_structural_property(kind, property) {
            resolved.insert(property.clone(), value.clone());
            continue;
        }
        let Value::UserData(ud) = value else {
            resolved.insert(property.clone(), value.clone());
            continue;
        };
        let Some(signal) = signal::from_userdata(ud) else {
            resolved.insert(property.clone(), value.clone());
            continue;
        };
        let value = signal.get_value(lua).map_err(|e| invalid(property, format!("Signal getter failed: {e}")))?;
        match value {
            Value::UserData(_) => {
                return Err(invalid(
                    property,
                    "a Signal resolved to another Signal -- resolution happens exactly once, not to a fixed point",
                ));
            }
            Value::Nil => {}
            value => {
                resolved.insert(property.clone(), value);
            }
        }
    }
    // `on_hover` fires on the crossing its node's own `hover` signal reports, so that signal is
    // where the "was it hovered last pass" memory lives and there is no second one (ADR-0095).
    // Without a slot the callback is unreachable, and this is the kind of silence
    // `deserialize_lua_table`'s unknown-key rejection exists to end: a config that declared it
    // would watch a handler never fire with nothing anywhere saying why.
    if resolved.contains_key("on_hover") && !resolved.contains_key("hover") {
        return Err(invalid(
            "on_hover",
            "declared without a `hover` slot on the same node -- add `hover = hover(\"a-name\")`, which is what remembers whether this node was hovered last pass, and so what tells its crossings from another node's",
        ));
    }
    Ok(resolved)
}

/// The carve-outs from decision 1's "parsers resolve a `Signal`" rule: [`SurfaceTopology`]'s five
/// fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/`parse_monitor`/`parse_namespace`) and
/// every node's optional `id` ([`parse_node_id`], ADR-0045 decision 1) keep rejecting one outright.
/// The unifying reason: each is read exactly once per evaluation and a *structural* decision
/// (where a surface is placed, or which retained node a fresh one is) is then made and acted on. A
/// `Signal` is free to change between passes, so admitting one here would leave that decision
/// resting on a value that no longer holds. Every other property is read for the geometry or
/// appearance of the pass it was read in, so a later change simply produces different output next
/// pass. Concretely: `surface_topology` runs on every `Scene::apply` so `socket.rs`'s
/// `handle_reevaluate` can diff it against `applied_topology` and choose swap-versus-in-place
/// (ADR-0001); a surface could otherwise move layer or monitor with no swap. `id` is
/// `pair_children_by_id_then_position`'s reconcile identity, matched once per `Scene::apply` to
/// pair a fresh child against its retained counterpart; a later-changing value would make "the
/// same node as last time" ambiguous. ADR-0044 decision 1 leaves both out: a gap, not a rejected
/// case. This only works because [`resolve_properties`] copies the keys
/// [`is_structural_property`] names through raw: these six parsers alone read the un-resolved
/// value, since a resolved signal is indistinguishable from a literal by the time it reaches a map.
fn reject_signal_in_structural_field(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

/// Whether `property` currently holds a live [`crate::lua::signal::Signal`]: the one thing an
/// **unresolved** property map can say that a resolved one cannot, "this pass is not in a position
/// to check it" (ADR-0049's second amendment). Only ever true on the evaluation-time pass:
/// [`resolve_properties`] reads every `Signal` it is handed and stores the *result* in its place,
/// refusing a result that is itself a `Signal`, so no map it has been through can hold one, except
/// under a key [`is_structural_property`] copies through raw, which
/// [`reject_signal_in_structural_field`] refuses outright instead of deferring. A parser
/// consulting this applies the amendment's split: on the pass reading raw properties
/// (`crate::socket`'s `surface_specs`, which runs before any getter has been called and must not
/// call one), a signal-bound property is skipped and left at the parser's documented placeholder;
/// the authoritative value is later re-read from the resolved tree by `App::apply_resolved_state`.
/// A *literal* is still fully validated on that pass, so a config typo fails fast into
/// ADR-0046's `rescue` log rather than as an `xdg_positioner` protocol error at first open.
/// Without this, `anchor_rect = popup_anchor` (ADR-0050 decision 3's spelling) would fail
/// evaluation: every parser below otherwise rejects a raw `Value::UserData` with a type error.
fn is_deferred_signal(properties: &HashMap<String, Value>, property: &str) -> bool {
    matches!(properties.get(property), Some(Value::UserData(ud)) if is_signal(ud))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Fit;
    use crate::lua::nodes::deserialize_lua_table;

    fn lua() -> mlua::Lua {
        mlua::Lua::new()
    }

    fn props_from_table(table: &mlua::Table) -> HashMap<String, Value> {
        deserialize_lua_table(table).unwrap().properties
    }

    #[test]
    fn a_signal_resolving_to_another_signal_is_an_error() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let inner = crate::lua::signal::Signal::new_live(Value::Integer(5), crate::lua::signal::DirtyFlag::new()).0;
        let inner_userdata = lua.create_userdata(inner).unwrap();
        let outer =
            crate::lua::signal::Signal::new_live(Value::UserData(inner_userdata), crate::lua::signal::DirtyFlag::new())
                .0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", outer).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(
            resolve_properties(&node.properties, "text", &lua).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    /// A *resolved* property bag whose `property` slot held a live signal currently reading `nil`
    /// -- exactly the state every rostered capability's global is in before its first
    /// `StateSnapshot` (`renderer/src/socket.rs`'s `RendererClient::new` seeds all of
    /// `shared::Capability::ALL` at `Value::Nil`), which is what a config binding a bare capability
    /// signal resolves at startup. Routed through [`resolve_properties`] because that is where the
    /// nil rule now lives: the key is omitted from the resolved map rather than each parser
    /// checking for a `Value::Nil` of its own.
    fn props_with_nil_signal(lua: &mlua::Lua, kind: &str, property: &str) -> HashMap<String, Value> {
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Nil, crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", kind).unwrap();
        table.set(property, signal).unwrap();
        resolve_properties(&props_from_table(&table), kind, lua).unwrap()
    }

    #[test]
    fn a_signal_resolving_to_nil_takes_each_parsers_absent_property_default() {
        let lua = lua();
        assert!(
            !props_with_nil_signal(&lua, "rect", "width").contains_key("width"),
            "the rule is one omitted key, not a Nil each parser re-checks"
        );
        assert_eq!(parse_size_mode(&props_with_nil_signal(&lua, "rect", "width"), "width").unwrap(), SizeMode::Content);
        assert_eq!(
            parse_edge_insets(&props_with_nil_signal(&lua, "rect", "padding"), "padding").unwrap(),
            EdgeInsets::default()
        );
        assert_eq!(parse_align(&props_with_nil_signal(&lua, "rect", "align_h"), "align_h").unwrap(), Align::Start);
        assert!(parse_visible(&props_with_nil_signal(&lua, "rect", "visible")).unwrap());
        assert_eq!(parse_spacing(&props_with_nil_signal(&lua, "row", "spacing")).unwrap(), 0.0);
        assert_eq!(parse_font_size(&props_with_nil_signal(&lua, "text", "font_size")).unwrap(), 12.0);
        assert!(parse_single_child(&props_with_nil_signal(&lua, "panel", "child"), "child").unwrap().is_none());
        assert!(parse_children(&props_with_nil_signal(&lua, "row", "children")).unwrap().is_empty());
        assert_eq!(parse_content(&props_with_nil_signal(&lua, "text", "content")).unwrap().0, "");
        assert_eq!(parse_icon_size(&props_with_nil_signal(&lua, "icon", "size")).unwrap(), 12.0);
        assert_eq!(parse_icon_name(&props_with_nil_signal(&lua, "icon", "name")).unwrap(), "");
        assert_eq!(parse_image_source(&props_with_nil_signal(&lua, "image", "source")).unwrap(), "");
        assert_eq!(parse_fit(&props_with_nil_signal(&lua, "image", "fit")).unwrap(), Fit::Cover);
    }

    #[test]
    fn resolve_properties_copies_a_structural_field_through_raw_so_it_can_still_be_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(&node.properties, "rect", &lua).unwrap();

        assert!(matches!(resolved.get("id"), Some(Value::UserData(_))), "id must survive the resolve step unresolved");
        assert!(
            matches!(parse_node_id(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id")
        );
    }

    /// The pairing rule (ADR-0095). `on_hover` fires on the crossing its node's `hover` signal
    /// reports, so without a slot the callback is unreachable -- refused, rather than left to be a
    /// handler a config watches never fire.
    #[test]
    fn on_hover_without_a_hover_slot_is_refused() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("on_hover", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        assert!(matches!(resolve_properties(&node.properties, "rect", &lua).unwrap_err(),
                LayoutError::InvalidProperty { property, .. } if property == "on_hover"));
    }

    #[test]
    fn on_hover_alongside_a_hover_slot_resolves() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let (over, _rect) = crate::lua::signal::Signal::new_hover(crate::lua::signal::DirtyFlag::new(), Value::Nil);
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("hover", over).unwrap();
        table.set("on_hover", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(&node.properties, "rect", &lua).unwrap();
        assert!(matches!(resolved.get("on_hover"), Some(Value::Function(_))), "a Function is not a Signal to resolve");
        assert!(matches!(resolved.get("hover"), Some(Value::UserData(_))), "the slot stays the handle it was");
    }

    #[test]
    fn two_failing_properties_always_report_the_same_one() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                    return {
                        kind = "rect",
                        background = computed({}, function() error("background boom") end),
                        radius = computed({}, function() error("radius boom") end),
                    }
                    "#,
            )
            .eval()
            .unwrap();
        for _ in 0..8 {
            let props = props_from_table(&table);
            let err = resolve_properties(&props, "rect", &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "background"),
                "the same broken config must always name the same property, got: {err}"
            );
        }
    }

    #[test]
    fn oversized_string_property_error_message_is_bounded() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", radius = string.rep("Q", 20 * 1024 * 1024) }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { property, detail } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(property, "radius");
        assert!(
            detail.len() < 1024,
            "a 20 MB input must not produce a multi-megabyte error message, got {} bytes",
            detail.len()
        );
    }

    #[test]
    fn oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", radius = string.rep("Q", 20 * 1024 * 1024) }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert!(detail.contains("expected a number"), "{detail}");
        assert!(detail.contains("String("), "must still name the rejected type: {detail}");
        assert!(detail.contains("QQQ"), "must show a recognizable prefix of the value: {detail}");
        assert!(
            detail.contains(&(20 * 1024 * 1024).to_string()),
            "must state the real length, or a truncated preview reads as the whole value: {detail}"
        );
    }

    #[test]
    fn short_string_property_error_message_is_unchanged() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = "banana" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(detail, "expected a number, got String(\"banana\")");
    }

    #[test]
    fn non_string_variant_error_message_is_unchanged() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(detail, "expected a number, got Boolean(true)");
    }
}
