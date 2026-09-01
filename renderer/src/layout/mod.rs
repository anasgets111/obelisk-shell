//! The layout engine and retained scene (`CONTEXT.md`'s "Retained scene"/"Retained-scene
//! transaction"/"Lease" entries). `taffy` 0.14 owns sizing and positioning; this module keeps node
//! identity and reconcile, the lease and child-first teardown, the depth cap, once-per-node
//! property resolution, the scroll clamp and writeback, and text elision (ADR-0077).
//!
//! `node.rs` turns a `lua::VirtualNode`'s raw properties into typed geometry values and resolves
//! `Signal` handles once per node per pass (`node::resolve_properties`). `scene.rs` runs those
//! passes and owns the `Scene`: the persistent node tree each generation's `Loader::evaluate`
//! output reconciles into on `apply`, rather than being rebuilt from scratch.
//!
//! The stacking positioning model (ADR-0023) is a `Display::Grid` with every child pinned to row
//! 1/column 1, one auto-sized cell, each child aligned independently, and the container sized to
//! their bounding union (ADR-0077); the lease/retiring mechanism has no real GPU resource to guard
//! yet.

pub mod hit;
pub mod hover;
pub mod instance;
pub mod node;
pub mod paint;
pub mod scene;
pub mod secure_submit;

// ponytail: this is the module's real, tested public surface, but `socket.rs`'s `RendererClient`
// (the only caller outside `layout` today) only needs `Scene`/`LogicalSize`, below, to call
// `apply`/`surface`. `LayoutError`'s variants get a real external caller once something matches
// on them instead of just logging the `Display` output; `NodeId`/`ResolvedNode` are already
// exercised via `Scene::surface`'s return type but never named directly outside this module;
// `ResolvedNode` and `overlay_input_regions` are named directly by `crate::wayland`, which pushes
// input regions per surface; `NodeId` is still only exercised through `Scene`'s own API.
#[allow(unused_imports)]
pub use node::{Align, EdgeInsets, LayoutError, SizeMode};
#[allow(unused_imports)]
pub use scene::NodeId;
pub use scene::{LogicalSize, ResolvedNode, Scene, overlay_input_regions};
