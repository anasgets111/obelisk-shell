-- One glyph in a circle, matching `Components/IconButton.qml`; twelve modules had duplicated its
-- button, radius, left-button guard, and child.
-- ## Why the glyph is text
-- It used an icon *theme name* and an `icon` node. The mirror needs state-coloured glyphs: themed
-- artwork is name-looked-up (ADR-0054), and `PaintStyle::Icon` has no tint, so bluetooth cannot
-- turn
-- accent on connect or update turn red on failure. The old bar spelled states "bt", "apps", and
-- "up to date"; a Nerd Font glyph is a `text` node whose `foreground` carries the state. Codepoints
-- live in `config/icons.lua`.
-- Themed icons remain right for unchosen artwork, such as a tray item's or application's own icon;
-- those call sites still use `icon` nodes.
-- ## Colour
-- Derive foreground with `theme.text_contrast(background)`, which picks black or white by WCAG
-- luminance. A red alert background stays legible without a second caller setting;
-- `opts.foreground`
-- overrides this for the mirror's special case, a state-tinted glyph on an unchanged ground.
-- Left button only, matching `components/panel_row.lua`: a close or toggle one stray right-click
-- away is worse than a no-op. `opts.on_button` receives the raw button name for the three modules
-- that need their own right-click.
-- Nil `on_activate` returns a `row`, matching `components/panel_row.lua`: a no-op button still
-- takes
-- the pointer and reads as clickable. Two indicators are mirror-clickable readouts whose capability
-- exposes no command here, so they must look like readouts.
local theme = require("config.theme")

return function(glyph, on_activate, opts)
    opts = opts or {}
    local side = opts.size or theme.item_height
    -- A circle by default, matching the mirror. Use half the side instead of a radius token: a
    -- token
    -- below half paints a rounded square.
    local radius = opts.radius or (opts.shape == "rounded" and theme.item_radius or side / 2)
    -- Annotated because a theme token is a `Signal` on a scaled display and a string otherwise;
    -- type inference then picks whichever `theme.lua` happened to build, leaving `---@cast` nothing
    -- to narrow.
    ---@type Color|Signal
    local base = opts.background or theme.GLASS_CONTROL
    ---@type Color|Signal
    local base_hover = opts.background_hover or theme.GLASS_CONTROL_HOVER

    -- Reuse one `hover(slot)` signal across four properties. The registry is name-keyed, so a
    -- second call returns the same signal (ADR-0062 decision 2), but repeating the slot reads as
    -- four regions.
    local hovered = opts.slot and hover(opts.slot) or nil

    -- Either ground may be a signal: the updates circle turns accent for waiting packages and the
    -- keyboard circle peach under caps lock. Resolve it before deriving foreground, because
    -- `text_contrast` needs a colour; passing the signal made `channels` fail on userdata in
    -- `theme.lua`.
    local function is_signal(value)
        return type(value) == "userdata"
    end

    -- `---@type` and `---@cast` are needed because runtime `is_signal` distinguishes strings and
    -- `Signal`, but the language server cannot follow `type(x) == "userdata"` without user-defined
    -- type guards. Otherwise it narrows each local to the first branch and flags the others. This
    -- is the only `dev-config` site needing them, so keep the annotations instead of disabling
    -- diagnostics.
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

    -- `selected` marks the open panel with an accent ring, identifying which of five indicators
    -- owns
    -- the popup. It replaces the hover border so selection remains legible under the pointer.
    if opts.selected ~= nil then
        border_color = opts.selected:map(function(is_selected)
            return is_selected and theme.ACCENT or theme.GLASS_BORDER
        end)
    end

    -- `align_h` on the node is not redundant with the glyph's. `layout::scene` uses it to place a
    -- stacking parent's child, but as a `row`'s main-axis alignment. A nil `on_activate` returns a
    -- `row`, which ignores child `align_h`; that left keyboard "EN" against the edge while buttons
    -- were centred. Set both so either returned shape centres its glyph.
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
        -- `opts.border == false` drops the ring. Use an `if`: `x and nil or y` is always `y`, which
        -- previously gave a ring to every caller asking for none.
        border_width = theme.border_width,
        border_color = border_color,
        -- `IconButton.qml`'s two `ColorAnimation`s: the ground and ring ease under the pointer and
        -- on selection (ADR-0145).
        animate = { background = theme.animation_ms, border_color = theme.animation_ms },
        children = { text {
            content = glyph,
            foreground = foreground,
            -- The glyph's own size, not an icon box. A `text` node measures the string, so this is
            -- the face's rasterised em size.
            --
            -- `icon.md`, because `IconButton.qml` defaults `size: "md"` and no bar indicator
            -- overrides it, so `iconSizeFor("md")` is `s(18, 14)`. This read `icon.lg` on a comment
            -- claiming that was the same number; it is `s(24, 18)`, the mirror's `iconSizeLg`, so
            -- every circle on the bar drew its glyph a third too large. Invisible on a wifi arc or
            -- a bell, obvious the moment one of them was a filled square.
            font_size = opts.icon_size or theme.icon.md,
            -- The declared chain, *not* `theme.icon_font`. `Theme.qml` has both faces and
            -- `IconButton.qml` picks the body one -- `font.family: Theme.fontFamily`,
            -- CaskaydiaCove Nerd Font Propo -- while `iconFontFamily`, JetBrainsMono Nerd Font
            -- Mono, is what the panel components use. `NetworkIndicator.qml` and
            -- `DateTimeDisplay.qml` draw their glyphs the same way, so the split is bar versus
            -- panel, not glyph versus text.
            --
            -- This passed `icon_font`, so every circle on the bar drew the mirror's codepoint in
            -- the wrong face. The codepoints were already right, which is why it read as "all the
            -- icons look off" rather than as any one wrong icon: JetBrainsMono's Material glyphs
            -- are lighter and narrower than CaskaydiaCove's at the same pixel size.
            -- `components/glyph.lua` keeps `icon_font`, because its callers are the panel
            -- components that use `iconFontFamily` there.
            align_h = "Center",
            align_v = "Center",
        } },
    }

    if opts.border == false then
        node.border_width = nil
        node.border_color = nil
    end

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
