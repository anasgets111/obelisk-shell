//! `json` global table (ADR-0057), the config's only reader for structured subprocess output.
//!
//! `process.run`'s `out_cb` fires once per newline-stripped line, so a config polling
//! `lsblk --json` accumulates and decodes the buffer. Without a decoder, such subprocesses are
//! fire-and-forget.
//!
//! There is no `json.encode`. ponytail: configs still concatenate strings for `--json` input.
//! Upgrade with `encode` beside `decode`, reversing [`to_lua`]'s options; nothing needs it yet, and
//! an unused writer has no checked round trip.

use mlua::{Lua, LuaSerdeExt, Value};

/// The sole JSON-to-Lua mapping, used by pushed capability payloads (`Loader::to_lua_value`) and
/// `json.decode`.
///
/// mlua's serde bridge defaults `serialize_none_to_null`/`serialize_unit_to_null` to true, mapping
/// `Value::Null` to truthy lightuserdata. Then `if payload.field` treats null as real data. Disable
/// both options so `null` becomes Lua `nil` and erases the table key.
///
/// Cost: a JSON-array `null` leaves a hole and stops `ipairs`. Measured on `[1, null, 3]`, `ipairs`
/// yields one element, while `#` returns 3 and `xs[3]` still reads 3. The trade favors null fields,
/// which payloads use (`icon_path`, `toggle_state`, `icon_name` in a tray menu), over null
/// elements, which no capability produces; the alternative makes every optional field truthy.
pub fn to_lua(lua: &Lua, json: &serde_json::Value) -> mlua::Result<Value> {
    let options = mlua::serde::ser::Options::new().serialize_none_to_null(false).serialize_unit_to_null(false);
    lua.to_value_with(json, options)
}

/// Parse and conversion failures become one message for `json.decode`. A convention that requires
/// `pcall` is a raise with extra steps; wording distinguishes config input errors from engine
/// conversion errors.
///
/// `from_slice` enforces its recursion limit first, so no test reaches conversion failure and it
/// may be unreachable today. Keep it because [`register`]'s contract promises the error path.
fn decode(lua: &Lua, bytes: &[u8]) -> Result<Value, String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    to_lua(lua, &json).map_err(|err| format!("decoded, but could not be converted to a Lua value: {err}"))
}

/// Registers `json.decode(text)`.
///
/// Returns one value on success, or `nil` plus a message on failure, matching Lua's `io.open`, not
/// cjson. mlua's `IntoLuaMulti for Result` is already that shape, so [`decode`]'s `Result` goes
/// back whole. Decode failure is routine because `out_cb` supplies lines, including partial buffers
/// and non-JSON stdout; raising would force `pcall` at every call site. No trailing `nil`: with three
/// arguments, `table.insert(t, decoded, nil)` treats the second as a position and raises "bad
/// argument #2 to 'insert' (number expected, got table)". Measured, not assumed.
///
/// A bare top-level `null` succeeds as `nil`, indistinguishable from decode failure to `if t then`
/// (pinned by `decode_of_a_bare_top_level_null_is_indistinguishable_from_a_decode_failure`). A
/// success sentinel would reintroduce the truthy-lightuserdata bug that [`to_lua`] avoids.
///
/// Take `mlua::LuaString`, not Rust `String`, so non-UTF-8 bytes reach serde as a readable decode
/// error instead of an mlua argument-conversion error raised past `if err then`.
///
/// `json.decode` does not raise on input, well-formed or not, unless Lua cannot allocate the
/// message string.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    let table = lua.create_table()?;
    table.set("decode", lua.create_function(|lua, text: mlua::LuaString| Ok(decode(lua, &text.as_bytes())))?)?;
    lua.globals().set("json", table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_json() -> Lua {
        let lua = Lua::new();
        register(&lua).unwrap();
        lua
    }

    #[test]
    fn decode_turns_a_json_object_into_a_table_a_config_can_index() {
        let lua = lua_with_json();
        let volume: f64 = lua.load(r#"return json.decode('{"volume":0.5,"muted":false}').volume"#).eval().unwrap();
        assert_eq!(volume, 0.5);
    }

    #[test]
    fn decode_reports_malformed_input_as_nil_plus_a_message_rather_than_raising() {
        let lua = lua_with_json();
        let (value, message): (Value, Option<String>) = lua.load(r#"return json.decode('{"volume":')"#).eval().unwrap();
        assert_eq!(value, Value::Nil);
        assert!(message.is_some_and(|m| !m.is_empty()), "the second return has to carry something a config can log");
    }

    #[test]
    fn decode_rejects_nesting_deep_enough_to_overflow_a_recursive_walk_instead_of_crashing() {
        let lua = lua_with_json();
        lua.globals().set("deep", format!("{}1{}", "[".repeat(10_000), "]".repeat(10_000))).unwrap();
        let (value, message): (Value, Option<String>) = lua.load("return json.decode(deep)").eval().unwrap();
        assert_eq!(value, Value::Nil);
        assert!(message.is_some(), "a config polling an untrusted subprocess must get an error, not a stack overflow");
    }

    #[test]
    fn decode_maps_a_null_field_to_an_absent_key_exactly_as_a_pushed_capability_payload_does() {
        let lua = lua_with_json();
        let (key_count, absent): (i64, bool) = lua
            .load(
                r#"
                local t = json.decode('{"icon_name":"a","icon_path":null}')
                local n = 0
                for _ in pairs(t) do n = n + 1 end
                return n, t.icon_path == nil
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(key_count, 1, "a null field is erased, not left as a truthy sentinel");
        assert!(absent);
    }

    #[test]
    fn decode_returns_one_value_on_success_so_it_can_be_forwarded_straight_into_another_call() {
        let lua = lua_with_json();
        // A trailing nil would make three-argument `table.insert` treat the value as a position.
        let count: i64 = lua.load(r#"local t = {} table.insert(t, json.decode('{"a":1}')) return #t"#).eval().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn decode_reads_a_json_array_as_a_one_based_lua_array() {
        let lua = lua_with_json();
        let (len, second): (i64, String) =
            lua.load(r#"local t = json.decode('["a","b"]') return #t, t[1]"#).eval().unwrap();
        assert_eq!((len, second.as_str()), (2, "a"));
    }

    #[test]
    fn decode_treats_bytes_that_are_not_utf8_as_a_decode_error_not_a_lua_argument_error() {
        let lua = lua_with_json();
        lua.globals().set("raw", lua.create_string(b"{\"name\":\"\xff\xfe\"}").unwrap()).unwrap();
        let (value, message): (Value, Option<String>) = lua.load("return json.decode(raw)").eval().unwrap();
        assert_eq!(
            value,
            Value::Nil,
            "a subprocess emitting a non-utf8 byte must not raise past the config's error check"
        );
        assert!(message.is_some());
    }

    #[test]
    fn decode_of_a_bare_top_level_null_is_indistinguishable_from_a_decode_failure() {
        let lua = lua_with_json();
        let (value, message): (Value, Option<String>) = lua.load(r#"return json.decode('null')"#).eval().unwrap();
        assert_eq!(value, Value::Nil);
        assert_eq!(
            message, None,
            "it succeeded, so there is no message -- but `if t then` cannot tell that apart from a parse error"
        );
    }

    /// Real captured `niri msg -j focused-window` input, preserving its non-ASCII title and
    /// `layout` null nested two levels deep.
    #[test]
    fn decode_reads_a_real_niri_focused_window_line_including_its_nested_nulls() {
        let lua = lua_with_json();
        lua.globals()
            .set(
                "line",
                r#"{"id":2,"title":"◐ Docs/build-steps.md next step","app_id":"kitty","pid":1481,"workspace_id":11,"is_focused":true,"layout":{"pos_in_scrolling_layout":null,"tile_size":[1536.0,960.0],"window_size":[1536,960],"tile_pos_in_workspace_view":[192.0,120.0]},"focus_timestamp":{"secs":64419,"nanos":188252639}}"#,
            )
            .unwrap();
        let (title, scrolling_absent, width): (String, bool, i64) = lua
            .load(
                r#"
                local w = json.decode(line)
                return w.title, w.layout.pos_in_scrolling_layout == nil, w.layout.window_size[1]
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            title, "\u{25d0} Docs/build-steps.md next step",
            "a \\uXXXX escape has to come back out as the character it names"
        );
        assert!(scrolling_absent, "a null two levels down has to erase its key like a top-level one");
        assert_eq!(width, 1536, "an integer array element stays an integer, not a float");
    }
}
