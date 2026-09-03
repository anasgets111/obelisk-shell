-- A panel's masthead, `Components/PanelHeader.qml`: a glyph on a tinted plate, a title over one
-- line of state, and whatever the panel keeps at hand on the right -- its radio switch, a rescan,
-- a close button.
--
-- The plate is the panel's state at a glance. It and the glyph go accent while the thing the panel
-- fronts is on and dim while it is off, so "network" and "bluetooth" read as two switches before
-- either word is read. `opts.active` is that one fact; the caller says which field it is.
--
-- This was a title and a close button, for `modules/bar/panels/settings.lua`. The network and
-- bluetooth panels each opened with a `section_header` and a `panel_toggle_card` under it -- a
-- small grey word, then a row saying "wi-fi" beside a switch -- which is the same information the
-- mirror puts in one line with a glyph, and the reason those two panels looked like settings lists
-- rather than the thing they are for.
--
-- The title column takes `width = "Fill"`, which pins the trailing controls to the far edge in one
-- property. That is what `scene.rs` sizing a `Fill` child from what its siblings leave is for, and
-- the title elides rather than pushing a control out because every `cell` declares `elide = "End"`.
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

    local children = {}
    if opts.icon then
        children[#children + 1] = rect {
            width = theme.control.lg,
            height = theme.control.lg,
            radius = theme.radius.md,
            background = plate,
            align_v = "Center",
            children = { cell(opts.icon, accent, theme.icon.lg, { align = "Center", align_v = "Center" }) },
        }
    end

    -- Bold, as the mirror's `titleBold`; a run rather than a property because weight lives on the
    -- `TextRun` (`lua-meta/nodes.lua`), and a title is a plain string here.
    local lines = { cell({ { text = opts.title, bold = true } }, theme.FG, theme.font.lg, { width = "Fill" }) }
    if opts.subtitle then
        lines[#lines + 1] = cell(opts.subtitle, theme.TEXT_OFF, theme.font.xs, { width = "Fill" })
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
