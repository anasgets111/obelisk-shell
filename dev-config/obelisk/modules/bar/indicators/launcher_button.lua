-- Mirrors LeftSide.qml's circular `IconButton` between the battery and workspaces.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local ui_state = require("lib.ui_state")
local tooltip = require("components.tooltip")

local SLOT = "launcher"

local launcher_button = icon_button(icons.launcher, function()
    -- Refresh on open, not a timer: enumeration walks a directory and only this click needs it
    -- current (ADR-0061).
    if not ui_state.launcher_open:get() then
        obelisk.applications:invoke("refresh")
    end
    ui_state.toggle_modal("launcher")
end, { slot = SLOT, selected = ui_state.launcher_open })

local launcher_tooltip = tooltip({
    id = "launcher_tooltip",
    slot = SLOT,
    children = {
        cell("open the app launcher", theme.FG, theme.font.sm),
        cell(obelisk.applications:map(function(applications)
            local entries = applications and applications.entries
            return string.format("%d application(s)", entries and #entries or 0)
        end), theme.DIM, theme.font.xs),
    },
})

return { button = launcher_button, tooltip = launcher_tooltip }
