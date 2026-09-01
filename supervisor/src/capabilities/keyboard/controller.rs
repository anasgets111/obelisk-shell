//! [`KeyboardController`]: the `oblisk.keyboard` write-action dispatcher and state owner.
//! Backlight, lock state, and layout are all wired in (ADR-0034), sharing one
//! `Arc<Mutex<KeyboardState>>` and one signal channel. Split from `hardware::keyboard` --
//! see `hardware/keyboard/mod.rs` for the module-level doc.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;

use crate::compositor::{CompositorKind, detect_compositor, unsupported_session_report};

use super::super::scale::{percent_from_raw, raw_from_percent};
use super::backlight::KbdBacklightProxy;
use super::layout::{CompositorLink, HyprlandLink, NiriLink};
use super::locks::{read_led_on, resolve_lock_leds};

/// `oblisk.keyboard`'s combined payload. `backlight_pct` is `-1` when this machine has no
/// keyboard-backlight hardware. `caps_lock`/`num_lock`/`scroll_lock` have no sentinel (bare
/// `bool`) -- they default `false` and stay there, logged once, if neither evdev nor sysfs
/// resolves. `active_layout` defaults to an empty string (IDL declares it non-nullable,
/// ADR-0034), `active_layout_index`/`layout_count` default `0`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
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
        Self {
            backlight_pct: -1,
            caps_lock: false,
            num_lock: false,
            scroll_lock: false,
            active_layout: String::new(),
            active_layout_index: 0,
            layout_count: 0,
        }
    }
}

/// One shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardSignal {
    Changed,
}

/// `keyboard:set_backlight(pct)`'s `arguments: [pct]`. `pct` is intentionally unclamped here
/// -- clamping happens once, in `backlight::raw_from_percent`.
pub fn parse_set_backlight_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `keyboard:switch_layout(index)`'s `arguments: [index]`.
pub fn parse_switch_layout_args(arguments: &[serde_json::Value]) -> Option<usize> {
    arguments.first()?.as_u64().map(|v| v as usize)
}

/// Either a live UPower `KbdBacklight` object with its `GetMaxBrightness()` cached at
/// construction (a keyboard's brightness step count doesn't change at runtime), or
/// `Unavailable` if this machine's UPower doesn't expose one at all.
enum Backlight {
    Live { proxy: KbdBacklightProxy<'static>, max: i32 },
    Unavailable,
}

/// `Clone` so `main.rs` can hand a cheap `Arc`-backed copy to the `tokio::spawn`ed task
/// `keyboard:set_backlight`'s dispatch arm needs, since it makes a real D-Bus call.
#[derive(Clone)]
pub struct KeyboardController {
    state: Arc<Mutex<KeyboardState>>,
    backlight: Arc<Backlight>,
    layout: Arc<Option<Box<dyn CompositorLink>>>,
}

impl KeyboardController {
    /// `system_bus` is the Supervisor's already-established `zbus::Connection::system()`;
    /// `KbdBacklightProxy` rides it directly (ADR-0034). Returns immediately; a failed
    /// `GetMaxBrightness()` degrades to [`Backlight::Unavailable`] rather than failing
    /// construction. `leds_root` (real default `/sys/class/leds`) is the lock-state sysfs
    /// fallback's root, injected for testability. Layout picks one [`CompositorLink`] via
    /// `crate::compositor`'s env-var probe -- `None` for a session running something with no
    /// implementor.
    pub async fn new(
        system_bus: zbus::Connection,
        leds_root: &Path,
        events_tx: UnboundedSender<KeyboardSignal>,
    ) -> Self {
        let state = Arc::new(Mutex::new(KeyboardState::default()));
        let backlight = resolve_backlight(&system_bus, &state, events_tx.clone()).await;
        resolve_locks(leds_root, &state, events_tx.clone()).await;
        let layout: Option<Box<dyn CompositorLink>> = match detect_compositor() {
            Some(CompositorKind::Hyprland) => {
                let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
                    .expect("detect_compositor already confirmed this env var is set");
                Some(Box::new(HyprlandLink::new(signature, Arc::clone(&state), events_tx.clone())))
            }
            Some(CompositorKind::Niri) => NiriLink::new(Arc::clone(&state), events_tx.clone())
                .map(|link| Box::new(link) as Box<dyn CompositorLink>),
            None => {
                eprintln!("keyboard: {}; layout reporting disabled for this run", unsupported_session_report());
                None
            }
        };
        if let Some(link) = &layout {
            eprintln!("keyboard: detected {:?} for layout tracking", link.kind());
        }
        Self { state, backlight: Arc::new(backlight), layout: Arc::new(layout) }
    }

    /// `keyboard:set_backlight(pct)`. A silent no-op (logged once) when this machine has no
    /// keyboard backlight.
    pub async fn set_backlight(&self, pct: u64) {
        let Backlight::Live { proxy, max } = self.backlight.as_ref() else {
            eprintln!("keyboard: set_backlight called but this machine has no keyboard backlight; ignored");
            return;
        };
        let raw = raw_from_percent(pct, *max);
        if let Err(err) = proxy.set_brightness(raw).await {
            eprintln!("keyboard: SetBrightness failed: {err}");
        }
        // No optimistic local update: state changes flow through `BrightnessChanged`, off
        // this call.
    }

    /// `keyboard:switch_layout(index)`. A no-op (logged) when no supported compositor was
    /// detected. Synchronous, not `async`: `CompositorLink::switch_layout` itself is
    /// synchronous, fire-and-forget.
    pub fn switch_layout(&self, index: usize) {
        match self.layout.as_ref() {
            Some(link) => link.switch_layout(index),
            None => eprintln!("keyboard: switch_layout called but no supported compositor was detected; ignored"),
        }
    }

    pub fn snapshot(&self) -> KeyboardState {
        self.state.lock().unwrap().clone()
    }
}

/// Binds `KbdBacklightProxy` and resolves it to [`Backlight::Live`] or
/// [`Backlight::Unavailable`], spawning the `BrightnessChanged` forwarder task for the `Live`
/// case. The subscription is established before the initial `GetBrightness()` read, not after
/// -- a `BrightnessChanged` emitted in that gap would otherwise be silently and permanently
/// missed. A failed initial read is best-effort only: `backlight_pct` stays at its `-1` default
/// until the first `BrightnessChanged` arrives, rather than degrading the whole capability.
async fn resolve_backlight(
    system_bus: &zbus::Connection,
    state: &Arc<Mutex<KeyboardState>>,
    events: UnboundedSender<KeyboardSignal>,
) -> Backlight {
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
            eprintln!(
                "keyboard: no usable KbdBacklight found on this system bus; backlight reporting disabled for this run"
            );
            return Backlight::Unavailable;
        }
    };
    let mut changed = match proxy.receive_brightness_changed().await {
        Ok(changed) => changed,
        Err(err) => {
            eprintln!(
                "keyboard: failed to subscribe to BrightnessChanged; backlight reporting disabled for this run: {err}"
            );
            return Backlight::Unavailable;
        }
    };

    match proxy.get_brightness().await {
        Ok(brightness) => state.lock().unwrap().backlight_pct = percent_from_raw(brightness, max),
        Err(err) => eprintln!(
            "keyboard: failed to read initial KbdBacklight brightness; will pick up from the next BrightnessChanged: {err}"
        ),
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

/// Picks the keyboard-like evdev device (the first one, in `evdev::enumerate()`'s order,
/// whose LED capability set includes `LED_CAPSL`) and returns it still open. Not unit-tested
/// against fake data: `evdev::enumerate()` scans real `/dev/input` device nodes, verified only
/// by live testing on this dev machine (docs/adr/0034).
fn find_keyboard_led_device() -> Option<evdev::Device> {
    evdev::enumerate()
        .find(|(_, device)| device.supported_leds().is_some_and(|leds| leds.contains(evdev::LedCode::LED_CAPSL)))
        .map(|(_, device)| device)
}

/// Resolves lock-state reporting and writes the initial value into `state`. evdev is primary
/// (ADR-0034): its `EV_LED` event stream carries every live change with no re-read needed, and
/// the kernel queues `EV_LED` events per open fd from the moment `Device::open` succeeds, so
/// there's no subscribe-before-read race like UPower's `BrightnessChanged`. Sysfs
/// (`locks::resolve_lock_leds`) is a static, read-once fallback when evdev isn't accessible;
/// neither resolving leaves all three lock fields at their `false` default, logged once.
async fn resolve_locks(leds_root: &Path, state: &Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) {
    if let Some(device) = find_keyboard_led_device() {
        match device.get_led_state() {
            Ok(led_state) => {
                let mut guard = state.lock().unwrap();
                guard.caps_lock = led_state.contains(evdev::LedCode::LED_CAPSL);
                guard.num_lock = led_state.contains(evdev::LedCode::LED_NUML);
                guard.scroll_lock = led_state.contains(evdev::LedCode::LED_SCROLLL);
            }
            Err(err) => eprintln!(
                "keyboard: failed to read initial evdev LED state; will pick up from the first EV_LED event: {err}"
            ),
        }
        match device.into_event_stream() {
            Ok(mut stream) => {
                let forward_state = Arc::clone(state);
                tokio::spawn(async move {
                    loop {
                        let event = match stream.next_event().await {
                            Ok(event) => event,
                            Err(err) => {
                                eprintln!(
                                    "keyboard: evdev event stream ended; lock-state will no longer update: {err}"
                                );
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
                // evdev is fully live now; the sysfs fallback below only applies when it isn't.
                return;
            }
            Err(err) => {
                // A device that's merely un-streamable still counts as "evdev can't be opened"
                // -- fall through to the sysfs branch instead of leaving lock state at `false` forever.
                eprintln!(
                    "keyboard: failed to open an EV_LED event stream; falling back to a one-time sysfs LED read for lock state: {err}"
                );
            }
        }
    } else {
        eprintln!(
            "keyboard: no accessible evdev device with LED_CAPSL capability; falling back to a one-time sysfs LED read for lock state"
        );
    }

    let Some(leds) = resolve_lock_leds(leds_root) else {
        eprintln!(
            "keyboard: no lock-state source available (neither evdev nor sysfs LED nodes); caps/num/scroll_lock will stay false"
        );
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
            KeyboardState {
                backlight_pct: -1,
                caps_lock: false,
                num_lock: false,
                scroll_lock: false,
                active_layout: String::new(),
                active_layout_index: 0,
                layout_count: 0
            }
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
