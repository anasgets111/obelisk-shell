//! `xdg_toplevel`/`xdg_positioner` specs (§ 6), including window and popup field parsers. These
//! describe live `xdg_shell` objects, not layer-shell or session-lock surfaces.

use std::collections::HashMap;

use mlua::Value;

use crate::text::snap::LogicalRect;

use super::content::parse_string_property;
use super::style::table_number;
use super::*;

/// § 6's live `title`, defaulting to empty rather than exposing the internal `id`. `set_title` is
/// valid after mapping, so signals update it in place (ADR-0044 decision 1).
pub fn parse_title(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    // Deferred on the evaluation pass; `show_window` sends the resolved title.
    if is_deferred_signal(properties, "title") {
        return Ok(String::new());
    }
    parse_string_property(properties, "title", Some(""))
}

/// § 6's `app_id`, used by compositor window rules, defaulting to `"obelisk-{id}"`. `set_app_id`
/// remains valid after mapping (`xdg-shell.xml`), unlike layer-shell `namespace`; `id` is still
/// structural because it is reconcile identity (ADR-0045 decision 1).
pub fn parse_app_id(properties: &HashMap<String, Value>, id: &str) -> Result<String, LayoutError> {
    let default = format!("obelisk-{id}");
    // Deferred on the evaluation pass; `set_app_id` is a live request.
    if is_deferred_signal(properties, "app_id") {
        return Ok(default);
    }
    parse_string_property(properties, "app_id", Some(&default))
}

/// § 6's advisory `{ width, height }` size hint. Layout does not enforce it; Wayland receives it
/// through `set_min_size`/`set_max_size`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeHint {
    pub width: f32,
    pub height: f32,
}

/// `None` means no request; `Some(0, 0)` sends an unconstrained request. A present hint must name
/// both axes; use `0` for an unconstrained axis.
fn parse_size_hint(properties: &HashMap<String, Value>, property: &str) -> Result<Option<SizeHint>, LayoutError> {
    // Deferred on the evaluation pass; `show_window` sends the resolved request.
    let Some(value) = non_deferred_property(properties, property) else {
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
        // Negative values make the request fail (`invalid_size`); § 5.1 supplies the `[0, 8192]`
        // upper bound.
        if !(0.0..=8192.0).contains(&n) {
            return Err(invalid(property, format!("`{key}` must be within [0, 8192], got {n}")));
        }
        Ok(n)
    };
    Ok(Some(SizeHint { width: axis("width")?, height: axis("height")? }))
}

/// Checks `set_max_size`'s `max >= min` rule before Wayland sees it. Zero means unset, so it is not
/// below the minimum; a bad pair becomes a `LayoutError` in `rescue` (§ 2.10), not `invalid_size`
/// on the Wayland connection.
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

/// The one live `xdg_toplevel` spec for a top-level `window` (§ 6, ADR-0040 decision 1). There is
/// no `WindowTopology`: title, app id, and size hints update the live object; `visible` creates and
/// destroys it rather than swapping the generation. Only `id` changes the declared set and swaps
/// generation (ADR-0001, ADR-0049 decisions 1-3). A window has one object regardless of outputs
/// (ADR-0038 decision 3); callbacks and visibility remain scene properties.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// Advisory (§ 6): parsed and carried, never enforced against the resolved tree.
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

/// § 6's shared `anchor`/`gravity` values. `Center` maps to protocol `none`, which centers an
/// unspecified axis; `crate::wayland` maps this local enum to the positioner protocol.
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

/// Parses either § 6 anchor field; `property` names errors. Absent defaults to the protocol's
/// [`PopupAnchor::Center`], unlike constraint adjustments.
pub fn parse_popup_anchor(properties: &HashMap<String, Value>, property: &str) -> Result<PopupAnchor, LayoutError> {
    let Some(value) = non_deferred_property(properties, property) else {
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

/// § 6's six independent adjustment permissions. Array order is irrelevant because the compositor
/// applies fixed Flip, Slide, Resize precedence and the request is a bitmask. Default is
/// `{ "FlipY", "SlideX" }`, not the protocol's empty default (ADR-0040 decision 3).
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
    /// Protocol default: no adjustment. An empty array parses here.
    pub const NONE: Self =
        Self { slide_x: false, slide_y: false, flip_x: false, flip_y: false, resize_x: false, resize_y: false };
}

impl Default for ConstraintAdjustment {
    fn default() -> Self {
        Self { flip_y: true, slide_x: true, ..Self::NONE }
    }
}

pub fn parse_constraint_adjustment(properties: &HashMap<String, Value>) -> Result<ConstraintAdjustment, LayoutError> {
    let Some(value) = non_deferred_property(properties, "constraint_adjustment") else {
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
        // Flags make repeats no-ops; array position has no meaning.
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

/// § 6's signed pixel nudge after anchor and gravity; negative values move up or left.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PopupOffset {
    pub x: f32,
    pub y: f32,
}

pub fn parse_popup_offset(properties: &HashMap<String, Value>) -> Result<PopupOffset, LayoutError> {
    let Some(value) = non_deferred_property(properties, "offset") else {
        return Ok(PopupOffset::default());
    };
    let Value::Table(table) = value else {
        return Err(invalid("offset", format!("expected an `{{ x, y }}` table, got {}", preview_for_error(value))));
    };
    let axis = |key: &str| -> Result<f32, LayoutError> { Ok(table_number("offset", table, key)?.unwrap_or(0.0)) };
    Ok(PopupOffset { x: axis("x")?, y: axis("y")? })
}

/// § 6's parent-local `anchor_rect`, reused from `on_click` (ADR-0050 decision 3). `x`/`y` default
/// to 0, but `width`/`height` must be in `(0, 8192]`: negative sizes raise `invalid_input`, zero
/// leaves the positioner incomplete and raises `invalid_positioner` at `get_popup`. Deferred
/// signals use a 1x1 placeholder because zero is incomplete; `App::apply_resolved_state` replaces
/// it before creation (ADR-0049's second amendment). `expand_instances` therefore measures a
/// signal-sized popup against 1x1 until its first configure.
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

/// Popup `width`/`height`, narrower than [`parse_size_mode`]: no `"Fill"` and no percent, because
/// there is no parent box for either to mean anything against -- `xdg_positioner::set_size` takes a
/// number, and the compositor places the popup rather than fitting it into something.
///
/// Omitted is [`SizeMode::Content`], on the same terms as every other node: `parse_size_mode`'s own
/// error says content sizing has no literal and the property is left off instead. That axis is then
/// whatever the resolved tree measures, and `wayland::surface::App::apply_resolved_state` reads it
/// off the root's box on the pass that opens the popup. A number is still a number, and still has
/// to be in `(0, 8192]`: `set_size` raises `invalid_input` on a zero or negative size.
fn parse_popup_extent(properties: &HashMap<String, Value>, property: &str) -> Result<SizeMode, LayoutError> {
    if is_deferred_signal(properties, property) {
        return Ok(SizeMode::Pixels(DEFERRED_POPUP_EXTENT));
    }
    let Some(value) = properties.get(property) else {
        return Ok(SizeMode::Content);
    };
    let n = value_as_f32(property, value)?.ok_or_else(|| {
        invalid(
            property,
            format!(
                "expected a number, got {} -- a popup has no \"Fill\" and no percent; omit the property to size it to its content",
                preview_for_error(value)
            ),
        )
    })?;
    if !(n > 0.0 && n <= 8192.0) {
        return Err(invalid(
            property,
            format!("must be within (0, 8192], got {n} -- set_size raises invalid_input on a zero or negative size"),
        ));
    }
    Ok(SizeMode::Pixels(n))
}

/// § 6's `grab`, defaulting to `true` so outside clicks dismiss a dropdown (ADR-0040 decision 2).
/// ADR-0040 chose a real `xdg_popup` here over a second `panel`.
/// Taking the grab needs a real input serial for one poll turn, and the compositor may deny it
/// (ADR-0049 amendment).
pub fn parse_grab(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    let Some(value) = non_deferred_property(properties, "grab") else {
        return Ok(true);
    };
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(invalid("grab", format!("expected a boolean, got {}", preview_for_error(other)))),
    }
}

/// One top-level popup's `xdg_popup`/`xdg_positioner` fields (§ 6). Its parent is a surface id,
/// because the protocol roots the popup through `xdg_surface.get_popup` or
/// `zwlr_layer_surface_v1.get_popup`. The object exists only while shown and the positioner is
/// consumed at `get_popup`, so all fields re-read on open; only declaration changes topology
/// (ADR-0001, ADR-0049 decisions 1 and 3).
#[derive(Debug, Clone, PartialEq)]
pub struct PopupSpec {
    pub id: String,
    /// The `id` of the `panel` or `window` this popup anchors to. Read with the same lossy
    /// conversion [`parse_surface_id`] uses on the other side of the match, so the two agree.
    pub parent: String,
    pub anchor_rect: LogicalRect,
    /// [`SizeMode::Pixels`] for a declared number, [`SizeMode::Content`] for an omitted axis. Only
    /// those two: [`parse_popup_extent`] admits nothing else. A `Content` axis carries no number
    /// here because there is none until the tree is solved; `wayland::surface` resolves it against
    /// the root's measured box before the positioner is built.
    pub width: SizeMode,
    pub height: SizeMode,
    pub anchor: PopupAnchor,
    pub gravity: PopupAnchor,
    pub constraint_adjustment: ConstraintAdjustment,
    pub offset: PopupOffset,
    pub grab: bool,
}

pub fn popup_spec(properties: &HashMap<String, Value>) -> Result<PopupSpec, LayoutError> {
    // `parent` is structural: `get_popup` pins this popup to one parent instance
    // (ADR-0051 decision 1).
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
                r#"return { kind = "window", id = "settings", title = "Obelisk Settings", app_id = "obelisk.settings",
                                min_size = { width = 320, height = 240 }, max_size = { width = 1280, height = 960 } }"#,
            )
            .eval()
            .unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(
            spec,
            WindowSpec {
                id: "settings".to_string(),
                title: "Obelisk Settings".to_string(),
                app_id: "obelisk.settings".to_string(),
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
    fn window_app_id_absent_defaults_to_obelisk_dash_id() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "settings" }"#).eval().unwrap();
        assert_eq!(window_spec(&props_from_table(&table)).unwrap().app_id, "obelisk-settings");
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
            Value::String(lua.create_string("obelisk.later").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("a", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "w", app_id = a }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "window", &lua).unwrap();
        assert_eq!(window_spec(&resolved).unwrap().app_id, "obelisk.later");
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
                width: SizeMode::Pixels(200.0),
                height: SizeMode::Pixels(300.0),
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
    fn an_omitted_popup_width_or_height_is_measured_from_the_tree_rather_than_refused() {
        // This used to be the error "required for `popup`", so every tooltip carried a
        // hand-guessed pair of numbers and cut off any text longer than the one it was guessed for. An omitted axis is `Content` here as it is
        // on every other node; `wayland::surface::popup_requested_size` turns it into the number
        // the positioner needs, off the box the pass measured.
        let lua = lua();
        for (present, omitted) in [("width", "height"), ("height", "width")] {
            let table: mlua::Table = lua
                .load(format!(
                    r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                     anchor_rect = {{ width = 24, height = 24 }}, {present} = 200 }}"#
                ))
                .eval()
                .unwrap();
            let spec = popup_spec(&props_from_table(&table)).expect("an omitted axis is not an error");
            let (declared, measured) =
                if omitted == "height" { (spec.width, spec.height) } else { (spec.height, spec.width) };
            assert_eq!(declared, SizeMode::Pixels(200.0), "the declared axis keeps its number");
            assert_eq!(measured, SizeMode::Content, "the omitted `{omitted}` is measured");
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
        assert_eq!((spec.width, spec.height), (SizeMode::Pixels(1.0), SizeMode::Pixels(1.0)));
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
                                app_id = state("a", "obelisk.later"),
                                min_size = state("mn", { width = 320, height = 240 }),
                                max_size = state("mx", { width = 1280, height = 800 }) }"#,
            )
            .eval()
            .unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(spec.title, "", "the placeholder is what a toplevel that never sends set_title has");
        assert_eq!(spec.app_id, "obelisk-w", "the same default an absent `app_id` takes");
        assert_eq!((spec.min_size, spec.max_size), (None, None), "absent means the request is simply not sent");
    }
}
