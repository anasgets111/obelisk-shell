//! `session_process { name, stop_signal }`: one long-running program, declared by name, read as
//! signals and driven with methods.
//!
//! Sibling of `lua::store`'s `persistent_table` and built the same way -- a plain Lua table whose
//! real fields are the methods and whose `__index` answers everything else with a signal mapped
//! over the owning capability, cached with `rawset` so later reads are ordinary.
//!
//! The reason it exists is lifetime. `process.run`'s child belongs to the generation that spawned
//! it and its group is reaped on every swap, which is right for a helper that answers a question
//! and exits. A program the config wants to keep -- a recorder, a stream a widget reads -- has to
//! outlive the VM that started it, and the only thing here that does is the Supervisor. So the
//! config names the program and the Supervisor holds it; what comes back is state, like every
//! other capability, rather than a handle that would be stale by the next reload.

use std::collections::HashMap;

use mlua::{Lua, ObjectLike, Table, Value};

use crate::lua::signal::{Signal, from_userdata};

/// Handles keyed by declared name, so two declarations of one program share a table and its
/// signals. Survives VM re-evaluation (ADR-0044 decision 4), so a reload hands back the same one.
#[derive(Default)]
struct SessionRegistry(HashMap<String, Table>);

/// Registers `session_process`. `obelisk.processes` is resolved at call time: registration runs in
/// `Loader::new`, before `lua::namespace::build` creates `obelisk`.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "session_process",
        lua.create_function(|lua, spec: Table| {
            let name: String = spec.get("name")?;
            if name.is_empty() {
                return Err(mlua::Error::runtime(
                    "session_process: name is how the Supervisor keys this program and how the config reads it back; it cannot be empty",
                ));
            }
            let stop_signal: Value = spec.get("stop_signal")?;

            let processes = processes_capability(lua)?;
            // Sent every evaluation, like `storage:open`: the Supervisor keeps the entry it has
            // and takes the newer stop signal, so editing that lands on reload without disturbing
            // a program already up.
            processes.call_method::<()>("invoke", ("declare", name.clone(), stop_signal))?;

            if lua.app_data_ref::<SessionRegistry>().is_none() {
                lua.set_app_data(SessionRegistry::default());
            }
            if let Some(existing) =
                lua.app_data_ref::<SessionRegistry>().expect("just ensured the registry exists").0.get(&name).cloned()
            {
                return Ok(existing);
            }

            let handle = build_handle(lua, &name, processes)?;
            lua.app_data_mut::<SessionRegistry>()
                .expect("just ensured the registry exists")
                .0
                .insert(name, handle.clone());
            Ok(handle)
        })?,
    )
}

/// `obelisk.processes` through namespace `__index`, so the read starts the capability
/// (ADR-0070 decision 1).
fn processes_capability(lua: &Lua) -> mlua::Result<mlua::AnyUserData> {
    let obelisk: Table = lua.globals().get("obelisk").map_err(|_| {
        mlua::Error::runtime("session_process: the `obelisk` namespace is not built yet on this Lua state")
    })?;
    obelisk.get("processes")
}

/// Config table: real `start`/`signal`/`stop` fields; `__index` answers other keys with signals.
fn build_handle(lua: &Lua, name: &str, processes: mlua::AnyUserData) -> mlua::Result<Table> {
    let handle = lua.create_table()?;

    let owner = processes.clone();
    let program = name.to_string();
    handle.set(
        "start",
        lua.create_function(move |_, (_handle, cmd, args): (Table, String, Option<Vec<String>>)| {
            owner.call_method::<()>("invoke", ("start", program.clone(), cmd, args.unwrap_or_default()))
        })?,
    )?;

    let owner = processes.clone();
    let program = name.to_string();
    handle.set(
        "signal",
        lua.create_function(move |_, (_handle, signal): (Table, String)| {
            owner.call_method::<()>("invoke", ("signal", program.clone(), signal))
        })?,
    )?;

    let owner = processes.clone();
    let program = name.to_string();
    handle.set(
        "stop",
        lua.create_function(move |_, _handle: Table| owner.call_method::<()>("invoke", ("stop", program.clone())))?,
    )?;

    let metatable = lua.create_table()?;
    let signal = from_userdata(&processes)
        .ok_or_else(|| mlua::Error::runtime("session_process: obelisk.processes is not a signal"))?;
    let program = name.to_string();
    metatable.set(
        "__index",
        lua.create_function(move |lua, (handle, key): (Table, String)| {
            let field = field_signal(lua, &signal, &program, &key)?;
            // Cache on the table: later reads are plain and each field has one signal.
            handle.raw_set(key.as_str(), field.clone())?;
            Ok(field)
        })?,
    )?;
    handle.set_metatable(Some(metatable))?;
    Ok(handle)
}

/// One field of one declared program, mapped over `obelisk.processes`. `nil` before the first push
/// and for a name the Supervisor has not answered for yet, matching the property's documented
/// default.
fn field_signal(lua: &Lua, processes: &Signal, name: &str, key: &str) -> mlua::Result<Signal> {
    let name = name.to_string();
    let key = key.to_string();
    let read = lua.create_function(move |_, payload: Value| {
        let Value::Table(payload) = payload else { return Ok(Value::Nil) };
        let Value::Table(sessions) = payload.get::<Value>("sessions")? else { return Ok(Value::Nil) };
        let Value::Table(session) = sessions.get::<Value>(name.as_str())? else { return Ok(Value::Nil) };
        session.get::<Value>(key.as_str())
    })?;
    Ok(processes.mapped(read))
}
