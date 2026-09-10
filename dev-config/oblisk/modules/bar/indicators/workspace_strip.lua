-- Mirrors `WorkspaceStrip.qml`: active workspace collapses to one circle; pointer expands it.
-- It shows one circle per workspace and narrows `animationDuration + 200` after the pointer leaves.
-- `components/expanding_pill.lua` supplies the shared pill, also used by the power menu.
--
-- Ground: accent when active, glass when populated, `DISABLED` at half opacity when empty
-- (ADR-0117,
-- `IconButton.qml`/`computeWorkspaceColor`). Use the standing window's icon when applications knows
-- its `app_id`, else `idx`, never `name`: an elided name draws three dots, while the number is the
-- keybind's target.
--
-- Collapse to the first output's `active_workspace`, not the focused workspace. Every output has an
-- active workspace but only one has focus, so another monitor would otherwise collapse to nothing.
--
-- Hyprland pads to ten slots (ADR-0119, `WorkspaceArrangement.qml`'s `fillEmptySlots`): it lists no
-- empty workspaces, creates a numbered one on focus, and dims padded numbers here. Each padded slot
-- focuses that number. Niri keeps a trailing empty workspace and needs no padding.
-- The payload lists only existing workspaces; padding is this strip's `compositor`-keyed policy.
--
-- The ground and border ease between states (ADR-0145); the expansion is the pill's.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local expanding_pill = require("components.expanding_pill")

local PADDED_SLOTS = 10

local function output_of(w)
    return w and (w.outputs or {})[1]
end

-- Pad listed workspaces with `{ id = n, idx = n, populated = false }` up to `PADDED_SLOTS` or the
-- highest number in use. On a compositor where `id` is the number, focusing a missing one
-- creates it. The padded entry matches a real one, and its `id` is sent to `focus`.
local function workspaces_of(w)
    local out = output_of(w)
    local listed = out and (out.workspaces or {}) or {}
    if not (w and w.compositor == "hyprland") then
        return listed
    end
    local by_idx, highest = {}, PADDED_SLOTS
    for _, ws in ipairs(listed) do
        by_idx[ws.idx] = ws
        highest = math.max(highest, ws.idx)
    end
    local padded = {}
    for n = 1, highest do
        padded[n] = by_idx[n] or { id = n, idx = n, populated = false }
    end
    return padded
end

local pill = expanding_pill.new({ slot = "workspace-pill", collapse_ms = theme.animation_ms + 200 })

local function workspace_button(ws)
    local id = ws.id
    -- Read the current snapshot, not the build-time `ws`. Key reconciliation keeps the button while
    -- its windows change.
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
    return pill.cell(button {
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
        animate = { background = theme.animation_ms, border_color = theme.animation_ms, opacity = theme.animation_ms },
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
    }, is_active)
end

-- No ground of its own; the mirror's circles sit directly on the bar.
return pill.row({
    list {
        direction = "Horizontal",
        align_v = "Center",
        source = oblisk.workspaces:map(workspaces_of),
        itemfn = workspace_button,
        key = function(ws)
            return tostring(ws.id)
        end,
    },
})
