//! The one-pass layout engine and retained scene (build-steps.md Phase 12,
//! `docs/oblisk-layout-engine-geometry.md` § 3-5, `CONTEXT.md`'s "Retained scene"/
//! "Retained-scene transaction"/"Lease" entries).
//!
//! `node.rs` turns a `lua::VirtualNode`'s raw, untyped properties into the typed geometry values
//! this module's constraint/size/position passes need. `scene.rs` runs those passes and owns the
//! `Scene`: the persistent node tree one generation's `Loader::evaluate` output is reconciled
//! into on every `apply`, instead of being rebuilt from scratch.
//!
//! Scope ceilings recorded in docs/adr/0023: `list`/`textfield` are unsupported node kinds; a
//! `Signal` landing in a geometry property is rejected, not auto-resolved; `rect`/`button`/
//! `surface` containers use a stacking positioning model with no formula in § 3.2; overlay
//! input-region computation is pure but not yet wired to a live `wl_surface::set_input_region`
//! call; the lease/retiring mechanism has no real GPU resource to guard yet.

pub mod node;
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
