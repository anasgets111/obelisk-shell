//! Media players (`obelisk.mpris`, ADR-0036).
//!
//! Supervisor-owned session-bus MPRIS discovery and zero-polling progress state, so `mpris.players`
//! survives Renderer crash/reload like idle/lock authority (ADR-0010). Capture position once with
//! a monotonic timestamp and interpolate elapsed time client-side; read live position only when
//! seeking. Hand-written proxies (`proxies.rs`), a live `HashMap<bus_name, entry>` registry with
//! one forwarder per player (`player.rs`), controller dispatch (`controller.rs`), and pure
//! parsing/comparison helpers in `metadata.rs`, unit-testable without a live D-Bus connection,
//! make the boundaries explicit.
//!
//! Players never register; discovery is active (`watcher.rs`): scan `ListNames` once, then watch
//! `NameOwnerChanged` for `org.mpris.MediaPlayer2.` arrivals and departures. ADR-0036 also fixes
//! `playerctld` exclusion, album-art trust checks, track-identity caching, and `SetPosition`'s
//! `TrackId` fallback.

pub mod controller;
pub mod metadata;
pub mod player;
pub mod proxies;
pub mod watcher;

pub use controller::{MprisController, MprisSignal, PlayerCommand};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MprisAction {
    /// Controls `players[].id`.
    Control { id: String, cmd: PlayerCommand },
    /// Seeks to an absolute position in microseconds.
    Seek { id: String, position_us: i64 },
    /// Seeks by a signed offset in microseconds.
    SeekRelative { id: String, offset_us: i64 },
}

/// `obelisk.mpris` action dispatch (ADR-0037): `tokio::spawn`s each write action
/// (ADR-0036/ADR-0029). Seeks are unclamped here; each command clamps, or declines to, where it runs.
pub fn dispatch(controller: &MprisController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<MprisAction>(&envelope.params) else { return };
    let controller = controller.clone();
    tokio::spawn(async move {
        match action {
            MprisAction::Control { id, cmd } => controller.control(&id, cmd).await,
            MprisAction::Seek { id, position_us } => controller.seek(&id, position_us).await,
            MprisAction::SeekRelative { id, offset_us } => controller.seek_relative(&id, offset_us).await,
        }
    });
}
