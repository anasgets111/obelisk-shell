-- The pairing prompt for the Supervisor's `org.bluez.Agent1`. BluetoothService.qml had none: its
-- `bluetoothctl --agent NoInputNoOutput` accepted every request, which let any nearby device pair
-- while the adapter was visible.
--
-- A card at the top of the screen, sized to itself, with no scrim. A device in range can raise it,
-- so it must not cover the desktop or hold the pointer. Buttons only, so it never takes the
-- keyboard. A code to type on the device has nothing to answer, and its one button only takes the
-- prompt down.
local theme = require("config.theme")
local cell = require("components.cell")
local panel_card = require("components.panel_card")
local action_button = require("components.action_button")

local function request(b)
    return b and b.pairing_request
end

-- A signal of `predicate(request)`, false while nothing is asked.
local function when(predicate)
    return obelisk.bluetooth:map(function(b)
        local r = request(b)
        return r ~= nil and predicate(r)
    end)
end

-- A signal of `read(request)`, empty while nothing is asked.
local function text(read)
    return obelisk.bluetooth:map(function(b)
        local r = request(b)
        return r and read(r) or ""
    end)
end

local TITLES = {
    confirm = "pair with %s?",
    authorize = "%s wants to pair",
    service = "%s wants to connect",
    display = "type this code on %s",
}

local DETAILS = {
    confirm = "pair only if the device shows the same code",
    authorize = "accept only a device you are pairing right now",
    service = "the device is paired but not trusted",
    display = "then press enter on the device",
}

-- Answers the request showing when the click lands, by its MAC. The Supervisor ignores a yes in the
-- first moments after a request appears, so one that replaced another cannot take a click meant
-- for the first.
local function answer(accept)
    return function()
        local r = request(obelisk.bluetooth:get())
        if r ~= nil then
            obelisk.bluetooth:invoke("answer_pairing", r.mac, accept)
        end
    end
end

local asks = when(function(r)
    return r.kind ~= "display"
end)
local pad = theme.spacing.lg

return panel {
    id = "bluetooth_pairing",
    namespace = "obelisk-bluetooth-pairing",
    layer = "Overlay",
    -- Top edge only. The protocol centres an axis with neither edge anchored, and with no `width` or
    -- `height` the surface is the card.
    anchor = { top = true },
    margin = { top = theme.s(96, 64) },
    exclusive = false,
    visible = when(function()
        return true
    end),
    keyboard_interactivity = "None",
    child = panel_card({
        cell(text(function(r)
            local name = r.name ~= "" and r.name or r.mac
            return { { text = string.format(TITLES[r.kind] or "%s", name), bold = true } }
        end), theme.FG, theme.font.md, { width = "Fill", wrap = "Word" }),
        cell(text(function(r)
            return r.code or ""
        end), theme.ACCENT, theme.font.xxl, {
            width = "Fill",
            align = "Center",
            visible = when(function(r)
                return r.code ~= nil
            end),
        }),
        cell(text(function(r)
            return DETAILS[r.kind] or ""
        end), theme.DIM, theme.font.sm, { width = "Fill", wrap = "Word" }),
        row {
            width = "Fill",
            align_h = "End",
            spacing = theme.spacing.sm,
            children = {
                action_button("reject", answer(false), "bluetooth-pairing-reject", {
                    tone = "quiet",
                    visible = asks,
                }),
                action_button(
                    text(function(r)
                        return r.kind == "service" and "allow" or "pair"
                    end),
                    answer(true),
                    "bluetooth-pairing-accept",
                    { tone = "solid", visible = asks }
                ),
                action_button("done", answer(false), "bluetooth-pairing-done", {
                    tone = "solid",
                    visible = when(function(r)
                        return r.kind == "display"
                    end),
                }),
            },
        },
    }, {
        width = theme.dialog_width,
        spacing = theme.spacing.md,
        padding = { top = pad, right = pad, bottom = pad, left = pad },
        radius = theme.radius.lg,
        background = theme.GLASS,
        blur = true,
        border_width = theme.border_width,
        border_color = theme.BORDER,
    }),
}
