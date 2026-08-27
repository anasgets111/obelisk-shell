//! Media players (`oblisk.mpris`, docs/oblisk-supervisor-services-dbus.md §3;
//! docs/oblisk-hardware-event-pipeline.md §3; docs/oblisk-idl-api-specs.md §2.8; docs/adr/0036).
//!
//! Supervisor-owned session-bus MPRIS player discovery and zero-polling progress-sync state, so
//! `mpris.players` survives a Renderer crash/reload the same way idle/lock authority does
//! (ADR-0010). Mirrors `dbus::tray`/`dbus::bluetooth`'s shapes: hand-written `#[zbus::proxy]`
//! traits (`proxies.rs` -- no maintained zbus proxy crate for MPRIS), a `HashMap<bus_name, entry>`
//! dynamic registry hydrated live and kept live via one forwarder task per tracked player
//! (`player.rs`), a `*Controller` struct owning that registry plus write-action dispatch
//! (`controller.rs`), and pure parsing/comparison helpers unit-testable without a live D-Bus
//! connection (`metadata.rs`).
//!
//! Unlike every other capability discovered via `NameOwnerChanged`-based *removal* only
//! (`dbus::tray`'s items self-register), MPRIS players never register with anything -- discovery
//! is active (`watcher.rs`): `ListNames` scanned once at startup, then `NameOwnerChanged` watched
//! for the `org.mpris.MediaPlayer2.` prefix going forward for both arrival and departure. This
//! shape, and every other real design decision in this module (`playerctld` exclusion, album-art
//! trust-checking, track-identity caching, `SetPosition`'s `TrackId` fallback), is grounded in
//! ADR-0036 -- live introspection on this dev machine plus two independent, mature prior-art
//! sources (Quickshell's own C++ `Mpris` service, and the user's own daily-driver Quickshell
//! config).

pub mod controller;
pub mod metadata;
pub mod player;
pub mod proxies;
pub mod watcher;

pub use controller::{MprisController, MprisSignal, MprisState, parse_control_args, parse_seek_args, parse_seek_relative_args};
