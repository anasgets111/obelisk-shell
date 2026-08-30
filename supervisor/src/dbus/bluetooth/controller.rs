//! [`BluetoothController`]: the `oblisk.bluetooth` write-action dispatcher and state owner.
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedObjectPath;

use super::agent::register_agent_best_effort;
use super::proxies::{Adapter1Proxy, Battery1Proxy, Device1Proxy, bind_adapter, bind_object_manager, subscribe_object_manager};
use super::registry::{DeviceRegistry, register_device, spawn_adapter_signal_forwarder, spawn_object_manager_forwarder};
use super::{BluetoothActionError, BluetoothSignal, BluetoothState, ConnectedDevice, DiscoveredDevice, class_to_category};

/// Holds every proxy `oblisk.bluetooth`'s write actions and state rebuilds need. `Clone`:
/// every field is a cheap `zbus` proxy/`Arc` handle, so a clone can be moved into a
/// `tokio::spawn`ed task for one write action without the caller losing its own handle.
#[derive(Clone)]
pub struct BluetoothController {
    adapter: Option<Adapter1Proxy<'static>>,
    devices: DeviceRegistry,
    /// `oblisk.bluetooth`'s own push state (ADR-0037), mutated only by
    /// [`handle_signal`](Self::handle_signal). `Mutex` because the controller is `Clone`;
    /// never held across an await.
    state: Arc<Mutex<BluetoothState>>,
    /// Clone of the signal channel's sender: lets [`clear_discovered`](Self::clear_discovered)
    /// route its immediate clear through the same FIFO as the forwarders' real signals.
    events: UnboundedSender<BluetoothSignal>,
}

impl BluetoothController {
    /// Binds `org.bluez`'s `ObjectManager`, hydrates the device registry and the first adapter
    /// found via one `GetManagedObjects()` call (ADR-0030: "single adapter, first one found"),
    /// spawns every signal-forwarder task this controller needs, and registers the
    /// Just-Works-only pairing agent, all at construction time. `events` is threaded in here
    /// rather than exposed via a getter: hydrating the device registry needs the channel,
    /// since every hydrated device gets its own forwarder immediately.
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

        // Subscribed before GetManagedObjects() below, not after: a device added/removed on
        // the bus in between would otherwise be silently missed forever.
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
            spawn_object_manager_forwarder(connection.clone(), added, removed, devices.clone(), events.clone());
        }

        register_agent_best_effort(&connection).await;

        Self { adapter, devices, state: Arc::new(Mutex::new(BluetoothState::default())), events }
    }

    /// Applies one [`BluetoothSignal`] to the controller-owned [`BluetoothState`] and returns
    /// the updated state for the main loop's one bluetooth arm to push: no debounce, every
    /// relevant event fully re-derives the affected part of the state from scratch.
    pub async fn handle_signal(&self, signal: BluetoothSignal) -> BluetoothState {
        match signal {
            BluetoothSignal::AdapterChanged => {
                let enabled = self.read_enabled().await;
                let discovering = self.read_discovering().await;
                let mut state = self.state.lock().unwrap();
                state.enabled = enabled;
                state.discovering = discovering;
                state.clone()
            }
            BluetoothSignal::DeviceRegistryChanged => {
                let (connected_devices, discovered_devices) = self.build_device_lists().await;
                let mut state = self.state.lock().unwrap();
                state.connected_devices = connected_devices;
                state.discovered_devices = discovered_devices;
                state.clone()
            }
            BluetoothSignal::DiscoveryCleared => {
                let mut state = self.state.lock().unwrap();
                state.discovered_devices = Vec::new();
                state.clone()
            }
        }
    }

    /// The immediate half of `bluetooth:start_discovery()`: queues
    /// [`BluetoothSignal::DiscoveryCleared`] so `discovered_devices` clears on initiation, not
    /// once `StartDiscovery`'s D-Bus round trip completes (docs/adr/0030).
    pub fn clear_discovered(&self) {
        let _ = self.events.send(BluetoothSignal::DiscoveryCleared);
    }

    /// Live `Powered` read, `false` (not an error) with no adapter present.
    async fn read_enabled(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.powered().await.unwrap_or(false),
            None => false,
        }
    }

    /// Live `Discovering` read, `false` (not an error) with no adapter present.
    async fn read_discovering(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.discovering().await.unwrap_or(false),
            None => false,
        }
    }

    /// Full, live re-derivation of both device lists from the entire tracked registry.
    /// `connected_devices` is every entry that's both `Paired` and `Connected` (§5's "active
    /// paired & connected accessories"); `discovered_devices` is every entry that isn't
    /// `Paired` yet. A device whose `Paired`/`Connected` read fails is excluded from both.
    /// `discovered_devices` is not scoped to the current discovery session -- see
    /// `dbus/bluetooth/mod.rs`'s module doc ponytail note.
    async fn build_device_lists(&self) -> (Vec<ConnectedDevice>, Vec<DiscoveredDevice>) {
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

    /// Synchronous registry scan resolving `mac` to its tracked object path and bound
    /// `Device1` proxy -- no `.await` here on purpose, so this can be called freely without
    /// ever holding the registry's `std::sync::Mutex` across an await point.
    fn resolve_device(&self, mac: &str) -> Option<(OwnedObjectPath, Device1Proxy<'static>)> {
        let guard = self.devices.lock().unwrap();
        guard.iter().find(|(_, entry)| entry.mac == mac).map(|(path, entry)| (path.clone(), entry.device.clone()))
    }

    /// `bluetooth:set_enabled(en)`: writes `Adapter1.Powered`. No local state flip and no
    /// snapshot push here -- [`spawn_adapter_signal_forwarder`] observes the real property
    /// change and `main.rs` rebuilds/pushes from that.
    pub async fn set_enabled(&self, enabled: bool) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: set_enabled({enabled}) failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.set_powered(enabled).await {
            eprintln!("bluetooth: failed to set Powered={enabled}: {err}");
        }
    }

    /// `bluetooth:start_discovery()`'s D-Bus half. The `discovered_devices` clear (ADR-0030)
    /// happens via [`clear_discovered`](Self::clear_discovered), immediately, before this is
    /// even spawned.
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

    /// `bluetooth:pair(mac)`. An unresolvable `mac` is a domain error -- logged and dropped,
    /// never a guessed object path (ADR-0030).
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

