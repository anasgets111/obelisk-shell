//! Parsing one evaluation's declared surfaces into the per-role specs the rest of the Renderer
//! resolves against, and the at-most-one-`lock` rule that is a property of the surface *set*.
//!
//! Here rather than in `crate::layout::node` beside the per-role parsers it calls: the input is a
//! [`LoadOutput`] and the error is a [`LoaderError`], both this module's, and `crate::layout` has
//! no production dependency on `crate::lua` to spend on the other direction.

use std::path::Path;

use crate::layout::{self, node::SurfaceSpec};
use crate::lua::{LoadOutput, Loader, LoaderError};

/// Parses **every** declared surface by its own role and returns the whole roster (§ 6).
/// A surface whose fields don't type-check fails with [`LoaderError::InvalidTopology`], a distinct
/// message from a top-level-return shape error.
///
/// **Parsing every role here is the point, even for properties nothing on this path sends.**
/// § 6's properties become requests that raise protocol errors -- a zero `anchor_rect`
/// answers `invalid_positioner`, a `max_size` under a `min_size` answers `invalid_size` -- and a
/// protocol error kills the whole connection. A config typo must be a `layout::node::LayoutError`
/// at evaluation instead, landing in `rescue`'s `error_log` (§ 2.10, ADR-0046).
///
/// **This is an evaluation-time, literal-only fast-fail, not the authoritative spec** for the two
/// roles whose properties are meant to move (ADR-0049's second amendment): a `Signal` in a
/// `window`'s `title` or a `popup`'s `anchor_rect` is **skipped** rather than rejected
/// (`layout::node::is_deferred_signal`), carrying that parser's placeholder in its place; a
/// *literal* is validated here in full. `crate::wayland::App::apply_resolved_state` builds the
/// authoritative [`WindowSpec`](layout::node::WindowSpec)/[`PopupSpec`](layout::node::PopupSpec)
/// from the *resolved* tree instead (ADR-0044 decision 1); resolving here too would double
/// ADR-0021's per-getter budget on every monitor hotplug via
/// [`RendererClient::applied_surface_specs`](crate::socket::RendererClient::applied_surface_specs).
///
/// What *is* authoritative here is the roster and the fingerprint: which surfaces were declared,
/// in what order, with what role. **§ 6's `lock` is authoritative here in full, and it is the
/// only role that is** (ADR-0052 decision 2): `id` is structural and `child` is the scene's
/// to walk, so [`lock_spec`](layout::node::lock_spec) consults `is_deferred_signal` nowhere.
pub(crate) fn surface_specs(output: &LoadOutput) -> Result<Vec<SurfaceSpec>, LoaderError> {
    let invalid = |err: layout::node::LayoutError| LoaderError::InvalidTopology(err.to_string());
    let mut specs = Vec::with_capacity(output.surfaces.len());
    for surface in &output.surfaces {
        specs.push(match surface.kind.as_str() {
            "panel" => SurfaceSpec::Panel(layout::node::panel_spec(&surface.properties).map_err(invalid)?),
            "window" => SurfaceSpec::Window(layout::node::window_spec(&surface.properties).map_err(invalid)?),
            "popup" => SurfaceSpec::Popup(layout::node::popup_spec(&surface.properties).map_err(invalid)?),
            "lock" => SurfaceSpec::Lock(layout::node::lock_spec(&surface.properties).map_err(invalid)?),
            // Unreachable: `lua::require_surface` admits exactly the four § 6 roles above and
            // rejects everything else. Named rather than left to a silent `_ => {}`, because that
            // arm would let a fifth role reach a generation unvalidated.
            other => return Err(LoaderError::InvalidTopology(format!("`{other}` is not a surface role"))),
        });
    }
    // **At most one `lock` in a config, and this is the only place that can say so.** Every other
    // § 6 role may be declared any number of times, so the check is a property of the surface
    // *set*, and this is the one function every declaration passes through on both the startup
    // path and the `Reevaluate` path.
    //
    // The failure it prevents is unrecoverable rather than cosmetic.
    // `layout::instance::expand_instances` emits one instance per lock spec per output, so two
    // declarations make `crate::wayland::App::ensure_lock_surfaces` send two `get_lock_surface`
    // for the same `wl_output`, and `ext-session-lock-v1` is explicit: "Attempting to create more
    // than one lock surface for a given output is a duplicate_output protocol error." The
    // compositor disconnects the client and does not unlock the session, leaving the user with a
    // VT switch as the only way back in.
    //
    // There is also nothing coherent to admit: § 6 gives a `lock` no `monitor` and exactly one
    // surface per output, so "two lock screens" names no arrangement a compositor could show.
    let locks = specs.iter().filter(|spec| matches!(spec, SurfaceSpec::Lock(_))).count();
    if locks > 1 {
        return Err(LoaderError::InvalidTopology(format!(
            "this config declares {locks} `lock` surfaces; § 6.4 gives a `lock` no `monitor` and exactly one surface per output, so a config may \
             declare at most one -- a second would ask the compositor for two lock surfaces on one output, which is `duplicate_output`, which kills \
             the connection with the session still locked"
        )));
    }
    Ok(specs)
}

// ponytail: this top-level evaluation is uncapped, unlike a `computed`/`map` closure's 5ms hook
// (ADR-0021). ADR-0039 accepts this: a slow evaluation blocks the Wayland dispatch
// thread it runs on, with no configure handling and no way to set `app.exit` until it returns --
// `while true do end` in `shell.lua` wedges the whole process. Upgrade path: extend ADR-0021's
// hook to cover `Loader::evaluate_file` itself, not just the closures it registers.
pub(crate) fn evaluate_and_specs(
    loader: &Loader,
    shell_lua_path: &Path,
) -> Result<(LoadOutput, Vec<SurfaceSpec>), LoaderError> {
    let output = loader.evaluate_file(shell_lua_path)?;
    let specs = surface_specs(&output)?;
    Ok((output, specs))
}
