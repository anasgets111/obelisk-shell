//! Discovery: which session-bus names `oblisk.mpris` tracks, and how it finds them. Split from
//! `dbus::mpris` -- see `dbus/mpris/mod.rs` for the module-level doc.
//!
//! Unlike `dbus::tray` (items register themselves via an explicit call, liveness tracked only by
//! `NameOwnerChanged` removal) or `dbus::bluetooth` (`ObjectManager`-driven), MPRIS players never
//! register with anything -- discovery is active: `ListNames` scanned once at startup, then
//! `NameOwnerChanged` watched for the same prefix going forward for both arrival and departure.
//! This exact shape (`registerExisting()` + a service watcher on `"org.mpris.MediaPlayer2*"`) is
//! independently validated by Quickshell's own `MprisWatcher` (`watcher.cpp`), a mature real-world
//! implementation -- confirmed here rather than invented from scratch (ADR-0036).

use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;

use super::MprisSignal;
use super::player::{PlayerRegistry, register_player, unregister_player};

pub(super) const MPRIS_SERVICE_PREFIX: &str = "org.mpris.MediaPlayer2.";
const EXCLUDED_SUFFIX: &str = "playerctld";

/// True for any real MPRIS player bus name this capability should track -- the discovery prefix,
/// minus `playerctld` (the `playerctl` project's own aggregator: it transparently mirrors
/// whichever real player is active, including its own `Identity`/`DesktopEntry` -- confirmed via
/// live introspection to be the only reliable exclusion signal, since no property distinguishes
/// it from the player it's proxying; ADR-0036). Excluding it here does not affect anything that
/// talks to it directly (media keys, the `playerctl` CLI) -- only `oblisk.mpris`'s own discovered-
/// player list.
pub(super) fn is_trackable_player(bus_name: &str) -> bool {
    bus_name.strip_prefix(MPRIS_SERVICE_PREFIX).is_some_and(|suffix| !suffix.is_empty() && suffix != EXCLUDED_SUFFIX)
}

/// The `id` IDL exposes: the bus name's suffix after the discovery prefix (ADR-0036, reversible
/// with [`service_name_for_id`]). Only ever meaningful on a name [`is_trackable_player`] accepted.
pub(super) fn player_id(bus_name: &str) -> &str {
    bus_name.strip_prefix(MPRIS_SERVICE_PREFIX).unwrap_or(bus_name)
}

/// Reconstructs the full bus name from an `id` a write command's `arguments[0]` carries -- the
/// other half of [`player_id`]'s reversible transform (ADR-0036). No `id -> bus_name` lookup
/// table is kept; every write dispatch recomputes this.
pub(super) fn service_name_for_id(id: &str) -> String {
    format!("{MPRIS_SERVICE_PREFIX}{id}")
}

/// `ListNames` scanned once, filtered by [`is_trackable_player`], each match handed to
/// [`register_player`].
async fn discover_existing(connection: &zbus::Connection, dbus_proxy: &zbus::fdo::DBusProxy<'static>, registry: &PlayerRegistry, events: &UnboundedSender<MprisSignal>) {
    let names = match dbus_proxy.list_names().await {
        Ok(names) => names,
        Err(err) => {
            eprintln!("mpris: ListNames failed; starting with no discovered players: {err}");
            return;
        }
    };
    for name in names {
        let name = name.to_string();
        if is_trackable_player(&name) {
            register_player(connection, registry, events, name).await;
        }
    }
}

/// Binds `org.freedesktop.DBus`, subscribes to `NameOwnerChanged` *before* running the initial
/// [`discover_existing`] scan, then spawns the ongoing forwarder loop over that already-live
/// subscription. Degrades to "no discovery" (logged) if `org.freedesktop.DBus` can't be bound, or
/// if the subscription itself fails -- matches `dbus::tray`'s own "degrade to inert, don't take
/// the Supervisor down" precedent for a session bus that isn't fully available.
///
/// Subscribe-then-scan, not scan-then-subscribe (Correctness review): `receive_name_owner_changed`
/// installs the real D-Bus match rule as soon as it completes. Scanning first left a real window
/// where a player that appeared or disappeared between the `ListNames` reply and the subscription
/// being installed was silently missed forever -- nothing else would ever trigger its
/// registration (or its removal) later. A `NameOwnerChanged` landing on this subscription before
/// `discover_existing`'s own scan reaches that same name is a harmless double-registration,
/// already safely handled by `register_player`'s insert-returns-previous-abort logic.
pub(super) async fn spawn_discovery(connection: zbus::Connection, registry: PlayerRegistry, events: UnboundedSender<MprisSignal>) {
    let dbus_proxy = match zbus::fdo::DBusProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("mpris: failed to bind org.freedesktop.DBus; player discovery disabled for this run: {err}");
            return;
        }
    };
    let mut stream = match dbus_proxy.receive_name_owner_changed().await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("mpris: failed to subscribe to NameOwnerChanged; player discovery disabled for this run: {err}");
            return;
        }
    };

    discover_existing(&connection, &dbus_proxy, &registry, &events).await;

    tokio::spawn(async move {
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };
            let name = args.name.to_string();
            if !is_trackable_player(&name) {
                continue;
            }
            if args.new_owner.is_some() {
                register_player(&connection, &registry, &events, name).await;
            } else {
                unregister_player(&registry, &name, &events);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_trackable_player_accepts_a_real_player_bus_name() {
        assert!(is_trackable_player("org.mpris.MediaPlayer2.firefox.instance_1_59239"));
    }

    #[test]
    fn is_trackable_player_rejects_playerctld() {
        assert!(!is_trackable_player("org.mpris.MediaPlayer2.playerctld"));
    }

    #[test]
    fn is_trackable_player_rejects_names_outside_the_prefix() {
        assert!(!is_trackable_player("org.freedesktop.DBus"));
        assert!(!is_trackable_player("org.mpris.MediaPlayer2"));
    }

    #[test]
    fn player_id_strips_the_discovery_prefix() {
        assert_eq!(player_id("org.mpris.MediaPlayer2.firefox.instance_1_59239"), "firefox.instance_1_59239");
    }

    #[test]
    fn service_name_for_id_and_player_id_are_inverse_transforms() {
        let bus_name = "org.mpris.MediaPlayer2.mpv.instance-abc123";
        let id = player_id(bus_name);
        assert_eq!(service_name_for_id(id), bus_name);
    }
}
