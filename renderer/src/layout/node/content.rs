//! Leaf-node content parsers: text, icons, images, font/color properties, and identity strings.
//! None affect the box model; geometry parsers live beside them rather than here.
//! [`paint_style`](super::paint_style) runs them once per node per pass. `parse_string_property`
//! is shared by `surface`/`toplevel`/`popup` parsers.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use mlua::Value;

use crate::image::{Fit, Load};
use crate::text::shaping::FontRun;

use super::*;

/// A stretch of a `text`'s content drawn differently from the rest (ADR-0104): in the chain's
/// bold and/or italic face, underlined, or in its own colour. Ranges are bytes into the node's
/// `content` string, in order and non-overlapping, and `layout::scene` remaps them when a wrap or
/// an elide rewrites that string. A `text` whose content is one plain string has none.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleRun {
    pub range: Range<usize>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub color: Option<Rgba>,
    /// What a press on this run hands the node's `on_link` (ADR-0106). Carried, never opened: the
    /// engine knows which run was pressed and nothing about URLs.
    pub href: Option<String>,
}

/// Splits a line at style-run boundaries for painting and link hit-testing (ADR-0104, ADR-0106).
/// Ranges are byte ranges into `line`; `None` marks a plain stretch.
pub fn segments(line: Range<usize>, runs: &[StyleRun]) -> Vec<(Range<usize>, Option<&StyleRun>)> {
    let mut pieces = Vec::new();
    let mut cursor = line.start;
    for run in runs {
        let start = run.range.start.max(line.start);
        let end = run.range.end.min(line.end);
        if start >= end {
            continue;
        }
        if start > cursor {
            pieces.push((cursor..start, None));
        }
        pieces.push((start..end, Some(run)));
        cursor = end;
    }
    if cursor < line.end || pieces.is_empty() {
        pieces.push((cursor..line.end, None));
    }
    pieces
}

/// The bold/italic subset of `runs` in the form the shaper needs. Underline and colour do not
/// change shaping, so contents differing only by underline or colour share a memo entry.
pub fn font_runs(runs: &[StyleRun]) -> Vec<FontRun> {
    runs.iter()
        .filter(|run| run.bold || run.italic)
        .map(|run| FontRun { range: run.range.clone(), bold: run.bold, italic: run.italic })
        .collect()
}

/// Parses `text.content` as one string or notification-body-style runs (§ 2.7, ADR-0104), joining
/// run text and preserving each run's style. The run shape is a body span minus `kind`, so text
/// spans can stream through; the caller filters image spans, which have no `text`.
///
/// Absent `content` is empty (ADR-0044 decision 1): before the first `StateSnapshot`, a capability
/// signal reads `nil`, and `run_startup_evaluation` runs before the poll loop drains one. A typo in
/// `content` therefore renders an empty node; `oblisk.rescue` covers the important failures.
pub fn parse_content(properties: &HashMap<String, Value>) -> Result<(String, Vec<StyleRun>), LayoutError> {
    let Some(value) = properties.get("content") else {
        return Ok((String::new(), Vec::new()));
    };
    match value {
        Value::String(s) => Ok((checked_string("content", s)?, Vec::new())),
        Value::Table(runs) => parse_runs(runs),
        other => {
            Err(invalid("content", format!("expected a string or an array of runs, got {}", preview_for_error(other))))
        }
    }
}

fn parse_runs(runs: &mlua::Table) -> Result<(String, Vec<StyleRun>), LayoutError> {
    let mut content = String::new();
    let mut styles = Vec::new();
    for (position, run) in runs.sequence_values::<Value>().enumerate() {
        if styles.len() == MAX_ARRAY_ELEMENTS {
            return Err(invalid("content", format!("more than {MAX_ARRAY_ELEMENTS} runs in one text node")));
        }
        let index = position + 1;
        let run = run.map_err(|e| invalid("content", format!("run {index}: {e}")))?;
        let Value::Table(run) = run else {
            return Err(invalid("content", format!("run {index}: expected a table, got {}", preview_for_error(&run))));
        };
        let text = match run.get::<Value>("text") {
            Ok(Value::String(s)) => checked_string("content", &s)?,
            Ok(Value::Nil) => {
                return Err(invalid(
                    "content",
                    format!("run {index} has no `text` -- an image span has no place in a line of text, leave it out"),
                ));
            }
            Ok(other) => {
                return Err(invalid(
                    "content",
                    format!("run {index}: expected `text` to be a string, got {}", preview_for_error(&other)),
                ));
            }
            Err(e) => return Err(invalid("content", format!("run {index}: {e}"))),
        };
        let flag = |key: &str| -> Result<bool, LayoutError> {
            match run.get::<Value>(key) {
                Ok(Value::Nil) => Ok(false),
                Ok(Value::Boolean(b)) => Ok(b),
                Ok(other) => Err(invalid(
                    "content",
                    format!("run {index}: expected `{key}` to be a boolean, got {}", preview_for_error(&other)),
                )),
                Err(e) => Err(invalid("content", format!("run {index}: {e}"))),
            }
        };
        let (bold, italic, underline) = (flag("bold")?, flag("italic")?, flag("underline")?);
        let color = match run.get::<Value>("color") {
            Ok(Value::Nil) => None,
            Ok(Value::String(s)) => Some(parse_hex_color("content", &checked_string("content", &s)?)?),
            Ok(other) => {
                return Err(invalid(
                    "content",
                    format!("run {index}: expected `color` to be a hex string, got {}", preview_for_error(&other)),
                ));
            }
            Err(e) => return Err(invalid("content", format!("run {index}: {e}"))),
        };
        let href = match run.get::<Value>("href") {
            Ok(Value::Nil) => None,
            Ok(Value::String(s)) => Some(checked_string("content", &s)?).filter(|href| !href.is_empty()),
            Ok(other) => {
                return Err(invalid(
                    "content",
                    format!("run {index}: expected `href` to be a string, got {}", preview_for_error(&other)),
                ));
            }
            Err(e) => return Err(invalid("content", format!("run {index}: {e}"))),
        };
        if text.is_empty() {
            continue;
        }
        let start = content.len();
        content.push_str(&text);
        if bold || italic || underline || color.is_some() || href.is_some() {
            styles.push(StyleRun { range: start..content.len(), bold, italic, underline, color, href });
        }
    }
    Ok((content, styles))
}

/// `icon.name` (§ 5.2 item 5) is a theme name or absolute path; `image::icons::resolve` tells them
/// apart. It defaults to `""` for the same pre-first-push nil rule as `content` (ADR-0044).
pub fn parse_icon_name(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "name")
}

/// `textfield.placeholder` (§ 5.2 item 8) is empty by default. `image.source` is an absolute path,
/// never an icon theme name (ADR-0054 decision 3).
pub fn parse_placeholder(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "placeholder")
}

/// `textfield.mask_character` (§ 5.2 item 8) is drawn once per typed character. It defaults to
/// U+2022 BULLET; `""` draws nothing, and longer strings use their first character.
pub fn parse_mask_character(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    let declared = parse_optional_string(properties, "mask_character")?;
    if !properties.contains_key("mask_character") {
        return Ok("\u{2022}".to_string());
    }
    Ok(declared.chars().next().map(String::from).unwrap_or_default())
}

pub fn parse_image_source(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "source")
}

/// `image.fit` (ADR-0055 decision 3) defaults to `cover`; an unrecognised string errors rather
/// than silently selecting a fit.
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

/// `image.async` (ADR-0122): absent/`false` decodes in the frame; `true` uses the pool and draws
/// nothing until the result lands. A signal resolving to `nil` arrives as an absent key.
pub fn parse_load(properties: &HashMap<String, Value>) -> Result<Load, LayoutError> {
    Ok(if parse_bool(properties, "async", false)? { Load::Background } else { Load::Inline })
}

/// `image.retain` (ADR-0180): while a new `source` decodes, keep drawing the one this node last
/// had pixels for instead of nothing. Inert without `async = true`, because an inline decode is
/// finished by the time the draw asks for it and never leaves a gap to cover.
pub fn parse_retain(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    parse_bool(properties, "retain", false)
}

fn parse_optional_string(properties: &HashMap<String, Value>, property: &str) -> Result<String, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(String::new());
    };
    match value {
        Value::String(s) => checked_string(property, s),
        other => Err(invalid(property, format!("expected a string, got {}", preview_for_error(other)))),
    }
}

/// `text.foreground` (§ 5.2 item 4) defaults to white; `layout::paint::paint_text` uses that same
/// white when a present value is malformed. `TextAlign` places glyphs inside the node's
/// box, unlike `align_h`, which places the node in its parent; it matters only when the box is
/// wider than the measured text.
///
/// Its own type rather than reusing [`Align`](super::Align): that carries `Stretch`, which would
/// be meaningless here since a run of glyphs has no size to force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Start,
    Center,
    End,
}

/// `font` (§ 5.2 item 4): the font family this node measures and paints in, as the config wrote it
/// (ADR-0144). Absent -- which is most nodes -- means the chain `fonts { ... }` declared.
///
/// A family name rather than a fixed set of roles, because a Nerd-Font-patched body family carries
/// the private-use icon block itself and always wins per-glyph fallback: no chain ordering reaches
/// a second family that also has those codepoints, so the node has to name one. The same mechanism
/// then covers a heading face or a monospaced readout without new IDL.
///
/// `Arc<str>` rather than `String`: this is cloned into a measurement cache key, a display-list
/// command and a paint call for every text node every pass, and the string is a theme constant
/// repeated across dozens of nodes.
///
/// The name is not validated here. Parsing sees the property, not the loaded font set -- and the
/// set is not fixed at parse time, since a family is resolved on first sight. An unresolvable name
/// draws in the declared chain and says so once on stderr, the same bargain `fonts { ... }` already
/// makes for a chain entry nothing on the system answers.
pub fn parse_font_family(properties: &HashMap<String, Value>) -> Result<Option<Arc<str>>, LayoutError> {
    let Some(value) = properties.get("font") else {
        return Ok(None);
    };
    let Value::String(s) = value else {
        return Err(invalid("font", format!("must be a family name string, got {}", preview_for_error(value))));
    };
    let family = checked_string("font", s)?;
    // An empty string is a config bug that would otherwise look like "no family named", and the
    // node would silently draw in the declared chain with nothing to point at.
    if family.is_empty() {
        return Err(invalid("font", "must be a family name, got an empty string".to_string()));
    }
    Ok(Some(Arc::from(family)))
}

/// What to do with text too wide for its box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Elide {
    /// Let the clip cut it off.
    #[default]
    None,
    /// Drop trailing characters and finish with a single-character ellipsis.
    End,
}

/// `elide` (`lua-api.md` § 5.2 item 4). Only `"End"` is offered: the reference config
/// uses neither head nor middle elision, and middle elision needs a grapheme budget across runs.
pub fn parse_elide(properties: &HashMap<String, Value>) -> Result<Elide, LayoutError> {
    let Some(value) = properties.get("elide") else {
        return Ok(Elide::None);
    };
    let Value::String(s) = value else {
        return Err(invalid("elide", format!("must be a string, got {}", preview_for_error(value))));
    };
    match checked_string("elide", s)?.as_str() {
        "None" => Ok(Elide::None),
        "End" => Ok(Elide::End),
        other => Err(invalid("elide", format!("must be \"None\" or \"End\", got {other:?}"))),
    }
}

/// Whether oversized text breaks onto another line. It composes with `elide`: `wrap = "Word"` and
/// `elide = "End"` fills the allowed lines, then ellipsizes the last one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wrap {
    /// One line, however long.
    #[default]
    None,
    /// Break at word boundaries, falling back to a glyph boundary for a word wider than the box,
    /// using cosmic-text's `Wrap::WordOrGlyph`.
    Word,
}

/// `wrap` (`lua-api.md` § 5.2 item 4) defaults to `None`. Before this, a fixed-width
/// `text` measured its full wrapped height but painted one clipped line; making wrapping default
/// would have drawn into that extra height everywhere. `None` now measures one line, keeping box
/// and paint consistent.
pub fn parse_wrap(properties: &HashMap<String, Value>) -> Result<Wrap, LayoutError> {
    let Some(value) = properties.get("wrap") else {
        return Ok(Wrap::None);
    };
    let Value::String(s) = value else {
        return Err(invalid("wrap", format!("must be a string, got {}", preview_for_error(value))));
    };
    match checked_string("wrap", s)?.as_str() {
        "None" => Ok(Wrap::None),
        "Word" => Ok(Wrap::Word),
        other => Err(invalid("wrap", format!("must be \"None\" or \"Word\", got {other:?}"))),
    }
}

/// `max_lines` (`lua-api.md` § 5.2 item 4) is uncapped when absent or `0`; zero lets
/// signal-driven values spell "absent" because `Bound` cannot. Negatives error rather than being
/// clamped, which would hide a sign mistake in config arithmetic. It is consulted
/// only for [`parse_wrap`] = `Word`, so setting both unconditionally is safe.
pub fn parse_max_lines(properties: &HashMap<String, Value>) -> Result<Option<usize>, LayoutError> {
    let Some(value) = properties.get("max_lines") else {
        return Ok(None);
    };
    let n = value_as_f32("max_lines", value)?
        .ok_or_else(|| invalid("max_lines", format!("expected a number, got {}", preview_for_error(value))))?;
    if n < 0.0 {
        return Err(invalid("max_lines", format!("must not be negative, got {n}")));
    }
    Ok((n >= 1.0).then_some(n as usize))
}

/// `text_align` (`lua-api.md` § 5.2 item 4) defaults to `Start` and uses the same
/// string boundary as `fit`, `layer`, `align_h`, and `on_click`. `Start`/`End` match `align_h`.
pub fn parse_text_align(properties: &HashMap<String, Value>) -> Result<TextAlign, LayoutError> {
    let Some(value) = properties.get("text_align") else {
        return Ok(TextAlign::Start);
    };
    let Value::String(s) = value else {
        return Err(invalid("text_align", format!("must be a string, got {}", preview_for_error(value))));
    };
    match checked_string("text_align", s)?.as_str() {
        "Start" => Ok(TextAlign::Start),
        "Center" => Ok(TextAlign::Center),
        "End" => Ok(TextAlign::End),
        other => Err(invalid("text_align", format!("must be \"Start\", \"Center\" or \"End\", got {other:?}"))),
    }
}

/// § 5.1's declared `foreground`, or `None` when absent. Icons preserve their file colours unless
/// a `currentColor` fill uses this value (ADR-0072).
pub fn parse_optional_foreground(properties: &HashMap<String, Value>) -> Result<Option<Rgba>, LayoutError> {
    if !properties.contains_key("foreground") {
        return Ok(None);
    }
    parse_foreground(properties).map(Some)
}

pub fn parse_foreground(properties: &HashMap<String, Value>) -> Result<Rgba, LayoutError> {
    let Some(value) = properties.get("foreground") else {
        return Ok(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
    };
    let Value::String(s) = value else {
        return Err(invalid("foreground", format!("expected a string, got {}", preview_for_error(value))));
    };
    let s = checked_string("foreground", s)?;
    parse_hex_color("foreground", &s)
}

pub fn parse_font_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    parse_number(properties, "font_size", 12.0)
}

/// Absent `size` defaults to 12.0, matching [`parse_font_size`] and ADR-0044's nil rule. A typo
/// such as `icon { sizee = 24 }` therefore draws a 12.0-sized icon rather than rejecting the tree;
/// text and icons share the same default visual scale.
pub fn parse_icon_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    parse_number(properties, "size", 12.0)
}

/// Shared boolean parser behind [`parse_load`], [`parse_retain`], `style::parse_blur` and
/// `style::parse_visible`, the way [`parse_string_property`] is shared by the string ones. An
/// absent key takes `default`; anything that is not a boolean is an error naming the property.
pub(super) fn parse_bool(
    properties: &HashMap<String, Value>,
    property: &str,
    default: bool,
) -> Result<bool, LayoutError> {
    match properties.get(property) {
        None => Ok(default),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(other) => Err(invalid(property, format!("expected a boolean, got {}", preview_for_error(other)))),
    }
}

/// Shared number parser behind [`parse_font_size`], [`parse_icon_size`] and `style::parse_spacing`.
/// `style::parse_opacity` keeps its own body: it range-checks on top of this.
pub(super) fn parse_number(
    properties: &HashMap<String, Value>,
    property: &str,
    default: f32,
) -> Result<f32, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(default);
    };
    value_as_f32(property, value)?
        .ok_or_else(|| invalid(property, format!("expected a number, got {}", preview_for_error(value))))
}

/// Shared structural-string parser behind [`parse_surface_id`], `surface::parse_layer`, and
/// `surface::parse_monitor`: reject a `Signal`, require a string, and use `default` when
/// absent. `None` makes the property required (Standards review, ADR-0024).
pub(super) fn parse_string_property(
    properties: &HashMap<String, Value>,
    property: &str,
    default: Option<&str>,
) -> Result<String, LayoutError> {
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

/// A top-level surface's `id`: required, unique per config, and keys `Scene::apply`'s `HashMap`
/// for keyed reconciliation (ADR-0045). It is also the surface's *reconcile* identity: the tree
/// root is found by key lookup rather than [`parse_node_id`]'s per-parent pairing, since a surface
/// has no parent to scope within (decision 5: the same mechanism restated one level down).
pub fn parse_surface_id(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "id", None)
}

/// The optional `id` base property on every node kind, one level below a surface's root
/// (ADR-0045 decisions 1-2). `None` means "no id", not an error:
/// `pair_children_by_id_then_position` pairs an id-less child positionally against its id-less
/// siblings (ADR-0023's rule applied to that subsequence). Adding or dropping an `id` changes
/// identity, dropping the retained counterpart and allocating a new node. Rejects a `Signal` via
/// [`reject_signal_in_structural_field`], same as [`parse_surface_id`]: reconcile identity is
/// decided once at match time, not left to drift.
///
/// Non-UTF-8 bytes are refused rather than converted, unlike [`checked_string`]'s lossy handling
/// of `content`-like properties: `to_string_lossy` maps `"\xFF"` and `"\xFE"` both to `U+FFFD`, so
/// distinct ids would compare equal and a fresh child could claim the wrong counterpart. Scoping
/// and duplicate rejection belong to `pair_children_by_id_then_position`, which has visibility
/// into siblings that this parser does not.
pub fn parse_node_id(properties: &HashMap<String, Value>) -> Result<Option<String>, LayoutError> {
    let Some(value) = properties.get("id") else {
        return Ok(None);
    };
    reject_signal_in_structural_field("id", value)?;
    match value {
        Value::String(s) => s.to_str().map(|s| Some(s.to_string())).map_err(|_| {
            invalid("id", "must be valid UTF-8 -- an id is compared for equality, so it cannot be converted lossily")
        }),
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
        assert_eq!(parse_content(&props).unwrap().0, "");
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
        assert_eq!(parse_content(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap().0, "hello");
    }

    #[test]
    fn a_signal_resolving_to_a_number_reports_the_same_error_a_literal_number_would() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();

        let literal_table: mlua::Table = lua.load(r#"return { kind = "text", content = 5 }"#).eval().unwrap();
        let literal_props = props_from_table(&literal_table);
        let literal_err = parse_content(&resolve_properties(&literal_props, "text", &lua).unwrap()).unwrap_err();

        let signal = crate::lua::signal::Signal::new_live(Value::Integer(5), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("content", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let signal_err = parse_content(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap_err();

        for err in [&literal_err, &signal_err] {
            assert!(matches!(
                err,
                LayoutError::InvalidProperty { property, detail }
                    if property == "content" && detail.starts_with("expected a string or an array of runs")
            ));
        }
    }

    // ---- styled runs (ADR-0104) ----

    fn runs_content(lua: &mlua::Lua, src: &str) -> Result<(String, Vec<StyleRun>), LayoutError> {
        let table: mlua::Table = lua.load(format!(r#"return {{ kind = "text", content = {src} }}"#)).eval().unwrap();
        parse_content(&props_from_table(&table))
    }

    #[test]
    fn an_array_of_runs_joins_their_text_and_keeps_where_each_styled_one_lies() {
        let lua = lua();
        let (content, runs) = runs_content(
            &lua,
            r##"{ { text = "Alice" , bold = true }, { text = ": see " }, { text = "this", underline = true, color = "#ff0000" }, { text = "!" } }"##,
        )
        .unwrap();
        assert_eq!(content, "Alice: see this!");
        assert_eq!(runs.len(), 2, "plain runs are text with no style entry of their own");
        assert_eq!(runs[0].range, 0..5);
        assert!(runs[0].bold && !runs[0].italic && !runs[0].underline && runs[0].color.is_none());
        assert_eq!(runs[1].range, 11..15);
        assert!(runs[1].underline);
        assert_eq!(runs[1].color, Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }));
    }

    #[test]
    fn an_empty_run_array_is_empty_content_and_an_empty_run_is_skipped() {
        let lua = lua();
        assert_eq!(runs_content(&lua, "{}").unwrap(), (String::new(), Vec::new()));
        let (content, runs) = runs_content(&lua, r#"{ { text = "", bold = true }, { text = "a" } }"#).unwrap();
        assert_eq!((content.as_str(), runs.len()), ("a", 0));
    }

    /// A notification body span of `kind = "image"` has no `text`. It is refused with a message
    /// that says what to do about it, rather than drawn as nothing or as its path.
    #[test]
    fn a_run_without_text_is_refused_naming_the_run() {
        let lua = lua();
        let err = runs_content(&lua, r#"{ { text = "a" }, { kind = "image", image_path = "/x.png" } }"#).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail }
            if property == "content" && detail.starts_with("run 2 has no `text`")),
            "{err}"
        );
    }

    #[test]
    fn a_run_with_a_mistyped_flag_or_colour_is_refused() {
        let lua = lua();
        assert!(runs_content(&lua, r#"{ { text = "a", bold = "yes" } }"#).is_err());
        assert!(runs_content(&lua, r#"{ { text = "a", color = "red" } }"#).is_err());
        assert!(runs_content(&lua, r#"{ "just a string" }"#).is_err());
    }

    #[test]
    fn a_run_with_an_href_is_a_styled_run_even_with_no_other_style() {
        let lua = lua();
        let (content, runs) =
            runs_content(&lua, r#"{ { text = "see " }, { text = "this", href = "https://x.example/" } }"#).unwrap();
        assert_eq!(content, "see this");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].href.as_deref(), Some("https://x.example/"));
        assert_eq!(runs[0].range, 4..8);
        let (_, none) = runs_content(&lua, r#"{ { text = "a", href = "" } }"#).unwrap();
        assert!(none.is_empty(), "an empty href is no href");
    }

    fn run(range: Range<usize>) -> StyleRun {
        StyleRun { range, bold: true, italic: false, underline: false, color: None, href: None }
    }

    // ---- segments (ADR-0104) ----

    #[test]
    fn a_line_with_no_run_in_it_is_one_plain_piece() {
        let pieces = segments(0..5, &[]);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, 0..5);
        assert!(pieces[0].1.is_none());
        // A run entirely on another line leaves this one plain too.
        assert_eq!(segments(0..5, &[run(6..9)]).len(), 1);
    }

    #[test]
    fn a_run_inside_a_line_splits_it_into_plain_styled_plain() {
        let runs = [run(2..4)];
        let pieces: Vec<(Range<usize>, bool)> =
            segments(0..6, &runs).into_iter().map(|(r, s)| (r, s.is_some())).collect();
        assert_eq!(pieces, vec![(0..2, false), (2..4, true), (4..6, false)]);
    }

    /// A wrap can break a run across lines: the second line's slice of it starts at the line, not
    /// at the run, and a run ending exactly at a line's end leaves no empty plain tail.
    #[test]
    fn a_run_crossing_a_line_boundary_is_clipped_to_the_line_on_each_side() {
        let runs = [run(3..9)];
        let first: Vec<_> = segments(0..5, &runs).into_iter().map(|(r, s)| (r, s.is_some())).collect();
        assert_eq!(first, vec![(0..3, false), (3..5, true)]);
        let second: Vec<_> = segments(6..10, &runs).into_iter().map(|(r, s)| (r, s.is_some())).collect();
        assert_eq!(second, vec![(6..9, true), (9..10, false)]);
    }

    #[test]
    fn adjacent_runs_touch_with_no_plain_piece_between_them() {
        let runs = [run(0..2), run(2..4)];
        let pieces: Vec<_> = segments(0..4, &runs).into_iter().map(|(r, s)| (r, s.is_some())).collect();
        assert_eq!(pieces, vec![(0..2, true), (2..4, true)]);
    }

    #[test]
    fn font_runs_keep_only_the_runs_the_shaper_can_see() {
        let runs = vec![
            StyleRun { range: 0..2, bold: false, italic: false, underline: true, color: None, href: None },
            StyleRun { range: 2..4, bold: true, italic: false, underline: false, color: None, href: None },
            StyleRun { range: 4..6, bold: false, italic: true, underline: true, color: None, href: None },
        ];
        let fonts = font_runs(&runs);
        assert_eq!(fonts.len(), 2);
        assert_eq!((fonts[0].range.clone(), fonts[0].bold, fonts[0].italic), (2..4, true, false));
        assert_eq!((fonts[1].range.clone(), fonts[1].bold, fonts[1].italic), (4..6, false, true));
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
        let table: mlua::Table = lua.load(r#"return { kind = "text", font_size = 1e300 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_font_size(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    #[test]
    fn wrap_defaults_to_one_line_and_rejects_a_mode_that_does_not_exist() {
        let lua = lua();
        assert_eq!(parse_wrap(&HashMap::new()).unwrap(), Wrap::None);

        let table: mlua::Table = lua.load(r#"return { kind = "text", wrap = "Word" }"#).eval().unwrap();
        assert_eq!(parse_wrap(&props_from_table(&table)).unwrap(), Wrap::Word);

        // "WordWrap" is the plausible typo, and QML spells this mode `Text.WordWrap`.
        let table: mlua::Table = lua.load(r#"return { kind = "text", wrap = "WordWrap" }"#).eval().unwrap();
        let err = parse_wrap(&props_from_table(&table)).unwrap_err();
        assert!(format!("{err}").contains("Word"), "the error should name the modes that do exist, got {err}");
    }

    /// Zero is the uncapped spelling a `Bound` needs, since a signal has no way to be absent. A
    /// negative has no reading at all, and clamping one would swallow a sign slip in a config's
    /// own arithmetic.
    #[test]
    fn max_lines_treats_absent_and_zero_alike_and_refuses_a_negative() {
        let lua = lua();
        assert_eq!(parse_max_lines(&HashMap::new()).unwrap(), None);

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = 0 }"#).eval().unwrap();
        assert_eq!(parse_max_lines(&props_from_table(&table)).unwrap(), None);

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = 2 }"#).eval().unwrap();
        assert_eq!(parse_max_lines(&props_from_table(&table)).unwrap(), Some(2));

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = -1 }"#).eval().unwrap();
        assert!(matches!(
            parse_max_lines(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "max_lines"
        ));

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = "two" }"#).eval().unwrap();
        assert!(parse_max_lines(&props_from_table(&table)).is_err());
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
        assert!(
            matches!(parse_node_id(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id")
        );
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
            assert!(
                matches!(parse_node_id(&props), Err(LayoutError::InvalidProperty { ref property, .. }) if property == "id")
            );
        }
    }

    #[test]
    fn foreground_absent_defaults_to_white() {
        let props = HashMap::new();
        assert_eq!(parse_foreground(&props).unwrap(), Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
    }

    #[test]
    fn foreground_reads_a_hex_colour() {
        let lua = lua();
        let table: mlua::Table = lua.load(r##"return { kind = "text", foreground = "#00ff0080" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_foreground(&props).unwrap(), Rgba { r: 0.0, g: 1.0, b: 0.0, a: 0x80 as f32 / 255.0 });
    }

    #[test]
    fn foreground_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "text", foreground = 5 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_foreground(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "foreground" && detail.contains("expected a string")),
            "{err}"
        );
    }
}
