//! The one snapshot-push path (ADR-0037): bump the capability's revision counter, serialize,
//! send to the authoritative generation, record in `last_snapshots`. Replaces the previous
//! one-near-identical-function-per-capability family (docs/adr/0029's `revisions`/
//! `last_snapshots` maps) -- the capability name was the only real datum ever varying.

use std::collections::HashMap;

use shared::SupervisorFrame;

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

/// Bumps `capability`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation, recording it in `last_snapshots` (docs/adr/0029), the same
/// per-capability hydration map a freshly-promoted PBA candidate is seeded from. The
/// `debug_assert` is ADR-0037's roster check: a capability pushed here but missing from
/// `shared::CAPABILITIES` would boot with no pre-seeded Lua global on the Renderer side.
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
