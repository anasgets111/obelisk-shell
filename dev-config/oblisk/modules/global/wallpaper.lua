-- The wallpaper, which is not a capability and never was (ADR-0055): a `Background` panel holding
-- one `image`, and `lib/wallpaper.lua` saying which file. One declaration for every output, and
-- `child` is a function of the output's name (ADR-0121), so each screen draws its own file at its
-- own fit and a monitor plugged in later gets its instance without a reload.
--
-- `async` is left off, deliberately: the file decodes inside the first frame, so the frame the
-- candidate presents is whole (ADR-0122). A wallpaper change is a decode in the frame too, a
-- stall of the frame it lands in rather than a flash of the ground under it; ADR-0002's crossfade
-- is what would make it neither, and that waits on an animation model.
local wallpaper = require("lib.wallpaper")

-- All four edges anchored, so the compositor sizes both axes and this covers the output.
--
-- `"Ignore"` rather than `false`, and the difference is the whole point. Both reserve nothing, but
-- `false` still leaves the surface inside the area *other* surfaces reserved, so the moment the bar
-- claimed its 39px this shrank to 1161 and sat below it. `"Ignore"` is layer-shell's `-1`: reserve
-- nothing, ignore everyone else, cover the output. `true` would not have helped either -- a surface
-- anchored to all four edges has no single edge to reserve against, so it reads as 0 (ADR-0078).
return panel {
    id = "wallpaper",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = "Ignore",
    width = "Fill",
    height = "Fill",
    -- Painted under the image, so a source that does not decode leaves the desktop dark rather
    -- than transparent, and the failure is visible instead of looking like a surface that never
    -- mapped.
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
