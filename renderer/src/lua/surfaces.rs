//! Parses one evaluation's surfaces into per-role specs and enforces the surface-set
//! at-most-one-`lock` rule.
//!
//! Kept here, not beside the per-role parsers in `crate::layout::node`: input is [`LoadOutput`],
//! error is [`LoaderError`], and `crate::layout` has no production dependency on `crate::lua`.

use std::path::Path;

use crate::layout::{self, node::SurfaceSpec};
use crate::lua::{LoadOutput, Loader, LoaderError};

/// Parses every declared § 6 role and returns the roster. Field type errors are
/// [`LoaderError::InvalidTopology`], distinct from top-level shape errors.
///
/// **Parse every role, including unused-path properties.** § 6 violations become protocol errors
/// (`invalid_positioner` for zero `anchor_rect`, `invalid_size` for `max_size < min_size`) that
/// kill the connection. Catch config typos as `layout::node::LayoutError` during evaluation, in
/// `rescue.error_log` (§ 2.10, ADR-0046).
///
/// **Literal fast-fail only, not the authoritative spec** for moving properties (ADR-0049 decision
/// 2): a `Signal` in `window.title` or `popup.anchor_rect` is skipped via
/// `layout::node::is_deferred_signal` and replaced by its parser placeholder; literals are fully
/// checked. `App::apply_resolved_state` builds authoritative `WindowSpec`/`PopupSpec` from the
/// resolved tree (ADR-0044 decision 1); resolving here would double ADR-0021's getter budget on
/// every monitor hotplug through `RendererClient::applied_surface_specs`.
///
/// Authoritative here: roster fingerprint, order, and roles. **`lock` is fully authoritative and
/// the only role that is** (ADR-0052 decision 2): `id` is structural and `child` belongs to the
/// scene, so
/// [`lock_spec`](layout::node::lock_spec) never consults `is_deferred_signal`.
pub(crate) fn surface_specs(output: &LoadOutput) -> Result<Vec<SurfaceSpec>, LoaderError> {
    let invalid = |err: layout::node::LayoutError| LoaderError::InvalidTopology(err.to_string());
    let mut specs = Vec::with_capacity(output.surfaces.len());
    for surface in &output.surfaces {
        specs.push(match surface.kind.as_str() {
            "panel" => SurfaceSpec::Panel(layout::node::panel_spec(&surface.properties).map_err(invalid)?),
            "window" => SurfaceSpec::Window(layout::node::window_spec(&surface.properties).map_err(invalid)?),
            "popup" => SurfaceSpec::Popup(layout::node::popup_spec(&surface.properties).map_err(invalid)?),
            "lock" => SurfaceSpec::Lock(layout::node::lock_spec(&surface.properties).map_err(invalid)?),
            // `lua::require_surface` admits exactly four roles; keep this arm explicit so a fifth
            // cannot reach a generation unvalidated.
            other => return Err(LoaderError::InvalidTopology(format!("`{other}` is not a surface role"))),
        });
    }
    // **At most one `lock`, checked here on startup and `Reevaluate`.** This is the only place
    // that can enforce it: every declaration passes through this function on both paths. Other § 6
    // roles may repeat.
    // Two locks make `expand_instances` produce two per-output instances and
    // `App::ensure_lock_surfaces` send two `get_lock_surface` requests. ext-session-lock-v1 calls
    // this `duplicate_output`; the compositor disconnects without unlocking, leaving only a VT
    // switch.
    // § 6 gives `lock` no monitor and one surface per output, so two screens have no valid layout.
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

// ponytail: top-level evaluation is uncapped, unlike a `computed`/`map` 5ms hook (ADR-0021). A
// slow evaluation blocks Wayland dispatch (ADR-0039), configure handling, and `app.exit`; `while
// true do end` wedges the process. Upgrade by extending ADR-0021's hook over
// `Loader::evaluate_file`.
pub(crate) fn evaluate_and_specs(
    loader: &Loader,
    shell_lua_path: &Path,
) -> Result<(LoadOutput, Vec<SurfaceSpec>), LoaderError> {
    let output = loader.evaluate_file(shell_lua_path)?;
    let specs = surface_specs(&output)?;
    Ok((output, specs))
}
