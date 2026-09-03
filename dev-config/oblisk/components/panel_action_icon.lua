-- A row's own small control, `Components/PanelActionIcon.qml`: a glyph with no ground of its own,
-- tinted by what it does -- red to disconnect or forget -- that only grows a ground under the
-- pointer. Quieter than a `components/icon_button.lua` on purpose: a list of six rows each carrying
-- two of these would otherwise read as eighteen buttons, and the row is the thing to look at.
local theme = require("config.theme")
local icon_button = require("components.icon_button")

-- Fully transparent, so the ring `icon_button` would otherwise draw is off too (`border = false`).
local CLEAR = "#00000000"

---@param glyph string
---@param on_activate fun()
---@param opts { slot: string, tint?: Color, visible?: boolean|Bound }
return function(glyph, on_activate, opts)
    local tint = opts.tint or theme.FG
    -- The same registry entry `icon_button` will ask for under this slot (ADR-0062 decision 2), so
    -- the glyph brightens on the hover the button already tracks.
    local hovered = hover(opts.slot)
    return icon_button(glyph, on_activate, {
        slot = opts.slot,
        size = theme.control.sm,
        icon_size = theme.icon.sm,
        radius = theme.radius.sm,
        border = false,
        background = CLEAR,
        background_hover = theme.with_opacity(tint, 0.15),
        foreground = hovered:map(function(is_hovered)
            return is_hovered and tint or theme.with_opacity(tint, 0.6)
        end),
        visible = opts.visible,
    })
end
