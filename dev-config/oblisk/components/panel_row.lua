-- One row of a panel list: a leading icon, a title with an optional subtitle under it, and a
-- trailing slot for whatever the row does. `Components/PanelRow.qml` is this shape, and it is what
-- every list in every bar panel is made of -- an access point, a bluetooth device, a notification.
--
-- The title column takes `width = "Fill"`, so the trailing slot sits against the right edge and the
-- title elides into whatever is left instead of pushing it out. That is the same one property
-- `components/panel_header.lua` spends and needed the same `Fill` fix in `scene.rs`.
--
-- `selected` is the mirror's `selected`: an accent ring and a tinted ground on the row the list is
-- "on" -- the joined network, the connected device -- with the title in accent. The row used to say
-- this with a coloured title alone, and a coloured word in a list of white ones reads as a
-- different kind of row, not as the chosen one.
--
-- `on_activate` is optional. A row with none is a `rect`, not a `button`: a button that does nothing
-- still takes the pointer and still reads as clickable, which is worse than a plain line. Both
-- shapes take the same look, so a selected row that cannot be clicked (a connected device, whose
-- actions are its two trailing icons) still wears the ring.
local theme = require("config.theme")
local cell = require("components.cell")

-- Annotated for the same reason `components/cell.lua` is: this is the hop where a payload field
-- becomes text, and `title`/`subtitle` are handed straight to a `cell`. Without the shapes below
-- an `any` from a `list`'s `itemfn` passes through untouched, which is how a notification's span
-- array reached `text.content`.
---@class PanelRowOpts
---@field title string|Bound
---@field subtitle? string|Bound
---@field icon? string|Bound A glyph drawn as text and recoloured with the row.
---@field art? string|Bound A theme name or absolute path drawn as an `icon`, never recoloured.
---@field leading? Node A composed leading slot in place of `icon`/`art`: a glyph with a badge beside it.
---@field color? Color|Bound
---@field icon_color? Color|Bound
---@field selected? boolean
---@field opacity? number|Bound
---@field height? integer
---@field slot? string
---@field visible? boolean|Bound
---@field trailing? Node
---@field on_activate? fun()

---@param opts PanelRowOpts
return function(opts)
    local title_color = opts.selected and theme.ACCENT or (opts.color or theme.FG)
    ---@type string|TextRun[]|Bound
    local title = opts.title
    if opts.selected and type(title) == "string" then
        title = { { text = title, bold = true } }
    end
    local title_lines = { cell(title, title_color, theme.font.sm, { width = "Fill" }) }
    if opts.subtitle then
        title_lines[#title_lines + 1] = cell(opts.subtitle, theme.TEXT_OFF, theme.font.xs, { width = "Fill" })
    end

    local children = {}
    if opts.leading then
        children[#children + 1] = opts.leading
    elseif opts.icon then
        -- A glyph, not a themed icon, for `components/icon_button.lua`'s reason: `Components/
        -- PanelRow.qml` tints its leading icon by state (a connected device accent, a failed one
        -- red) and `PaintStyle::Icon` carries no tint. `opts.art` takes a themed icon name instead,
        -- for the rows that show something the config did not choose -- an application's own icon.
        children[#children + 1] = cell(opts.icon, opts.icon_color or title_color, theme.icon.md, { align_v = "Center" })
    elseif opts.art then
        children[#children + 1] = icon { name = opts.art, size = theme.icon.md, align_v = "Center" }
    end
    children[#children + 1] = column {
        width = "Fill",
        align_v = "Center",
        children = title_lines,
    }
    if opts.trailing then
        if opts.trailing.align_v == nil then
            opts.trailing.align_v = "Center"
        end
        children[#children + 1] = opts.trailing
    end

    local body = row {
        width = "Fill",
        spacing = theme.spacing.sm,
        align_v = "Center",
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = children,
    }

    local hovered = (opts.on_activate and opts.slot) and hover(opts.slot) or nil
    ---@type Color|Signal|nil
    local ground = opts.selected and theme.ACCENT_SUBTLE or nil
    if hovered then
        ground = hovered:map(function(is_hovered)
            if opts.selected then
                return is_hovered and theme.ACCENT_LIGHT or theme.ACCENT_SUBTLE
            end
            return is_hovered and theme.GLASS_HOVER or nil
        end)
    end

    -- One look for both shapes; only the handler decides which constructor takes it.
    local shell = {
        hover = hovered,
        width = "Fill",
        height = opts.height or theme.control.lg,
        align_v = "Center",
        radius = theme.radius.md,
        visible = opts.visible,
        opacity = opts.opacity,
        background = ground,
        border_width = opts.selected and theme.border_width or nil,
        border_color = opts.selected and theme.ACCENT or nil,
        children = { body },
    }
    if opts.on_activate == nil then
        return rect(shell)
    end
    shell.on_click = function(_, mouse_button)
        if mouse_button ~= "left" then
            return
        end
        opts.on_activate()
    end
    return button(shell)
end
