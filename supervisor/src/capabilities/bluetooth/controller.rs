//! [`BluetoothController`]: the `obelisk.bluetooth` write-action dispatcher and state owner.
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedObjectPath;

use super::agent::{self, PromptSlot, register_agent_best_effort};
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
    /// MAC to the action this Supervisor is running for it, drawn as each device's `busy`.
    busy: Arc<Mutex<HashMap<String, &'static str>>>,
    /// The pairing prompt, shared with the agent that fills it.
    prompts: PromptSlot,
    /// Signal sender used by [`clear_discovered`](Self::clear_discovered) to share the forwarders'
    /// FIFO.
    events: UnboundedSender<BluetoothSignal>,
}

impl BluetoothController {
    /// Binds `org.bluez`'s `ObjectManager`, hydrates devices and the first adapter from one
    /// `GetManagedObjects()` call (ADR-0030), starts signal forwarders, and registers the
    /// pairing agent. `events` is passed in because hydration starts each device forwarder.
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

        let prompts = PromptSlot::default();
        register_agent_best_effort(&connection, prompts.clone(), devices.clone(), events.clone()).await;

        let controller = Self {
            adapter,
            devices,
            state: Arc::new(Mutex::new(BluetoothState::default())),
            busy: Arc::default(),
            prompts,
            events,
        };
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
                let discoverable = self.read_discoverable().await;
                let mut state = self.state.lock().unwrap();
                state.available = self.adapter.is_some();
                state.enabled = enabled;
                state.discovering = discovering;
                state.discoverable = discoverable;
                state.clone()
            }
            BluetoothSignal::DeviceRegistryChanged => {
                let (connected_devices, paired_devices, discovered_devices) = self.build_device_lists().await;
                self.clear_finished_display(&connected_devices, &paired_devices);
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
            BluetoothSignal::PairingChanged => {
                let request = self.prompts.lock().unwrap().as_ref().map(|prompt| prompt.request.clone());
                let mut state = self.state.lock().unwrap();
                state.pairing_request = request;
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

    /// Live `Discoverable` value; `false` without an adapter is not an error.
    async fn read_discoverable(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.discoverable().await.unwrap_or(false),
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

        let running = self.busy.lock().unwrap().clone();
        let mut connected = Vec::new();
        let mut paired_only = Vec::new();
        let mut discovered = Vec::new();
        for (mac, device, battery) in snapshot {
            let paired = device.paired().await.unwrap_or(false);
            let is_connected = device.connected().await.unwrap_or(false);
            let name = device.name().await.unwrap_or_default();
            let busy = running.get(&mac).map(|action| action.to_string());
            if !paired {
                let blocked = device.blocked().await.unwrap_or(false);
                discovered.push(DiscoveredDevice { mac, name, paired: false, blocked, busy });
                continue;
            }
            let category = class_to_category(device.class().await.unwrap_or(0)).to_string();
            if !is_connected {
                let blocked = device.blocked().await.unwrap_or(false);
                paired_only.push(PairedDevice { mac, name, category, blocked, busy });
                continue;
            }
            let battery_percent = match &battery {
                Some(battery) => battery.percentage().await.map(i32::from).unwrap_or(-1),
                None => -1,
            };
            connected.push(ConnectedDevice { mac, name, battery: battery_percent, codec: None, category, busy });
        }
        (connected, paired_only, discovered)
    }

    /// Marks `mac` as running `action` until the guard drops, pushing a rebuild on both edges so
    /// the row spins on the click. A later action replaces the label, which is how `pair` hands the
    /// row to its `connect`; the earlier guard then finds its label gone and leaves it.
    fn mark_busy(&self, mac: &str, action: &'static str) -> BusyGuard<'_> {
        self.busy.lock().unwrap().insert(mac.to_string(), action);
        let _ = self.events.send(BluetoothSignal::DeviceRegistryChanged);
        BusyGuard { controller: self, mac: mac.to_string(), action }
    }

    /// Synchronously resolves `mac` to its tracked path and `Device1` proxy. No `.await`, so the
    /// registry mutex is never held across an await.
    fn resolve_device(&self, mac: &str) -> Option<(OwnedObjectPath, Device1Proxy<'static>)> {
        let guard = self.devices.lock().unwrap();
        guard.iter().find(|(_, entry)| entry.mac == mac).map(|(path, entry)| (path.clone(), entry.device.clone()))
    }

    /// `bluetooth:answer_pairing(accept)`: answers the prompt on screen. `false` on a code display
    /// only takes it down.
    pub fn answer_pairing(&self, accept: bool) {
        if agent::answer(&self.prompts, accept) {
            let _ = self.events.send(BluetoothSignal::PairingChanged);
        }
    }

    /// Takes down a code display once its device is paired. BlueZ ends a successful passkey entry
    /// without calling `Cancel`, so nothing else would. Checked and taken under one lock, so a
    /// request that replaced the display in between is not the one removed.
    fn clear_finished_display(&self, connected: &[ConnectedDevice], paired: &[PairedDevice]) {
        let finished = {
            let mut slot = self.prompts.lock().unwrap();
            let done = slot.as_ref().is_some_and(|prompt| {
                prompt.request.kind == "display"
                    && connected
                        .iter()
                        .map(|d| &d.mac)
                        .chain(paired.iter().map(|d| &d.mac))
                        .any(|mac| *mac == prompt.request.mac)
            });
            if done { slot.take() } else { None }
        };
        if finished.is_some() {
            let _ = self.events.send(BluetoothSignal::PairingChanged);
        }
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

    /// `bluetooth:set_discoverable(on)`: writes `Adapter1.Discoverable`. The adapter forwarder
    /// observes the change, including BlueZ's own switch-off at `DiscoverableTimeout`.
    pub async fn set_discoverable(&self, on: bool) {
        let Some(adapter) = &self.adapter else {
            eprintln!("bluetooth: set_discoverable({on}) failed: {}", BluetoothActionError::NoAdapter);
            return;
        };
        if let Err(err) = adapter.set_discoverable(on).await {
            eprintln!("bluetooth: failed to set Discoverable={on}: {err}");
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
        let _busy = self.mark_busy(mac, "pairing");
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
        let _busy = self.mark_busy(mac, "connecting");
        if let Err(err) = device.set_trusted(true).await {
            eprintln!("bluetooth: failed to set Trusted on {mac:?}: {err}");
        }
        if let Err(err) = device.connect().await {
            eprintln!("bluetooth: connect({mac:?}) failed: {err}");
        }
    }

    /// `bluetooth:disconnect(mac)`.
    pub async fn disconnect(&self, mac: &str) {
        let Some((_, device)) = self.resolve_device(mac) else {
            eprintln!("bluetooth: disconnect({mac:?}) failed: {}", BluetoothActionError::UnknownDevice);
            return;
        };
        let _busy = self.mark_busy(mac, "disconnecting");
        if let Err(err) = device.disconnect().await {
            eprintln!("bluetooth: disconnect({mac:?}) failed: {err}");
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

/// Clears its action from the busy map when the call ends, on every return path and on
/// cancellation.
struct BusyGuard<'a> {
    controller: &'a BluetoothController,
    mac: String,
    action: &'static str,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        let mut busy = self.controller.busy.lock().unwrap();
        if busy.get(&self.mac) == Some(&self.action) {
            busy.remove(&self.mac);
        }
        drop(busy);
        let _ = self.controller.events.send(BluetoothSignal::DeviceRegistryChanged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> (BluetoothController, tokio::sync::mpsc::UnboundedReceiver<BluetoothSignal>) {
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let controller = BluetoothController {
            adapter: None,
            devices: Arc::default(),
            state: Arc::default(),
            busy: Arc::default(),
            prompts: Arc::default(),
            events,
        };
        (controller, receiver)
    }

    #[test]
    fn a_pair_hands_its_row_to_the_connect_that_follows_it() {
        let (controller, mut receiver) = controller();
        let running = |c: &BluetoothController| c.busy.lock().unwrap().get("AA").copied();

        let pairing = controller.mark_busy("AA", "pairing");
        assert_eq!(running(&controller), Some("pairing"));
        let connecting = controller.mark_busy("AA", "connecting");
        assert_eq!(running(&controller), Some("connecting"));

        drop(connecting);
        assert_eq!(running(&controller), None, "the connect ending clears the row");
        drop(pairing);
        assert_eq!(running(&controller), None, "the pair's guard does not resurrect or clear a newer label");

        let pushes = std::iter::from_fn(|| receiver.try_recv().ok()).count();
        assert_eq!(pushes, 4, "every edge rebuilds, so the spinner follows each one");
    }
}
