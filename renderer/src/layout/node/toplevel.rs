//! `xdg_toplevel` and `xdg_positioner` specs: [`WindowSpec`] (§ 6.2, title/app_id/min_size/max_size)
//! and [`PopupSpec`] (§ 6.3, anchor/gravity/constraint_adjustment/offset/grab), plus every parser
//! that builds one field of either. Grouped together because both describe a live, positioned
//! `xdg_shell` object rather than a layer-shell surface or the session lock.
//!
//! `parse_size_hint`, `check_max_size_above_min`, `DEFERRED_POPUP_EXTENT` and `parse_popup_extent`
//! stay private: nothing outside this file needs a half-built size hint or a raw popup extent.

use std::collections::HashMap;

use mlua::Value;

use crate::text::snap::LogicalRect;

use super::content::parse_string_property;
use super::style::table_number;
use super::*;

/// § 6.2's `title`, the string the compositor shows in a task bar or window list. Absent is the
/// empty string, which is exactly what a toplevel that never sends `set_title` has: § 6.2
/// documents no default, and substituting the `id` would put an internal identifier in the user's
/// task switcher.
///
/// In-place, deliberately outside [`is_structural_property`]'s carve-out: `xdg_toplevel::set_title`
/// is a request on a live toplevel, so a `Signal` here resolves like any other property
/// (ADR-0044 decision 1, and § 6.2 spells the `string`/`Signal` union out) and the next pass
/// simply sends the new title.
pub fn parse_title(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    // Deferred rather than rejected on the evaluation-time pass ([`is_deferred_signal`]): § 6.2
    // spells `title` as `string`/`Signal`, and `parse_string_property`'s `Signal` refusal is meant
    // for the topology fields it also backs. Same placeholder an absent `title` gets, and for the
    // same reason -- a toplevel that never sends `set_title` has no title -- since `show_window`
    // builds the real one from the resolved spec.
    if is_deferred_signal(properties, "title") {
        return Ok(String::new());
    }
    parse_string_property(properties, "title", Some(""))
}

/// § 6.2's `app_id`, the string a compositor matches its own window rules against. Defaults to
/// `"oblisk-{id}"` for exactly the reason `surface::parse_namespace` defaults the same way: it is the
/// toplevel's half of the same problem, and without a default nobody could write a `windowrule`
/// against their own window without naming an app id by hand.
///
/// **Not** an [`is_structural_property`] carve-out, and the protocol decides that: `xdg-shell.xml`'s
/// own `set_app_id` description says a request "can be sent after the xdg_toplevel has been mapped
/// to update the property" -- it changes on a live object, the test `keyboard_interactivity` passes
/// and `namespace` fails (`get_layer_surface` fixes a namespace at creation; `set_app_id` fixes
/// nothing). A `window`'s `id` is a carve-out on every kind regardless: it is the reconcile
/// identity, not a protocol field (ADR-0045 decision 1).
pub fn parse_app_id(properties: &HashMap<String, Value>, id: &str) -> Result<String, LayoutError> {
    let default = format!("oblisk-{id}");
    // Deferred on the evaluation-time pass for [`parse_title`]'s reason: `set_app_id` is a request
    // on a live toplevel, so § 5.1's blanket "any property accepts a `Signal`" applies and only
    // this pass is unable to read it.
    if is_deferred_signal(properties, "app_id") {
        return Ok(default);
    }
    parse_string_property(properties, "app_id", Some(&default))
}

/// § 6.2's `min_size`/`max_size` value, `{ width, height }`. Advisory, in the spec's own words and
/// the protocol's: "The client should not rely on the compositor to obey the maximum size." So
/// nothing in `layout` clamps a resolved tree against these -- they are carried to
/// `set_min_size`/`set_max_size` and no further.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeHint {
    pub width: f32,
    pub height: f32,
}

/// `Ok(None)` when the property is absent, which is not the same as `Some(0, 0)`: absent means the
/// engine never sends the request at all, while a zero means "no expected maximum size in the given
/// dimension" in a request that *was* sent.
///
/// Both axes are required when the table is present. A `min_size` naming only a width is a config
/// typo, not a request to leave the height unconstrained -- that spelling is an explicit `0`.
fn parse_size_hint(properties: &HashMap<String, Value>, property: &str) -> Result<Option<SizeHint>, LayoutError> {
    // Deferred on the evaluation-time pass, and `None` is the honest placeholder: the request is
    // simply not sent from a spec built there, and `show_window` sends the resolved one.
    if is_deferred_signal(properties, property) {
        return Ok(None);
    }
    let Some(value) = properties.get(property) else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            property,
            format!("expected a `{{ width, height }}` table, got {}", preview_for_error(value)),
        ));
    };
    let axis = |key: &str| -> Result<f32, LayoutError> {
        let n = table_number(property, table, key)?.ok_or_else(|| {
            invalid(
                property,
                format!("`{key}` is required -- a size hint names both axes, or use 0 for an unconstrained one"),
            )
        })?;
        // Negative is the one value the requests refuse outright ("Using strictly negative values
        // for width or height will result in an invalid_size error"); the upper end is § 5.1's own
        // `[0, 8192]`, already enforced by `parse_size_mode` for the same quantity on the same node.
        if !(0.0..=8192.0).contains(&n) {
            return Err(invalid(property, format!("`{key}` must be within [0, 8192], got {n}")));
        }
        Ok(n)
    };
    Ok(Some(SizeHint { width: axis("width")?, height: axis("height")? }))
}

/// `set_max_size`: "Requesting a maximum size to be smaller than the minimum size of a surface is
/// illegal and will result in an invalid_size error." Zero means unset in that request, so a zero
/// maximum is not a maximum below the minimum.
///
/// Checked here, per axis, so a config typo is a [`LayoutError`] a human reads out of `rescue`'s
/// `error_log` (§ 2.10) rather than a protocol error that takes the Wayland connection -- and the
/// whole shell -- down with it. Same call build-steps.md Phase 20's `ambiguous_zero_axis` made for
/// a singly-anchored layer surface's `set_size(0)`.
fn check_max_size_above_min(min: Option<SizeHint>, max: Option<SizeHint>) -> Result<(), LayoutError> {
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(());
    };
    for (axis, min_n, max_n) in [("width", min.width, max.width), ("height", min.height, max.height)] {
        if max_n > 0.0 && max_n < min_n {
            return Err(invalid(
                "max_size",
                format!(
                    "`{axis}` is {max_n}, below `min_size`'s {min_n} -- a maximum under the minimum raises xdg_toplevel's invalid_size"
                ),
            ));
        }
    }
    Ok(())
}

/// Everything one `xdg_toplevel` needs, read off a `window` node's properties in one pass (§ 6.2,
/// build-steps.md Phase 22), the way [`PanelSpec`] does for a layer surface.
///
/// A `window` is a **top-level** node returned from `shell.lua`, a sibling of `panel`, not
/// something nested inside a panel's child tree (ADR-0040 decision 1).
///
/// **No `WindowTopology`.** [`PanelSpec`] carries one because five of a panel's fields are fixed at
/// `get_layer_surface` time. A toplevel's are not: `set_title`, `set_app_id`, `set_min_size` and
/// `set_max_size` are all requests on a live toplevel, and `visible` creates and destroys the
/// object rather than swapping the generation (ADR-0049 decisions 1-3). What is left is `id`,
/// and an `id` changing is adding one declaration and removing another, which ADR-0001 and
/// ADR-0049 decision 3 already route to a swap on the *declared set*.
///
/// § 6.2 gives a `window` no `monitor`: the compositor places a toplevel, so unlike a `panel` one
/// declaration is one Wayland object, never one per output (ADR-0038 decision 3).
///
/// `on_close` and `visible` are absent here for the same reasons [`PanelSpec`] omits their
/// equivalents. A callback rides along untouched in `layout::scene::RetainedNode::properties`,
/// exactly as `button`'s `on_click` does and where ADR-0050's input path reads it; `visible`
/// is § 5.1 base state on every node, parsed by [`parse_visible`] in the reconcile walk.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// Advisory (§ 6.2): parsed and carried, never enforced against the resolved tree.
    pub min_size: Option<SizeHint>,
    pub max_size: Option<SizeHint>,
}

pub fn window_spec(properties: &HashMap<String, Value>) -> Result<WindowSpec, LayoutError> {
    let id = parse_surface_id(properties)?;
    let app_id = parse_app_id(properties, &id)?;
    let min_size = parse_size_hint(properties, "min_size")?;
    let max_size = parse_size_hint(properties, "max_size")?;
    check_max_size_above_min(min_size, max_size)?;
    Ok(WindowSpec { id, title: parse_title(properties)?, app_id, min_size, max_size })
}

/// § 6.3's `anchor` and `gravity`, which share one value set. `layout`'s own enum rather than
/// `xdg_positioner`'s `Anchor`/`Gravity`, holding the same deliberate boundary [`LayerKind`] and
/// [`KeyboardInteractivity`] already hold: this module carries no Wayland types, and
/// `crate::wayland` maps at the single call site that builds a positioner.
///
/// `Center` is the one value with no entry of its own in the protocol enums, which run
/// `none`(0) / `top` / `bottom` / `left` / `right` and the four corners. It maps to `none`, and the
/// XML says why that is a translation rather than a fudge: with no edge specified the anchor point
/// is "in the center of the anchor rectangle", and a gravity of `none` centers the surface "over
/// the anchor point on any axis that had no gravity specified". § 6.3's `"Center"` is that
/// behaviour under a name a config author can guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupAnchor {
    /// The protocol's `none`, which is also its default for both requests.
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// One parser, two callers: § 6.3 gives `anchor` and `gravity` the same value set and the protocol
/// gives them identical enums, so `property` exists only to name which one a config got wrong.
///
/// Absent defaults to [`PopupAnchor::Center`], the protocol's own default rather than a choice
/// invented here -- unlike [`parse_constraint_adjustment`], where § 6.3 departs from the protocol
/// default on purpose.
pub fn parse_popup_anchor(properties: &HashMap<String, Value>, property: &str) -> Result<PopupAnchor, LayoutError> {
    if is_deferred_signal(properties, property) {
        return Ok(PopupAnchor::Center);
    }
    let Some(value) = properties.get(property) else {
        return Ok(PopupAnchor::Center);
    };
    let Value::String(s) = value else {
        return Err(invalid(property, format!("expected a string, got {}", preview_for_error(value))));
    };
    match checked_string(property, s)?.as_str() {
        "Top" => Ok(PopupAnchor::Top),
        "Bottom" => Ok(PopupAnchor::Bottom),
        "Left" => Ok(PopupAnchor::Left),
        "Right" => Ok(PopupAnchor::Right),
        "TopLeft" => Ok(PopupAnchor::TopLeft),
        "TopRight" => Ok(PopupAnchor::TopRight),
        "BottomLeft" => Ok(PopupAnchor::BottomLeft),
        "BottomRight" => Ok(PopupAnchor::BottomRight),
        "Center" => Ok(PopupAnchor::Center),
        other => Err(invalid(
            property,
            format!(
                "unknown popup anchor `{other}` -- expected \"Top\", \"Bottom\", \"Left\", \"Right\", \"TopLeft\", \"TopRight\", \"BottomLeft\", \"BottomRight\", or \"Center\""
            ),
        )),
    }
}

/// § 6.3's `constraint_adjustment`, as a set of six independent permissions rather than as the
/// array a config writes it as. The array's *order* carries no meaning and this type must not let
/// it look as though it does: the XML fixes the precedence inside the compositor ("The adjustments
/// can be combined, according to a defined precedence: 1) Flip, 2) Slide, 3) Resize") and the
/// request itself takes a bitmask, so `{ "SlideX", "FlipY" }` and `{ "FlipY", "SlideX" }` are one
/// value. Six booleans say that. A `Vec` of adjustments would preserve an ordering the protocol
/// discards and invite a reader to think the config chose the precedence.
///
/// [`Default`] is § 6.3's `{ "FlipY", "SlideX" }` -- dropdown behaviour, and deliberately **not**
/// the protocol's own default of no adjustment at all (ADR-0040 decision 3). An explicitly
/// empty array is how a config asks for [`ConstraintAdjustment::NONE`] back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstraintAdjustment {
    pub slide_x: bool,
    pub slide_y: bool,
    pub flip_x: bool,
    pub flip_y: bool,
    pub resize_x: bool,
    pub resize_y: bool,
}

impl ConstraintAdjustment {
    /// The protocol's own default: never move the popup, even when it falls off-screen. What an
    /// explicitly empty array parses to, and the base [`parse_constraint_adjustment`] accumulates
    /// named entries onto.
    pub const NONE: Self =
        Self { slide_x: false, slide_y: false, flip_x: false, flip_y: false, resize_x: false, resize_y: false };
}

impl Default for ConstraintAdjustment {
    fn default() -> Self {
        Self { flip_y: true, slide_x: true, ..Self::NONE }
    }
}

pub fn parse_constraint_adjustment(properties: &HashMap<String, Value>) -> Result<ConstraintAdjustment, LayoutError> {
    if is_deferred_signal(properties, "constraint_adjustment") {
        return Ok(ConstraintAdjustment::default());
    }
    let Some(value) = properties.get("constraint_adjustment") else {
        return Ok(ConstraintAdjustment::default());
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            "constraint_adjustment",
            format!("expected an array table, got {}", preview_for_error(value)),
        ));
    };
    let mut adjustment = ConstraintAdjustment::NONE;
    for entry in table.sequence_values::<Value>() {
        let entry = entry.map_err(|e| invalid("constraint_adjustment", e.to_string()))?;
        let Value::String(s) = entry else {
            return Err(invalid(
                "constraint_adjustment",
                format!("expected a string entry, got {}", preview_for_error(&entry)),
            ));
        };
        // Setting a flag rather than pushing: a repeat is a no-op and the position in the array is
        // never read, which is the type's whole point.
        match checked_string("constraint_adjustment", &s)?.as_str() {
            "SlideX" => adjustment.slide_x = true,
            "SlideY" => adjustment.slide_y = true,
            "FlipX" => adjustment.flip_x = true,
            "FlipY" => adjustment.flip_y = true,
            "ResizeX" => adjustment.resize_x = true,
            "ResizeY" => adjustment.resize_y = true,
            other => {
                return Err(invalid(
                    "constraint_adjustment",
                    format!(
                        "unknown adjustment `{other}` -- expected \"SlideX\", \"SlideY\", \"FlipX\", \"FlipY\", \"ResizeX\", or \"ResizeY\""
                    ),
                ));
            }
        }
    }
    Ok(adjustment)
}

/// § 6.3's `offset`, the pixel nudge applied after anchor and gravity. Signed on purpose: the XML's
/// worked example adds the offset to the derived anchor point, so pulling a dropdown up or left
/// needs a negative and `set_offset` refuses neither sign.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PopupOffset {
    pub x: f32,
    pub y: f32,
}

pub fn parse_popup_offset(properties: &HashMap<String, Value>) -> Result<PopupOffset, LayoutError> {
    if is_deferred_signal(properties, "offset") {
        return Ok(PopupOffset::default());
    }
    let Some(value) = properties.get("offset") else {
        return Ok(PopupOffset::default());
    };
    let Value::Table(table) = value else {
        return Err(invalid("offset", format!("expected an `{{ x, y }}` table, got {}", preview_for_error(value))));
    };
    let axis = |key: &str| -> Result<f32, LayoutError> { Ok(table_number("offset", table, key)?.unwrap_or(0.0)) };
    Ok(PopupOffset { x: axis("x")?, y: axis("y")? })
}

/// § 6.3's `anchor_rect`, in the parent surface's logical coordinates. [`LogicalRect`] rather than
/// a type of its own: `crate::wayland`'s `rect_table` builds `on_click`'s single argument out of a
/// `LogicalRect` (ADR-0050 decision 3), and § 6.3 says this property is "normally passed
/// straight from the rect `button`'s `on_click` hands back", so the same rect round-trips through
/// the config and lands back in the type it left as.
///
/// Required. `x`/`y` default to 0 when the table omits them -- an origin at the parent's own
/// top-left corner is a legitimate rect -- but `width`/`height` do not, because a zero size is
/// precisely the failure this parser exists to catch. Two different protocol errors sit behind
/// that: `set_anchor_rect` itself raises `invalid_input` only on a *negative* size, while a *zero*
/// size leaves the positioner incomplete ("must have a non-zero size set by set_size, and a
/// non-zero anchor rectangle set by set_anchor_rect"), which raises `invalid_positioner` later, at
/// `get_popup`. Either way one config typo would take the Wayland connection down for the whole
/// shell, so both are a [`LayoutError`] here.
///
/// What [`parse_anchor_rect`] and [`parse_popup_extent`] answer for a property whose value is a
/// live `Signal`, which only the evaluation-time pass ever sees ([`is_deferred_signal`]). One
/// logical pixel rather than zero: `xdg_positioner`'s own description calls a zero size or a zero
/// anchor rectangle incomplete, and a placeholder that would itself be a protocol error is not a
/// placeholder.
///
/// It never reaches a compositor: ADR-0049's second amendment re-derives the authoritative
/// spec from the resolved tree in `App::apply_resolved_state`, before `apply_visibility` can create
/// anything from it. The one place it is observable is `expand_instances`, which seeds a popup
/// instance's `available` from the declared `width`/`height` -- a popup that signal-binds a size
/// measures its child against 1x1 until its first configure replaces `available`, and it is not on
/// screen before then, since no `xdg_popup` exists until `visible` resolves true.
const DEFERRED_POPUP_EXTENT: f32 = 1.0;

pub fn parse_anchor_rect(properties: &HashMap<String, Value>) -> Result<LogicalRect, LayoutError> {
    if is_deferred_signal(properties, "anchor_rect") {
        return Ok(LogicalRect { x: 0.0, y: 0.0, width: DEFERRED_POPUP_EXTENT, height: DEFERRED_POPUP_EXTENT });
    }
    let value = properties.get("anchor_rect").ok_or_else(|| {
        invalid(
            "anchor_rect",
            "required for `popup`, got nothing -- a popup with no anchor rectangle raises invalid_positioner at get_popup",
        )
    })?;
    let Value::Table(table) = value else {
        return Err(invalid(
            "anchor_rect",
            format!("expected an `{{ x, y, width, height }}` table, got {}", preview_for_error(value)),
        ));
    };
    let origin =
        |key: &str| -> Result<f32, LayoutError> { Ok(table_number("anchor_rect", table, key)?.unwrap_or(0.0)) };
    let extent = |key: &str| -> Result<f32, LayoutError> {
        let n = table_number("anchor_rect", table, key)?.ok_or_else(|| {
            invalid("anchor_rect", format!("`{key}` is required and must be greater than 0 -- a zero-size anchor rectangle raises invalid_positioner"))
        })?;
        if !(n > 0.0 && n <= 8192.0) {
            return Err(invalid(
                "anchor_rect",
                format!(
                    "`{key}` must be within (0, 8192], got {n} -- a zero or negative anchor rectangle size is a protocol error"
                ),
            ));
        }
        Ok(n)
    };
    Ok(LogicalRect { x: origin("x")?, y: origin("y")?, width: extent("width")?, height: extent("height")? })
}

/// § 6.3's `width`/`height`. **Not** [`parse_size_mode`], and that is the whole reason this is a
/// separate parser: a popup has no `"Fill"` because there is nothing for it to fill. An
/// `xdg_popup` is sized by `xdg_positioner::set_size`, which takes a concrete int and "If a zero or
/// negative size is set the invalid_input error is raised", so `"Fill"`, a percent, a `0`, and an
/// omitted axis are four config typos that would otherwise reach the compositor as a protocol
/// error rather than as a rejected config.
///
/// `f32` rather than `i32` even though § 6.3 and the request both say integer: this is the popup's
/// logical size and becomes its child's layout budget, which is `LogicalSize`'s `f32` like every
/// other budget in this engine. It quantizes once, at the `set_size` call site that also knows the
/// output scale -- the same split [`PanelSpec`]'s `width` already documents for layer-shell.
fn parse_popup_extent(properties: &HashMap<String, Value>, property: &str) -> Result<f32, LayoutError> {
    if is_deferred_signal(properties, property) {
        return Ok(DEFERRED_POPUP_EXTENT);
    }
    let value = properties.get(property).ok_or_else(|| {
        invalid(property, "required for `popup`, got nothing -- a popup has no \"Fill\", and set_size raises invalid_input on a zero size")
    })?;
    let n = value_as_f32(property, value)?.ok_or_else(|| {
        invalid(property, format!("expected a number, got {} -- a popup has no \"Fill\"", preview_for_error(value)))
    })?;
    if !(n > 0.0 && n <= 8192.0) {
        return Err(invalid(
            property,
            format!("must be within (0, 8192], got {n} -- set_size raises invalid_input on a zero or negative size"),
        ));
    }
    Ok(n)
}

/// § 6.3's `grab`, defaulting to `true`. A dropdown that cannot be dismissed by clicking outside it
/// is the whole reason ADR-0040 decision 2 reached for a real `xdg_popup` instead of a second
/// `panel`, so the default is the behaviour a config author expects rather than the protocol's
/// "only if you ask".
///
/// Whether the grab can actually be taken is not decided here: it needs a serial from a real input
/// event, which only exists for the length of one poll turn (ADR-0049's amendment), and a
/// compositor may deny it anyway, which ADR-0040 decision 2 records as a normal outcome.
pub fn parse_grab(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    if is_deferred_signal(properties, "grab") {
        return Ok(true);
    }
    let Some(value) = properties.get("grab") else {
        return Ok(true);
    };
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(invalid("grab", format!("expected a boolean, got {}", preview_for_error(other)))),
    }
}

/// Everything one `xdg_popup` and its `xdg_positioner` need, read off a `popup` node's properties
/// in one pass (§ 6.3, build-steps.md Phase 22).
///
/// A `popup` is a **top-level** node returned from `shell.lua`, a sibling of `panel`, and it names
/// its parent surface by id through `parent` rather than sitting inside that surface's child tree.
/// The word "popup" suggests otherwise, which is why it is said here: the protocol roots a popup
/// under a parent at creation (`xdg_surface.get_popup`, or `zwlr_layer_surface_v1.get_popup` for a
/// layer-shell parent), and that parent is a *surface*, not a node.
///
/// **No `PopupTopology`, for a stronger reason than [`WindowSpec`] has.** A popup's Wayland object
/// exists only while it is shown (ADR-0049 decision 1) and its `xdg_positioner` is consumed by
/// `get_popup`, so every field on this type is re-read from scratch on every open -- `parent`
/// included, which is why it is an ordinary field and not a carve-out. What remains topology is the
/// declaration itself: adding or removing a `popup` node changes the declared set (ADR-0001,
/// ADR-0049 decision 3), while opening and closing one is explicitly a value change.
///
/// `on_dismiss` and `visible` are not fields here, same as [`WindowSpec`] and [`PanelSpec`]: the
/// callback rides along in `RetainedNode::properties` like `on_click`, and `visible` is § 5.1 base
/// state read by [`parse_visible`] during the reconcile walk.
#[derive(Debug, Clone, PartialEq)]
pub struct PopupSpec {
    pub id: String,
    /// The `id` of the `panel` or `window` this popup anchors to. Read with the same lossy
    /// conversion [`parse_surface_id`] uses on the other side of the match, so the two agree.
    pub parent: String,
    pub anchor_rect: LogicalRect,
    pub width: f32,
    pub height: f32,
    pub anchor: PopupAnchor,
    pub gravity: PopupAnchor,
    pub constraint_adjustment: ConstraintAdjustment,
    pub offset: PopupOffset,
    pub grab: bool,
}

pub fn popup_spec(properties: &HashMap<String, Value>) -> Result<PopupSpec, LayoutError> {
    // The one field here that is *not* deferred when it holds a `Signal` (see
    // [`is_deferred_signal`], which every other field below consults). `parent` decides which
    // surface `get_popup` roots this popup under and ADR-0051 decision 1 pins that to one
    // parent instance chosen at creation, which is the structural-decision test
    // [`reject_signal_in_structural_field`] exists for -- `parse_string_property` applies it.
    let parent = parse_string_property(properties, "parent", None)?;
    if parent.is_empty() {
        return Err(invalid("parent", "must name the `id` of the `panel` or `window` this popup anchors to"));
    }
    Ok(PopupSpec {
        id: parse_surface_id(properties)?,
        parent,
        anchor_rect: parse_anchor_rect(properties)?,
        width: parse_popup_extent(properties, "width")?,
        height: parse_popup_extent(properties, "height")?,
        anchor: parse_popup_anchor(properties, "anchor")?,
        gravity: parse_popup_anchor(properties, "gravity")?,
        constraint_adjustment: parse_constraint_adjustment(properties)?,
        offset: parse_popup_offset(properties)?,
        grab: parse_grab(properties)?,
    })
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
    fn window_spec_reads_every_toplevel_field_in_one_pass() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "settings", title = "Oblisk Settings", app_id = "oblisk.settings",
                                min_size = { width = 320, height = 240 }, max_size = { width = 1280, height = 960 } }"#,
            )
            .eval()
            .unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(
            spec,
            WindowSpec {
                id: "settings".to_string(),
                title: "Oblisk Settings".to_string(),
                app_id: "oblisk.settings".to_string(),
                min_size: Some(SizeHint { width: 320.0, height: 240.0 }),
                max_size: Some(SizeHint { width: 1280.0, height: 960.0 }),
            }
        );
    }

    #[test]
    fn a_window_without_an_id_is_rejected_the_same_way_a_panel_is() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", title = "x" }"#).eval().unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "id"
        ));
    }

    #[test]
    fn window_title_absent_defaults_to_the_empty_string_rather_than_the_id() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "settings" }"#).eval().unwrap();
        assert_eq!(window_spec(&props_from_table(&table)).unwrap().title, "");
    }

    #[test]
    fn window_app_id_absent_defaults_to_oblisk_dash_id() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "settings" }"#).eval().unwrap();
        assert_eq!(window_spec(&props_from_table(&table)).unwrap().app_id, "oblisk-settings");
    }

    #[test]
    fn window_min_size_and_max_size_are_absent_when_undeclared() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "settings" }"#).eval().unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(spec.min_size, None);
        assert_eq!(spec.max_size, None);
    }

    #[test]
    fn a_negative_window_min_size_axis_is_a_layout_error_rather_than_invalid_size() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "window", id = "settings", min_size = { width = -1, height = 240 } }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "min_size"
        ));
    }

    #[test]
    fn a_max_size_below_min_size_is_a_layout_error_rather_than_invalid_size() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "settings", min_size = { width = 800, height = 600 },
                                max_size = { width = 400, height = 600 } }"#,
            )
            .eval()
            .unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "max_size"
        ));
    }

    #[test]
    fn a_zero_max_size_axis_means_unset_and_does_not_collide_with_min_size() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "settings", min_size = { width = 800, height = 600 },
                                max_size = { width = 0, height = 0 } }"#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            window_spec(&props_from_table(&table)).unwrap().max_size,
            Some(SizeHint { width: 0.0, height: 0.0 })
        );
    }

    #[test]
    fn a_window_size_hint_missing_an_axis_is_rejected_naming_the_axis() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "window", id = "settings", min_size = { width = 320 } }"#).eval().unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, detail } if property == "min_size" && detail.contains("height")
        ));
    }

    #[test]
    fn a_window_size_hint_that_is_not_a_table_is_rejected() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "window", id = "settings", max_size = 800 }"#).eval().unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "max_size"
        ));
    }

    #[test]
    fn a_signal_in_a_window_title_resolves_because_set_title_is_valid_on_a_live_toplevel() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("Now Playing").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("t", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "w", title = t }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "window", &lua).unwrap();
        assert_eq!(window_spec(&resolved).unwrap().title, "Now Playing");
    }

    #[test]
    fn a_signal_in_a_window_app_id_resolves_because_set_app_id_is_valid_on_a_live_toplevel() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("oblisk.later").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("a", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "w", app_id = a }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "window", &lua).unwrap();
        assert_eq!(window_spec(&resolved).unwrap().app_id, "oblisk.later");
    }

    #[test]
    fn a_signal_in_a_window_id_is_still_rejected_because_id_is_every_kinds_reconcile_identity() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("w").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("i", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = i }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "window", &lua).unwrap();
        assert!(matches!(
            window_spec(&resolved).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "id"
        ));
    }

    /// Every popup fixture needs the four required properties, so only the property under test
    /// varies. `extra` is spliced in as further table entries, and a key it repeats *overrides* the
    /// default above it -- a Lua table constructor performs its assignments in order, so
    /// `{ width = 200, width = 0 }` is a table with `width == 0`. That is how a test declares one
    /// bad value without restating the other three good ones.
    fn popup_props(lua: &mlua::Lua, extra: &str) -> HashMap<String, Value> {
        let table: mlua::Table = lua
            .load(format!(
                r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                 anchor_rect = {{ x = 10, y = 0, width = 24, height = 24 }},
                                 width = 200, height = 300 {extra} }}"#
            ))
            .eval()
            .unwrap();
        props_from_table(&table)
    }

    #[test]
    fn popup_spec_reads_every_positioner_field_in_one_pass() {
        let lua = lua();
        let props = popup_props(
            &lua,
            r#", anchor = "BottomLeft", gravity = "BottomRight",
                    constraint_adjustment = { "SlideY", "ResizeX" }, offset = { x = -4, y = 2 }, grab = false"#,
        );
        let spec = popup_spec(&props).unwrap();
        assert_eq!(
            spec,
            PopupSpec {
                id: "menu".to_string(),
                parent: "bar".to_string(),
                anchor_rect: LogicalRect { x: 10.0, y: 0.0, width: 24.0, height: 24.0 },
                width: 200.0,
                height: 300.0,
                anchor: PopupAnchor::BottomLeft,
                gravity: PopupAnchor::BottomRight,
                constraint_adjustment: ConstraintAdjustment {
                    slide_y: true,
                    resize_x: true,
                    ..ConstraintAdjustment::NONE
                },
                offset: PopupOffset { x: -4.0, y: 2.0 },
                grab: false,
            }
        );
    }

    #[test]
    fn a_popup_without_a_parent_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
                .load(r#"return { kind = "popup", id = "menu", anchor_rect = { width = 1, height = 1 }, width = 8, height = 8 }"#)
                .eval()
                .unwrap();
        assert!(matches!(
            popup_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "parent"
        ));
    }

    #[test]
    fn a_popup_with_an_empty_parent_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", parent = """#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "parent"
        ));
    }

    #[test]
    fn a_popup_without_an_anchor_rect_is_a_layout_error_rather_than_invalid_positioner() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "popup", id = "menu", parent = "bar", width = 8, height = 8 }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            popup_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"
        ));
    }

    #[test]
    fn a_zero_size_anchor_rect_is_a_layout_error_rather_than_invalid_positioner() {
        let lua = lua();
        for axis in ["width", "height"] {
            let props = popup_props(
                &lua,
                &format!(r#", anchor_rect = {{ x = 0, y = 0, width = 24, height = 24, {axis} = 0 }}"#),
            );
            assert!(
                matches!(popup_spec(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"),
                "a zero anchor_rect {axis} must be a LayoutError"
            );
        }
    }

    #[test]
    fn a_negative_anchor_rect_size_is_a_layout_error_rather_than_invalid_input() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor_rect = { x = 0, y = 0, width = -24, height = 24 }"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"
        ));
    }

    #[test]
    fn an_anchor_rect_omitting_x_and_y_defaults_them_to_zero() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor_rect = { width = 24, height = 24 }"#);
        let rect = popup_spec(&props).unwrap().anchor_rect;
        assert_eq!(rect, LogicalRect { x: 0.0, y: 0.0, width: 24.0, height: 24.0 });
    }

    #[test]
    fn an_anchor_rect_that_is_not_a_table_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor_rect = 24"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"
        ));
    }

    #[test]
    fn a_popup_missing_its_width_or_height_is_a_layout_error_rather_than_invalid_positioner() {
        let lua = lua();
        for (present, missing) in [("width", "height"), ("height", "width")] {
            let table: mlua::Table = lua
                .load(format!(
                    r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                     anchor_rect = {{ width = 24, height = 24 }}, {present} = 200 }}"#
                ))
                .eval()
                .unwrap();
            let err = popup_spec(&props_from_table(&table)).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == missing),
                "an omitted `{missing}` must be a LayoutError naming it: {err:?}"
            );
        }
    }

    #[test]
    fn a_zero_popup_width_or_height_is_a_layout_error_rather_than_invalid_input() {
        let lua = lua();
        for axis in ["width", "height"] {
            let props = popup_props(&lua, &format!(r#", {axis} = 0"#));
            assert!(
                matches!(popup_spec(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == axis),
                "a zero popup {axis} must be a LayoutError naming it"
            );
        }
    }

    #[test]
    fn a_fill_popup_width_is_rejected_because_a_popup_has_no_fill() {
        let lua = lua();
        let props = popup_props(&lua, r#", width = "Fill""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "width"
        ));
    }

    #[test]
    fn popup_anchor_and_gravity_read_the_same_nine_value_set() {
        let lua = lua();
        for (text, expected) in [
            ("Top", PopupAnchor::Top),
            ("Bottom", PopupAnchor::Bottom),
            ("Left", PopupAnchor::Left),
            ("Right", PopupAnchor::Right),
            ("TopLeft", PopupAnchor::TopLeft),
            ("TopRight", PopupAnchor::TopRight),
            ("BottomLeft", PopupAnchor::BottomLeft),
            ("BottomRight", PopupAnchor::BottomRight),
            ("Center", PopupAnchor::Center),
        ] {
            let props = popup_props(&lua, &format!(r#", anchor = "{text}", gravity = "{text}""#));
            let spec = popup_spec(&props).unwrap();
            assert_eq!(spec.anchor, expected);
            assert_eq!(spec.gravity, expected);
        }
    }

    #[test]
    fn popup_anchor_and_gravity_absent_default_to_center() {
        let lua = lua();
        let spec = popup_spec(&popup_props(&lua, "")).unwrap();
        assert_eq!(spec.anchor, PopupAnchor::Center);
        assert_eq!(spec.gravity, PopupAnchor::Center);
    }

    #[test]
    fn an_unknown_popup_anchor_is_rejected_naming_the_property() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor = "Middle""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor"
        ));
    }

    #[test]
    fn an_unknown_popup_gravity_is_rejected_naming_the_property() {
        let lua = lua();
        let props = popup_props(&lua, r#", gravity = "Downward""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "gravity"
        ));
    }

    #[test]
    fn constraint_adjustment_absent_defaults_to_flip_y_and_slide_x() {
        let lua = lua();
        assert_eq!(
            popup_spec(&popup_props(&lua, "")).unwrap().constraint_adjustment,
            ConstraintAdjustment { flip_y: true, slide_x: true, ..ConstraintAdjustment::NONE }
        );
    }

    #[test]
    fn constraint_adjustment_reads_every_named_adjustment() {
        let lua = lua();
        let props = popup_props(
            &lua,
            r#", constraint_adjustment = { "SlideX", "SlideY", "FlipX", "FlipY", "ResizeX", "ResizeY" }"#,
        );
        assert_eq!(
            popup_spec(&props).unwrap().constraint_adjustment,
            ConstraintAdjustment {
                slide_x: true,
                slide_y: true,
                flip_x: true,
                flip_y: true,
                resize_x: true,
                resize_y: true
            }
        );
    }

    #[test]
    fn constraint_adjustment_is_a_set_so_order_and_repetition_do_not_change_it() {
        let lua = lua();
        let ordered = popup_spec(&popup_props(&lua, r#", constraint_adjustment = { "FlipY", "SlideX" }"#)).unwrap();
        let reversed =
            popup_spec(&popup_props(&lua, r#", constraint_adjustment = { "SlideX", "FlipY", "SlideX" }"#)).unwrap();
        assert_eq!(ordered.constraint_adjustment, reversed.constraint_adjustment);
    }

    #[test]
    fn an_explicitly_empty_constraint_adjustment_is_the_protocols_own_no_adjustment() {
        let lua = lua();
        assert_eq!(
            popup_spec(&popup_props(&lua, r#", constraint_adjustment = {}"#)).unwrap().constraint_adjustment,
            ConstraintAdjustment::NONE
        );
    }

    #[test]
    fn an_unknown_constraint_adjustment_entry_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", constraint_adjustment = { "FlipY", "SlideZ" }"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "constraint_adjustment"
        ));
    }

    #[test]
    fn a_constraint_adjustment_that_is_not_an_array_table_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", constraint_adjustment = "FlipY""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "constraint_adjustment"
        ));
    }

    #[test]
    fn popup_offset_absent_defaults_to_zero() {
        let lua = lua();
        assert_eq!(popup_spec(&popup_props(&lua, "")).unwrap().offset, PopupOffset { x: 0.0, y: 0.0 });
    }

    #[test]
    fn popup_offset_may_be_negative_on_either_axis() {
        let lua = lua();
        let props = popup_props(&lua, r#", offset = { x = -8, y = -2 }"#);
        assert_eq!(popup_spec(&props).unwrap().offset, PopupOffset { x: -8.0, y: -2.0 });
    }

    #[test]
    fn a_popup_offset_that_is_not_a_table_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", offset = 4"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "offset"
        ));
    }

    #[test]
    fn popup_grab_absent_defaults_to_true() {
        let lua = lua();
        assert!(popup_spec(&popup_props(&lua, "")).unwrap().grab);
    }

    #[test]
    fn popup_grab_reads_the_boolean_and_rejects_anything_else() {
        let lua = lua();
        assert!(!popup_spec(&popup_props(&lua, ", grab = false")).unwrap().grab);
        assert!(matches!(
            popup_spec(&popup_props(&lua, ", grab = 1")).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "grab"
        ));
    }

    #[test]
    fn a_signal_in_a_popup_anchor_rect_resolves_because_the_positioner_is_rebuilt_on_every_open() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "popup", id = "menu", parent = "bar", width = 200, height = 300,
                                anchor_rect = state("menu_anchor", { x = 4, y = 8, width = 16, height = 24 }) }"#,
            )
            .eval()
            .unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "popup", &lua).unwrap();
        assert_eq!(
            popup_spec(&resolved).unwrap().anchor_rect,
            LogicalRect { x: 4.0, y: 8.0, width: 16.0, height: 24.0 }
        );
    }

    /// The same fixture as [`popup_props`] but with every property under test bound to a live
    /// signal instead of a literal, and *not* run through [`resolve_properties`] -- which is
    /// exactly the map `crate::socket`'s `surface_specs` parses.
    fn unresolved_popup_props(lua: &mlua::Lua, extra: &str) -> HashMap<String, Value> {
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(format!(
                r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                 anchor_rect = state("a", {{ x = 4, y = 8, width = 16, height = 24 }}),
                                 width = 200, height = 300 {extra} }}"#
            ))
            .eval()
            .unwrap();
        props_from_table(&table)
    }

    #[test]
    fn a_signal_bound_popup_property_is_deferred_rather_than_rejected_before_it_resolves() {
        let lua = lua();
        let spec = popup_spec(&unresolved_popup_props(
            &lua,
            r#", width = state("w", 200), height = state("h", 300), anchor = state("an", "Top"),
                    gravity = state("g", "Bottom"), constraint_adjustment = state("c", {}),
                    offset = state("o", { x = 3, y = 3 }), grab = state("gr", false)"#,
        ))
        .unwrap();
        assert_eq!(spec.anchor_rect, LogicalRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 });
        assert_eq!((spec.width, spec.height), (1.0, 1.0));
        assert_eq!((spec.anchor, spec.gravity), (PopupAnchor::Center, PopupAnchor::Center));
        assert_eq!(spec.constraint_adjustment, ConstraintAdjustment::default());
        assert_eq!(spec.offset, PopupOffset::default());
        assert!(spec.grab, "a deferred `grab` takes § 6.3's default, not the signal's current value");
    }

    #[test]
    fn a_literal_typo_beside_a_deferred_signal_still_fails_on_the_evaluation_pass() {
        let lua = lua();
        assert!(matches!(
            popup_spec(&unresolved_popup_props(&lua, r#", anchor = "Middle""#)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor"
        ));
    }

    #[test]
    fn a_signal_in_a_popup_parent_is_still_rejected_on_the_evaluation_pass() {
        let lua = lua();
        assert!(matches!(
            popup_spec(&unresolved_popup_props(&lua, r#", parent = state("p", "bar")"#)).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "parent"
        ));
    }

    #[test]
    fn a_signal_in_a_window_title_app_id_or_size_hint_is_deferred_on_the_evaluation_pass_too() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "w", title = state("t", "Now Playing"),
                                app_id = state("a", "oblisk.later"),
                                min_size = state("mn", { width = 320, height = 240 }),
                                max_size = state("mx", { width = 1280, height = 800 }) }"#,
            )
            .eval()
            .unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(spec.title, "", "the placeholder is what a toplevel that never sends set_title has");
        assert_eq!(spec.app_id, "oblisk-w", "the same default an absent `app_id` takes");
        assert_eq!((spec.min_size, spec.max_size), (None, None), "absent means the request is simply not sent");
    }
}
