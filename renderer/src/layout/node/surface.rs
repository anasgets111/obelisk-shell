//! Layer-shell topology: `layer`, `anchor`, `monitor`, `namespace`, `keyboard_interactivity`,
//! `exclusive`, and the [`PanelSpec`] that bundles them with a panel's margin and size for
//! `panel_spec` to build in one pass (§ 6.1, build-steps.md Phase 20).
//!
//! `layer`, `anchor`, `monitor` and `namespace` are the structural carve-outs
//! [`is_structural_property`] names: `get_layer_surface` fixes all five at creation, so a `Signal`
//! in one is rejected outright rather than resolved.

use std::collections::HashMap;

use mlua::Value;

use super::content::parse_string_property;
use super::*;

/// § 6.1's `layer`, the layer-shell stacking level a `panel` is created on. `layout`'s own enum
/// rather than smithay-client-toolkit's `Layer`, for the same reason [`KeyboardInteractivity`]
/// below is: this module stays free of Wayland types, and `crate::wayland` maps it at its one call
/// site (build-steps.md Phase 20 item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// § 6.1's `layer` (`"Background"`/`"Bottom"`/`"Top"`/`"Overlay"`). Required, same shape as
/// [`parse_surface_id`].
///
/// Validates rather than passing the raw string through: `crate::wayland::App::create_panel`
/// creates one layer surface per instance straight from this value (ADR-0038 decision 1), so
/// an unrecognized string is a config error the author must see rather than a silent fall to some
/// default layer -- a typo'd `layer = "Toop"` that quietly stacked a bar on `Background` would be a
/// far worse failure than a rejected config, because nothing on screen would say why.
pub fn parse_layer(properties: &HashMap<String, Value>) -> Result<LayerKind, LayoutError> {
    match parse_string_property(properties, "layer", None)?.as_str() {
        "Background" => Ok(LayerKind::Background),
        "Bottom" => Ok(LayerKind::Bottom),
        "Top" => Ok(LayerKind::Top),
        "Overlay" => Ok(LayerKind::Overlay),
        other => Err(invalid(
            "layer",
            format!("unknown layer `{other}` -- expected \"Background\", \"Bottom\", \"Top\", or \"Overlay\""),
        )),
    }
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
    reject_signal_in_structural_field("anchor", value)?;
    let Value::Table(table) = value else {
        return Err(invalid("anchor", format!("expected a table, got {}", preview_for_error(value))));
    };
    let edge = |key: &str| -> Result<bool, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("anchor", e.to_string()))?;
        match v {
            Value::Nil => Ok(false),
            Value::Boolean(b) => Ok(b),
            other => Err(invalid("anchor", format!("`{key}` must be a boolean, got {}", preview_for_error(&other)))),
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

/// § 6.1's `namespace`: the layer-shell namespace string the compositor sees, and the key its own
/// rules match on (Hyprland's `layerrule` for blur and animations). Defaults to `"oblisk-{id}"`,
/// which makes every `panel` addressable from a compositor config without the author naming one.
///
/// A [`SurfaceTopology`] field, not an in-place one: `get_layer_surface` takes the namespace at
/// creation and the protocol has no request to change it afterwards, so an edit to it is a
/// generation swap (`CONTEXT.md`, Topology change).
pub fn parse_namespace(properties: &HashMap<String, Value>, id: &str) -> Result<String, LayoutError> {
    let default = format!("oblisk-{id}");
    parse_string_property(properties, "namespace", Some(&default))
}

/// § 6.1's `keyboard_interactivity`, mapping one-for-one onto layer-shell's own field.
/// `layout`-owned rather than reusing smithay-client-toolkit's identical enum so this module keeps
/// no Wayland dependency; `crate::wayland::keyboard_interactivity_for` maps it at the single call
/// site that binds a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardInteractivity {
    /// § 6.1's default: the surface never receives key events.
    #[default]
    None,
    OnDemand,
    Exclusive,
}

/// § 6.1's `keyboard_interactivity` (`"None"` (default) / `"OnDemand"` / `"Exclusive"`). An
/// in-place field, deliberately outside [`SurfaceTopology`] and outside
/// [`is_structural_property`]'s carve-out: `zwlr_layer_surface_v1::set_keyboard_interactivity` is
/// valid on a live surface, so a `Signal` here resolves like any other property
/// (ADR-0044 decision 1) and an edit to it is a value change, not a swap.
pub fn parse_keyboard_interactivity(properties: &HashMap<String, Value>) -> Result<KeyboardInteractivity, LayoutError> {
    // Deferred on the evaluation-time pass ([`is_deferred_signal`]), same split as [`parse_title`]'s:
    // this doc comment's own argument is what makes it a deferral rather than a rejection, since a
    // field valid on a live surface is one only the resolved pass is in a position to read.
    if is_deferred_signal(properties, "keyboard_interactivity") {
        return Ok(KeyboardInteractivity::None);
    }
    let Some(value) = properties.get("keyboard_interactivity") else {
        return Ok(KeyboardInteractivity::None);
    };
    let Value::String(s) = value else {
        return Err(invalid("keyboard_interactivity", format!("expected a string, got {}", preview_for_error(value))));
    };
    match checked_string("keyboard_interactivity", s)?.as_str() {
        "None" => Ok(KeyboardInteractivity::None),
        "OnDemand" => Ok(KeyboardInteractivity::OnDemand),
        "Exclusive" => Ok(KeyboardInteractivity::Exclusive),
        other => Err(invalid(
            "keyboard_interactivity",
            format!("unknown keyboard interactivity `{other}` -- expected \"None\", \"OnDemand\", or \"Exclusive\""),
        )),
    }
}

/// What § 6.1's `exclusive` asks the compositor for, which is three answers rather than the two a
/// boolean can carry. Each maps to one `zwlr_layer_surface_v1::set_exclusive_zone` value.
///
/// The third one exists because a boolean could not say what a wallpaper needs. Layer-shell's zone
/// is a signed number with three meanings: a positive one reserves that much, `0` reserves nothing
/// *and still sits inside what everyone else reserved*, and `-1` ignores every other surface's zone
/// and covers the output. A full-screen backdrop wants the last of those, and before this it could
/// reach neither: [`exclusive_zone_for`](crate::wayland) answers `0` for a surface anchored to all
/// four edges, because there is no single edge to reserve against, so `true` and `false` were the
/// same request on exactly the surface that needed a third.
///
/// Named for what each does rather than mirroring the protocol's integer, and deliberately the same
/// three Quickshell's `ExclusionMode` settles on (`Auto`, `Normal`, `Ignore`): that enum is the
/// prior art for this protocol and its `Ignore` carries the same "ignore exclusion zones of other
/// shell layers" wording. The spellings differ because `Reserve`/`Respect` say which of the two
/// non-ignoring answers a surface picked, where `Auto`/`Normal` name how the number was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusive {
    /// Reserve screen area along the anchored edge, derived from the size the compositor
    /// configured. § 6.1's "Reserves physical screen area for bar if true".
    Reserve,
    /// Reserve nothing, and stay inside the area other surfaces reserved. The default, and the
    /// protocol's `0`.
    Respect,
    /// Reserve nothing and ignore what everyone else reserved, covering the output. The protocol's
    /// `-1`, and the only setting under which a wallpaper stays full-bleed once a bar is up.
    Ignore,
}

/// § 6.1's `exclusive`. Default [`Exclusive::Respect`], so an undeclared panel floats over whatever
/// is behind it rather than pushing windows aside or covering them.
///
/// `boolean / string`, the same shape § 6.1 already gives `width`/`height` (`integer / "Fill"`):
/// `true` and `false` keep exactly the meanings they had, and `"Ignore"` is the value neither could
/// express. Additive on purpose -- every config written before this one means what it meant.
///
/// In-place, same as [`parse_keyboard_interactivity`]: `set_exclusive_zone` is valid on a live
/// surface. The *zone* itself is not computed here -- `crate::wayland` derives it at configure
/// time from the size the compositor actually chose, which is the only point a real number exists.
pub fn parse_exclusive(properties: &HashMap<String, Value>) -> Result<Exclusive, LayoutError> {
    // Deferred on the evaluation-time pass for [`parse_keyboard_interactivity`]'s reason:
    // `set_exclusive_zone` is valid on a live surface, so `exclusive = hide_bar` is a config § 5.1
    // permits and only this pass cannot read.
    //
    // `Respect` is the placeholder, and it has to be the one that reserves and covers nothing:
    // this pass runs before any getter has been called, so the value is genuinely unknown, and both
    // other answers are visible mistakes for a frame. Guessing `Ignore` would paint a wallpaper
    // over the bar until the resolved pass corrected it; guessing `Reserve` would shove every
    // window aside. Doing nothing is the only answer that looks like nothing.
    if is_deferred_signal(properties, "exclusive") {
        return Ok(Exclusive::Respect);
    }
    let Some(value) = properties.get("exclusive") else {
        return Ok(Exclusive::Respect);
    };
    match value {
        Value::Boolean(true) => Ok(Exclusive::Reserve),
        Value::Boolean(false) => Ok(Exclusive::Respect),
        Value::String(s) if checked_string("exclusive", s)? == "Ignore" => Ok(Exclusive::Ignore),
        other => {
            Err(invalid("exclusive", format!("expected a boolean or \"Ignore\", got {}", preview_for_error(other))))
        }
    }
}

/// A surface's topology-relevant fields (`CONTEXT.md`, Topology change: a config edit that adds or
/// removes a top-level `surface` node, or changes its layer, anchor, monitor target, or
/// namespace). Structural equality on `Vec<SurfaceTopology>` (order-sensitive) is the Renderer's
/// own topology diff -- see `renderer/src/socket.rs`.
///
/// This is the whole of the swap fingerprint, and [`PanelSpec`]'s other fields are deliberately
/// not in it: `margin`, `keyboard_interactivity`, `exclusive`, `width` and `height` are all
/// requests layer-shell accepts on a live surface, so changing one reloads in place
/// (ADR-0038 decision 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceTopology {
    pub id: String,
    pub layer: LayerKind,
    pub anchor: Anchor,
    pub monitor: String,
    pub namespace: String,
}

pub fn surface_topology(properties: &HashMap<String, Value>) -> Result<SurfaceTopology, LayoutError> {
    let id = parse_surface_id(properties)?;
    let namespace = parse_namespace(properties, &id)?;
    Ok(SurfaceTopology {
        id,
        layer: parse_layer(properties)?,
        anchor: parse_anchor(properties)?,
        monitor: parse_monitor(properties)?,
        namespace,
    })
}

/// Everything one `zwlr_layer_surface_v1` needs, read off a `panel` node's properties in one pass
/// (§ 6.1, build-steps.md Phase 20 item 3). `crate::socket`'s `surface_specs` builds one per
/// declared `panel`; `layout::instance::expand_instances` turns them into per-output instances, and
/// `crate::wayland::App::create_panel` is what actually binds them.
///
/// The split between `topology` and the rest is the swap-versus-in-place split itself, so it is
/// worth reading as one: `renderer/src/socket.rs`'s `handle_reevaluate` diffs *only* `topology`,
/// which is why editing a `margin` reloads in place while editing a `layer` respawns the process.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelSpec {
    /// The swap fingerprint: id, layer, anchor, monitor, namespace.
    pub topology: SurfaceTopology,
    pub keyboard_interactivity: KeyboardInteractivity,
    pub exclusive: Exclusive,
    /// § 6.1's `margin`, which on a `panel` root is the layer-shell **anchor offset** -- how far
    /// the surface itself sits from the edges it is anchored to -- not layout spacing between the
    /// root and its child. There is no conflict with layout's own reading of the property because
    /// layout never reads it here: `layout::scene`'s `Scene::apply_one_surface` passes `None` for
    /// both parent-margin arguments when it resolves a surface root, so a root's `margin` is
    /// consumed by nobody but this field. Below a root it stays ordinary layout margin, parsed by
    /// the same [`parse_edge_insets`] and consumed by the parent's child loop.
    pub margin: EdgeInsets,
    /// § 6.1's `width`/`height`, which become the layer-shell `set_size` request rather than a
    /// layout constraint of their own. `SizeMode::Fill` is the protocol's `0` ("the anchors
    /// decide"); a percent resolves against the output, at the one call site that knows it.
    pub width: SizeMode,
    pub height: SizeMode,
}

pub fn panel_spec(properties: &HashMap<String, Value>) -> Result<PanelSpec, LayoutError> {
    Ok(PanelSpec {
        topology: surface_topology(properties)?,
        keyboard_interactivity: parse_keyboard_interactivity(properties)?,
        exclusive: parse_exclusive(properties)?,
        margin: parse_edge_insets(properties, "margin")?,
        width: parse_size_mode(properties, "width")?,
        height: parse_size_mode(properties, "height")?,
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
    fn layer_is_required() {
        let props = HashMap::new();
        assert!(matches!(parse_layer(&props).unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn layer_reads_each_of_the_four_protocol_levels() {
        let lua = lua();
        for (text, expected) in [
            ("Background", LayerKind::Background),
            ("Bottom", LayerKind::Bottom),
            ("Top", LayerKind::Top),
            ("Overlay", LayerKind::Overlay),
        ] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "panel", layer = "{text}" }}"#)).eval().unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_layer(&props).unwrap(), expected);
        }
    }

    #[test]
    fn an_unrecognized_layer_is_a_config_error_not_a_silent_default() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", layer = "Toop" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_layer(&props).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "layer"), "got {err}");
        assert!(err.to_string().contains("Toop"), "the message must name the value the config wrote: {err}");
    }

    #[test]
    fn anchor_absent_defaults_all_false() {
        let props = HashMap::new();
        assert_eq!(parse_anchor(&props).unwrap(), Anchor::default());
    }

    #[test]
    fn anchor_reads_named_edges_defaulting_absent_ones_to_false() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "panel", anchor = { top = true, left = true } }"#).eval().unwrap();
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
        let table: mlua::Table = lua.load(r#"return { kind = "panel", monitor = "eDP-1" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_monitor(&props).unwrap(), "eDP-1");
    }

    #[test]
    fn surface_topology_combines_id_layer_anchor_monitor_and_namespace() {
        let lua = lua();
        let table: mlua::Table = lua
                .load(r#"return { kind = "panel", id = "bar", layer = "Top", anchor = { top = true }, monitor = "eDP-1", namespace = "my-bar" }"#)
                .eval()
                .unwrap();
        let props = props_from_table(&table);
        let topology = surface_topology(&props).unwrap();
        assert_eq!(
            topology,
            SurfaceTopology {
                id: "bar".to_string(),
                layer: LayerKind::Top,
                anchor: Anchor { top: true, right: false, bottom: false, left: false },
                monitor: "eDP-1".to_string(),
                namespace: "my-bar".to_string(),
            }
        );
    }

    #[test]
    fn namespace_absent_defaults_to_oblisk_dash_id() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "panel", id = "launcher", layer = "Overlay" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_namespace(&props, "launcher").unwrap(), "oblisk-launcher");
        assert_eq!(surface_topology(&props).unwrap().namespace, "oblisk-launcher");
    }

    #[test]
    fn a_signal_in_namespace_on_a_panel_is_rejected_like_every_other_topology_field() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("x").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("ns", signal).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { kind = "panel", id = "bar", layer = "Top", namespace = ns }"#).eval().unwrap();
        let props = props_from_table(&table);
        let resolved = resolve_properties(&props, "panel", &lua).unwrap();
        assert!(
            matches!(parse_namespace(&resolved, "bar").unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "namespace")
        );
    }

    #[test]
    fn keyboard_interactivity_absent_defaults_to_none() {
        let props = HashMap::new();
        assert_eq!(parse_keyboard_interactivity(&props).unwrap(), KeyboardInteractivity::None);
    }

    #[test]
    fn keyboard_interactivity_reads_each_of_the_three_protocol_modes() {
        let lua = lua();
        for (text, expected) in [
            ("None", KeyboardInteractivity::None),
            ("OnDemand", KeyboardInteractivity::OnDemand),
            ("Exclusive", KeyboardInteractivity::Exclusive),
        ] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "panel", keyboard_interactivity = "{text}" }}"#)).eval().unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_keyboard_interactivity(&props).unwrap(), expected);
        }
    }

    #[test]
    fn an_unrecognized_keyboard_interactivity_is_rejected() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "panel", keyboard_interactivity = "Always" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(
            matches!(parse_keyboard_interactivity(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "keyboard_interactivity")
        );
    }

    #[test]
    fn a_signal_in_keyboard_interactivity_resolves_rather_than_being_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("Exclusive").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("mode", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return { kind = "panel", id = "bar", layer = "Top", keyboard_interactivity = mode }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let resolved = resolve_properties(&props, "panel", &lua).unwrap();
        assert_eq!(parse_keyboard_interactivity(&resolved).unwrap(), KeyboardInteractivity::Exclusive);
    }

    #[test]
    fn exclusive_absent_reserves_and_covers_nothing() {
        let props = HashMap::new();
        assert_eq!(parse_exclusive(&props).unwrap(), Exclusive::Respect);
    }

    #[test]
    fn exclusive_reads_both_booleans_and_ignore_and_rejects_anything_else() {
        let lua = lua();
        let parse = |src: &str| {
            let table: mlua::Table = lua.load(src).eval().unwrap();
            parse_exclusive(&props_from_table(&table))
        };
        assert_eq!(parse(r#"return { kind = "panel", exclusive = true }"#).unwrap(), Exclusive::Reserve);
        assert_eq!(parse(r#"return { kind = "panel", exclusive = false }"#).unwrap(), Exclusive::Respect);
        assert_eq!(parse(r#"return { kind = "panel", exclusive = "Ignore" }"#).unwrap(), Exclusive::Ignore);

        // A number was the original rejection case and stays one. The unknown string is the new
        // one, and it matters more: `"ignore"` and `"None"` are the shapes a config author actually
        // reaches for, and silently reading either as `Respect` would be the quiet miss
        // `NODE_PROPERTIES` exists to prevent one level up.
        for bad in
            [r#"return { kind = "panel", exclusive = 32 }"#, r#"return { kind = "panel", exclusive = "ignore" }"#]
        {
            assert!(
                matches!(parse(bad).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "exclusive"),
                "{bad} must be refused by name"
            );
        }
    }

    /// The placeholder the raw pass takes, which has to be the answer that does nothing visible:
    /// `crate::socket`'s `surface_specs` runs before any getter, so it cannot know, and both other
    /// answers are a wrong frame on screen (a wallpaper over the bar, or every window shoved aside).
    #[test]
    fn a_signal_valued_exclusive_defers_to_respect_rather_than_guessing() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { kind = "panel", exclusive = state("hide_bar", true) }"#).eval().unwrap();
        assert_eq!(parse_exclusive(&props_from_table(&table)).unwrap(), Exclusive::Respect);
    }

    #[test]
    fn panel_spec_reads_every_layer_surface_field_in_one_pass() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "panel", id = "dock", layer = "Bottom", anchor = { bottom = true },
                       monitor = "DP-1", namespace = "my-dock", keyboard_interactivity = "OnDemand",
                       exclusive = true, margin = { top = 4, left = 8 }, width = "Fill", height = 48 }"#,
            )
            .eval()
            .unwrap();
        let spec = panel_spec(&props_from_table(&table)).unwrap();

        assert_eq!(spec.topology.id, "dock");
        assert_eq!(spec.topology.layer, LayerKind::Bottom);
        assert_eq!(spec.topology.anchor, Anchor { top: false, right: false, bottom: true, left: false });
        assert_eq!(spec.topology.monitor, "DP-1");
        assert_eq!(spec.topology.namespace, "my-dock");
        assert_eq!(spec.keyboard_interactivity, KeyboardInteractivity::OnDemand);
        assert_eq!(spec.exclusive, Exclusive::Reserve);
        assert_eq!(spec.margin, EdgeInsets { top: 4.0, right: 0.0, bottom: 0.0, left: 8.0 });
        assert_eq!(spec.width, SizeMode::Fill);
        assert_eq!(spec.height, SizeMode::Pixels(48.0));
    }

    #[test]
    fn a_signal_in_a_panels_five_in_place_fields_is_deferred_on_the_evaluation_pass() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "panel", id = "bar", layer = "Top",
                                keyboard_interactivity = state("k", "Exclusive"), exclusive = state("e", true),
                                margin = state("m", { top = 4 }), width = state("w", 100), height = state("h", 48) }"#,
            )
            .eval()
            .unwrap();
        let spec = panel_spec(&props_from_table(&table)).unwrap();
        assert_eq!(
            spec.keyboard_interactivity,
            KeyboardInteractivity::None,
            "§ 6.1's default, not the signal's current value"
        );
        assert_eq!(spec.exclusive, Exclusive::Respect);
        assert_eq!(spec.margin, EdgeInsets::default());
        assert_eq!((spec.width, spec.height), (SizeMode::Content, SizeMode::Content));
    }

    #[test]
    fn a_signal_in_a_panels_topology_fields_is_still_rejected_on_the_evaluation_pass() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        for property in ["layer", "anchor", "monitor", "namespace"] {
            let table: mlua::Table = lua
                .load(format!(
                    r#"return {{ kind = "panel", id = "bar", layer = "Top", {property} = state("s", "Top") }}"#
                ))
                .eval()
                .unwrap();
            assert!(
                matches!(panel_spec(&props_from_table(&table)).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == property),
                "{property} is structural"
            );
        }
    }

    #[test]
    fn a_panel_roots_margin_is_the_anchor_offset_and_no_layout_pass_consumes_it() {
        let lua = mlua::Lua::new();
        crate::lua::nodes::register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua
                .load(r#"return panel { id = "bar", layer = "Top", width = 80, height = 20, margin = { top = 12, left = 30 } }"#)
                .eval()
                .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let spec = panel_spec(&surface.properties).unwrap();
        assert_eq!(spec.margin, EdgeInsets { top: 12.0, right: 0.0, bottom: 0.0, left: 30.0 });

        let mut scene = crate::layout::Scene::new();
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        let instances = vec![crate::layout::instance::SurfaceInstance {
            instance_id: "bar@TEST".to_string(),
            declared_id: "bar".to_string(),
            output: "TEST".to_string(),
            available: crate::layout::LogicalSize { width: 1000.0, height: 500.0 },
        }];
        scene.apply(&[surface], &instances, &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        assert_eq!((root.rect.x, root.rect.y), (0.0, 0.0), "a root's margin must not offset it inside its own surface");
        assert_eq!((root.rect.width, root.rect.height), (80.0, 20.0), "a root's margin must not shrink it either");
    }

    #[test]
    fn a_signal_userdata_in_layer_is_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "panel").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(
            matches!(parse_layer(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer")
        );
    }

    #[test]
    fn a_surface_topology_field_on_a_non_panel_node_is_refused_rather_than_carried() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        for property in ["layer", "anchor", "monitor"] {
            let signal =
                crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
            let table = lua.create_table().unwrap();
            table.set("kind", "rect").unwrap();
            table.set(property, signal).unwrap();

            let err = deserialize_lua_table(&table).unwrap_err();

            assert!(
                err.to_string().contains(property),
                "`{property}` is § 6.1 topology and a rect has no row for it, so it must be refused by name: {err}"
            );
        }
    }

    #[test]
    fn a_signal_in_layer_on_a_panel_still_survives_raw_for_parse_layer_to_reject() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "panel").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(&node.properties, "panel", &lua).unwrap();

        assert!(
            matches!(resolved.get("layer"), Some(Value::UserData(_))),
            "layer must survive the resolve step unresolved on a panel"
        );
        assert!(
            matches!(parse_layer(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer")
        );
    }
}
