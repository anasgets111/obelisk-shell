-- Mirrors WorkspaceStrip.qml.
--
-- Workspaces, and the only module here reading a compositor's IPC rather than a device or a D-Bus
-- service (docs/adr/0056). A `list` of buttons laid out left to right, which needed `list` to grow
-- a `direction` first (§ 5.2 item 7): until that landed a `list` stacked downwards inside a 34px
-- bar, so this was one `text` cell reading "1 2 [4] 5" and a click that cycled.
--
-- Clicking a workspace now goes to that workspace. That is the point of the conversion and it is a
-- behaviour change rather than a layout one: the cycle was what a single cell could express, not
-- what a strip should do.
--
-- It reads `outputs[1]` rather than looping, which is the fixture winning over the worked example:
-- this machine has one output, and a config for a real multi-monitor setup would match
-- `out.focused_workspace ~= nil` to find the monitor with keyboard focus (docs/adr/0056 decision 4)
-- or loop and draw a strip per monitor.
--
-- `name` before `idx`: niri lets a workspace be named, and a named one is what its user calls it.
-- `idx` is the position on the output, which is what an unnamed workspace has instead.
--
-- It sends `id`, never `idx`: `idx` shifts when workspaces are reordered, so the id is the only
-- argument that still names the workspace the user actually clicked.
--
-- § 2.9's `active_client` used to hang off the end of this pill and is gone: the centre zone draws
-- the focused window (`modules/bar/center_side.lua`), so this was the same fact twice, and on a
-- twelve-workspace output the two together were wider than the left zone can hold.
local theme = require("config.theme")
local cell = require("components.cell")
local pill = require("components.pill")

local function workspaces_of(w)
    local out = w and (w.outputs or {})[1]
    return out and (out.workspaces or {}) or {}
end

local function is_active(w, id)
    local out = w and (w.outputs or {})[1]
    return out ~= nil and out.active_workspace == id
end

-- One hover slot per workspace, named after its id. Quickshell's strip highlights the slot under
-- the pointer, and that only became expressible here when each workspace became a node of its own:
-- a hover region is a node's box (docs/adr/0062 decision 5), so the single cell this replaced could
-- only ever highlight all of itself.
--
-- ponytail: a slot is created on first ask and kept for the life of the generation (`hover`'s
-- registry is keyed by name), so closing a workspace leaves its slot behind reading false. Bounded
-- by how many workspace ids one session produces and freed on the next generation swap, which is
-- why this is a note rather than a cleanup path.
local function workspace_button(ws)
    local hovered = hover("workspace-" .. tostring(ws.id))
    return button {
        hover = hovered,
        height = 18,
        align_v = "Center",
        padding = { left = 3, right = 3 },
        radius = 4,
        -- Active wins over hovered, so pointing at the workspace you are already on does not make
        -- it look like a different one. Both are read in one `computed` rather than two chained
        -- `map`s: a signal resolves once per pass either way, and one closure is one place to look
        -- when the colours are wrong.
        background = computed({ oblisk.workspaces, hovered }, function(w, is_hovered)
            if is_active(w, ws.id) then
                return theme.ACCENT
            end
            return is_hovered and theme.HOVER or theme.SURFACE
        end),
        children = {
            -- Dark text on the accent fill: `theme.FG` is a light foreground and reading it against
            -- `theme.ACCENT` is the one combination in this palette that genuinely does not work.
            cell(ws.name or tostring(ws.idx), oblisk.workspaces:map(function(w)
                return is_active(w, ws.id) and theme.BG or theme.FG
            end), 11),
        },
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            oblisk.workspaces:invoke("focus", ws.id)
        end,
    }
end

return pill({
    list {
        direction = "Horizontal",
        spacing = 2,
        align_v = "Center",
        source = oblisk.workspaces:map(workspaces_of),
        itemfn = workspace_button,
        -- Reconciles by id, so switching workspaces rebuilds nothing: the ids are unchanged, and
        -- what moved is the `active_workspace` each button's own `background` closure reads. Without
        -- this an insertion at the front would renumber every sibling and reconcile each against
        -- the wrong previous node (ADR-0045).
        key = function(ws)
            return tostring(ws.id)
        end,
    },
})
