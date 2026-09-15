//! Surface specs, swap fingerprints, child-list parsers, and masked `SecureSubmitTarget`.
//! List generation also owns duplicate-key rejection.

use std::collections::{HashMap, HashSet};

use mlua::Value;

use crate::lua::nodes::{VirtualNode, deserialize_lua_table};

use super::*;

/// A `lock` is in `BOX_KINDS` (`crate::lua::nodes`) and accepts the common and box properties;
/// [`lock_spec`] refuses only `visible`, `monitor`, `anchor`, `width`, and `height` (ADR-0052
/// decision 2). `child` is walked into the retained tree, so the spec carries no layout field. It
/// stays a struct rather than `SurfaceSpec::Lock(String)`, giving [`lock_spec`] a place to attach
/// those refusals. There is no `LockTopology`: the protocol exposes only `ack_configure`, with
/// size supplied by configure, so only declaration existence is fingerprinted.
#[derive(Debug, Clone, PartialEq)]
pub struct LockSpec {
    pub id: String,
}

/// Refuses unsupported properties before reading `id`. Ignoring `visible` could tear down a
/// compositor-owned lock at `locked`/`unlock_and_destroy`, causing ADR-0042's solid-color fallback;
/// the error reaches `rescue`'s `error_log` (ADR-0046) while unlocked. `monitor`, `anchor`,
/// `width`, and `height` are inert because configure owns geometry and lock surfaces cover every
/// output (ADR-0052 decision 2), but silent no-ops are still errors. A `Signal` under a refused key
/// is refused too; `is_deferred_signal` does not apply to a lock.
pub fn lock_spec(properties: &HashMap<String, Value>) -> Result<LockSpec, LayoutError> {
    for property in ["visible", "monitor", "anchor", "width", "height"] {
        if properties.contains_key(property) {
            return Err(invalid(
                property,
                format!(
                    "a `lock` takes no `{property}`: a lock surface covers every connected output, for exactly as long as the compositor holds \
                     the session locked, and none of that is the config's to set (ADR-0042, ADR-0052 decision 2)"
                ),
            ));
        }
    }
    Ok(LockSpec { id: parse_surface_id(properties)? })
}

/// One declared top-level surface, parsed by its role (ADR-0040 decision 1). Declaration order
/// remains in the roster and swap fingerprint, so one enum preserves it across roles.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceSpec {
    Panel(PanelSpec),
    Window(WindowSpec),
    Popup(PopupSpec),
    Lock(LockSpec),
}

impl SurfaceSpec {
    /// The declared id used to match a `SurfaceInstance` back to its `VirtualNode`.
    pub fn declared_id(&self) -> &str {
        match self {
            SurfaceSpec::Panel(spec) => &spec.topology.id,
            SurfaceSpec::Window(spec) => &spec.id,
            SurfaceSpec::Popup(spec) => &spec.id,
            SurfaceSpec::Lock(spec) => &spec.id,
        }
    }

    pub fn fingerprint(&self) -> SurfaceFingerprint {
        match self {
            SurfaceSpec::Panel(spec) => SurfaceFingerprint::Panel(spec.topology.clone()),
            SurfaceSpec::Window(spec) => SurfaceFingerprint::Window(spec.id.clone()),
            SurfaceSpec::Popup(spec) => SurfaceFingerprint::Popup(spec.id.clone()),
            SurfaceSpec::Lock(spec) => SurfaceFingerprint::Lock(spec.id.clone()),
        }
    }
}

/// A declaration's creation-time fields: one whose fingerprint changed is rebuilt in place
/// (ADR-0216). A `panel` carries all five topology fields; `window`, `popup`, and `lock` carry only
/// `id` because their other fields update live or rebuild per open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceFingerprint {
    Panel(SurfaceTopology),
    Window(String),
    Popup(String),
    Lock(String),
}

/// A single-node property converted with `deserialize_lua_table`.
pub fn parse_single_child(properties: &HashMap<String, Value>) -> Result<Option<VirtualNode>, LayoutError> {
    let Some(value) = properties.get("child") else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid("child", format!("expected a node table, got {}", preview_for_error(value))));
    };
    let node = deserialize_lua_table(table).map_err(|e| invalid("child", e.to_string()))?;
    Ok(Some(node))
}

/// An array-of-nodes `children` property.
pub fn parse_children(properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    let Some(value) = properties.get("children") else {
        return Ok(Vec::new());
    };
    let Value::Table(table) = value else {
        return Err(invalid("children", format!("expected an array table, got {}", preview_for_error(value))));
    };
    let mut children = Vec::new();
    for entry in table.sequence_values::<mlua::Table>() {
        if children.len() == MAX_ARRAY_ELEMENTS {
            return Err(invalid("children", format!("more than {MAX_ARRAY_ELEMENTS} children in one node")));
        }
        let entry = entry.map_err(|e| invalid("children", e.to_string()))?;
        let node = deserialize_lua_table(&entry).map_err(|e| invalid("children", e.to_string()))?;
        children.push(node);
    }
    Ok(children)
}

/// A `list`'s children (ADR-0045 decision 3) are generated once per resolved
/// `source` item; it arrives already resolved, so a `Signal` there was read exactly once before
/// `itemfn` runs. Without `key`, reconciliation is positional. With it, `key(element)`
/// is called on the source value, not the built node, and overwrites that node's `id`; duplicate
/// keys fail here before `pair_children_by_id_then_position` sees them.
///
/// ponytail: `key` speeds reconciliation, not evaluation. A 30-item tray still runs `itemfn` 30
/// times and discards 29 fresh nodes on ADR-0044 decision 2's per-poll-turn capability-push
/// cadence. `list` is a "fast-reconciling virtual repeater"; skipping unchanged items
/// needs retained-side data, which `children_of` does not provide. That is worth about 19% of the
/// pass (ADR-0132); the whole of it is a viewport, measured at 32us a row by
/// `layout::scene::tests::list_pass_cost` and designed in ADR-0191.
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
        if children.len() == MAX_ARRAY_ELEMENTS {
            return Err(invalid("source", format!("more than {MAX_ARRAY_ELEMENTS} items in one list")));
        }
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
            // List identity wins over any `id` the item function supplied.
            node.properties.insert("id".to_string(), Value::String(key_str));
        }

        children.push(node);
    }
    Ok(children)
}

/// `textfield.secure_submit` routes a masked field's committed buffer without Lua
/// (ADR-0005, ADR-0027). Enter is read from `wl_keyboard` in `renderer/src/wayland/mod.rs`, not
/// `zwp_text_input_v3`; the pair keys `RendererFrame::SecureSubmit` (ADR-0050 decision 4). See
/// `secure_key_action` for why a password must bypass the input-method bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureSubmitTarget {
    pub capability: String,
    pub action: String,
}

/// `secure_submit` is optional because an unread mask is unreadable from Lua, and it
/// is non-structural, so signal-bound values arrive resolved. `capability`/`action` reject
/// non-UTF-8 rather than collapsing distinct bytes onto one Supervisor capability name, as
/// [`parse_node_id`] does.
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
        let child = parse_single_child(&props).unwrap();
        assert_eq!(child.unwrap().kind, "rect");
    }

    #[test]
    fn parse_single_child_absent_is_none() {
        let props = HashMap::new();
        assert!(parse_single_child(&props).unwrap().is_none());
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
    fn lock_spec_reads_the_id_and_carries_nothing_else() {
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
    fn every_property_a_lock_denies_is_refused_by_name_rather_than_ignored() {
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

    /// The other half of the lock's denial, one layer up. A name a `lock` has no row for cannot
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

    /// Depth was capped and breadth was not, and the pass budget cannot cover the gap: filling this
    /// array is a Rust loop with no Lua in it, so the deadline hook never runs.
    #[test]
    fn a_children_array_wider_than_the_cap_is_a_config_error_rather_than_an_allocation() {
        let lua = mlua::Lua::new();
        crate::lua::nodes::register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local kids = {}
                for i = 1, 10001 do kids[i] = rect { width = 1, height = 1 } end
                return kids
                "#,
            )
            .eval()
            .unwrap();
        let mut properties = HashMap::new();
        properties.insert("children".to_string(), Value::Table(table));

        let err = parse_children(&properties).expect_err("past the cap this must be refused");
        assert!(format!("{err:?}").contains("more than"), "the error has to say what to fix: {err:?}");
    }

    /// The cap must not be in the way of anything a real config builds.
    #[test]
    fn an_ordinary_children_array_is_unaffected() {
        let lua = mlua::Lua::new();
        crate::lua::nodes::register_node_constructors(&lua).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { rect { width = 1, height = 1 }, rect { width = 2, height = 2 } }"#).eval().unwrap();
        let mut properties = HashMap::new();
        properties.insert("children".to_string(), Value::Table(table));

        assert_eq!(parse_children(&properties).unwrap().len(), 2);
    }
}
