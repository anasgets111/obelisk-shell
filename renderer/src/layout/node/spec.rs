//! The surface-spec envelope: [`LockSpec`] (§ 6.4), the [`SurfaceSpec`] enum every surface role
//! resolves to and its [`SurfaceFingerprint`] (the swap-detection key), the generic child-list
//! parsers (`parse_single_child`/`parse_children`/`parse_list_children`), and the masked
//! [`SecureSubmitTarget`] (§ 5.2 item 8).
//!
//! `parse_children`/`parse_list_children` are the two places a node's `kind` decides how its
//! `children`/`itemfn` property is walked, so they carry the `HashSet` dedup check for a `list`'s
//! keys.

use std::collections::{HashMap, HashSet};

use mlua::Value;

use crate::lua::nodes::{VirtualNode, deserialize_lua_table};

use super::*;

/// § 6.4's `lock`, whose whole property list is `id` and `child` (build-steps.md Phase 23,
/// docs/adr/0052 decision 2). `child` is not a field here for the same reason it is not one on the
/// other three roles: `layout::scene::children_of` walks it into the retained tree, and a spec
/// carries what the Wayland side has to be told, not what the layout engine reads.
///
/// So this is one field, and it stays a struct rather than collapsing into a
/// `SurfaceSpec::Lock(String)`: [`lock_spec`] is where § 6.4's four refusals live, and a bare
/// `String` variant would leave them with no parser to hang off.
///
/// **No `LockTopology`, for a stronger reason than [`PopupSpec`] has.** A lock surface has *no*
/// protocol field at all that a config could set: `ext_session_lock_surface_v1` has exactly one
/// request, `ack_configure`, and the size arrives in the configure rather than being asked for.
/// There is nothing for a topology diff to compare beyond the declaration's existence, which is
/// what [`SurfaceFingerprint::Lock`] holds.
#[derive(Debug, Clone, PartialEq)]
pub struct LockSpec {
    pub id: String,
}

/// § 6.4's parser. Refuses the properties § 6.4 says a `lock` does not have, then reads the one it
/// does.
///
/// **Refusing rather than ignoring is this parser's one real decision.** `visible = false` on a
/// lock screen implies the config decides when the lock is up, and it does not: the compositor
/// creates lock surfaces after `locked` and destroys them at `unlock_and_destroy`, and obeying the
/// property mid-session would destroy a surface the compositor is still showing -- docs/adr/0042
/// records that as what makes the compositor "fall back to rendering a solid color". Ignoring it
/// silently would leave the wrong mental model in place until the author meets it from the other
/// side, locked out by a screen that did not do what they wrote. An error lands in `rescue`'s
/// `error_log` (§ 2.10, docs/adr/0046) at evaluation time, where a human is reading and the session
/// is not locked -- the cheapest place the correction can happen.
///
/// `monitor`, `anchor`, `width` and `height` get the same treatment for a weaker reason: each is
/// inert rather than dangerous (a lock surface's geometry is entirely the compositor's configure,
/// and it expands per output because the protocol says so, not because a `monitor` asked --
/// docs/adr/0052 decision 2), and a property that quietly does nothing is worse unreported than
/// reported.
///
/// The refusals run *before* `id` is read, deliberately: `lock { visible = false }` with no `id`
/// has two problems, and leading with "missing `id`" would hide the one that says the author's
/// whole mental model of the role is wrong.
///
/// Nothing here consults [`is_deferred_signal`]: a refusal tests for the *key*, so a `Signal` under
/// it is refused exactly as a literal is, and § 6.4 leaves a `lock` no movable property for the
/// two-pass split docs/adr/0049's second amendment set up for `window` and `popup` to apply to.
pub fn lock_spec(properties: &HashMap<String, Value>) -> Result<LockSpec, LayoutError> {
    for property in ["visible", "monitor", "anchor", "width", "height"] {
        if properties.contains_key(property) {
            return Err(invalid(
                property,
                format!(
                    "§ 6.4 gives a `lock` no `{property}`: a lock surface covers every connected output, for exactly as long as the compositor holds \
                     the session locked, and none of that is the config's to set (docs/adr/0042, docs/adr/0052 decision 2)"
                ),
            ));
        }
    }
    Ok(LockSpec { id: parse_surface_id(properties)? })
}

/// One declared top-level surface, parsed by whichever § 6 role its `kind` names (docs/adr/0040
/// decision 1). `crate::socket`'s `surface_specs` builds one per node the evaluation returned, and
/// this is the roster every later stage reads: `layout::instance::expand_instances` turns it into
/// surface instances and `crate::wayland::App::create_surfaces` binds them.
///
/// One enum rather than three parallel lists, because the *order* of the declarations is part of
/// the swap fingerprint (see [`SurfaceFingerprint`]) and three lists would lose the interleaving.
/// It is also what keeps a surface's role one `match` away at every consumer instead of a lookup in
/// whichever list happens to hold it.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceSpec {
    Panel(PanelSpec),
    Window(WindowSpec),
    Popup(PopupSpec),
    Lock(LockSpec),
}

impl SurfaceSpec {
    /// The `id` this surface was declared with, whatever its role -- what
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
/// choose a generation swap over an in-place reload (docs/adr/0001, `CONTEXT.md`'s Topology
/// change). Order-sensitive equality on `Vec<SurfaceFingerprint>` is that diff.
///
/// The four roles contribute different amounts, and the protocol decides how much rather than a
/// preference. A `panel` carries all five of [`SurfaceTopology`]'s fields, because
/// `get_layer_surface` fixes every one of them at creation. A `window`, a `popup` and a `lock`
/// carry their `id` alone: everything else they hold is either a request on a live object
/// (`set_title`, `set_app_id`, the two size hints -- see [`WindowSpec`]'s own "no `WindowTopology`"
/// note) or rebuilt per open (the whole `xdg_positioner`, docs/adr/0049 decision 1), so none of it
/// can strand a live object the way a changed `namespace` would. A `lock` reaches the same
/// one-field answer from the other end: § 6.4 gives it `id` and `child` alone, so the only
/// question a topology diff can ask about it is whether it is still there.
///
/// What the three `id` arms *do* catch is the case docs/adr/0049 decision 3 names: adding or
/// removing a declaration is a topology change for every role, including the three whose Wayland
/// object comes and goes inside one generation. Deleting a `lock` mid-session is the sharpest case:
/// it is a topology change, so it is a swap, so docs/adr/0042's rule queues it until unlock and a
/// live lock screen cannot lose its tree underneath it (docs/adr/0052, Consequences).
///
/// The role itself is part of the fingerprint by construction: rewriting `panel { id = "x" }` as
/// `window { id = "x" }` changes the variant, which is a different Wayland object entirely and so a
/// swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceFingerprint {
    Panel(SurfaceTopology),
    Window(String),
    Popup(String),
    Lock(String),
}

/// A single-node property (`panel.child`), converted from its raw table via
/// `lua::nodes::deserialize_lua_table` -- not re-implemented here.
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

/// A `list` node's children (`oblisk-idl-api-specs.md` § 5.2 item 7, docs/adr/0045 decision 3,
/// build-steps.md Phase 19 item 12). Parallels [`parse_children`]'s role for
/// `rect`/`row`/`column`/`button`, but a `list`'s children are never a literal Lua table: they are
/// generated here, once per element of `source`, by calling `itemfn(element)` and deserializing the
/// node table it returns.
///
/// `source` arrives already resolved: `resolve_properties` treats it like any other non-structural
/// property, so a `Signal` there was read exactly once before this function ever runs.
///
/// Without `key`, a generated child gets no `id` at all, so
/// `layout::scene::pair_children_by_id_then_position` matches list items by position -- the same
/// rule an id-less literal child already gets, and exactly what decision 3 specifies. With `key`,
/// `key(element)` -- called on the source element, never on the node `itemfn` built -- becomes that
/// child's `id`, overwriting whatever `id` `itemfn`'s own node table carried: a list item's
/// identity belongs to the list, and honoring an inner `id` instead would let two items that happen
/// to declare the same one collide.
///
/// Duplicate keys are rejected here, before any `id` reaches `pair_children_by_id_then_position`,
/// so that function's own "duplicate id" message stays about a literal sibling `id` and a list
/// author gets a message naming `key`, the property they actually wrote.
///
/// ponytail: `key` makes *reconciliation* cheap, not *evaluation*. This calls `itemfn` for every
/// element on every resolve, so a 30-item tray builds 30 fresh nodes each time, and
/// `pair_children_by_id_then_position` then matches 29 of them to retained nodes and throws the
/// fresh ones away. § 5.2 calls `list` a "fast-reconciling virtual repeater", and the reconciling
/// half is what ADR-0045 delivered; the repeater half still re-runs a Lua closure per item per
/// pass. `resolve_and_reconcile` runs per `Scene::apply`, which ADR-0044 decision 2's dirty flag
/// made per poll turn rather than per config edit, so this is the same cadence change item 14's
/// text-reshaping `ponytail:` records against `intrinsic_content_size`.
///
/// The fix is to compute keys first and skip `itemfn` for an element whose key already matches a
/// retained child, which is what makes it a virtual repeater rather than a loop. It is not built
/// here because this function cannot see the retained children: `children_of` hands it only the
/// fresh node's own properties, and giving it the retained side means changing that signature and
/// the two other `children_of` arms with it. Worth doing when a real config drives a list from a
/// capability that pushes often, not before.
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
            // The key wins over any `id` the node itemfn built already carried -- see this
            // function's doc comment.
            node.properties.insert("id".to_string(), Value::String(key_str));
        }

        children.push(node);
    }
    Ok(children)
}

/// `textfield.secure_submit` (§ 5.2 item 8): the `{ capability, action }` pair a masked field's
/// committed buffer is addressed to once the focused field submits, instead of the value ever
/// reaching Lua (docs/adr/0005, docs/adr/0027). The submit is Enter on `wl_keyboard`, read natively
/// in `renderer/src/wayland/mod.rs` -- ADR-0027's `zwp_text_input_v3` bridge was the original
/// transport and no longer carries this path at all; see that file's `secure_key_action` for why a
/// password must not travel through an input method. This pair becomes the routing key on a
/// `RendererFrame::SecureSubmit` envelope (docs/adr/0050 decision 4), which is why both fields are
/// required rather than falling back to some default capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureSubmitTarget {
    pub capability: String,
    pub action: String,
}

/// `Ok(None)` when the property is absent -- `secure_submit` is optional even on a masked field
/// (§ 5.2 item 8's own note: without it, a masked value is just unreadable from Lua).
///
/// `secure_submit` is not in [`is_structural_property`]'s carve-out, so a signal-bound value
/// arrives here already resolved -- nothing reconciles a node by its `secure_submit`, so there is
/// no structural decision here for a live-changing signal to undermine.
///
/// `capability`/`action` are refused non-UTF-8 rather than converted lossily, the same call
/// [`parse_node_id`] makes for the same reason: this pair addresses a secret to a Supervisor
/// capability, so a lossy conversion could collapse two distinct byte strings onto the same name
/// and route a password to a capability nobody registered.
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

    /// The other half of the § 6.4 denial, one layer up. A name a `lock` has no row for cannot
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
