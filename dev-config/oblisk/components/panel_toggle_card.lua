-- A radio as a tile, `Components/PanelToggleCard.qml`: a glyph over a word, the whole tile a
-- button, lit accent while the thing is on. Two of them side by side under a panel's header is
-- how the mirror lays out wi-fi beside ethernet, and it is the one place on this bar where the
-- ground lights up: a filled ground means "this is on" (`modules/bar/panels/power_menu.lua` states
-- the rule), and a radio being on is exactly that.
--
-- This was a label beside a `components/toggle.lua` -- the settings-row shape -- and the switch
-- itself now sits in the panel's header (`components/panel_header.lua`), which is where the
-- mirror puts the master switch. What this tile answers is the next question down: with
-- networking on, which radios?
--
-- Takes the raw signal and a `read` function rather than a boolean signal, the split
-- `components/toggle.lua` and `components/meter.lua` both make for the same reason: the
-- capability pushes a table, and only the caller knows which field is the switch.
local theme = require("config.theme")
local cell = require("components.cell")

---@class PanelToggleCardOpts
---@field slot string The hover region's name; one per tile.
---@field icon string|Bound
---@field label string
---@field detail? string|Bound A second line under the label -- a band, an address. Hidden while it reads empty.
---@field signal Signal The capability whose payload `read` inspects.
---@field read fun(payload: any): boolean
---@field on_change fun(checked: boolean)

local function read_bool(value, read)
    if value == nil then
        return false
    end
    local ok, result = pcall(read, value)
    return ok and result == true
end

---@param opts PanelToggleCardOpts
return function(opts)
    local hovered = hover(opts.slot)
    local checked = opts.signal:map(function(value)
        return read_bool(value, opts.read)
    end)

    -- The mirror's three colour bindings, each on the pair (checked, hovered): the ground, its ring,
    -- and the ink the glyph and word share.
    local ground = computed({ checked, hovered }, function(on, hot)
        if on then
            return hot and theme.ACCENT_LIGHT or theme.ACCENT_SUBTLE
        end
        return hot and theme.GLASS_HOVER or theme.GLASS_CONTENT
    end)
    local ring = computed({ checked, hovered }, function(on, hot)
        if on then
            return theme.ACCENT_MEDIUM
        end
        return hot and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
    end)
    local ink = computed({ checked, hovered }, function(on, hot)
        if on then
            return theme.ACCENT
        end
        return hot and theme.FG or theme.TEXT_OFF
    end)

    local lines = {
        cell(opts.icon, ink, theme.icon.md, { align = "Center" }),
        cell(opts.label, ink, theme.font.xs, { align = "Center" }),
    }
    if opts.detail then
        local detail = opts.detail
        ---@cast detail -nil
        ---@type boolean|Signal
        local shown = true
        if type(detail) == "userdata" then
            ---@cast detail Signal
            shown = detail:map(function(text)
                return text ~= nil and text ~= ""
            end)
        end
        lines[#lines + 1] = cell(detail, theme.TEXT_OFF, theme.font.xs, { align = "Center", visible = shown })
    end

    return button {
        width = "Fill",
        height = theme.panel_toggle_height,
        radius = theme.radius.lg,
        hover = hovered,
        background = ground,
        border_width = theme.border_width,
        border_color = ring,
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            opts.on_change(not read_bool(opts.signal:get(), opts.read))
        end,
        -- A `button` stacks its children; the column is what puts the word under the glyph, and its
        -- own two alignments centre the stack in the tile.
        children = { column { align_h = "Center", align_v = "Center", spacing = theme.spacing.xs, children = lines } },
    }
end
