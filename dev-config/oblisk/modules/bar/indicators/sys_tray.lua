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
-- The ground is the mirror's own: a `Rectangle` filling the tray, `glassControlColor` behind
-- `glassBorderColor` at `itemRadius`, with the row centred in it and no padding of its own -- the
-- gap around each icon is the button's width, not the pill's.
--
-- An earlier pass here had no ground at all, because a *fixed* 135px pill looked like a failed
-- control with no items and a fixed 150px left two items sitting in 110px of empty bar: a `list`
-- without `width` sizes to content, so a fixed width is both floor and ceiling. Computing the width
-- from item count fixed that, and the ground could come back. The mirror's empty state is the same
-- answer -- the pill shrinks to `emptyLabel.implicitWidth` and says "No tray items" rather than
-- hiding or standing empty.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local tray_menu = require("modules.bar.panels.tray_menu")

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
-- One square per item, `IconButton`'s `implicitWidth: _size` at the `md` step the mirror's delegate
-- takes. The room around each icon is this square minus `icon.md`, which is where the pill's
-- breathing space comes from -- it has no padding of its own. Squares also make the row measurable
-- from the count alone: the no-artwork fallback draws letters of some other width, and an
-- icon-plus-gap item width was off by that difference.
local ITEM_WIDTH = theme.control.md

local function tray_width(count)
    return math.max(0, math.min(TRAY_WIDTH, count * ITEM_WIDTH))
end

local has_items = util.shown_when(oblisk.tray, function(t)
    return #items_of(t) > 0
end)

local items = list {
    width = computed({ oblisk.tray }, function(t)
        return tray_width(#items_of(t))
    end),
    direction = "Horizontal",
    spacing = 0,
    align_v = "Center",
    scroll = SCROLL,
    visible = has_items,
    source = computed({ oblisk.tray, oblisk.applications }, items_of),
    itemfn = function(item)
        local entry = util.app_entry(oblisk.applications:get(), item.name or item.id)
        local art = artwork(item) or (entry and entry.icon)
        local face
        if art then
            -- `anchors.centerIn: parent`. Without `align_h` the artwork sits against the left edge
            -- of its square, which both lops the row's spacing onto one end and pushes the first
            -- icon under the pill's corner radius (`components/icon_button.lua` hit this with "EN").
            face = icon {
                name = art,
                size = theme.icon.md,
                align_h = "Center",
                align_v = "Center",
                foreground = theme.FG,
            }
        else
            -- No artwork happens. Two 9px `DIM` letters beside 22px glyphs looked like a rendering
            -- fault
            -- between Bluetooth and the clock, so match the icon weight.
            face = cell(
                (item.name or item.id or "?"):sub(1, 2),
                theme.FG,
                theme.font.sm,
                { align = "Center", align_v = "Center" }
            )
        end
        -- `SysTray.qml`'s `onClicked`: right opens the menu when there is one, left activates,
        -- anything else is the secondary activation. An item whose `item_is_menu` is set has no
        -- meaningful activation, so its left click opens the menu too rather than doing nothing.
        return button {
            width = ITEM_WIDTH,
            height = "Fill",
            align_v = "Center",
            on_click = function(rect_, mouse_button)
                local wants_menu = mouse_button == "right" or item.item_is_menu
                if wants_menu and item.menu ~= nil then
                    tray_menu.open(item, rect_)
                elseif mouse_button == "left" then
                    oblisk.tray:invoke("activate", item.id, 0, 0)
                elseif mouse_button == "middle" then
                    oblisk.tray:invoke("secondary_activate", item.id, 0, 0)
                end
            end,
            -- `onWheel`: the item decides what a notch means; ours is the vertical axis, which is
            -- the only one `on_wheel` reports (ADR-0116).
            on_wheel = function(_, notches)
                oblisk.tray:invoke("scroll", item.id, math.floor(notches), "vertical")
            end,
            children = { face },
        }
    end,
    key = function(item)
        return tostring(item.id)
    end,
}

-- `opacity: Theme.opacityMuted` on a `DIM` line, folded into the colour: `cell` takes no opacity,
-- and an alpha is what the mirror's opacity resolves to on one run of text.
local empty_label = cell(
    "No tray items",
    theme.with_opacity(theme.DIM, theme.opacity.muted),
    theme.font.xs,
    {
        align_v = "Center",
        visible = has_items:map(function(any)
            return not any
        end)
    }
)

return row {
    height = theme.item_height,
    align_v = "Center",
    radius = theme.item_radius,
    background = theme.GLASS_CONTROL,
    border_width = theme.border_width,
    border_color = theme.GLASS_BORDER,
    -- Both children are always here; an invisible one takes no width, no position and no gap, so
    -- the pill measures whichever is showing.
    children = { items, empty_label },
}
