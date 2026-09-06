//! Layer-shell topology and `PanelSpec` (§ 6). `get_layer_surface` fixes `layer`, `anchor`,
//! `monitor`, and `namespace` at creation, so those structural fields reject `Signal`s.

use std::collections::HashMap;

use mlua::Value;

use super::content::parse_string_property;
use super::*;

/// § 6's layer-shell stacking level. Kept separate from smithay-client-toolkit so this module has
/// no Wayland types; `crate::wayland` maps it at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// § 6's required layer string (ADR-0038 decision 1). `create_panel` uses it per instance, so a
/// typo such as `"Toop"` errors instead of silently selecting `Background`.
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

/// § 6's `{ top, bottom, left, right }` edge booleans, defaulting to false.
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

/// § 6's specific output EDID, or `"All"`; absent defaults to `"All"`.
pub fn parse_monitor(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "monitor", Some("All"))
}

/// § 6's layer-shell namespace, also used by Hyprland `layerrule` for blur and animations. It
/// defaults to `"oblisk-{id}"`; `get_layer_surface` fixes it at creation, so edits swap generation.
pub fn parse_namespace(properties: &HashMap<String, Value>, id: &str) -> Result<String, LayoutError> {
    let default = format!("oblisk-{id}");
    parse_string_property(properties, "namespace", Some(&default))
}

/// § 6's `keyboard_interactivity`, mapped to layer-shell by
/// `crate::wayland::keyboard_interactivity_for`; kept local to avoid Wayland types here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardInteractivity {
    /// The surface never receives key events.
    #[default]
    None,
    OnDemand,
    Exclusive,
}

/// § 6's `keyboard_interactivity`. It is live, not part of [`SurfaceTopology`]:
/// `zwlr_layer_surface_v1::set_keyboard_interactivity` accepts changes on a mapped surface, so a
/// `Signal` is a value change (ADR-0044 decision 1).
pub fn parse_keyboard_interactivity(properties: &HashMap<String, Value>) -> Result<KeyboardInteractivity, LayoutError> {
    // Deferred on the evaluation-time pass ([`is_deferred_signal`]), same split as
    // [`parse_title`]'s: a field valid on a live surface is one only the resolved pass can read.
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

/// § 6's three exclusion answers mapped to `set_exclusive_zone`: positive reserves space, `0`
/// stays inside other reservations, and `-1` ignores them. A wallpaper anchored to all four edges
/// gets `0` from [`exclusive_zone_for`](crate::wayland), making boolean true/false identical there;
/// `Ignore` supplies the missing third answer, matching Quickshell's `ExclusionMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusive {
    /// Reserve screen area along the anchored edge, using the compositor-configured size.
    Reserve,
    /// Reserve nothing and stay inside other surfaces' reservations. The default, protocol `0`.
    Respect,
    /// Reserve nothing and ignore other reservations, covering the output. Protocol `-1`.
    Ignore,
}

/// § 6's `exclusive`: booleans preserve their meanings, and `"Ignore"` adds the third protocol
/// answer. It defaults to [`Exclusive::Respect`], so an undeclared panel floats over what is behind
/// it rather than pushing windows aside or covering them. The additive default preserves old
/// configs; `crate::wayland` computes the zone at configure time from the compositor's chosen size.
pub fn parse_exclusive(properties: &HashMap<String, Value>) -> Result<Exclusive, LayoutError> {
    // `exclusive = hide_bar` is a config § 5.1 permits and only this pass cannot read, so
    // `Respect` is the placeholder: a signal is unknown before its getter runs, and the other
    // answers are visible mistakes for a frame -- `Ignore` paints over the bar, `Reserve` shoves
    // every window aside.
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

/// Creation-time topology (`layer`, `anchor`, `monitor`, `namespace`, and `id`). The
/// order-sensitive
/// `Vec<SurfaceTopology>` diff in `renderer/src/socket.rs` triggers a swap; other `PanelSpec`
/// fields reload in place (ADR-0038 decision 2).
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

/// One `panel`'s layer-shell spec (§ 6): `surface_specs` builds it, `expand_instances` makes
/// per-output instances, and `App::create_panel` binds them. `handle_reevaluate` diffs only
/// `topology`, so `margin` reloads in place while `layer` swaps generation.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelSpec {
    /// Swap fingerprint: id, layer, anchor, monitor, namespace.
    pub topology: SurfaceTopology,
    pub keyboard_interactivity: KeyboardInteractivity,
    pub exclusive: Exclusive,
    /// § 6's root anchor offset, not spacing between root and child. `Scene::apply_one_surface`
    /// passes `None` for both parent-margin arguments when resolving a root. Below a root it
    /// remains ordinary layout margin parsed by [`parse_edge_insets`].
    pub margin: EdgeInsets,
    /// § 6's layer-shell `set_size` request. `SizeMode::Fill` is protocol `0`; percentages resolve
    /// against the output at the call site that knows it.
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
    /// answers are a wrong frame (a wallpaper over the bar, or every window shoved aside).
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
