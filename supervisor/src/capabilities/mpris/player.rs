//! Per-player registry: binds a discovered MPRIS bus name, hydrates and keeps live one
//! [`PlayerState`] entry via its own resync loop. Split from `dbus::mpris`, see
//! `dbus/mpris/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use super::MprisSignal;
use super::metadata::{TrackIdentity, parse_metadata, resolve_album_art_path};
use super::proxies::{MprisPlayerProxy, MprisRootProxy, bind_player, bind_root};
use super::watcher::player_id;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PlayerState {
    /// Bus name with `org.mpris.MediaPlayer2.` stripped, e.g. `"spotify"`; what `mpris:`
    /// commands use to name a player.
    pub id: String,
    /// `MediaPlayer2.Identity`, the player's display name, e.g. `"Spotify"`; empty if unanswered.
    pub identity: String,
    /// `"Playing"`, `"Paused"` or `"Stopped"`; keeps its previous value rather than fabricating
    /// `"Stopped"` if the player fails to answer.
    pub play_state: String,
    /// `xesam:title`; empty when the player publishes no metadata, the normal state between tracks.
    pub title: String,
    /// `xesam:artist`, joined with `", "` when there is more than one. Empty when absent.
    pub artist: String,
    /// An absolute path to the artwork, or empty. `mpris:artUrl` counts only as a `file://` URL
    /// that canonicalizes to an existing file, so a remote or stale URL both arrive empty rather
    /// than a path that fails to load; held across a same-track update so the cover doesn't blink.
    pub album_art_path: String,
    /// Playback offset in microseconds, correct as of [`PlayerState::position_updated_at`] and
    /// not after; nothing polls it while playing, so a progress bar must add elapsed time itself.
    pub position: i64,
    /// `CLOCK_MONOTONIC` microseconds when [`PlayerState::position`] was read. Monotonic, not
    /// wall clock, so it survives a clock adjustment; subtract from a monotonic `now` for elapsed.
    pub position_updated_at: i64,
    /// `-1` when `mpris:length` is absent or malformed: a live stream, or a player that simply
    /// doesn't report it. A genuine unavailable, not a fabricated zero (ADR-0036).
    pub length: i64,
}

/// Hands out [`PlayerEntry::registered`], on the same terms as the tray's own counter.
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(0);

pub(super) struct PlayerEntry {
    pub(super) player: MprisPlayerProxy<'static>,
    pub(super) last_known: PlayerState,
    /// When this player was first seen; the order [`ordered_players`] uses. A `HashMap`'s order
    /// used to leak into `mpris.players`, letting `players[1]` swap tracks on an unrelated tick.
    registered: u64,
    track_identity: TrackIdentity,
    /// `mpris:trackid`, cached for `SetPosition`'s required `TrackId` argument (ADR-0036); not
    /// an IDL-declared `PlayerState` field, so it isn't pushed to Lua.
    pub(super) cached_trackid: Option<String>,
    /// `None` only between this entry's insert and its forwarder's spawn completing, since the
    /// forwarder's first resync can fire immediately on subscribe (insert always comes first).
    forwarder: Option<JoinHandle<()>>,
}

pub(super) type PlayerRegistry = Arc<Mutex<HashMap<String, PlayerEntry>>>;

/// `mpris.players`, longest-running first: appearance order, not a sort on [`PlayerState::id`],
/// since a config reaching for `players[1]` means "the one that has been there", not the
/// browser tab an alphabetical sort would hand it after it just started playing.
pub(super) fn ordered_players(registry: &PlayerRegistry) -> Vec<PlayerState> {
    let guard = registry.lock().expect("mpris registry mutex poisoned");
    let mut entries: Vec<&PlayerEntry> = guard.values().collect();
    entries.sort_by_key(|entry| entry.registered);
    entries.into_iter().map(|entry| entry.last_known.clone()).collect()
}

/// `CLOCK_MONOTONIC` microseconds, cross-process comparable unlike opaque `std::time::Instant`,
/// matching the IDL's "Monotonic clock timestamp in microseconds" for `position_updated_at`.
/// Known gap (ADR-0036): `system.time` (§2.11, unbuilt) is 1Hz, not comparable at this resolution.
pub(super) fn monotonic_micros() -> i64 {
    let now: std::time::Duration = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(std::time::Duration::from)
        .unwrap_or_default();
    i64::try_from(now.as_micros()).unwrap_or(i64::MAX)
}

/// Re-reads every field `PlayerState` needs from `player`/`root`, folding in `previous`
/// (track-identity caching for `album_art_path`/`length`, ADR-0036); always succeeds, since
/// each property degrades to `previous`'s last value on failure, never the whole entry.
struct Resynced {
    state: PlayerState,
    identity: TrackIdentity,
    trackid: Option<String>,
}

/// What `resync` falls back to when a round's reads fail: `PlayerEntry`'s three cached fields
/// relevant to degradation, not `player`/`forwarder`, which `resync` never touches.
struct Previous<'a> {
    state: &'a PlayerState,
    identity: &'a TrackIdentity,
    trackid: &'a Option<String>,
}

async fn resync(
    bus_name: &str,
    player: &MprisPlayerProxy<'static>,
    root: &MprisRootProxy<'static>,
    previous: Option<Previous<'_>>,
) -> Resynced {
    let play_state = match player.playback_status().await {
        Ok(status) => status,
        Err(err) => {
            eprintln!(
                "mpris: PlaybackStatus read failed for {bus_name}; keeping the last known value this round: {err}"
            );
            previous.as_ref().map(|p| p.state.play_state.clone()).unwrap_or_default()
        }
    };
    // `player_identity` is `MediaPlayer2.Identity`, unrelated to `TrackIdentity`
    // (`identity`/`new_identity` below), the composite key this function tracks.
    let player_identity = root.identity().await.unwrap_or_default();
    let position = player.position().await.unwrap_or(0);

    // A full Metadata read failure (GetAll erroring, not one key absent) means metadata-derived
    // fields keep their previous value instead of resetting to empty (a spurious track change).
    let Ok(metadata) = player.metadata().await else {
        eprintln!(
            "mpris: Metadata read failed for {bus_name}; keeping the last known title/artist/art/length/trackid this round"
        );
        let state = PlayerState {
            id: player_id(bus_name).to_string(),
            identity: player_identity,
            play_state,
            title: previous.as_ref().map(|p| p.state.title.clone()).unwrap_or_default(),
            artist: previous.as_ref().map(|p| p.state.artist.clone()).unwrap_or_default(),
            album_art_path: previous.as_ref().map(|p| p.state.album_art_path.clone()).unwrap_or_default(),
            position,
            position_updated_at: monotonic_micros(),
            length: previous.as_ref().map(|p| p.state.length).unwrap_or(-1),
        };
        let identity = previous.as_ref().map(|p| p.identity.clone()).unwrap_or_default();
        let trackid = previous.as_ref().and_then(|p| p.trackid.clone());
        return Resynced { state, identity, trackid };
    };
    let parsed = parse_metadata(&metadata);

    let new_identity = parsed.track_identity();
    let same_track = previous.as_ref().is_some_and(|p| *p.identity == new_identity);

    let album_art_path = match resolve_album_art_path(parsed.art_url.as_deref()) {
        path if !path.is_empty() => path,
        _ if same_track => previous.as_ref().map(|p| p.state.album_art_path.clone()).unwrap_or_default(),
        _ => String::new(),
    };
    let length = match parsed.length_us {
        Some(length) if length >= 0 => length,
        _ if same_track => previous.as_ref().map(|p| p.state.length).unwrap_or(-1),
        _ => -1,
    };

    let trackid = parsed.trackid.clone();
    let state = PlayerState {
        id: player_id(bus_name).to_string(),
        identity: player_identity,
        play_state,
        title: parsed.title,
        artist: parsed.artist,
        album_art_path,
        position,
        position_updated_at: monotonic_micros(),
        length,
    };
    Resynced { state, identity: new_identity, trackid }
}

/// Binds `bus_name`'s player/root proxies, runs one initial [`resync`], inserts the resulting
/// entry, then spawns its live forwarder. Returns early (logged) if the bind fails or `CanControl`
/// is `false`: uncontrollable sources aren't useful in a status bar (ADR-0036).
///
/// Insert-then-spawn, not the reverse: zbus's `receive_*_changed` streams replay their cached value
/// on subscribe, so the forwarder's first `select!` could resolve before insertion, find nothing to
/// write to, and freeze the player. Confirmed live on a fresh session's first player.
pub(super) async fn register_player(
    connection: &zbus::Connection,
    registry: &PlayerRegistry,
    events: &UnboundedSender<MprisSignal>,
    bus_name: String,
) {
    let player = match bind_player(connection, &bus_name).await {
        Ok(player) => player,
        Err(err) => {
            eprintln!("mpris: failed to bind Player for {bus_name}: {err}");
            return;
        }
    };
    let root = match bind_root(connection, &bus_name).await {
        Ok(root) => root,
        Err(err) => {
            eprintln!("mpris: failed to bind MediaPlayer2 for {bus_name}: {err}");
            return;
        }
    };
    match player.can_control().await {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("mpris: {bus_name} reports CanControl=false; not tracking it");
            return;
        }
        Err(err) => eprintln!("mpris: CanControl read failed for {bus_name} (tracking anyway): {err}"),
    }

    let Resynced { state, identity, trackid } = resync(&bus_name, &player, &root, None).await;

    let mut entry = PlayerEntry {
        player: player.clone(),
        last_known: state,
        registered: 0,
        track_identity: identity,
        cached_trackid: trackid,
        forwarder: None,
    };
    let previous = {
        let mut guard = registry.lock().unwrap();
        entry.registered = match guard.get(&bus_name) {
            // Same bus name, same player: holds its place; a restart gets a new unique name.
            Some(existing) => existing.registered,
            None => NEXT_REGISTRATION.fetch_add(1, Ordering::Relaxed),
        };
        guard.insert(bus_name.clone(), entry)
    };
    if let Some(previous) = previous
        && let Some(handle) = previous.forwarder
    {
        handle.abort();
    }

    let forwarder = spawn_player_forwarder(bus_name.clone(), player, root, registry.clone(), events.clone());
    match registry.lock().unwrap().get_mut(&bus_name) {
        Some(entry) => entry.forwarder = Some(forwarder),
        // Unregistered (a real NameOwnerChanged departure) between the insert above and here:
        // abort the just-spawned forwarder rather than leaking it untracked and un-abortable.
        None => forwarder.abort(),
    }
    let _ = events.send(MprisSignal::Changed);
}

/// Runs until `PlaybackStatus`/`Metadata`'s `receive_*_changed` streams and `Seeked` all end,
/// re-running [`resync`] on each and updating the registry entry in place: no debounce, no
/// incremental patching. `Position` has no such trigger (freedesktop excludes it from
/// `PropertiesChanged`, too high-frequency), so `Seeked` alone signals a position change.
fn spawn_player_forwarder(
    bus_name: String,
    player: MprisPlayerProxy<'static>,
    root: MprisRootProxy<'static>,
    registry: PlayerRegistry,
    events: UnboundedSender<MprisSignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut playback_status = player.receive_playback_status_changed().await;
        let mut metadata = player.receive_metadata_changed().await;
        let Ok(mut seeked) = player.receive_seeked().await else { return };

        loop {
            let fired = tokio::select! {
                Some(_) = playback_status.next() => true,
                Some(_) = metadata.next() => true,
                Some(_) = seeked.next() => true,
                else => false,
            };
            if !fired {
                break;
            }

            let previous =
                {
                    registry.lock().unwrap().get(&bus_name).map(|entry| {
                        (entry.last_known.clone(), entry.track_identity.clone(), entry.cached_trackid.clone())
                    })
                };
            let previous_ctx =
                previous.as_ref().map(|(state, identity, trackid)| Previous { state, identity, trackid });
            let Resynced { state, identity, trackid } = resync(&bus_name, &player, &root, previous_ctx).await;

            let mut guard = registry.lock().unwrap();
            let Some(entry) = guard.get_mut(&bus_name) else { break };
            entry.last_known = state;
            entry.track_identity = identity;
            entry.cached_trackid = trackid;
            drop(guard);

            if events.send(MprisSignal::Changed).is_err() {
                break;
            }
        }
    })
}

/// Removes `bus_name`'s registry entry (a player that dropped off the bus: `NameOwnerChanged`
/// with an empty new owner), aborting its forwarder task. No-op if never tracked, e.g.
/// `playerctld` or a non-controllable source `register_player` already skipped.
pub(super) fn unregister_player(registry: &PlayerRegistry, bus_name: &str, events: &UnboundedSender<MprisSignal>) {
    let removed = registry.lock().unwrap().remove(bus_name);
    if let Some(entry) = removed {
        if let Some(handle) = entry.forwarder {
            handle.abort();
        }
        let _ = events.send(MprisSignal::Changed);
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair;

    /// An entry with everything but the two fields the ordering depends on stubbed out. Binding a
    /// proxy makes no call, so a p2p pair with nobody answering is enough.
    async fn entry(connection: &zbus::Connection, id: &str, registered: u64) -> PlayerEntry {
        PlayerEntry {
            player: super::super::proxies::bind_player(connection, "org.mpris.MediaPlayer2.probe")
                .await
                .expect("binding makes no call"),
            last_known: PlayerState { id: id.to_string(), ..PlayerState::default() },
            registered,
            track_identity: TrackIdentity::default(),
            cached_trackid: None,
            forwarder: None,
        }
    }

    /// A `HashMap`'s iteration order is seeded per process, so this used to be whatever the seed
    /// said: a config reaching for `players[1]` could get a different player between two pushes
    /// over the same set, with nothing about that set having changed.
    #[tokio::test]
    async fn the_list_is_in_appearance_order_whatever_the_map_says() {
        let (connection, _peer) = p2p_pair().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Built before the lock: holding a guard across an await is `clippy::await_holding_lock`.
        let third = entry(&connection, "third", 2).await;
        let first = entry(&connection, "first", 0).await;
        let second = entry(&connection, "second", 1).await;
        {
            let mut guard = registry.lock().unwrap();
            guard.insert("zed".to_string(), third);
            guard.insert("alpha".to_string(), first);
            guard.insert("mid".to_string(), second);
        }
        let ids: Vec<String> = ordered_players(&registry).into_iter().map(|player| player.id).collect();
        assert_eq!(ids, ["first", "second", "third"], "an alphabetical sort would answer the other way");
    }

    /// A player that keeps pushing position updates must not move under a config holding its index.
    #[tokio::test]
    async fn a_player_that_resyncs_holds_its_place() {
        let (connection, _peer) = p2p_pair().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let spotify = entry(&connection, "spotify", 0).await;
        let firefox = entry(&connection, "firefox", 1).await;
        {
            let mut guard = registry.lock().unwrap();
            guard.insert("spotify".to_string(), spotify);
            guard.insert("firefox".to_string(), firefox);
        }
        let mut replacement = entry(&connection, "spotify-again", 999).await;
        // What `register_player` does when a live bus name registers again: keep the sequence.
        replacement.registered = registry.lock().unwrap().get("spotify").expect("just inserted").registered;
        registry.lock().unwrap().insert("spotify".to_string(), replacement);

        let ids: Vec<String> = ordered_players(&registry).into_iter().map(|player| player.id).collect();
        assert_eq!(ids, ["spotify-again", "firefox"]);
    }
}
