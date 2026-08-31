//! `json` global table (`oblisk-idl-api-specs.md` § 3.3, docs/adr/0057, build-steps.md section 6
//! item 2) -- the config's only reader for structured subprocess output.
//!
//! `process.run`'s `out_cb` fires once per line, newline stripped, so a config polling
//! `lsblk --json` accumulates lines and decodes the buffer. Without a decoder every such
//! subprocess is fire-and-forget; with one it is a data source.
//!
//! There is no `json.encode`. ponytail: a config cannot build a JSON argument to hand a subprocess,
//! so anything wanting `--json` *input* still concatenates strings by hand. Upgrade: an `encode`
//! beside `decode`, serializing through the same [`to_lua`] options in reverse. Nothing needs it
//! yet, and a writer nobody calls is a writer nobody has checked round-trips.

use mlua::{IntoLua, Lua, LuaSerdeExt, MultiValue, Value};

/// The one JSON-to-Lua mapping this engine has. Both callers go through it: a pushed capability
/// payload (`Loader::to_lua_value`) and `json.decode` below.
///
/// mlua's serde bridge defaults `serialize_none_to_null`/`serialize_unit_to_null` to true, which
/// maps `Value::Null` to a lightuserdata sentinel rather than Lua `nil` -- and lightuserdata is
/// truthy, so `if payload.field then` took the branch that assumes a real value (docs/build-steps.md
/// Phase 19 item 16). Both options are turned off here so `null` becomes `nil`, which also erases
/// the key from the table entirely rather than leaving it present with a nil-ish value.
///
/// The cost: a `null` sitting in a JSON *array* now leaves a hole, and `ipairs` stops at a hole.
/// Measured on `[1, null, 3]`: `ipairs` yields one element, while `#` returns 3 and `xs[3]` still
/// reads back 3. Still the right trade: a null *field* is the shape every payload actually has
/// (`icon_path`, `toggle_state`, `icon_name` in a tray menu), a null array *element* is not one any
/// capability produces today, and the alternative leaves every optional field truthy.
pub fn to_lua(lua: &Lua, json: &serde_json::Value) -> mlua::Result<Value> {
    let options = mlua::serde::ser::Options::new().serialize_none_to_null(false).serialize_unit_to_null(false);
    lua.to_value_with(json, options)
}

/// Both ways this can fail, flattened into the one message `json.decode` hands back: a convention
/// a config has to `pcall` around anyway is not a convention, it is a raise with extra steps. The
/// two are told apart by their wording -- bad input is the config author's problem, a conversion
/// failure is this engine's.
///
/// The conversion arm is not reached by any test and may not be reachable at all today, since
/// `from_slice` enforces its own recursion limit and rejects anything deep enough first. Handled
/// because [`register`]'s doc comment promises it is.
fn decode(lua: &Lua, bytes: &[u8]) -> Result<Value, String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    to_lua(lua, &json).map_err(|err| format!("decoded, but could not be converted to a Lua value: {err}"))
}

/// Registers the `json` global with `json.decode(text)`.
///
/// The return convention is Lua's own, not cjson's: one value on success, `nil` plus a message on
/// failure, matching `io.open`. Raising is wrong here because a decode failure is a *routine*
/// path -- `out_cb` delivers a line at a time, so a config often decodes a partial buffer or a
/// failing subprocess's non-JSON stdout, and raising would put a `pcall` around every call site.
/// One value on success rather than a trailing `nil` is load-bearing too: with three arguments
/// `table.insert` reads the second as a *position*, so `table.insert(t, decoded, nil)` raises
/// "bad argument #2 to 'insert' (number expected, got table)" and inserts nothing. Measured, not
/// assumed.
///
/// One ambiguous case, pinned by
/// `decode_of_a_bare_top_level_null_is_indistinguishable_from_a_decode_failure`: a bare top-level
/// `null` decodes successfully to `nil`, which `if t then` reads as failure. Both mean "no data",
/// and the fix (a sentinel for a successful null) would reintroduce the truthy-lightuserdata bug
/// [`to_lua`] exists to avoid.
///
/// The argument is an `mlua::LuaString`, not a Rust `String`, so a non-UTF-8 byte reaches serde as
/// a decode error the config can read instead of an mlua argument-conversion error raised past its
/// `if err then` check.
///
/// `json.decode` does not raise on any input, well-formed or not, short of Lua failing to allocate
/// the message string itself.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    let table = lua.create_table()?;
    table.set(
        "decode",
        lua.create_function(|lua, text: mlua::LuaString| match decode(lua, &text.as_bytes()) {
            Ok(value) => Ok(MultiValue::from_vec(vec![value])),
            Err(message) => Ok(MultiValue::from_vec(vec![Value::Nil, lua.create_string(message)?.into_lua(lua)?])),
        })?,
    )?;
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
        // With three arguments `table.insert` reads the second as a position, so a decoder that
        // always returned a trailing nil would raise here rather than insert.
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

    /// A real captured `niri msg -j focused-window` line, worth pinning over a synthetic fixture
    /// because it carries a non-ASCII title and a `null` nested two levels deep inside `layout`.
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
