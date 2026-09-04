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

-- `solid` is the mirror's `variant: "primary"`, and it is the only opaque ground here. The other two
-- are tints, which is right for a row of equal choices on a notification card and wrong for the one
-- control a panel is open for: at 15% alpha over a glass card you read the window behind the shell
-- through the word "update". It picks its own foreground, because a control that chooses its own
-- background has to choose the text on it or every caller has to remember to do both.
local GROUND = {
    accent = { rest = theme.ACCENT_SUBTLE, hover = theme.ACCENT_LIGHT, border = theme.ACCENT_MEDIUM },
    quiet = { rest = theme.GLASS_CONTROL, hover = theme.GLASS_CONTROL_HOVER, border = theme.GLASS_BORDER },
    solid = {
        rest = theme.ACCENT,
        hover = theme.ACCENT_HOVER,
        border = theme.ACCENT,
        text = theme.text_contrast(theme.ACCENT),
    },
}

---@param label string|Bound
---@param on_activate? fun() Absent on a `submit` button, whose click is the field's Enter.
---@param slot string A `hover` slot unique to this button; two buttons sharing one light up together.
---@param opts? { icon?: string, tone?: "accent"|"quiet"|"solid", width?: integer|"Fill", visible?: boolean|Bound, submit?: boolean }
return function(label, on_activate, slot, opts)
    opts = opts or {}
    local ground = GROUND[opts.tone or "accent"]
    local hovered = hover(slot)
    -- A `button` stacks, so its one child is placed by `align_h`; a `row` does not, so its children
    -- sit at its start. That is fine on a content-sized button, whose row is exactly as wide as the
    -- word in it, and wrong on a filling one, where the row inherits nothing and leaves the label
    -- against the left padding -- which is where "update" sat across the whole width of the update
    -- panel. Filling the row and the label both is what puts the word back in the middle, and
    -- `cell`'s `text_align` is what centres it inside the box the fill just gave it.
    local fill = opts.width == "Fill" and "Fill" or nil
    local children = {}
    if opts.icon then
        children[#children + 1] = icon {
            name = opts.icon,
            size = theme.icon.sm,
            align_v = "Center",
            foreground = ground.text,
        }
    end
    if label and label ~= "" then
        children[#children + 1] = cell(label, ground.text or theme.FG, theme.font.sm, {
            align = "Center",
            align_v = "Center",
            width = fill,
        })
    end
    return button {
        submit = opts.submit,
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
                if on_activate then
                    on_activate()
                end
            end
        end,
        -- The row is what puts a glyph beside a word, rather than on top of one.
        children = { row { width = fill, height = "Fill", align_v = "Center", spacing = theme.spacing.xs, children = children } },
    }
end
