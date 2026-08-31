-- Mirrors SysTray.qml: the registered `StatusNotifierItem`s laid out horizontally, each item its
-- own borderless button carrying that application's own icon.
--
-- Themed icons here, not glyphs, and that split is the point of `components/icon_button.lua`'s
-- note: a tray item ships its own artwork and nobody gets to recolour it. Everything else on this
-- bar is a glyph precisely because this config chooses those and does not choose these.
--
-- No ground, and absent when the tray is empty. It was a glass pill of a fixed 135px, which on a
-- session that registers nothing is an empty box sitting on the bar looking like a control that
-- failed to load, and on a session that registers two is a box with a lot of nothing to the right
-- of them. The width is still capped, because a `row` does not shrink its children and a dozen
-- registrations would otherwise push the clock off the edge; with no ground behind it that cap
-- reads as whitespace rather than as a container.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

local SCROLL = scroll("sys_tray")

-- Bounded, so a session that registers a dozen items scrolls rather than taking the whole zone
-- (docs/adr/0069). The mirror has no cap because a QML `RowLayout` shrinks its children; a `row`
-- here does not.
local TRAY_WIDTH = theme.s(150, 110)

local function items_of(t)
    return (t and t.items) or {}
end

return list {
    width = TRAY_WIDTH,
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
            return icon { name = art, size = theme.icon.md, align_v = "Center" }
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
