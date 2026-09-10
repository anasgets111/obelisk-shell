-- A Nerd Font glyph uses `theme.icon_font`, not the declared chain (ADR-0144). `cell` adds one
-- property because repeating it at twenty call sites leaves some calls without it.
--
-- The declared family is Nerd-Font-patched, so it carries these private-use codepoints and wins the
-- engine's per-glyph fallback. A `Propo` face spaces them proportionally and fills most of the em;
-- a `Mono` face fits each into one cell. At the same `theme.icon.*` size, the faces produce visibly
-- different icons, so naming the family chooses.
--
-- Takes `cell`'s own signature so a site converts by changing the function name and nothing else.
local cell = require("components.cell")
local theme = require("config.theme")

---@param content string|Bound
---@param color? Color|Bound
---@param size? integer
---@param opts? { width?: integer|"Fill", align?: "Start"|"Center"|"End", align_v?: "Start"|"Center"|"End", visible?: boolean|Bound, wrap?: "None"|"Word"|Bound, max_lines?: integer|Bound, on_link?: fun(href: string) }
return function(content, color, size, opts)
    -- Copy the inline options so each call gets `font` without mutating a shared table.
    local with_font = { font = theme.icon_font }
    for key, value in pairs(opts or {}) do
        with_font[key] = value
    end
    return cell(content, color, size, with_font)
end
