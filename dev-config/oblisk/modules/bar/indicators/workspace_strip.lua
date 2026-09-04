-- Mirrors `WorkspaceStrip.qml`: the mirror's `ExpandingPill` of workspaces. Collapsed it is one
-- circle, the active workspace; under the pointer it widens into one circle per workspace, each
-- drawing the icon of what runs there or its number when nothing does, and narrows back when the
-- pointer leaves. The same pattern as `modules/bar/panels/power_menu.lua`: `hover` on the row
-- holding the circles, since a hover region answers containment and a pointer crossing the gap
-- between two circles never leaves the row. An earlier strip here was twelve always-open dots on
-- the belief that collapsing needed a timer; it needed the row.
--
-- The ground says what a workspace holds (ADR-0117): accent when active, glass when populated,
-- `DISABLED` at half opacity when empty, `IconButton.qml`'s colours through
-- `computeWorkspaceColor`. The glyph is the standing window's icon when `oblisk.applications`
-- knows its `app_id`, else `idx`, never `name`: a named workspace elided into a circle draws three
-- dots and no information, and the number is what the keybind uses anyway.
--
-- The collapsed slot is the first output's `active_workspace` rather than the mirror's focused one:
-- every output has an active workspace and only one output holds focus, so a strip on the other
-- monitor would otherwise collapse to nothing. On the focused output the two are the same.
--
-- Not mirrored: the width animation and the opacity fade, which the engine has no way to draw.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

local function output_of(w)
    return w and (w.outputs or {})[1]
end

local function workspaces_of(w)
    local out = output_of(w)
    return out and (out.workspaces or {}) or {}
end

local pill_hovered = hover("workspace-pill")

local function workspace_button(ws)
    local id = ws.id
    -- Read off the snapshot rather than the `ws` this was built from: the list reconciles by
    -- key, so a workspace whose windows come and go keeps its button and this is what changes.
    local entry = oblisk.workspaces:map(function(w)
        for _, candidate in ipairs(workspaces_of(w)) do
            if candidate.id == id then
                return candidate
            end
        end
        return ws
    end)
    local is_active = oblisk.workspaces:map(function(w)
        local out = output_of(w)
        return out ~= nil and out.active_workspace == id
    end)
    local slot_hovered = hover("workspace-" .. tostring(id))
    local ground = computed({ is_active, slot_hovered, entry }, function(active, is_hovered, current)
        if active then
            return theme.ACCENT
        elseif is_hovered then
            return theme.GLASS_CONTROL_HOVER
        end
        return current.populated and theme.GLASS_CONTROL or theme.DISABLED
    end)
    local icon_name = computed({ oblisk.applications, entry }, function(applications, current)
        local app = util.app_entry(applications, current.app_id)
        return (app and app.icon) or ""
    end)
    local has_icon = icon_name:map(function(name)
        return name ~= ""
    end)
    return button {
        width = theme.item_width,
        height = theme.item_height,
        align_v = "Center",
        radius = theme.item_radius,
        hover = slot_hovered,
        background = ground,
        border_width = theme.border_width,
        border_color = slot_hovered:map(function(is_hovered)
            return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
        end),
        opacity = entry:map(function(current)
            return current.populated and 1 or theme.opacity.disabled
        end),
        visible = computed({ pill_hovered, is_active }, function(open, active)
            return open or active
        end),
        children = {
            icon {
                name = icon_name,
                size = theme.icon.md,
                align_h = "Center",
                align_v = "Center",
                visible = has_icon,
            },
            cell(tostring(ws.idx), ground:map(theme.text_contrast), theme.font.sm, {
                align = "Center",
                align_v = "Center",
                visible = has_icon:map(function(shown)
                    return not shown
                end),
            }),
        },
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" or is_active:get() then
                return
            end
            oblisk.workspaces:invoke("focus", id)
        end,
    }
end

-- The row is the pill: it carries the hover and nothing else, no ground of its own, since the
-- mirror's circles sit straight on the bar.
return row {
    height = theme.item_height,
    align_v = "Center",
    hover = pill_hovered,
    children = {
        list {
            direction = "Horizontal",
            spacing = theme.spacing.sm,
            align_v = "Center",
            source = oblisk.workspaces:map(workspaces_of),
            itemfn = workspace_button,
            key = function(ws)
                return tostring(ws.id)
            end,
        },
    },
}
