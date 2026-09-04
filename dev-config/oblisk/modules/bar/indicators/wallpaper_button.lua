-- `Modules/Bar/Indicators/WallpaperButton.qml`: one glyph, left click opens the picker, right
-- click deals every screen a new file from the folder without opening anything.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local ui_state = require("lib.ui_state")
local wallpaper = require("lib.wallpaper")
local tooltip = require("components.tooltip")

local SLOT = "wallpaper"

local wallpaper_button = icon_button(icons.wallpaper, nil, {
    slot = SLOT,
    selected = ui_state.wallpaper_picker_open,
    on_button = function(_, mouse_button)
        if mouse_button == "left" then
            ui_state.wallpaper_picker_open:set(not ui_state.wallpaper_picker_open:get())
        elseif mouse_button == "right" then
            wallpaper.randomize_all()
        end
    end,
})

local wallpaper_tooltip = tooltip({
    id = "wallpaper_tooltip",
    slot = SLOT,
    width = 250,
    height = 44,
    children = {
        cell("open the wallpaper picker", theme.FG, theme.font.sm),
        cell("right-click for a random one everywhere", theme.DIM, theme.font.xs),
    },
})

return { button = wallpaper_button, tooltip = wallpaper_tooltip }
