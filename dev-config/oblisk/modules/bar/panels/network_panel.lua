-- Mirrors NetworkPanel.qml: a masthead that says what the link is, two tiles for the two radios,
-- and the access points in range, the joined one first and ringed.
--
-- Laid out as the mirror lays it out, which it was not until now. This opened with a grey
-- "network" word, a switch labelled "wi-fi", and rows whose subtitle spelled "45% 5 GHz lock --
-- connected" -- four facts the mirror draws instead of writing: the bars of the glyph are the
-- strength, a small coloured "5G" beside it is the band, a lock badge is the security, and the
-- accent ring is the connection. What the words were doing is now done by shape and colour, and
-- the row is left with the one word a row needs, the SSID.
--
-- Not carried over from the mirror: the "Hidden network..." row (it needs a name typed into a plain
-- field, and this surface asks for the keyboard only while a password is pending -- see
-- `modules/shell/panel_host.lua`), the IP address in the wi-fi tile (`NetworkState` does not carry
-- one), and Saved/Available sections (no `saved` flag on an access point). A connected network is
-- saved by construction, so its forget action is offered there.
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

-- Payload order is already connected-first then descending signal (§ 2.5), so this copied the
-- list and re-sorted it on every rebuild to arrive back where it started. Reading it straight is
-- the whole function now.
local function access_points(n)
    return (n and n.available_networks) or {}
end

local function radio_on(n)
    return n ~= nil and n.networking_enabled and n.wifi_enabled
end

-- Four bars' worth of glyph, which is what a strength percentage actually reads as.
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

-- `Theme.networkBandColor`: the band as a colour on the glyph and a short label beside it, so the
-- two 5 GHz networks and the 2.4 GHz one are told apart without reading. `band` arrives as
-- `"2.4 GHz"`, `"5 GHz"` or `"6 GHz"` (§ 2.5); the number is what is drawn.
local BAND_COLOR = { ["2.4"] = theme.YELLOW, ["5"] = theme.ACCENT, ["6"] = theme.GREEN }

local function band_of(ap)
    local number = ap.band and ap.band:match("^[%d%.]+")
    if not number then
        return nil, theme.FG
    end
    return number == "2.4" and "2.4" or (number .. "G"), BAND_COLOR[number] or theme.FG
end

-- The header's second line, the mirror's `subtitle` chain, one state at a time in the order that
-- matters: a stack that is off says so before anything about radios does.
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

-- The rows, with the two per-row facts the payload does not put on the access point: whether this
-- is the one being joined, and whether another one is, in which case this row is blocked (the
-- mirror's `blockedByOtherConnection`). Rebuilt per push, which is what a `list` does anyway
-- (`parse_list_children` calls `itemfn` on every element every pass), so an enriched item costs
-- nothing the plain one did not.
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
        -- The one subtitle a network row carries, and only while it is true. The old row wrote
        -- strength, band, security and connection here; each of those is now drawn.
        subtitle = entry.connecting and "connecting…" or nil,
        selected = ap.active,
        opacity = entry.blocked and theme.opacity.disabled or nil,
        trailing = row { spacing = theme.spacing.xs, align_v = "Center", children = trailing },
        on_activate = clickable and function()
            -- `hidden` is a required second argument (§ 3.2), and every entry in
            -- `available_networks` was found by a scan, so none of them is hidden.
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
            -- Rescan, lit while a scan is in flight: the mirror swaps the icon for a spinner, and
            -- this bar has no spinner, so the same ground-lights-up rule the do-not-disturb and
            -- bluetooth-scan buttons follow says "running" here. `scanning` flips on the click
            -- (§ 2.5), so the light is immediate.
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
            -- The master switch, `NetworkService.setNetworkingEnabled`. The whole stack, not one
            -- radio: with it off the tiles below hide, because a radio switch under a stack that
            -- is off would be a control that does nothing.
            toggle(oblisk.network, function(n)
                return n.networking_enabled
            end, function(new_value)
                oblisk.network:invoke("set_networking_enabled", new_value)
            end),
        },
    },
    -- The two radios as tiles. The wi-fi tile's detail is the joined network's strength and band,
    -- where the mirror shows its address and band; there is no address in `NetworkState`.
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
    -- The mirror's error `PanelCard`: what the last attempt said, in red on a red-tinted ground.
    -- `connect_error` is sticky until the next attempt (§ 2.5) and there is no command that clears
    -- it alone, so this has no dismiss; the next click on a row is the dismiss.
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
    -- The password prompt, raised by the Supervisor rather than by this file: `network:connect` on
    -- a secured network with no saved profile is the one case that cannot proceed on the click
    -- alone, and `password_ssid` is how it says so (§ 2.5). Nothing here decides when to ask,
    -- because whether a profile exists is NetworkManager's fact, not a config's.
    --
    -- Typed characters never reach this VM. `mask_character` plus `secure_submit` sends every
    -- keystroke into a native buffer on the Renderer's Wayland thread, out as a
    -- `("network", "connect")` envelope, and nowhere else -- the same pair `modules/global/lock.lua`
    -- uses and for the same reason (ADR-0005/ADR-0027). So there is no `on_change` and no
    -- `on_submit`: either would be the hole the design exists to close.
    --
    -- It is the only `secure_submit` field on `panel_host`, across all five panels, and that is
    -- load-bearing rather than incidental: the engine focuses a surface's *sole* such field when
    -- the compositor hands the surface keyboard focus, and refuses to guess between two. A second
    -- one anywhere in this popup would leave this field needing a click that `modules/bar/init.lua`
    -- cannot arrange -- and would take the lock screen's own rule with it if it landed there.
    --
    -- Declared always, hidden mostly: an invisible node leaves the layout entirely (`resolve_sizes`
    -- in scene.rs) but stays in the tree, so this costs a row of nothing while it is not asking and
    -- keeps "exactly one" true by construction rather than by a rule about when it is built.
    column {
        width = "Fill",
        spacing = theme.spacing.xs,
        visible = util.shown_when(oblisk.network, function(n)
            return n.password_ssid ~= nil
        end),
        children = {
            -- The mirror's sheet title, `Connect to "%1"`, so the field says which network it is
            -- for before anything is typed into it.
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
                    -- The only way out. Escape inside a `secure_submit` field clears what was typed
                    -- and stays in the field, so without this a prompt raised by a mis-click would
                    -- hold the bar's keyboard focus until something else took it.
                    icon_button(icons.close, function()
                        oblisk.network:invoke("cancel_connect")
                    end, { slot = "network-password-cancel", size = theme.control.md, foreground = theme.RED }),
                },
            },
        },
    },
    -- As tall as its rows up to the cap, then a scrolling viewport (ADR-0110): the mirror's
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
