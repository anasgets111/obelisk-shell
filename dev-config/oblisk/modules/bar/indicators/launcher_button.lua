-- Mirrors the `IconButton` LeftSide.qml puts between the battery and the workspaces: one glyph,
-- circular, opening the launcher.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local ui_state = require("lib.ui_state")
local tooltip = require("components.tooltip")

local SLOT = "launcher"

local launcher_button = icon_button(icons.launcher, function()
    -- Refreshing on the way open rather than on a timer, because the enumeration is a directory
    -- walk and nothing outside this click cares whether it is current (ADR-0061).
    local opening = not ui_state.launcher_open:get()
    if opening then
        oblisk.applications:invoke("refresh")
    end
    ui_state.launcher_open:set(opening)
end, { slot = SLOT, selected = ui_state.launcher_open })

local launcher_tooltip = tooltip({
    id = "launcher_tooltip",
    slot = SLOT,
    width = 190,
    height = 44,
    children = {
        cell("open the app launcher", theme.FG, theme.font.sm),
        cell(oblisk.applications:map(function(applications)
            local entries = applications and applications.entries
            return string.format("%d application(s)", entries and #entries or 0)
        end), theme.DIM, theme.font.xs),
    },
})

return { button = launcher_button, tooltip = launcher_tooltip }
