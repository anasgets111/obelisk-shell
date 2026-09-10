//! [`MprisController`]: `oblisk.mpris`'s write dispatcher and state owner. Split from
//! `dbus::mpris`; see `dbus/mpris/mod.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use super::metadata::clamp_seek_target;
use super::player::PlayerState;
use super::watcher::{service_name_for_id, spawn_discovery};

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct MprisState {
    /// Every MPRIS player, longest-running first. New players append and position updates do not
    /// move entries, so `players[1]` keeps its meaning.
    ///
    /// Empty when no player is running, which is valid, not an error.
    pub players: Vec<PlayerState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MprisSignal {
    Changed,
}

/// Every command here fails the same one way, and only into an `eprintln!`. An enum with `Display`
/// and `Error` impls bought nothing a constant does not: nothing matches on it and nothing returns
/// it.
const UNKNOWN_PLAYER: &str = "no MPRIS player with that id is currently tracked";

const VALID_COMMANDS: [&str; 5] = ["play", "pause", "play_pause", "next", "previous"];

/// `mpris:send_command(id, cmd)`'s `arguments: [id, cmd]`; rejects anything outside the IDL's
/// five command values.
pub fn parse_control_args(arguments: &[serde_json::Value]) -> Option<(String, String)> {
    let id = arguments.first()?.as_str()?.to_string();
    let cmd = arguments.get(1)?.as_str()?.to_string();
    VALID_COMMANDS.contains(&cmd.as_str()).then_some((id, cmd))
}

/// `arguments: [id, microseconds]`, shared by `mpris:seek` and `mpris:seek_relative`. The two
/// commands mean different things by the number -- an absolute position and a signed offset -- but
/// parse it identically, and a second copy of four lines only invited them to drift. Unclamped by
/// design (ADR-0036); each command clamps, or declines to, where it is executed.
pub fn parse_seek_args(arguments: &[serde_json::Value]) -> Option<(String, i64)> {
    let id = arguments.first()?.as_str()?.to_string();
    let microseconds = arguments.get(1)?.as_i64()?;
    Some((id, microseconds))
}

/// No `events` field, unlike `TrayController` (ADR-0031): `control`/`seek`/`seek_relative` issue
/// real D-Bus calls and never self-send a signal (ADR-0036); the next player event triggers the
/// push. The channel belongs to [`watcher::spawn_discovery`] and is consumed at construction.
#[derive(Clone)]
pub struct MprisController {
    registry: super::player::PlayerRegistry,
}

impl MprisController {
    /// Spawns discovery (`ListNames`, then `NameOwnerChanged`) on the session bus. Returns
    /// immediately; discovery fills the registry in background tasks.
    pub fn new(connection: zbus::Connection, events: UnboundedSender<MprisSignal>) -> Self {
        let registry: super::player::PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(spawn_discovery(connection, registry.clone(), events));
        Self { registry }
    }

    /// Inert controller with an empty registry and no discovery task, used when the session-bus
    /// connection cannot be established.
    pub fn inert() -> Self {
        Self { registry: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Re-derives `mpris.players` from the full registry. Synchronous because forwarders update
    /// each entry's `last_known` before sending an [`MprisSignal`].
    pub fn build_state(&self) -> MprisState {
        MprisState { players: super::player::ordered_players(&self.registry) }
    }

    /// `mpris:send_command(id, cmd)` after [`parse_control_args`] validates the IDL's five values.
    pub async fn control(&self, id: &str, cmd: &str) {
        let Some(player) = self.find_player(id) else {
            eprintln!("mpris: send_command({id:?}, {cmd:?}) failed: {}", UNKNOWN_PLAYER);
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

    /// `mpris:seek(id, pos_us)`: `SetPosition(cached trackid, clamped pos_us)` when available;
    /// otherwise relative `Seek` from the last known position because some players never report
    /// `mpris:trackid` (ADR-0036). State waits for real `Seeked`/`PropertiesChanged` signals.
    pub async fn seek(&self, id: &str, pos_us: i64) {
        self.seek_to(id, pos_us).await;
    }

    /// `mpris:seek_relative(id, off)`: MPRIS `Seek`, which is relative already.
    ///
    /// This used to read a live `Position` and convert the offset into an absolute `SetPosition`.
    /// That read is the one place `resolve_position`'s protection does not reach, and Firefox
    /// answers `Position` with `0` for seconds after any seek, so "forward five seconds" from
    /// 5:40 became `SetPosition(5s)` -- a jump to the start of the track rather than a step.
    /// Handing the offset to the player removes both the round trip and the invented origin, and
    /// is what `MediaService.qml`'s `seekBy` does.
    ///
    /// No clamping: the player owns its own endpoints, and MPRIS lets a `Seek` past the end move
    /// to the next track. Clamping here would need a length we may not have (ADR-0036) and would
    /// silently differ from what every other MPRIS client does.
    pub async fn seek_relative(&self, id: &str, off: i64) {
        let Some(player) = self.find_player(id) else {
            eprintln!("mpris: seek_relative({id:?}, {off}) failed: {}", UNKNOWN_PLAYER);
            return;
        };
        if let Err(err) = player.seek(off).await {
            eprintln!("mpris: seek_relative({id:?}, {off}) failed: {err}");
        }
    }

    /// Converts an absolute `target` into the relative `Seek` a player without a usable trackid
    /// needs. Refuses rather than inventing an origin: `unwrap_or(0)` here turned an unknown
    /// position into "seek to `target` from the start", and `-1` from a never-read position made
    /// the subtraction overflow for a large target.
    async fn seek_by_difference(
        &self,
        id: &str,
        player: &super::proxies::MprisPlayerProxy<'static>,
        target: i64,
    ) -> zbus::Result<()> {
        let Some(position) = self.live_position(id).await.filter(|position| *position >= 0) else {
            eprintln!("mpris: seek to {target} for {id:?} needs a position to convert against and has none");
            return Ok(());
        };
        let Some(offset) = target.checked_sub(position) else {
            eprintln!("mpris: seek to {target} for {id:?} does not fit an i64 offset from {position}");
            return Ok(());
        };
        player.seek(offset).await
    }

    /// A live `Position` read, not the cached snapshot.
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
            eprintln!("mpris: seek to {target_us} for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        let target = clamp_seek_target(target_us, context.length);
        let result = match context.trackid {
            Some(trackid) => match zbus::zvariant::ObjectPath::try_from(trackid.as_str()) {
                Ok(path) => context.player.set_position(path, target).await,
                Err(err) => {
                    eprintln!(
                        "mpris: cached trackid {trackid:?} for {} isn't a valid object path, falling back to relative Seek: {err}",
                        context.bus_name
                    );
                    self.seek_by_difference(id, &context.player, target).await
                }
            },
            // Some players never report `mpris:trackid`; `Player.Seek` takes a relative offset, so
            // the absolute target has to be converted against a live position.
            None => self.seek_by_difference(id, &context.player, target).await,
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
        Some(SeekContext {
            bus_name: bus_name.clone(),
            player: entry.player.clone(),
            trackid: entry.cached_trackid.clone(),
            length: entry.last_known.length,
        })
    }
}

/// [`MprisController::find_seek_context`]'s named return instead of a four-element tuple, so
/// `seek_to` reads fields by name.
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
        // Parse unclamped by design (ADR-0036); clamp once at the write.
        let args = vec![serde_json::json!("id"), serde_json::json!(-500)];
        assert_eq!(parse_seek_args(&args), Some(("id".to_string(), -500)));
    }

    #[test]
    fn parse_seek_args_reads_a_signed_offset_for_the_relative_command() {
        let args = vec![serde_json::json!("firefox.instance_1"), serde_json::json!(-10_000_000)];
        assert_eq!(parse_seek_args(&args), Some(("firefox.instance_1".to_string(), -10_000_000)));
    }
}
