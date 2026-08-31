-- Mirrors the bare `IconButton` LeftSide.qml gives the launcher, which calls `IPC.toggleLauncher()`.
-- One bar button, one write, no capability underneath it. Toggling rather than only opening, so
-- the same button that raised `modules/global/launcher.lua` can lower it without reaching for its
-- close button.
local theme = require("config.theme")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local tooltip = require("components.tooltip")

local launcher_button = button {
    width = 46,
    height = 24,
    background = theme.SURFACE,
    radius = 6,
    on_click = function(_, mouse_button)
        if mouse_button ~= "left" then
            return
        end
        local opening = not ui_state.launcher_open:get()
        if opening then
            -- Rescan on open rather than watching the applications directories: a package
            -- installed mid-session is the only thing that changes this list, and one directory
            -- read when a launcher opens is cheaper than an inotify watch held all session for an
            -- event that arrives a handful of times a month (docs/adr/0061 decision 4). The scan
            -- runs off-thread and only pushes when the result actually differs, so reopening the
            -- launcher repeatedly costs nothing after the first time.
            oblisk.applications:invoke("refresh")
        end
        ui_state.launcher_open:set(opening)
    end,
    children = { cell("apps", theme.ACCENT) },
}

-- Quickshell's version of this button carries `tooltipText: "Open application launcher"`, which is
-- the plainest use a tooltip has: a label for a control whose own text is too short to explain it.
local SLOT = "launcher"
launcher_button.hover = hover(SLOT)

local launcher_tooltip = tooltip({
    id = "launcher_tooltip",
    slot = SLOT,
    width = 190,
    height = 44,
    children = {
        cell("open the app launcher", theme.FG, 12),
        cell(oblisk.applications:map(function(applications)
            local entries = applications and applications.entries
            return string.format("%d application(s)", entries and #entries or 0)
        end), theme.DIM, 11),
    },
})

return { button = launcher_button, tooltip = launcher_tooltip }
