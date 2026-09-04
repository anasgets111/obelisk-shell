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
-- `margin` and `visible` are the two properties here that are not a look, and both are passed
-- through for the same reason: they are facts about where the card is, not about how it is drawn.
-- The panel host places its card by hand, because a layer surface has no `anchor_rect` to hang from
-- and the offset from the indicator that opened it is an outer margin on this node (see that file's
-- `card_margin`). `modules/bar/panels/update_panel.lua` shows and hides two whole cards -- the
-- package table and the log -- and hiding a card by hiding each of its children leaves its ground
-- and its padding behind, which is a rounded empty rectangle where nothing is happening.
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
        visible = opts.visible,
        spacing = opts.spacing or theme.spacing.xs,
        background = opts.background or theme.BG,
        radius = opts.radius or theme.radius.md,
        border_width = opts.border_width,
        border_color = opts.border_color,
        children = children,
    }
end
