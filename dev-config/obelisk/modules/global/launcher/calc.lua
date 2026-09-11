-- Mirrors `Launcher/CalcProvider.qml`: an arithmetic query becomes one row whose Enter copies the
-- result.
--
-- `load` is the mirror's `Function("return (" + expr + ")")`. The config VM has it: mlua opens the
-- base library whatever `config_stdlib()` lists. Two guards rather than one, because the allowlist
-- admits no letters and so no identifier, and the empty `_ENV` leaves nothing to reach if one ever
-- got through.
--
-- `--` and `//` are refused. Both pass the allowlist, and Lua reads them as a comment and as
-- floor division: `2--3` would answer 2 and `10//3` would answer 3, where the mirror raises a
-- `SyntaxError` on each and shows no row. A wrong number is worse than no row.
--
-- Three rewrites put JavaScript's arithmetic back. `**` becomes `^`, undoing the mirror's own
-- `^`-to-`**`; a leading unary `+` goes, Lua having none; and every integer literal gains a `.0`,
-- because Lua 5.4 integers wrap where JavaScript Numbers do not, and `4294967296*4294967296`
-- otherwise answers 0 rather than 1.8e19.
--
-- The percent rewrite is kept verbatim, which is why `%` means percent rather than Lua's modulo
-- and `10%3` is refused by both configs.
local icons = require("config.icons")
local util = require("lib.util")

local M = {}

-- `/^[\d\s+\-*/().,%^]+$/`, in the order that pattern lists them.
local ALLOWED = "^[%d%s%+%-%*/%(%)%.,%%%^]+$"

---@param query string
---@return LauncherRow|nil
function M.claims(query)
    local input = util.trim(query)
    if not input:match(ALLOWED) or not input:match("%d") or not input:match("[%+%-%*/%^%%]") then
        return nil
    end
    -- `!/^\d+\.?\d*$/`: a bare number is not a calculation.
    if input:match("^%d+%.?%d*$") or input:find("--", 1, true) or input:find("//", 1, true) then
        return nil
    end
    local expression = input
        :gsub(",", "")
        :gsub("%*%*", "^")
        :gsub("^%s*%+", "")
        :gsub("(%d+%.?%d*)%%", "(%1/100)")
        :gsub("%d+%.?%d*", function(number)
            return number:find(".", 1, true) and number or (number .. ".0")
        end)
    local chunk = load("return " .. expression, "=calc", "t", {})
    if not chunk then
        return nil
    end
    local ok, value = pcall(chunk)
    -- `!Number.isFinite(value)` covers both infinities and NaN; `value ~= value` is the NaN half.
    if not ok or type(value) ~= "number" or value ~= value or value == math.huge or value == -math.huge then
        return nil
    end
    local result
    if value == math.floor(value) then
        result = util.thousands(string.format("%.0f", value))
    else
        -- `parseFloat(value.toPrecision(12)).toString()`: twelve significant digits, and `%g` drops
        -- the trailing zeros `parseFloat` drops.
        result = string.format("%.12g", value)
    end
    return {
        kind = "calc",
        badge = "CALC",
        hint = "Enter to copy",
        icon = icons.calc,
        title = input .. " = " .. result,
        subtitle = "Calculator",
        payload = result,
    }
end

return M
