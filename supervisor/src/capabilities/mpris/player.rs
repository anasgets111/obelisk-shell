//! Per-player registry: binds a discovered MPRIS bus name, hydrates and keeps live one
//! [`PlayerState`] entry via its own resync loop. Split from `dbus::mpris` -- see
//! `dbus/mpris/mod.rs` for the module-level doc.

use std::collections::HashMap;
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
    /// The bus name with `org.mpris.MediaPlayer2.` stripped, e.g. `"spotify"`. What every
    /// `mpris:` command takes to name the player it acts on.
    pub id: String,
    /// `MediaPlayer2.Identity`, the player's own display name, e.g. `"Spotify"`. Empty string
    /// for a player that does not answer the property.
    pub identity: String,
    /// `"Playing"`, `"Paused"` or `"Stopped"`. A player that fails to answer keeps its previous
    /// value rather than dropping to a fabricated `"Stopped"`.
    pub play_state: String,
    /// `xesam:title`. Empty string when the player publishes no metadata, which is the normal
    /// state between tracks.
    pub title: String,
    /// `xesam:artist`, joined with `", "` when there is more than one. Empty string when absent.
    pub artist: String,
    /// An absolute path to the artwork, or an empty string. `mpris:artUrl` is taken only when it
    /// is a `file://` URL that canonicalizes to a file that exists, so a remote URL and a stale
    /// path both arrive as empty rather than as a path that fails to load. Held across an
    /// update that did not change the track, so the cover does not blink on a position tick.
    pub album_art_path: String,
    /// Playback offset in microseconds, correct as of [`PlayerState::position_updated_at`] and not
    /// after. Nothing polls it while a track plays, so a progress bar has to add the elapsed time
    /// itself rather than reading this every frame.
    pub position: i64,
    /// `CLOCK_MONOTONIC` microseconds at the instant [`PlayerState::position`] was read. Monotonic,
    /// not wall clock, so it survives a clock adjustment. Subtract it from a monotonic `now` to
    /// get how far the track has moved since.
    pub position_updated_at: i64,
    /// `-1` when `mpris:length` is absent/malformed (a live stream, or a player that simply
    /// doesn't report it) -- a genuine unavailable, not a fabricated zero (ADR-0036).
    pub length: i64,
}

pub(super) struct PlayerEntry {
    pub(super) player: MprisPlayerProxy<'static>,
    pub(super) last_known: PlayerState,
    track_identity: TrackIdentity,
    /// `mpris:trackid`, cached for `SetPosition`'s required `TrackId` argument (ADR-0036) --
    /// not an IDL-declared `PlayerState` field, so it isn't part of what gets pushed to Lua.
    pub(super) cached_trackid: Option<String>,
    /// `None` only in the brief window between this entry's insert and its forwarder task's
    /// own spawn completing (the forwarder's very first resync can fire immediately on
    /// subscribe, so `register_player` inserts before spawning).
    forwarder: Option<JoinHandle<()>>,
}

pub(super) type PlayerRegistry = Arc<Mutex<HashMap<String, PlayerEntry>>>;

/// `CLOCK_MONOTONIC`, in microseconds -- cross-process comparable on this machine (unlike
/// `std::time::Instant`, which Rust deliberately keeps opaque/non-serializable), matching the
/// IDL's "Monotonic clock timestamp in microseconds" declaration for `position_updated_at`.
/// Known open gap (ADR-0036's Consequences): `system.time` (§2.11, unbuilt) is a 1Hz
/// whole-second epoch, not comparable to this at microsecond resolution.
pub(super) fn monotonic_micros() -> i64 {
    let now: std::time::Duration = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(std::time::Duration::from)
        .unwrap_or_default();
    i64::try_from(now.as_micros()).unwrap_or(i64::MAX)
}

/// Re-reads every field `PlayerState` needs from `player`/`root` and folds it into
/// `previous` (track-identity caching for `album_art_path`/`length`, ADR-0036). Always
/// succeeds: every individual property read degrades to `previous`'s own last value on
/// failure, never the whole entry.
struct Resynced {
    state: PlayerState,
    identity: TrackIdentity,
    trackid: Option<String>,
}

/// What `resync` falls back to when a given round's own reads fail -- bundles `PlayerEntry`'s
/// three per-player-cached fields relevant to degradation (not `player`/`forwarder`, which
/// `resync` never touches).
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
    // `player_identity` is `MediaPlayer2.Identity`, the player's own human-readable name --
    // unrelated to `TrackIdentity` (`identity`/`new_identity` below), the composite key this
    // function uses to detect a real track change.
    let player_identity = root.identity().await.unwrap_or_default();
    let position = player.position().await.unwrap_or(0);

    // A full Metadata read failure (the GetAll call erroring, not one key inside it being
    // absent) means "we learned nothing new this round" -- every metadata-derived field keeps
    // its previous value rather than resetting to empty, which would register as a spurious
    // track change.
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

/// Binds `bus_name`'s player/root proxies, runs one initial [`resync`], inserts the
/// resulting entry, and only then spawns its live forwarder task -- returns early (logged)
/// if the initial bind fails, or if `CanControl` is `false` (a source that can't be
/// controlled isn't meaningfully useful in a status-bar UI, ADR-0036).
///
/// Insert-then-spawn, not spawn-then-insert: zbus's generated `receive_*_changed` streams
/// replay their cached current value immediately on subscribe, so the forwarder's first
/// `select!` can resolve before this function would otherwise have inserted the entry --
/// its write-back would then find nothing and `break`, permanently freezing the player at
/// its initial snapshot. Confirmed live: consistently hit on the first player registered in
/// a fresh session.
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

    let entry = PlayerEntry {
        player: player.clone(),
        last_known: state,
        track_identity: identity,
        cached_trackid: trackid,
        forwarder: None,
    };
    let previous = registry.lock().unwrap().insert(bus_name.clone(), entry);
    if let Some(previous) = previous
        && let Some(handle) = previous.forwarder
    {
        handle.abort();
    }

    let forwarder = spawn_player_forwarder(bus_name.clone(), player, root, registry.clone(), events.clone());
    match registry.lock().unwrap().get_mut(&bus_name) {
        Some(entry) => entry.forwarder = Some(forwarder),
        // Unregistered (a real NameOwnerChanged departure) in the brief window between the insert
        // above and this line -- abort the just-spawned forwarder rather than leaking it
        // untracked and un-abortable.
        None => forwarder.abort(),
    }
    let _ = events.send(MprisSignal::Changed);
}

/// Runs until every one of `PlaybackStatus`/`Metadata`'s generated `receive_*_changed`
/// streams and the real `Seeked` signal all end, re-running [`resync`] on any of them and
/// updating the registry entry in place -- no debounce, no incremental patching. `Position`
/// has no `receive_position_changed` trigger: the real freedesktop spec excludes it from
/// `PropertiesChanged` (too high-frequency), so `Seeked` is the only live signal for a
/// position change on its own.
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

/// Removes `bus_name`'s registry entry (a player that dropped off the bus -- `NameOwnerChanged`
/// with an empty new owner), aborting its forwarder task. No-op if it was never tracked (e.g. it
/// was `playerctld` or a non-controllable source `register_player` already skipped).
pub(super) fn unregister_player(registry: &PlayerRegistry, bus_name: &str, events: &UnboundedSender<MprisSignal>) {
    let removed = registry.lock().unwrap().remove(bus_name);
    if let Some(entry) = removed {
        if let Some(handle) = entry.forwarder {
            handle.abort();
        }
        let _ = events.send(MprisSignal::Changed);
    }
}
