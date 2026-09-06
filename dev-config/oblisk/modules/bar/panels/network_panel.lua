-- Mirrors NetworkPanel.qml: masthead, two radio tiles, and access points with the joined one first.
--
-- The old header opened with a grey `network` word and a `wi-fi` switch. Its rows wrote "45% 5 GHz
-- lock -- connected". The mirror draws those facts as signal bars, a coloured "5G" band label, a
-- lock badge, and an accent ring; the row keeps only the SSID.
--
-- Dropped: "Hidden network..." (it needs a typed name, while this surface asks for the keyboard
-- only
-- for a pending password; see `modules/shell/panel_host.lua`), the IP address (`NetworkState` lacks
-- it), and Saved/Available sections (no `saved` flag). A connected network is saved by
-- construction,
-- so its forget action is offered there.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local toggle = require("components.toggle")
local icon_button = require("components.icon_button")
local panel_header = require("components.panel_header")
local panel_toggle_card = require("components.panel_toggle_card")
local panel_row = require("components.panel_row")
local panel_action_icon = require("components.panel_action_icon")
local panel_empty_state = require("components.panel_empty_state")

local KIND = "network"
local SCROLL = scroll("network_aps")

-- Payload order is connected-first, then descending signal (§ 2.5). The old copy-and-sort returned
-- to that same order on every rebuild; reading it straight is enough.
local function access_points(n)
    return (n and n.available_networks) or {}
end

local function radio_on(n)
    return n ~= nil and n.networking_enabled and n.wifi_enabled
end

-- Four bars, which is what a strength percentage needs to say.
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

-- `Theme.networkBandColor`: band colour plus a short label. `band` is `"2.4 GHz"`, `"5 GHz"`, or
-- `"6 GHz"` (§ 2.5), so two 5 GHz networks remain distinguishable.
local BAND_COLOR = { ["2.4"] = theme.YELLOW, ["5"] = theme.ACCENT, ["6"] = theme.GREEN }

local function band_of(ap)
    local number = ap.band and ap.band:match("^[%d%.]+")
    if not number then
        return nil, theme.FG
    end
    return number == "2.4" and "2.4" or (number .. "G"), BAND_COLOR[number] or theme.FG
end

-- Header subtitle, in priority order: an off stack speaks before its radios.
local function state_line(n)
    if not n.networking_enabled then
        return "off"
    end
    if n.connecting_ssid then
        return "connecting to " .. n.connecting_ssid
    end
    if n.ssid == "Ethernet" then
        return "ethernet connected"
    end
    if n.ssid then
        return n.ssid
    end
    if not n.wifi_enabled then
        return "wi-fi off"
    end
    return n.scanning and "scanning…" or "not connected"
end

local function header_glyph(n)
    if n == nil or not n.networking_enabled then
        return icons.wifi_off
    end
    if n.ssid == "Ethernet" then
        return icons.ethernet
    end
    return n.wifi_enabled and icons.wifi[4] or icons.wifi_off
end

-- Enrich each row with joined state and `blockedByOtherConnection`. Rebuilding per push costs no
-- more than the list already does: `parse_list_children` calls `itemfn` for every element each
-- pass.
local rows = oblisk.network:map(function(n)
    local out = {}
    local connecting = n and n.connecting_ssid
    for _, ap in ipairs(access_points(n)) do
        out[#out + 1] = {
            ap = ap,
            connecting = connecting ~= nil and connecting == ap.ssid,
            blocked = connecting ~= nil and connecting ~= ap.ssid,
        }
    end
    return out
end)

local function access_point_row(entry)
    local ap = entry.ap
    local band, color = band_of(ap)

    local leading = { cell(strength_glyph(ap.strength), color, theme.icon.md, { align_v = "Center" }) }
    if band then
        leading[#leading + 1] = cell({ { text = band, bold = true } }, color, theme.font.xs, { align_v = "End" })
    end

    local trailing = {}
    if ap.active then
        trailing[#trailing + 1] = panel_action_icon(icons.trash, function()
            oblisk.network:invoke("forget", ap.ssid)
        end, { slot = "network-forget-" .. tostring(ap.ssid), tint = theme.RED })
    end
    if ap.secure then
        trailing[#trailing + 1] = cell(icons.lock, theme.TEXT_OFF, theme.font.xs, { align_v = "Center" })
    end

    local clickable = not ap.active and not entry.blocked
    return panel_row {
        slot = "network-ap-" .. tostring(ap.ssid),
        leading = row { align_v = "Center", children = leading },
        title = ap.ssid or "?",
        -- The only row subtitle, shown while true. Strength, band, security, and connection are
        -- drawn instead of written.
        subtitle = entry.connecting and "connecting…" or nil,
        selected = ap.active,
        opacity = entry.blocked and theme.opacity.disabled or nil,
        trailing = row { spacing = theme.spacing.xs, align_v = "Center", children = trailing },
        on_activate = clickable and function()
            -- `hidden` is required (§ 3.2); scanned `available_networks` entries are not hidden.
            oblisk.network:invoke("connect", ap.ssid, false)
        end or nil,
    }
end

local body = {
    panel_header {
        title = "network",
        icon = oblisk.network:map(header_glyph),
        active = oblisk.network:map(function(n)
            return n ~= nil and n.networking_enabled
        end),
        subtitle = util.label(oblisk.network, state_line),
        trailing = {
            -- Rescan is lit while scanning. The mirror uses a spinner; this has none, so the same
            -- lit-ground rule as DND and Bluetooth scan says "running". `scanning` flips on click
            -- (§ 2.5), making the light immediate.
            icon_button(icons.refresh, function()
                oblisk.network:invoke("scan")
            end, {
                slot = "network-rescan",
                size = theme.control.sm,
                icon_size = theme.icon.sm,
                background = oblisk.network:map(function(n)
                    return (n and n.scanning) and theme.ACCENT_MEDIUM or theme.GLASS_CONTROL
                end),
                visible = util.shown_when(oblisk.network, radio_on),
            }),
            -- `NetworkService.setNetworkingEnabled` controls the whole stack; off hides the tiles,
            -- avoiding a radio control that does nothing.
            toggle(oblisk.network, function(n)
                return n.networking_enabled
            end, function(new_value)
                oblisk.network:invoke("set_networking_enabled", new_value)
            end),
        },
    },
    -- Two radio tiles. Wi-Fi shows joined strength and band; `NetworkState` has no address.
    row {
        width = "Fill",
        spacing = theme.spacing.xs,
        visible = util.shown_when(oblisk.network, function(n)
            return n.networking_enabled
        end),
        children = {
            panel_toggle_card {
                slot = "network-wifi-tile",
                icon = icons.wifi[4],
                label = "wi-fi",
                detail = util.label(oblisk.network, function(n)
                    if not n.wifi_enabled or n.ssid == nil or n.ssid == "Ethernet" then
                        return ""
                    end
                    for _, ap in ipairs(access_points(n)) do
                        if ap.active then
                            return string.format("%d%% · %s", ap.strength or 0, ap.band or "")
                        end
                    end
                    return ""
                end),
                signal = oblisk.network,
                read = function(n)
                    return n.wifi_enabled
                end,
                on_change = function(new_value)
                    oblisk.network:invoke("set_wifi_enabled", new_value)
                end,
            },
            panel_toggle_card {
                slot = "network-ethernet-tile",
                icon = icons.ethernet,
                label = "ethernet",
                signal = oblisk.network,
                read = function(n)
                    return n.ethernet_enabled
                end,
                on_change = function(new_value)
                    oblisk.network:invoke("set_ethernet_enabled", new_value)
                end,
            },
        },
    },
    -- Mirror error card, red on a red-tinted ground. `connect_error` is sticky until the next
    -- attempt (§ 2.5), with no clear command; the next row click dismisses it.
    row {
        width = "Fill",
        spacing = theme.spacing.sm,
        align_v = "Center",
        padding = { top = theme.spacing.sm, right = theme.spacing.sm, bottom = theme.spacing.sm, left = theme.spacing.sm },
        radius = theme.radius.md,
        background = theme.ALERT_BG,
        visible = util.shown_when(oblisk.network, function(n)
            return n.connect_error ~= nil and n.connecting_ssid == nil
        end),
        children = {
            cell(icons.warning, theme.RED, theme.icon.sm, { align_v = "Center" }),
            cell(util.label(oblisk.network, function(n)
                return n.connect_error or ""
            end), theme.RED, theme.font.sm, { width = "Fill", wrap = "Word", max_lines = 2 }),
        },
    },
    -- The Supervisor raises this when `network:connect` hits a secured network without a saved
    -- profile; `password_ssid` says so (§ 2.5). NetworkManager, not this config, knows whether to
    -- ask.
    --
    -- Typed characters never reach this VM. `mask_character` plus `secure_submit` stores keystrokes
    -- in a native buffer on the Renderer's Wayland thread and sends a `("network", "connect")`
    -- envelope, as in `modules/global/lock.lua` (ADR-0005/ADR-0027). Hence no `on_change` or
    -- `on_submit` callback can reopen that hole.
    --
    -- It is the only `secure_submit` field across `panel_host`'s five panels. The engine focuses a
    -- surface's *sole* such field on keyboard focus and refuses to guess between two; a second here
    -- would require a click that `modules/bar/init.lua` cannot arrange, and would violate the lock
    -- screen's own sole-field rule if placed there.
    --
    -- Always declared, usually hidden. Invisible nodes leave layout (`resolve_sizes` in scene.rs)
    -- but stay in the tree, keeping "exactly one" structural.
    column {
        width = "Fill",
        spacing = theme.spacing.xs,
        visible = util.shown_when(oblisk.network, function(n)
            return n.password_ssid ~= nil
        end),
        children = {
            -- Mirror title `Connect to "%1"`, identifying the network before typing.
            cell(util.label(oblisk.network, function(n)
                return string.format("connect to “%s”", n.password_ssid or "")
            end), theme.FG, theme.font.sm, { width = "Fill" }),
            row {
                width = "Fill",
                align_v = "Center",
                spacing = theme.spacing.sm,
                children = {
                    textfield {
                        width = "Fill",
                        height = theme.control.md,
                        placeholder = "password, then Enter",
                        mask_character = "*",
                        secure_submit = { capability = "network", action = "connect" },
                        font_size = theme.font.sm,
                    },
                    -- The only way out: Escape clears a `secure_submit` field and stays in it, so
                    -- this closes a prompt raised by a mis-click and releases the bar's focus.
                    icon_button(icons.close, function()
                        oblisk.network:invoke("cancel_connect")
                    end, { slot = "network-password-cancel", size = theme.control.md, foreground = theme.RED }),
                },
            },
        },
    },
    -- Rows up to the cap, then a scrolling viewport (ADR-0110), matching
    -- `Math.min(networkList.contentHeight, Theme.itemHeight * 7)`.
    list {
        width = "Fill",
        max_height = theme.panel_list_height,
        scroll = SCROLL,
        spacing = theme.spacing.xs,
        visible = util.shown_when(oblisk.network, radio_on),
        source = rows,
        itemfn = access_point_row,
        key = function(entry)
            return tostring(entry.ap.ssid)
        end,
    },
    panel_empty_state(
        util.label(oblisk.network, function(n)
            if not n.networking_enabled then
                return "networking off"
            elseif not n.wifi_enabled then
                return "wi-fi off"
            elseif n.scanning then
                return "scanning…"
            end
            return "no networks found"
        end),
        util.shown_when(oblisk.network, function(n)
            return not radio_on(n) or #access_points(n) == 0
        end),
        {
            icon = oblisk.network:map(function(n)
                return radio_on(n) and icons.wifi_none or icons.wifi_off
            end),
        }
    ),
}

return { kind = KIND, body = body }
