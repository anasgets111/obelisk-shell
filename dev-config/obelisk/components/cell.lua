-- Shared bar text cell with default size and weight; modules use it instead of a local.
-- `elide = "End"` only affects bounded `opts.width`; content-sized cells measure to fit. The engine
-- (`layout::scene::fit_text_to_box`) measures before searching, and modules supply boxes to cut.
-- `opts.wrap` moves the ellipsis to the last of `opts.max_lines` lines (ADR-0089). It is off by
-- default because a second line grows fixed-height bar slots; cards can opt in.
-- The old `util.truncate(s, limit)` served eight modules, but character counts are the wrong unit:
-- "WWWWWWWWWW" and "iiiiiiiiii" have ten characters but different widths, so the reader sees a box.
-- `opts.align` sets `text_align` and `align_h`: centring depends on the parent.
-- `text_align` centres inside the cell; `align_h` places a content-sized box in its parent.
-- Stacking parents read it (`rect`, `button`, a surface; `layout::scene`'s catch-all arm). Only
-- `text_align` left workspace digits at the dot's left edge: their own-width box began at x=0
-- in a 22px button. `row` ignores child `align_h`; `column` uses it on the cross axis.
local theme = require("config.theme")

-- Last hop before `text.content`: a wrong shape fails re-resolve. `notification` payloads pass
-- through a `list`'s `itemfn`, typed by `lua-meta/nodes.lua` as `fun(item: any)` because `Signal`
-- has no element type. A raw span array could reach `content` and freeze the shell on its last good
-- scene.
-- `TextRun[]` is accepted (ADR-0104), but image spans are not text;
-- `util.notification_body` converts them.
---@param content string|TextRun[]|Bound
---@param color? Color|Bound
---@param size? integer
---@param opts? { width?: integer|"Fill", align?: "Start"|"Center"|"End", align_v?: "Start"|"Center"|"End", visible?: boolean|Bound, wrap?: "None"|"Word"|Bound, max_lines?: integer|Bound, on_link?: fun(href: string), font?: "Body"|"Icon"|Bound }
return function(content, color, size, opts)
    opts = opts or {}
    return text {
        content = content,
        foreground = color or theme.FG,
        font_size = size or theme.font.md,
        font = opts.font,
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
