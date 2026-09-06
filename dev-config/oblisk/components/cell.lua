-- One run of text at the bar's default size and weight. Every module uses it, so it lives here
-- rather than as a `shell.lua` local.
-- `elide = "End"` is unconditional but only matters with `opts.width`. It is free on content-sized
-- cells because their measured box fits the string
-- (`layout::scene::fit_text_to_box` measures before searching); the module supplies bounded boxes
-- for the engine to cut.
-- `opts.wrap` turns the cut line into `opts.max_lines` lines, moving the ellipsis to the last one
-- (ADR-0089). It is off by default because a second line grows a bar's fixed-height slot; cards can
-- opt in.
-- Replaced the deleted `util.truncate(s, limit)`, called by eight modules. Character counts are the
-- wrong unit: "WWWWWWWWWW" and "iiiiiiiiii" have ten characters but different widths, so config
-- limits guessed against one string failed on the next. The reader sees a box.
-- `opts.align` sets both properties because "centre this text" depends on the parent. `text_align`
-- centres inside the cell's box, but does nothing for a content-sized cell; `align_h` places that
-- box in its parent and is read by stacking parents (`rect`, `button`, a surface; `layout::scene`'s
-- catch-all arm). Setting only `text_align` left every workspace digit at the dot's left edge: its
-- own-width box began at x=0 in a 22px button. A `row` ignores child `align_h`; a `column` uses it
-- on the cross axis, where centring is intended.
local theme = require("config.theme")

-- Annotated because this is the last hop before `text.content`, whose wrong shape fails the whole
-- re-resolve. `notification` payloads pass through a `list`'s `itemfn`, typed by
-- `lua-meta/nodes.lua` as `fun(item: any)` because `Signal` has no element type. That let a raw
-- span array reach `content`
-- and freeze the shell on its last good scene. `TextRun[]` is accepted (ADR-0104), but image spans
-- still are not text; `util.notification_body` converts them.
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
