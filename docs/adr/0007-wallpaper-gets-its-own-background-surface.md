# Wallpaper gets its own Background-layer surface

> Amended by ADR-0038. The paint-order reasoning below stands unchanged and wallpaper still gets
> its own `Background`-layer surface. What changed is who declares it: `shell.lua`, not a Rust-owned
> role. Read "a third static surface" here as "a third surface in the default config". That this
> ADR had to add a role to a supposedly fixed set is the evidence ADR-0038 builds on.

`oblisk-supervisor-services-dbus.md` §14 fixes exactly two static surfaces: `main_bar` (layer `Top`) and `overlay_canvas` (layer `Overlay`). Wallpaper rendering (§8) has no surface to live on. Considered painting it inside `overlay_canvas` since that surface already spans the whole screen. Rejected: `Overlay` is the topmost layer in wlr-layer-shell stacking, above every application window, by protocol definition, not by z-order the shell controls. Wallpaper content drawn there would cover the desktop instead of sitting behind it, and no input-region trick changes that: the problem is paint order, not click routing.

Decision: add a third static surface, `wallpaper_layer` (`Background` layer, non-exclusive, one per monitor), owned separately from the two UI surfaces. See the **Wallpaper surface** term in `CONTEXT.md` and ADR-0002 for its reload and transition behavior.
