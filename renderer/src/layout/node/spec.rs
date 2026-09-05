//! The surface-spec envelope: [`LockSpec`] (§ 6), the [`SurfaceSpec`] enum every surface role
//! resolves to and its [`SurfaceFingerprint`] (swap-detection key), the generic child-list parsers
//! (`parse_single_child`/`parse_children`/`parse_list_children`), and the masked
//! [`SecureSubmitTarget`] (§ 5.2 item 8). `parse_children`/`parse_list_children` are where a
//! node's `kind` decides how its `children`/`itemfn` property is walked, carrying the `HashSet`
//! dedup check for a `list`'s keys.

use std::collections::{HashMap, HashSet};

use mlua::Value;

use crate::lua::nodes::{VirtualNode, deserialize_lua_table};

use super::*;

/// § 6's `lock`: `id` and `child` are its whole property list (ADR-0052 decision 2). `child`
/// is not a field here for the same reason it is not one on the other three roles:
/// `layout::scene::children_of` walks it into the retained tree, and a spec carries what Wayland
/// needs told, not what layout reads. Stays a struct rather than `SurfaceSpec::Lock(String)`:
/// [`lock_spec`] hangs § 6's four refusals off it, which a bare `String` variant could not.
///
/// **No `LockTopology`, for a stronger reason than [`PopupSpec`] has:** a lock surface has *no*
/// protocol field a config could set. `ext_session_lock_surface_v1` has one request,
/// `ack_configure`, and its size arrives in the configure, never asked for. Nothing exists to
/// diff beyond the declaration's existence, which [`SurfaceFingerprint::Lock`] holds.
#[derive(Debug, Clone, PartialEq)]
pub struct LockSpec {
    pub id: String,
}

/// § 6's parser: refuses the properties a `lock` does not have, then reads the one it does.
///
/// Refusing rather than ignoring is the real decision. `visible = false` implies the config
/// decides when the lock is up, but the compositor creates and destroys lock surfaces itself, at
/// `locked` and `unlock_and_destroy`; obeying it mid-session would tear down a surface it still
/// shows, which ADR-0042 says makes it "fall back to rendering a solid color". The error lands in
/// `rescue`'s `error_log` (§ 2.10, ADR-0046) at evaluation time instead, while a human is reading
/// and the session is not locked.
///
/// `monitor`, `anchor`, `width` and `height` get the same treatment for a weaker reason: each is
/// inert rather than dangerous (geometry is entirely the compositor's configure, expanding per
/// output because the protocol says so, not a `monitor`, per ADR-0052 decision 2), but a silent
/// no-op is still worse than a reported one. The refusals run before `id` is read, so a `lock`
/// missing both leads with the problem about the role, not the merely missing `id`.
/// [`is_deferred_signal`] is never consulted either: a refusal tests the *key*, so a `Signal`
/// under it is refused like a literal, since § 6 leaves `lock` no movable property for
/// ADR-0049's second amendment to apply to.
pub fn lock_spec(properties: &HashMap<String, Value>) -> Result<LockSpec, LayoutError> {
    for property in ["visible", "monitor", "anchor", "width", "height"] {
        if properties.contains_key(property) {
            return Err(invalid(
                property,
                format!(
                    "§ 6.4 gives a `lock` no `{property}`: a lock surface covers every connected output, for exactly as long as the compositor holds \
                     the session locked, and none of that is the config's to set (ADR-0042, ADR-0052 decision 2)"
                ),
            ));
        }
    }
    Ok(LockSpec { id: parse_surface_id(properties)? })
}

/// One declared top-level surface, parsed by whichever § 6 role its `kind` names (ADR-0040
/// decision 1). `crate::socket`'s `surface_specs` builds one per evaluated node; later stages
/// read this roster, `expand_instances` turning it into surface instances and `create_surfaces`
/// binding them. One enum rather than three parallel lists: declaration *order* is part of the
/// swap fingerprint (see [`SurfaceFingerprint`]), which three lists would lose, and it keeps a
/// surface's role one `match` away instead of a lookup in whichever list holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceSpec {
    Panel(PanelSpec),
    Window(WindowSpec),
    Popup(PopupSpec),
    Lock(LockSpec),
}

impl SurfaceSpec {
    /// The `id` this surface was declared with, whatever its role: what
    /// `layout::scene::Scene`'s apply matches a `SurfaceInstance` back to its `VirtualNode` by.
    pub fn declared_id(&self) -> &str {
        match self {
            SurfaceSpec::Panel(spec) => &spec.topology.id,
            SurfaceSpec::Window(spec) => &spec.id,
            SurfaceSpec::Popup(spec) => &spec.id,
            SurfaceSpec::Lock(spec) => &spec.id,
        }
    }

    /// This declaration's share of the swap fingerprint.
    pub fn fingerprint(&self) -> SurfaceFingerprint {
        match self {
            SurfaceSpec::Panel(spec) => SurfaceFingerprint::Panel(spec.topology.clone()),
            SurfaceSpec::Window(spec) => SurfaceFingerprint::Window(spec.id.clone()),
            SurfaceSpec::Popup(spec) => SurfaceFingerprint::Popup(spec.id.clone()),
            SurfaceSpec::Lock(spec) => SurfaceFingerprint::Lock(spec.id.clone()),
        }
    }
}

/// One declared surface's share of the topology `crate::socket`'s `handle_reevaluate` diffs to
/// choose a generation swap over an in-place reload (ADR-0001, `CONTEXT.md`'s Topology change).
/// Order-sensitive equality on `Vec<SurfaceFingerprint>` is that diff.
///
/// The protocol decides how much each role contributes. A `panel` carries all five of
/// [`SurfaceTopology`]'s fields because `get_layer_surface` fixes every one at creation. A
/// `window`, `popup` and `lock` carry only their `id`: everything else is a request on a live
/// object (`set_title`, `set_app_id`, the two size hints, see [`WindowSpec`]'s "no
/// `WindowTopology`" note) or rebuilt per open (`xdg_positioner`, ADR-0049 decision 1), so none of
/// it can strand a live object the way a changed `namespace` would; a `lock` lands on the same
/// one field because § 6 gives it only `id` and `child` to begin with.
///
/// The three `id` arms still catch ADR-0049 decision 3: adding or removing a declaration is a
/// topology change for every role, even one whose Wayland object comes and goes inside a
/// generation. Deleting a `lock` mid-session is the sharpest case: ADR-0042's rule queues that
/// swap until unlock, so a live lock screen cannot lose its tree underneath it (ADR-0052,
/// Consequences). The role itself is part of the fingerprint too: rewriting `panel { id = "x" }`
/// as `window { id = "x" }` changes the variant, a different Wayland object and so a swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceFingerprint {
    Panel(SurfaceTopology),
    Window(String),
    Popup(String),
    Lock(String),
}

/// A single-node property (`panel.child`), converted from its raw table via
/// `lua::nodes::deserialize_lua_table`, not re-implemented here.
pub fn parse_single_child(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<Option<VirtualNode>, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid(property, format!("expected a node table, got {}", preview_for_error(value))));
    };
    let node = deserialize_lua_table(table).map_err(|e| invalid(property, e.to_string()))?;
    Ok(Some(node))
}

/// An array-of-nodes property (`rect`/`row`/`column`/`button.children`).
pub fn parse_children(properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    let Some(value) = properties.get("children") else {
        return Ok(Vec::new());
    };
    let Value::Table(table) = value else {
        return Err(invalid("children", format!("expected an array table, got {}", preview_for_error(value))));
    };
    let mut children = Vec::new();
    for entry in table.sequence_values::<mlua::Table>() {
        let entry = entry.map_err(|e| invalid("children", e.to_string()))?;
        let node = deserialize_lua_table(&entry).map_err(|e| invalid("children", e.to_string()))?;
        children.push(node);
    }
    Ok(children)
}

/// A `list` node's children (`oblisk-idl-api-specs.md` § 5.2 item 7, ADR-0045 decision 3).
/// Parallels [`parse_children`] for `rect`/`row`/`column`/`button`, but a `list`'s children are
/// never a literal Lua table: they are generated here, once per element of `source`, by calling
/// `itemfn(element)` and deserializing the node table it returns. `source` arrives already
/// resolved: `resolve_properties` treats it like any other non-structural property, so a `Signal`
/// there was read exactly once before this runs.
///
/// Without `key`, a generated child gets no `id`, so `pair_children_by_id_then_position` matches
/// list items by position, the rule an id-less literal child already gets and exactly what
/// decision 3 specifies. With `key`, `key(element)` (called on the source element, never on the
/// node `itemfn` built) becomes that child's `id`, overwriting whatever `itemfn`'s own table
/// carried: identity belongs to the list, so an inner `id` could let two items collide. Duplicate
/// keys are rejected here, before any `id` reaches `pair_children_by_id_then_position`, so a list
/// author gets a message naming `key`, the property they actually wrote.
///
/// ponytail: `key` makes reconciliation cheap, not evaluation. `itemfn` still runs for every
/// element on every resolve, so a 30-item tray builds 30 fresh nodes and throws 29 away each
/// pass, at ADR-0044 decision 2's per-poll-turn cadence (§ 5.2 calls `list` a "fast-reconciling
/// virtual repeater"). The fix, computing keys first and skipping `itemfn` for unchanged ones,
/// is not built because `children_of` hands this only the fresh node's properties, never the
/// retained side.
pub fn parse_list_children(properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    let source_value = properties.get("source").ok_or_else(|| invalid("source", "required for `list`, got nothing"))?;
    let Value::Table(source) = source_value else {
        return Err(invalid("source", format!("expected an array table, got {}", preview_for_error(source_value))));
    };

    let itemfn = match properties.get("itemfn") {
        Some(Value::Function(f)) => f,
        Some(other) => return Err(invalid("itemfn", format!("expected a function, got {}", preview_for_error(other)))),
        None => return Err(invalid("itemfn", "required for `list`, got nothing")),
    };

    let key_fn = match properties.get("key") {
        Some(Value::Function(f)) => Some(f),
        Some(other) => return Err(invalid("key", format!("expected a function, got {}", preview_for_error(other)))),
        None => None,
    };

    let mut children = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    for element in source.sequence_values::<Value>() {
        let element = element.map_err(|e| invalid("source", e.to_string()))?;

        let built = itemfn.call::<Value>(element.clone()).map_err(|e| invalid("itemfn", e.to_string()))?;
        let Value::Table(built_table) = built else {
            return Err(invalid("itemfn", format!("expected a node table, got {}", preview_for_error(&built))));
        };
        let mut node = deserialize_lua_table(&built_table).map_err(|e| invalid("itemfn", e.to_string()))?;

        if let Some(key_fn) = key_fn {
            let key_value = key_fn.call::<Value>(element).map_err(|e| invalid("key", e.to_string()))?;
            let Value::String(key_str) = key_value else {
                return Err(invalid(
                    "key",
                    format!("expected key(item) to return a string, got {}", preview_for_error(&key_value)),
                ));
            };
            let key_text = key_str.to_str().map(|s| s.to_string()).map_err(|_| {
                invalid(
                    "key",
                    "must be valid UTF-8 -- a key is compared for equality, so it cannot be converted lossily",
                )
            })?;
            if !seen_keys.insert(key_text.clone()) {
                return Err(invalid("key", format!("duplicate key `{key_text}` among list items")));
            }
            // The key wins over any `id` itemfn's node already carried; see this fn's doc comment.
            node.properties.insert("id".to_string(), Value::String(key_str));
        }

        children.push(node);
    }
    Ok(children)
}

/// `textfield.secure_submit` (§ 5.2 item 8): the `{ capability, action }` pair a masked field's
/// committed buffer is addressed to once it submits, instead of reaching Lua (ADR-0005,
/// ADR-0027). Submit is Enter on `wl_keyboard`, read natively in `renderer/src/wayland/mod.rs`,
/// not through the `zwp_text_input_v3` bridge (see that file's `secure_key_action` for why a
/// password must not travel through an input method). This pair is the routing key on a
/// `RendererFrame::SecureSubmit` envelope (ADR-0050 decision 4), so both fields are required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureSubmitTarget {
    pub capability: String,
    pub action: String,
}

/// `Ok(None)` when absent: `secure_submit` is optional even on a masked field (§ 5.2 item 8: an
/// unread mask is just unreadable from Lua). Not in [`is_structural_property`]'s carve-out, so a
/// signal-bound value arrives already resolved: nothing reconciles a node by its `secure_submit`,
/// so there is no structural decision here for a live-changing signal to undermine.
///
/// `capability`/`action` are refused non-UTF-8 rather than converted lossily, the same call
/// [`parse_node_id`] makes: this pair addresses a secret to a Supervisor capability, and a lossy
/// conversion could collapse two distinct byte strings onto one name, routing a password to a
/// capability nobody registered.
pub fn parse_secure_submit(properties: &HashMap<String, Value>) -> Result<Option<SecureSubmitTarget>, LayoutError> {
    let Some(value) = properties.get("secure_submit") else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid("secure_submit", format!("expected a table, got {}", preview_for_error(value))));
    };
    let field = |key: &str| -> Result<String, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("secure_submit", e.to_string()))?;
        let s = match v {
            Value::Nil => return Err(invalid("secure_submit", format!("`{key}` is required"))),
            Value::String(s) => s,
            other => {
                return Err(invalid(
                    "secure_submit",
                    format!("`{key}` must be a string, got {}", preview_for_error(&other)),
                ));
            }
        };
        let s = s.to_str().map(|s| s.to_string()).map_err(|_| {
            invalid(
                "secure_submit",
                format!(
                    "`{key}` must be valid UTF-8 -- it addresses a Supervisor capability, so it cannot be converted lossily"
                ),
            )
        })?;
        if s.is_empty() {
            return Err(invalid("secure_submit", format!("`{key}` must not be empty")));
        }
        Ok(s)
    };
    Ok(Some(SecureSubmitTarget { capability: field("capability")?, action: field("action")? }))
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
    fn parse_children_walks_nested_node_tables() {
        let lua = lua();
        let table: mlua::Table = lua
                .load(r#"return { kind = "row", children = { { kind = "text", content = "a" }, { kind = "text", content = "b" } } }"#)
                .eval()
                .unwrap();
        let props = props_from_table(&table);
        let children = parse_children(&props).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].kind, "text");
        assert_eq!(children[1].properties.get("content").unwrap().as_string().unwrap().to_string_lossy(), "b");
    }

    #[test]
    fn parse_single_child_converts_the_child_table() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", child = { kind = "rect" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let child = parse_single_child(&props, "child").unwrap();
        assert_eq!(child.unwrap().kind, "rect");
    }

    #[test]
    fn parse_single_child_absent_is_none() {
        let props = HashMap::new();
        assert!(parse_single_child(&props, "child").unwrap().is_none());
    }

    #[test]
    fn secure_submit_absent_is_none() {
        let props = HashMap::new();
        assert_eq!(parse_secure_submit(&props).unwrap(), None);
    }

    #[test]
    fn secure_submit_well_formed_table_parses_capability_and_action() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "network", action = "connect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_secure_submit(&props).unwrap(),
            Some(SecureSubmitTarget { capability: "network".to_string(), action: "connect".to_string() })
        );
    }

    #[test]
    fn secure_submit_missing_capability_is_invalid_property_naming_the_field() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "textfield", secure_submit = { action = "connect" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("capability")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_missing_action_is_invalid_property_naming_the_field() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "textfield", secure_submit = { capability = "network" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("action")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_empty_capability_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "", action = "connect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("capability")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_empty_action_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "network", action = "" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("action")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_non_table_value_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "textfield", secure_submit = "network.connect" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_secure_submit(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "secure_submit"
        ));
    }

    #[test]
    fn secure_submit_non_utf8_capability_is_rejected_rather_than_lossily_converted() {
        let lua = lua();
        let table = lua.create_table().unwrap();
        table.set("kind", "textfield").unwrap();
        let inner = lua.create_table().unwrap();
        inner.set("capability", lua.create_string(b"\xff").unwrap()).unwrap();
        inner.set("action", "connect").unwrap();
        table.set("secure_submit", inner).unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "secure_submit"),
            "a non-UTF-8 secure_submit field must be a LayoutError naming the property: {err:?}"
        );
    }

    #[test]
    fn lock_spec_reads_the_id_and_that_is_the_whole_of_section_6_4() {
        let lua = lua();
        let table: mlua::Table =
            lua.load(r#"return { kind = "lock", id = "screen-lock", child = { kind = "rect" } }"#).eval().unwrap();
        assert_eq!(lock_spec(&props_from_table(&table)).unwrap(), LockSpec { id: "screen-lock".to_string() });
    }

    #[test]
    fn a_lock_without_an_id_is_rejected_the_same_way_every_other_role_is() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "lock" }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "id"
        ));
    }

    #[test]
    fn every_property_section_6_4_denies_a_lock_is_refused_by_name_rather_than_ignored() {
        let lua = lua();
        // `monitor` and `anchor` never reach `lock_spec` from a config any more: they are not on
        // `lock`'s row in `nodes::NODE_PROPERTIES`, so `deserialize_lua_table` refuses them first
        // (`a_lock_property_that_is_not_even_on_the_kind_is_refused_before_lock_spec_sees_it`).
        // The three left here are ones a lock legitimately has a row for and refuses anyway.
        for property in ["visible", "width", "height"] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "lock", id = "screen-lock", {property} = 1 }}"#)).eval().unwrap();
            let err = lock_spec(&props_from_table(&table)).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property),
                "`{property}` must be refused by name, got {err:?}"
            );
        }
    }

    /// The other half of the § 6 denial, one layer up. A name a `lock` has no row for cannot
    /// reach `lock_spec` at all, so the refusal a config author sees is the property gate's.
    #[test]
    fn a_lock_property_that_is_not_even_on_the_kind_is_refused_before_lock_spec_sees_it() {
        let lua = lua();
        for property in ["monitor", "anchor"] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "lock", id = "screen-lock", {property} = 1 }}"#)).eval().unwrap();
            let err = crate::lua::nodes::deserialize_lua_table(&table).unwrap_err();
            assert!(err.to_string().contains(property), "`{property}` must be refused by name, got {err}");
        }
    }

    #[test]
    fn a_refused_lock_property_wins_over_a_missing_id_because_it_is_the_error_that_teaches() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "lock", visible = false }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "visible"
        ));
    }

    #[test]
    fn a_signal_bound_lock_property_is_refused_on_the_evaluation_pass_like_a_literal_one() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { kind = "lock", id = "screen-lock", visible = state("v", true) }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "visible"
        ));
    }

    #[test]
    fn a_signal_in_a_lock_id_is_rejected_by_the_universal_structural_arm_with_no_new_carve_out() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { kind = "lock", id = state("i", "screen-lock") }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "lock", &lua).unwrap();
        assert!(matches!(
            lock_spec(&resolved).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "id"
        ));
    }

    #[test]
    fn a_lock_fingerprints_on_its_id_alone_so_only_its_existence_is_a_topology_change() {
        let spec = SurfaceSpec::Lock(LockSpec { id: "screen-lock".to_string() });
        assert_eq!(spec.declared_id(), "screen-lock");
        assert_eq!(spec.fingerprint(), SurfaceFingerprint::Lock("screen-lock".to_string()));
        assert_ne!(spec.fingerprint(), SurfaceFingerprint::Window("screen-lock".to_string()));
    }
}
