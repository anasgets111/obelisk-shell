-- Mirrors `Launcher/CalcProvider.qml`: an arithmetic query becomes one row whose Enter copies the
-- result.
--
-- `load` is the mirror's `Function("return (" + expr + ")")`. The config VM has it: mlua opens the
-- base library whatever `config_stdlib()` lists. Two guards rather than one, because the allowlist
-- admits no letters and so no identifier, and the empty `_ENV` leaves nothing to reach if one ever
-- got through.
--
-- `--` is refused. It passes the allowlist and Lua reads it as a comment, so `2--3` would answer 2
-- where the mirror answers 5; a wrong number is worse than no row.
--
-- `^` needs no rewrite, being exponentiation already. The percent rewrite is kept verbatim, which
-- is why `%` means percent rather than Lua's modulo and `10%3` is refused by both configs.
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
    if input:match("^%d+%.?%d*$") or input:find("--", 1, true) then
        return nil
    end
    local expression = input:gsub(",", ""):gsub("(%d+%.?%d*)%%", "(%1/100)")
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
