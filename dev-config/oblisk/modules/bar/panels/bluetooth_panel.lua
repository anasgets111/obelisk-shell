-- Mirrors BluetoothPanel.qml: radio switch, then paired and discovered devices in named sections.
--
-- The old header opened with a grey `bluetooth` word, an `enabled` switch, and a `scanning` line
-- with a refresh button. Its single list made a connected headset and anonymous beacon identical;
-- discovered `name` is often `""` (§ 2.6), and Lua's `name or mac` does not skip an empty string.
-- Two lists in one column would each want their own extent, and neither knows what the other took.
-- Now paired devices are ringed with a battery badge and disconnect/forget actions; available
-- devices use their address when unnamed and show only the pair action.
--
-- Dropped: the "Visible" tile (`set_discoverable` is unavailable) and codec picker (`codec` is
-- always `nil`, ADR-0030). Discovery is a header button so both radio panels open the same way.
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
local info_badge = require("components.info_badge")
local panel_empty_state = require("components.panel_empty_state")

local KIND = "bluetooth"
local SCROLL = scroll("bluetooth_devices")

-- One glyph per § 2.6 `category`, centralized in `config/icons.lua`; discovered devices without one
-- use the generic glyph.
local function device_icon(device)
    return icons.device[device.category or "generic"] or icons.device.generic
end

-- `name or mac` was wrong because an empty string is true in Lua.
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

-- Header subtitle: connection count/name/battery, or the radio's current activity.
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
-- The capsule itself is `components/info_badge.lua`; only the level's colour is bluetooth's.
local function battery_badge(device)
    local text = battery_text(device)
    if not text then
        return nil
    end
    local level = device.battery
    local color = level <= 10 and theme.RED or (level <= 20 and theme.YELLOW or theme.ACCENT)
    return info_badge(text, color, { opacity = theme.opacity.strong })
end

-- Mirror `OButton { text: "Pair" }`: an accented word whose ground appears on hover.
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

-- Paired and available rows share one list so neither section must guess the other's extent. It is
-- empty while the radio is off, matching the mirror's `visible: root.active && ...`.
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
    -- Neither row is a button (`rowActionEnabled` is false): paired actions are icons, unpaired is
    -- the word "pair"; the row is for reading.
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
            -- Scan toggles discovery. It used to start discovery with no stop except BlueZ's
            -- timeout;
            -- the header now says "scanning…" while it runs.
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
    -- Rows up to the cap, then a scrolling viewport (ADR-0110), matching
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
