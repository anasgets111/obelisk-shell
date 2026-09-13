//! `persistent_table { path, name, defaults }` (ADR-0136): named JSON file, read as signals and
//! written one key at a time.
//!
//! Config supplies `path` and `name`, usually from `obelisk.config_dir` and `os.getenv` (one of
//! ADR-0048's four calls), so `$XDG_STATE_HOME`, `$XDG_CACHE_HOME`, a file beside `shell.lua`, and
//! three simultaneous files are all the same call with different inputs.
//!
//! Plain Lua table, not userdata: a missing `store.theme` falls through `__index`, gets a signal,
//! and is `rawset` so later reads are ordinary. `set` is a real field, so configs cannot store that
//! key.

use std::collections::HashMap;

use mlua::{Lua, ObjectLike, Table, Value};

use crate::lua::signal::{Signal, from_userdata};

/// Stores keyed by joined absolute path: two calls naming one file share one table/signals. It
/// survives VM re-evaluation (ADR-0044 decision 4), so reload does not hand back a second table.
#[derive(Default)]
struct StoreRegistry(HashMap<String, Table>);

/// Registers `persistent_table`. Resolve `obelisk.storage` at call time; registration runs in
/// `Loader::new`, before `lua::namespace::build` creates `obelisk`.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "persistent_table",
        lua.create_function(|lua, spec: Table| {
            let path: String = spec.get("path")?;
            let name: String = spec.get("name")?;
            let defaults: Value = spec.get("defaults")?;
            let file = join(&path, &name)?;

            let storage = storage_capability(lua)?;
            // Send every evaluation: the Supervisor merges defaults (ADR-0136 decision 4), so
            // edited defaults land on reload without reverting user values.
            let defaults = match defaults {
                Value::Nil => Value::Table(lua.create_table()?),
                defaults => defaults,
            };
            storage.call_method::<()>("invoke", ("open", file.clone(), defaults))?;

            if lua.app_data_ref::<StoreRegistry>().is_none() {
                lua.set_app_data(StoreRegistry::default());
            }
            if let Some(existing) =
                lua.app_data_ref::<StoreRegistry>().expect("just ensured the registry exists").0.get(&file).cloned()
            {
                return Ok(existing);
            }

            let store = build_store(lua, &file, storage)?;
            lua.app_data_mut::<StoreRegistry>()
                .expect("just ensured the registry exists")
                .0
                .insert(file, store.clone());
            Ok(store)
        })?,
    )
}

/// Config file as one absolute path.
///
/// Keep `path` and `name` separate because config computes the directory and writes the filename.
/// Join here so config and Supervisor use the same key by construction.
fn join(path: &str, name: &str) -> mlua::Result<String> {
    if !path.starts_with('/') {
        return Err(mlua::Error::runtime(format!(
            "persistent_table: path must be absolute, got {path:?}. A relative path resolves against the Supervisor's working directory, which nothing sets"
        )));
    }
    if name.is_empty() || name.contains('/') {
        return Err(mlua::Error::runtime(format!("persistent_table: name is one file name, not a path, got {name:?}")));
    }
    Ok(format!("{}/{name}", path.trim_end_matches('/')))
}

/// `obelisk.storage` through namespace `__index`, so the read starts the capability
/// (ADR-0070 decision 1).
fn storage_capability(lua: &Lua) -> mlua::Result<mlua::AnyUserData> {
    let obelisk: Table = lua.globals().get("obelisk").map_err(|_| {
        mlua::Error::runtime("persistent_table: the `obelisk` namespace is not built yet on this Lua state")
    })?;
    obelisk.get("storage")
}

/// Config table: real `set` field; `__index` answers other keys with per-file signals.
fn build_store(lua: &Lua, file: &str, storage: mlua::AnyUserData) -> mlua::Result<Table> {
    let store = lua.create_table()?;
    let path = file.to_string();
    store.set(
        "set",
        lua.create_function(move |_, (_store, key, value): (Table, String, Value)| {
            storage.call_method::<()>("invoke", ("set", path.clone(), key, value))
        })?,
    )?;

    let metatable = lua.create_table()?;
    let signal_source: mlua::AnyUserData = lua.globals().get::<Table>("obelisk")?.get("storage")?;
    let signal = from_userdata(&signal_source)
        .ok_or_else(|| mlua::Error::runtime("persistent_table: obelisk.storage is not a signal"))?;
    let path = file.to_string();
    metatable.set(
        "__index",
        lua.create_function(move |lua, (store, key): (Table, String)| {
            let key_signal = key_signal(lua, &signal, &path, &key)?;
            // Cache on the table: later reads are plain and each key has one signal.
            store.raw_set(key.as_str(), key_signal.clone())?;
            Ok(key_signal)
        })?,
    )?;
    store.set_metatable(Some(metatable))?;
    Ok(store)
}

/// One file key mapped over `obelisk.storage`. `nil` before first push and for absent keys,
/// matching the property's documented default.
fn key_signal(lua: &Lua, storage: &Signal, file: &str, key: &str) -> mlua::Result<Signal> {
    let file = file.to_string();
    let key = key.to_string();
    let read = lua.create_function(move |_, payload: Value| {
        let Value::Table(payload) = payload else { return Ok(Value::Nil) };
        let Value::Table(files) = payload.get::<Value>("files")? else { return Ok(Value::Nil) };
        let Value::Table(stored) = files.get::<Value>(file.as_str())? else { return Ok(Value::Nil) };
        stored.get::<Value>(key.as_str())
    })?;
    Ok(storage.mapped(read))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_and_a_name_join_into_one_absolute_file() {
        assert_eq!(join("/home/u/.config/bar", "settings.json").unwrap(), "/home/u/.config/bar/settings.json");
        assert_eq!(join("/home/u/.config/bar/", "settings.json").unwrap(), "/home/u/.config/bar/settings.json");
    }

    #[test]
    fn a_relative_path_is_refused_at_the_call_rather_than_on_the_wire() {
        let err = join(".config/bar", "settings.json").unwrap_err().to_string();
        assert!(err.contains("must be absolute"), "the message has to say what to fix: {err}");
    }

    #[test]
    fn a_name_carrying_a_separator_is_refused() {
        assert!(join("/home/u", "nested/settings.json").is_err());
        assert!(join("/home/u", "").is_err());
    }
}
