-- One glyph in a circle, which is what `Components/IconButton.qml` is and what most of the bar is
-- made of. Twelve modules were each writing out the same button, radius, left-button guard and
-- child.
--
-- ## Why the glyph is text
--
-- This took an icon *theme name* and drew an `icon` node until now, and that is the single change
-- that makes this bar look like the one it mirrors. A themed icon is artwork looked up by name
-- (ADR-0054) and paints in its own colours: `PaintStyle::Icon` carries no tint, so a bluetooth
-- icon cannot go accent-coloured when a device connects and an update icon cannot go red on
-- failure. Every state this bar wants to show had to be spelled as a word beside the icon instead,
-- which is why the old bar read "bt", "apps", "up to date" where the mirror shows one glyph that
-- changes colour. A Nerd Font glyph is a `text` node, so `foreground` is just a property and the
-- state is the colour. `config/icons.lua` holds the codepoints.
--
-- Themed icons are still right for artwork nobody chose: a tray item's own icon, an application's
-- own icon. Those pass through `icon` nodes at their call sites and always did.
--
-- ## Colour
--
-- The foreground is derived, not passed: `theme.text_contrast(background)` picks black or white by
-- WCAG luminance, so a caller that sets a red background for an alert gets legible text without
-- also remembering to set the foreground. `opts.foreground` overrides it for the one case the
-- mirror also special-cases, a glyph tinted by state on an unchanged ground.
--
-- Left button only, the same guard `components/panel_row.lua` applies: a close or a toggle one
-- stray right-click away is a worse default than a click that does nothing. `opts.on_button` takes
-- the raw button name for the three modules that want a right-click of their own.
--
-- A nil `on_activate` gives a `row`, not a `button`, which is the same rule `components/panel_row.lua`
-- states for the same reason: a button that does nothing still takes the pointer and still reads as
-- clickable. Two indicators here are readouts the mirror can click and this engine cannot, because
-- the capability behind them exposes no command, and they should look like readouts.
local theme = require("config.theme")

return function(glyph, on_activate, opts)
    opts = opts or {}
    local side = opts.size or theme.item_height
    -- A circle unless asked otherwise, which is the mirror's default too. Half the side rather than
    -- a radius token, because a token that happened to be less than half would paint a rounded
    -- square and read as a near-miss rather than a decision.
    local radius = opts.radius or (opts.shape == "rounded" and theme.item_radius or side / 2)
    -- Annotated because a theme token is a `Signal` on a scaled display and a plain string
    -- otherwise, so inference picks whichever `theme.lua` happened to build and the `---@cast`
    -- below then has nothing to narrow.
    ---@type Color|Signal
    local base = opts.background or theme.GLASS_CONTROL
    ---@type Color|Signal
    local base_hover = opts.background_hover or theme.GLASS_CONTROL_HOVER

    -- One `hover(slot)` call, reused across the four properties that read it. The registry is
    -- name-keyed so a second call returns the same signal (ADR-0062 decision 2), but naming the
    -- slot four times reads as four regions.
    local hovered = opts.slot and hover(opts.slot) or nil

    -- Either ground may be a signal rather than a string: the updates circle turns accent when
    -- packages are waiting, the keyboard circle turns peach under caps lock. So the effective
    -- ground is resolved first and the foreground is derived from *that*, which is the only way
    -- `text_contrast` gets a colour to measure. Handing it the signal itself is what the first
    -- version did, and `channels` fails on the userdata with a traceback into `theme.lua`.
    local function is_signal(value)
        return type(value) == "userdata"
    end

    -- `---@type` and `---@cast` because these hold either a colour string or a `Signal`, chosen at
    -- runtime by `is_signal`, and the language server has no way to follow a
    -- `type(x) == "userdata"` test: there are no user-defined type guards. Without the annotations
    -- it narrows each local to whatever the first branch assigned and calls every other branch a
    -- type error. This is the one place in `dev-config` that needs them, which is what makes them
    -- worth writing rather than turning the diagnostic off.
    ---@type Color|Signal
    local ground
    if not hovered then
        ground = base
    elseif is_signal(base) and is_signal(base_hover) then
        ---@cast base Signal
        ---@cast base_hover Signal
        ground = computed({ hovered, base, base_hover }, function(is_hovered, plain, lit)
            return is_hovered and lit or plain
        end)
    elseif is_signal(base) then
        ---@cast base Signal
        ground = computed({ hovered, base }, function(is_hovered, plain)
            return is_hovered and base_hover or plain
        end)
    elseif is_signal(base_hover) then
        ---@cast base_hover Signal
        ground = computed({ hovered, base_hover }, function(is_hovered, lit)
            return is_hovered and lit or base
        end)
    else
        ground = hovered:map(function(is_hovered)
            return is_hovered and base_hover or base
        end)
    end

    ---@type Color|Signal
    local foreground = opts.foreground
    if foreground == nil then
        if is_signal(ground) then
            ---@cast ground Signal
            foreground = ground:map(theme.text_contrast)
        else
            ---@cast ground Color
            foreground = theme.text_contrast(ground)
        end
    end

    ---@type Color|Signal
    local border_color = theme.GLASS_BORDER
    if hovered then
        border_color = hovered:map(function(is_hovered)
            return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
        end)
    end
    local background = ground

    -- `selected` marks the button whose panel is open with an accent ring, which is how the mirror
    -- shows which of five indicators the popup belongs to. It beats the hover border rather than
    -- blending with it, so a selected button under the pointer still reads as selected.
    if opts.selected ~= nil then
        border_color = opts.selected:map(function(is_selected)
            return is_selected and theme.ACCENT or theme.GLASS_BORDER
        end)
    end

    -- `align_h` on the node itself, and it is not redundant with the one on the glyph below.
    -- `layout::scene` reads the same key for two jobs: on a stacking parent's *child* it places
    -- that child, and on a `row` it is the row's own main-axis alignment. A nil `on_activate` makes
    -- this a `row`, and a `row` ignores its children's `align_h` entirely -- which is why the
    -- keyboard circle drew "EN" hard against its left edge while every neighbouring `button` looked
    -- fine. Set on both, so the two shapes this function returns centre their glyph the same way.
    local node = {
        width = opts.width or side,
        height = side,
        align_h = "Center",
        align_v = "Center",
        hover = hovered,
        radius = radius,
        background = background,
        opacity = opts.opacity,
        visible = opts.visible,
        border_width = opts.border == false and nil or theme.border_width,
        border_color = opts.border == false and nil or border_color,
        children = { text {
            content = glyph,
            foreground = foreground,
            -- The glyph's own size, not an icon box: a `text` node measures the string, so this is
            -- the em size the face is rasterised at. `icon.lg` is the mirror's `iconSizeFor("md")`
            -- once its 24px base has been through the scale.
            font_size = opts.icon_size or theme.icon.lg,
            align_h = "Center",
            align_v = "Center",
        } },
    }

    if on_activate == nil and opts.on_button == nil then
        return row(node)
    end

    node.on_click = function(rect, mouse_button)
        if opts.on_button then
            opts.on_button(rect, mouse_button)
            return
        end
        if mouse_button ~= "left" then
            return
        end
        on_activate(rect, mouse_button)
    end
    return button(node)
end
