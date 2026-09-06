//! Per-player registry: bind a discovered MPRIS name, hydrate one live [`PlayerState`] entry,
//! and maintain it in a resync loop. Split from `dbus::mpris`; see `dbus/mpris/mod.rs`.

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
    /// Bus-name suffix after `org.mpris.MediaPlayer2.`, e.g. `"spotify"`; used by `mpris:`
    /// commands.
    pub id: String,
    /// `MediaPlayer2.Identity`, e.g. `"Spotify"`; empty if unanswered.
    pub identity: String,
    /// `"Playing"`, `"Paused"`, or `"Stopped"`; retains the previous value if the player fails.
    pub play_state: String,
    /// `xesam:title`; empty when metadata is absent, normal between tracks.
    pub title: String,
    /// `xesam:artist`, joined with `", "`; empty when absent.
    pub artist: String,
    /// Absolute artwork path, or empty. `mpris:artUrl` must be a `file://` URL canonicalizing to
    /// an existing file; remote/stale URLs become empty. Held across same-track updates so covers
    /// do not blink.
    pub album_art_path: String,
    /// Playback offset in microseconds, valid at [`PlayerState::position_updated_at`]. Nothing
    /// polls it while playing; progress bars add elapsed time.
    pub position: i64,
    /// `CLOCK_MONOTONIC` microseconds when [`PlayerState::position`] was read; subtract from a
    /// monotonic `now` for elapsed time and survive wall-clock adjustments.
    pub position_updated_at: i64,
    /// `-1` when `mpris:length` is absent or malformed, as for a live stream; unavailable is not
    /// fabricated as zero (ADR-0036).
    pub length: i64,
    /// `xesam:url`, such as a local `file://` path or browser `https://` page; empty when absent,
    /// normal for a stream.
    ///
    /// Carried for ADR-0137: configs cannot reliably classify video versus song from site lists or
    /// extensions, which are taste-dependent.
    pub url: String,
    /// `MediaPlayer2.DesktopEntry`, the `.desktop` basename, e.g. `"mpv"` or `"firefox"`; empty
    /// when unpublished.
    ///
    /// Stable player name for app matching. `identity` is a display string that may localize or
    /// decorate.
    pub desktop_entry: String,
}

/// Allocates [`PlayerEntry::registered`] on the same terms as the tray counter.
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(0);

pub(super) struct PlayerEntry {
    pub(super) player: MprisPlayerProxy<'static>,
    pub(super) last_known: PlayerState,
    /// First-seen order used by [`ordered_players`]. HashMap order once leaked into
    /// `mpris.players`, letting `players[1]` swap on an unrelated tick.
    registered: u64,
    track_identity: TrackIdentity,
    /// Cached `mpris:trackid` for `SetPosition` (ADR-0036); internal, not pushed to Lua.
    pub(super) cached_trackid: Option<String>,
    /// `None` only between insertion and forwarder spawn; the first resync can fire immediately on
    /// subscribe, so insertion always comes first.
    forwarder: Option<JoinHandle<()>>,
}

pub(super) type PlayerRegistry = Arc<Mutex<HashMap<String, PlayerEntry>>>;

/// `mpris.players`, longest-running first by appearance, not [`PlayerState::id`]: `players[1]`
/// means the player that has been there, not a newly playing tab after an alphabetical reorder.
pub(super) fn ordered_players(registry: &PlayerRegistry) -> Vec<PlayerState> {
    let guard = registry.lock().expect("mpris registry mutex poisoned");
    let mut entries: Vec<&PlayerEntry> = guard.values().collect();
    entries.sort_by_key(|entry| entry.registered);
    entries.into_iter().map(|entry| entry.last_known.clone()).collect()
}

/// Cross-process-comparable `CLOCK_MONOTONIC` microseconds, matching the IDL's timestamp field;
/// opaque `std::time::Instant` would not. Known gap (ADR-0036): unbuilt `system.time` (§2.11) is
/// 1Hz, too coarse for this resolution.
pub(super) fn monotonic_micros() -> i64 {
    let now: std::time::Duration = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(std::time::Duration::from)
        .unwrap_or_default();
    i64::try_from(now.as_micros()).unwrap_or(i64::MAX)
}

/// Re-reads every `PlayerState` field from `player`/`root`, folding in `previous` for track
/// identity caching (`album_art_path`/`length`, ADR-0036). Always succeeds; failed properties
/// retain their previous values.
struct Resynced {
    state: PlayerState,
    identity: TrackIdentity,
    trackid: Option<String>,
}

/// `resync`'s fallback fields: the three cached values relevant to degradation, not
/// `player`/`forwarder`, which it never changes.
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
    // `player_identity` is MediaPlayer2.Identity, not the TrackIdentity key below.
    let player_identity = root.identity().await.unwrap_or_default();
    // Optional and absent on several players: an error means "I have none", not a stale value.
    let desktop_entry = root.desktop_entry().await.unwrap_or_default();
    let position = player.position().await.unwrap_or(0);

    // A full Metadata read failure keeps metadata-derived fields instead of resetting them and
    // causing a spurious track change.
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
            url: previous.as_ref().map(|p| p.state.url.clone()).unwrap_or_default(),
            desktop_entry,
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
        // Keep artwork across same-track updates; players may drop metadata keys after a full
        // description.
        url: match parsed.url {
            Some(url) => url,
            None if same_track => previous.as_ref().map(|p| p.state.url.clone()).unwrap_or_default(),
            None => String::new(),
        },
        desktop_entry,
    };
    Resynced { state, identity: new_identity, trackid }
}

/// Binds `bus_name`, runs [`resync`], inserts the entry, then starts its forwarder. Logs and
/// returns on bind failure or `CanControl == false`; uncontrollable sources are not useful in a
/// status bar (ADR-0036).
///
/// Insert before spawn: zbus `receive_*_changed` streams replay cached values, so a forwarder
/// could otherwise run before insertion, find nothing to update, and freeze the player. Confirmed
/// live for the first player in a fresh session.
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
        // A real NameOwnerChanged departure raced insertion; abort the new forwarder rather than
        // leak an untracked, un-abortable task.
        None => forwarder.abort(),
    }
    let _ = events.send(MprisSignal::Changed);
}

/// Re-runs [`resync`] on each `PlaybackStatus`/`Metadata` change or `Seeked`, updating the entry in
/// place with no debounce or incremental patching. `Position` is excluded from
/// `PropertiesChanged` as too high-frequency, so only `Seeked` signals position changes.
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

/// Removes a departed `bus_name` (`NameOwnerChanged` with an empty new owner) and aborts its
/// forwarder. No-op if untracked, including skipped `playerctld` or uncontrollable sources.
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

    /// An entry with only the two ordering fields. Binding a proxy makes no call, so a p2p pair
    /// with nobody answering is enough.
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

    /// HashMap iteration is process-seeded; without ordering, `players[1]` changed between pushes
    /// over the same set.
    #[tokio::test]
    async fn the_list_is_in_appearance_order_whatever_the_map_says() {
        let (connection, _peer) = p2p_pair().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Build before locking; holding a guard across await triggers clippy's
        // `await_holding_lock` lint.
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

    /// Position updates must not move a player under a config holding its index.
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
        // Re-registration of a live bus name keeps its sequence, as `register_player` does.
        replacement.registered = registry.lock().unwrap().get("spotify").expect("just inserted").registered;
        registry.lock().unwrap().insert("spotify".to_string(), replacement);

        let ids: Vec<String> = ordered_players(&registry).into_iter().map(|player| player.id).collect();
        assert_eq!(ids, ["spotify-again", "firefox"]);
    }
}
