//! [`KeyboardController`]: the `oblisk.keyboard` write-action dispatcher and state owner.
//! Backlight and lock state are wired in (ADR-0034); layout joins the same
//! `Arc<Mutex<KeyboardState>>` and shared signal channel in the same shape once built.
//! Split from `hardware::keyboard` -- see `hardware/keyboard/mod.rs` for the module-level doc.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;

use super::super::scale::{percent_from_raw, raw_from_percent};
use super::backlight::KbdBacklightProxy;
use super::layout::{CompositorKind, CompositorLink, HyprlandLink, NiriLink, detect_compositor};
use super::locks::{read_led_on, resolve_lock_leds};

/// `oblisk.keyboard`'s combined payload. `backlight_pct` is `-1` (the same "sentinel, not a
/// fabricated zero" convention `sysinfo::SysinfoState::temp_gpu` already established) when this
/// machine has no keyboard-backlight hardware for UPower to report on. `caps_lock`/`num_lock`/
/// `scroll_lock` have no equivalent sentinel (a bare `bool` has no "unavailable" value) --
/// they default `false` and stay there, logged once, if neither evdev nor sysfs resolves.
/// `active_layout` defaults to an empty string (the IDL declares it a plain, non-nullable
/// `string` -- ADR-0034), `active_layout_index`/`layout_count` default `0`, when neither
/// compositor is detected.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct KeyboardState {
    pub backlight_pct: i32,
    pub caps_lock: bool,
    pub num_lock: bool,
    pub scroll_lock: bool,
    pub active_layout: String,
    pub active_layout_index: u32,
    pub layout_count: u32,
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self { backlight_pct: -1, caps_lock: false, num_lock: false, scroll_lock: false, active_layout: String::new(), active_layout_index: 0, layout_count: 0 }
    }
}

/// One shared signal, `Changed` only (mirrors `SysinfoSignal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardSignal {
    Changed,
}

/// `keyboard:set_backlight(pct)`'s `arguments: [pct]` -- mirrors `idle::parse_register_args`'s
/// single-numeric-argument shape exactly; `pct` is intentionally unclamped here, matching every
/// other numeric `parse_*_args` in this codebase (clamping happens once, in
/// `backlight::raw_from_percent`, the one place that actually needs the bound).
pub fn parse_set_backlight_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `keyboard:switch_layout(index)`'s `arguments: [index]` -- mirrors `parse_set_backlight_args`'s
/// single-numeric-argument shape.
pub fn parse_switch_layout_args(arguments: &[serde_json::Value]) -> Option<usize> {
    arguments.first()?.as_u64().map(|v| v as usize)
}

/// Either a live UPower `KbdBacklight` object with its `GetMaxBrightness()` cached at
/// construction (a keyboard's brightness step count doesn't change at runtime -- the same
/// resolve-once precedent `sysinfo::temp::CoreTempSource` already established), or `Unavailable`
/// if this machine's UPower doesn't expose one at all -- degrade-and-log, not a fabricated value,
/// the same posture every other missing-capability path in this codebase already takes.
enum Backlight {
    Live { proxy: KbdBacklightProxy<'static>, max: i32 },
    Unavailable,
}

/// `Clone` (mirrors `IdleController`) so `main.rs` can hand a cheap `Arc`-backed copy to the
/// `tokio::spawn`ed task `keyboard:set_backlight`'s dispatch arm needs -- `set_backlight` makes a
/// real D-Bus call, so it can't run inline in `main.rs`'s `select!` without blocking every other
/// arm on it.
#[derive(Clone)]
pub struct KeyboardController {
    state: Arc<Mutex<KeyboardState>>,
    backlight: Arc<Backlight>,
    layout: Arc<Option<Box<dyn CompositorLink>>>,
}

impl KeyboardController {
    /// `system_bus` is the Supervisor's already-established `zbus::Connection::system()` (the
    /// same one NetworkManager/BlueZ/polkit/idle-inhibit share) -- `KbdBacklightProxy` rides it
    /// directly, no new connection (ADR-0034, as corrected against this dev machine's real
    /// `UPower.KbdBacklight` introspection). Returns immediately; a failed `GetMaxBrightness()`
    /// (no backlight hardware) degrades to [`Backlight::Unavailable`] rather than failing
    /// construction. See [`resolve_backlight`] for the subscribe-before-read ordering this
    /// delegates to. `leds_root` (real default `/sys/class/leds`) is the lock-state sysfs
    /// fallback's root, injected rather than hardcoded (`docs/oblisk-tdd-test-harness.md`'s
    /// mandate, the same convention `sysinfo`'s `proc_root`/`hwmon_root` already established).
    /// Layout picks one [`CompositorLink`] implementor via [`detect_compositor`]'s env-var probe
    /// -- `None` (degraded, `active_layout` stays the empty-string default) if neither compositor
    /// is detected.
    pub async fn new(system_bus: zbus::Connection, leds_root: &Path, events_tx: UnboundedSender<KeyboardSignal>) -> Self {
        let state = Arc::new(Mutex::new(KeyboardState::default()));
        let backlight = resolve_backlight(&system_bus, &state, events_tx.clone()).await;
        resolve_locks(leds_root, &state, events_tx.clone()).await;
        let layout: Option<Box<dyn CompositorLink>> = match detect_compositor() {
            Some(CompositorKind::Hyprland) => {
                let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").expect("detect_compositor already confirmed this env var is set");
                Some(Box::new(HyprlandLink::new(signature, Arc::clone(&state), events_tx.clone())))
            }
            Some(CompositorKind::Niri) => NiriLink::new(Arc::clone(&state), events_tx.clone()).map(|link| Box::new(link) as Box<dyn CompositorLink>),
            None => {
                eprintln!("keyboard: neither HYPRLAND_INSTANCE_SIGNATURE nor NIRI_SOCKET is set; layout reporting disabled for this run");
                None
            }
        };
        if let Some(link) = &layout {
            eprintln!("keyboard: detected {:?} for layout tracking", link.kind());
        }
        Self { state, backlight: Arc::new(backlight), layout: Arc::new(layout) }
    }

    /// `keyboard:set_backlight(pct)`. A silent no-op (logged once per call, matching
    /// `IdleController::inhibit`'s own malformed/unavailable-path posture) when this machine has
    /// no keyboard backlight -- there is nothing to set.
    pub async fn set_backlight(&self, pct: u64) {
        let Backlight::Live { proxy, max } = self.backlight.as_ref() else {
            eprintln!("keyboard: set_backlight called but this machine has no keyboard backlight; ignored");
            return;
        };
        let raw = raw_from_percent(pct, *max);
        if let Err(err) = proxy.set_brightness(raw).await {
            eprintln!("keyboard: SetBrightness failed: {err}");
        }
        // No optimistic local update: the real state update happens off `BrightnessChanged`,
        // the same "state changes flow through the signal, not the write call" shape every other
        // controller in this codebase already uses (e.g. `BluetoothController::set_enabled`
        // doesn't touch `BluetoothState` itself either).
    }

    /// `keyboard:switch_layout(index)`. A no-op (logged) when neither compositor was detected --
    /// same posture as `set_backlight`'s missing-hardware path. Synchronous, not `async`
    /// (`CompositorLink::switch_layout` itself is synchronous, fire-and-forget -- see the
    /// trait's own doc comment).
    pub fn switch_layout(&self, index: usize) {
        match self.layout.as_ref() {
            Some(link) => link.switch_layout(index),
            None => eprintln!("keyboard: switch_layout called but no compositor was detected; ignored"),
        }
    }

    pub fn snapshot(&self) -> KeyboardState {
        self.state.lock().unwrap().clone()
    }
}

/// Binds `KbdBacklightProxy` and resolves it to [`Backlight::Live`] or [`Backlight::Unavailable`],
/// spawning the `BrightnessChanged` forwarder task for the `Live` case. Order matters (Correctness
/// review): the subscription is established *before* the initial `GetBrightness()` read, not
/// after -- a `BrightnessChanged` UPower emits in the gap between an unsubscribed initial read and
/// the subscription becoming active would otherwise be silently and permanently missed, the same
/// bug class `bluetooth::registry::spawn_object_manager_forwarder`'s own doc comment already
/// documents fixing once for `InterfacesAdded`/`InterfacesRemoved` ("the subscription must
/// complete before `BluetoothController::new`'s own hydration call runs, not after this task
/// actually gets scheduled"). A failed initial read (after a successful subscribe) is best-effort
/// only -- it leaves `backlight_pct` at its `-1` default until the first `BrightnessChanged`
/// arrives, rather than discarding an otherwise-live `SetBrightness` path over one transient read
/// failure (Correctness review): `GetMaxBrightness` already proved this object is real, so a
/// `GetBrightness` hiccup alone shouldn't degrade the whole capability to `Unavailable`.
async fn resolve_backlight(system_bus: &zbus::Connection, state: &Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) -> Backlight {
    let proxy = match KbdBacklightProxy::new(system_bus).await {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("keyboard: failed to bind UPower KbdBacklight; backlight reporting disabled for this run: {err}");
            return Backlight::Unavailable;
        }
    };
    let max = match proxy.get_max_brightness().await {
        Ok(max) if max > 0 => max,
        Ok(_) | Err(_) => {
            eprintln!("keyboard: no usable KbdBacklight found on this system bus; backlight reporting disabled for this run");
            return Backlight::Unavailable;
        }
    };
    let mut changed = match proxy.receive_brightness_changed().await {
        Ok(changed) => changed,
        Err(err) => {
            eprintln!("keyboard: failed to subscribe to BrightnessChanged; backlight reporting disabled for this run: {err}");
            return Backlight::Unavailable;
        }
    };

    match proxy.get_brightness().await {
        Ok(brightness) => state.lock().unwrap().backlight_pct = percent_from_raw(brightness, max),
        Err(err) => eprintln!("keyboard: failed to read initial KbdBacklight brightness; will pick up from the next BrightnessChanged: {err}"),
    }

    let forward_state = Arc::clone(state);
    tokio::spawn(async move {
        while let Some(signal) = changed.next().await {
            let Ok(args) = signal.args() else { continue };
            forward_state.lock().unwrap().backlight_pct = percent_from_raw(args.value, max);
            if events.send(KeyboardSignal::Changed).is_err() {
                break;
            }
        }
    });

    Backlight::Live { proxy, max }
}

/// Picks the keyboard-like evdev device (the first one, in `evdev::enumerate()`'s order, whose
/// LED capability set includes `LED_CAPSL`) and returns it still open, ready for
/// `get_led_state()`/`into_event_stream()`. Not unit-tested against fake data (unlike
/// `locks::resolve_lock_leds`): `evdev::enumerate()` scans real `/dev/input` device nodes, which
/// can't be faked in a `tempfile::tempdir()` the way a sysfs directory tree can -- verified only
/// by live testing on this dev machine (docs/adr/0034), the same category as
/// `idle::notify::connect_wayland_idle`'s real-Wayland-only verification.
fn find_keyboard_led_device() -> Option<evdev::Device> {
    evdev::enumerate().find(|(_, device)| device.supported_leds().is_some_and(|leds| leds.contains(evdev::LedCode::LED_CAPSL))).map(|(_, device)| device)
}

/// Resolves lock-state reporting and writes the initial value into `state`. evdev is primary
/// (ADR-0034, as corrected against this dev machine's live-tested inotify-on-sysfs failure): its
/// `EV_LED` event stream carries every live change with no re-read needed, since it's the
/// kernel's own mechanism for this exact state. No subscribe-before-read race exists here the
/// way one did for UPower's `BrightnessChanged` (Correctness review on backlight): the kernel
/// queues `EV_LED` events per open file descriptor from the moment `Device::open` succeeds, not
/// from a separate, later "subscription" step, so `get_led_state()`'s ioctl and
/// `into_event_stream()`'s later reads race nothing -- any event in between is already sitting
/// in the same fd's kernel-side queue either way. Sysfs (via `locks::resolve_lock_leds`) is a
/// static, read-once fallback when no evdev device is accessible (permission denied, or no
/// LED-capable device found) -- real state, just frozen after this call, not a fabricated
/// value. Neither resolving leaves all three lock fields at their `false` default, logged once.
async fn resolve_locks(leds_root: &Path, state: &Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) {
    if let Some(device) = find_keyboard_led_device() {
        match device.get_led_state() {
            Ok(led_state) => {
                let mut guard = state.lock().unwrap();
                guard.caps_lock = led_state.contains(evdev::LedCode::LED_CAPSL);
                guard.num_lock = led_state.contains(evdev::LedCode::LED_NUML);
                guard.scroll_lock = led_state.contains(evdev::LedCode::LED_SCROLLL);
            }
            Err(err) => eprintln!("keyboard: failed to read initial evdev LED state; will pick up from the first EV_LED event: {err}"),
        }
        match device.into_event_stream() {
            Ok(mut stream) => {
                let forward_state = Arc::clone(state);
                tokio::spawn(async move {
                    loop {
                        let event = match stream.next_event().await {
                            Ok(event) => event,
                            Err(err) => {
                                eprintln!("keyboard: evdev event stream ended; lock-state will no longer update: {err}");
                                break;
                            }
                        };
                        let evdev::EventSummary::Led(_, code, value) = event.destructure() else { continue };
                        let on = value != 0;
                        {
                            let mut guard = forward_state.lock().unwrap();
                            match code {
                                evdev::LedCode::LED_CAPSL => guard.caps_lock = on,
                                evdev::LedCode::LED_NUML => guard.num_lock = on,
                                evdev::LedCode::LED_SCROLLL => guard.scroll_lock = on,
                                _ => continue,
                            }
                        }
                        if events.send(KeyboardSignal::Changed).is_err() {
                            break;
                        }
                    }
                });
                // evdev is fully live (initial state plus a running event forwarder) -- the
                // sysfs fallback below is for when evdev *isn't* live, so it's skipped here.
                return;
            }
            Err(err) => {
                // Correctness + Spec review: this specific failure (a device was found, but its
                // event stream couldn't be opened) used to `return` here too, silently leaving
                // lock state at its `false` default forever -- contradicting both this function's
                // own doc comment and the ADR's "if evdev can't be opened... sysfs is read once"
                // promise. Falling through to the sysfs branch below is the fix: a device that's
                // merely un-streamable is exactly one of the "evdev can't be opened" cases the
                // sysfs fallback exists for.
                eprintln!("keyboard: failed to open an EV_LED event stream; falling back to a one-time sysfs LED read for lock state: {err}");
            }
        }
    } else {
        eprintln!("keyboard: no accessible evdev device with LED_CAPSL capability; falling back to a one-time sysfs LED read for lock state");
    }

    let Some(leds) = resolve_lock_leds(leds_root) else {
        eprintln!("keyboard: no lock-state source available (neither evdev nor sysfs LED nodes); caps/num/scroll_lock will stay false");
        return;
    };
    let mut guard = state.lock().unwrap();
    match read_led_on(&leds.caps) {
        Ok(on) => guard.caps_lock = on,
        Err(err) => eprintln!("keyboard: failed to read the sysfs capslock LED; caps_lock will stay false: {err}"),
    }
    match read_led_on(&leds.num) {
        Ok(on) => guard.num_lock = on,
        Err(err) => eprintln!("keyboard: failed to read the sysfs numlock LED; num_lock will stay false: {err}"),
    }
    match read_led_on(&leds.scroll) {
        Ok(on) => guard.scroll_lock = on,
        Err(err) => eprintln!("keyboard: failed to read the sysfs scrolllock LED; scroll_lock will stay false: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_set_backlight_args_reads_the_first_argument_as_a_percent() {
        let args = vec![serde_json::json!(42)];
        assert_eq!(parse_set_backlight_args(&args), Some(42));
    }

    #[test]
    fn parse_set_backlight_args_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_set_backlight_args(&[]), None);
        let args = vec![serde_json::json!("not a number")];
        assert_eq!(parse_set_backlight_args(&args), None);
    }

    #[test]
    fn keyboard_state_default_is_the_unavailable_sentinel() {
        assert_eq!(
            KeyboardState::default(),
            KeyboardState { backlight_pct: -1, caps_lock: false, num_lock: false, scroll_lock: false, active_layout: String::new(), active_layout_index: 0, layout_count: 0 }
        );
    }

    #[test]
    fn parse_switch_layout_args_reads_the_first_argument_as_an_index() {
        let args = vec![serde_json::json!(1)];
        assert_eq!(parse_switch_layout_args(&args), Some(1));
    }

    #[test]
    fn parse_switch_layout_args_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_switch_layout_args(&[]), None);
        let args = vec![serde_json::json!("not a number")];
        assert_eq!(parse_switch_layout_args(&args), None);
    }
}
