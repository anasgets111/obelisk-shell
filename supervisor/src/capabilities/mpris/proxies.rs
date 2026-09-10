//! Hand-written proxies for `org.mpris.MediaPlayer2`/`Player`; no maintained zbus MPRIS crate.
//! Split from `dbus::mpris`; see `dbus/mpris/mod.rs`.
//!
//! Every player uses fixed `/org/mpris/MediaPlayer2` (the freedesktop spec disallows otherwise),
//! while `destination` varies. Declare `default_path` only; binders take the bus name explicitly.

use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, OwnedValue};

#[zbus::proxy(interface = "org.mpris.MediaPlayer2", default_path = "/org/mpris/MediaPlayer2")]
pub(super) trait MprisRoot {
    #[zbus(property, name = "Identity")]
    fn identity(&self) -> zbus::Result<String>;
    /// Optional in the spec; several players publish no `.desktop` name, so absence is an answer
    /// rather than a failure (ADR-0137).
    #[zbus(property, name = "DesktopEntry")]
    fn desktop_entry(&self) -> zbus::Result<String>;
}

#[zbus::proxy(interface = "org.mpris.MediaPlayer2.Player", default_path = "/org/mpris/MediaPlayer2")]
pub(super) trait MprisPlayer {
    #[zbus(name = "Play")]
    fn play(&self) -> zbus::Result<()>;
    #[zbus(name = "Pause")]
    fn pause(&self) -> zbus::Result<()>;
    #[zbus(name = "PlayPause")]
    fn play_pause(&self) -> zbus::Result<()>;
    #[zbus(name = "Next")]
    fn next(&self) -> zbus::Result<()>;
    #[zbus(name = "Previous")]
    fn previous(&self) -> zbus::Result<()>;
    /// Relative seek in microseconds; also the `mpris:trackid` fallback when uncached (ADR-0036).
    #[zbus(name = "Seek")]
    fn seek(&self, offset_us: i64) -> zbus::Result<()>;
    /// Absolute seek. Per freedesktop, `track_id` must be the currently playing track's
    /// `mpris:trackid` or the call is a no-op. Real players do not enforce this reliably, so the
    /// cache stays fresh (ADR-0036).
    #[zbus(name = "SetPosition")]
    fn set_position(&self, track_id: ObjectPath<'_>, position_us: i64) -> zbus::Result<()>;

    #[zbus(property, name = "PlaybackStatus")]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Metadata")]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    /// Freedesktop excludes `Position` from `PropertiesChanged` because it changes too often.
    /// zbus otherwise caches this getter until that absent signal; live `busctl` showed a player
    /// advancing while the getter stayed at stale `0`. Disable caching with
    /// `emits_changed_signal = "false"`.
    #[zbus(property(emits_changed_signal = "false"), name = "Position")]
    fn position(&self) -> zbus::Result<i64>;
    #[zbus(property, name = "CanControl")]
    fn can_control(&self) -> zbus::Result<bool>;

    /// Freedesktop excludes `Position` from `PropertiesChanged`; this signal marks discontinuous
    /// jumps. `player.rs` waits on it for position-only changes beside `PlaybackStatus`/`Metadata`.
    #[zbus(signal, name = "Seeked")]
    fn seeked(&self, position_us: i64) -> zbus::Result<()>;
}

pub(super) async fn bind_root(connection: &zbus::Connection, bus_name: &str) -> zbus::Result<MprisRootProxy<'static>> {
    MprisRootProxy::builder(connection).destination(bus_name.to_string())?.build().await
}

pub(super) async fn bind_player(
    connection: &zbus::Connection,
    bus_name: &str,
) -> zbus::Result<MprisPlayerProxy<'static>> {
    MprisPlayerProxy::builder(connection).destination(bus_name.to_string())?.build().await
}
