-- Mirrors BluetoothPanel.qml: a masthead with the radio switch, then the paired devices and
-- whatever discovery has turned up, in two named sections.
--
-- Laid out as the mirror lays it out, which it was not until now. This opened with a grey
-- "bluetooth" word, a switch labelled "enabled", a "scanning" line with a refresh button, and one
-- undifferentiated list in which a connected headset and an anonymous beacon were the same row --
-- and the beacon's row had no title at all, because a discovered device's `name` is often `""`
-- (§ 2.6) and `name or mac` does not fall through an empty string. Now: the connected device is
-- ringed under "paired", with its battery as a coloured badge and two quiet actions (disconnect,
-- forget); everything else is under "available", named by its address when it has no name, with
-- the one thing to do to it -- pair -- as a word on the right.
--
-- Not carried over: the "Visible" tile (`set_discoverable` is not a command this capability has)
-- and the codec picker (`codec` is always `nil` this round, ADR-0030). Discovery is a button in
-- the header rather than the mirror's second tile, so the two radio panels open the same way.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local toggle = require("components.toggle")
local icon_button = require("components.icon_button")
local section_header = require("components.section_header")
local panel_header = require("components.panel_header")
local panel_row = require("components.panel_row")
local panel_action_icon = require("components.panel_action_icon")
local panel_empty_state = require("components.panel_empty_state")

local KIND = "bluetooth"
local SCROLL = scroll("bluetooth_devices")

-- One glyph per § 2.6 `category`, held in `config/icons.lua` beside the rest of them so a category
-- added there is added once. A discovered device carries no category and draws the generic one.
local function device_icon(device)
    return icons.device[device.category or "generic"] or icons.device.generic
end

-- `name or mac` was the bug: an empty string is true in Lua.
local function display_name(device)
    if device.name == nil or device.name == "" then
        return device.mac or "?"
    end
    return device.name
end

local function battery_text(device)
    if device.battery == nil or device.battery < 0 then
        return nil
    end
    return string.format("%d%%", device.battery)
end

local function connected(b)
    return (b and b.connected_devices) or {}
end

local function discovered(b)
    return (b and b.discovered_devices) or {}
end

local function enabled(b)
    return b ~= nil and b.enabled
end

-- The header's second line, the mirror's `subtitle`: what is connected, how charged, or what the
-- radio is doing instead.
local function state_line(b)
    if not b.enabled then
        return "off"
    end
    local first = connected(b)[1]
    if first then
        local parts = { string.format("%d connected", #connected(b)), display_name(first) }
        local battery = battery_text(first)
        if battery then
            parts[#parts + 1] = battery
        end
        return table.concat(parts, " · ")
    end
    return b.discovering and "scanning…" or "no devices connected"
end

-- `BatteryBadge`: the level as a small filled pill, red under 10%, amber under 20%, accent above.
local function battery_badge(device)
    local text = battery_text(device)
    if not text then
        return nil
    end
    local level = device.battery
    local color = level <= 10 and theme.RED or (level <= 20 and theme.YELLOW or theme.ACCENT)
    return row {
        height = theme.control.xs,
        align_v = "Center",
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        radius = theme.control.xs / 2,
        background = color,
        border_width = theme.border_width,
        border_color = theme.GLASS_BORDER,
        children = { cell({ { text = text, bold = true } }, theme.text_contrast(color), theme.font.xs, { align_v = "Center" }) },
    }
end

-- The mirror's ghost `OButton { text: "Pair" }`: a word in accent that grows a ground under the
-- pointer. The one action an unpaired device has.
local function pair_button(device)
    local slot = "bluetooth-pair-" .. tostring(device.mac)
    local hovered = hover(slot)
    return button {
        height = theme.control.sm,
        align_v = "Center",
        radius = theme.radius.sm,
        hover = hovered,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        background = hovered:map(function(is_hovered)
            return is_hovered and theme.ACCENT_SUBTLE or nil
        end),
        on_click = function(_, mouse_button)
            if mouse_button == "left" then
                oblisk.bluetooth:invoke("pair", device.mac)
            end
        end,
        children = { cell("pair", theme.ACCENT, theme.font.xs, { align = "Center", align_v = "Center" }) },
    }
end

-- Paired first under its own header, then found, as one list: two lists in one column would each
-- want their own extent and neither knows what the other took. Empty while the radio is off, which
-- is the mirror's `visible: root.active && ...` on the whole list.
local rows = oblisk.bluetooth:map(function(b)
    local out = {}
    if not enabled(b) then
        return out
    end
    if #connected(b) > 0 then
        out[#out + 1] = { kind = "header", label = "paired", key = "header-paired" }
        for _, device in ipairs(connected(b)) do
            out[#out + 1] = { kind = "device", device = device, paired = true, key = "paired-" .. tostring(device.mac) }
        end
    end
    if #discovered(b) > 0 then
        out[#out + 1] = { kind = "header", label = "available", key = "header-available" }
        for _, device in ipairs(discovered(b)) do
            out[#out + 1] = { kind = "device", device = device, paired = false, key = "found-" .. tostring(device.mac) }
        end
    end
    return out
end)

local function device_row(item)
    if item.kind == "header" then
        return section_header(item.label)
    end
    local device = item.device
    local trailing = {}
    if item.paired then
        local badge = battery_badge(device)
        if badge then
            trailing[#trailing + 1] = badge
        end
        trailing[#trailing + 1] = panel_action_icon(icons.disconnect, function()
            oblisk.bluetooth:invoke("disconnect", device.mac)
        end, { slot = "bluetooth-disconnect-" .. tostring(device.mac), tint = theme.RED })
        trailing[#trailing + 1] = panel_action_icon(icons.trash, function()
            oblisk.bluetooth:invoke("forget", device.mac)
        end, { slot = "bluetooth-forget-" .. tostring(device.mac), tint = theme.RED })
    else
        trailing[#trailing + 1] = pair_button(device)
    end
    -- Neither row is itself a button, as in the mirror (`rowActionEnabled` is false for both): a
    -- connected device's actions are its two icons, and an unpaired one's is the word. The row is
    -- for reading.
    return panel_row {
        slot = "bluetooth-device-" .. tostring(device.mac),
        icon = device_icon(device),
        title = display_name(device),
        subtitle = item.paired and (device.codec and ("connected · " .. device.codec) or "connected") or nil,
        selected = item.paired,
        trailing = row { spacing = theme.spacing.xs, align_v = "Center", children = trailing },
    }
end

local body = {
    panel_header {
        title = "bluetooth",
        icon = oblisk.bluetooth:map(function(b)
            return enabled(b) and icons.bt_on or icons.bt_off
        end),
        active = oblisk.bluetooth:map(enabled),
        subtitle = util.label(oblisk.bluetooth, state_line),
        trailing = {
            -- Scan, lit while discovery runs, and a second press stops it -- the mirror's "Scan"
            -- tile as a button. Discovery used to be armed by this button and disarmed by nothing
            -- but BlueZ's own timeout; now it is a toggle, and the header line says "scanning…"
            -- while it is on.
            icon_button(icons.refresh, function()
                local b = oblisk.bluetooth:get()
                oblisk.bluetooth:invoke((b and b.discovering) and "stop_discovery" or "start_discovery")
            end, {
                slot = "bluetooth-scan",
                size = theme.control.sm,
                icon_size = theme.icon.sm,
                background = oblisk.bluetooth:map(function(b)
                    return (b and b.discovering) and theme.ACCENT_MEDIUM or theme.GLASS_CONTROL
                end),
                visible = util.shown_when(oblisk.bluetooth, enabled),
            }),
            toggle(oblisk.bluetooth, function(b)
                return b.enabled
            end, function(new_value)
                oblisk.bluetooth:invoke("set_enabled", new_value)
            end),
        },
    },
    -- As tall as its rows up to the cap, then a scrolling viewport (ADR-0110): the mirror's
    -- `Math.min(deviceList.contentHeight, Theme.itemHeight * 10)`.
    list {
        width = "Fill",
        max_height = theme.panel_list_height,
        scroll = SCROLL,
        spacing = theme.spacing.xs,
        source = rows,
        itemfn = device_row,
        key = function(item)
            return item.key
        end,
    },
    panel_empty_state(
        util.label(oblisk.bluetooth, function(b)
            if not b.enabled then
                return "bluetooth off"
            end
            return b.discovering and "scanning…" or "no devices found"
        end),
        util.shown_when(oblisk.bluetooth, function(b)
            return not b.enabled or (#connected(b) == 0 and #discovered(b) == 0)
        end),
        { icon = icons.bt_off }
    ),
}

return { kind = KIND, body = body }
