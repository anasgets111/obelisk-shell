-- Mirrors NetworkPanel.qml: masthead, two radio tiles, and access points with the joined one first.
--
-- The old header wrote "45% 5 GHz lock -- connected" beside a grey `network` word and `wi-fi`
-- switch. The mirror draws those facts as signal bars, a coloured "5G" band label, lock badge, and
-- accent ring, while rows keep only the SSID.
--
-- "Hidden network..." was dropped while this surface asked for the keyboard only for a pending
-- password: the name field could not be typed into because the engine armed the panel host's
-- password field whether or not it was on screen, sending every key to that invisible buffer.
-- `layout::secure_submit` now counts only reachable fields, so an `autofocus` name field arms
-- normally; `modules/shell/panel_host.lua` asks for the keyboard for both.
--
-- Still dropped: Saved/Available sections (no `saved` flag). A connected network is saved by
-- construction, so its forget action is offered there.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local glyph = require("components.glyph")
local toggle = require("components.toggle")
local icon_button = require("components.icon_button")
local panel_header = require("components.panel_header")
local panel_toggle_card = require("components.panel_toggle_card")
local panel_row = require("components.panel_row")
local panel_action_icon = require("components.panel_action_icon")
local panel_empty_state = require("components.panel_empty_state")
local spinner = require("components.spinner")
local action_button = require("components.action_button")
local ui = require("lib.ui_state")

local KIND = "network"
local SCROLL = scroll("network_aps")

-- Payload order is connected-first, then descending signal (§ 2.5); reading it straight is enough.
local function access_points(n)
    return (n and n.available_networks) or {}
end

-- A tile's second line, the mirror's `[address, band].filter(Boolean).join(" · ")`. Takes a
-- `table.pack` because a missing address is a `nil` hole that `ipairs` would stop at.
local function detail_line(parts)
    local shown = {}
    for index = 1, parts.n do
        if parts[index] ~= nil and parts[index] ~= "" then
            shown[#shown + 1] = parts[index]
        end
    end
    return table.concat(shown, " · ")
end

-- `formatEthernetSpeed`. NetworkManager reports Mb/s, with `0` for unknown.
local function speed_text(mbps)
    if mbps == nil or mbps <= 0 then
        return nil
    end
    return mbps >= 1000 and string.format("%g Gb/s", mbps / 1000) or string.format("%d Mb/s", mbps)
end

local function radio_on(n)
    return n ~= nil and n.networking_enabled and n.wifi_enabled
end

-- Header subtitle, in priority order: an off stack speaks before its radios. No payload at all means
-- NetworkManager never answered (the capability stays down for the run), the mirror's `!ready`.
local function state_line(n)
    if n == nil then
        return "unavailable"
    end
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

-- Enrich each row with joined state and `blockedByOtherConnection`; `parse_list_children` already
-- calls `itemfn` for every element each pass.
-- Both the scanned list and the hidden-network row appear under the same condition, so they share
-- one signal rather than each recomputing it on every network push.
local radio_up_and_idle = computed({ obelisk.network, ui.hidden_join }, function(n, joining)
    return radio_on(n) and not joining
end)

-- The error card's close button, the mirror's `errorDismissed`. `connect_error` itself stays until
-- the next attempt (§ 2.5), so the dismissal is view state and a new attempt re-arms it.
local error_dismissed = state("network_error_dismissed", false)

obelisk.network:on_change(function(n, previous)
    if previous == nil then
        return
    end
    if n.connecting_ssid ~= nil and previous.connecting_ssid == nil then
        error_dismissed:set(false)
    elseif previous.connecting_ssid ~= nil and n.connecting_ssid == nil and n.connect_error == nil then
        -- The mirror's `onConnectSucceeded`: the join the panel was opened for is done.
        if ui.panel_open:get() and ui.panel_kind:get() == KIND then
            ui.close_panel()
        end
    end
end)

local rows = obelisk.network:map(function(n)
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
    local band, color = util.band_of(ap)

    local leading = { glyph(util.wifi_glyph(ap.strength), color, theme.icon.md, { align_v = "Center" }) }
    if band then
        leading[#leading + 1] = cell({ { text = band, bold = true } }, color, theme.font.xs, { align_v = "End" })
    end

    local trailing = {}
    if ap.active then
        trailing[#trailing + 1] = panel_action_icon(icons.trash, function()
            obelisk.network:invoke("forget", ap.ssid)
        end, { slot = "network-forget-" .. tostring(ap.ssid), tint = theme.RED })
    end
    if ap.secure then
        trailing[#trailing + 1] = glyph(icons.lock, theme.DIM, theme.font.xs, { align_v = "Center" })
    end

    local clickable = not ap.active and not entry.blocked
    return panel_row {
        slot = "network-ap-" .. tostring(ap.ssid),
        leading = row { align_v = "Center", children = leading },
        title = ap.ssid or "?",
        subtitle = entry.connecting and "connecting…" or nil,
        selected = ap.active,
        opacity = entry.blocked and theme.opacity.disabled or nil,
        trailing = row { spacing = theme.spacing.xs, align_v = "Center", children = trailing },
        on_activate = clickable and function()
            -- `hidden` is required (§ 3.2); scanned `available_networks` entries are not hidden.
            obelisk.network:invoke("connect", ap.ssid, false)
        end or nil,
    }
end

-- ## The credential sheet
-- `NetworkPanel.qml`'s `CredentialSheet`, which is the part of the mirror this panel was missing:
-- one block that walks a join from a typed name through the wait to the password, retitled at each
-- step, rather than two prompts stacked in the card. `lib/ui_state.lua`'s `credential_step` says
-- which step is on screen and `hidden_join` whether the list should stand aside for it.
--
-- It answers the plain row click too, whose `password_ssid` is `"password"` with no name step in
-- front of it. The mirror puts that form inside the row it belongs to; one sheet is the shape this
-- surface can hold, because a form per row would put a `secure_submit` field in every one of them.
local step = ui.credential_step

local function during(...)
    local wanted = {}
    for _, name in ipairs({ ... }) do
        wanted[name] = true
    end
    return step:map(function(current)
        return wanted[current] == true
    end)
end

-- The mirror's three titles, plus one it has no need for: a failure keeps the sheet up here, where
-- the mirror hands the error back to the row it came from.
local sheet_title = computed({ step, ui.hidden_ssid, obelisk.network }, function(current, name, n)
    local target = (n and n.password_ssid) or name
    if current == "name" then
        return { { text = "hidden network", bold = true } }
    elseif current == "waiting" then
        return { { text = string.format("connecting to “%s”", target), bold = true } }
    elseif current == "failed" then
        return { { text = string.format("could not join “%s”", target), bold = true } }
    end
    return { { text = string.format("connect to “%s”", target), bold = true } }
end)

-- The mirror's `OInput`: a glass box with a ring, which `textfield` cannot draw itself. The ring is
-- accent throughout because the sheet's field is the only thing on this surface that can hold the
-- keyboard -- `hasError ? critical : activeFocus ? active : border` needs a focus a config cannot
-- observe (ADR-0102), and the two states it would tell apart are the same state here.
local function field_box(shown, field)
    return rect {
        visible = shown,
        width = "Fill",
        height = theme.control.md,
        radius = theme.radius.md,
        background = theme.GLASS_CONTROL,
        border_width = theme.border_width,
        border_color = theme.ACCENT,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = { field },
    }
end

-- Enter, or Next. `hidden = true` is what makes the Supervisor write `802-11-wireless.hidden` and
-- `scan-ssid`, so NetworkManager probes for the name instead of waiting to see it advertised
-- (§ 3.2) -- and what makes it treat the target as secured even though no scanned row says so,
-- since a network it cannot see is one it cannot ask about. So either a saved profile answers and
-- the join goes through, or `password_ssid` comes back and this same sheet asks for the rest.
--
-- The name is kept because the sheet is titled with it and a Retry reconnects to it; the Supervisor
-- has its own copy parked under the intent.
--
-- Submitting nothing is not an attempt to join "": leave the step where it is.
local function submit_hidden_name()
    local name = ui.hidden_draft:get():match("^%s*(.-)%s*$")
    if name == "" then
        return
    end
    ui.hidden_ssid:set(name)
    obelisk.network:invoke("connect", name, true)
end

-- A failed attempt leaves no pending intent: `connect` consumes it and `begin_connect` clears the
-- prompt before `finish_connect` records the verdict. Retry starts a fresh `connect`, not a
-- resubmission; it takes the sheet from "failed" back to "password" with the name it already knows.
local function retry_hidden()
    obelisk.network:invoke("connect", ui.hidden_ssid:get(), true)
end

local body = {
    panel_header {
        title = "network",
        icon = obelisk.network:map(header_glyph),
        active = obelisk.network:map(function(n)
            return n ~= nil and n.networking_enabled
        end),
        subtitle = obelisk.network:map(state_line),
        trailing = {
            -- The mirror swaps rescan for a spinner while scanning; `scanning` flips on click (§ 2.5).
            icon_button(icons.refresh, function()
                obelisk.network:invoke("scan")
            end, {
                slot = "network-rescan",
                size = theme.control.sm,
                icon_size = theme.icon.sm,
                visible = util.shown_when(obelisk.network, function(n)
                    return radio_on(n) and not n.scanning
                end),
            }),
            spinner(util.shown_when(obelisk.network, function(n)
                return n.scanning
            end), theme.icon.md),
            -- `NetworkService.setNetworkingEnabled` controls the whole stack; off hides the tiles,
            -- avoiding a radio control that does nothing.
            toggle(obelisk.network, function(n)
                return n.networking_enabled
            end, function(new_value)
                obelisk.network:invoke("set_networking_enabled", new_value)
            end),
        },
    },
    -- Two radio tiles, each with its address under the label; Wi-Fi adds the joined band.
    row {
        width = "Fill",
        spacing = theme.spacing.xs,
        visible = util.shown_when(obelisk.network, function(n)
            return n.networking_enabled
        end),
        children = {
            panel_toggle_card {
                slot = "network-wifi-tile",
                icon = icons.wifi[4],
                label = "wi-fi",
                -- Keyed on the association, not `ssid`. A docked laptop's joined radio still has an
                -- address while `ssid` names the cable.
                detail = util.label(obelisk.network, function(n)
                    local ap = util.active_access_point(n)
                    if ap == nil then
                        return ""
                    end
                    return detail_line(table.pack(n.wifi_ip, (util.band_of(ap))))
                end),
                signal = obelisk.network,
                read = function(n)
                    return n.wifi_enabled
                end,
                on_change = function(new_value)
                    obelisk.network:invoke("set_wifi_enabled", new_value)
                end,
            },
            panel_toggle_card {
                slot = "network-ethernet-tile",
                icon = icons.ethernet,
                label = "ethernet",
                detail = util.label(obelisk.network, function(n)
                    return n.ethernet_enabled and detail_line(table.pack(n.ethernet_ip, speed_text(n.ethernet_speed)))
                        or ""
                end),
                signal = obelisk.network,
                read = function(n)
                    return n.ethernet_enabled
                end,
                on_change = function(new_value)
                    obelisk.network:invoke("set_ethernet_enabled", new_value)
                end,
            },
        },
    },
    -- Mirror error card, red on a red-tinted ground, closed by its own button or the next attempt.
    -- It yields to the sheet, where `visible: ... && !root.isHiddenTarget` prevents two copies
    -- reading as two failures.
    row {
        width = "Fill",
        spacing = theme.spacing.sm,
        align_v = "Center",
        padding = { top = theme.spacing.sm, right = theme.spacing.sm, bottom = theme.spacing.sm, left = theme.spacing.sm },
        radius = theme.radius.md,
        background = theme.ALERT_BG,
        visible = computed({ obelisk.network, step, error_dismissed }, function(n, current, dismissed)
            return current == ""
                and not dismissed
                and n ~= nil
                and n.connect_error ~= nil
                and n.connecting_ssid == nil
        end),
        children = {
            glyph(icons.warning, theme.RED, theme.icon.sm, { align_v = "Center" }),
            cell(util.label(obelisk.network, function(n)
                return n.connect_error or ""
            end), theme.RED, theme.font.sm, { width = "Fill", wrap = "Word", max_lines = 2 }),
            panel_action_icon(icons.close, function()
                error_dismissed:set(true)
            end, { slot = "network-error-dismiss", tint = theme.RED }),
        },
    },
    -- The sheet's parts leave layout as the step moves; `panel_host` tweens the card's height to
    -- the section's measurement, so each step slides into the last one's room rather than snapping
    -- (ADR-0147). Typed passwords never reach this VM. `mask_character` plus `secure_submit` stores
    -- keystrokes in a native buffer on the Renderer's Wayland thread and sends a `("network",
    -- "connect")` envelope, as in `modules/global/lock.lua` (ADR-0005/ADR-0027); no `on_change` or
    -- `on_submit` callback can reopen that hole. `submit = true` is the only password-button path
    -- (ADR-0114). The masked field is the only `secure_submit` field across `panel_host`'s nine
    -- panels. The engine focuses a surface's *sole* such field and refuses to guess between two, so
    -- the name field is plain -- an SSID is an ordinary `connect` argument, which is why it can be
    -- typed. Only shown fields are counted or armed by
    -- `layout::secure_submit::typable_secure_submit_targets`, letting each step take the keyboard
    -- while the other field is down.
    column {
        width = "Fill",
        spacing = theme.spacing.sm,
        visible = step:map(function(current)
            return current ~= ""
        end),
        children = {
            cell(sheet_title, theme.FG, theme.font.sm, { width = "Fill" }),
            -- `autofocus` rather than a click: the row that raises this sheet is the last thing the
            -- pointer touches, and `panel_host` turns keyboard `Exclusive` on the same edge. The
            -- draft is stored on every keystroke because Next has no other way to read the field.
            field_box(during("name"), textfield {
                width = "Fill",
                height = "Fill",
                autofocus = true,
                placeholder = "network name",
                font_size = theme.font.sm,
                foreground = theme.FG,
                on_change = function(typed)
                    ui.hidden_draft:set(typed or "")
                end,
                on_submit = submit_hidden_name,
                -- Escape empties the field and releases the keyboard (ADR-0102); take the sheet
                -- down with it rather than leaving an empty field holding focus.
                on_cancel = ui.clear_network_prompts,
            }),
            field_box(during("password"), textfield {
                width = "Fill",
                height = "Fill",
                placeholder = "password",
                mask_character = "*",
                secure_submit = { capability = "network", action = "connect" },
                font_size = theme.font.sm,
            }),
            -- The mirror's `OSpinner` beside "Connecting…".
            row {
                spacing = theme.spacing.xs,
                align_v = "Center",
                visible = during("waiting"),
                children = { spinner(during("waiting"), theme.icon.md), cell("connecting…", theme.DIM, theme.font.xs) },
            },
            -- `⚠ errorMessage` under the field, not at the card's top, where the mirror puts it:
            -- the error belongs to the network being asked about. A password step carries one
            -- when NetworkManager rejected the last key and the Supervisor asked again.
            row {
                width = "Fill",
                spacing = theme.spacing.xs,
                align_v = "Center",
                visible = computed({ step, obelisk.network }, function(current, n)
                    return current == "failed" or (current == "password" and n ~= nil and n.connect_error ~= nil)
                end),
                children = {
                    glyph(icons.warning, theme.RED, theme.icon.sm, { align_v = "Center" }),
                    cell(util.label(obelisk.network, function(n)
                        return n.connect_error or ""
                    end), theme.RED, theme.font.xs, { width = "Fill", wrap = "Word", max_lines = 2 }),
                },
            },
            row {
                width = "Fill",
                align_h = "End",
                spacing = theme.spacing.sm,
                children = {
                    action_button("cancel", ui.clear_network_prompts, "network-sheet-cancel", { tone = "quiet" }),
                    -- Hidden rather than disabled while the name is empty: `action_button` has no
                    -- disabled tone, and a useless button is better absent than greyed.
                    -- Enter does the same thing for anyone already typing.
                    action_button("next", submit_hidden_name, "network-sheet-next", {
                        tone = "solid",
                        visible = computed({ step, ui.hidden_draft }, function(current, draft)
                            return current == "name" and draft:match("^%s*(.-)%s*$") ~= ""
                        end),
                    }),
                    -- No `on_activate`: its click *is* the field's Enter (ADR-0114), which is the
                    -- only path a password has out of the Renderer.
                    action_button("connect", nil, "network-sheet-connect", {
                        tone = "solid",
                        submit = true,
                        visible = during("password"),
                    }),
                    action_button("retry", retry_hidden, "network-sheet-retry", {
                        tone = "solid",
                        glyph = icons.warning,
                        visible = during("failed"),
                    }),
                },
            },
        },
    },
    -- Rows up to the cap, then a scrolling viewport (ADR-0110), matching
    -- `Math.min(networkList.contentHeight, Theme.itemHeight * 7)`. The sheet replaces it during a
    -- hidden join rather than above it, matching the mirror's `visible: !root.isHiddenTarget`;
    -- the card tweens down to the sheet's height instead of growing to hold both.
    list {
        width = "Fill",
        max_height = theme.panel_list_height,
        scroll = SCROLL,
        spacing = theme.spacing.xs,
        visible = radio_up_and_idle,
        source = rows,
        itemfn = access_point_row,
        key = function(entry)
            return tostring(entry.ap.ssid)
        end,
    },
    -- The one row nothing scanned put there, last as in the mirror. A network broadcasting no SSID
    -- is dropped from `available_networks`, so this stands in for it and asks for the name instead.
    -- It leaves with the list while the sheet is asking.
    panel_row {
        slot = "network-hidden",
        icon = icons.wifi_hidden,
        title = "hidden network…",
        visible = radio_up_and_idle,
        trailing = glyph(icons.chevron_right, theme.DIM, theme.font.sm, { align_v = "Center" }),
        on_activate = ui.open_hidden_prompt,
    },
    panel_empty_state(
        obelisk.network:map(function(n)
            if n == nil then
                return "network unavailable"
            elseif not n.networking_enabled then
                return "networking off"
            elseif not n.wifi_enabled then
                return "wi-fi off"
            elseif n.scanning then
                return "scanning…"
            end
            return "no networks found"
        end),
        computed({ obelisk.network, ui.hidden_join }, function(n, joining)
            return not radio_on(n) or (not joining and #access_points(n) == 0)
        end),
        {
            icon = obelisk.network:map(function(n)
                return radio_on(n) and icons.wifi_none or icons.wifi_off
            end),
        }
    ),
}

return { kind = KIND, body = body }
