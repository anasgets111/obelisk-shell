//! Typed property parsing for the layout engine (build-steps.md Phase 12,
//! `docs/oblisk-idl-api-specs.md` § 5.1). `lua::nodes::VirtualNode` leaves every property as a raw
//! `mlua::Value`; this module turns it into typed, validated properties.
//!
//! A `Value::UserData` holding a `Signal` (§ 1.2) is resolved rather than rejected (ADR-0044
//! decision 1), and every one of those reads happens in exactly one place: [`resolve_properties`],
//! run once per node per pass. The parsers below take plain values and no `&Lua` -- they see a map
//! where every `Signal` has already been read once, so two parsers reading the same property in the
//! same pass see the same answer.
//!
//! That guarantee covers a `Signal` and nothing else: a plain Lua table with an `__index`
//! metamethod is copied in as-is, and every `table.get` a parser makes still runs that metamethod
//! afresh -- see [`parse_edge_insets`]'s `ponytail:` for the hole this leaves and what closing it
//! costs.
//!
//! Resolution happens exactly once per property: a signal resolving to another `Signal` is an
//! error, not a second read. That guard is not a recursion bound -- a computed signal whose getter
//! returns fresh depth on every call recurses through `resolve_and_reconcile` and
//! `deserialize_lua_table` as deep as the getter wants. `layout::scene::MAX_TREE_DEPTH` is what caps
//! that and raises [`LayoutError::TreeTooDeep`].
//!
//! [`SurfaceTopology`]'s five fields and every node's optional `id` (docs/adr/0045 decision 1) are
//! structural carve-outs that keep rejecting a `Signal` outright -- see
//! [`reject_signal_in_structural_field`]. A `panel`'s remaining § 6.1 properties are not carve-outs:
//! layer-shell accepts each on a live surface, so a `Signal` in one resolves normally.

mod content;
mod paint_style;
mod spec;
mod style;
mod surface;
mod toplevel;

// The paint-only parsers are imported, not re-exported. They were `pub` for `layout::paint`, which
// ran them itself on every node on every frame; [`paint_style`] is their only caller now
// (docs/adr/0068), so the way to ask what a node paints is to ask for its `PaintStyle`. `super::*`
// is what carries them into `paint_style.rs`.
use content::{
    parse_elide, parse_fit, parse_font_size, parse_foreground, parse_icon_name, parse_image_source,
    parse_mask_character, parse_placeholder, parse_text_align,
};
use spec::parse_secure_submit;
use style::{parse_background, parse_border_color, parse_border_width, parse_radius};

pub use content::{Elide, TextAlign, parse_content, parse_icon_size, parse_node_id, parse_surface_id};
pub use paint_style::{PaintStyle, paint_style};
pub use spec::{
    SecureSubmitTarget, SurfaceFingerprint, SurfaceSpec, lock_spec, parse_children, parse_list_children,
    parse_single_child,
};
// `wayland::tests`' and `instance::tests`' fixtures name it as `node::LockSpec`, but nothing in
// this crate's non-test reachable set does.
#[cfg(test)]
pub use spec::LockSpec;
pub use style::{
    BorderColor, parse_align, parse_edge_insets, parse_list_direction, parse_opacity, parse_size_mode, parse_spacing,
    parse_visible,
};
pub use surface::{Anchor, KeyboardInteractivity, LayerKind, PanelSpec, SurfaceTopology, panel_spec};
pub use toplevel::{ConstraintAdjustment, PopupAnchor, PopupSpec, SizeHint, WindowSpec, popup_spec, window_spec};
// `wayland::tests`' fixtures name it as `node::PopupOffset`, same reason as `LockSpec` above.
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

/// A parsed colour, four channels in `0.0..=1.0`. `f32`, not `u8`: femtovg's `Color` (the drawing
/// pass's paint target, read by `layout::paint`) stores its channels as `f32` already --
/// `Color::rgbaf` takes them as-is, while `Color::rgba` takes `u8` and immediately divides by
/// 255.0 to reach that same `f32` form internally. Storing `f32` here means the drawing pass
/// copies four fields straight into `Color`, with no `u8` round-trip to undo.
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

/// `pub(crate)` rather than private since build-steps.md Phase 20 item 4: `layout::scene`'s
/// `Scene::apply_one_instance` raises an `id`-scoped error for an instance naming an undeclared
/// surface, and every other `InvalidProperty` in this crate is built here rather than by hand.
pub(crate) fn invalid(property: &str, detail: impl Into<String>) -> LayoutError {
    LayoutError::InvalidProperty { property: property.to_string(), detail: detail.into() }
}

/// Longest prefix of a rejected value's `Debug` form this file will ever put in an error message.
/// 200 bytes, not `marshal::MAX_STRING_BYTES` (64KB): that cap answers how much of a *string
/// property* is a legitimate value; this one answers how much of a rejected value belongs in one
/// line of `rescue`'s `error_log` (§ 2.10) -- enough bytes for a human to recognize the value, not
/// a paste buffer.
const MAX_ERROR_VALUE_PREVIEW_BYTES: usize = 200;

/// `pub(crate)` since `layout::scene`'s `list` node (build-steps.md Phase 19 item 12) rejects a bad
/// `source`/`itemfn`/`key` value from outside this module and needs the same bounded preview.
///
/// Renders a `Value` for an [`invalid`] detail without ever formatting its `Debug` form in full
/// first. `format!("{value:?}")` on an oversized `Value::String` allocates and escapes the whole
/// thing before any truncation could run -- `rect { radius = string.rep("x", 20 * 1024 * 1024) }`
/// would format 20 MB on the Wayland dispatch thread before the error even reaches `rescue`.
/// `marshal::check_string`'s 64KB cap never runs here: it lives in [`checked_string`], which a
/// value rejected for having the wrong *type* never reaches.
///
/// Measured on this machine, formatting a Lua string of each size against this function:
///
/// | size | `format!("{value:?}")` | this function |
/// |---|---|---|
/// | 1 MB | 1.64 ms | 0.0088 ms |
/// | 20 MB | 23.96 ms | 0.0057 ms |
/// | 100 MB | 93.88 ms | 0.0061 ms |
///
/// Cost here is a function of the cap, not of the input. 23.96 ms is more than a whole frame at
/// 60fps, which is what made this worth fixing while `layout::paint` still validated a `background`
/// or a `radius` while drawing. It no longer does (docs/adr/0068): every parser here runs once per
/// node per apply, so the cap now buys a bounded `rescue` message rather than a bounded frame.
///
/// `oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix` is the
/// regression test: a naive format-then-truncate still comes in under a loose timing bound, so what
/// it actually asserts on is that format-then-truncate cannot report the value's true length.
///
/// `Value::String` is the only unbounded `Debug` case in mlua 0.12: every other variant that can
/// hold non-trivial data (`Table`, `Function`, `Thread`, `UserData`) derives or hand-writes a
/// `Debug` over a `ValueRef`, whose own `Debug` is a fixed-width pointer regardless of size.
pub(crate) fn preview_for_error(value: &Value) -> String {
    let Value::String(s) = value else {
        return format!("{value:?}");
    };
    // `as_bytes()` borrows the Lua string's own buffer -- no copy, no escaping -- so measuring its
    // length is the O(1) check that has to run *before* any formatting decision, not after.
    let bytes = s.as_bytes();
    let total_len = bytes.len();
    if total_len <= MAX_ERROR_VALUE_PREVIEW_BYTES {
        return format!("{value:?}");
    }
    // Slice first, format second: only the bounded prefix is ever handed to a `Debug`-style
    // formatter, so a 20 MB string costs this function O(200 bytes), not O(len). The prefix is
    // rendered lossily on purpose -- this is a log preview, not an equality key, and slicing raw
    // bytes at a fixed offset can land mid-codepoint.
    let prefix = String::from_utf8_lossy(&bytes[..MAX_ERROR_VALUE_PREVIEW_BYTES]);
    format!(
        "String({prefix:?}...) -- {total_len} bytes total, truncated to the first {MAX_ERROR_VALUE_PREVIEW_BYTES} here"
    )
}

/// Runs a numeric `Value` through the marshalling boundary (`lua::marshal`, ADR-0044 decision 1)
/// before this parser's own range checks (e.g. `parse_size_mode`'s `[0, 8192]`) ever see it --
/// catches a NaN/Inf `f64` or an out-of-2^53-range `Integer`, whether literal or resolved from a
/// `Signal` via [`resolve_properties`].
///
/// `marshal::check_number` alone is not enough: it only guards the `f64` representation, and a
/// finite `f64` like `1e300` sails through it and then overflows to `f32::INFINITY` on the
/// narrowing cast below. A caller with no further range check (e.g. `parse_spacing`) would hand
/// that `Inf` straight into layout arithmetic -- `inf * 0.0` is `NaN`, and `snap_to_physical`'s
/// final `as i32` silently saturates a `NaN` rect to `0` instead of raising an error. So the
/// finiteness check re-runs after the cast, on the `f32`, naming the same property a non-finite
/// literal would.
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

/// Strict `#RRGGBB` / `#RRGGBBAA` hex colour parsing (§ 5.2's `rect.background`, `border_color`,
/// `text.foreground`). No 3-digit shorthand, no named colours, no bare digits without `#` -- § 5.2
/// specifies none of them.
fn parse_hex_color(property: &str, s: &str) -> Result<Rgba, LayoutError> {
    let Some(digits) = s.strip_prefix('#') else {
        return Err(invalid(property, format!("hex colour must start with `#`, got `{s}`")));
    };
    // Digit check before length check, and in that order deliberately: every byte of a multi-byte
    // UTF-8 sequence is >= 0x80 and so fails `is_ascii_hexdigit`, so non-ASCII input (`"#日本語"`)
    // is caught here with the accurate diagnosis. By the time the length check below runs, the
    // string is known to be pure ASCII, where `.len()` is the character count -- reporting a byte
    // count for `"#日本語"` (9 bytes, 3 characters) would name the wrong number entirely.
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
/// refuse it. Resolving then rejecting afterwards is unimplementable: once a signal has been read,
/// the value in the map is indistinguishable from a literal.
///
/// Kind-aware, because a skip is only sound where a parser actually runs to do the rejecting. `id`
/// is skipped on every kind ([`parse_node_id`]/[`parse_surface_id`] read it wherever it appears).
/// `layer`/`anchor`/`monitor`/`namespace` are read by `surface::surface_topology` alone, called only on
/// top-level surfaces -- so on a `rect` no parser looks at them, and skipping them there would copy
/// a live `Signal` handle into `layout::scene::RetainedNode::properties`, falsifying the "never a
/// `Signal`" invariant `ResolvedNode::properties` hands to the paint stage. Below a surface these
/// four are ordinary properties and resolve like any other.
///
/// `namespace` joins the carve-out for the same protocol reason `monitor` is already in it:
/// `zwlr_layer_shell_v1::get_layer_surface` fixes a namespace at creation and no request changes it
/// on a live surface. The in-place `panel` fields -- `keyboard_interactivity`, `exclusive`,
/// `margin`, `width`/`height` -- are deliberately *not* here: layer-shell permits changing each on a
/// live surface, so a `Signal` in one resolves normally (docs/adr/0044 decision 1).
///
/// `window`, `popup` and `lock` add nothing to the carve-out (build-steps.md Phase 22 and 23), by
/// the same live-object test: a `window`'s `set_title`/`set_app_id`/`set_min_size`/`set_max_size`
/// are all valid requests on a mapped toplevel; a `popup`'s whole `xdg_positioner` is rebuilt on
/// every open (docs/adr/0049 decision 1), so `parent`/`anchor_rect`/`anchor`/`gravity` are meant to
/// carry a `Signal`; a `lock`'s § 6.4 property list is only `id` and `child`. All three roles' `id`
/// is already covered by the universal arm, since it is a reconcile identity rather than a protocol
/// field.
///
/// `hover` joins `id` in the universal arm for the same reason and on any kind (docs/adr/0062
/// decision 3): it names the signal the pointer handler writes, and a resolved `hover` would arrive
/// there as the boolean `false`, saying nothing about *which* signal that is. Structural properties
/// are identities, and identities do not resolve.
fn is_structural_property(kind: &str, property: &str) -> bool {
    property == "id"
        || property == "hover"
        // docs/adr/0069 decision 4: the positioning pass reads this signal's number *and* writes
        // the clamped one back, so it needs the handle rather than a snapshot of it.
        || property == "scroll"
        || (kind == "panel" && matches!(property, "layer" | "anchor" | "monitor" | "namespace"))
}

/// One node's raw property map with every `Signal` replaced by its current value (ADR-0044
/// decision 1). Called once per node per pass, at the point that node enters reconciliation;
/// everything downstream -- this module's parsers, `layout::scene`'s sizing/positioning passes, and
/// `RetainedNode::properties` -- reads the result rather than the raw map.
///
/// Once, and once is load-bearing. `Signal::get_value` runs a `computed` signal's Lua closure, and
/// a closure that is not a pure function of unchanged state (`os.clock()`, `math.random`, an
/// accumulator upvalue) answers differently on every call. `margin` used to be read four separate
/// times in one `Scene::apply`, so a row could be measured against one answer and position its
/// child against another. One read per property makes the resolved tree a snapshot of one pass, and
/// stops ADR-0021's per-`get_value` 5ms budget being paid four times over for one property.
///
/// The snapshot is a snapshot of the *signals*, and only of them. A plain table with an `__index`
/// metamethod is copied through as that table, and each `table.get` a parser makes runs the
/// metamethod again, so a `margin` of that shape still measures against one answer and positions
/// against another. See [`parse_edge_insets`]'s `ponytail:`.
///
/// Not the memoization ADR-0044 decision 3 rejects: that decision is about caching *across* pushes,
/// which needs an invalidation rule no push has; this caches nothing beyond the single pass it runs
/// in.
///
/// Per entry:
///
/// - a key [`is_structural_property`] names for this node's `kind` is copied through raw, signal
///   and all;
/// - a `Value::UserData` holding a `Signal` is read through `Signal::get_value` and the *result*
///   stored in its place, under the same rules a parser would apply to a literal;
/// - a result that is itself a `Signal` is an error naming the property, not a second read:
///   chasing it to a fixed point is an unbounded loop on a cyclic construction;
/// - a result of `Value::Nil` **omits the key entirely** -- ADR-0044 decision 1's amendment ("a
///   signal resolving to nil means the property is absent") falls out of the map itself rather
///   than being re-checked in every parser. This matters at boot:
///   `RendererClient::run_startup_evaluation` runs before the poll loop has drained a single
///   inbound frame, so every `shared::CAPABILITIES` signal still reads `nil` at the first
///   `Scene::apply`, and a config binding a bare capability signal must not fail layout there. It
///   also matches a Lua table's own inability to store `nil`, so `visible = nil` in a config and a
///   signal resolving to nil read the same;
/// - everything else, including a `UserData` that is not a `Signal`, is copied through unchanged,
///   left for whichever parser consumes it to reject with its own message.
///
/// Every property resolves, including ones no parser reads today: the resolved map is what the
/// paint stage reads a colour or radius straight off (`ResolvedNode::properties`), and § 5.1 puts
/// no property out of a `Signal`'s reach, so there is no subset safe to skip. A getter that raises
/// fails the whole apply, even for a property nothing downstream looked at -- deferring the error
/// would mean keeping the getter around to re-run later, which is the second read this function
/// exists to prevent.
///
/// ponytail: one fresh `HashMap` per node per pass, its `String` keys cloned with it. Honestly
/// counted, that is not a new allocation: the code before this built exactly one map per node per
/// pass too, cloning `fresh.properties` into the `RetainedNode`, and the map built here is *moved*
/// into that node rather than cloned again. What stands as the ceiling is that neither shape
/// reuses the retained node's existing allocation across passes, and that costs more now than it
/// used to, because ADR-0044 decision 2's dirty flag makes a pass a per-push event rather than a
/// per-config-edit one, on the Wayland dispatch thread. Resolving in place over the retained map
/// is the upgrade path; it needs the reconcile match to happen before resolution rather than
/// after, which is a reordering not worth doing until a profile names this.
///
/// ponytail: resolving every property means every property *that holds a `Signal`* is evaluated on
/// every pass, paint-only ones included -- `background`, `color` and `radius` are read here even
/// though no parser in this module looks at them yet. Each one is its own `Signal::get_value` and
/// therefore buys its own ADR-0021 5ms budget (docs/adr/0021, § 1.2), so a node with four
/// signal-bound paint properties can spend four budgets in a pass that ADR-0044 decision 2's dirty
/// flag now runs per capability push, on the Wayland dispatch thread. That is the price of the
/// resolved map being a *complete* snapshot rather than a snapshot of the properties layout
/// happens to consume; the alternative, resolving only what a parser asks for, is what item 5 was
/// written to end. Charging one budget per pass instead of one per property needs the whole-pass
/// budget noted in [`parse_edge_insets`]'s `ponytail:`, not a smaller resolve.
pub fn resolve_properties(
    properties: &HashMap<String, Value>,
    kind: &str,
    lua: &Lua,
) -> Result<HashMap<String, Value>, LayoutError> {
    let mut resolved = HashMap::with_capacity(properties.len());
    // Sorted, and the sort is the point: `properties` is a `HashMap` whose iteration order is
    // randomised per process, so with two failing properties on one node the error a run reported
    // was whichever the hash seed happened to reach first -- eight runs of the same broken config
    // named two different properties. This message is what `renderer/src/socket.rs` puts in the
    // `rescue` global's `error_log` for a human to read after the fact (§ 2.10, docs/adr/0024), so
    // which property a given broken config names has to be a function of the config alone, not of
    // the hash seed the process happened to boot with. Do not "optimise" this into a bare
    // `for (property, value) in properties`.
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
    Ok(resolved)
}

/// The carve-outs from decision 1's "parsers resolve a `Signal`" rule: [`SurfaceTopology`]'s five
/// fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/`parse_monitor`/`parse_namespace`) and
/// every node's optional `id` ([`parse_node_id`], docs/adr/0045 decision 1) keep rejecting one
/// outright, the same way every parser used to.
///
/// The unifying reason: each is read exactly once per evaluation and a *structural* decision is
/// then made from it and acted on -- where a surface is placed, or which retained node a fresh one
/// is. A `Signal` is free to change between passes, so admitting one here would leave a decision
/// already taken resting on a value that no longer holds, with nothing left to re-check it. Every
/// other property is read for the geometry or appearance of the pass it was read in, so a later
/// change simply produces different output on the next pass.
///
/// Concretely: `surface_topology` runs on every `Scene::apply` so `socket.rs`'s `handle_reevaluate`
/// can diff it against `applied_topology` and choose swap-versus-in-place (ADR-0001) -- a surface
/// could otherwise move layer or monitor with no swap. `id` is `pair_children_by_id_then_position`'s
/// reconcile identity, matched once per `Scene::apply` to pair a fresh child against its retained
/// counterpart -- a value that could change between the match and whatever reads it afterward would
/// make "the same node as last time" itself ambiguous. ADR-0044 decision 1 doesn't carve either out
/// explicitly; it's a gap in the ADR, not a case it considered and rejected.
///
/// This only works because [`resolve_properties`] copies the keys [`is_structural_property`] names
/// through raw: these six parsers are the only ones that read the un-resolved value, since a
/// resolved signal is indistinguishable from a literal by the time it reaches a map.
fn reject_signal_in_structural_field(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

/// Whether `property` currently holds a live [`crate::lua::signal::Signal`] -- the one thing an **unresolved** property
/// map can say that a resolved one cannot: "this pass is not in a position to check it"
/// (docs/adr/0049's second amendment).
///
/// Only ever true on the evaluation-time pass. [`resolve_properties`] reads every `Signal` it is
/// handed and stores the *result* in its place, refusing a result that is itself a `Signal`, so no
/// map that has been through it can hold one -- except under a key [`is_structural_property`]
/// copies through raw, and those keys are exactly the ones
/// [`reject_signal_in_structural_field`] refuses outright instead of deferring.
///
/// A parser that consults this is applying the amendment's split: on the pass that reads raw
/// properties (`crate::socket`'s `surface_specs`, which runs before any getter has been called and
/// must not call one), a signal-bound property is skipped and left at the parser's documented
/// placeholder; the authoritative value is re-read from the resolved tree by
/// `App::apply_resolved_state` before anything is built from it. A *literal* is still fully
/// validated on that pass, so a config typo fails fast into docs/adr/0046's `rescue` log rather than
/// surfacing as an `xdg_positioner` protocol error at first open.
///
/// Without this, `anchor_rect = popup_anchor` -- the spelling docs/adr/0050 decision 3 tells a
/// config to write -- would fail the whole evaluation: every parser below rejects a raw
/// `Value::UserData` with a type error otherwise.
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
    /// `shared::CAPABILITIES` at `Value::Nil`), which is what a config binding a bare capability
    /// signal resolves at startup. Routed through [`resolve_properties`] because that is where the
    /// nil rule now lives: the key is omitted from the resolved map rather than each parser
    /// checking for a `Value::Nil` of its own (build-steps.md Phase 19 item 5).
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
        assert_eq!(parse_content(&props_with_nil_signal(&lua, "text", "content")).unwrap(), "");
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

    #[test]
    fn two_failing_properties_always_report_the_same_one() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                    return {
                        kind = "rect",
                        alpha = computed({}, function() error("alpha boom") end),
                        beta = computed({}, function() error("beta boom") end),
                    }
                    "#,
            )
            .eval()
            .unwrap();
        for _ in 0..8 {
            let props = props_from_table(&table);
            let err = resolve_properties(&props, "rect", &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "alpha"),
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
