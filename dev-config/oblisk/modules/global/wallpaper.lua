-- Wallpaper is not a capability (ADR-0055): a `Background` panel with one image per output, using
-- `lib/wallpaper.lua`. `child` is keyed by output name (ADR-0121), so each screen gets its own file
-- and fit, including monitors plugged in later without a reload.
--
-- `async` with `transition` (ADR-0180, ADR-0181): the decode leaves the render thread, the picture
-- already up holds the screen until the replacement is ready, and then the two cross rather than
-- swapping in one frame. ADR-0122 kept `async`
-- off here because a pending image drew nothing and a change flashed the ground; ADR-0179 measured
-- what that bought -- 162.7ms of decode on the render thread at every change, 114ms in release --
-- and `retain` is what removes the flash the stall was paying for.
local wallpaper = require("lib.wallpaper")

-- Anchor all four edges so the compositor sizes both axes over the output.
--
-- Use `"Ignore"`, not `false`. Both reserve nothing, but `false` respects other reservations, so a
-- 39px bar shrank this to 1161px and placed it below. Layer-shell `-1` ignores them and covers the
-- output. `true` also reads as 0 for an all-edge surface with no single edge (ADR-0078).
return panel {
    id = "wallpaper",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = "Ignore",
    width = "Fill",
    height = "Fill",
    -- Under the image: a failed decode leaves the desktop dark, not transparent, so the failure is
    -- visible instead of looking like an unmapped surface.
    background = "#11111bff",
    child = function(output)
        return image {
            -- `retain` holds the last picture across a source change, so the node has to be the
            -- same node across it: a stable `id`, with the path in `source` rather than in the
            -- identity.
            id = "wallpaper_image",
            source = wallpaper.path_of(output),
            fit = wallpaper.fit_of(output),
            async = true,
            -- `transition` implies `retain` (ADR-0181), so the hold and the cross are one
            -- declaration. The effect is a shader file in this directory, not a name the engine
            -- knows (ADR-0184); `lib/wallpaper.lua` randomises its parameters per change the way
            -- `AnimatedWallpaper.qml` does.
            transition = wallpaper.transition_for("wipe"),
            width = "Fill",
            height = "Fill",
        }
    end,
}
