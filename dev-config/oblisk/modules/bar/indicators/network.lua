-- Mirrors NetworkIndicator.qml.
--
-- Clicking opens Bar/Panels/NetworkPanel.qml's counterpart, which is how every status indicator in
-- that config reaches its detail view. The `kind` comes from the panel module rather than being
-- spelled here twice, and the rect `on_click` hands back is what the popup anchors to
-- (ADR-0050 decision 3).
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")
local ui_state = require("lib.ui_state")
local network_panel = require("modules.bar.panels.network_panel")

return pill({
    button {
        height = 18,
        align_v = "Center",
        on_click = function(rect, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            ui_state.open_panel(network_panel.kind, rect)
        end,
        children = { cell(util.label(oblisk.network, function(n)
            for _, ap in ipairs(n.available_networks or {}) do
                if ap.active then
                    return string.format("%s %d%%", util.truncate(ap.ssid, 12), ap.strength or 0)
                end
            end
            return n.scanning and "scanning" or "offline"
        end), theme.ACCENT) },
    },
})
