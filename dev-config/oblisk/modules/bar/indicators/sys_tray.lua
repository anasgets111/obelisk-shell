-- Mirrors SysTray.qml: the registered `StatusNotifierItem`s laid out horizontally, each item its
-- own borderless button carrying that application's own icon.
--
-- Themed icons here, not glyphs, and that split is the point of `components/icon_button.lua`'s
-- note: a tray item ships its own artwork and nobody gets to recolour it. Everything else on this
-- bar is a glyph precisely because this config chooses those and does not choose these.
--
-- One carve-out, and it is not really one: `foreground` on an `icon` is what CSS `color` is, the
-- value a `currentColor` fill resolves to (ADR-0072). A symbolic icon is *defined* as taking the
-- panel's colour, and Breeze bakes its own light-theme grey into the file expecting the toolkit to
-- rewrite it. Telegram's tray icon drew near-black on this bar until it did. A full-colour app
-- icon names no `currentColor` and ignores this, so it goes on every item unconditionally.
--
-- No ground, and absent when the tray is empty. It was a glass pill of a fixed 135px, which on a
-- session that registers nothing is an empty box sitting on the bar looking like a control that
-- failed to load, and on a session that registers two is a box with a lot of nothing to the right
-- of them. The fixed 150px that replaced it kept the second half of that: a `list` sizes to its
-- content when `width` is omitted, so stating one is a floor as well as a ceiling, and two tray
-- items sat in 110px of empty bar. The width is computed from the item count now, capped.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

local SCROLL = scroll("sys_tray")

local function items_of(t)
    return (t and t.items) or {}
end

-- The ceiling, not the width. A session that registers a dozen items scrolls rather than taking
-- the whole zone (docs/adr/0069). The mirror has no cap because a QML `RowLayout` shrinks its
-- children; a `row` here does not.
local TRAY_WIDTH = theme.s(150, 110)

-- Content-sized up to that ceiling, which § 5.1 has no `max_width` for. `width` takes a signal and
-- a signal resolves before the property is parsed (ADR-0044), so the item count can state the
-- number the engine would otherwise have measured -- the same trick `components/meter.lua` uses to
-- get a progress bar out of a `"NN%"` string.
--
-- ponytail: this re-derives the row's own measurement in Lua, and only `itemfn`'s icon branch is
-- `icon.md` wide. An item that registered no artwork falls back to two glyphs at `font.sm`, which
-- shape to something else, so a tray holding one of those is off by the difference. The upgrade
-- path is a `max_width` on § 5.1: the engine already knows every child's real width and this
-- guesses at it.
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
        local art = item.icon_name or item.icon_path or (entry and entry.icon)
        if art then
            return icon { name = art, size = theme.icon.md, align_v = "Center", foreground = theme.FG }
        end
        -- No artwork registered, which happens, and is what the faint smudge between the
        -- bluetooth circle and the clock was: two 9px letters at `DIM` next to a row of 22px
        -- glyphs reads as a rendering fault rather than as a fallback. Same weight as the icons
        -- it stands in for.
        return cell((item.name or item.id or "?"):sub(1, 2), theme.FG, theme.font.sm, { align_v = "Center" })
    end,
    key = function(item)
        return tostring(item.id)
    end,
}
