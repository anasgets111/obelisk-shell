-- Panel masthead matching `Components/PanelHeader.qml`: glyph on a tinted plate, title over one
-- state line, and trailing controls such as a radio switch, rescan, or close button.
-- The plate and glyph turn accent when the panel's subject is on and dim when off, so "network" and
-- "bluetooth" read as switches before their labels. The caller supplies that fact as `opts.active`.
-- Replaced the title/close pair in `modules/bar/panels/settings.lua`. Network and bluetooth had a
-- `section_header` plus `panel_toggle_card`, making a grey heading and a "wi-fi" switch row where
-- the mirror uses one glyph line, so they looked like settings lists instead of their subject.
-- `width = "Fill"` leaves the title's remaining space and pins trailing controls to the far edge.
-- `scene.rs` sizes it from its siblings; `cell`'s `elide = "End"` keeps a long title from pushing
-- controls out.
local theme = require("config.theme")
local cell = require("components.cell")
local icons = require("config.icons")
local icon_button = require("components.icon_button")

---@class PanelHeaderOpts
---@field title string
---@field subtitle? string|Bound One line of state under the title: the joined network, "2 connected · P30i · 90%", "off".
---@field icon? string|Bound A glyph on a plate; the plate and glyph take `active`'s colour.
---@field active? boolean|Bound Accent while true, dim while false. Default true.
---@field trailing? Node[] Controls at the far edge, in order.
---@field on_close? fun() Adds a close button after `trailing`.
---@field title_size? integer The title's font size. Default `theme.font.lg`, which is a bar panel's masthead; a modal's is bigger, and so is a section header inside one.
---@field subtitle_color? Color|Bound The state line's colour. Default `theme.TEXT_OFF`.
---@field subtitle_size? integer The state line's font size. Default `theme.font.xs`, which is right under a bar panel's 16px title and unreadably small under a modal's 28px one.
---@field plate? integer The icon plate's side. Default `theme.control.lg`, and it tracks `title_size` in the mirror rather than being set on its own.

---@param opts PanelHeaderOpts
return function(opts)
    local active = opts.active
    if active == nil then
        active = true
    end
    ---@type Color|Signal
    local accent
    ---@type Color|Signal
    local plate
    if type(active) == "userdata" then
        ---@cast active Signal
        accent = active:map(function(on)
            return on and theme.ACCENT or theme.TEXT_OFF
        end)
        plate = active:map(function(on)
            return on and theme.ACCENT_SUBTLE or theme.GLASS_CONTENT
        end)
    else
        accent = active and theme.ACCENT or theme.TEXT_OFF
        plate = active and theme.ACCENT_SUBTLE or theme.GLASS_CONTENT
    end

    local title_size = opts.title_size or theme.font.lg
    local plate_size = opts.plate or theme.control.lg

    local children = {}
    if opts.icon then
        children[#children + 1] = rect {
            width = plate_size,
            height = plate_size,
            radius = theme.radius.md,
            background = plate,
            align_v = "Center",
            children = { cell(opts.icon, accent, math.floor(plate_size * 0.55), { align = "Center", align_v = "Center" }) },
        }
    end

    -- Bold like the mirror's `titleBold`; weight lives on the `TextRun` (`lua-meta/nodes.lua`), not
    -- the plain title string's node.
    local lines = { cell({ { text = opts.title, bold = true } }, theme.FG, title_size, { width = "Fill" }) }
    if opts.subtitle then
        lines[#lines + 1] = cell(opts.subtitle, opts.subtitle_color or theme.TEXT_OFF, opts.subtitle_size or theme.font.xs, { width = "Fill" })
    end
    children[#children + 1] = column { width = "Fill", align_v = "Center", children = lines }

    for _, control in ipairs(opts.trailing or {}) do
        if control.align_v == nil then
            control.align_v = "Center"
        end
        children[#children + 1] = control
    end
    if opts.on_close then
        children[#children + 1] = icon_button(icons.close, opts.on_close, { size = theme.control.sm, icon_size = theme.icon.sm })
    end

    return row {
        width = "Fill",
        spacing = theme.spacing.sm,
        align_v = "Center",
        children = children,
    }
end
