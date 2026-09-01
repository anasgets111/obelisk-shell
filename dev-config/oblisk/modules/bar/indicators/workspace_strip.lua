-- Mirrors WorkspaceStrip.qml: one circular button per workspace, laid out horizontally, with the
-- focused one filled accent.
--
-- Dots at `workspace_size`, not controls at `item_height`. Twelve full-size bordered circles was
-- 416px of a 1920px bar and the widest thing on the left by a factor of five, which is the wrong
-- shape for the one module here that carries a single digit. Smaller, unbordered, and dimmed unless
-- focused: the strip reads as a strip rather than as twelve more controls.
--
-- The label is `idx`, never `name`. A named workspace elided into a circle draws "sta...", which is
-- three dots and no information; the name has nowhere to go on a bar this size and the number is
-- what the keybind uses anyway.
--
-- ponytail: the mirror pays that back with `ExpandingPill`, which collapses to just the focused
-- workspace and expands on hover. That needs a per-slot `visible` driven by one hover region, which
-- this engine can express (`hover` is a signal, ADR-0062) but which also needs the collapse to
-- be delayed past the pointer leaving one circle for the next, and nothing here has a timer that
-- short (ADR-0021's 5ms cap is the ceiling). Until then the strip is always open.
local theme = require("config.theme")
local cell = require("components.cell")

-- Fully transparent rather than an absent `background`. A signal that resolves to nil omits the
-- property, and "omitted" and "clear" are the same pixel only until something else sets a default.
local CLEAR = theme.with_opacity(theme.BG, 0)

local function workspaces_of(w)
    local out = w and (w.outputs or {})[1]
    return out and (out.workspaces or {}) or {}
end

local function is_active(w, id)
    local out = w and (w.outputs or {})[1]
    return out ~= nil and out.active_workspace == id
end

local function workspace_button(ws)
    local hovered = hover("workspace-" .. tostring(ws.id))
    local active = computed({ oblisk.workspaces, hovered }, function(w, is_hovered)
        return is_active(w, ws.id) or is_hovered
    end)
    return button {
        hover = hovered,
        width = theme.workspace_size,
        height = theme.workspace_size,
        align_v = "Center",
        radius = theme.workspace_size / 2,
        -- Filled only when focused or under the pointer. An unfocused workspace has no ground at
        -- all: twelve glass discs in a row is twelve objects to look at, and only one of them is
        -- ever the answer to the question the strip is asked.
        background = active:map(function(on)
            return on and theme.ACCENT or CLEAR
        end),
        -- The mirror dims a workspace with nothing on it. `WorkspaceEntry` in
        -- `supervisor/src/workspaces/controller.rs` is `id`, `idx` and `name`, so there is no
        -- populated flag to read and every dot is drawn at the same strength.
        children = {
            cell(tostring(ws.idx), active:map(function(on)
                return on and theme.text_contrast(theme.ACCENT) or theme.DIM
            end), theme.font.xs, {
                align = "Center",
                align_v = "Center",
            }),
        },
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            oblisk.workspaces:invoke("focus", ws.id)
        end,
    }
end

-- No pill around it. The mirror's strip sits on the bar itself, and wrapping a row of circles in a
-- second rounded ground draws a box around them that nothing in the reference has.
return list {
    direction = "Horizontal",
    spacing = theme.spacing.xs,
    align_v = "Center",
    source = oblisk.workspaces:map(workspaces_of),
    itemfn = workspace_button,
    key = function(ws)
        return tostring(ws.id)
    end,
}
