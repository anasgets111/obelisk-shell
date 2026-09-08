-- A labelled button, unlike `icon_button`: use it when the word is the point, such as a
-- notification action named "Update", "Retry", or "Close".
-- Extraction rule: one call site is a local; two agreeing call sites are a component. This was the
-- second: `modules/bar/panels/update_panel.lua`. Offered actions use accent; "Close" should not
-- compete with the result.
-- `opts.icon` is the theme icon named by a sender's `action-icons` key (ADR-0090), beside the label
-- or alone when there is no label. A media notification's prev/play/next are glyphs, not words.
local theme = require("config.theme")
local cell = require("components.cell")

-- `solid` mirrors `variant: "primary"` and is the only opaque ground. The other tints suit equal
-- notification choices, but at 15% alpha a panel's active control shows the glass behind "update".
-- It picks its foreground too, so callers cannot mismatch a chosen background and text.
local GROUND = {
    accent = { rest = theme.ACCENT_SUBTLE, hover = theme.ACCENT_LIGHT, border = theme.ACCENT_MEDIUM },
    quiet = { rest = theme.GLASS_CONTROL, hover = theme.GLASS_CONTROL_HOVER, border = theme.GLASS_BORDER },
    solid = {
        rest = theme.ACCENT,
        hover = theme.ACCENT_HOVER,
        border = theme.ACCENT,
        text = theme.text_contrast(theme.ACCENT),
    },
    -- `bgColor: Theme.critical` on `ScreenRecorderPanel.qml`'s stop button: solid's shape with the
    -- alert colour, for the one action in a panel that ends something already running.
    danger = {
        rest = theme.RED,
        hover = theme.RED_HOVER,
        border = theme.RED,
        text = theme.text_contrast(theme.RED),
    },
}

---@param label string|Bound
---@param on_activate? fun() Absent on a `submit` button, whose click is the field's Enter.
---@param slot string A `hover` slot unique to this button; two buttons sharing one light up together.
---@param opts? { icon?: string, glyph?: string|Bound, tone?: "accent"|"quiet"|"solid"|"danger", width?: integer|"Fill", height?: integer, visible?: boolean|Bound, submit?: boolean, on_button?: fun(rect: Rect, button: string) }
return function(label, on_activate, slot, opts)
    opts = opts or {}
    local ground = GROUND[opts.tone or "accent"]
    local hovered = hover(slot)
    -- `button` centres its one child with `align_h`; a `row` starts its children. This is fine on a
    -- content-sized button, whose row is exactly as wide as the word, but wrong on a filling one:
    -- it left "update" against the padding. Fill both row and label, then let `cell`'s `text_align`
    -- centre the label in the filled box.
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
    -- `opts.glyph` is the Nerd Font half of the same slot. A notification's action icon is a theme
    -- name (ADR-0090) and has to stay an `icon` node; a panel's own control is a codepoint from
    -- `config/icons.lua`, which is a `text` node and takes the button's ink like the label does.
    if opts.glyph then
        children[#children + 1] = text {
            content = opts.glyph,
            foreground = ground.text or theme.FG,
            font_size = theme.icon.sm,
            font = theme.icon_font,
            align_v = "Center",
        }
    end
    if label and label ~= "" then
        local label_content = type(label) == "string" and { { text = label, bold = true } } or label
        children[#children + 1] = cell(label_content, ground.text or theme.FG, theme.font.sm, {
            align = "Center",
            align_v = "Center",
            width = fill,
        })
    end
    return button {
        submit = opts.submit,
        width = opts.width,
        height = opts.height or theme.control.md,
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
        animate = { background = theme.animation_ms },
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
