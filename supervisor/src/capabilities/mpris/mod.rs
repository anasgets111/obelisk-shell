//! Media players (`oblisk.mpris`, docs/oblisk-supervisor-services-dbus.md §3;
//! docs/oblisk-idl-api-specs.md §2.8; ADR-0036).
//!
//! Supervisor-owned session-bus MPRIS player discovery and zero-polling progress-sync state,
//! so `mpris.players` survives a Renderer crash/reload the same way idle/lock authority does
//! (ADR-0010). Position is captured once with a monotonic timestamp rather than polled; a
//! progress bar interpolates the elapsed time client-side. Seeking reads a live position over
//! D-Bus on demand instead of tracking one continuously. Hand-written `#[zbus::proxy]` traits
//! (`proxies.rs` -- no maintained zbus proxy crate for MPRIS), a `HashMap<bus_name, entry>`
//! dynamic registry hydrated live and kept live via one forwarder task per tracked player
//! (`player.rs`), a `*Controller` struct owning that registry plus write-action dispatch
//! (`controller.rs`), and pure parsing/comparison helpers unit-testable without a live D-Bus
//! connection (`metadata.rs`).
//!
//! MPRIS players never register with anything -- discovery is active (`watcher.rs`):
//! `ListNames` scanned once at startup, then `NameOwnerChanged` watched for the
//! `org.mpris.MediaPlayer2.` prefix going forward for both arrival and departure. This shape,
//! and every other real design decision in this module (`playerctld` exclusion, album-art
//! trust-checking, track-identity caching, `SetPosition`'s `TrackId` fallback), is grounded
//! in ADR-0036.

pub mod controller;
pub mod metadata;
pub mod player;
pub mod proxies;
pub mod watcher;

pub use controller::{MprisController, MprisSignal, parse_control_args, parse_seek_args, parse_seek_relative_args};

/// Every action `oblisk.mpris:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MprisAction {
    Control,
    Seek,
    SeekRelative,
}

/// `oblisk.mpris`'s action dispatch (ADR-0037): owns the action match, argument parse, and
/// write-action spawn for every `mpris` `CommandEnvelope`. Write actions are `tokio::spawn`ed
/// rather than awaited inline (ADR-0036/ADR-0029).
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
