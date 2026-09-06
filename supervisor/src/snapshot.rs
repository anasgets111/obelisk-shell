//! Snapshot-push path (ADR-0037): bump the capability revision, serialize, send to the
//! authoritative generation, and record in `last_snapshots`. Replaces per-capability functions
//! (ADR-0029's `revisions`/`last_snapshots` maps).

use std::collections::HashMap;

use shared::{Capability, SupervisorFrame};

use crate::{send_frame_logged, socket};

/// Bumps and returns `capability`'s state-version counter (ADR-0004; ADR-0029's name-keyed map).
/// First push is `1`.
pub(crate) fn bump_revision(revisions: &mut HashMap<String, u32>, capability: Capability) -> u32 {
    let revision = revisions.entry(capability.to_string()).or_insert(0);
    *revision += 1;
    *revision
}

/// Bumps the revision, pushes `state` as a fresh `StateSnapshot`, and records it in
/// `last_snapshots` (ADR-0029), which seeds a promoted PBA candidate.
///
/// ADR-0037's `&[&str]` roster check was a `debug_assert`. Taking [`Capability`] makes off-roster
/// names unrepresentable (ADR-0076).
pub(crate) fn push_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    capability: Capability,
    state: &impl serde::Serialize,
) {
    let revision = bump_revision(revisions, capability);
    match serde_json::to_value(state) {
        Ok(payload) => {
            // Move the snapshot through the frame and take it back out. `send_frame_logged`
            // borrows, so the obvious spelling deep-clones the whole `payload` tree -- the largest
            // thing on this path -- on every signal, purely to keep a copy.
            let frame = SupervisorFrame::StateSnapshot(shared::StateSnapshot {
                capability: capability.to_string(),
                revision,
                payload,
            });
            send_frame_logged(registry, generation_id, &frame);
            if let SupervisorFrame::StateSnapshot(snapshot) = frame {
                last_snapshots.insert(capability.to_string(), snapshot);
            }
        }
        Err(err) => eprintln!("failed to serialize {capability} StateSnapshot: {err}"),
    }
}
