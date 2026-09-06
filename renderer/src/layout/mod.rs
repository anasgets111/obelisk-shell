//! Retained layout scene, per `CONTEXT.md`'s "Retained scene", "Retained-scene transaction", and
//! "Lease" entries. `taffy` 0.14 owns sizing/positioning; this module owns identity and
//! reconcile, child-first leases, the depth cap, once-per-node resolution, scroll writeback, and
//! text elision (ADR-0077). `node` parses/resolves properties; `scene` applies them to the
//! persistent tree: each generation's `Loader::evaluate` output reconciles into it on `apply`, not
//! from scratch. Stacking is a one-cell `Display::Grid` with independently aligned children
//! and a bounding-union container (ADR-0023); retiring has no GPU resource to guard yet.

pub mod hit;
pub mod hover;
pub mod instance;
pub mod node;
pub mod paint;
pub mod scene;
pub mod secure_submit;

// ponytail: this is the tested public surface, but external callers still mostly need
// `Scene`/`LogicalSize`. Upgrade path: expose the remaining re-exports when callers match
// `LayoutError` or name `NodeId` directly; `wayland` already names `ResolvedNode` and
// `overlay_input_regions` for input regions.
#[allow(unused_imports)]
pub use node::{Align, EdgeInsets, LayoutError, SizeMode};
#[allow(unused_imports)]
pub use scene::NodeId;
pub use scene::{LogicalSize, ResolvedNode, Scene, overlay_input_regions};
