//! [`MprisController`]: the `oblisk.mpris` write-action dispatcher and state owner. Split from
//! `dbus::mpris` -- see `dbus/mpris/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use super::metadata::clamp_seek_target;
use super::player::PlayerState;
use super::watcher::{service_name_for_id, spawn_discovery};

// -------------------------------------------------------------------------------------------
// State shape pushed as `oblisk.mpris`'s StateSnapshot (docs/oblisk-idl-api-specs.md §2.8).
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MprisState {
    pub players: Vec<PlayerState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MprisSignal {
    Changed,
}

#[derive(Debug)]
enum MprisActionError {
    UnknownPlayer,
}

impl std::fmt::Display for MprisActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPlayer => write!(f, "no MPRIS player with that id is currently tracked"),
        }
    }
}

impl std::error::Error for MprisActionError {}

const VALID_COMMANDS: [&str; 5] = ["play", "pause", "play_pause", "next", "previous"];

/// `mpris:send_command(id, cmd)`'s `arguments: [id, cmd]`. `cmd` validated against the IDL's
/// own five-value enum right here (drops the whole call on anything else).
pub fn parse_control_args(arguments: &[serde_json::Value]) -> Option<(String, String)> {
    let id = arguments.first()?.as_str()?.to_string();
    let cmd = arguments.get(1)?.as_str()?.to_string();
    VALID_COMMANDS.contains(&cmd.as_str()).then_some((id, cmd))
}

/// `mpris:seek(id, pos_us)`'s `arguments: [id, pos_us]`. Absolute microseconds, intentionally
/// unclamped here -- clamping happens once, in [`MprisController::seek`].
pub fn parse_seek_args(arguments: &[serde_json::Value]) -> Option<(String, i64)> {
    let id = arguments.first()?.as_str()?.to_string();
    let pos_us = arguments.get(1)?.as_i64()?;
    Some((id, pos_us))
}

/// `mpris:seek_relative(id, off)`'s `arguments: [id, off]`. Same shape as [`parse_seek_args`],
/// kept as a distinct function so each write action's parser matches its own command name.
pub fn parse_seek_relative_args(arguments: &[serde_json::Value]) -> Option<(String, i64)> {
    let id = arguments.first()?.as_str()?.to_string();
    let off = arguments.get(1)?.as_i64()?;
    Some((id, off))
}

/// No `events` field, unlike `TrayController` (docs/adr/0031): every write action here
/// (`control`/`seek`/`seek_relative`) issues its real D-Bus call and returns without ever
/// self-sending a signal (ADR-0036's "state flows through the signal, not the write call")
/// -- the next real player event is what triggers the next push.
/// `events` is only needed by [`watcher::spawn_discovery`]'s background task,
/// consumed directly at construction rather than stored redundantly.
#[derive(Clone)]
pub struct MprisController {
    registry: super::player::PlayerRegistry,
}

impl MprisController {
    /// Spawns discovery (`ListNames` scan, then live `NameOwnerChanged` tracking) on
    /// `connection` -- the session bus (MPRIS is a session-bus protocol, unlike
    /// NetworkManager/BlueZ/polkit's system bus). Returns immediately; the registry starts
    /// empty and fills in as `spawn_discovery`'s own tasks run.
    pub fn new(connection: zbus::Connection, events: UnboundedSender<MprisSignal>) -> Self {
        let registry: super::player::PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(spawn_discovery(connection, registry.clone(), events));
        Self { registry }
    }

    /// Fully inert controller: empty registry, no discovery task. Used when a dedicated
    /// session-bus connection couldn't even be established.
    pub fn inert(_events: UnboundedSender<MprisSignal>) -> Self {
        Self { registry: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Full, live re-derivation of `mpris.players` from the entire tracked registry.
    /// Synchronous: every registry entry's `last_known` is already up to date (the forwarder
    /// tasks recompute it before ever sending an [`MprisSignal`]), so no further D-Bus round
    /// trip is needed here.
    pub fn build_state(&self) -> MprisState {
        MprisState { players: self.registry.lock().unwrap().values().map(|entry| entry.last_known.clone()).collect() }
    }

    /// `mpris:send_command(id, cmd)`. `cmd` is already validated by [`parse_control_args`] against
    /// the IDL's five-value enum by the time it reaches here.
    pub async fn control(&self, id: &str, cmd: &str) {
        let Some(player) = self.find_player(id) else {
            eprintln!("mpris: send_command({id:?}, {cmd:?}) failed: {}", MprisActionError::UnknownPlayer);
            return;
        };
        let result = match cmd {
            "play" => player.play().await,
            "pause" => player.pause().await,
            "play_pause" => player.play_pause().await,
            "next" => player.next().await,
            "previous" => player.previous().await,
            _ => unreachable!("parse_control_args already validated cmd against VALID_COMMANDS"),
        };
        if let Err(err) = result {
            eprintln!("mpris: send_command({id:?}, {cmd:?}) failed: {err}");
        }
    }

    /// `mpris:seek(id, pos_us)`: `SetPosition(cached trackid, clamped pos_us)` when a trackid
    /// is cached, otherwise a relative `Seek` computed from the last known position -- some
    /// real players never report `mpris:trackid` at all (ADR-0036). State
    /// (`position`/`position_updated_at`) updates only once the real `Seeked`/
    /// `PropertiesChanged` signal arrives, never optimistically here.
    pub async fn seek(&self, id: &str, pos_us: i64) {
        self.seek_to(id, pos_us).await;
    }

    /// `mpris:seek_relative(id, off)`: same clamp-and-dispatch path as [`Self::seek`], computed
    /// from a live `Position` read, not the registry's cached one. `Position` is excluded from
    /// `PropertiesChanged` by the real MPRIS spec, so the cached value only advances when some
    /// other property triggers a resync -- a player playing steadily with nothing else changing
    /// can have an arbitrarily stale cached position. A live read costs one extra round trip
    /// but is the only way to make "-10s" mean 10 seconds before now, not before whenever the
    /// registry last resynced for an unrelated reason.
    pub async fn seek_relative(&self, id: &str, off: i64) {
        let Some(position) = self.live_position(id).await else {
            eprintln!("mpris: seek_relative({id:?}, {off}) failed: {}", MprisActionError::UnknownPlayer);
            return;
        };
        self.seek_to(id, position.saturating_add(off)).await;
    }

    /// A real, live `Position` read (not the registry's cached snapshot) -- see
    /// [`Self::seek_relative`]'s own doc comment for why this matters.
    async fn live_position(&self, id: &str) -> Option<i64> {
        let player = self.find_player(id)?;
        match player.position().await {
            Ok(position) => Some(position),
            Err(err) => {
                eprintln!("mpris: live Position read failed for {id:?}, falling back to the cached value: {err}");
                self.cached_position(id)
            }
        }
    }

    async fn seek_to(&self, id: &str, target_us: i64) {
        let Some(context) = self.find_seek_context(id) else {
            eprintln!("mpris: seek to {target_us} for {id:?} failed: {}", MprisActionError::UnknownPlayer);
            return;
        };
        let target = clamp_seek_target(target_us, context.length);
        let result = match context.trackid {
            Some(trackid) => match zbus::zvariant::ObjectPath::try_from(trackid.as_str()) {
                Ok(path) => context.player.set_position(path, target).await,
                Err(err) => {
                    eprintln!("mpris: cached trackid {trackid:?} for {} isn't a valid object path, falling back to relative Seek: {err}", context.bus_name);
                    context.player.seek(target - self.live_position(id).await.unwrap_or(0)).await
                }
            },
            // No trackid cached (some real players never report `mpris:trackid` at all) --
            // `Player.Seek` itself takes a relative offset, so `target` (already absolute) is
            // converted against a live position read, same reasoning as seek_relative's own.
            None => context.player.seek(target - self.live_position(id).await.unwrap_or(0)).await,
        };
        if let Err(err) = result {
            eprintln!("mpris: seek to {target} for {id:?} failed: {err}");
        }
    }

    fn find_player(&self, id: &str) -> Option<super::proxies::MprisPlayerProxy<'static>> {
        let bus_name = service_name_for_id(id);
        self.registry.lock().unwrap().get(&bus_name).map(|entry| entry.player.clone())
    }

    fn cached_position(&self, id: &str) -> Option<i64> {
        let bus_name = service_name_for_id(id);
        self.registry.lock().unwrap().get(&bus_name).map(|entry| entry.last_known.position)
    }

    fn find_seek_context(&self, id: &str) -> Option<SeekContext> {
        let bus_name = service_name_for_id(id);
        let guard = self.registry.lock().unwrap();
        let entry = guard.get(&bus_name)?;
        Some(SeekContext { bus_name: bus_name.clone(), player: entry.player.clone(), trackid: entry.cached_trackid.clone(), length: entry.last_known.length })
    }
}

/// [`MprisController::find_seek_context`]'s return shape -- a small named struct instead of a
/// four-element tuple, so `seek_to`'s call site reads by field name.
struct SeekContext {
    bus_name: String,
    player: super::proxies::MprisPlayerProxy<'static>,
    trackid: Option<String>,
    length: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_control_args_accepts_every_valid_command() {
        for cmd in VALID_COMMANDS {
            let args = vec![serde_json::json!("firefox.instance_1"), serde_json::json!(cmd)];
            assert_eq!(parse_control_args(&args), Some(("firefox.instance_1".to_string(), cmd.to_string())));
        }
    }

    #[test]
    fn parse_control_args_rejects_an_unrecognized_command() {
        let args = vec![serde_json::json!("firefox.instance_1"), serde_json::json!("stop")];
        assert_eq!(parse_control_args(&args), None);
    }

    #[test]
    fn parse_control_args_is_none_for_missing_or_wrong_typed_arguments() {
        assert_eq!(parse_control_args(&[]), None);
        assert_eq!(parse_control_args(&[serde_json::json!("id")]), None);
        assert_eq!(parse_control_args(&[serde_json::json!(1), serde_json::json!("play")]), None);
    }

    #[test]
    fn parse_seek_args_reads_id_and_absolute_microseconds() {
        let args = vec![serde_json::json!("firefox.instance_1"), serde_json::json!(1_000_000)];
        assert_eq!(parse_seek_args(&args), Some(("firefox.instance_1".to_string(), 1_000_000)));
    }

    #[test]
    fn parse_seek_args_accepts_a_negative_position() {
        // Unclamped at parse time by design (ADR-0036) -- clamping happens once, at the write.
        let args = vec![serde_json::json!("id"), serde_json::json!(-500)];
        assert_eq!(parse_seek_args(&args), Some(("id".to_string(), -500)));
    }

    #[test]
    fn parse_seek_relative_args_reads_id_and_a_signed_offset() {
        let args = vec![serde_json::json!("firefox.instance_1"), serde_json::json!(-10_000_000)];
        assert_eq!(parse_seek_relative_args(&args), Some(("firefox.instance_1".to_string(), -10_000_000)));
    }
}
