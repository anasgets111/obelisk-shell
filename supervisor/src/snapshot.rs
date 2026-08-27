//! The `push_*_snapshot` family: one near-identical function per capability, each doing
//! "bump this capability's revision counter, serialize, send, record in `last_snapshots`".
//! Extracted out of `main.rs` so each new capability's own copy of this pattern doesn't keep
//! padding that file (docs/adr/0029's `revisions`/`last_snapshots` maps).

use std::collections::HashMap;

use shared::SupervisorFrame;

use crate::dbus::{bluetooth, network, notifications, tray};
use crate::hardware::{keyboard, sysinfo};
use crate::{send_frame_logged, socket};

/// Bumps and returns `capability`'s own state-version counter (ADR-0004; docs/adr/0029
/// generalizes the old single `audio_revision: u32` into this map, keyed by capability name).
/// Starts at `1` for a capability's first-ever push, matching the old `audio_revision`'s own
/// `0`-initialized-then-pre-incremented behavior.
pub(crate) fn bump_revision(revisions: &mut HashMap<String, u32>, capability: &str) -> u32 {
    let revision = revisions.entry(capability.to_string()).or_insert(0);
    *revision += 1;
    *revision
}

/// Bumps `"network"`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation -- the one place every network-capability push in `run_supervisor`'s
/// `select!` goes through, so the bump-then-serialize-then-send sequence only lives once. Also
/// records the pushed snapshot in `last_snapshots` (docs/adr/0029), the same per-capability
/// hydration map a freshly-promoted PBA candidate is seeded from.
pub(crate) fn push_network_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &network::NetworkState,
) {
    let revision = bump_revision(revisions, "network");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "network".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("network".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize network StateSnapshot: {err}"),
    }
}

/// Bumps `"bluetooth"`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation -- mirrors [`push_network_snapshot`] exactly (docs/adr/0030 needs
/// zero new plumbing beyond a fresh capability name flowing through ADR-0029's already-generic
/// `revisions`/`last_snapshots` maps).
pub(crate) fn push_bluetooth_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &bluetooth::BluetoothState,
) {
    let revision = bump_revision(revisions, "bluetooth");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "bluetooth".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("bluetooth".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize bluetooth StateSnapshot: {err}"),
    }
}

/// Bumps `"tray"`'s revision and pushes `state` as a fresh `StateSnapshot` to the authoritative
/// generation -- mirrors [`push_bluetooth_snapshot`] exactly (docs/adr/0031 needs zero new
/// plumbing beyond a fresh capability name flowing through ADR-0029's already-generic
/// `revisions`/`last_snapshots` maps).
pub(crate) fn push_tray_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &tray::TrayState,
) {
    let revision = bump_revision(revisions, "tray");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "tray".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("tray".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize tray StateSnapshot: {err}"),
    }
}

/// Bumps `"notifications"`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation -- mirrors [`push_tray_snapshot`] exactly (ADR-0033: "reuses
/// `StateSnapshot`, no new `SupervisorFrame` variant", same proven shape as tray/network/bluetooth).
pub(crate) fn push_notifications_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &notifications::NotificationsState,
) {
    let revision = bump_revision(revisions, "notifications");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "notifications".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("notifications".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize notifications StateSnapshot: {err}"),
    }
}

/// Bumps `"sysinfo"`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation -- mirrors [`push_notifications_snapshot`] exactly. `revision`
/// bumps once per push regardless of which of the three underlying tasks (cpu; ram+swap;
/// temp_cores+temp_gpu) actually changed the field that triggered it (docs/adr/0035:
/// `bump_revision`'s existing per-capability, not per-field, granularity applies unchanged
/// even with three independent producers writing into this one capability's state).
pub(crate) fn push_sysinfo_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &sysinfo::SysinfoState,
) {
    let revision = bump_revision(revisions, "sysinfo");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "sysinfo".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("sysinfo".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize sysinfo StateSnapshot: {err}"),
    }
}

/// Bumps `"keyboard"`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation -- mirrors [`push_sysinfo_snapshot`] exactly (docs/adr/0034).
/// Backlight is the only producer wired in so far; locks and layout will bump the same
/// per-capability revision through this same helper once they join.
pub(crate) fn push_keyboard_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &keyboard::KeyboardState,
) {
    let revision = bump_revision(revisions, "keyboard");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "keyboard".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("keyboard".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize keyboard StateSnapshot: {err}"),
    }
}
