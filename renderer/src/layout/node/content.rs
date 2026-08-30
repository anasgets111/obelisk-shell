//! Leaf-node content parsers: text content, icon name/size, image source/fit, font size,
//! foreground color, and the two identity strings (`id`, `surface_id`). None of these carry
//! children or affect the box model -- they are read once per paint, not per layout pass.
//!
//! `parse_string_property` is `pub(super)`: `surface`, `toplevel` and `popup`-adjacent code in
//! `toplevel` reuse it for `namespace`, `title`, `app_id` and the like.

use std::collections::HashMap;

use mlua::Value;

use crate::image::Fit;

use super::*;

/// Absent `content` defaults to the empty string. It used to be required, but decision 1's nil
/// rule (docs/adr/0044) means a `text` bound to a bare, not-yet-pushed capability signal resolves
/// `content` to absent at boot, since every rostered signal reads `nil` until its first
/// `StateSnapshot` and `run_startup_evaluation` runs before the poll loop drains one. Rejecting
/// that would reject the whole tree and boot a blank shell.
///
/// Accepted cost: a misspelled `content` key now renders an empty node instead of being rejected --
/// the better failure for a shell that has to boot; `oblisk.rescue` still exists for the failures
/// that matter.
pub fn parse_content(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "content")
}

/// `icon.name` (§ 5.2 item 5): a theme name, or an absolute path, which `image::icons::resolve`
/// tells apart. Defaults to `""` for the same boot reason `content` does (docs/adr/0044): a `name`
/// bound to a capability signal is `nil` until that capability's first push, and rejecting the
/// tree over it would fail every config that binds one.
pub fn parse_icon_name(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "name")
}

/// `image.source` (docs/adr/0054 decision 3): an absolute path, never a theme name. The split from
/// [`parse_icon_name`] is the whole difference between the two node kinds, so they do not share a
/// property spelling either.
pub fn parse_image_source(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "source")
}

/// `image.fit` (docs/adr/0055 decision 3). Absent is `cover`; a string that is not one of the three
/// modes is an error rather than a silent fallback, because `fit = "fill"` is a config author
/// reaching for a mode that does not exist and a silently-covered image would hide that.
pub fn parse_fit(properties: &HashMap<String, Value>) -> Result<Fit, LayoutError> {
    let Some(value) = properties.get("fit") else {
        return Ok(Fit::default());
    };
    let Value::String(s) = value else {
        return Err(invalid("fit", format!("expected a string, got {}", preview_for_error(value))));
    };
    let s = checked_string("fit", s)?;
    Fit::from_str(&s).ok_or_else(|| invalid("fit", format!("expected `cover`, `contain` or `stretch`, got {s:?}")))
}

/// The shared shape of every § 5.2 string property that defaults to empty when absent. One
/// function rather than three copies of the same six lines.
fn parse_optional_string(properties: &HashMap<String, Value>, property: &str) -> Result<String, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(String::new());
    };
    match value {
        Value::String(s) => checked_string(property, s),
        other => Err(invalid(property, format!("expected a string, got {}", preview_for_error(other)))),
    }
}

/// `text.foreground` (§ 5.2 item 4). Absent defaults to white -- `layout::paint`'s `paint_text`
/// falls back to the same white whenever this parser errors on a present-but-malformed value, so
/// the rendered result agrees whether the key was omitted or rejected.
pub fn parse_foreground(properties: &HashMap<String, Value>) -> Result<Rgba, LayoutError> {
    let Some(value) = properties.get("foreground") else {
        return Ok(Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        });
    };
    let Value::String(s) = value else {
        return Err(invalid(
            "foreground",
            format!("expected a string, got {}", preview_for_error(value)),
        ));
    };
    let s = checked_string("foreground", s)?;
    parse_hex_color("foreground", &s)
}

pub fn parse_font_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("font_size") else {
        return Ok(12.0);
    };
    value_as_f32("font_size", value)?
        .ok_or_else(|| invalid("font_size", format!("expected a number, got {}", preview_for_error(value))))
}

/// Absent `size` defaults to 12.0, the same nil-rule rationale as [`parse_content`] (docs/adr/0044's
/// amendment banner): `icon` was the second property the amendment names as still failing after
/// decision 1's nil rule alone. Same accepted cost: `icon { sizee = 24 }` now renders a
/// 12.0-sized icon instead of being rejected.
///
/// § 5.2 documents `size` with no default of its own, so this matches [`parse_font_size`]'s
/// default: `text` and `icon` are the two leaf kinds sized by one numeric property, so an icon
/// dropped inline with default-sized text lands at the same visual scale.
pub fn parse_icon_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("size") else {
        return Ok(12.0);
    };
    value_as_f32("size", value)?
        .ok_or_else(|| invalid("size", format!("expected a number, got {}", preview_for_error(value))))
}

/// Shared shape behind [`parse_surface_id`]/`surface::parse_layer`/`surface::parse_monitor`: fetch `property`,
/// reject a `Signal`, require it to be a string. `default` supplies the value when the property
/// is absent; `None` makes it required, erroring instead (Standards review, docs/adr/0024).
pub(super) fn parse_string_property(properties: &HashMap<String, Value>, property: &str, default: Option<&str>) -> Result<String, LayoutError> {
    let value = match properties.get(property) {
        Some(value) => value,
        None => match default {
            Some(default) => return Ok(default.to_string()),
            None => return Err(invalid(property, format!("surface node requires `{property}`"))),
        },
    };
    reject_signal_in_structural_field(property, value)?;
    match value {
        Value::String(s) => Ok(s.to_string_lossy()),
        other => Err(invalid(property, format!("expected a string, got {}", preview_for_error(other)))),
    }
}

/// A top-level surface's `id`: required, unique among the surfaces in one config, and keys
/// `Scene::apply`'s `HashMap` (`docs/oblisk-layout-engine-geometry.md` § 4). Since docs/adr/0045,
/// this same property is also the surface's *reconcile* identity: the root of a tree is the one
/// node whose retained counterpart is found by key lookup rather than by [`parse_node_id`]'s
/// per-parent pairing, because a surface has no parent to be scoped within -- decision 5 is
/// explicit that this is the same mechanism restated at the level below, not a second one.
pub fn parse_surface_id(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "id", None)
}

/// The optional `id` base property on every node kind, one level below a surface's root
/// (docs/adr/0045 decisions 1-2). `None` means "no id" and is not an error --
/// `pair_children_by_id_then_position` pairs a child that carries none positionally against the
/// other id-less children (ADR-0023's original rule applied to that subsequence). Adding or
/// dropping an `id` is a change of identity, not a cosmetic edit: the retained counterpart is
/// retired and a new node allocated. Rejects a `Signal` via [`reject_signal_in_structural_field`]
/// for the same reason [`parse_surface_id`] does: this is a reconcile identity, decided once at
/// match time, not a value that should drift between the fresh tree and whatever the match
/// produces.
///
/// Non-UTF-8 bytes are refused rather than converted, unlike [`checked_string`]'s lossy handling of
/// display-oriented properties like `content`. An id is an *equality key*: with `to_string_lossy`,
/// `"\xFF"` and `"\xFE"` both become `U+FFFD` and two genuinely distinct ids compare equal, so
/// `pair_children_by_id_then_position`'s duplicate check would reject a valid config and a fresh
/// child could claim the wrong retained counterpart.
///
/// Scoping ("unique among siblings, not across the tree") and duplicate rejection are
/// `pair_children_by_id_then_position`'s job, not this parser's: a duplicate can only be detected
/// by comparing this node's id against its siblings', which this function has no visibility into.
pub fn parse_node_id(properties: &HashMap<String, Value>) -> Result<Option<String>, LayoutError> {
    let Some(value) = properties.get("id") else {
        return Ok(None);
    };
    reject_signal_in_structural_field("id", value)?;
    match value {
        Value::String(s) => s
            .to_str()
            .map(|s| Some(s.to_string()))
            .map_err(|_| invalid("id", "must be valid UTF-8 -- an id is compared for equality, so it cannot be converted lossily")),
        other => Err(invalid("id", format!("expected a string, got {}", preview_for_error(other)))),
    }
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
        fn text_content_absent_defaults_to_the_empty_string() {
            let props = HashMap::new();
            assert_eq!(parse_content(&props).unwrap(), "");
        }

        #[test]
        fn a_signal_resolving_to_a_string_satisfies_content() {
            let lua = lua();
            crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
            let hello = lua.create_string("hello").unwrap();
            let signal = crate::lua::signal::Signal::new_live(Value::String(hello), crate::lua::signal::DirtyFlag::new()).0;
            let table = lua.create_table().unwrap();
            table.set("kind", "text").unwrap();
            table.set("content", signal).unwrap();
            let node = deserialize_lua_table(&table).unwrap();
            assert_eq!(parse_content(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap(), "hello");
        }

        #[test]
        fn a_signal_resolving_to_a_table_reports_the_same_error_a_literal_table_would() {
            let lua = lua();
            crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();

            let literal_table: mlua::Table = lua.load(r#"return { kind = "text", content = {} }"#).eval().unwrap();
            let literal_props = props_from_table(&literal_table);
            let literal_err = parse_content(&resolve_properties(&literal_props, "text", &lua).unwrap()).unwrap_err();

            let signal = crate::lua::signal::Signal::new_live(Value::Table(lua.create_table().unwrap()), crate::lua::signal::DirtyFlag::new()).0;
            let table = lua.create_table().unwrap();
            table.set("kind", "text").unwrap();
            table.set("content", signal).unwrap();
            let node = deserialize_lua_table(&table).unwrap();
            let signal_err = parse_content(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap_err();

            for err in [&literal_err, &signal_err] {
                assert!(matches!(
                    err,
                    LayoutError::InvalidProperty { property, detail }
                        if property == "content" && detail.starts_with("expected a string")
                ));
            }
        }

        #[test]
        fn fit_rejects_a_mode_that_does_not_exist_rather_than_covering_silently() {
            let lua = lua();
            let table: mlua::Table = lua.load(r#"return { kind = "image", fit = "fill" }"#).eval().unwrap();
            let err = parse_fit(&props_from_table(&table)).unwrap_err();
            assert!(format!("{err}").contains("cover"), "the error should name the modes that do exist, got {err}");

            let table: mlua::Table = lua.load(r#"return { kind = "image", fit = 3 }"#).eval().unwrap();
            assert!(parse_fit(&props_from_table(&table)).is_err());
        }

        #[test]
        fn an_image_source_that_is_not_a_string_is_rejected() {
            let lua = lua();
            let table: mlua::Table = lua.load(r#"return { kind = "image", source = 5 }"#).eval().unwrap();
            assert!(parse_image_source(&props_from_table(&table)).is_err());
            let table: mlua::Table = lua.load(r#"return { kind = "image", source = "/tmp/w.png" }"#).eval().unwrap();
            assert_eq!(parse_image_source(&props_from_table(&table)).unwrap(), "/tmp/w.png");
        }

        #[test]
        fn font_size_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
            let lua = lua();
            let table: mlua::Table = lua
                .load(r#"return { kind = "text", font_size = 1e300 }"#)
                .eval()
                .unwrap();
            let props = props_from_table(&table);
            assert!(matches!(
                parse_font_size(&props).unwrap_err(),
                LayoutError::InvalidProperty { property, .. } if property == "font_size"
            ));
        }

        #[test]
        fn font_size_absent_defaults_to_twelve() {
            let props = HashMap::new();
            assert_eq!(parse_font_size(&props).unwrap(), 12.0);
        }

        #[test]
        fn a_signal_resolving_to_a_number_satisfies_font_size_through_marshals_check_number() {
            let lua = lua();
            crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
            let signal = crate::lua::signal::Signal::new_live(Value::Number(18.0), crate::lua::signal::DirtyFlag::new()).0;
            let table = lua.create_table().unwrap();
            table.set("kind", "text").unwrap();
            table.set("font_size", signal).unwrap();
            let node = deserialize_lua_table(&table).unwrap();
            assert_eq!(parse_font_size(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap(), 18.0);
        }

        #[test]
        fn icon_size_absent_defaults_to_twelve() {
            let props = HashMap::new();
            assert_eq!(parse_icon_size(&props).unwrap(), 12.0);
        }

        #[test]
        fn node_id_absent_is_none() {
            let props = HashMap::new();
            assert_eq!(parse_node_id(&props).unwrap(), None);
        }

        #[test]
        fn node_id_reads_the_string() {
            let lua = lua();
            let table: mlua::Table = lua.load(r#"return { kind = "rect", id = "handle" }"#).eval().unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_node_id(&props).unwrap(), Some("handle".to_string()));
        }

        #[test]
        fn a_signal_userdata_in_node_id_is_rejected() {
            let lua = lua();
            crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
            let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
            let table = lua.create_table().unwrap();
            table.set("kind", "rect").unwrap();
            table.set("id", signal).unwrap();
            let node = deserialize_lua_table(&table).unwrap();
            assert!(matches!(parse_node_id(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id"));
        }

        #[test]
        fn a_non_utf8_node_id_is_rejected_rather_than_lossily_converted() {
            let lua = lua();
            let table = lua.create_table().unwrap();
            table.set("kind", "rect").unwrap();
            table.set("id", lua.create_string(b"\xff").unwrap()).unwrap();
            let props = props_from_table(&table);
            let err = parse_node_id(&props).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "id"),
                "a non-UTF-8 id must be a LayoutError naming the property: {err:?}"
            );
        }

        #[test]
        fn two_distinct_non_utf8_ids_do_not_collapse_onto_one_replacement_character() {
            let lua = lua();
            for byte in [b"\xff".as_slice(), b"\xfe".as_slice()] {
                let table = lua.create_table().unwrap();
                table.set("kind", "rect").unwrap();
                table.set("id", lua.create_string(byte).unwrap()).unwrap();
                let props = props_from_table(&table);
                assert!(matches!(parse_node_id(&props), Err(LayoutError::InvalidProperty { ref property, .. }) if property == "id"));
            }
        }

        #[test]
        fn foreground_absent_defaults_to_white() {
            let props = HashMap::new();
            assert_eq!(
                parse_foreground(&props).unwrap(),
                Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }
            );
        }

        #[test]
        fn foreground_reads_a_hex_colour() {
            let lua = lua();
            let table: mlua::Table = lua
                .load(r##"return { kind = "text", foreground = "#00ff0080" }"##)
                .eval()
                .unwrap();
            let props = props_from_table(&table);
            assert_eq!(
                parse_foreground(&props).unwrap(),
                Rgba { r: 0.0, g: 1.0, b: 0.0, a: 0x80 as f32 / 255.0 }
            );
        }

        #[test]
        fn foreground_wrong_type_is_rejected() {
            let lua = lua();
            let table: mlua::Table = lua
                .load(r#"return { kind = "text", foreground = 5 }"#)
                .eval()
                .unwrap();
            let props = props_from_table(&table);
            let err = parse_foreground(&props).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "foreground" && detail.contains("expected a string")),
                "{err}"
            );
        }
}
