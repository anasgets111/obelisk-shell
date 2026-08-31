-- Mirrors Modules/Shell/PanelHost.qml: one popup surface that every bar panel is shown in, rather
-- than one surface per panel.
--
-- Not just economy. An `xdg_popup` takes a grab, and a grab is exclusive: two popups up at once is
-- not a state this protocol has, so a surface each would be five objects to keep mutually shut by
-- hand. One surface with a `kind` signal makes that impossible to get wrong -- whichever panel was
-- asked for last is the one on screen, because there is only one screen slot.
--
-- The cost is that every panel shares one size. A popup has no `Fill` (§ 6.3) and its width and
-- height are read once at declaration, so this is the largest body any panel carries rather than a
-- fit to the current one. A panel that needs its own geometry wants its own surface.
--
-- That cost got much smaller. Every body now ends in a `list` with a `scroll` of its own
-- (docs/adr/0069), so the shared size bounds the *viewport* and a panel with more rows than fit
-- scrolls instead of being cut off. What still has to fit is the fixed rows above each list, which
-- is what `the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing` measures.
local theme = require("config.theme")
local panel_card = require("components.panel_card")
local ui_state = require("lib.ui_state")

local power_menu = require("modules.bar.panels.power_menu")
local network_panel = require("modules.bar.panels.network_panel")
local bluetooth_panel = require("modules.bar.panels.bluetooth_panel")
local calendar_panel = require("modules.bar.panels.minimal_calendar")
local notification_history = require("modules.bar.panels.notification_history")

local panels = { power_menu, network_panel, bluetooth_panel, calendar_panel, notification_history }

-- Every panel's body is built and handed to the card; only the one whose `kind` matches is
-- visible. An invisible child contributes nothing to its parent's size (`resolve_sizes` in
-- scene.rs), so the stacked columns cost the height of whichever one is showing rather than the
-- sum of all of them.
local function panel_section(panel)
    return column {
        width = "Fill",
        height = "Fill",
        spacing = theme.spacing.xs,
        visible = ui_state.panel_kind:map(function(kind)
            return kind == panel.kind
        end),
        children = panel.body,
    }
end

local sections = {}
for _, panel in ipairs(panels) do
    table.insert(sections, panel_section(panel))
end

local surface = popup {
    id = "panel_host",
    -- The `id` of the surface this anchors to, not a node: the protocol roots a popup under a
    -- parent surface at creation (§ 6.3).
    parent = "bar",
    -- Bound as a signal, which is the spelling § 6.3 and ADR-0050 decision 3 prescribe, so the
    -- popup opens over whichever indicator was actually clicked.
    anchor_rect = ui_state.popup_anchor,
    -- Required and non-zero on both axes: a popup has no "Fill" (§ 6.3), because there is
    -- nothing for it to fill.
    -- Sized for the tallest panel, not the current one, because a popup's geometry is literal and
    -- read once. `the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing` in
    -- renderer/src/socket.rs measures every panel against these two numbers -- the power menu
    -- overran a 150px popup by 58px before it did.
    --
    -- A panel whose *list* is longer than this no longer has to fit, which is what changed: each
    -- body ends in a `list` with a `scroll` of its own, so the popup bounds the viewport rather
    -- than the content (docs/adr/0069). The fixed rows above each list still have to fit, and that
    -- is what the test measures.
    width = theme.panel_width,
    height = theme.panel_height,
    anchor = "BottomLeft",
    gravity = "BottomRight",
    constraint_adjustment = { "FlipY", "SlideX" },
    offset = { x = 0, y = theme.panel_gap },
    -- `visible` going true is what creates the `xdg_popup`, and it may only do so from inside a
    -- click, because that is the only turn a grab serial is armed for (ADR-0049's amendment).
    visible = ui_state.panel_open,
    -- Fired when the compositor dismisses this, which for a grabbing popup is a click anywhere
    -- outside it (§ 6.3). Writing the flag back is the config's half of ADR-0051 decision 2: the
    -- engine has already destroyed the object and latched the declaration shut, and this false is
    -- what unlatches it so the next click can reopen it.
    --
    -- Called with no arguments, so there is no telling which popup was dismissed; see
    -- `open_panel` in `lib/ui_state.lua` for what that costs.
    on_dismiss = ui_state.close_panel,
    child = panel_card(sections, {
        width = "Fill",
        height = "Fill",
        background = theme.GLASS,
        border_width = theme.border_width,
        border_color = theme.BORDER,
    }),
}

return surface
