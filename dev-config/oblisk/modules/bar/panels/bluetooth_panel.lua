-- Mirrors BluetoothPanel.qml: the radio switch, the connected accessories, and whatever discovery
-- has turned up.
--
-- Two lists rather than a count, which is what this was. `connected_devices` and
-- `discovered_devices` have both been in the payload since ADR-0053 (§ 2.6) and the panel showed
-- neither, for `network_panel.lua`'s reason: a body taller than the popup was cut off.
--
-- Discovery is armed by opening the panel and disarmed by nothing, which is a real leak and is
-- called out rather than hidden. `on_dismiss` reaches `lib/ui_state.lua`'s `close_panel`, which
-- carries no token saying which panel it dismissed, so this cannot pair a `stop_discovery` with the
-- `start_discovery` below. BlueZ stops discovering on its own after a few minutes; until a
-- dismissal says what it dismissed, that timeout is the only thing that stops it.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local section_header = require("components.section_header")
local panel_toggle_card = require("components.panel_toggle_card")
local panel_row = require("components.panel_row")
local panel_empty_state = require("components.panel_empty_state")
local icon_button = require("components.icon_button")

local KIND = "bluetooth"
local SCROLL = scroll("bluetooth_devices")

-- One glyph per § 2.6 `category`, held in `config/icons.lua` beside the rest of them so a category
-- added there is added once.
local function device_icon(device)
    return icons.device[device.category or "generic"] or icons.device.generic
end

local function battery_note(device)
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

local body = {
    section_header("bluetooth"),
    panel_toggle_card("enabled", oblisk.bluetooth, function(b)
        return b.enabled
    end, function(new_value)
        oblisk.bluetooth:invoke("set_enabled", new_value)
    end),
    -- The whole body below the switch dims when the radio is off. One `opacity` on the container
    -- rather than a dim colour on each row, because `opacity` multiplies down the subtree (§ 5.1)
    -- and these rows are already three different colours.
    column {
        width = "Fill",
        height = "Fill",
        spacing = theme.spacing.xs,
        opacity = oblisk.bluetooth:map(function(b)
            return (b and b.enabled) and 1.0 or theme.opacity.disabled
        end),
        children = {
            row {
                width = "Fill",
                align_v = "Center",
                spacing = theme.spacing.sm,
                children = {
                    cell(util.label(oblisk.bluetooth, function(b)
                        return b.discovering and "scanning" or string.format("%d connected", #connected(b))
                    end), theme.TEXT_OFF, theme.font.xs, { width = "Fill" }),
                    icon_button(icons.refresh, function()
                        oblisk.bluetooth:invoke("start_discovery")
                    end, { size = theme.control.xs, icon_size = theme.icon.xs }),
                },
            },
            list {
                width = "Fill",
                height = "Fill",
                scroll = SCROLL,
                spacing = theme.spacing.xs,
                -- Connected first, then whatever discovery found, as one list: two lists in one
                -- column would each want their own extent and neither knows what the other took.
                source = oblisk.bluetooth:map(function(b)
                    local rows = {}
                    for _, device in ipairs(connected(b)) do
                        rows[#rows + 1] = { device = device, paired = true }
                    end
                    for _, device in ipairs(discovered(b)) do
                        rows[#rows + 1] = { device = device, paired = false }
                    end
                    return rows
                end),
                itemfn = function(entry)
                    local device = entry.device
                    local subtitle = entry.paired and (battery_note(device) or device.codec or "connected") or "not paired"
                    return panel_row {
                        slot = "bluetooth-device-" .. tostring(device.mac),
                        icon = device_icon(device),
                        title = device.name or device.mac or "?",
                        subtitle = subtitle,
                        color = entry.paired and theme.ACCENT or theme.FG,
                        on_activate = function()
                            if entry.paired then
                                oblisk.bluetooth:invoke("disconnect", device.mac)
                            else
                                oblisk.bluetooth:invoke("pair", device.mac)
                            end
                        end,
                    }
                end,
                key = function(entry)
                    return tostring(entry.device.mac)
                end,
            },
            panel_empty_state("no devices", util.shown_when(oblisk.bluetooth, function(b)
                return #connected(b) == 0 and #discovered(b) == 0
            end)),
        },
    },
}

return { kind = KIND, body = body }
