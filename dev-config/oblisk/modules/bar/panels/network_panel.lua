-- Mirrors NetworkPanel.qml: the access points in range, the connected one first, each a row you can
-- click to connect to.
--
-- A scrolling list rather than three summary lines, which is what this was. The summary was not a
-- design choice; `oblisk.network` has carried `available_networks` since ADR-0053 and the panel
-- could not show them, because a column taller than the popup was simply cut off with no way to
-- reach the rest. `scroll(name)` is what changed (docs/adr/0069).
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local section_header = require("components.section_header")
local panel_row = require("components.panel_row")
local panel_empty_state = require("components.panel_empty_state")

local KIND = "network"
local SCROLL = scroll("network_aps")

-- Connected first, then by descending signal. `table.sort` over a copy, because the payload table
-- belongs to the signal and sorting it in place would reorder what every other reader sees.
local function access_points(n)
    local points = {}
    for _, ap in ipairs((n and n.available_networks) or {}) do
        points[#points + 1] = ap
    end
    table.sort(points, function(a, b)
        if (a.active or false) ~= (b.active or false) then
            return a.active or false
        end
        return (a.strength or 0) > (b.strength or 0)
    end)
    return points
end

-- Four bars' worth of glyph, which is what a strength percentage actually reads as. These were
-- freedesktop theme names until now, under a comment claiming they were Nerd Font codepoints --
-- they were the one thing on the bar the comment described correctly and the code did not do.
local function strength_glyph(strength)
    local percent = strength or 0
    if percent >= 75 then
        return icons.wifi[4]
    elseif percent >= 50 then
        return icons.wifi[3]
    elseif percent >= 25 then
        return icons.wifi[2]
    end
    return icons.wifi[1]
end

local body = {
    row {
        width = "Fill",
        align_v = "Center",
        spacing = theme.spacing.sm,
        children = {
            section_header("network"),
            -- `Fill` between the header and the state pushes the state to the far edge, the same
            -- one-property trick `components/panel_header.lua` spends.
            cell(util.label(oblisk.network, function(n)
                return n.scanning and "scanning" or string.format("%d in range", util.count(n.available_networks))
            end), theme.TEXT_OFF, theme.font.xs, { width = "Fill", align = "End" }),
        },
    },
    list {
        width = "Fill",
        height = "Fill",
        scroll = SCROLL,
        spacing = theme.spacing.xs,
        source = oblisk.network:map(access_points),
        itemfn = function(ap)
            return panel_row {
                slot = "network-ap-" .. tostring(ap.ssid),
                icon = strength_glyph(ap.strength),
                title = ap.ssid or "?",
                -- `band` and `secure` are in the payload (§ 2.5) and nothing was reading them.
                subtitle = string.format("%d%%%s%s%s", ap.strength or 0, ap.band and (" " .. ap.band) or "", ap.secure and " lock" or " open", ap.active and " -- connected" or ""),
                color = ap.active and theme.ACCENT or theme.FG,
                on_activate = function()
                    -- `hidden` is a required second argument (§ 3.2), and every entry in
                    -- `available_networks` was found by a scan, so none of them is hidden.
                    oblisk.network:invoke("connect", ap.ssid, false)
                end,
            }
        end,
        key = function(ap)
            return tostring(ap.ssid)
        end,
    },
    panel_empty_state("nothing in range", util.shown_when(oblisk.network, function(n)
        return #access_points(n) == 0
    end)),
}

return { kind = KIND, body = body }
