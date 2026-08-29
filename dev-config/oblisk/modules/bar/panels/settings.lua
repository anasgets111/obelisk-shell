local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local sysinfo_module = require("modules.bar.indicators.system_info")

-- A real `xdg_toplevel`, opened and closed by `menu.lua`'s button. The compositor places and sizes
-- this, not the config: § 6.2 gives a `window` no `monitor`, no `anchor` and no size, so
-- `niri msg windows` is where you check that the title and app_id arrived.
return window {
    id = "settings",
    title = "Oblisk settings",
    app_id = "oblisk.settings",
    min_size = { width = 320, height = 240 },
    max_size = { width = 1280, height = 800 },
    visible = ui_state.settings_open,
    child = column {
        -- Fills whatever the compositor configured, so the window is opaque and takes clicks
        -- across its whole area. A `Content`-sized child under a tiling compositor would leave
        -- most of the surface transparent and, since the input region is the visible content
        -- (ADR-0038 decision 5), click-through.
        width = "Fill",
        height = "Fill",
        padding = { top = 14, right = 14, bottom = 14, left = 14 },
        spacing = 8,
        background = theme.BG,
        children = {
            cell("oblisk settings", theme.FG, 16),
            cell(util.label(oblisk.system, function(s)
                return "up since " .. os.date("%H:%M:%S", s.time)
            end), theme.DIM, 11),
            cell(util.label(oblisk.audio, function(a)
                return string.format("%d playback stream(s)", util.count(a.apps))
            end), theme.DIM, 11),
            cell(util.label(oblisk.screens, function(s)
                return string.format("%d output(s)", #s)
            end), theme.DIM, 11),
            sysinfo_module,
        },
    },
}
