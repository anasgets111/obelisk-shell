//! The one snapshot-push path (ADR-0037): bump the capability's revision counter, serialize,
//! send to the authoritative generation, record in `last_snapshots`. Replaces a previous
//! one-function-per-capability family (docs/adr/0029's `revisions`/`last_snapshots` maps).

use std::collections::HashMap;

use shared::SupervisorFrame;

use crate::{send_frame_logged, socket};

/// Bumps and returns `capability`'s own state-version counter (ADR-0004; docs/adr/0029
/// generalizes this into a map keyed by capability name). Starts at `1` for a first push.
pub(crate) fn bump_revision(revisions: &mut HashMap<String, u32>, capability: &str) -> u32 {
    let revision = revisions.entry(capability.to_string()).or_insert(0);
    *revision += 1;
    *revision
}

/// Bumps `capability`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation, recording it in `last_snapshots` (docs/adr/0029), the map a
/// freshly-promoted PBA candidate is seeded from. `debug_assert` is ADR-0037's roster check.
pub(crate) fn push_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    capability: &str,
    state: &impl serde::Serialize,
) {
    debug_assert!(
        shared::CAPABILITIES.contains(&capability),
        "capability {capability:?} pushes snapshots but is missing from shared::CAPABILITIES (ADR-0037)"
    );
    let revision = bump_revision(revisions, capability);
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: capability.to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert(capability.to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize {capability} StateSnapshot: {err}"),
    }
}
