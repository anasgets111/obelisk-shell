//! `fonts { ... }` declares the font families used for measurement and paint (ADR-0043 decision 2).
//!
//! It records in `Lua::app_data` rather than returning a value: `shell.lua` returns surfaces, so
//! there is no slot for a font chain. Like `state`, `hover`, and `scroll`, it is written during
//! config evaluation and read afterwards.
//!
//! This stays out of `lua::signal`: those registries share `SignalKind`, write gating, and the
//! dirty flag; this is a plain, non-reactive list read once.
//!
//! **The declared chain, not the only font.** This is the fallback chain every node uses unless it
//! names a family itself: cosmic-text picks each glyph's face, falling back through every loaded
//! family, and femtovg draws that face (ADR-0211), so CJK and emoji coverage sits behind a sans
//! body face and the codepoint picks the face. A `text` node that
//! sets `font = "<family>"` leads with that family instead and keeps this chain behind it as
//! coverage (ADR-0144); the family is resolved on first sight, through this module's own
//! `fc-match` path, into the same database -- so there is still one font discovery, not two.

use mlua::Lua;

/// The declared chain in config order, absent until `fonts` is called.
#[derive(Default)]
struct FontRegistry(Vec<String>);

/// Registers `fonts(chain)`, taking an array of family names.
///
/// Last call wins. Multiple declarations are a mistake; appending would silently create a chain
/// neither declaration asked for, while replacement matches the second declaration's reading.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "fonts",
        lua.create_function(|lua, chain: mlua::Table| {
            // Collect every key, then require exactly `1..=n`: `sequence_values` stops at the first
            // `nil`, while Lua's `#` is undefined for sparse tables and returns 1 for
            // `{ [1] = "A", [3] = "C" }`. Either would silently drop the tail and shorten fallback.
            let mut indexed: Vec<(i64, String)> = Vec::new();
            for pair in chain.pairs::<mlua::Value, mlua::Value>() {
                let (key, value) = pair?;
                let mlua::Value::Integer(index) = key else {
                    return Err(mlua::Error::runtime(
                        "fonts() takes an array of family-name strings, not a table with named keys",
                    ));
                };
                // Check before mlua's `FromLua`, which coerces numbers like Lua. Without this,
                // `fonts { 12 }` records family `"12"`; only `resolve_chain` then reports a
                // missing font, hiding the config error.
                let mlua::Value::String(family) = value else {
                    return Err(mlua::Error::runtime(format!(
                        "fonts() takes an array of family-name strings; entry {index} is not a string"
                    )));
                };
                indexed.push((index, family.to_string_lossy()));
            }
            indexed.sort_by_key(|(index, _)| *index);
            for (position, (index, _)) in indexed.iter().enumerate() {
                let expected = position as i64 + 1;
                if *index != expected {
                    return Err(mlua::Error::runtime(format!(
                        "fonts() takes a dense array of family-name strings; entry {expected} is missing"
                    )));
                }
            }
            lua.set_app_data(FontRegistry(indexed.into_iter().map(|(_, family)| family).collect()));
            Ok(())
        })?,
    )
}

/// What the config declared, or empty if it declared nothing.
///
/// Empty keeps `text::fonts::DEFAULT_CHAIN`; omitting `fonts` is normal, not "no text".
/// ponytail: a config writes this once and normally never touches it; `wayland::run` reads it once
/// at startup. A live edit re-evaluates and records a new chain, but nothing uses it until restart.
///
/// Deliberate: applying a change requires reaching the Wayland thread that owns the `TextPainter`,
/// dropping and rebuilding it, and threading a new `FrameOutcome` through the swap's reload path.
/// A chain change invalidates every measurement, unlike reload's in-place restyle. Upgrade with
/// that outcome variant when live font changes are needed.
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

    /// Review found that `sequence_values` stops at the first `nil`: `{ [1] = "A", [3] = "C" }`
    /// recorded `["A"]` and silently shortened the fallback chain.
    #[test]
    fn a_hole_in_the_chain_is_refused_rather_than_truncating_it() {
        let lua = lua_with_fonts();
        let err = lua.load(r#"local t = {} t[1] = "A" t[3] = "C" fonts(t)"#).exec().unwrap_err().to_string();
        assert!(err.contains("entry 2"), "the message must name the gap: {err}");
        assert!(declared_chain(&lua).is_empty(), "a refused call records nothing");
    }

    /// The key walk catches named keys, which are not array entries and would silently drop a
    /// declared family.
    #[test]
    fn a_table_with_named_keys_is_not_a_chain() {
        let lua = lua_with_fonts();
        assert!(lua.load(r#"fonts { family = "Noto Sans" }"#).exec().is_err());
        assert!(declared_chain(&lua).is_empty());
    }

    #[test]
    fn the_last_declaration_wins_rather_than_the_two_being_appended() {
        let lua = lua_with_fonts();
        lua.load(r#"fonts { "A" } fonts { "B" }"#).exec().unwrap();
        assert_eq!(declared_chain(&lua), vec!["B"]);
    }

    /// mlua's `FromLua<String>` coerces numbers like Lua, so without the check `12` becomes family
    /// `"12"` and only `resolve_chain` reports it missing.
    #[test]
    fn a_chain_entry_that_is_not_a_string_is_refused_by_name() {
        let lua = lua_with_fonts();
        let err = lua.load(r#"fonts { "Noto Sans", 12 }"#).exec().unwrap_err().to_string();
        assert!(err.contains("entry 2"), "the message must say which entry is wrong: {err}");
        assert!(lua.load(r#"fonts("Noto Sans")"#).exec().is_err(), "a bare string is not a chain");
        assert!(declared_chain(&lua).is_empty(), "a refused call records nothing");
    }
}
