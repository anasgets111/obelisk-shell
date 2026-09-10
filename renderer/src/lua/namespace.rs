//! The `oblisk` table for capabilities, `rescue`, `screens`, `version`, and `config_dir`
//! (`CONTEXT.md`, **Oblisk namespace**).
//!
//! Built once per generation before `shell.lua`; it answers no `SupervisorFrame`, so construction
//! stays separate from `crate::socket` frame handling.

use std::collections::HashMap;
use std::path::Path;

use crate::lua::Loader;
use crate::lua::capability::{Capability, CapabilityHandle, CommandSender};
use crate::lua::idle::IdleRegistry;
use crate::lua::signal::{DirtyFlag, LiveSignalHandle};

/// One generation's `oblisk` table and the handles its owner writes after construction.
pub(crate) struct Namespace {
    pub(crate) table: mlua::Table,
    /// One `StateSnapshot` hydration handle per `shared::Capability::ALL` name.
    pub(crate) capabilities: HashMap<String, CapabilityHandle>,
    pub(crate) rescue: LiveSignalHandle,
    /// `oblisk.idle` registry for inbound `SupervisorFrame::IdleEvent` callback dispatch.
    pub(crate) idle: IdleRegistry,
    pub(crate) screens: LiveSignalHandle,
    /// Current `screens` payload, for diffing later output changes.
    pub(crate) screens_payload: serde_json::Value,
}

/// Builds `oblisk`: every roster name, Renderer-sourced `rescue`/`screens`, `idle`, `version`, and
/// `config_dir`.
///
/// **Roster names stay off the table.** `__index` moves each from a side table on first read and
/// starts its controller (ADR-0070 decision 1). Before the first push, config reads a live `nil`
/// signal
/// (ADR-0037); an unread name costs only that `nil`, not the old D-Bus subscription. The unrelated
/// unrostered `crate::socket` lazy path starts from `StateSnapshot`, not here.
///
/// **One table, so typos raise.** § 6's `lock` constructor owns global `lock`; a bare `lock` signal
/// once overwrote it silently and broke every `lock { ... }` declaration (ADR-0052 decision 1).
pub(crate) fn build(
    loader: &Loader,
    dirty: &DirtyFlag,
    commands: &CommandSender,
    shell_lua_path: &Path,
) -> mlua::Result<Namespace> {
    let table = loader.create_table()?;
    let mut capabilities = HashMap::new();
    let pending = loader.create_table()?;
    // `idle` alone bypasses `pending`: its three callbacks cannot cross the wire as `:invoke`, so
    // `lua::idle` wraps it directly (ADR-0141). Its handle remains in `capabilities`, letting an
    // `idle` `StateSnapshot` hydrate the signal the wrapper reads.
    let mut idle_member = None;
    for capability in shared::Capability::ALL {
        let name = capability.as_str();
        let (member, handle) = Capability::new(name, dirty.clone(), commands.clone());
        if *capability == shared::Capability::Idle {
            idle_member = Some(member);
        } else {
            pending.set(name, member)?;
        }
        capabilities.insert(name.to_string(), handle);
    }
    install_capability_index(loader, &table, pending, commands.clone())?;
    let idle_state = idle_member.expect("shared::Capability::ALL must contain Idle");
    let idle = IdleRegistry::new(idle_state);
    table.set("idle", idle.member())?;
    loader.register_idle(idle.clone());
    let rescue = register_rescue_signal(loader, &table, dirty.clone())?;
    // Seed with an empty list, not `nil`, so `oblisk.screens` loops zero times; pass it to
    // `new_live` rather than `set` so initialization does not dirty an unapplied scene.
    let screens_payload = serde_json::Value::Array(Vec::new());
    let screens = register_screens_signal(loader, &table, dirty.clone(), &screens_payload)?;
    table.set("version", version_table(loader)?)?;
    // Parent of the loaded `shell.lua`, so config can name adjacent files without disagreeing with
    // `shared::config_dir()`. Static string beside `version`, not a pushing capability.
    table
        .set("config_dir", shell_lua_path.parent().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default())?;
    loader.set_global("oblisk", table.clone())?;
    Ok(Namespace { table, capabilities, rescue, idle, screens, screens_payload })
}

/// Puts `pending` behind `oblisk.__index`; first read installs the member and starts its controller
/// (ADR-0070 decision 1).
///
/// `raw_set` installs the member, so the metamethod fires once per name and later reads are
/// ordinary lookups. This matters because `computed({ oblisk.audio }, f)` in a `list` `itemfn`
/// indexes it once per row per layout pass.
///
/// Returns `nil` for absent names, preserving ordinary-table behavior: `oblisk.audioo` must remain
/// a Lua nil-index error naming the config line, not a metamethod error.
fn install_capability_index(
    loader: &Loader,
    oblisk: &mlua::Table,
    pending: mlua::Table,
    commands: CommandSender,
) -> mlua::Result<()> {
    let index = loader.lua().create_function(move |_, (table, key): (mlua::Table, mlua::LuaString)| {
        let name = key.to_str()?.to_owned();
        let member: mlua::Value = pending.raw_get(name.as_str())?;
        if member.is_nil() {
            return Ok(mlua::Value::Nil);
        }
        table.raw_set(name.as_str(), member.clone())?;
        commands.start_capability(&name);
        Ok(member)
    })?;
    let meta = loader.create_table()?;
    meta.set("__index", index)?;
    oblisk.set_metatable(Some(meta))?;
    Ok(())
}

/// `oblisk.rescue` (§ 2.10), returning its update handle.
///
/// Bare `lua::signal::Signal`, not [`Capability`]: Renderer-sourced, with no Supervisor dispatch or
/// roster entry, so `invoke` would only queue a command the Supervisor drops.
fn register_rescue_signal(loader: &Loader, oblisk: &mlua::Table, dirty: DirtyFlag) -> mlua::Result<LiveSignalHandle> {
    let table = rescue_table(loader, false, "")?;
    let (signal, handle) = crate::lua::signal::Signal::new_live(mlua::Value::Table(table), dirty);
    oblisk.set("rescue", signal)?;
    Ok(handle)
}

/// Registers reactive `oblisk.screens` (ADR-0041 decision 2), seeded with `initial`.
///
/// Deliberately outside `shared::Capability::ALL` and its map: `smithay_client_toolkit`'s
/// `OutputState` sources it in the Renderer, not the Supervisor's `StateSnapshot` roster
/// (ADR-0037/ADR-0041 decision 2). It still lives in `oblisk` because § 2.15 names it there.
fn register_screens_signal(
    loader: &Loader,
    oblisk: &mlua::Table,
    dirty: DirtyFlag,
    initial: &serde_json::Value,
) -> mlua::Result<LiveSignalHandle> {
    let (signal, handle) = crate::lua::signal::Signal::new_live(loader.to_lua_value(initial)?, dirty);
    oblisk.set("screens", signal)?;
    Ok(handle)
}

/// Renderer version as `{ major, minor, patch }` integers from `CARGO_PKG_VERSION_*`.
///
/// Plain table, not signal: it cannot change during the process. Register it at namespace build so
/// configs written before first use still have a version to guard on.
///
/// Use the Renderer's version, not the Supervisor's. They match today, but this process hosts the
/// VM and defines the config API if workspace versions diverge.
fn version_table(loader: &Loader) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    let [major, minor, patch] = version_parts();
    table.set("major", major)?;
    table.set("minor", minor)?;
    table.set("patch", patch)?;
    Ok(table)
}

/// `expect`, not `0`: a non-numeric `CARGO_PKG_VERSION_*` is a broken build, while silent `0.0.0`
/// makes config guards take the wrong branch forever. The socket test
/// `oblisk_version_is_three_integers_a_config_can_compare` reaches this through [`build`] and
/// catches panic or wrong values.
fn version_parts() -> [u32; 3] {
    [env!("CARGO_PKG_VERSION_MAJOR"), env!("CARGO_PKG_VERSION_MINOR"), env!("CARGO_PKG_VERSION_PATCH")].map(|part| {
        part.parse().expect("Cargo's CARGO_PKG_VERSION_* are the numeric components of an already-parsed semver")
    })
}

/// `oblisk.rescue`'s `{ is_rescue, error_log }` table, rebuilt by
/// `RendererClient::set_rescue_state` on each genuine rescue transition.
pub(crate) fn rescue_table(loader: &Loader, is_rescue: bool, error_log: &str) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    table.set("is_rescue", is_rescue)?;
    table.set("error_log", error_log)?;
    Ok(table)
}
