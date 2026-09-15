//! [`KeyboardController`] owns `obelisk.keyboard` state and write actions. Backlight, lock state,
//! and layout share one `Arc<Mutex<KeyboardState>>` and signal channel (ADR-0034).

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{CompositorKind, detect_compositor, hyprland_signature, unsupported_session_report};

use super::super::scale::{percent_from_raw, raw_from_percent};
use super::backlight::KbdBacklightProxy;
use super::layout::{CompositorLink, HyprlandLink, NiriLink};
use super::locks::{read_led_on, resolve_lock_leds};

/// `obelisk.keyboard`'s combined payload. `backlight_pct` is `-1` without keyboard-backlight
/// hardware. Lock booleans have no sentinel: they default and remain `false` if neither evdev nor
/// sysfs resolves. `active_layout` is the non-nullable empty-string sentinel; index and count are
/// `0` by default (ADR-0034).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct KeyboardState {
    /// Keyboard backlight, `0` to `100`, or `-1` without a backlight device. Check `-1` before
    /// drawing a slider.
    pub backlight_pct: i32,
    /// Caps Lock is on.
    pub caps_lock: bool,
    /// Num Lock is on.
    pub num_lock: bool,
    /// Scroll Lock is on.
    pub scroll_lock: bool,
    /// Layout display name, e.g. `"English (US)"`; empty before the compositor answers.
    pub active_layout: String,
    /// Active layout's 0-based configured-list position, passed to
    /// `keyboard:invoke("switch_layout", index)`.
    pub active_layout_index: u32,
    /// Configured layout count. Below `2`, `switch_layout` has nothing to change and a layout
    /// indicator need not be drawn.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardSignal {
    Changed,
}

/// A live UPower `KbdBacklight` with construction-time `GetMaxBrightness()` or `Unavailable` if
/// UPower exposes none. Brightness step count does not change at runtime.
enum Backlight {
    Live { proxy: KbdBacklightProxy<'static>, max: i32 },
    Unavailable,
}

#[derive(Clone)]
pub struct KeyboardController {
    state: Arc<Mutex<KeyboardState>>,
    backlight: Arc<Backlight>,
    layout: Arc<Option<Box<dyn CompositorLink>>>,
}

impl KeyboardController {
    /// `system_bus` is the Supervisor's system bus, used by `KbdBacklightProxy`
    /// (ADR-0034). A failed `GetMaxBrightness()` yields [`Backlight::Unavailable`]. `leds_root`
    /// (default `/sys/class/leds`) is the test-injected sysfs fallback root. Layout selects one
    /// [`CompositorLink`] via `crate::compositor`'s env probe, or `None` without an implementor.
    pub async fn new(
        system_bus: zbus::Connection,
        leds_root: &Path,
        events_tx: UnboundedSender<KeyboardSignal>,
    ) -> Self {
        let state = Arc::new(Mutex::new(KeyboardState::default()));
        let backlight = resolve_backlight(&system_bus, &state, events_tx.clone()).await;
        resolve_locks(leds_root, &state, events_tx.clone()).await;
        let layout: Option<Box<dyn CompositorLink>> = match detect_compositor() {
            Some(CompositorKind::Hyprland) => match hyprland_signature() {
                Some(signature) => Some(Box::new(HyprlandLink::new(signature, Arc::clone(&state), events_tx.clone()))),
                None => {
                    eprintln!(
                        "keyboard: HYPRLAND_INSTANCE_SIGNATURE is unset or empty; layout reporting disabled for this run"
                    );
                    None
                }
            },
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

    /// `keyboard:set_backlight(pct)`. Logs and returns without keyboard-backlight hardware.
    pub async fn set_backlight(&self, pct: u64) {
        let Backlight::Live { proxy, max } = self.backlight.as_ref() else {
            eprintln!("keyboard: set_backlight called but this machine has no keyboard backlight; ignored");
            return;
        };
        let raw = raw_from_percent(pct, *max);
        if let Err(err) = proxy.set_brightness(raw).await {
            eprintln!("keyboard: SetBrightness failed: {err}");
        }
        // State changes arrive through `BrightnessChanged`, not this call.
    }

    /// `keyboard:switch_layout(index)`. Logs and returns without a supported compositor.
    /// Synchronous because `CompositorLink::switch_layout` is synchronous fire-and-forget.
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

/// Binds `KbdBacklightProxy`, then returns [`Backlight::Live`] or [`Backlight::Unavailable`]. For
/// the live case, subscribe before `GetBrightness()` or a signal in that gap is lost. A failed
/// initial read leaves `backlight_pct` at `-1` until the next `BrightnessChanged`.
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

/// Picks the first `evdev::enumerate()` device whose LEDs include `LED_CAPSL`, leaving it open.
/// Not fake-data unit-tested: enumeration scans real `/dev/input` nodes and was verified live on
/// this machine (ADR-0034).
fn find_keyboard_led_device() -> Option<evdev::Device> {
    evdev::enumerate()
        .find(|(_, device)| device.supported_leds().is_some_and(|leds| leds.contains(evdev::LedCode::LED_CAPSL)))
        .map(|(_, device)| device)
}

/// Uses evdev first (ADR-0034): `EV_LED` carries live changes, queued per open fd from
/// `Device::open`, so it has no UPower-style subscribe-before-read race. Sysfs
/// (`locks::resolve_lock_leds`) is a static read-once fallback; neither source leaves all locks at
/// their logged `false` defaults.
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
                // evdev is live; use sysfs only when it is not.
                return;
            }
            Err(err) => {
                // An un-streamable device still counts as evdev unavailable; use sysfs rather than
                // leaving lock state at `false` forever.
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
}
