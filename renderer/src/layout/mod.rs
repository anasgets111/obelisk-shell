//! The one-pass layout engine and retained scene (build-steps.md Phase 12,
//! `docs/oblisk-layout-engine-geometry.md` § 3-5, `CONTEXT.md`'s "Retained scene"/
//! "Retained-scene transaction"/"Lease" entries).
//!
//! `node.rs` turns a `lua::VirtualNode`'s raw, untyped properties into the typed geometry values
//! this module's constraint/size/position passes need, and resolves the `Signal` handles among
//! them once per node per pass (`node::resolve_properties`, build-steps.md Phase 19 items 1 and
//! 5). `scene.rs` runs those passes and owns the `Scene`: the persistent node tree one
//! generation's `Loader::evaluate` output is reconciled into on every `apply`, instead of being
//! rebuilt from scratch.
//!
//! Scope ceilings recorded in docs/adr/0023, minus the ones later phases lifted: `list` is an
//! unsupported node kind (`textfield` became one in Phase 15, and a `Signal` in a geometry
//! property now resolves rather than being rejected -- docs/adr/0044 decision 1); `rect`/`button`/
//! `panel` containers use a stacking positioning model with no formula in § 3.2; overlay
//! input-region computation is pure but not yet wired to a live `wl_surface::set_input_region`
//! call; the lease/retiring mechanism has no real GPU resource to guard yet.

pub mod node;
pub mod paint;
pub mod scene;

// ponytail: this is the module's real, tested public surface, but `socket.rs`'s `RendererClient`
// (the only caller outside `layout` today) only needs `Scene`/`LogicalSize`, below, to call
// `apply`/`surface`. `LayoutError`'s variants get a real external caller once something matches
// on them instead of just logging the `Display` output; `NodeId`/`ResolvedNode` are already
// exercised via `Scene::surface`'s return type but never named directly outside this module;
// `overlay_input_regions` is now reachable from the `wl_region` call site (docs/adr/0039 put the
// scene and the surfaces on one thread) and waits only on its own slice, build-steps.md Phase 20.
#[allow(unused_imports)]
pub use node::{Align, EdgeInsets, LayoutError, SizeMode};
pub use scene::{LogicalSize, Scene};
#[allow(unused_imports)]
pub use scene::{NodeId, ResolvedNode, overlay_input_regions};
