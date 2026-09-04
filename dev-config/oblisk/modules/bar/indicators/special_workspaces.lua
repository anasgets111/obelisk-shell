-- Mirrors `SpecialWorkspaces.qml`: one circle per special workspace, Hyprland's scratchpads,
-- accent while it is shown on some output and glass while hidden, a click toggling it. Reads
-- `oblisk.workspaces.special` (ADR-0119), which is `nil` on a compositor without specials, so the
-- whole row is absent there; `LeftSide.qml` gates its loader on `supportsSpecialWorkspaces` the
-- same way. Also absent while the list is empty, so the row costs the bar no spacing gap then.
--
-- The glyph is the standing window's icon when `oblisk.applications` knows its `app_id`, else the
-- first two letters of the name after `special:`, which is the mirror's fallback; its keyword-to-
-- glyph table (`term`, `slack`...) is not carried, since the real icon covers what it guessed at.
--
-- No tooltip: one would be a `popup` per special mounted in `shell.lua`, for a list that changes
-- as scratchpads come and go. The two letters carry the name well enough.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

local function specials_of(w)
    return (w and w.special) or {}
end

local function short_name(name)
    return name:gsub("^special:?", "")
end

local function special_button(special)
    local name = special.name
    local entry = oblisk.workspaces:map(function(w)
        for _, candidate in ipairs(specials_of(w)) do
            if candidate.name == name then
                return candidate
            end
        end
        return special
    end)
    local is_shown = entry:map(function(current)
        return current.shown_on ~= nil
    end)
    local slot_hovered = hover("special-" .. name)
    local ground = computed({ is_shown, slot_hovered }, function(shown, is_hovered)
        if shown then
            return theme.ACCENT
        end
        return is_hovered and theme.GLASS_CONTROL_HOVER or theme.GLASS_CONTROL
    end)
    local icon_name = computed({ oblisk.applications, entry }, function(applications, current)
        local app = util.app_entry(applications, current.app_id)
        return (app and app.icon) or ""
    end)
    local has_icon = icon_name:map(function(icon)
        return icon ~= ""
    end)
    local short = short_name(name)
    local letters = #short > 2 and short:sub(1, 2):upper() or short:upper()
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
        children = {
            icon {
                name = icon_name,
                size = theme.icon.md,
                align_h = "Center",
                align_v = "Center",
                visible = has_icon,
            },
            cell(letters, ground:map(theme.text_contrast), theme.font.xs, {
                align = "Center",
                align_v = "Center",
                visible = has_icon:map(function(shown)
                    return not shown
                end),
            }),
        },
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            oblisk.workspaces:invoke("toggle_special", name)
        end,
    }
end

return row {
    height = theme.item_height,
    align_v = "Center",
    visible = oblisk.workspaces:map(function(w)
        return #specials_of(w) > 0
    end),
    children = {
        list {
            direction = "Horizontal",
            spacing = theme.spacing.sm,
            align_v = "Center",
            source = oblisk.workspaces:map(specials_of),
            itemfn = special_button,
            key = function(special)
                return special.name
            end,
        },
    },
}
