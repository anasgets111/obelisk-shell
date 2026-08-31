//! The `oblisk` namespace: the one Lua table every capability, `rescue`, `screens`, `version` and
//! `config_dir` hang off (`CONTEXT.md`, **Oblisk namespace**).
//!
//! Built once per generation, before `shell.lua` is ever evaluated. Nothing here answers a
//! `SupervisorFrame`; it is construction, which is why it does not live beside the frame handling
//! in `crate::socket`.

use std::collections::HashMap;
use std::path::Path;

use crate::lua::Loader;
use crate::lua::capability::{Capability, CapabilityHandle, CommandSender};
use crate::lua::idle::IdleRegistry;
use crate::lua::signal::{DirtyFlag, LiveSignalHandle};

/// One generation's built `oblisk` table, plus the handles its owner needs to keep writing to
/// after construction.
pub(crate) struct Namespace {
    pub(crate) table: mlua::Table,
    /// One handle per `shared::CAPABILITIES` name, for `StateSnapshot` pushes to hydrate.
    pub(crate) capabilities: HashMap<String, CapabilityHandle>,
    pub(crate) rescue: LiveSignalHandle,
    /// `oblisk.idle`'s registry, kept so an inbound `SupervisorFrame::IdleEvent` can find the
    /// callbacks the config registered for that threshold.
    pub(crate) idle: IdleRegistry,
    pub(crate) screens: LiveSignalHandle,
    /// The value `screens` currently holds, so a later output change can diff against it.
    pub(crate) screens_payload: serde_json::Value,
}

/// Builds the whole `oblisk` namespace: every `shared::CAPABILITIES` roster name, the two
/// Renderer-sourced signals `rescue` and `screens`, `idle`, `version`, and `config_dir`.
///
/// **Every roster name is pre-seeded here**, not left to a lazy path, so a `shell.lua` reading any
/// rostered capability before its first push gets a live signal reading `nil` instead of an
/// index-into-nil error and rescue (ADR-0037). The lazy path stays as the fallback for unrostered
/// names, in `crate::socket`, because it is reached from a `StateSnapshot` rather than from here.
///
/// **One table, so a typo is a Lua error rather than silence.** § 6.4's `lock` node constructor
/// owns the global `lock`, and a bare `lock` signal used to silently overwrite it and break every
/// `lock { ... }` declaration (docs/adr/0052 decision 1).
pub(crate) fn build(
    loader: &Loader,
    dirty: &DirtyFlag,
    commands: &CommandSender,
    shell_lua_path: &Path,
) -> mlua::Result<Namespace> {
    let table = loader.create_table()?;
    let mut capabilities = HashMap::new();
    for capability in shared::CAPABILITIES {
        let (member, handle) = Capability::new(capability, dirty.clone(), commands.clone());
        table.set(*capability, member)?;
        capabilities.insert((*capability).to_string(), handle);
    }
    // Off-roster like `rescue` and `screens`, but for the opposite reason: those are Renderer
    // state the Supervisor never pushes, and idle is a Supervisor service that pushes nothing --
    // its events are threshold crossings, not state. See `lua::idle`.
    let idle = IdleRegistry::new(commands.clone());
    table.set("idle", idle.member())?;
    loader.register_idle(idle.clone());
    let rescue = register_rescue_signal(loader, &table, dirty.clone())?;
    // Seeded to an empty list (not `nil`) so a config looping over `oblisk.screens` iterates zero
    // times rather than erroring, and set through `new_live`'s initial value rather than a `set` so
    // seeding it does not mark the scene dirty before anything has ever applied.
    let screens_payload = serde_json::Value::Array(Vec::new());
    let screens = register_screens_signal(loader, &table, dirty.clone(), &screens_payload)?;
    table.set("version", version_table(loader)?)?;
    // The directory the config was loaded from, so a config can name a file it ships beside itself.
    // A string beside `version` rather than a capability: static process information, not something
    // that pushes. The parent of `shell.lua` rather than a second call to `shared::config_dir()`,
    // so this cannot disagree with the file actually loaded.
    table
        .set("config_dir", shell_lua_path.parent().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default())?;
    loader.set_global("oblisk", table.clone())?;
    Ok(Namespace { table, capabilities, rescue, idle, screens, screens_payload })
}

/// `oblisk.rescue` (§ 2.10). Returns the handle so later evaluations can update it.
///
/// A bare `lua::signal::Signal` and not a [`Capability`], for the same reason
/// [`register_screens_signal`] is: this is Renderer-sourced, has no `dispatch` on the Supervisor
/// side and no roster entry, so an `invoke` on it could only ever be a command the Supervisor
/// drops.
fn register_rescue_signal(loader: &Loader, oblisk: &mlua::Table, dirty: DirtyFlag) -> mlua::Result<LiveSignalHandle> {
    let table = rescue_table(loader, false, "")?;
    let (signal, handle) = crate::lua::signal::Signal::new_live(mlua::Value::Table(table), dirty);
    oblisk.set("rescue", signal)?;
    Ok(handle)
}

/// Registers the reactive `oblisk.screens` signal (docs/adr/0041 decision 2), seeded with
/// `initial`.
///
/// Deliberately outside `shared::CAPABILITIES` and outside the capability map, an exception to the
/// shape ADR-0037 established that ADR-0041 decision 2 states as such: this is sourced in the
/// Renderer from `smithay_client_toolkit`'s `OutputState`, not pushed by the Supervisor as a
/// `StateSnapshot`, so the roster (the Supervisor's own dispatch and push list) has nothing to say
/// about it. It sits in the same table anyway, because § 2.15 names this `oblisk.screens` like
/// everything else in § 2.
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

/// This Renderer binary's version as `{ major, minor, patch }` integers, from Cargo's own
/// `CARGO_PKG_VERSION_*`.
///
/// A plain table, not a signal: it cannot change while the process runs. Registered on the day the
/// namespace is built rather than on the day a config needs it, since a config written before any
/// version exists has nothing to guard on, forever.
///
/// The Renderer's version and not the Supervisor's: they are the same number today because the
/// workspace versions both together, but the day they diverge this is still the right one, since it
/// is the process that hosts the VM and defines the API a config is written against.
fn version_table(loader: &Loader) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    let [major, minor, patch] = version_parts();
    table.set("major", major)?;
    table.set("minor", minor)?;
    table.set("patch", patch)?;
    Ok(table)
}

/// `expect` rather than a `0` fallback: a non-numeric `CARGO_PKG_VERSION_*` means the build is
/// broken, and a version table that quietly reads `0.0.0` is worse than not booting -- a config
/// would guard on it and take the wrong branch forever. Guarded by
/// `socket::tests::oblisk_version_is_three_integers_a_config_can_compare`, which reaches this
/// through [`build`] and so fails on the panic as well as on a wrong number.
fn version_parts() -> [u32; 3] {
    [env!("CARGO_PKG_VERSION_MAJOR"), env!("CARGO_PKG_VERSION_MINOR"), env!("CARGO_PKG_VERSION_PATCH")].map(|part| {
        part.parse().expect("Cargo's CARGO_PKG_VERSION_* are the numeric components of an already-parsed semver")
    })
}

/// `oblisk.rescue`'s `{ is_rescue, error_log }` table. `pub(crate)` for `RendererClient::set_rescue_state`,
/// which rebuilds it on every genuine rescue transition.
pub(crate) fn rescue_table(loader: &Loader, is_rescue: bool, error_log: &str) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    table.set("is_rescue", is_rescue)?;
    table.set("error_log", error_log)?;
    Ok(table)
}
