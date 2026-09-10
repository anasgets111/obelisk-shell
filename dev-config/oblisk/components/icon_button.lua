-- One glyph in a circle, matching `Components/IconButton.qml`; twelve modules had duplicated its
-- button, radius, left-button guard, and child.
-- It used an icon *theme name* and an `icon` node. The mirror needs state-coloured glyphs: themed
-- artwork is name-looked-up (ADR-0054), and `PaintStyle::Icon` has no tint, so bluetooth cannot
-- turn accent on connect or make the update red on failure. The old bar spelled "bt", "apps", and
-- "up to date"; a Nerd Font glyph is a `text` node whose `foreground` carries the state. Codepoints
-- live in `config/icons.lua`.
-- Themed icons remain for unchosen artwork, such as tray or application icons; those callers use
-- `icon` nodes.
-- Derive foreground with `theme.text_contrast(background)`, which picks black or white by WCAG
-- luminance. Red alert backgrounds stay legible by default; `opts.foreground`
-- overrides this for the mirror's special case, a state-tinted glyph on an unchanged ground.
-- Left button only, matching `components/panel_row.lua`: a close or toggle one stray right-click
-- away is worse than a no-op. `opts.on_button` gets the raw button for three right-click modules.
-- Nil `on_activate` returns a `row`, matching `components/panel_row.lua`: a no-op button still
-- takes the pointer and reads clickable. Two mirror-clickable indicators expose no command, so they
-- must look like readouts.
local theme = require("config.theme")

return function(glyph, on_activate, opts)
    opts = opts or {}
    local side = opts.size or theme.item_height
    -- A circle by default, matching the mirror. Use half the side instead of a radius token because
    -- a token below half paints a rounded square.
    local radius = opts.radius or (opts.shape == "rounded" and theme.item_radius or side / 2)
    -- A theme token is a `Signal` on a scaled display and a string otherwise; inference cannot
    -- narrow the union without these annotations and casts.
    ---@type Color|Signal
    local base = opts.background or theme.GLASS_CONTROL
    ---@type Color|Signal
    local base_hover = opts.background_hover or theme.GLASS_CONTROL_HOVER

    -- Reuse one `hover(slot)` signal across four properties. The registry is name-keyed, so
    -- repeated calls return the same signal (ADR-0062 decision 2), but repeating the slot reads
    -- as four regions.
    local hovered = opts.slot and hover(opts.slot) or nil

    -- Either ground may be a signal: updates turn accent while packages wait; the keyboard turns
    -- peach under caps lock. Resolve it before `text_contrast`; passing it to `channels` failed on
    -- signal userdata in `theme.lua`.
    local function is_signal(value)
        return type(value) == "userdata"
    end

    -- `---@type` and `---@cast` are needed because the language server cannot follow
    -- `type(x) == "userdata"` as a type guard. Without them it narrows the union and flags it.
    -- This is the only `dev-config` site needing them, so keep the annotations rather than
    -- disable diagnostics.
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

    -- `selected` marks which of five indicators owns the open panel's popup. Its accent ring
    -- replaces the hover border so selection remains legible under the pointer.
    if opts.selected ~= nil then
        border_color = opts.selected:map(function(is_selected)
            return is_selected and theme.ACCENT or theme.GLASS_BORDER
        end)
    end

    -- `align_h` is needed on node and glyph. `layout::scene` uses it for stacking parents; a `row`
    -- uses its main axis. A nil `on_activate` returns a `row` that ignores child `align_h`, leaving
    -- keyboard "EN" against the edge. Set both to centre either.
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
            -- `icon.md`: `IconButton.qml` defaults `size: "md"` and no bar indicator overrides it,
            -- so
            -- `iconSizeFor("md")` is `s(18, 14)`. `icon.lg` is `s(24, 18)`, the mirror's
            -- `iconSizeLg`, so every circle on the bar drew its glyph a third too large. A filled
            -- square exposed it sooner than a wifi arc or bell.
            font_size = opts.icon_size or theme.icon.md,
            -- Use the declared chain, *not* `theme.icon_font`. `Theme.qml` has both faces, and
            -- `IconButton.qml` picks `font.family: Theme.fontFamily`, CaskaydiaCove Nerd Font
            -- Propo; panel components use `iconFontFamily`, JetBrainsMono Nerd Font Mono. The split
            -- is bar versus panel, not glyph versus text; `NetworkIndicator.qml` and
            -- `DateTimeDisplay.qml` draw their glyphs the same way.
            --
            -- Passing `icon_font` put every bar circle's correct codepoint in the wrong face. It
            -- read as "all the icons look off" because JetBrainsMono's Material glyphs are lighter
            -- and narrower than CaskaydiaCove's at the same pixel size.
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
