-- A Nerd Font glyph, drawn in `theme.icon_font` instead of the declared chain (ADR-0144).
-- `cell` with one property set, because that property is the whole distinction and repeating it at
-- twenty call sites is how half of them end up missing it.
--
-- Why it exists at all: the declared family here is Nerd-Font-patched, so it carries these
-- private-use codepoints itself and wins the engine's per-glyph fallback. A `Propo` face draws them
-- proportionally spaced and filling most of the em; a `Mono` face fits each into one cell. At the
-- same `theme.icon.*` size those are visibly different icons, and only naming the family chooses.
--
-- Takes `cell`'s own signature so a site converts by changing the function name and nothing else.
local cell = require("components.cell")
local theme = require("config.theme")

---@param content string|Bound
---@param color? Color|Bound
---@param size? integer
---@param opts? { width?: integer|"Fill", align?: "Start"|"Center"|"End", align_v?: "Start"|"Center"|"End", visible?: boolean|Bound, wrap?: "None"|"Word"|Bound, max_lines?: integer|Bound, on_link?: fun(href: string) }
return function(content, color, size, opts)
    -- Copied rather than written through: callers build these tables inline, but a shared one
    -- would pick up `font` from whichever call reached it first.
    local with_font = { font = theme.icon_font }
    for key, value in pairs(opts or {}) do
        with_font[key] = value
    end
    return cell(content, color, size, with_font)
end
