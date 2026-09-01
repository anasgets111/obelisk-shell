//! Every paint property of one node, parsed once.
//!
//! Here rather than in `layout::paint` because this is parsing, and `node` is where parsing lives:
//! `layout::scene` calls [`paint_style`] while it resolves a node, and `layout::paint` reads the
//! result. Putting the type in `paint` would make `scene` depend on the module that depends on it.
//!
//! ## Why not at paint time, where it was
//!
//! `layout::paint::build` runs for every mapped surface on every dirty turn, because comparing the
//! display list is *how* a surface declines a repaint (ADR-0063 decision 1). So the parse was
//! the price of finding out that nothing had changed, at ADR-0044 decision 2's cadence rather than
//! the once-per-config-edit one it was written for.
//!
//! The failure rule moved with it, and that is the bigger half. Paint logged a malformed
//! `background` and substituted the absent-key default, every frame, forever. Geometry one line
//! away already failed the apply, rolled the scene back and reached `oblisk.rescue`. One resolved
//! property map with two opinions about what a broken value means is the thing this deletes: a
//! config whose `background` is an integer now says so once, loudly, the way a bad `align_v`
//! always did.
//!
//! ## What stays at paint time
//!
//! Anything that needs an input this pass does not have. `icon`/`image` need the physical scale to
//! turn a logical edge into a pixel count, and a `textfield` needs to know whether it holds the
//! keyboard focus. Both are arithmetic over already-parsed data, not parsing.
//!
//! An `icon`'s `size` is absent here on purpose: it is the node's geometry, read by
//! `layout::scene`'s measure callback, and the drawn pixel count comes from the resolved rect.

use std::collections::HashMap;

use mlua::Value;

use crate::image::Fit;

use super::*;

/// One node's paint properties, with every `mlua::Value` already gone.
///
/// The variants are the kinds that draw something. Everything else parses to `None`, which is the
/// same set `layout::paint::build_node`'s match arm bounds: a kind added to
/// `layout::scene::ensure_supported_kind` without a decision here draws nothing, on purpose.
///
/// Holds no Lua value, for ADR-0063 decision 2's reason. That ADR is about `Draw`, but the
/// trap is the same one: mlua compares tables by identity, so a property whose signal resolves to
/// a table would compare unequal every pass and repaint forever.
#[derive(Debug, Clone, PartialEq)]
pub enum PaintStyle {
    /// `rect`/`row`/`column`/`button` and all four surface roles: the fill, then the border.
    ///
    /// `clip` is here with `radius` rather than off in `LayoutStyle` because it is the property
    /// that decides what `radius` means to everything underneath this node, and the two are read
    /// together. It draws nothing itself: `layout::paint::build_node` is its only reader.
    Box {
        background: Option<Rgba>,
        radius: f32,
        colors: BorderColor,
        widths: EdgeInsets,
        clip: ClipShape,
    },
    Text {
        content: String,
        font_size: f32,
        color: Rgba,
        align: TextAlign,
        elide: Elide,
    },
    /// The theme *name*, not the resolved path: `layout::paint::execute` does the
    /// `image::icons::resolve` lookup, so neither this pass nor the display-list build touches the
    /// icon theme.
    Icon {
        name: String,
        /// § 5.1's `foreground`, which for an icon means what CSS `color` means: the value a
        /// `currentColor` fill resolves to (ADR-0072). `None` leaves the file's own colours
        /// alone, which is every full-colour app icon.
        color: Option<Rgba>,
    },
    Image {
        source: String,
        fit: Fit,
    },
    /// `target` is `None` when the field declares no `secure_submit` at all. A malformed one is an
    /// error now, unlike before: `layout::secure_submit::secure_submit_targets` used to skip it
    /// silently on the grounds that the press path would log it, and the press path was the only
    /// other reader.
    TextField {
        target: Option<SecureSubmitTarget>,
        placeholder: String,
        mask: String,
        font_size: f32,
        color: Rgba,
        align: TextAlign,
    },
}

/// Parses `kind`'s paint properties out of an already-resolved property map.
///
/// `Ok(None)` for a kind that draws nothing. An `Err` fails the whole apply, which is the point:
/// see the module doc comment.
pub fn paint_style(kind: &str, properties: &HashMap<String, Value>) -> Result<Option<PaintStyle>, LayoutError> {
    let style = match kind {
        // row/column/button have no paint properties of their own beyond the base `rect` ones
        // (`oblisk-idl-api-specs.md` § 5.2), and a surface root paints exactly like a rect. All
        // four surface roles, not just `panel`: § 6.2, § 6.3 and § 6.4 give a `window`, a `popup`
        // and a `lock` the same § 5.1 base properties § 6.1 gives a `panel`.
        "rect" | "row" | "column" | "button" | "panel" | "window" | "popup" | "lock" => PaintStyle::Box {
            background: parse_background(properties)?,
            radius: parse_radius(properties)?,
            colors: parse_border_color(properties)?,
            widths: parse_border_width(properties)?,
            clip: parse_clip(properties)?,
        },
        "text" => PaintStyle::Text {
            content: parse_content(properties)?,
            font_size: parse_font_size(properties)?,
            color: parse_foreground(properties)?,
            align: parse_text_align(properties)?,
            elide: parse_elide(properties)?,
        },
        "icon" => {
            PaintStyle::Icon { name: parse_icon_name(properties)?, color: parse_optional_foreground(properties)? }
        }
        "image" => PaintStyle::Image { source: parse_image_source(properties)?, fit: parse_fit(properties)? },
        "textfield" => PaintStyle::TextField {
            target: parse_secure_submit(properties)?,
            placeholder: parse_placeholder(properties)?,
            mask: parse_mask_character(properties)?,
            font_size: parse_font_size(properties)?,
            color: parse_foreground(properties)?,
            align: parse_text_align(properties)?,
        },
        _ => return Ok(None),
    };
    Ok(Some(style))
}

#[cfg(test)]
mod tests {
    use super::*;

    use mlua::Lua;

    use crate::lua::nodes::deserialize_lua_table;

    /// Through `deserialize_lua_table`, the way production reaches these parsers: it is what strips
    /// `kind` back out of the property map, so a hand-built `HashMap` here would feed
    /// [`paint_style`] a map no `Scene::apply` ever produces.
    fn style(lua: &Lua, lua_src: &str) -> Result<Option<PaintStyle>, LayoutError> {
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        paint_style(&node.kind, &node.properties)
    }

    #[test]
    fn an_unknown_text_align_fails_the_pass_naming_the_property() {
        let lua = Lua::new();
        let err = style(&lua, r#"return { kind = "text", content = "hi", text_align = "Middle" }"#).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "text_align"),
            "got {err:?}"
        );
        let err = style(&lua, r#"return { kind = "text", content = "hi", text_align = 1 }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "text_align"));
    }

    #[test]
    fn a_kind_that_draws_nothing_has_no_style() {
        let lua = Lua::new();
        assert_eq!(style(&lua, "return { kind = 'list', direction = 'row' }").unwrap(), None);
    }

    /// The behaviour change this module exists for. `layout::paint` logged this and painted on.
    #[test]
    fn a_malformed_background_fails_the_pass_instead_of_defaulting() {
        let lua = Lua::new();
        assert!(style(&lua, "return { kind = 'rect', background = 5 }").is_err());
    }

    #[test]
    fn a_textfields_absent_secure_submit_is_none_not_an_error() {
        let lua = Lua::new();
        let Some(PaintStyle::TextField { target, .. }) = style(&lua, "return { kind = 'textfield' }").unwrap() else {
            panic!("a `textfield` always has a style");
        };
        assert_eq!(target, None);
    }

    /// `wayland::input::focused_target` used to hold this guarantee and log the failure against the
    /// surface. It cannot any more: a tree carrying a `secure_submit` this rejects never reaches a
    /// pointer event, because the pass that would have built it failed here.
    #[test]
    fn a_malformed_secure_submit_names_no_capability_and_fails_the_pass() {
        let lua = Lua::new();
        assert!(style(&lua, "return { kind = 'textfield', secure_submit = 'polkit' }").is_err());
    }

    /// Both surface roles and plain containers take the same arm, so a `lock`'s background is the
    /// same parse a `rect`'s is.
    #[test]
    fn every_surface_role_parses_the_same_box_properties_a_rect_does() {
        let lua = Lua::new();
        let rect = style(&lua, "return { kind = 'rect', background = '#112233', radius = 4 }").unwrap();
        for role in ["panel", "window", "popup", "lock"] {
            let src = format!("return {{ kind = '{role}', background = '#112233', radius = 4 }}");
            assert_eq!(style(&lua, &src).unwrap(), rect, "{role}");
        }
    }
}
