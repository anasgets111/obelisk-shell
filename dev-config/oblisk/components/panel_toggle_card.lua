-- Radio tile matching `Components/PanelToggleCard.qml`: glyph over a word, whole tile a button,
-- accent-lit when on. Side-by-side tiles put wi-fi beside ethernet; here a filled ground means
-- "this is on" (`modules/bar/panels/power_menu.lua`), exactly matching radio state.
-- Replaced a label beside `components/toggle.lua`'s settings-row switch. The master switch now sits
-- in `components/panel_header.lua`, as in the mirror; this tile answers which radios are on.
-- Takes the raw signal plus `read`, like `components/toggle.lua` and `components/meter.lua`,
-- because
-- the capability pushes a table and only the caller knows which field is the switch.
local theme = require("config.theme")
local cell = require("components.cell")
local glyph = require("components.glyph")

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

    -- Mirror colour bindings on `(checked, hovered)`: ground, ring, and shared glyph/word ink.
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
        return hot and theme.FG or theme.DIM
    end)

    local lines = {
        glyph(opts.icon, ink, theme.icon.md, { align = "Center" }),
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
        lines[#lines + 1] = cell(detail, theme.DIM, theme.font.xs, { align = "Center", visible = shown })
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
        -- `button` stacks children; the column puts the word under the glyph and centres the stack.
        children = { column { align_h = "Center", align_v = "Center", spacing = theme.spacing.xs, children = lines } },
    }
end
