//! Leaf-node content parsers: text content, icon name/size, image source/fit, font size,
//! foreground color, and identity strings (`id`, `surface_id`). None affect the box model;
//! [`paint_style`](super::paint_style) runs them once per node per pass, alongside the geometry
//! parsers. `parse_string_property` is `pub(super)`, reused by `surface`/`toplevel`/`popup` code
//! for `namespace`, `title`, `app_id` and the like.

use std::collections::HashMap;
use std::ops::Range;

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

/// One line of `text` cut at the points where its style changes: each piece is a byte range of
/// the line and the run it falls in, or `None` for a plain stretch. What paint draws piece by piece
/// and what a press walks to find the run under it (ADR-0104, ADR-0106). Pure, so the split -- the
/// part of a styled draw that can go wrong quietly -- is tested without a GL context.
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

/// The half of `runs` the shaper needs -- the ones in another face -- in the shape it takes. An
/// underlined or recoloured run in the regular face measures like plain text and is left out, so
/// two contents that differ only in colour share one memo entry.
pub fn font_runs(runs: &[StyleRun]) -> Vec<FontRun> {
    runs.iter()
        .filter(|run| run.bold || run.italic)
        .map(|run| FontRun { range: run.range.clone(), bold: run.bold, italic: run.italic })
        .collect()
}

/// `text.content`: one string, or an array of runs `{ text = ..., bold = ..., italic = ...,
/// underline = ..., color = ..., href = ... }` whose texts are joined into the string this returns
/// and whose styles become the [`StyleRun`]s beside it (ADR-0104). The run shape is a notification
/// body span's own (§ 2.7) minus `kind`, so a body's text spans can be handed over as they arrive;
/// an image span has no `text` and is refused, since a picture inside a line of text is not
/// something this node draws -- the caller filters those out.
///
/// Absent `content` defaults to the empty string (ADR-0044 decision 1's nil rule): a `text` bound
/// to a not-yet-pushed capability signal reads `nil` until its first `StateSnapshot`, and
/// `run_startup_evaluation` runs before the poll loop drains one. Rejecting that would boot a
/// blank shell. Accepted cost: a misspelled `content` key renders an empty node instead of
/// failing the whole tree; `oblisk.rescue` covers the failures that matter.
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
    for (position, run) in runs.clone().sequence_values::<Value>().enumerate() {
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

/// `icon.name` (§ 5.2 item 5): a theme name or an absolute path, told apart by
/// `image::icons::resolve`. Defaults to `""` for the same boot reason `content` does (ADR-0044): a
/// signal-bound `name` is `nil` until its first push, and rejecting the tree would fail every
/// config that binds one.
pub fn parse_icon_name(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "name")
}

/// `image.source` (ADR-0054 decision 3): an absolute path, never a theme name, the whole
/// difference from [`parse_icon_name`] and why the two share no property spelling.
/// `textfield.placeholder` (§ 5.2 item 8): what an empty field shows; defaults to `""`, the same
/// boot-tolerance [`parse_content`] takes.
pub fn parse_placeholder(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "placeholder")
}

/// `textfield.mask_character` (§ 5.2 item 8): the glyph drawn once per typed character. Defaults
/// to U+2022 BULLET, the usual password-field glyph. An empty string means "draw nothing",
/// honoured since a config wanting no visible length says so explicitly. Longer strings truncate
/// to the first character: the property names a *character*, not worth failing a tree over.
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

/// `image.fit` (ADR-0055 decision 3). Absent is `cover`; an unrecognised string errors rather
/// than silently falling back, so `fit = "fill"` doesn't hide behind a silently-covered image.
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

/// `image.async` (ADR-0122). Absent and `false` decode in the frame; `true` hands the decode to
/// the pool and draws nothing until it lands. A boolean or nothing, since a signal resolving to
/// `nil` arrives as an absent key.
pub fn parse_load(properties: &HashMap<String, Value>) -> Result<Load, LayoutError> {
    match properties.get("async") {
        None | Some(Value::Boolean(false)) => Ok(Load::Inline),
        Some(Value::Boolean(true)) => Ok(Load::Background),
        Some(other) => Err(invalid("async", format!("expected a boolean, got {}", preview_for_error(other)))),
    }
}

/// The shared shape behind every § 5.2 string property that defaults to empty when absent.
fn parse_optional_string(properties: &HashMap<String, Value>, property: &str) -> Result<String, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(String::new());
    };
    match value {
        Value::String(s) => checked_string(property, s),
        other => Err(invalid(property, format!("expected a string, got {}", preview_for_error(other)))),
    }
}

/// `text.foreground` (§ 5.2 item 4). Absent defaults to white: `layout::paint`'s `paint_text`
/// falls back to the same white whenever this parser errors on a present-but-malformed value, so
/// the rendered result agrees whether the key was omitted or rejected.
/// Where a run of glyphs sits inside the box the node was given, distinct from where the node
/// sits inside its parent (`align_h`). Only visible when the box is wider than the text, so it
/// does nothing on a `Content`-sized node measuring that same string; an explicit `width`,
/// `"Fill"`, or a `Stretch`ed cross axis makes room for it to matter.
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

/// What to do with a run of text too wide for the box it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Elide {
    /// Let the clip cut it off mid-glyph.
    #[default]
    None,
    /// Drop trailing characters and finish with a single-character ellipsis.
    End,
}

/// `elide` (`oblisk-idl-api-specs.md` § 5.2 item 4). Absent is `None`. Only `"End"` is offered:
/// QML also has head and middle elision, but the reference config uses neither, and middle elide
/// has to split a grapheme budget across two runs, real work nothing has asked for.
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

/// Whether a run too wide for its box breaks onto another line, and where it may break.
///
/// Its own property rather than something `elide` implies: the two answer different questions and
/// compose, `wrap = "Word"` with `elide = "End"` being the notification-body case -- fill the
/// lines allowed, then ellipsize the last one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wrap {
    /// One line, however long. What every `text` did before wrapping existed.
    #[default]
    None,
    /// Break at word boundaries, falling back to a glyph boundary for a word wider than the box,
    /// which is cosmic-text's own `Wrap::WordOrGlyph` and the only sensible behaviour for a
    /// 40-character German compound in a 120px card.
    Word,
}

/// `wrap` (`oblisk-idl-api-specs.md` § 5.2 item 4). Absent is `None`, which is what every existing
/// config gets and what the engine did before: a `text` stays on one line unless it asks not to.
///
/// Opt-in rather than always-on even though measurement already wrapped. Before this, a
/// fixed-width `text` measured its full wrapped height and painted one clipped line, so its box
/// was already too tall; making wrapping the default would have started *drawing* into that extra
/// height everywhere at once. `"None"` now measures one line too, so the box and the paint agree
/// in both modes -- which is the actual fix, and the reason this is not purely additive.
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

/// `max_lines` (`oblisk-idl-api-specs.md` § 5.2 item 4). Absent, or `0`, is no cap.
///
/// Zero means uncapped rather than being refused as nonsense, because the property exists to be
/// driven by a signal: an expander is `max_lines = expanded:map(function(e) return e and 0 or 2
/// end)`, and a `Bound` has no way to spell "absent". A negative is still an error -- there is no
/// reading of it, and silently clamping would hide a sign slip in a config's arithmetic.
///
/// Only consulted when [`parse_wrap`] said `Word`: capping the lines of a run that cannot make a
/// second one is a no-op, not an error, so a config can set both unconditionally.
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

/// `text_align` (`oblisk-idl-api-specs.md` § 5.2 item 4). Absent is `Start`. A string, matching
/// `fit`, `layer`, `align_h` and `on_click`'s button name at this boundary. `Start`/`End` rather
/// than `Left`/`Right`, the names § 5.2 uses for the same axis elsewhere, as `align_h` does.
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

/// § 5.1's `foreground` when the node declares one, `None` when it does not. Separate from
/// [`parse_foreground`] because an icon's default is not white: with none set it rasterizes
/// exactly as its file says. Only a `currentColor` icon takes a colour from this (ADR-0072).
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
    let Some(value) = properties.get("font_size") else {
        return Ok(12.0);
    };
    value_as_f32("font_size", value)?
        .ok_or_else(|| invalid("font_size", format!("expected a number, got {}", preview_for_error(value))))
}

/// Absent `size` defaults to 12.0, the same nil-rule rationale as [`parse_content`] (ADR-0044's
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
/// is absent; `None` makes it required, erroring instead (Standards review, ADR-0024).
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
/// identity, retiring the retained counterpart and allocating a new node. Rejects a `Signal` via
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
