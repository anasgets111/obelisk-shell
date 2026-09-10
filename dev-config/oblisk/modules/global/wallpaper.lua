-- Wallpaper is not a capability (ADR-0055): a `Background` panel with one image per output, using
-- `lib/wallpaper.lua`. `child` is keyed by output name (ADR-0121), so each screen gets its own file
-- and fit, including monitors plugged in later without a reload.
--
-- `async` with `transition` (ADR-0180, ADR-0181) decodes off the render thread, holds the current
-- image until replacement is ready, then cross-fades. ADR-0122 kept `async` off because a pending
-- image drew nothing and changes flashed the ground. ADR-0179 measured 162.7ms of render-thread
-- decode at every change and 114ms in release; `retain` removes the flash without that stall.
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
            -- `retain` holds the last picture across a source change, so keep the node's `id`
            -- stable and put the path in `source`, not its identity.
            id = "wallpaper_image",
            source = wallpaper.path_of(output),
            fit = wallpaper.fit_of(output),
            async = true,
            -- `transition` implies `retain` (ADR-0181): one declaration holds the old picture and
            -- crosses with the selected `.frag` from `wallpaper.SHADER_FOLDER`, not an engine-known
            -- name (ADR-0184).
            transition = wallpaper.transition(),
            width = "Fill",
            height = "Fill",
        }
    end,
}
