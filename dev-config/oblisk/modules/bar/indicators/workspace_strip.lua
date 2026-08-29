-- Mirrors WorkspaceStrip.qml.
--
-- Workspaces, and the only module here reading a compositor's IPC rather than a device or a D-Bus
-- service (docs/adr/0056). The strip is one `text` cell, not a `list` of buttons, because `list`
-- lays out vertically only (the no-horizontal-list ponytail in scene.rs) -- this is that ponytail's
-- second consumer, the tray being the first.
--
-- It reads `outputs[1]` rather than looping, which is the fixture winning over the worked example:
-- this machine has one output, and a config for a real multi-monitor setup would match
-- `out.focused_workspace ~= nil` to find the monitor with keyboard focus (docs/adr/0056 decision 4)
-- or loop and draw a strip per monitor.
--
-- `name` before `idx`: niri lets a workspace be named, and a named one is what its user calls it.
-- `idx` is the position on the output, which is what an unnamed workspace has instead.
--
-- The click cycles to the next workspace and is what proves § 3.2's `workspaces:focus(id)`. It
-- sends `id`, never `idx`: `idx` shifts when workspaces are reordered, so the id is the only
-- argument that still names the workspace the user just saw.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

return pill({
    button {
        height = 18,
        align_v = "Center",
        -- Right cycles backwards, the same reading of the second argument the brightness pill makes.
        on_click = function(_, button)
            local w = oblisk.workspaces:get()
            local out = w and (w.outputs or {})[1]
            if out == nil then
                return
            end
            local entries = out.workspaces or {}
            if #entries == 0 then
                return
            end
            local at = 1
            for i, ws in ipairs(entries) do
                if ws.id == out.active_workspace then
                    at = i
                end
            end
            local step = button == "right" and -1 or 1
            oblisk.workspaces:invoke("focus", entries[(at - 1 + step) % #entries + 1].id)
        end,
        children = { cell(util.label(oblisk.workspaces, function(w)
            local out = (w.outputs or {})[1]
            if out == nil then
                return "no workspaces"
            end
            local marks = {}
            for _, ws in ipairs(out.workspaces or {}) do
                local name = ws.name or tostring(ws.idx)
                marks[#marks + 1] = ws.id == out.active_workspace and ("[" .. name .. "]") or name
            end
            return table.concat(marks, " ")
        end), theme.FG) },
    },
    -- § 2.9's `active_client`, minus the `is_fullscreen` niri cannot answer (docs/adr/0056
    -- decision 5). `class` is the app id: on Wayland there is no WM_CLASS to read.
    cell(util.label(oblisk.workspaces, function(w)
        local client = w.active_client
        if client == nil then
            return "no window"
        end
        local name = util.truncate(client.class ~= "" and client.class or "?", 14)
        return name .. (client.is_floating and " (float)" or "")
    end), theme.DIM, 11),
})
