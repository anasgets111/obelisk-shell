//! Hand-written BlueZ proxies (ADR-0030: no maintained zbus proxy crate for BlueZ).
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use zbus::zvariant::{ObjectPath, OwnedObjectPath};

#[zbus::proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
pub(super) trait Adapter1 {
    #[zbus(name = "StartDiscovery")]
    fn start_discovery(&self) -> zbus::Result<()>;

    #[zbus(name = "StopDiscovery")]
    fn stop_discovery(&self) -> zbus::Result<()>;

    #[zbus(name = "RemoveDevice")]
    fn remove_device(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_powered(&self, value: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub(super) trait Device1 {
    #[zbus(name = "Connect")]
    fn connect(&self) -> zbus::Result<()>;

    #[zbus(name = "Disconnect")]
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(name = "Pair")]
    fn pair(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn class(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn set_trusted(&self, value: bool) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.bluez.Battery1", default_service = "org.bluez")]
pub(super) trait Battery1 {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<u8>;
}

#[zbus::proxy(interface = "org.bluez.AgentManager1", default_service = "org.bluez", default_path = "/org/bluez")]
pub(super) trait AgentManager1 {
    #[zbus(name = "RegisterAgent")]
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;

    #[zbus(name = "RequestDefaultAgent")]
    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// Wrap each macro-generated `builder()` so per-path binding stays one line.
pub(super) async fn bind_adapter(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> zbus::Result<Adapter1Proxy<'static>> {
    Adapter1Proxy::builder(connection).path(path)?.build().await
}

pub(super) async fn bind_device(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> zbus::Result<Device1Proxy<'static>> {
    Device1Proxy::builder(connection).path(path)?.build().await
}

pub(super) async fn bind_battery(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> zbus::Result<Battery1Proxy<'static>> {
    Battery1Proxy::builder(connection).path(path)?.build().await
}

pub(super) async fn bind_object_manager(
    connection: &zbus::Connection,
) -> zbus::Result<zbus::fdo::ObjectManagerProxy<'static>> {
    zbus::fdo::ObjectManagerProxy::builder(connection).destination("org.bluez")?.path("/")?.build().await
}

/// Returns live `InterfacesAdded`/`InterfacesRemoved` streams before `GetManagedObjects()`
/// hydration, so changes between subscription and hydration are not missed.
pub(super) async fn subscribe_object_manager(
    object_manager: &zbus::fdo::ObjectManagerProxy<'static>,
) -> zbus::Result<(zbus::fdo::InterfacesAddedStream, zbus::fdo::InterfacesRemovedStream)> {
    let added = object_manager.receive_interfaces_added().await?;
    let removed = object_manager.receive_interfaces_removed().await?;
    Ok((added, removed))
}

pub(super) async fn bind_agent_manager(connection: &zbus::Connection) -> zbus::Result<AgentManager1Proxy<'static>> {
    AgentManager1Proxy::new(connection).await
}
