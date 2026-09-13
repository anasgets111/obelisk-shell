-- Mirrors BluetoothPanel.qml: radio switch, then paired and discovered devices in named sections.
--
-- Discovered `name` is often `""` (§ 2.6), and Lua's `name or mac` keeps an empty string.
-- Connected rows show battery, disconnect and forget. Paired rows reconnect on click and show
-- forget. Available rows fall back to MAC and show pair.
--
-- A connected audio device's codecs come from `obelisk.audio`'s `bluetooth`, joined by MAC.
-- Clicking its row lists them, the mirror's codec picker. Discovery runs while the panel shows
-- (`lib/ui_state.lua`); the scan tile still stops or restarts it.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local toggle = require("components.toggle")
local panel_toggle_card = require("components.panel_toggle_card")
local section_header = require("components.section_header")
local panel_header = require("components.panel_header")
local panel_row = require("components.panel_row")
local panel_action_icon = require("components.panel_action_icon")
local info_badge = require("components.info_badge")
local panel_empty_state = require("components.panel_empty_state")
local spinner = require("components.spinner")

local KIND = "bluetooth"
local SCROLL = scroll("bluetooth_devices")

-- One glyph per § 2.6 `category`, from `config/icons.lua`; missing categories use `generic`.
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

local ui = require("lib.ui_state")

local function connected(b)
    return util.sorted_devices(b and b.connected_devices)
end

local function paired(b)
    return util.sorted_devices(b and b.paired_devices)
end

local function discovered(b)
    return util.sorted_devices(b and b.discovered_devices)
end

local function enabled(b)
    return b ~= nil and b.enabled
end

local function state_line(b)
    if not b.available then
        return "unavailable"
    end
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

-- `BatteryBadge` is red under 10%, amber under 20%, and accent above; its capsule is
-- `components/info_badge.lua`.
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
                obelisk.bluetooth:invoke("pair", device.mac)
            end
        end,
        children = { cell("pair", theme.ACCENT, theme.font.xs, { align = "Center", align_v = "Center" }) },
    }
end

-- The `obelisk.audio` entry for `mac`, or `nil` when PipeWire has no codec to offer for it.
local function codec_card(a, mac)
    for _, card in ipairs((a and a.bluetooth) or {}) do
        if card.mac == mac and #card.codecs > 0 then
            return card
        end
    end
end

local function active_codec(card)
    for _, option in ipairs((card and card.codecs) or {}) do
        if option.index == card.active then
            return option.codec
        end
    end
end

-- Paired and available rows share one list so neither section must guess the other's extent. It is
-- empty while the radio is off, matching the mirror's `visible: root.active && ...`. Connected
-- devices lead the paired section, as the mirror's sort puts them. A row keeps its key across
-- connect and disconnect, so it changes in place rather than leaving and arriving.
local rows = computed({ obelisk.bluetooth, obelisk.audio, ui.bluetooth_codec_for }, function(b, a, open_for)
    local out = {}
    if not enabled(b) then
        return out
    end
    -- An open codec list follows its device row as rows of its own, so the list keeps one flat
    -- source rather than a row that grows.
    local function add(devices, status)
        for _, device in ipairs(devices) do
            local card = status == "connected" and codec_card(a, device.mac) or nil
            out[#out + 1] = {
                kind = "device",
                device = device,
                status = status,
                card = card,
                key = "device-" .. tostring(device.mac),
            }
            if card and open_for == device.mac then
                for _, option in ipairs(card.codecs) do
                    out[#out + 1] = {
                        kind = "codec",
                        device = device,
                        card = card,
                        option = option,
                        key = "codec-" .. tostring(device.mac) .. "-" .. option.index,
                    }
                end
            end
        end
    end
    local joined, known, found = connected(b), paired(b), discovered(b)
    if #joined + #known > 0 then
        out[#out + 1] = { kind = "header", label = "paired", key = "header-paired" }
        add(joined, "connected")
        add(known, "paired")
    end
    if #found > 0 then
        out[#out + 1] = { kind = "header", label = "available", key = "header-available" }
        add(found, "available")
    end
    return out
end)

-- One shared always-on signal for every busy row's spinner. `itemfn` runs on each push, and a
-- signal minted per row there would grow the registry for the life of the session.
local SPINNING = obelisk.bluetooth:map(function()
    return true
end)

-- The mirror's `busy` row: a spinner in place of the actions, the action as the subtitle, and no
-- click, so a second pair or connect cannot start over the first.
local function busy_row(device)
    return panel_row {
        slot = "bluetooth-device-" .. tostring(device.mac),
        icon = device_icon(device),
        title = display_name(device),
        subtitle = device.busy .. "…",
        trailing = spinner(SPINNING, theme.icon.md),
    }
end

-- One codec under its device, the mirror's codec `PanelRow`: the codec as the title, PipeWire's
-- description under it, the active one selected. Picking another switches and closes the list.
local function codec_row(item)
    local option = item.option
    local active = option.index == item.card.active
    return panel_row {
        slot = "bluetooth-codec-" .. tostring(item.device.mac) .. "-" .. option.index,
        title = option.codec,
        subtitle = option.description,
        selected = active,
        on_activate = not active and function()
            obelisk.audio:invoke("set_bluetooth_profile", item.card.device, option.index)
            ui.bluetooth_codec_for:set("")
        end or nil,
    }
end

local function device_row(item)
    if item.kind == "header" then
        return section_header(item.label)
    end
    if item.kind == "codec" then
        return codec_row(item)
    end
    local device = item.device
    if device.busy ~= nil then
        return busy_row(device)
    end
    local trailing = {}
    if item.status == "connected" then
        local badge = battery_badge(device)
        if badge then
            trailing[#trailing + 1] = badge
        end
        trailing[#trailing + 1] = panel_action_icon(icons.disconnect, function()
            obelisk.bluetooth:invoke("disconnect", device.mac)
        end, { slot = "bluetooth-disconnect-" .. tostring(device.mac), tint = theme.RED })
    end
    if item.status == "available" then
        if not device.blocked then
            trailing[#trailing + 1] = pair_button(device)
        end
    else
        trailing[#trailing + 1] = panel_action_icon(icons.trash, function()
            obelisk.bluetooth:invoke("forget", device.mac)
        end, { slot = "bluetooth-forget-" .. tostring(device.mac), tint = theme.RED })
    end
    -- Two rows are buttons. An unblocked paired row connects, the mirror's `canConnect`, and a
    -- connected row with codecs opens its codec list, the mirror's `canPickCodec`. Other rows act
    -- through their icons and the word "pair". A blocked row says so, the mirror's `statusText`,
    -- and offers nothing BlueZ would refuse.
    local subtitle = nil
    if item.status == "connected" then
        local codec = active_codec(item.card)
        subtitle = codec and ("connected · " .. codec) or "connected"
    elseif device.blocked then
        subtitle = "blocked"
    end
    local on_activate = nil
    if item.status == "paired" and not device.blocked then
        on_activate = function()
            obelisk.bluetooth:invoke("connect", device.mac)
        end
    elseif item.card ~= nil then
        on_activate = function()
            local open_for = ui.bluetooth_codec_for:get()
            ui.bluetooth_codec_for:set(open_for == device.mac and "" or device.mac)
        end
    end
    return panel_row {
        slot = "bluetooth-device-" .. tostring(device.mac),
        icon = device_icon(device),
        title = display_name(device),
        subtitle = subtitle,
        selected = item.status == "connected",
        trailing = row { spacing = theme.spacing.xs, align_v = "Center", children = trailing },
        on_activate = on_activate,
    }
end

local body = {
    panel_header {
        title = "bluetooth",
        icon = obelisk.bluetooth:map(function(b)
            return enabled(b) and icons.bt_on or icons.bt_off
        end),
        active = obelisk.bluetooth:map(enabled),
        subtitle = util.label(obelisk.bluetooth, state_line),
        trailing = {
            -- No adapter means no switch to flip, the mirror's `disabled: !root.ready`. Hidden rather
            -- than greyed, because `toggle` has no disabled look.
            rect {
                visible = util.shown_when(obelisk.bluetooth, function(b)
                    return b.available
                end),
                children = {
                    toggle(obelisk.bluetooth, function(b)
                        return b.enabled
                    end, function(new_value)
                        obelisk.bluetooth:invoke("set_enabled", new_value)
                    end),
                },
            },
        },
    },
    -- The mirror's two tiles. "visible" lets other devices find this one, and the agent asks before
    -- any of them pairs (`modules/global/bluetooth_pairing.lua`). "scan" is discovery.
    row {
        width = "Fill",
        spacing = theme.spacing.xs,
        visible = util.shown_when(obelisk.bluetooth, enabled),
        children = {
            panel_toggle_card {
                slot = "bluetooth-visible-tile",
                icon = icons.bt_visible,
                label = "visible",
                signal = obelisk.bluetooth,
                read = function(b)
                    return b.discoverable
                end,
                on_change = function(on)
                    obelisk.bluetooth:invoke("set_discoverable", on)
                end,
            },
            panel_toggle_card {
                slot = "bluetooth-scan-tile",
                icon = icons.bt_scan,
                label = "scan",
                signal = obelisk.bluetooth,
                read = function(b)
                    return b.discovering
                end,
                on_change = function(on)
                    obelisk.bluetooth:invoke(on and "start_discovery" or "stop_discovery")
                end,
            },
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
        util.label(obelisk.bluetooth, function(b)
            if not b.available then
                return "bluetooth unavailable"
            elseif not b.enabled then
                return "bluetooth off"
            end
            return b.discovering and "scanning…" or "no devices found"
        end),
        util.shown_when(obelisk.bluetooth, function(b)
            return not b.enabled or (#connected(b) == 0 and #paired(b) == 0 and #discovered(b) == 0)
        end),
        { icon = icons.bt_off }
    ),
}

return { kind = KIND, body = body }
