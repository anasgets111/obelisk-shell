-- Wallpaper is not a capability (ADR-0055): a `Background` panel with one image per output, using
-- `lib/wallpaper.lua`. `child` is keyed by output name (ADR-0121), so each screen gets its own file
-- and fit, including monitors plugged in later without a reload.
--
-- Keep `async` off: decode inside the first frame makes the presented frame whole (ADR-0122). A
-- change stalls its landing frame instead of flashing the ground; ADR-0002's crossfade awaits an
-- animation model.
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
            source = wallpaper.path_of(output),
            fit = wallpaper.fit_of(output),
            width = "Fill",
            height = "Fill",
        }
    end,
}
