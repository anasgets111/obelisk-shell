-- The background-plus-padding-plus-radius block every dropdown and panel body turns out to be.
-- `modules/shell/panel_host.lua`'s card and `modules/bar/panels/settings.lua`'s window
-- child were the same six properties before this existed, which is exactly `components/pill.lua`'s
-- own reason for being: two call sites in agreement is a component, not a coincidence.
--
-- `opts` overrides rather than replaces: a card called with no options at all is the dropdown
-- shape (10px radius, 6px spacing), since that is the more common of the two. The settings window
-- wants `radius = 0` (an opaque toplevel has no edge to round against) and its own padding, and
-- passes both explicitly.
--
-- `margin` is the one property here that is not a look. It is passed through because the panel host
-- places its card by hand: a layer surface has no `anchor_rect` to hang from, so the offset from the
-- indicator that opened it is an outer margin on this node (see that file's `card_margin`).
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
        spacing = opts.spacing or theme.spacing.xs,
        background = opts.background or theme.BG,
        radius = opts.radius or theme.radius.md,
        border_width = opts.border_width,
        border_color = opts.border_color,
        children = children,
    }
end
