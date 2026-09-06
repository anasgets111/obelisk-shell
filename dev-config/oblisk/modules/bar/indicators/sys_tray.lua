-- Mirrors SysTray.qml: registered `StatusNotifierItem`s laid out horizontally as borderless buttons
-- carrying their applications' icons.
--
-- Use themed icons, not glyphs: tray items ship their own artwork and cannot be recoloured, as
-- `components/icon_button.lua` notes. The rest of this bar chooses its glyphs explicitly.
--
-- `icon.foreground` is CSS `color`, the value `currentColor` resolves to (ADR-0072). Symbolic icons
-- are defined to take the panel colour; Breeze bakes light-theme grey into the file for the toolkit
-- to rewrite. Telegram drew near-black until this was set. Full-colour icons have no `currentColor`
-- and ignore it, so apply it to every item.
--
-- No ground, and hidden when empty. A fixed 135px pill looked like a failed control with no items;
-- fixed 150px left two items in 110px of empty bar because a `list` without `width` sizes to
-- content,
-- so a fixed width is both floor and ceiling. Compute width from item count, capped.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

local SCROLL = scroll("sys_tray")

-- Hide `Passive`: the spec treats it as no presentation, so disabling an application's tray icon
-- removes it. This is presentation policy in Lua; other bars may dim Passive items.
local function items_of(t)
    local out = {}
    for _, item in ipairs((t and t.items) or {}) do
        if item.status ~= "Passive" then
            out[#out + 1] = item
        end
    end
    return out
end

-- `NeedsAttention` uses supplied attention artwork. Telegram instead rewrites `icon_name` to
-- `-attention-symbolic`, so fallback to the base pair rather than drawing nothing.
local function artwork(item)
    if item.status == "NeedsAttention" and (item.attention_icon_name or item.attention_icon_path) then
        return item.attention_icon_name or item.attention_icon_path
    end
    return item.icon_name or item.icon_path
end

-- The ceiling, not the width: a dozen items scroll instead of taking the zone (ADR-0069). The
-- mirror
-- has no cap because QML `RowLayout` shrinks children; this `row` does not.
local TRAY_WIDTH = theme.s(150, 110)

-- Content-sized up to the ceiling; § 5.1 has no `max_width`. `width` accepts a signal resolved
-- before
-- the property is parsed (ADR-0044), so item count supplies the width, like `components/meter.lua`
-- turns `"NN%"` into a progress bar.
--
-- ponytail: Lua re-derives row measurement. The icon branch is `icon.md`, but no-artwork fallback
-- uses two `font.sm` glyphs, so a tray containing one is off by that difference. Upgrade path:
-- § 5.1
-- `max_width`, letting the engine use real child widths.
local ITEM_WIDTH = theme.icon.md + theme.spacing.xs

local function tray_width(count)
    return math.max(0, math.min(TRAY_WIDTH, count * ITEM_WIDTH - theme.spacing.xs))
end

return list {
    width = computed({ oblisk.tray }, function(t)
        return tray_width(#items_of(t))
    end),
    direction = "Horizontal",
    spacing = theme.spacing.xs,
    align_v = "Center",
    scroll = SCROLL,
    visible = util.shown_when(oblisk.tray, function(t)
        return #items_of(t) > 0
    end),
    source = computed({ oblisk.tray, oblisk.applications }, items_of),
    itemfn = function(item)
        local entry = util.app_entry(oblisk.applications:get(), item.name or item.id)
        local art = artwork(item) or (entry and entry.icon)
        if art then
            return icon { name = art, size = theme.icon.md, align_v = "Center", foreground = theme.FG }
        end
        -- No artwork happens. Two 9px `DIM` letters beside 22px glyphs looked like a rendering
        -- fault
        -- between Bluetooth and the clock, so match the icon weight.
        return cell((item.name or item.id or "?"):sub(1, 2), theme.FG, theme.font.sm, { align_v = "Center" })
    end,
    key = function(item)
        return tostring(item.id)
    end,
}
