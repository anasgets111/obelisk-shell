-- Mirrors AppLauncher.qml, as far as an engine with no text input can.
--
-- The list scrolls now. It did not before: a `window` clips its children to its own box and nothing
-- moved them, so entry 20 of 61 was the last one that existed as far as a user was concerned.
-- `scroll(name)` gives the column an offset the wheel writes and the layout pass clamps
-- (ADR-0069), which is the whole of it -- no viewport node, no scrollbar, no virtualization.
--
-- ponytail: 61 entries is one `list` of 61 resolved rows, re-laid out on every wheel event, at
-- about 1.2ms per pass on this machine (ADR-0069 decision 1 has the numbers). That is fine at this
-- size and stops being fine somewhere past 500. The upgrade is a `list` that builds only the rows
-- near the viewport, which is additive and needs nothing here to change.
--
-- Still missing the half that makes it a launcher rather than a list: there is no search box,
-- because `textfield` is masked-only (`renderer/src/wayland/input.rs` binds no
-- `zwp_text_input_v3`). Until that lands this opens on the full list and the wheel is the only
-- filter.
local theme = require("config.theme")
local cell = require("components.cell")
local util = require("lib.util")
local ui_state = require("lib.ui_state")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local panel_empty_state = require("components.panel_empty_state")

local SCROLL = scroll("launcher_list")

local function entries_of(applications)
    return (applications and applications.entries) or {}
end

local function app_row(app)
    local slot = "launcher-app-" .. app.id
    local hovered = hover(slot)
    return button {
        hover = hovered,
        width = "Fill",
        height = theme.launcher_row_height,
        align_v = "Center",
        radius = theme.radius.sm,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        background = hovered:map(function(is_hovered)
            return is_hovered and theme.GLASS_HOVER or nil
        end),
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            oblisk.applications:invoke("launch", app.id)
            ui_state.launcher_open:set(false)
        end,
        children = { row {
            width = "Fill",
            spacing = theme.spacing.sm,
            align_v = "Center",
            children = {
                icon { name = app.icon or "", size = theme.icon.md },
                -- `Fill` plus the elide every cell declares: a long application name is cut at the
                -- window's edge rather than painting past it, and the row stays one line.
                cell(app.name, theme.FG, theme.font.sm, { width = "Fill" }),
            },
        } },
    }
end

local app_list = list {
    width = "Fill",
    height = "Fill",
    scroll = SCROLL,
    spacing = theme.spacing.xs,
    source = oblisk.applications:map(entries_of),
    itemfn = app_row,
    key = function(app)
        return app.id
    end,
}

return window {
    id = "launcher",
    title = "Oblisk launcher",
    app_id = "oblisk.launcher",
    min_size = { width = 320, height = 240 },
    max_size = { width = 480, height = 640 },
    visible = ui_state.launcher_open,
    child = panel_card({
        panel_header("launcher", function()
            ui_state.launcher_open:set(false)
        end),
        app_list,
        -- Shown only when the scan found nothing, which is a real state rather than a defensive
        -- one: `applications` is scanned once at startup and again on `refresh`, so a config error
        -- in `$XDG_DATA_DIRS` gives an empty list and no other signal that anything went wrong.
        panel_empty_state("no applications found", util.shown_when(oblisk.applications, function(applications)
            return #entries_of(applications) == 0
        end)),
    }, {
        width = "Fill",
        height = "Fill",
        padding = { top = theme.spacing.lg, right = theme.spacing.lg, bottom = theme.spacing.lg, left = theme.spacing.lg },
        spacing = theme.spacing.sm,
        radius = 0,
    }),
}
