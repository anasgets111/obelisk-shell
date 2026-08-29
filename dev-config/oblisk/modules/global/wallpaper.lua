-- The wallpaper, which is not a capability and never was (docs/adr/0055). Everything
-- `wallpaper:set(mon, path, fit, anim, dur)` was going to carry already had a home once ADR-0038
-- moved surface declaration here and Phase 21 built `state`: the monitor is `panel.monitor`, the
-- path is `image.source`, the fit is `image.fit`, and the two animation arguments need an animation
-- model this engine does not have. Changing it at runtime is `wallpaper:set(path)` on the signal
-- below, with no IPC anywhere in the path.
--
-- `oblisk.config_dir` is what lets this name a file it ships beside itself. It stays a `state`
-- signal rather than a constant so the runtime path is the one being exercised, not a literal that
-- happens to work at boot.
local wallpaper = state("wallpaper", oblisk.config_dir .. "/wallpaper.svg")

-- All four edges anchored, so the compositor sizes both axes and this covers the output. Not
-- exclusive: a wallpaper that reserved screen area would push every other surface off the
-- screen it is behind.
return panel {
    id = "wallpaper",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = false,
    width = "Fill",
    height = "Fill",
    -- Painted under the image, so a source that does not decode leaves the desktop dark rather
    -- than transparent, and the failure is visible instead of looking like a surface that never
    -- mapped.
    background = "#11111bff",
    child = image {
        source = wallpaper,
        fit = "cover",
        width = "Fill",
        height = "Fill",
    },
}
