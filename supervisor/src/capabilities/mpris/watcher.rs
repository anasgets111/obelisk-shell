//! Discovery: which session-bus names `obelisk.mpris` tracks. Split from `dbus::mpris`; see
//! `dbus/mpris/mod.rs`.
//!
//! Players never register; scan `ListNames` once, then watch `NameOwnerChanged` for arrivals and
//! departures under the same prefix (ADR-0036, independently validated by Quickshell's
//! `MprisWatcher`).

use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;

use super::MprisSignal;
use super::player::{PlayerRegistry, register_player, unregister_player};

pub(super) const MPRIS_SERVICE_PREFIX: &str = "org.mpris.MediaPlayer2.";
const EXCLUDED_SUFFIX: &str = "playerctld";

/// True for a trackable MPRIS bus name: the discovery prefix, except `playerctld`, `playerctl`'s
/// aggregator. It mirrors the active player, including `Identity`/`DesktopEntry`; live
/// introspection found that exclusion signal reliable (ADR-0036). This only affects this list.
pub(super) fn is_trackable_player(bus_name: &str) -> bool {
    bus_name.strip_prefix(MPRIS_SERVICE_PREFIX).is_some_and(|suffix| !suffix.is_empty() && suffix != EXCLUDED_SUFFIX)
}

/// IDL `id`: bus-name suffix after the discovery prefix, reversible with
/// [`service_name_for_id`] (ADR-0036). Meaningful only for names accepted by
/// [`is_trackable_player`].
pub(super) fn player_id(bus_name: &str) -> &str {
    bus_name.strip_prefix(MPRIS_SERVICE_PREFIX).unwrap_or(bus_name)
}

/// Reconstructs the bus name from write argument `id`, the inverse of [`player_id`] (ADR-0036);
/// writes recompute it instead of keeping a lookup table.
pub(super) fn service_name_for_id(id: &str) -> String {
    format!("{MPRIS_SERVICE_PREFIX}{id}")
}

/// Scans `ListNames` once, filters with [`is_trackable_player`], and registers each match.
async fn discover_existing(
    connection: &zbus::Connection,
    dbus_proxy: &zbus::fdo::DBusProxy<'static>,
    registry: &PlayerRegistry,
    events: &UnboundedSender<MprisSignal>,
) {
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

/// Binds `org.freedesktop.DBus`, subscribes to `NameOwnerChanged`, scans with
/// [`discover_existing`], then starts the forwarder over that live subscription. Bind or
/// subscribe failure logs and disables discovery.
///
/// Subscribe before scanning: scan-first can permanently miss a player appearing or disappearing
/// between the `ListNames` reply and subscription. A signal arriving before the scan reaches its
/// name only double-registers it, handled by `register_player`'s replace-and-abort logic.
pub(super) async fn spawn_discovery(
    connection: zbus::Connection,
    registry: PlayerRegistry,
    events: UnboundedSender<MprisSignal>,
) {
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
