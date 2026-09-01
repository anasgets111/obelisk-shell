//! Hand-written proxies for `org.mpris.MediaPlayer2`/`org.mpris.MediaPlayer2.Player` (no
//! maintained zbus proxy crate for MPRIS). Split from `dbus::mpris` -- see `dbus/mpris/mod.rs`
//! for the module-level doc.
//!
//! Every real MPRIS player lives at the fixed object path `/org/mpris/MediaPlayer2` (the
//! freedesktop spec doesn't allow otherwise), but the bus name (`destination`) varies per player
//! -- so these declare `default_path` only, never `default_service`, and every binder below takes
//! the bus name explicitly.

use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, OwnedValue};

#[zbus::proxy(interface = "org.mpris.MediaPlayer2", default_path = "/org/mpris/MediaPlayer2")]
pub(super) trait MprisRoot {
    #[zbus(property, name = "Identity")]
    fn identity(&self) -> zbus::Result<String>;
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
    /// Relative seek, microseconds -- also `mpris:trackid`'s fallback when no trackid is cached
    /// (ADR-0036).
    #[zbus(name = "Seek")]
    fn seek(&self, offset_us: i64) -> zbus::Result<()>;
    /// Absolute seek. `track_id` must be the *currently playing* track's `mpris:trackid` per
    /// the real freedesktop spec (a no-op otherwise) -- ADR-0036 notes this isn't reliably
    /// enforced by every real player, so the cache feeding this is kept fresh regardless.
    #[zbus(name = "SetPosition")]
    fn set_position(&self, track_id: ObjectPath<'_>, position_us: i64) -> zbus::Result<()>;

    #[zbus(property, name = "PlaybackStatus")]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Metadata")]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    /// The real freedesktop spec excludes `Position` from `PropertiesChanged` (it changes too
    /// often). zbus's proxy macro caches a `#[zbus(property)]` getter's value and only
    /// refreshes it on that property's own change signal, so without `emits_changed_signal =
    /// "false"` here this always returns whatever was cached on the first read -- confirmed
    /// live via `busctl` showing a real player's `Position` genuinely advancing while this
    /// stayed at a stale `0`.
    #[zbus(property(emits_changed_signal = "false"), name = "Position")]
    fn position(&self) -> zbus::Result<i64>;
    #[zbus(property, name = "CanControl")]
    fn can_control(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanPlay")]
    fn can_play(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanPause")]
    fn can_pause(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanSeek")]
    fn can_seek(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanGoNext")]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanGoPrevious")]
    fn can_go_previous(&self) -> zbus::Result<bool>;

    /// The real freedesktop spec excludes `Position` from `PropertiesChanged`; a discontinuous
    /// jump is signaled here instead. `player.rs`'s forwarder task waits on this for
    /// position-only changes, alongside `PlaybackStatus`/`Metadata`'s own change streams.
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
