-- One row of a panel list: a leading icon, a title with an optional subtitle under it, and a
-- trailing slot for whatever the row does. `Components/PanelRow.qml` is this shape, and it is what
-- every list in every bar panel is made of -- an access point, a bluetooth device, a notification.
--
-- The title column takes `width = "Fill"`, so the trailing slot sits against the right edge and the
-- title elides into whatever is left instead of pushing it out. That is the same one property
-- `components/panel_header.lua` spends and needed the same `Fill` fix in `scene.rs`.
--
-- `on_activate` is optional. A row with none is a `row`, not a `button`: a button that does nothing
-- still takes the pointer and still reads as clickable, which is worse than a plain line.
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
---@field color? Color|Bound
---@field icon_color? Color|Bound
---@field height? integer
---@field slot? string
---@field visible? boolean|Bound
---@field trailing? Node
---@field on_activate? fun()

---@param opts PanelRowOpts
return function(opts)
    local title_lines = { cell(opts.title, opts.color or theme.FG, theme.font.sm, { width = "Fill" }) }
    if opts.subtitle then
        title_lines[#title_lines + 1] = cell(opts.subtitle, theme.TEXT_OFF, theme.font.xs, { width = "Fill" })
    end

    local children = {}
    if opts.icon then
        -- A glyph, not a themed icon, for `components/icon_button.lua`'s reason: `Components/
        -- PanelRow.qml` tints its leading icon by state (a connected device accent, a failed one
        -- red) and `PaintStyle::Icon` carries no tint. `opts.art` takes a themed icon name instead,
        -- for the rows that show something the config did not choose -- an application's own icon.
        children[#children + 1] = cell(opts.icon, opts.icon_color or opts.color or theme.FG, theme.icon.md, { align_v = "Center" })
    elseif opts.art then
        children[#children + 1] = icon { name = opts.art, size = theme.icon.md, align_v = "Center" }
    end
    children[#children + 1] = column {
        width = "Fill",
        align_v = "Center",
        children = title_lines,
    }
    if opts.trailing then
        children[#children + 1] = opts.trailing
    end

    local body = row {
        width = "Fill",
        spacing = theme.spacing.sm,
        align_v = "Center",
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = children,
    }

    if opts.on_activate == nil then
        body.height = opts.height or theme.control.lg
        body.visible = opts.visible
        return body
    end

    local hovered = opts.slot and hover(opts.slot) or nil
    return button {
        hover = hovered,
        width = "Fill",
        height = opts.height or theme.control.lg,
        align_v = "Center",
        radius = theme.radius.sm,
        visible = opts.visible,
        background = hovered and hovered:map(function(is_hovered)
            return is_hovered and theme.GLASS_HOVER or nil
        end) or nil,
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            opts.on_activate()
        end,
        children = { body },
    }
end
