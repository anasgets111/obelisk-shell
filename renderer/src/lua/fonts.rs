//! The `fonts { ... }` declaration: which font families this shell measures and paints with
//! (docs/adr/0043 decision 2).
//!
//! A global that records rather than a value the config returns, because `shell.lua` returns an
//! array of surfaces and there is no slot in it for something that is not one. That puts this in
//! the same shape as `state`, `hover` and `scroll`: a registry in `Lua::app_data`, written while
//! the config evaluates and read afterwards.
//!
//! Its own module rather than a fourth registry inside `lua::signal`, where those three live. They
//! are there because they are signals and share `SignalKind`, its write gating and the dirty flag.
//! This is a plain list read once with no reactivity at all, so putting it beside them would file it
//! under the one thing it is not.
//!
//! **Process-wide, not per node.** One chain, ordered, and `femtovg` and `cosmic-text` both fall
//! back across it per glyph. That is what makes a Nerd Font's private-use glyphs work beside a sans
//! face for body text without any node saying which it wants: the codepoint decides. A per-node
//! `font_family` is the separate and harder half, because measurement and paint select faces
//! through different mechanisms and `text::shaping`'s module doc already records what it cost when
//! those two disagreed.

use mlua::Lua;

/// The declared chain, in the order the config wrote it. Absent until a config calls `fonts`.
#[derive(Default)]
struct FontRegistry(Vec<String>);

/// Registers the `fonts(chain)` global, taking an array of family names.
///
/// Last call wins rather than accumulating. Two `fonts` declarations in one config are a mistake,
/// and appending them would silently produce a chain neither file asked for; taking the last at
/// least matches what a reader of the second one expects.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "fonts",
        lua.create_function(|lua, chain: mlua::Table| {
            let mut families = Vec::new();
            for (index, entry) in chain.sequence_values::<mlua::Value>().enumerate() {
                // Checked rather than left to mlua's `FromLua`, which coerces a number to a string
                // the way Lua itself does. `fonts { 12 }` would otherwise record a family named
                // "12", and the only symptom would be `resolve_chain` logging that it could not
                // find it -- a miss that reads like the font is not installed rather than like the
                // config is wrong.
                let mlua::Value::String(family) = entry? else {
                    return Err(mlua::Error::runtime(format!(
                        "fonts() takes an array of family-name strings; entry {} is not a string",
                        index + 1
                    )));
                };
                families.push(family.to_string_lossy());
            }
            lua.set_app_data(FontRegistry(families));
            Ok(())
        })?,
    )
}

/// What the config declared, or empty if it declared nothing.
///
/// Empty means keep `text::fonts::DEFAULT_CHAIN`: a config that never mentions fonts is the normal
/// case, not one that wants no text.
/// ponytail: read once, at startup, by `wayland::run`. Editing `fonts { ... }` in a live config
/// re-evaluates and records the new chain here, and nothing acts on it until the process restarts.
///
/// Deliberate rather than missed. Acting on it needs the change to reach the Wayland thread, which
/// owns the `TextPainter` that would have to be dropped and rebuilt, so it needs a new
/// `FrameOutcome` threaded through the reload path that PBA also runs on. That is a lot of delicate
/// machinery for a declaration a config writes once and then does not touch, and a font chain
/// change invalidates every measurement in the shell, which is closer to a topology change than to
/// the in-place restyle a reload is for. The upgrade path is that outcome variant, when something
/// wants it.
pub fn declared_chain(lua: &Lua) -> Vec<String> {
    lua.app_data_ref::<FontRegistry>().map(|registry| registry.0.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_fonts() -> Lua {
        let lua = Lua::new();
        register(&lua).unwrap();
        lua
    }

    #[test]
    fn a_config_that_never_mentions_fonts_declares_no_chain() {
        assert!(declared_chain(&lua_with_fonts()).is_empty());
    }

    #[test]
    fn a_declared_chain_is_read_back_in_the_order_it_was_written() {
        let lua = lua_with_fonts();
        lua.load(r#"fonts { "CaskaydiaCove Nerd Font Propo", "Noto Sans", "Noto Color Emoji" }"#).exec().unwrap();
        assert_eq!(
            declared_chain(&lua),
            vec!["CaskaydiaCove Nerd Font Propo", "Noto Sans", "Noto Color Emoji"],
            "order is the fallback order, so it has to survive the round trip"
        );
    }

    #[test]
    fn the_last_declaration_wins_rather_than_the_two_being_appended() {
        let lua = lua_with_fonts();
        lua.load(r#"fonts { "A" } fonts { "B" }"#).exec().unwrap();
        assert_eq!(declared_chain(&lua), vec!["B"]);
    }

    /// A number is where this bites: mlua's own `FromLua` for `String` coerces one the way Lua
    /// does, so without the check the chain records a family named "12" and the only symptom is
    /// `resolve_chain` reporting it could not find it.
    #[test]
    fn a_chain_entry_that_is_not_a_string_is_refused_by_name() {
        let lua = lua_with_fonts();
        let err = lua.load(r#"fonts { "Noto Sans", 12 }"#).exec().unwrap_err().to_string();
        assert!(err.contains("entry 2"), "the message must say which entry is wrong: {err}");
        assert!(lua.load(r#"fonts("Noto Sans")"#).exec().is_err(), "a bare string is not a chain");
        assert!(declared_chain(&lua).is_empty(), "a refused call records nothing");
    }
}
