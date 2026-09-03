-- A labelled button, which is what `icon_button` is not: a glyph circle says what it does by being
-- a picture of it, and this is for the cases where the word is the point -- a notification action
-- the sender named, "Update", "Retry", "Close".
--
-- This was a local in `components/notification_card.lua`, with a comment saying one call site is a
-- local and two in agreement are a component. `modules/bar/panels/update_panel.lua` is the second,
-- and it agrees about all of it but the ground: an action you are being *offered* is accent, and a
-- "Close" that only tidies away a result you have already read should not compete with it.
--
-- `opts.icon` is the theme icon a notification sender that set `action-icons` named through the key
-- (ADR-0090), drawn beside the label, or alone when the sender sent no label -- a media
-- notification's prev/play/next is three glyphs, not three words.
local theme = require("config.theme")
local cell = require("components.cell")

local GROUND = {
    accent = { rest = theme.ACCENT_SUBTLE, hover = theme.ACCENT_LIGHT, border = theme.ACCENT_MEDIUM },
    quiet = { rest = theme.GLASS_CONTROL, hover = theme.GLASS_CONTROL_HOVER, border = theme.GLASS_BORDER },
}

---@param label string|Bound
---@param on_activate fun()
---@param slot string A `hover` slot unique to this button; two buttons sharing one light up together.
---@param opts? { icon?: string, tone?: "accent"|"quiet", width?: integer|"Fill", visible?: boolean|Bound }
return function(label, on_activate, slot, opts)
    opts = opts or {}
    local ground = GROUND[opts.tone or "accent"]
    local hovered = hover(slot)
    local children = {}
    if opts.icon then
        children[#children + 1] = icon { name = opts.icon, size = theme.icon.sm, align_v = "Center" }
    end
    if label and label ~= "" then
        children[#children + 1] = cell(label, theme.FG, theme.font.sm, { align = "Center", align_v = "Center" })
    end
    return button {
        width = opts.width,
        height = theme.control.md,
        align_v = "Center",
        radius = theme.radius.md,
        visible = opts.visible,
        hover = hovered,
        background = hovered:map(function(is_hovered)
            return is_hovered and ground.hover or ground.rest
        end),
        border_width = theme.border_width,
        border_color = ground.border,
        padding = { left = theme.spacing.md, right = theme.spacing.md },
        on_click = function(_, mouse_button)
            if mouse_button == "left" then
                on_activate()
            end
        end,
        -- A `button` stacks its children; the row is what puts a glyph beside a word.
        children = { row { height = "Fill", align_v = "Center", spacing = theme.spacing.xs, children = children } },
    }
end
