//! [`PowerController`]: the `oblisk.power` state owner and its one write action.
//! Split from `power` -- see `power/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{Stream, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedValue;

/// `oblisk.power`'s full payload (§ 2.13). Every field is `Option` and every absent one is
/// omitted from the JSON rather than serialized as `null`, so a config reads `nil` for anything
/// this host cannot answer. See `power/mod.rs` for why that is four optional fields instead of a
/// capability that either works or does not.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PowerState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_rate: Option<f64>,
}

/// One shared signal, `Changed` only (mirrors `BatterySignal`/`BrightnessSignal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSignal {
    Changed,
}

/// `OnBattery` is on the manager object, not on any device: it is the system-wide answer across
/// every power supply UPower knows about, which is what makes it worth a D-Bus call rather than
/// a second reading of the sysfs tree `battery` already walks. A laptop in a dock with two mains
/// adapters is the case that separates the two, and UPower aggregates it for free.
#[zbus::proxy(interface = "org.freedesktop.UPower", default_service = "org.freedesktop.UPower", default_path = "/org/freedesktop/UPower")]
trait UPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

/// The composite `DisplayDevice`, not a specific `battery_BAT0`: UPower already sums every
/// battery on the machine into this one object, and picking a device here would repeat
/// `battery::controller::select_system_battery`'s work against a different source of truth.
///
/// `EnergyRate` is a magnitude in Watts and is positive while charging and while discharging
/// alike; `State` is what says which. § 2.13 asks for the rate and specifies no direction, so
/// this reports the rate, and a config that needs the direction reads `oblisk.battery.charging`,
/// which is already there.
#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait DisplayDevice {
    #[zbus(property)]
    fn energy_rate(&self) -> zbus::Result<f64>;
}

/// power-profiles-daemon. `ActiveProfile` is a writable property rather than a method, so
/// `set_profile` is a property write and zbus generates the setter from the getter below.
///
/// `Profiles` is an array of dictionaries, not an array of strings: each entry describes one
/// profile with a `Profile` key holding its name alongside driver details this capability has no
/// use for. § 2.13 asks for "an array of strings representing all hardware profiles", which is
/// the `Profile` values, so [`profile_names`] pulls them out.
#[zbus::proxy(interface = "org.freedesktop.UPower.PowerProfiles", assume_defaults = false)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn profiles(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

/// The daemon renamed itself from `net.hadess.PowerProfiles` to `org.freedesktop.UPower.
/// PowerProfiles` in 0.20 and kept the old name working, so both are tried in that order:
/// service, object path, interface. Newest first, because a machine that answers both should be
/// read through the name its own documentation now uses.
const POWER_PROFILES_ENDPOINTS: [(&str, &str, &str); 2] = [
    ("org.freedesktop.UPower.PowerProfiles", "/org/freedesktop/UPower/PowerProfiles", "org.freedesktop.UPower.PowerProfiles"),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles", "net.hadess.PowerProfiles"),
];

/// Pulls § 2.13's array of profile names out of power-profiles-daemon's array of profile
/// descriptions. An entry with no `Profile` key, or one holding something that is not a string,
/// is skipped rather than turned into a placeholder: this list is what `power:set_profile(p)`
/// validates against by handing it back, and a name that does not name a profile is worse than a
/// shorter list.
fn profile_names(profiles: &[HashMap<String, OwnedValue>]) -> Vec<String> {
    profiles
        .iter()
        .filter_map(|entry| entry.get("Profile"))
        .filter_map(|value| <&str>::try_from(value).ok())
        .map(str::to_string)
        .collect()
}

/// `power:set_profile(p)`'s `arguments: [p]`. Shape check only: § 3.2 says `p` must match one of
/// the host's profiles, and power-profiles-daemon is the thing that knows them. It rejects an
/// unknown name itself, which is one authority rather than two copies of the same list drifting.
pub fn parse_set_profile_args(arguments: &[serde_json::Value]) -> Option<String> {
    Some(arguments.first()?.as_str()?.to_string())
}

/// `Clone` (mirrors `BrightnessController`): `main.rs`'s `power:set_profile` dispatch arm needs a
/// cheap `Arc`-backed copy to hand to the `tokio::spawn`ed task the D-Bus write runs in.
#[derive(Clone)]
pub struct PowerController {
    state: Arc<Mutex<PowerState>>,
    system_bus: zbus::Connection,
}

impl PowerController {
    /// Returns immediately; every proxy is built inside the spawned task, because building one
    /// is an async round trip and a controller constructor that awaits would make `main.rs`'s
    /// startup serial in the number of D-Bus capabilities.
    pub fn new(system_bus: zbus::Connection, events: UnboundedSender<PowerSignal>) -> Self {
        let state = Arc::new(Mutex::new(PowerState::default()));
        tokio::spawn(run_power_task(system_bus.clone(), Arc::clone(&state), events));
        Self { state, system_bus }
    }

    pub fn snapshot(&self) -> PowerState {
        self.state.lock().expect("power state mutex poisoned").clone()
    }

    /// `power:set_profile(p)`. Builds the proxy fresh, the same shape
    /// `BrightnessController::set` uses for its own logind call: this runs when a user clicks a
    /// profile, not on a hot path, and a cached proxy would have to survive a daemon restart
    /// that a fresh one simply reconnects across.
    ///
    /// No optimistic local update. The real state change arrives on `ActiveProfile`'s own
    /// property-changed stream, which is also what catches a profile switched by something other
    /// than this shell.
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

/// Tries each [`POWER_PROFILES_ENDPOINTS`] triple in order and returns the first that answers a
/// real property read. Building a proxy alone proves nothing (zbus does not contact the service
/// to build one), so the probe is `active_profile()`: a host with no daemon fails both and gets
/// `None`, which is what makes those two fields absent rather than fabricated.
async fn connect_power_profiles(system_bus: &zbus::Connection) -> Option<PowerProfilesProxy<'static>> {
    for (service, path, interface) in POWER_PROFILES_ENDPOINTS {
        let built = PowerProfilesProxy::builder(system_bus).destination(service).ok()?.path(path).ok()?.interface(interface).ok()?.build().await;
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

/// Reads every field this host can answer. A property read that fails leaves its field `None`
/// rather than keeping the last known value: a daemon that stopped answering is a fact worth
/// showing, and a stale number that looks live is the failure mode this codebase has now been
/// bitten by three times.
async fn read_state(upower: Option<&UPowerProxy<'static>>, device: Option<&DisplayDeviceProxy<'static>>, profiles: Option<&PowerProfilesProxy<'static>>) -> PowerState {
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

/// `select!` needs every arm to hold a future, and a half of this capability that is not present
/// on the host has no stream to poll. A future that never completes is the honest stand-in: the
/// arm is simply never the one that wins, and the loop keeps running on whichever half is real.
async fn next_change<S: Stream + Unpin>(stream: &mut Option<S>) -> Option<S::Item> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Reads once, pushes, then follows all three property-changed streams. Every wake re-reads the
/// whole payload rather than patching the one field that fired, which is the same full-re-derive
/// shape every `main.rs` snapshot arm already uses, and it costs three property reads on a change
/// that happens when a user unplugs a charger.
///
/// A host with neither UPower nor power-profiles-daemon never sends a signal at all, so
/// `oblisk.power` stays `nil` (ADR-0037), and the task exits instead of parking on a stream that
/// will never fire.
async fn run_power_task(system_bus: zbus::Connection, state: Arc<Mutex<PowerState>>, events: UnboundedSender<PowerSignal>) {
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
        eprintln!("power: no power-profiles-daemon reachable; active_profile and profiles will not be reported this run");
    }
    if upower.is_none() && device.is_none() && profiles.is_none() {
        eprintln!("power: nothing on this host can answer any of § 2.13's fields; power reporting disabled for this run");
        return;
    }

    let mut previous = read_state(upower.as_ref(), device.as_ref(), profiles.as_ref()).await;
    *state.lock().expect("power state mutex poisoned") = previous.clone();
    if events.send(PowerSignal::Changed).is_err() {
        return;
    }

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
        // Every stream ended, so nothing left can wake this task and the `select!` above would
        // park on three `pending` futures forever. Dropping out is what releases the proxies and
        // the connection references they hold.
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

    // ---- profile_names ----

    #[test]
    fn profile_names_pulls_the_profile_key_out_of_each_description_and_keeps_the_order() {
        // The shape power-profiles-daemon actually publishes: each profile is a dictionary whose
        // other keys name the drivers implementing it, none of which § 2.13 asks for.
        let raw = vec![
            entry(&[("Profile", Value::from("power-saver")), ("Driver", Value::from("intel_pstate"))]),
            entry(&[("Profile", Value::from("balanced")), ("Driver", Value::from("intel_pstate"))]),
            entry(&[("Profile", Value::from("performance")), ("Driver", Value::from("intel_pstate"))]),
        ];

        assert_eq!(profile_names(&raw), ["power-saver", "balanced", "performance"]);
    }

    #[test]
    fn profile_names_skips_an_entry_with_no_profile_key_or_a_non_string_one() {
        // A shorter list is recoverable: `power:set_profile` hands a name straight back to the
        // daemon, so a fabricated placeholder would be a name the config could pick and the
        // daemon would reject.
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

    // ---- parse_set_profile_args ----

    #[test]
    fn parse_set_profile_args_reads_the_first_argument_as_a_profile_name() {
        assert_eq!(parse_set_profile_args(&[serde_json::json!("performance")]), Some("performance".to_string()));
    }

    #[test]
    fn parse_set_profile_args_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_set_profile_args(&[]), None);
        assert_eq!(parse_set_profile_args(&[serde_json::json!(2)]), None);
    }

    // ---- PowerState serialization ----

    #[test]
    fn a_field_this_host_cannot_answer_is_absent_from_the_json_rather_than_null() {
        // The whole point of `power/mod.rs`'s optional-field rule: a machine with UPower and no
        // power-profiles-daemon reports two real fields and stays silent on the other two, and
        // `nil` is how a config tells "no profile daemon" from "the balanced profile".
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
