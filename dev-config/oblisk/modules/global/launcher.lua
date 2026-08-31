-- Mirrors Global/AppLauncher.qml.
--
-- The app launcher: a real `xdg_toplevel` listing every installed `.desktop` entry, click to run.
--
-- Reads `oblisk.applications` (docs/adr/0061) rather than scanning `/usr/share/applications` from
-- Lua. The first version of this file did the scan itself, with `process.run("sh", {"-c", grep})`
-- and an `Exec=` parser written in Lua, which is what the capability replaced: the shell-out
-- reparsed every entry on each open, could not tell `Exec=foo "a b"` from three arguments, and
-- had no way to reach a `Terminal=true` entry at all.
--
-- The argv never arrives here. `applications:launch(id)` runs the entry the id names, and the
-- capability keeps the parsed command line on its own side (ADR-0061 decision 3), so this file
-- has nothing to assemble and nothing to get wrong.
--
-- Still no search field. § 5.2 item 8's `textfield` declares `on_change`/`on_submit`, but
-- `renderer/src/wayland/input.rs` says the unmasked half of that design is recorded and not built
-- -- its `zwp_text_input_v3` binding "is gone entirely" -- so nothing delivers committed text to
-- Lua. A box that swallowed keystrokes and filtered nothing would look real and lie.
--
-- One `list`, which lays out exactly like a `column` and has no viewport (§ 5.2 item 7), so a
-- longer list than this window clips rather than scrolls. That is the same ceiling
-- `modules/bar/indicators/sys_tray.lua` already lives with, met here with more rows.
local theme = require("config.theme")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")

local app_list = list {
    source = oblisk.applications:map(function(applications)
        return (applications and applications.entries) or {}
    end),
    itemfn = function(app)
        return button {
            height = 26,
            align_v = "Center",
            on_click = function(_, mouse_button)
                if mouse_button ~= "left" then
                    return
                end
                oblisk.applications:invoke("launch", app.id)
                ui_state.launcher_open:set(false)
            end,
            children = { row {
                spacing = 8,
                align_v = "Center",
                children = {
                    -- `Icon=` is a theme name on almost every entry and an absolute path on a
                    -- few, and `icon` takes either without a branch here (ADR-0054 decision 2).
                    icon { name = app.icon or "", size = 18 },
                    cell(app.name, theme.FG, 12),
                },
            } },
        }
    end,
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
    }, {
        width = "Fill",
        height = "Fill",
        padding = { top = 14, right = 14, bottom = 14, left = 14 },
        spacing = 8,
        radius = 0,
    }),
}
