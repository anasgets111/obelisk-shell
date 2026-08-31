//! The one-pass layout engine and retained scene (build-steps.md Phase 12,
//! `docs/oblisk-layout-engine-geometry.md` § 3-5, `CONTEXT.md`'s "Retained scene"/
//! "Retained-scene transaction"/"Lease" entries).
//!
//! `node.rs` turns a `lua::VirtualNode`'s raw properties into typed geometry values and resolves
//! `Signal` handles once per node per pass (`node::resolve_properties`). `scene.rs` runs those
//! passes and owns the `Scene`: the persistent node tree each generation's `Loader::evaluate`
//! output reconciles into on `apply`, rather than being rebuilt from scratch.
//!
//! Scope ceilings from docs/adr/0023: `rect`/`button`/`panel` containers use a stacking
//! positioning model with no formula in § 3.2; the lease/retiring mechanism has no real GPU
//! resource to guard yet.

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
// `ResolvedNode` and `overlay_input_regions` are named directly by `crate::wayland` since
// build-steps.md Phase 20 item 5 wired the input-region push per surface; `NodeId` is still only
// exercised through `Scene`'s own API.
#[allow(unused_imports)]
pub use node::{Align, EdgeInsets, LayoutError, SizeMode};
pub use scene::{LogicalSize, ResolvedNode, Scene, overlay_input_regions};
#[allow(unused_imports)]
pub use scene::NodeId;
