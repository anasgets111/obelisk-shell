# Wallpaper surface skips transitions on reload and queues overlapping sets

The wallpaper surface (ADR-0007) paints its current texture directly on candidate first-frame and on in-place reload: no shader transition. Transitions run only in response to a live `wallpaper:set()` call. If `wallpaper:set()` is called again while a transition is already in flight, the new target queues and replays once the current transition finishes, rather than interrupting the running shader or stomping the in-flight texture. The second texture buffer used for a transition is allocated only for the transition's duration and released immediately after, not held permanently per surface.

This mirrors a working implementation already in production (Quickshell `AnimatedWallpaper.qml`): `Component.onCompleted` sets the image source with no shader involved, the transition `Loader` is `active` only while a transition runs, and overlapping `changeWallpaper()` calls queue via `pendingUrl` instead of racing. Adopted directly rather than re-derived, since the failure modes (double-buffer left permanently allocated, transitions racing/tearing on rapid wallpaper switches) are exactly what this pattern avoids.

## Amendment: the no-transition branch is what shipped, per ADR-0055

Wallpaper is now an `image` node on a config-declared `Background` panel (ADR-0055), and it paints a
new texture directly with no transition. That is this ADR's first-frame and reload branch, built as
written. The transition branch is unbuilt and stays specified: there is no animation model in the
engine at all (`build-steps.md`, "The missing animation model"), so there is no shader for the queue
to arbitrate between and no second buffer to release. Nothing here is retracted. `wallpaper:set()`
as a capability call is retracted, by ADR-0055 decision 1; read every mention of it below as the
config writing to the `state()` signal bound to `image.source`.
