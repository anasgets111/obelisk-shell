-- One run of text at the bar's default size and weight. The smallest thing every module is built
-- out of, which is why it is a component rather than a local in `shell.lua`.
--
-- `opts.width` is what makes the elide below do anything. `elide = "End"` is declared on every cell
-- unconditionally and costs nothing on a cell that is content-sized, because such a node's box came
-- from measuring the very string in it and therefore always fits it
-- (`layout::scene::fit_text_to_box` measures before it searches). It bites only where a caller has
-- bounded the box, which is the point: a module says how much room it will take and the engine cuts
-- the string to it, instead of every caller guessing a character budget.
--
-- `opts.wrap` turns that one cut line into `opts.max_lines` of them, with the ellipsis moving to
-- the last one (ADR-0089). Off by default: a bar cell is a fixed-height slot and a second line
-- would grow it, so wrapping is for the cards that have room to grow.
--
-- This replaced a `util.truncate(s, limit)` that eight modules called, now deleted. It counted
-- characters, which is the wrong unit: "WWWWWWWWWW" and "iiiiiiiiii" are the same ten characters
-- and twice different widths, so every limit in the config was a guess tuned against one string
-- and wrong for the next. A box is the unit the reader actually sees.
--
-- `opts.align` sets both alignments, and it has to, because "centre this text" means two different
-- properties depending on the parent and a caller should not have to know which. `text_align`
-- centres the string inside the cell's own box and does nothing at all to a content-sized cell,
-- whose box is the string. `align_h` places the cell inside its parent, and is what a stacking
-- parent (`rect`, `button`, a surface) reads (`layout::scene`'s catch-all arm). Setting only the
-- first is why every workspace digit sat against the left edge of its dot: the text was centred in
-- a box exactly its own width, and that box was at x=0 of a 22px button. A `row` ignores `align_h`
-- on its children and a `column` reads it as the cross axis, where centring is what the caller
-- meant anyway, so setting both is right everywhere rather than merely harmless.
local theme = require("config.theme")

-- Annotated, unlike most of `components/`, because this is the last hop before `text.content`, a
-- property the engine fails the whole re-resolve over when it is the wrong shape. `notification`
-- payloads reach here through a `list`'s `itemfn`, which `lua-meta/nodes.lua` types as
-- `fun(item: any)` because a `Signal` carries no element type, so `any` used to flow all the way
-- down and a raw span array landed in `content` with nothing between the stub that got it right
-- and the shell freezing on its last good scene. A `TextRun[]` is accepted now (ADR-0104); a
-- span array still is not, since an image span has no `text` -- `util.notification_body` is the
-- step between.
---@param content string|TextRun[]|Bound
---@param color? Color|Bound
---@param size? integer
---@param opts? { width?: integer|"Fill", align?: "Start"|"Center"|"End", align_v?: "Start"|"Center"|"End", visible?: boolean|Bound, wrap?: "None"|"Word"|Bound, max_lines?: integer|Bound, on_link?: fun(href: string) }
return function(content, color, size, opts)
    opts = opts or {}
    return text {
        content = content,
        foreground = color or theme.FG,
        font_size = size or theme.font.md,
        width = opts.width,
        align_v = opts.align_v,
        align_h = opts.align,
        visible = opts.visible,
        text_align = opts.align,
        elide = "End",
        wrap = opts.wrap,
        max_lines = opts.max_lines,
        on_link = opts.on_link,
    }
end
