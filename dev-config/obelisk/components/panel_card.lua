-- Shared background, padding, and radius for dropdowns and panel bodies.
-- `modules/shell/panel_host.lua` and `modules/bar/panels/settings.lua` repeated the same six
-- properties, which is the two-call-site bar for a shared component.
--
-- `opts` overrides defaults. By default, dropdowns use `theme.radius.md` and `theme.spacing.sm`.
-- The opaque settings toplevel passes `radius = 0` and its own padding because it has no edge to
-- round against.
--
-- Pass through `margin`, `align_h`, `align_v`, and `visible`: they locate the card. The panel host
-- positions it manually because a layer surface has no `anchor_rect`; its indicator offset is this
-- node's outer `card_margin`. The content-sized polkit prompt centres with the two aligns.
--
-- `modules/bar/panels/update_panel.lua` hides whole package/log cards; hiding children alone leaves
-- their ground and padding as a rounded empty rectangle.
local theme = require("config.theme")

return function(children, opts)
    opts = opts or {}
    return column {
        width = opts.width,
        height = opts.height,
        padding = opts.padding or {
            top = theme.spacing.sm,
            right = theme.spacing.md,
            bottom = theme.spacing.sm,
            left = theme.spacing.md,
        },
        margin = opts.margin,
        align_h = opts.align_h,
        align_v = opts.align_v,
        visible = opts.visible,
        opacity = opts.opacity,
        animate = opts.animate,
        spacing = opts.spacing or theme.spacing.xs,
        background = opts.background or theme.BG,
        -- Passed through rather than defaulted: a card on an already-blurred sheet asking again
        -- would union into a region that covers it, which is work for no pixels (ADR-0195).
        blur = opts.blur,
        radius = opts.radius or theme.radius.md,
        border_width = opts.border_width,
        border_color = opts.border_color,
        children = children,
    }
end
