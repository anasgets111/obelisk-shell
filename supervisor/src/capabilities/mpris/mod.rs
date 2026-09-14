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

pub use controller::{MprisController, MprisSignal, parse_control_args, parse_seek_args};

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MprisAction {
    /// (id: string, command: "play"|"pause"|"play_pause"|"next"|"previous") Controls `players[].id`.
    Control,
    /// (id: string, position_us: integer) Seeks to an absolute position in microseconds.
    Seek,
    /// (id: string, offset_us: integer) Seeks by a signed offset in microseconds.
    SeekRelative,
}

/// `obelisk.mpris` action dispatch (ADR-0037): matches, parses, and `tokio::spawn`s each write
/// action (ADR-0036/ADR-0029).
pub fn dispatch(controller: &MprisController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<MprisAction>(params) else { return };
    match action {
        MprisAction::Control => match parse_control_args(&params.arguments) {
            Some((id, cmd)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.control(&id, &cmd).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        // Same wire shape and same spawn; only the controller method differs.
        MprisAction::Seek | MprisAction::SeekRelative => match parse_seek_args(&params.arguments) {
            Some((id, position)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    match action {
                        MprisAction::Seek => controller.seek(&id, position).await,
                        MprisAction::SeekRelative => controller.seek_relative(&id, position).await,
                        other => unreachable!("the arm above admits two actions, not {other:?}"),
                    }
                });
            }
            None => crate::log_malformed_command(params),
        },
    }
}
