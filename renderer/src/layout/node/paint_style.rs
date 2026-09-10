//! Paint properties are parsed during `Scene::apply`, not in `layout::paint`: keeping this type
//! here avoids making `scene` depend on a module that already depends on it. Display-list builds
//! run every dirty turn because list equality controls repaint (ADR-0063 decision 1), while applies
//! run at capability-push cadence (ADR-0044 decision 2). This also makes malformed values fail once
//! through `oblisk.rescue` instead of painting with a default every frame. Geometry already fails
//! `apply` and reaches `oblisk.rescue`; one resolved map cannot give paint a second opinion on
//! malformed values. Paint-time work remains arithmetic needing scale or focus; `icon.size` stays
//! geometry for the scene's measure callback.

use std::collections::HashMap;
use std::sync::Arc;

use mlua::Value;

use crate::image::{Fit, Load};

use super::*;

/// Parsed paint properties with no `mlua::Value`. A kind admitted by
/// `layout::scene::ensure_supported_kind` but absent here draws nothing. Lua tables compare by
/// identity, so keeping one here would make a signal-resolved table repaint forever (ADR-0063).
#[derive(Debug, Clone, PartialEq)]
pub enum PaintStyle {
    /// Box fill/border for containers and all four surface roles. `clip` travels with `radius`
    /// because it changes how the node's shape clips descendants.
    Box { background: Option<Rgba>, radius: f32, colors: BorderColor, widths: EdgeInsets, clip: ClipShape },
    /// Text before/after `Scene::finish` rewrites it to an ellipsized prefix under `elide` or
    /// wrapped lines joined by `\n`; display-list paint may therefore receive `\n`-joined lines.
    /// `elide`, `wrap`, and `max_lines` survive for that rewrite but are dead to `layout::paint`.
    Text {
        content: String,
        /// Styled stretches of `content`, remapped when the scene rewrites it (ADR-0104).
        runs: Vec<StyleRun>,
        font_size: f32,
        /// The family this node named, or `None` for the declared chain (ADR-0144).
        font: Option<Arc<str>>,
        color: Rgba,
        align: TextAlign,
        elide: Elide,
        wrap: Wrap,
        max_lines: Option<usize>,
    },
    /// Theme name; `layout::paint::execute` resolves it, keeping filesystem access out of parsing
    /// and display-list building.
    Icon {
        name: String,
        /// § 5.1's `foreground` for `currentColor` fills (ADR-0072); `None` preserves file colours.
        color: Option<Rgba>,
    },
    Image {
        source: String,
        fit: Fit,
        /// `async = true` (ADR-0122): decode on the pool and draw nothing until it lands.
        load: Load,
        /// `retain = true` (ADR-0180): cover that gap with the source this node last had pixels
        /// for, rather than with nothing. Inert under [`Load::Inline`], which leaves no gap.
        /// Implied by `transition`, which has nothing to cross from without it.
        retain: bool,
        /// `transition` (ADR-0181): cross from the covering source to the landed one over a
        /// duration, instead of swapping between them in one frame.
        transition: Option<TransitionSpec>,
    },
    /// `target` is `None` when no `secure_submit` is declared. Malformed targets fail here instead
    /// of being skipped until the press path (`layout::secure_submit` used to do that).
    TextField {
        target: Option<SecureSubmitTarget>,
        placeholder: String,
        mask: String,
        font_size: f32,
        color: Rgba,
        align: TextAlign,
    },
}

/// Parses an already-resolved kind. `Ok(None)` means the kind draws nothing; an error fails apply.
pub fn paint_style(kind: &str, properties: &HashMap<String, Value>) -> Result<Option<PaintStyle>, LayoutError> {
    let style = match kind {
        // All containers and surface roles use the base box properties (§ 5.2; § 6).
        "rect" | "row" | "column" | "button" | "panel" | "window" | "popup" | "lock" => PaintStyle::Box {
            background: parse_background(properties)?,
            radius: parse_radius(properties)?,
            colors: parse_border_color(properties)?,
            widths: parse_border_width(properties)?,
            clip: parse_clip(properties)?,
        },
        "text" => {
            let (content, runs) = parse_content(properties)?;
            PaintStyle::Text {
                content,
                runs,
                font_size: parse_font_size(properties)?,
                font: parse_font_family(properties)?,
                color: parse_foreground(properties)?,
                align: parse_text_align(properties)?,
                elide: parse_elide(properties)?,
                wrap: parse_wrap(properties)?,
                max_lines: parse_max_lines(properties)?,
            }
        }
        "icon" => {
            PaintStyle::Icon { name: parse_icon_name(properties)?, color: parse_optional_foreground(properties)? }
        }
        "image" => {
            let transition = parse_transition(properties)?;
            PaintStyle::Image {
                source: parse_image_source(properties)?,
                fit: parse_fit(properties)?,
                load: parse_load(properties)?,
                // A dissolve crosses *from* the picture the node is holding, so declaring one is
                // declaring retention; making a config write both would only let it write one.
                retain: parse_retain(properties)? || transition.is_some(),
                transition,
            }
        }
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

    /// A `text` that says nothing draws in the declared chain, which is most nodes.
    #[test]
    fn a_text_node_without_a_font_property_draws_in_the_declared_chain() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "text", content = "hi" }"#).unwrap().unwrap();
        assert!(matches!(parsed, PaintStyle::Text { font: None, .. }), "got {parsed:?}");
    }

    /// The family reaches paint as the config wrote it, not normalised: the painter keys its
    /// chains on the name the node asked for, so any rewriting here would miss the chain.
    #[test]
    fn a_text_node_carries_the_family_name_it_was_given() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "text", content = "hi", font = "JetBrainsMono Nerd Font Mono" }"#)
            .unwrap()
            .unwrap();
        let PaintStyle::Text { font, .. } = parsed else { panic!("expected text") };
        assert_eq!(font.as_deref(), Some("JetBrainsMono Nerd Font Mono"));
    }

    /// An empty string would otherwise read as "no family named" and silently draw in the declared
    /// chain with nothing for the reader to point at.
    #[test]
    fn an_empty_or_non_string_font_fails_the_pass_naming_the_property() {
        let lua = Lua::new();
        let err = style(&lua, r#"return { kind = "text", content = "hi", font = "" }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "font"), "got {err:?}");
        let err = style(&lua, r#"return { kind = "text", content = "hi", font = 1 }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "font"));
    }

    #[test]
    fn a_kind_that_draws_nothing_has_no_style() {
        let lua = Lua::new();
        assert_eq!(style(&lua, "return { kind = 'list', direction = 'row' }").unwrap(), None);
    }

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

    #[test]
    fn a_malformed_secure_submit_names_no_capability_and_fails_the_pass() {
        let lua = Lua::new();
        assert!(style(&lua, "return { kind = 'textfield', secure_submit = 'polkit' }").is_err());
    }

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
