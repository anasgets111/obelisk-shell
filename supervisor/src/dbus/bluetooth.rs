//! BlueZ Bluetooth D-Bus controller (`oblisk.bluetooth`, build-steps.md; docs/oblisk-supervisor-
//! services-dbus.md §5; docs/oblisk-idl-api-specs.md §2.6; docs/adr/0030).
//!
//! Mirrors `dbus::network`'s controller/state/signal shape (a controller holding the proxies it
//! needs, a plain accumulator struct pushed as a `StateSnapshot`, write actions `tokio::spawn`ed
//! rather than awaited inline, a per-object signal-forwarder task feeding a tagged enum into a
//! channel `main.rs`'s own `select!` drains) and `dbus::polkit`'s shape for the one hand-written
//! `#[zbus::interface]` this module needs (`Agent1`, mirroring `AuthenticationAgent` almost
//! exactly: export on the `ObjectServer` before the registration call that makes it live).
//!
//! Every proxy here is hand-written against BlueZ's own D-Bus API docs (ADR-0030's "no
//! maintained zbus proxy crate" decision) -- `org.freedesktop.DBus.ObjectManager` is the one
//! exception, reusing `zbus::fdo::ObjectManagerProxy` rather than hand-rolling a duplicate.
//!
//! ponytail: [`BluetoothController::new`] never fails outright, unlike `NetworkController::new`
//! (`zbus::Result<Self>`, propagated with `?` at the call site). NetworkManager is assumed
//! present on every target system (this codebase already depends on it for `oblisk.network`);
//! BlueZ is not -- a machine with no Bluetooth hardware, or one where `bluetoothd` simply isn't
//! running, is a real, unremarkable case that must not take the whole Supervisor down. Every
//! step that can fail (binding the `ObjectManager`, finding an adapter, registering the pairing
//! agent) is logged and degrades to an inert controller instead: `enabled`/`discovering` read as
//! `false`, the device lists stay empty, and every write action logs-and-no-ops. This extends the
//! same "graceful hardware absence" precedent `NetworkController::has_wifi_device` already
//! established for a single missing device, one level further, since here the whole service can
//! be absent.
//!
//! ponytail: `discovered_devices` is clarified once and pushed to a fresh Candidate the same way
//! every other capability's `last_snapshots` entry is -- but between an explicit `start_discovery`
//! clear and the next real event, [`BluetoothSignal::DeviceRegistryChanged`] fully re-derives both
//! device lists from the *entire* tracked registry (mirrors `NetworkController::
//! build_available_networks`'s own "no debounce, no incremental patching" discipline), not just
//! devices newly seen in this discovery session. In practice this means a device this Supervisor
//! already knew about from a previous session can reappear in `discovered_devices` after the
//! list was just cleared, as soon as *any* other device's registry entry changes -- BlueZ's own
//! `Device1` objects persist across discovery sessions and this controller doesn't track `RSSI`
//! to notice "back in range" on its own. Accepted as the simplest faithful reading of ADR-0030's
//! own deferral of these exact mechanics ("an implementation-pass detail, not designed here",
//! mirroring `NetworkState`'s own doc comment) -- a stricter session-scoped list is the upgrade
//! path if this proves visibly wrong on real hardware.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

/// Object path this Supervisor's `org.bluez.Agent1` is exported at on its own unique connection
/// name -- mirrors `dbus::polkit::AGENT_OBJECT_PATH`'s naming convention.
pub const AGENT_OBJECT_PATH: &str = "/org/oblisk/Bluez/Agent1";

// ---------------------------------------------------------------------------------------------
// Hand-written proxies (ADR-0030: no maintained zbus proxy crate for BlueZ).
// ---------------------------------------------------------------------------------------------

#[zbus::proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter1 {
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
    fn discoverable(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn pairable(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device1 {
    #[zbus(name = "Connect")]
    fn connect(&self) -> zbus::Result<()>;

    #[zbus(name = "Disconnect")]
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(name = "Pair")]
    fn pair(&self) -> zbus::Result<()>;

    #[zbus(name = "CancelPairing")]
    fn cancel_pairing(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn icon(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn class(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn trusted(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(interface = "org.bluez.Battery1", default_service = "org.bluez")]
trait Battery1 {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<u8>;
}

#[zbus::proxy(interface = "org.bluez.AgentManager1", default_service = "org.bluez", default_path = "/org/bluez")]
trait AgentManager1 {
    #[zbus(name = "RegisterAgent")]
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;

    #[zbus(name = "RequestDefaultAgent")]
    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// Small convenience wrappers around each proxy's own macro-generated `builder()` -- unlike
/// `dbus::network`'s `bind_*` helpers, these aren't working around a lifetime-elision bug (the
/// macro-generated `builder()` here already lets the caller bind past the `&Connection`
/// argument's own borrow -- verified against vendored `zbus_macros-5.19.0`'s proxy-macro output:
/// `impl<'p> Proxy<'p> { pub fn builder(conn: &Connection) -> Builder<'p, Self> }`, where `'p` is
/// the impl block's own free lifetime parameter, not elided from `conn`'s borrow). They exist
/// purely to keep every per-path `.path(path)?.build().await` call site one line instead of
/// three.
async fn bind_adapter(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<Adapter1Proxy<'static>> {
    Adapter1Proxy::builder(connection).path(path)?.build().await
}

async fn bind_device(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<Device1Proxy<'static>> {
    Device1Proxy::builder(connection).path(path)?.build().await
}

async fn bind_battery(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<Battery1Proxy<'static>> {
    Battery1Proxy::builder(connection).path(path)?.build().await
}

async fn bind_object_manager(connection: &zbus::Connection) -> zbus::Result<zbus::fdo::ObjectManagerProxy<'static>> {
    zbus::fdo::ObjectManagerProxy::builder(connection).destination("org.bluez")?.path("/")?.build().await
}

/// Subscribes to `object_manager`'s `InterfacesAdded`/`InterfacesRemoved` signals, returning both
/// streams already-live rather than a proxy [`spawn_object_manager_forwarder`] would have to
/// subscribe through itself later -- see that function's own doc comment for why the ordering
/// this enables (subscribe, then hydrate via `GetManagedObjects()`) matters.
async fn subscribe_object_manager(
    object_manager: &zbus::fdo::ObjectManagerProxy<'static>,
) -> zbus::Result<(zbus::fdo::InterfacesAddedStream, zbus::fdo::InterfacesRemovedStream)> {
    let added = object_manager.receive_interfaces_added().await?;
    let removed = object_manager.receive_interfaces_removed().await?;
    Ok((added, removed))
}

async fn bind_agent_manager(connection: &zbus::Connection) -> zbus::Result<AgentManager1Proxy<'static>> {
    AgentManager1Proxy::new(connection).await
}

// ---------------------------------------------------------------------------------------------
// Hand-written org.bluez.Agent1 (ADR-0030: Just-Works-only pairing, enforced by our own agent).
// ---------------------------------------------------------------------------------------------

/// `org.bluez.Error.Rejected` as a properly-named D-Bus error reply -- `zbus::fdo::Error::Failed`
/// would carry the wrong error name (`org.freedesktop.DBus.Error.Failed`), so this is a small
/// custom `DBusError`-derived type instead, mirroring the derive macro's own documented example
/// (`zbus_macros::DBusError`'s doc comment) almost verbatim.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// `org.bluez.Agent1`, registered as the sole pairing agent for this session (ADR-0030). Method
/// bodies are verbatim per the ADR: `RequestPinCode`/`RequestPasskey`/`DisplayPinCode` reject
/// (legacy PIN-only devices cannot pair through this controller -- an intentional limitation, not
/// an oversight: `bluetooth:pair(mac)` takes no PIN/passkey argument). `RequestConfirmation`/
/// `DisplayPasskey`/`AuthorizeService`/`RequestAuthorization` auto-accept unconditionally --
/// there is no UI to ask a human, and refusing would silently break `connect()` for already-
/// trusted or SSP-Just-Works devices. `Cancel`/`Release` are no-ops.
struct BluetoothAgent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl BluetoothAgent {
    async fn request_pin_code(&self, _device: OwnedObjectPath) -> Result<String, AgentError> {
        Err(AgentError::Rejected("legacy PIN-only pairing is not supported".to_string()))
    }

    async fn request_passkey(&self, _device: OwnedObjectPath) -> Result<u32, AgentError> {
        Err(AgentError::Rejected("legacy PIN-only pairing is not supported".to_string()))
    }

    async fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) -> Result<(), AgentError> {
        Err(AgentError::Rejected("legacy PIN-only pairing is not supported".to_string()))
    }

    async fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) -> Result<(), AgentError> {
        Ok(())
    }

    async fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}

    async fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        Ok(())
    }

    async fn request_authorization(&self, _device: OwnedObjectPath) -> Result<(), AgentError> {
        Ok(())
    }

    async fn cancel(&self) {}

    async fn release(&self) {}
}

/// Exports [`BluetoothAgent`] on `connection`'s object server, then registers it with capability
/// `"NoInputNoOutput"` (forces Just Works for any SSP-capable peer, ADR-0030) and requests it as
/// the system default -- so a pairing attempt triggered outside our own `pair()` (e.g.
/// `bluetoothctl`) hits this policy too, not BlueZ's undocumented built-in fallback. Every step is
/// logged-and-continue, not `?`-propagated: a machine with no `bluetoothd` running must not take
/// the whole Supervisor down over a pairing agent it doesn't need yet (see the module doc
/// comment). Exports the agent object *before* calling `RegisterAgent`, same ordering
/// `dbus::polkit::register_agent` uses, so a callback arriving right after registration always
/// finds a live object to dispatch to.
async fn register_agent_best_effort(connection: &zbus::Connection) {
    if let Err(err) = connection.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await {
        eprintln!("bluetooth: failed to export the Agent1 object at {AGENT_OBJECT_PATH}: {err}");
        return;
    }
    let agent_manager = match bind_agent_manager(connection).await {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("bluetooth: failed to bind org.bluez.AgentManager1 (bluetoothd not running?): {err}");
            return;
        }
    };
    let path = match ObjectPath::try_from(AGENT_OBJECT_PATH) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("bluetooth: {AGENT_OBJECT_PATH} is not a valid object path: {err}");
            return;
        }
    };
    if let Err(err) = agent_manager.register_agent(&path, "NoInputNoOutput").await {
        eprintln!("bluetooth: RegisterAgent failed: {err}");
        return;
    }
    if let Err(err) = agent_manager.request_default_agent(&path).await {
        eprintln!("bluetooth: RequestDefaultAgent failed: {err}");
    }
}

// ---------------------------------------------------------------------------------------------
// State shape pushed as `oblisk.bluetooth`'s StateSnapshot (docs/oblisk-idl-api-specs.md §2.6).
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ConnectedDevice {
    pub mac: String,
    pub name: String,
    /// `-1` if unsupported/unknown (no `Battery1` interface on this device, or its `Percentage`
    /// property failed to read) -- per the IDL comment, not a sentinel invented here.
    pub battery: i32,
    /// Always `None` this round -- codec query/control is deferred (ADR-0030): it needs a live
    /// PipeWire `Device` proxy this controller has no business owning (that channel is an
    /// `audio`-capability concern, not `bluetooth`'s).
    pub codec: Option<String>,
    pub category: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DiscoveredDevice {
    pub mac: String,
    pub name: String,
    /// Always `false` -- per the IDL comment, every entry in this pool is by definition
    /// unpaired.
    pub paired: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BluetoothState {
    pub enabled: bool,
    pub discovering: bool,
    pub connected_devices: Vec<ConnectedDevice>,
    pub discovered_devices: Vec<DiscoveredDevice>,
}

/// What the signal-forwarder tasks (see [`BluetoothController::new`]) report back to `main.rs`'s
/// own top-level `select!` -- mirrors `dbus::network::NetworkSignal`'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothSignal {
    /// The adapter's own `Powered` or `Discovering` property changed.
    AdapterChanged,
    /// A device was added to or removed from the registry, or one of a tracked device's own
    /// `Connected`/`Paired`/`Name`/`Battery1.Percentage` properties changed.
    DeviceRegistryChanged,
}

/// Failure modes a `bluetooth:*` write action can hit before ever reaching BlueZ itself. Logged
/// via `Display` at the call site, not user-facing -- mirrors `dbus::network::ConnectError`'s
/// shape.
#[derive(Debug)]
enum BluetoothActionError {
    NoAdapter,
    UnknownDevice,
}

impl std::fmt::Display for BluetoothActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter => write!(f, "no Bluetooth adapter is present"),
            Self::UnknownDevice => write!(f, "no device with that MAC address has been observed via ObjectManager"),
        }
    }
}

impl std::error::Error for BluetoothActionError {}

/// Bits 8-12 (Major Device Class) and 2-7 (Minor Device Class) of a BlueZ `Class` property
/// (docs/adr/0030's exact bit layout). Parses `Class` ourselves rather than trusting BlueZ's own
/// `Icon` property, which comes back empty whenever `Class == 0` (common for BLE peripherals
/// before GAP data is read).
fn class_to_category(class: u32) -> &'static str {
    let major = (class >> 8) & 0x1F;
    let minor = (class >> 2) & 0x3F;
    match major {
        0x01 => "computer",
        0x02 => "phone",
        0x04 => match minor {
            0x01 | 0x02 => "headset",
            0x06 => "headphones",
            _ => "generic",
        },
        0x05 => match (minor >> 4) & 0x3 {
            0b01 => "keyboard",
            0b10 => "mouse",
            0b11 => "keyboard",
            _ => "generic",
        },
        _ => "generic",
    }
}

/// `bluetooth:set_enabled(en)`'s `arguments: [en]`. Defined once in `dbus` (shared with
/// `network::parse_bool_arg`) and re-exported here so `bluetooth::parse_bool_arg` keeps working
/// unchanged at every call site.
pub use crate::dbus::parse_bool_arg;

/// `bluetooth:pair(mac)` / `connect(mac)` / `disconnect(mac)` / `forget(mac)`'s `arguments: [mac]`.
pub fn parse_mac_arg(arguments: &[serde_json::Value]) -> Option<String> {
    Some(arguments.first()?.as_str()?.to_string())
}

// ---------------------------------------------------------------------------------------------
// Device registry.
// ---------------------------------------------------------------------------------------------

/// One tracked `Device1` object. `mac` is cached at registration time (read once via `Device1`'s
/// own `Address` property) rather than re-read live on every lookup -- resolving `pair`/`connect`/
/// `disconnect`/`forget`'s `mac` argument back to an object path only needs a synchronous
/// `HashMap` scan this way, with no `.await` (and therefore no held-across-`.await` lock) in the
/// hot path. `forwarder` is this device's own signal-forwarder task (mirrors
/// `dbus::network::spawn_wifi_signal_forwarder`'s shape, one instance per device instead of one
/// for the whole Wi-Fi device) -- aborted on `InterfacesRemoved` so it doesn't keep polling a
/// D-Bus object that no longer exists.
struct DeviceEntry {
    mac: String,
    device: Device1Proxy<'static>,
    battery: Option<Battery1Proxy<'static>>,
    forwarder: JoinHandle<()>,
}

type DeviceRegistry = Arc<Mutex<HashMap<OwnedObjectPath, DeviceEntry>>>;

/// Binds `path` as a `Device1`, caches its `Address`, optionally binds `Battery1` (only if
/// `has_battery`), spawns this device's own signal forwarder, and inserts the resulting entry
/// into `devices`. Used both by [`BluetoothController::new`]'s startup hydration (one call per
/// `Device1`-bearing path `GetManagedObjects` returns) and by the `ObjectManager` forwarder's
/// `InterfacesAdded` handler (one call per newly-added `Device1`-bearing path) -- the exact same
/// registration work either way. Logs and skips (never registers a half-built entry) on any
/// D-Bus failure.
async fn register_device(connection: &zbus::Connection, devices: &DeviceRegistry, path: OwnedObjectPath, has_battery: bool, events: UnboundedSender<BluetoothSignal>) {
    let device = match bind_device(connection, path.clone()).await {
        Ok(device) => device,
        Err(err) => {
            eprintln!("bluetooth: failed to bind device {path}: {err}");
            return;
        }
    };
    let mac = match device.address().await {
        Ok(mac) => mac,
        Err(err) => {
            eprintln!("bluetooth: failed to read Address for device {path}: {err}");
            return;
        }
    };
    let battery = if has_battery {
        match bind_battery(connection, path.clone()).await {
            Ok(battery) => Some(battery),
            Err(err) => {
                eprintln!("bluetooth: failed to bind Battery1 for device {path} ({mac}): {err}");
                None
            }
        }
    } else {
        None
    };
    let forwarder = spawn_device_signal_forwarder(device.clone(), battery.clone(), events);
    // `insert` returns the prior value at this key, if any -- real BlueZ can emit a *second*
    // `InterfacesAdded` for a path this registry already tracks (e.g. `Battery1` attaching to an
    // already-known `Device1` once GATT battery-service discovery finishes after connection).
    // Dropping a `JoinHandle` does not abort its task, so without this the old forwarder would
    // leak forever (an un-abortable task holding a live D-Bus subscription) and every later
    // property change on this device would fire `DeviceRegistryChanged` twice, once from each
    // still-running forwarder (Correctness review).
    let previous = devices.lock().unwrap().insert(path, DeviceEntry { mac, device, battery, forwarder });
    if let Some(previous) = previous {
        previous.forwarder.abort();
    }
}

/// Runs until `device`'s connection drops (or, in the ordinary case, until `main.rs`'s side of
/// `events` is dropped), forwarding `Connected`/`Paired`/`Name` property changes -- and, only if
/// `battery` is `Some`, `Battery1.Percentage` changes -- as [`BluetoothSignal::DeviceRegistryChanged`].
/// One instance per tracked device (mirrors `dbus::network::spawn_wifi_signal_forwarder`'s shape,
/// instantiated per-device instead of once); its `JoinHandle` lives in the device's own
/// [`DeviceEntry`] and is aborted on `InterfacesRemoved`, not left to run its natural course.
///
/// The `Battery1.Percentage` branch is folded into the same `select!` as the other three (rather
/// than spawned as a second task) so this function returns exactly one `JoinHandle` -- the
/// registry entry has room for only one, and a second, un-aborted task per battery-equipped
/// device would leak on `InterfacesRemoved`. `battery`'s absence is modeled as a
/// `std::future::pending()` arm rather than an `Option<Stream>` `if`-guard: it's simpler than
/// unifying a `PropertyStream<bool>` and a `PropertyStream<u8>` behind one type, and a pending
/// future just never wins the race, exactly as if that arm didn't exist.
fn spawn_device_signal_forwarder(device: Device1Proxy<'static>, battery: Option<Battery1Proxy<'static>>, events: UnboundedSender<BluetoothSignal>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut connected_changed = device.receive_connected_changed().await;
        let mut paired_changed = device.receive_paired_changed().await;
        let mut name_changed = device.receive_name_changed().await;
        let mut percentage_changed = match &battery {
            Some(battery) => Some(battery.receive_percentage_changed().await),
            None => None,
        };

        loop {
            tokio::select! {
                Some(_) = connected_changed.next() => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                Some(_) = paired_changed.next() => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                Some(_) = name_changed.next() => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                Some(_) = async {
                    match &mut percentage_changed {
                        Some(stream) => stream.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                else => break,
            }
        }
    })
}

/// Runs until `added`/`removed` end, mutating `devices` directly (registering a fresh
/// [`DeviceEntry`] on `InterfacesAdded`, removing and aborting one on `InterfacesRemoved`) and
/// forwarding [`BluetoothSignal::DeviceRegistryChanged`] after each mutation -- mirrors
/// `dbus::network::spawn_wifi_signal_forwarder`'s shape, but (unlike that task, which only
/// forwards a tag and leaves rebuilding to `main.rs`) this one also owns the registry mutation
/// itself: unlike NetworkManager's fixed Wi-Fi device set, resolving `pair`/`connect`/
/// `disconnect`/`forget`'s `mac` argument needs a registry that's already up to date the moment a
/// command arrives, not just eventually consistent once `main.rs` gets around to reacting to a
/// signal.
///
/// Takes the already-subscribed `added`/`removed` streams rather than the bare
/// `ObjectManagerProxy` (and subscribing internally): the *subscription* must complete before
/// [`BluetoothController::new`]'s own `GetManagedObjects()` hydration call runs, not after this
/// task actually gets scheduled -- otherwise any device added or removed on the bus in the window
/// between `GetManagedObjects()` returning and a subscription completing would be silently and
/// permanently missed, with nothing to ever re-sync the registry afterward (Correctness review).
fn spawn_object_manager_forwarder<A, R>(connection: zbus::Connection, mut added: A, mut removed: R, devices: DeviceRegistry, events: UnboundedSender<BluetoothSignal>)
where
    A: tokio_stream::Stream<Item = zbus::fdo::InterfacesAdded> + Unpin + Send + 'static,
    R: tokio_stream::Stream<Item = zbus::fdo::InterfacesRemoved> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(signal) = added.next() => {
                    let Ok(args) = signal.args() else { continue; };
                    let has_device = args.interfaces_and_properties().keys().any(|k| k.as_str() == "org.bluez.Device1");
                    let has_battery = args.interfaces_and_properties().keys().any(|k| k.as_str() == "org.bluez.Battery1");
                    if has_device {
                        register_device(&connection, &devices, args.object_path().to_owned().into(), has_battery, events.clone()).await;
                        if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                    } else if has_battery {
                        // `InterfacesAdded` reports only interfaces newly present at this exact
                        // signal, not the path's full interface set (org.freedesktop.DBus.
                        // ObjectManager semantics) -- BlueZ commonly emits `Device1` first (on
                        // pair/connect) and a *second*, `Battery1`-only `InterfacesAdded` on the
                        // same path once GATT battery-service discovery finishes afterward.
                        // Re-running `register_device` (with `has_battery: true`) on an
                        // already-tracked path is exactly the case its own doc comment already
                        // describes: it rebinds `Device1` (harmless -- the path already has that
                        // interface), binds `Battery1`, spawns a fresh forwarder that now covers
                        // `Percentage`, and its `insert` aborts the old, battery-less forwarder.
                        let path: OwnedObjectPath = args.object_path().to_owned().into();
                        let already_tracked = devices.lock().unwrap().contains_key(&path);
                        if already_tracked {
                            register_device(&connection, &devices, path, true, events.clone()).await;
                            if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                        }
                        // else: a `Battery1`-only event with no prior `Device1` for this path --
                        // shouldn't normally happen, but there's nothing to attach it to yet, so
                        // skip it, matching resolve_device's own skip-on-unknown-path style.
                    }
                }
                Some(signal) = removed.next() => {
                    let Ok(args) = signal.args() else { continue; };
                    let has_device = args.interfaces().iter().any(|i| i.as_str() == "org.bluez.Device1");
                    if has_device {
                        let path: OwnedObjectPath = args.object_path().to_owned().into();
                        let removed_entry = devices.lock().unwrap().remove(&path);
                        if let Some(entry) = removed_entry {
                            entry.forwarder.abort();
                        }
                        if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                    }
                }
                else => break,
            }
        }
    });
}

/// Runs until `adapter`'s connection drops, forwarding `Powered`/`Discovering` property changes
/// as [`BluetoothSignal::AdapterChanged`] -- mirrors `dbus::network::spawn_wifi_signal_forwarder`'s
/// shape at the adapter level, needed so `bluetooth.enabled`/`bluetooth.discovering` stay correct
/// after any change BlueZ makes on its own (a completed/expired discovery session, or Bluetooth
/// toggled by something other than this controller's own `set_enabled`), not just after this
/// controller's own writes.
fn spawn_adapter_signal_forwarder(adapter: Adapter1Proxy<'static>, events: UnboundedSender<BluetoothSignal>) {
    tokio::spawn(async move {
        let mut powered_changed = adapter.receive_powered_changed().await;
        let mut discovering_changed = adapter.receive_discovering_changed().await;
        loop {
            tokio::select! {
                Some(_) = powered_changed.next() => {
                    if events.send(BluetoothSignal::AdapterChanged).is_err() { break; }
                }
                Some(_) = discovering_changed.next() => {
                    if events.send(BluetoothSignal::AdapterChanged).is_err() { break; }
                }
                else => break,
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Controller.
// ---------------------------------------------------------------------------------------------

/// Holds every proxy `oblisk.bluetooth`'s write actions and state rebuilds need. `Clone`: every
/// field is a cheap `zbus` proxy/`Arc` handle, so a clone can be moved into a `tokio::spawn`ed
/// task for one write action without the caller losing its own handle -- exactly what ADR-0030 /
/// ADR-0029's "write actions get `tokio::spawn`ed rather than awaited inline" needs.
#[derive(Clone)]
pub struct BluetoothController {
    adapter: Option<Adapter1Proxy<'static>>,
    devices: DeviceRegistry,
}

impl BluetoothController {
    /// Binds `org.bluez`'s `ObjectManager`, hydrates the device registry and the first adapter
    /// found via one `GetManagedObjects()` call (ADR-0030: "single adapter, first one found" --
    /// any additional adapter is silently ignored, not an error), spawns every signal-forwarder
    /// task this controller needs (per-device, the adapter's own `Powered`/`Discovering`, and the
    /// `ObjectManager` itself), and registers the Just-Works-only pairing agent -- all at
    /// construction time, not lazily (ADR-0030). `events` is threaded in here rather than
    /// exposed via a `*_signal_source()` getter for `main.rs` to spawn separately (contrast
    /// `dbus::network::NetworkController::wifi_signal_source`): unlike NetworkManager's fixed
    /// Wi-Fi device set, hydrating the device registry itself needs the channel (every hydrated
    /// device gets its own forwarder immediately), so there's no meaningful "construct, then
    /// spawn" split left to preserve.
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<BluetoothSignal>) -> Self {
        let object_manager = match bind_object_manager(&connection).await {
            Ok(object_manager) => Some(object_manager),
            Err(err) => {
                eprintln!("bluetooth: failed to bind org.bluez's ObjectManager (bluetoothd not running?): {err}");
                None
            }
        };

        let devices: DeviceRegistry = Arc::new(Mutex::new(HashMap::new()));
        let mut adapter = None;

        // Subscribed *before* GetManagedObjects() below, not after -- see
        // spawn_object_manager_forwarder's own doc comment for why the ordering matters
        // (Correctness review: a device added/removed on the bus in between would otherwise be
        // silently missed forever).
        let object_manager_streams = match &object_manager {
            Some(object_manager) => match subscribe_object_manager(object_manager).await {
                Ok(streams) => Some(streams),
                Err(err) => {
                    eprintln!("bluetooth: failed to subscribe to ObjectManager signals: {err}");
                    None
                }
            },
            None => None,
        };

        if let Some(object_manager) = &object_manager {
            match object_manager.get_managed_objects().await {
                Ok(objects) => {
                    for (path, interfaces) in objects {
                        let has = |name: &str| interfaces.keys().any(|k| k.as_str() == name);
                        if adapter.is_none() && has("org.bluez.Adapter1") {
                            match bind_adapter(&connection, path.clone()).await {
                                Ok(proxy) => adapter = Some(proxy),
                                Err(err) => eprintln!("bluetooth: failed to bind adapter {path}: {err}"),
                            }
                        }
                        if has("org.bluez.Device1") {
                            register_device(&connection, &devices, path.clone(), has("org.bluez.Battery1"), events.clone()).await;
                        }
                    }
                }
                Err(err) => eprintln!("bluetooth: GetManagedObjects failed: {err}"),
            }
        }

        if let Some(adapter) = adapter.clone() {
            spawn_adapter_signal_forwarder(adapter, events.clone());
        } else {
            eprintln!("bluetooth: no adapter found; bluetooth.enabled/discovering will stay false for this session");
        }
        if let Some((added, removed)) = object_manager_streams {
            spawn_object_manager_forwarder(connection.clone(), added, removed, devices.clone(), events);
        }

        register_agent_best_effort(&connection).await;

        Self { adapter, devices }
    }

    /// Live `Powered` read, `false` (not an error) with no adapter present.
    pub async fn read_enabled(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.powered().await.unwrap_or(false),
            None => false,
        }
    }

    /// Live `Discovering` read, `false` (not an error) with no adapter present.
    pub async fn read_discovering(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.discovering().await.unwrap_or(false),
            None => false,
        }
    }

    /// Full, live re-derivation of both device lists from the entire tracked registry (mirrors
    /// `NetworkController::build_available_networks`'s "no debounce" discipline). `connected_devices`
    /// is every entry that's both `Paired` and `Connected` (§5's "active paired & connected
    /// accessories"); `discovered_devices` is every entry that isn't `Paired` yet. A device whose
    /// `Paired`/`Connected` read fails is treated as neither (silently excluded from both lists) --
    /// consistent with `unwrap_or(false)`'s "an unreadable property reads as absent" discipline
    /// used throughout this module.
    ///
    /// `discovered_devices` is **not scoped to the current discovery session** (see also the
    /// module doc comment's ponytail note). This function re-derives it, in full, from every
    /// tracked-but-unpaired device on *every* call -- not just ones newly seen since the last
    /// `start_discovery()` clear. So a device this Supervisor already knew about from an
    /// unrelated, older session can resurface into `discovered_devices` shortly after a
    /// `start_discovery()` clear, the moment *any* registry event fires for *any* device (e.g. a
    /// different device's `Connected` flip) -- not only when new devices are actually discovered
    /// over the air. Accepted as ADR-0030's simplest faithful reading of the IDL's literal text
    /// ("every entry that isn't paired yet" is satisfied either way); the upgrade path, if this
    /// proves visibly wrong on real hardware, is tracking a "first seen while discovering"
    /// timestamp or set that's cleared/reset on `start_discovery()`, so this list can be scoped to
    /// only the current session's newly-seen devices instead of the entire registry.
    pub async fn build_device_lists(&self) -> (Vec<ConnectedDevice>, Vec<DiscoveredDevice>) {
        let snapshot: Vec<(String, Device1Proxy<'static>, Option<Battery1Proxy<'static>>)> = {
            let guard = self.devices.lock().unwrap();
            guard.values().map(|entry| (entry.mac.clone(), entry.device.clone(), entry.battery.clone())).collect()
        };

        let mut connected = Vec::new();
        let mut discovered = Vec::new();
        for (mac, device, battery) in snapshot {
            let paired = device.paired().await.unwrap_or(false);
            let is_connected = device.connected().await.unwrap_or(false);
            let name = device.name().await.unwrap_or_default();
            if paired && is_connected {
                let class = device.class().await.unwrap_or(0);
                let battery_percent = match &battery {
                    Some(battery) => battery.percentage().await.map(i32::from).unwrap_or(-1),
                    None => -1,
                };
                connected.push(ConnectedDevice { mac, name, battery: battery_percent, codec: None, category: class_to_category(class).to_string() });
            } else if !paired {
                discovered.push(DiscoveredDevice { mac, name, paired: false });
            }
        }
        (connected, discovered)
    }

    /// Synchronous registry scan resolving `mac` to its tracked object path and bound `Device1`
    /// proxy -- no `.await` here on purpose (see [`DeviceEntry`]'s own doc comment), so this can
    /// be called freely from `pair`/`connect`/`disconnect`/`forget` without ever holding the
    /// registry's `std::sync::Mutex` across an await point.
    fn resolve_device(&self, mac: &str) -> Option<(OwnedObjectPath, Device1Proxy<'static>)> {
        let guard = self.devices.lock().unwrap();
        guard.iter().find(|(_, entry)| entry.mac == mac).map(|(path, entry)| (path.clone(), entry.device.clone()))
    }

    /// `bluetooth:set_enabled(en)`: writes `Adapter1.Powered`. No local state flip and no
    /// snapshot push here -- [`spawn_adapter_signal_forwarder`] observes the real property change
    /// and `main.rs` rebuilds/pushes from that, the same "let the real event drive the push"
    /// discipline `dbus::network`'s own `set_wifi_enabled`/`set_ethernet_enabled` already use.
    pub async fn set_enabled(&self, enabled: bool) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: set_enabled({enabled}) failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.set_powered(enabled).await {
            eprintln!("bluetooth: failed to set Powered={enabled}: {err}");
        }
    }

    /// `bluetooth:start_discovery()`'s D-Bus half. The `discovered_devices` clear-and-push
    /// (ADR-0030) happens in `main.rs`, immediately, before this is even spawned -- mirrors
    /// `dbus::network::NetworkController::scan`'s own split between the immediate local flip and
    /// the D-Bus call proper.
    pub async fn start_discovery(&self) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: start_discovery() failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.start_discovery().await {
            eprintln!("bluetooth: StartDiscovery failed: {err}");
        }
    }

    /// `bluetooth:stop_discovery()`: no local state mutation (ADR-0030: the last
    /// `discovered_devices` snapshot stays visible).
    pub async fn stop_discovery(&self) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: stop_discovery() failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.stop_discovery().await {
            eprintln!("bluetooth: StopDiscovery failed: {err}");
        }
    }

    /// `bluetooth:pair(mac)`. An unresolvable `mac` is a domain error (mirrors
    /// `dbus::network::ConnectError::NoWifiDevice`'s shape) -- logged and dropped, never a guessed
    /// object path (ADR-0030).
    pub async fn pair(&self, mac: &str) {
        match self.resolve_device(mac) {
            Some((_, device)) => {
                if let Err(err) = device.pair().await {
                    eprintln!("bluetooth: pair({mac:?}) failed: {err}");
                }
            }
            None => eprintln!("bluetooth: pair({mac:?}) failed: {}", BluetoothActionError::UnknownDevice),
        }
    }

    /// `bluetooth:connect(mac)`.
    pub async fn connect(&self, mac: &str) {
        match self.resolve_device(mac) {
            Some((_, device)) => {
                if let Err(err) = device.connect().await {
                    eprintln!("bluetooth: connect({mac:?}) failed: {err}");
                }
            }
            None => eprintln!("bluetooth: connect({mac:?}) failed: {}", BluetoothActionError::UnknownDevice),
        }
    }

    /// `bluetooth:disconnect(mac)`.
    pub async fn disconnect(&self, mac: &str) {
        match self.resolve_device(mac) {
            Some((_, device)) => {
                if let Err(err) = device.disconnect().await {
                    eprintln!("bluetooth: disconnect({mac:?}) failed: {err}");
                }
            }
            None => eprintln!("bluetooth: disconnect({mac:?}) failed: {}", BluetoothActionError::UnknownDevice),
        }
    }

    /// `bluetooth:forget(mac)`: resolves `mac` to its tracked path and calls
    /// `Adapter1.RemoveDevice(path)` on the bound adapter -- clears paired credentials from disk
    /// (docs/oblisk-supervisor-services-dbus.md §5.1).
    pub async fn forget(&self, mac: &str) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: forget({mac:?}) failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        let Some((path, _)) = self.resolve_device(mac) else {
            eprintln!("bluetooth: forget({mac:?}) failed: {}", BluetoothActionError::UnknownDevice);
            return;
        };
        if let Err(err) = adapter.remove_device(&path).await {
            eprintln!("bluetooth: RemoveDevice({mac:?}) failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    // ---- class_to_category ----

    #[test]
    fn class_to_category_maps_a_real_keyboards_class() {
        assert_eq!(class_to_category(0x002540), "keyboard");
    }

    #[test]
    fn class_to_category_maps_a_real_mouses_class() {
        assert_eq!(class_to_category(0x002580), "mouse");
    }

    #[test]
    fn class_to_category_maps_a_combo_peripheral_to_keyboard() {
        // Major 0x05 (Peripheral), minor top-2-bits `11` (combo keyboard/pointing device).
        let class = (0x05 << 8) | (0b11_0000 << 2);
        assert_eq!(class_to_category(class), "keyboard");
    }

    #[test]
    fn class_to_category_maps_an_unclassified_peripheral_to_generic() {
        // Major 0x05, minor top-2-bits `00` (uncategorized device per the Bluetooth spec).
        assert_eq!(class_to_category(0x05 << 8), "generic");
    }

    #[test]
    fn class_to_category_maps_major_0x01_to_computer() {
        assert_eq!(class_to_category(0x01 << 8), "computer");
    }

    #[test]
    fn class_to_category_maps_major_0x02_to_phone() {
        assert_eq!(class_to_category(0x02 << 8), "phone");
    }

    #[test]
    fn class_to_category_maps_audio_video_minor_0x01_and_0x02_to_headset() {
        assert_eq!(class_to_category((0x04 << 8) | (0x01 << 2)), "headset");
        assert_eq!(class_to_category((0x04 << 8) | (0x02 << 2)), "headset");
    }

    #[test]
    fn class_to_category_maps_audio_video_minor_0x06_to_headphones() {
        assert_eq!(class_to_category((0x04 << 8) | (0x06 << 2)), "headphones");
    }

    #[test]
    fn class_to_category_maps_other_audio_video_minors_to_generic() {
        assert_eq!(class_to_category((0x04 << 8) | (0x03 << 2)), "generic");
    }

    #[test]
    fn class_to_category_maps_an_unmapped_major_to_generic() {
        // Major 0x03 is "LAN/Network Access Point" -- not one of the categories this Supervisor
        // renders an icon for.
        assert_eq!(class_to_category(0x03 << 8), "generic");
    }

    #[test]
    fn class_to_category_ignores_service_class_bits() {
        // A real Bluetooth headset's Class value carries Service Class bits above bit 12 too
        // (e.g. "Audio" = bit 21) -- these must not perturb the Major/Minor extraction.
        let class = 0x24_0404_u32 & 0x00FF_FFFF;
        assert_eq!(class_to_category(class), "headset");
    }

    // ---- arg parsers ----

    #[test]
    fn parse_bool_arg_reads_the_first_argument() {
        assert_eq!(parse_bool_arg(&[serde_json::json!(true)]), Some(true));
        assert_eq!(parse_bool_arg(&[serde_json::json!(false)]), Some(false));
    }

    #[test]
    fn parse_bool_arg_rejects_a_malformed_shape() {
        assert_eq!(parse_bool_arg(&[]), None);
        assert_eq!(parse_bool_arg(&[serde_json::json!("yes")]), None);
    }

    #[test]
    fn parse_mac_arg_reads_the_first_argument() {
        assert_eq!(parse_mac_arg(&[serde_json::json!("00:1A:7D:DA:71:11")]), Some("00:1A:7D:DA:71:11".to_string()));
        assert_eq!(parse_mac_arg(&[]), None);
        assert_eq!(parse_mac_arg(&[serde_json::json!(42)]), None);
    }

    // ---- BluetoothAgent (mirrors dbus::polkit's own p2p-connection test harness) ----

    /// A connected pair of p2p zbus connections, no bus daemon involved -- copied from
    /// `dbus::polkit`'s own test helper of the same name (see its doc comment for why both
    /// builders must be driven concurrently via `try_join!`).
    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder = zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p();
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    async fn agent1_proxy(caller_side: &zbus::Connection) -> zbus::Proxy<'_> {
        zbus::proxy::Builder::new(caller_side)
            .destination("org.oblisk.Supervisor")
            .expect("valid destination bus name")
            .path(AGENT_OBJECT_PATH)
            .expect("valid object path")
            .interface("org.bluez.Agent1")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p proxy to the agent")
    }

    fn dummy_device_path() -> zbus::zvariant::ObjectPath<'static> {
        zbus::zvariant::ObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55").expect("valid object path")
    }

    #[tokio::test]
    async fn request_pin_code_returns_a_properly_named_rejected_error() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        let result: zbus::Result<String> = proxy.call("RequestPinCode", &(dummy_device_path(),)).await;

        match result.expect_err("RequestPinCode must be rejected") {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), "org.bluez.Error.Rejected"),
            other => panic!("expected a MethodError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_passkey_returns_a_properly_named_rejected_error() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        let result: zbus::Result<u32> = proxy.call("RequestPasskey", &(dummy_device_path(),)).await;

        match result.expect_err("RequestPasskey must be rejected") {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), "org.bluez.Error.Rejected"),
            other => panic!("expected a MethodError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_confirmation_auto_accepts() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy.call::<_, _, ()>("RequestConfirmation", &(dummy_device_path(), 123456u32)).await.expect("RequestConfirmation must auto-accept");
    }

    #[tokio::test]
    async fn request_authorization_auto_accepts() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy.call::<_, _, ()>("RequestAuthorization", &(dummy_device_path(),)).await.expect("RequestAuthorization must auto-accept");
    }

    #[tokio::test]
    async fn authorize_service_auto_accepts() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy
            .call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), "0000110b-0000-1000-8000-00805f9b34fb"))
            .await
            .expect("AuthorizeService must auto-accept");
    }

    #[tokio::test]
    async fn cancel_and_release_are_no_ops() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy.call::<_, _, ()>("Cancel", &()).await.expect("Cancel must succeed");
        proxy.call::<_, _, ()>("Release", &()).await.expect("Release must succeed");
    }
}
