-- A row's small control, matching `Components/PanelActionIcon.qml`: tinted by what it does, red to
-- disconnect or forget, with no ground until hover. It stays quieter than
-- `components/icon_button.lua`; two per six-row list must not read as eighteen buttons.
local theme = require("config.theme")
local icon_button = require("components.icon_button")

-- Fully transparent; `border = false` also disables `icon_button`'s ring.
local CLEAR = "#00000000"

---@param glyph string
---@param on_activate fun()
---@param opts { slot: string, tint?: Color, visible?: boolean|Bound }
return function(glyph, on_activate, opts)
    local tint = opts.tint or theme.FG
    -- `icon_button` asks for the same registry entry under this slot (ADR-0062 decision 2), so the
    -- glyph brightens with the button's hover.
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
