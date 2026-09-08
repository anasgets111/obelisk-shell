-- Mirrors `Components/InfoBadge.qml`: a short bold count on a filled capsule with the shell's
-- hairline, sized by its text rather than a fixed width.
--
-- Extraction rule: `modules/bar/panels/bluetooth_panel.lua` had written this shape as its own
-- `battery_badge`, and `modules/bar/panels/notification_history.lua` wanted the same capsule for
-- the urgent count. Two agreeing call sites is a component, and the mirror had already made it one.
--
-- The text takes `text_contrast(ground)`, not `FG`: these grounds are filled swatches -- accent,
-- peach, red -- and white on peach is unreadable where the mirror's contrast rule puts dark ink.
local theme = require("config.theme")
local cell = require("components.cell")

---@param label string|Bound
---@param ground? Color|Bound The capsule's fill. Default `theme.GLASS_CONTROL`, the mirror's own.
---@param opts? { visible?: boolean|Bound, align_v?: "Start"|"Center"|"End", opacity?: number }
return function(label, ground, opts)
    opts = opts or {}
    ground = ground or theme.GLASS_CONTROL
    -- A live ground maps the same contrast rule over itself, the way `components/icon_button.lua`
    -- does. The recorder's elapsed badge is the call site that needs it: `badgeColor: paused ?
    -- warning : critical` swaps peach for red under a running capture, and the ink has to follow or
    -- one of the two is unreadable.
    ---@type Color|Signal
    local ink
    if type(ground) == "userdata" then
        ---@cast ground Signal
        ink = ground:map(theme.text_contrast)
    else
        ---@cast ground Color
        ink = theme.text_contrast(ground)
    end
    -- A `TextRun` carries the weight, and its `text` is a plain string: a signal has to be mapped
    -- into the run rather than dropped inside one.
    ---@type TextRun[]|Bound
    local content
    if type(label) == "string" then
        content = { { text = label, bold = true } }
    else
        ---@cast label Signal
        content = label:map(function(shown)
            return { { text = shown, bold = true } }
        end)
    end
    return row {
        height = theme.control.xs,
        align_v = opts.align_v or "Center",
        visible = opts.visible,
        opacity = opts.opacity,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        radius = theme.control.xs / 2,
        background = ground,
        border_width = theme.border_width,
        border_color = theme.GLASS_BORDER,
        children = { cell(content, ink, theme.font.xs, {
            align = "Center",
            align_v = "Center",
        }) },
    }
end
