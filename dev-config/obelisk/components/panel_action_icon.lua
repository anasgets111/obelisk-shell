-- A row's small control, matching `Components/PanelActionIcon.qml`: tinted by what it does, red to
-- disconnect or forget, with no ground until hover. It stays quieter than
-- `components/icon_button.lua`; two per six-row list must not read as eighteen buttons.
local theme = require("config.theme")
local icon_button = require("components.icon_button")

-- Fully transparent; `border = false` also disables `icon_button`'s ring.
local CLEAR = "#00000000"

---@param glyph string|Bound A `text` glyph, or a signal of one for a control whose icon follows state.
---@param on_activate fun()?
---@param opts { slot: string, tint?: Color, visible?: boolean|Bound, size?: "sm"|"md" }
return function(glyph, on_activate, opts)
    local tint = opts.tint or theme.FG
    -- `PanelActionIcon.qml`'s own `size: "sm"`, which the media panel overrides to `"md"` for the
    -- one control in a transport row that is the row's subject.
    local step = opts.size or "sm"
    -- `icon_button` asks for the same registry entry under this slot (ADR-0062 decision 2), so the
    -- glyph brightens with the button's hover.
    local hovered = hover(opts.slot)
    return icon_button(glyph, on_activate, {
        slot = opts.slot,
        size = theme.control[step],
        icon_size = theme.icon[step],
        radius = theme.radius.sm,
        border = false,
        background = CLEAR,
        background_hover = theme.with_opacity(tint, theme.opacity.subtle),
        foreground = hovered:map(function(is_hovered)
            return is_hovered and tint or theme.with_opacity(tint, theme.opacity.disabled)
        end),
        visible = opts.visible,
    })
end
