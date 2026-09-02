-- Mirrors NetworkPanel.qml: the access points in range, the connected one first, each a row you can
-- click to connect to.
--
-- A scrolling list rather than three summary lines, which is what this was. The summary was not a
-- design choice; `oblisk.network` has carried `available_networks` since ADR-0053 and the panel
-- could not show them, because a column taller than the popup was simply cut off with no way to
-- reach the rest. `scroll(name)` is what changed (ADR-0069).
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local section_header = require("components.section_header")
local panel_row = require("components.panel_row")
local panel_empty_state = require("components.panel_empty_state")
local panel_toggle_card = require("components.panel_toggle_card")

local KIND = "network"
local SCROLL = scroll("network_aps")

-- Payload order is already connected-first then descending signal (§ 2.5), so this copied the
-- list and re-sorted it on every rebuild to arrive back where it started. Reading it straight is
-- the whole function now.
local function access_points(n)
    return (n and n.available_networks) or {}
end

-- Four bars' worth of glyph, which is what a strength percentage actually reads as. These were
-- freedesktop theme names until now, under a comment claiming they were Nerd Font codepoints --
-- they were the one thing on the bar the comment described correctly and the code did not do.
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

local body = {
    row {
        width = "Fill",
        align_v = "Center",
        spacing = theme.spacing.sm,
        children = {
            section_header("network"),
            -- `Fill` between the header and the state pushes the state to the far edge, the same
            -- one-property trick `components/panel_header.lua` spends.
            -- The header is where a connect attempt reports, rather than the row it belongs to.
            -- A per-row spinner would mean mapping `available_networks` into enriched items, and
            -- the payload is already in the order the list draws (ADR-0082) -- rebuilding it per
            -- push to carry one flag would put back the copy that ADR removed. `connecting_ssid`
            -- names the network here instead, which is the same information in one line.
            cell(util.label(oblisk.network, function(n)
                if n.connect_error then
                    return n.connect_error
                end
                if n.connecting_ssid then
                    return string.format("connecting to %s", n.connecting_ssid)
                end
                if n.password_ssid then
                    return string.format("password for %s", n.password_ssid)
                end
                if not n.wifi_enabled then
                    return "wi-fi off"
                end
                return n.scanning and "scanning" or string.format("%d in range", util.count(n.available_networks))
            end), oblisk.network:map(function(n)
                -- `lock.lua` draws its own failed attempt in RED for the same reason: an error at
                -- 35% opacity is an error nobody reads. Guarded for nil like every other bare
                -- `:map` here -- unlike `util.label`, a raw map runs before the first push.
                return (n and n.connect_error) and theme.RED or theme.TEXT_OFF
            end), theme.font.xs, { width = "Fill", align = "End" }),
        },
    },
    -- The radio switch `BluetoothPanel.qml`'s mirror has had all along. It could not be written
    -- until `NetworkState` carried `wifi_enabled`, since a toggle with no read-back is a button
    -- that lies about its own position.
    panel_toggle_card("wi-fi", oblisk.network, function(n)
        return n.wifi_enabled
    end, function(new_value)
        oblisk.network:invoke("set_wifi_enabled", new_value)
    end),
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
    row {
        width = "Fill",
        align_v = "Center",
        spacing = theme.spacing.sm,
        visible = util.shown_when(oblisk.network, function(n)
            return n.password_ssid ~= nil
        end),
        children = {
            textfield {
                width = "Fill",
                height = theme.control.md,
                placeholder = "password, then Enter",
                mask_character = "*",
                secure_submit = { capability = "network", action = "connect" },
                font_size = theme.font.sm,
            },
            -- The only way out. Escape inside a `secure_submit` field clears what was typed and
            -- stays in the field, so without this a prompt raised by a mis-click would hold the
            -- bar's keyboard focus until something else took it.
            icon_button(icons.close, function()
                oblisk.network:invoke("cancel_connect")
            end, { slot = "network-password-cancel", size = theme.control.md, foreground = theme.RED }),
        },
    },
    list {
        width = "Fill",
        height = "Fill",
        scroll = SCROLL,
        spacing = theme.spacing.xs,
        source = oblisk.network:map(access_points),
        itemfn = function(ap)
            return panel_row {
                slot = "network-ap-" .. tostring(ap.ssid),
                icon = strength_glyph(ap.strength),
                title = ap.ssid or "?",
                -- `band` and `secure` are in the payload (§ 2.5) and nothing was reading them.
                subtitle = string.format("%d%%%s%s%s", ap.strength or 0, ap.band and (" " .. ap.band) or "", ap.secure and " lock" or " open", ap.active and " -- connected" or ""),
                color = ap.active and theme.ACCENT or theme.FG,
                on_activate = function()
                    -- `hidden` is a required second argument (§ 3.2), and every entry in
                    -- `available_networks` was found by a scan, so none of them is hidden.
                    oblisk.network:invoke("connect", ap.ssid, false)
                end,
            }
        end,
        key = function(ap)
            return tostring(ap.ssid)
        end,
    },
    panel_empty_state("nothing in range", util.shown_when(oblisk.network, function(n)
        return #access_points(n) == 0
    end)),
}

return { kind = KIND, body = body }
