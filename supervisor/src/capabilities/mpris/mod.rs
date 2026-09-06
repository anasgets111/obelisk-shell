//! Media players (`oblisk.mpris`, docs/oblisk-supervisor-services-dbus.md §3;
//! docs/oblisk-idl-api-specs.md §2.8; ADR-0036).
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

pub use controller::{MprisController, MprisSignal, parse_control_args, parse_seek_args, parse_seek_relative_args};

/// Actions accepted by `oblisk.mpris:invoke(...)`; exhaustive dispatch keeps variants and arms in
/// sync.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MprisAction {
    Control,
    Seek,
    SeekRelative,
}

/// `oblisk.mpris` action dispatch (ADR-0037): matches, parses, and `tokio::spawn`s each write
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
        MprisAction::Seek => match parse_seek_args(&params.arguments) {
            Some((id, pos_us)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.seek(&id, pos_us).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        MprisAction::SeekRelative => match parse_seek_relative_args(&params.arguments) {
            Some((id, off)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.seek_relative(&id, off).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
    }
}
