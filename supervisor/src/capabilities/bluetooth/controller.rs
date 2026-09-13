//! [`BluetoothController`]: the `obelisk.bluetooth` write-action dispatcher and state owner.
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedObjectPath;

use super::agent::register_agent_best_effort;
use super::proxies::{
    Adapter1Proxy, Battery1Proxy, Device1Proxy, bind_adapter, bind_object_manager, subscribe_object_manager,
};
use super::registry::{
    DeviceRegistry, register_device, spawn_adapter_signal_forwarder, spawn_object_manager_forwarder,
};
use super::{
    BluetoothActionError, BluetoothSignal, BluetoothState, ConnectedDevice, DiscoveredDevice, PairedDevice,
    class_to_category,
};

/// Proxies needed by `obelisk.bluetooth` writes and state rebuilds. Every field is a cheap zbus
/// proxy or `Arc`, so a clone can move into a `tokio::spawn` task.
#[derive(Clone)]
pub struct BluetoothController {
    adapter: Option<Adapter1Proxy<'static>>,
    devices: DeviceRegistry,
    /// Push state (ADR-0037), mutated only by [`handle_signal`](Self::handle_signal). The mutex is
    /// never held across an await.
    state: Arc<Mutex<BluetoothState>>,
    /// Signal sender used by [`clear_discovered`](Self::clear_discovered) to share the forwarders'
    /// FIFO.
    events: UnboundedSender<BluetoothSignal>,
}

impl BluetoothController {
    /// Binds `org.bluez`'s `ObjectManager`, hydrates devices and the first adapter from one
    /// `GetManagedObjects()` call (ADR-0030), starts signal forwarders, and registers the
    /// Just-Works agent. `events` is passed in because hydration starts each device forwarder.
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

        // Subscribe before GetManagedObjects(): a device changing between them would be missed.
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
                            register_device(
                                &connection,
                                &devices,
                                path.clone(),
                                has("org.bluez.Battery1"),
                                events.clone(),
                            )
                            .await;
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

        let controller = Self { adapter, devices, state: Arc::new(Mutex::new(BluetoothState::default())), events };
        // Hydrate before returning, because the forwarders above are already queueing and zbus
        // yields a cached property's current value as its stream's first item: one of them writes
        // the first snapshot a config ever sees. Each signal re-derives only its own half, so
        // whichever won that race published the other half's `Default`. A `DeviceRegistryChanged`
        // arriving first announced a powered adapter as `enabled = false`, and the `AdapterChanged`
        // behind it then read as the user switching Bluetooth on -- `modules/osd/service.lua`
        // showed "bluetooth on" at every shell start. `network` is safe by accident: every signal
        // its forwarders can emit is a full re-derive.
        controller.handle_signal(BluetoothSignal::AdapterChanged).await;
        controller.handle_signal(BluetoothSignal::DeviceRegistryChanged).await;
        controller
    }

    /// Applies one [`BluetoothSignal`] to [`BluetoothState`] for the main loop to push. No
    /// debounce: each relevant event fully re-derives its affected state.
    pub async fn handle_signal(&self, signal: BluetoothSignal) -> BluetoothState {
        match signal {
            BluetoothSignal::AdapterChanged => {
                let enabled = self.read_enabled().await;
                let discovering = self.read_discovering().await;
                let mut state = self.state.lock().unwrap();
                state.available = self.adapter.is_some();
                state.enabled = enabled;
                state.discovering = discovering;
                state.clone()
            }
            BluetoothSignal::DeviceRegistryChanged => {
                let (connected_devices, paired_devices, discovered_devices) = self.build_device_lists().await;
                let mut state = self.state.lock().unwrap();
                state.connected_devices = connected_devices;
                state.paired_devices = paired_devices;
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

    /// Queues [`BluetoothSignal::DiscoveryCleared`] immediately when
    /// `bluetooth:start_discovery()` begins, before `StartDiscovery`'s D-Bus round trip (ADR-0030).
    pub fn clear_discovered(&self) {
        let _ = self.events.send(BluetoothSignal::DiscoveryCleared);
    }

    /// Live `Powered` value; `false` without an adapter is not an error.
    async fn read_enabled(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.powered().await.unwrap_or(false),
            None => false,
        }
    }

    /// Live `Discovering` value; `false` without an adapter is not an error.
    async fn read_discovering(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.discovering().await.unwrap_or(false),
            None => false,
        }
    }

    /// Re-derives the three lists from the full registry: paired and connected, paired only, and
    /// unpaired. A failed `Paired` or `Connected` read counts as `false`. Discovery is not
    /// session-scoped; see `dbus/bluetooth/mod.rs`'s ponytail note.
    async fn build_device_lists(&self) -> (Vec<ConnectedDevice>, Vec<PairedDevice>, Vec<DiscoveredDevice>) {
        let snapshot: Vec<(String, Device1Proxy<'static>, Option<Battery1Proxy<'static>>)> = {
            let guard = self.devices.lock().unwrap();
            guard.values().map(|entry| (entry.mac.clone(), entry.device.clone(), entry.battery.clone())).collect()
        };

        let mut connected = Vec::new();
        let mut paired_only = Vec::new();
        let mut discovered = Vec::new();
        for (mac, device, battery) in snapshot {
            let paired = device.paired().await.unwrap_or(false);
            let is_connected = device.connected().await.unwrap_or(false);
            let name = device.name().await.unwrap_or_default();
            if !paired {
                discovered.push(DiscoveredDevice { mac, name, paired: false });
                continue;
            }
            let category = class_to_category(device.class().await.unwrap_or(0)).to_string();
            if !is_connected {
                paired_only.push(PairedDevice { mac, name, category });
                continue;
            }
            let battery_percent = match &battery {
                Some(battery) => battery.percentage().await.map(i32::from).unwrap_or(-1),
                None => -1,
            };
            connected.push(ConnectedDevice { mac, name, battery: battery_percent, codec: None, category });
        }
        (connected, paired_only, discovered)
    }

    /// Synchronously resolves `mac` to its tracked path and `Device1` proxy. No `.await`, so the
    /// registry mutex is never held across an await.
    fn resolve_device(&self, mac: &str) -> Option<(OwnedObjectPath, Device1Proxy<'static>)> {
        let guard = self.devices.lock().unwrap();
        guard.iter().find(|(_, entry)| entry.mac == mac).map(|(path, entry)| (path.clone(), entry.device.clone()))
    }

    /// `bluetooth:set_enabled(en)`: writes `Adapter1.Powered`. The adapter signal forwarder
    /// observes the real change; `main.rs` then rebuilds and pushes state.
    pub async fn set_enabled(&self, enabled: bool) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: set_enabled({enabled}) failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.set_powered(enabled).await {
            eprintln!("bluetooth: failed to set Powered={enabled}: {err}");
        }
    }

    /// D-Bus half of `bluetooth:start_discovery()`. [`clear_discovered`](Self::clear_discovered)
    /// clears `discovered_devices` before this task is spawned (ADR-0030).
    pub async fn start_discovery(&self) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: start_discovery() failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.start_discovery().await {
            eprintln!("bluetooth: StartDiscovery failed: {err}");
        }
    }

    /// `bluetooth:stop_discovery()`: no local mutation; the last `discovered_devices` snapshot
    /// stays visible (ADR-0030).
    pub async fn stop_discovery(&self) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: stop_discovery() failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.stop_discovery().await {
            eprintln!("bluetooth: StopDiscovery failed: {err}");
        }
    }

    /// `bluetooth:pair(mac)`, then [`connect`](Self::connect), as `BluetoothService.qml`'s
    /// `connectAfterPairAddress` does. `Pair` returns when pairing ends, so no `Paired` watch is
    /// needed. An unresolvable `mac` is logged and dropped, never guessed (ADR-0030).
    pub async fn pair(&self, mac: &str) {
        let Some((_, device)) = self.resolve_device(mac) else {
            eprintln!("bluetooth: pair({mac:?}) failed: {}", BluetoothActionError::UnknownDevice);
            return;
        };
        if let Err(err) = device.pair().await {
            eprintln!("bluetooth: pair({mac:?}) failed: {err}");
            return;
        }
        self.connect(mac).await;
    }

    /// `bluetooth:connect(mac)`. Sets `Trusted` first, as `BluetoothService.qml` does, so the device
    /// can reconnect on its own later without the agent authorizing each service.
    pub async fn connect(&self, mac: &str) {
        let Some((_, device)) = self.resolve_device(mac) else {
            eprintln!("bluetooth: connect({mac:?}) failed: {}", BluetoothActionError::UnknownDevice);
            return;
        };
        if let Err(err) = device.set_trusted(true).await {
            eprintln!("bluetooth: failed to set Trusted on {mac:?}: {err}");
        }
        if let Err(err) = device.connect().await {
            eprintln!("bluetooth: connect({mac:?}) failed: {err}");
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

    /// `bluetooth:forget(mac)`: resolves `mac` and calls `Adapter1.RemoveDevice(path)`, clearing
    /// paired credentials from disk (docs/services.md §5.1).
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
