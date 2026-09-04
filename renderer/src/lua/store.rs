//! The `persistent_table { path, name, defaults }` global (ADR-0136): a JSON file the config
//! names, read as signals and written a key at a time.
//!
//! Nothing here decides where anything goes. `path` and `name` are the config's, and
//! `oblisk.config_dir` plus `os.getenv` (one of the four `os` calls ADR-0048 kept) are what a
//! config builds them from, so `$XDG_STATE_HOME`, `$XDG_CACHE_HOME`, a file beside `shell.lua` and
//! three files at once are all the same call made differently.
//!
//! The store is a plain Lua table, not userdata: `store.theme` misses the table, falls through to
//! `__index`, and gets a signal over that key which is then `rawset` so the second read is a plain
//! table lookup. `store:set` is a real field, which is why it is also the one key name a config
//! cannot store.

use std::collections::HashMap;

use mlua::{Lua, ObjectLike, Table, Value};

use crate::lua::signal::{Signal, from_userdata};

/// Every store this generation has built, keyed by the joined absolute path, so two
/// `persistent_table` calls naming one file are one table with one set of signals. Survives
/// re-evaluation with the VM (ADR-0044 decision 4), which is what keeps a reload from handing the
/// config a second table over the same file.
#[derive(Default)]
struct StoreRegistry(HashMap<String, Table>);

/// Registers the `persistent_table` global. Reads `oblisk.storage` at call time rather than at
/// registration: this runs in `Loader::new`, and `lua::namespace::build` has not put the `oblisk`
/// table in place yet.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "persistent_table",
        lua.create_function(|lua, spec: Table| {
            let path: String = spec.get("path")?;
            let name: String = spec.get("name")?;
            let defaults: Value = spec.get("defaults")?;
            let file = join(&path, &name)?;

            let storage = storage_capability(lua)?;
            // Re-sent by every evaluation rather than only the first: the Supervisor merges
            // defaults into what it already holds (ADR-0136 decision 4), so an edit to `defaults`
            // lands on a reload while a value the user changed does not revert.
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

/// The file a config named, as one absolute path.
///
/// `path` and `name` stay two arguments because the directory is the part a config computes and
/// the file name is the part it writes as a literal. Joined here rather than in the Supervisor so
/// the key a config reads back and the key the Supervisor stores are the same string by
/// construction rather than by two implementations agreeing.
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

/// `oblisk.storage`, read through the namespace's `__index` so the read itself is what asks the
/// Supervisor to start the capability (ADR-0070 decision 1).
fn storage_capability(lua: &Lua) -> mlua::Result<mlua::AnyUserData> {
    let oblisk: Table = lua.globals().get("oblisk").map_err(|_| {
        mlua::Error::runtime("persistent_table: the `oblisk` namespace is not built yet on this Lua state")
    })?;
    oblisk.get("storage")
}

/// The table a config holds: `set` as a real field, every other key answered by `__index` with a
/// signal over that key of that file.
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
    let signal_source: mlua::AnyUserData = lua.globals().get::<Table>("oblisk")?.get("storage")?;
    let signal = from_userdata(&signal_source)
        .ok_or_else(|| mlua::Error::runtime("persistent_table: oblisk.storage is not a signal"))?;
    let path = file.to_string();
    metatable.set(
        "__index",
        lua.create_function(move |lua, (store, key): (Table, String)| {
            let key_signal = key_signal(lua, &signal, &path, &key)?;
            // Cached onto the table itself, so the second read of `store.theme` is a plain lookup
            // and the config holds one signal per key rather than one per resolve.
            store.raw_set(key.as_str(), key_signal.clone())?;
            Ok(key_signal)
        })?,
    )?;
    store.set_metatable(Some(metatable))?;
    Ok(store)
}

/// One key of one file, as a signal over the whole `oblisk.storage` payload. `nil` until the first
/// push, and `nil` for a key the file does not have, which is § 3.1's rule: an absent signal value
/// leaves the property's documented default in place.
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
