-- Mirrors `SysTray.qml`: registered `StatusNotifierItem`s laid out horizontally as borderless
-- buttons carrying their applications' icons.
--
-- Use themed icons, not glyphs: tray items ship artwork and cannot be recoloured, as
-- `components/icon_button.lua` notes. The rest of this bar chooses glyphs explicitly.
--
-- `icon.foreground` is CSS `color`, which resolves `currentColor` (ADR-0072). Symbolic icons take
-- the panel colour; Breeze bakes light-theme grey for the toolkit to rewrite. Telegram drew
-- near-black until this was set. Full-colour icons ignore `currentColor`, so apply it to every
-- item.
--
-- The ground is the mirror's own: a `Rectangle` filling the tray, `glassControlColor` behind
-- `glassBorderColor` at `itemRadius`, with a centred row and no padding -- the gap around each icon
-- is the button's width, not the pill's.
--
-- An earlier pass had no ground: a *fixed* 135px pill looked like a failed control with no items,
-- while a fixed 150px left two items in 110px of empty bar. A `list` without `width` sizes to
-- content, so a fixed width is both floor and ceiling. Item-count sizing fixes that, and the ground
-- can remain. The mirror's empty state matches it: shrink to `emptyLabel.implicitWidth` and say
-- "No tray items" rather than hide or stand empty.
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
-- mirror has no cap because QML `RowLayout` shrinks children; this `row` does not.
local TRAY_WIDTH = theme.s(150, 110)

-- One square per item, `IconButton`'s `implicitWidth: _size` at the `md` step the mirror's delegate
-- takes. The square minus `icon.md` is the pill's breathing space; it has no padding. Equal squares
-- keep the row even, where fallback letters and icon-plus-gap widths would not.
local ITEM_WIDTH = theme.control.md

local has_items = util.shown_when(oblisk.tray, function(t)
    return #items_of(t) > 0
end)

-- No `width`: the row measures its own children and stops at `max_width`, which is the same
-- `min(count * ITEM_WIDTH, TRAY_WIDTH)` the config used to compute, with `spacing = 0` and every
-- child a fixed `ITEM_WIDTH`.
local items = list {
    max_width = TRAY_WIDTH,
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
            -- `anchors.centerIn: parent`. Without `align_h`, artwork sits at the square's left
            -- edge, lops spacing onto one end and pushes the first icon under the pill's corner
            -- radius (`components/icon_button.lua` hit this with "EN").
            face = icon {
                name = art,
                size = theme.icon.md,
                align_h = "Center",
                align_v = "Center",
                foreground = theme.FG,
            }
        else
            -- No artwork happens. Two 9px `DIM` letters beside 22px glyphs looked like a rendering
            -- fault between Bluetooth and the clock, so match the icon weight.
            face = cell(
                (item.name or item.id or "?"):sub(1, 2),
                theme.FG,
                theme.font.sm,
                { align = "Center", align_v = "Center" }
            )
        end
        -- `SysTray.qml`'s `onClicked`: right opens a menu, left activates, and anything else is
        -- secondary activation. `item_is_menu` makes left click open the menu when one exists
        -- instead of becoming a no-op.
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
            -- `onWheel`: the item decides what a notch means; ours uses the vertical axis, the only
            -- one `on_wheel` reports (ADR-0116).
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
-- so alpha supplies the mirror's opacity for one run of text.
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
    -- Both children stay here; an invisible one takes no width, position or gap, so the pill
    -- measures whichever is showing.
    children = { items, empty_label },
}
