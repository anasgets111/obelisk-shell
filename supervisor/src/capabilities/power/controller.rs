//! [`PowerController`] owns `obelisk.power` and its write action.
//! See `power/mod.rs` for why the payload has four optional fields.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{Stream, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedValue;

/// `obelisk.power`'s full payload. Optional fields are omitted from JSON, so unavailable
/// host data reads as Lua `nil`; see `power/mod.rs` for the four-field split.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PowerState {
    /// Active platform profile, e.g. `"balanced"`, set by `:invoke("set_profile", p)`; `nil` without
    /// power-profiles-daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    /// Profiles in daemon order, e.g. `{"performance", "balanced", "power-saver"}`; `nil` when
    /// power-profiles-daemon is absent. Drive selectors from this list because machines differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    /// Running on battery rather than mains, from UPower; `nil` without UPower. This is the mains
    /// question; charge direction is `obelisk.battery.state`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<bool>,
    /// UPower's `EnergyRate` in watts, unchanged. It is positive in both directions, so
    /// [`PowerState::on_battery`] supplies the sign; `nil` without UPower.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_rate: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSignal {
    Changed,
}

/// `OnBattery` is a manager-wide answer across all UPower supplies, not a device property. This
/// matters for a docked laptop with two mains adapters; UPower aggregates it.
#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

/// The composite `DisplayDevice`, not `battery_BAT0`: UPower sums every battery there. `EnergyRate`
/// is a positive watt magnitude while charging or discharging; `obelisk.power` wants no direction,
/// so configs needing it read `obelisk.battery.state`.
#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait DisplayDevice {
    #[zbus(property)]
    fn energy_rate(&self) -> zbus::Result<f64>;
}

/// power-profiles-daemon. `ActiveProfile` is a writable property, so zbus generates its setter
/// from the getter. `Profiles` contains dictionaries with a `Profile` name plus unused driver
/// details; [`profile_names`] extracts the names.
#[zbus::proxy(interface = "org.freedesktop.UPower.PowerProfiles", assume_defaults = false)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn profiles(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

/// The daemon renamed `net.hadess.PowerProfiles` to `org.freedesktop.UPower.PowerProfiles` in
/// 0.20 while keeping the old name; try both, newest first.
const POWER_PROFILES_ENDPOINTS: [(&str, &str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
        "org.freedesktop.UPower.PowerProfiles",
    ),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles", "net.hadess.PowerProfiles"),
];

/// Extracts profile names from daemon descriptions. Missing or non-string `Profile`
/// entries are skipped; `power:set_profile(p)` validates against this list.
fn profile_names(profiles: &[HashMap<String, OwnedValue>]) -> Vec<String> {
    profiles
        .iter()
        .filter_map(|entry| entry.get("Profile"))
        .filter_map(|value| <&str>::try_from(value).ok())
        .map(str::to_string)
        .collect()
}

#[derive(Clone)]
pub struct PowerController {
    state: Arc<Mutex<PowerState>>,
    system_bus: zbus::Connection,
}

impl PowerController {
    /// Returns immediately; proxies build inside the spawned task because construction is an async
    /// round trip that would serialize `main.rs` startup.
    pub fn new(system_bus: zbus::Connection, events: UnboundedSender<PowerSignal>) -> Self {
        let state = Arc::new(Mutex::new(PowerState::default()));
        tokio::spawn(run_power_task(system_bus.clone(), Arc::clone(&state), events));
        Self { state, system_bus }
    }

    pub fn snapshot(&self) -> PowerState {
        self.state.lock().expect("power state mutex poisoned").clone()
    }

    /// `power:set_profile(p)`. Build a fresh proxy per click, not a cached one that must survive a
    /// daemon restart. Do not update locally; `ActiveProfile`'s property stream reports both this
    /// write and external switches.
    pub async fn set_profile(&self, profile: &str) {
        let Some(proxy) = connect_power_profiles(&self.system_bus).await else {
            eprintln!("power: set_profile({profile}) called but no power-profiles-daemon is reachable; ignored");
            return;
        };
        if let Err(err) = proxy.set_active_profile(profile).await {
            eprintln!("power: setting ActiveProfile to {profile} failed: {err}");
        }
    }
}

/// Tries [`POWER_PROFILES_ENDPOINTS`] in order; a proxy counts only after `active_profile()` reads
/// successfully because zbus contacts no service while building it.
async fn connect_power_profiles(system_bus: &zbus::Connection) -> Option<PowerProfilesProxy<'static>> {
    for (service, path, interface) in POWER_PROFILES_ENDPOINTS {
        let built = PowerProfilesProxy::builder(system_bus)
            .destination(service)
            .ok()?
            .path(path)
            .ok()?
            .interface(interface)
            .ok()?
            .build()
            .await;
        match built {
            Ok(proxy) => {
                if proxy.active_profile().await.is_ok() {
                    return Some(proxy);
                }
            }
            Err(err) => eprintln!("power: failed to build a proxy for {service}: {err}"),
        }
    }
    None
}

/// Reads every available field. Failed reads become `None`, not stale values: a stopped daemon is
/// worth showing, and this codebase has hit the live-looking stale-number failure three times.
async fn read_state(
    upower: Option<&UPowerProxy<'static>>,
    device: Option<&DisplayDeviceProxy<'static>>,
    profiles: Option<&PowerProfilesProxy<'static>>,
) -> PowerState {
    let mut state = PowerState::default();
    if let Some(upower) = upower {
        state.on_battery = upower.on_battery().await.ok();
    }
    if let Some(device) = device {
        state.energy_rate = device.energy_rate().await.ok();
    }
    if let Some(profiles) = profiles {
        state.active_profile = profiles.active_profile().await.ok();
        state.profiles = profiles.profiles().await.ok().map(|raw| profile_names(&raw));
    }
    state
}

/// `select!` needs a future per arm. A never-completing future represents a missing service; its
/// arm never wins while the other half runs.
async fn next_change<S: Stream + Unpin>(stream: &mut Option<S>) -> Option<S::Item> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Reads once, pushes, then follows all three property streams. Every wake re-reads the payload
/// instead of patching one field.
///
/// With neither service, no signal is sent, `obelisk.power` stays `nil` (ADR-0037), and the task
/// exits instead of parking on a dead stream.
async fn run_power_task(
    system_bus: zbus::Connection,
    state: Arc<Mutex<PowerState>>,
    events: UnboundedSender<PowerSignal>,
) {
    let upower = match UPowerProxy::new(&system_bus).await {
        Ok(proxy) => Some(proxy),
        Err(err) => {
            eprintln!("power: no UPower manager reachable ({err}); on_battery will not be reported this run");
            None
        }
    };
    let device = match DisplayDeviceProxy::new(&system_bus).await {
        Ok(proxy) => Some(proxy),
        Err(err) => {
            eprintln!("power: no UPower DisplayDevice reachable ({err}); energy_rate will not be reported this run");
            None
        }
    };
    let profiles = connect_power_profiles(&system_bus).await;
    if profiles.is_none() {
        eprintln!(
            "power: no power-profiles-daemon reachable; active_profile and profiles will not be reported this run"
        );
    }
    if upower.is_none() && device.is_none() && profiles.is_none() {
        eprintln!(
            "power: nothing on this host can answer any of obelisk.power's fields; power reporting disabled for this run"
        );
        return;
    }

    // Subscribe before the first read. A charger unplugged during the round trip must not land
    // between a read and a subscription that does not exist yet.
    let mut on_battery_changed = match upower.as_ref() {
        Some(proxy) => Some(proxy.receive_on_battery_changed().await),
        None => None,
    };
    let mut energy_rate_changed = match device.as_ref() {
        Some(proxy) => Some(proxy.receive_energy_rate_changed().await),
        None => None,
    };
    let mut active_profile_changed = match profiles.as_ref() {
        Some(proxy) => Some(proxy.receive_active_profile_changed().await),
        None => None,
    };

    let mut previous = read_state(upower.as_ref(), device.as_ref(), profiles.as_ref()).await;
    *state.lock().expect("power state mutex poisoned") = previous.clone();
    if events.send(PowerSignal::Changed).is_err() {
        return;
    }

    loop {
        tokio::select! {
            change = next_change(&mut on_battery_changed) => {
                if change.is_none() { on_battery_changed = None; }
            }
            change = next_change(&mut energy_rate_changed) => {
                if change.is_none() { energy_rate_changed = None; }
            }
            change = next_change(&mut active_profile_changed) => {
                if change.is_none() { active_profile_changed = None; }
            }
        }
        // All streams ended; parking would leak the proxies and their connection references.
        if on_battery_changed.is_none() && energy_rate_changed.is_none() && active_profile_changed.is_none() {
            eprintln!("power: every property stream ended; power will no longer update this run");
            return;
        }

        let current = read_state(upower.as_ref(), device.as_ref(), profiles.as_ref()).await;
        if current != previous {
            *state.lock().expect("power state mutex poisoned") = current.clone();
            previous = current;
            if events.send(PowerSignal::Changed).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use zbus::zvariant::Value;

    fn entry(pairs: &[(&str, Value<'static>)]) -> HashMap<String, OwnedValue> {
        pairs.iter().map(|(key, value)| (key.to_string(), OwnedValue::try_from(value.clone()).unwrap())).collect()
    }

    #[test]
    fn profile_names_pulls_the_profile_key_out_of_each_description_and_keeps_the_order() {
        let raw = vec![
            entry(&[("Profile", Value::from("power-saver")), ("Driver", Value::from("intel_pstate"))]),
            entry(&[("Profile", Value::from("balanced")), ("Driver", Value::from("intel_pstate"))]),
            entry(&[("Profile", Value::from("performance")), ("Driver", Value::from("intel_pstate"))]),
        ];

        assert_eq!(profile_names(&raw), ["power-saver", "balanced", "performance"]);
    }

    #[test]
    fn profile_names_skips_an_entry_with_no_profile_key_or_a_non_string_one() {
        let raw = vec![
            entry(&[("Driver", Value::from("placeholder"))]),
            entry(&[("Profile", Value::from(3i32))]),
            entry(&[("Profile", Value::from("balanced"))]),
        ];

        assert_eq!(profile_names(&raw), ["balanced"]);
    }

    #[test]
    fn profile_names_is_empty_for_an_empty_list() {
        assert_eq!(profile_names(&[]), Vec::<String>::new());
    }

    #[test]
    fn a_field_this_host_cannot_answer_is_absent_from_the_json_rather_than_null() {
        let state = PowerState { on_battery: Some(false), energy_rate: Some(0.0), ..PowerState::default() };

        let json = serde_json::to_value(&state).unwrap();

        assert_eq!(json["on_battery"], serde_json::json!(false));
        assert_eq!(json["energy_rate"], serde_json::json!(0.0));
        assert!(json.get("active_profile").is_none());
        assert!(json.get("profiles").is_none());
    }

    #[test]
    fn a_host_that_answers_nothing_serializes_to_an_empty_object() {
        assert_eq!(serde_json::to_value(PowerState::default()).unwrap(), serde_json::json!({}));
    }
}
