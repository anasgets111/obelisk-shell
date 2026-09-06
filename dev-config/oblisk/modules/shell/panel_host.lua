-- Mirrors PanelHost.qml: one surface displays every bar panel.
--
-- One screen slot means the last-requested panel is the one shown. Five surfaces would need manual
-- mutual exclusion; one `kind` signal makes that impossible to get wrong.
--
-- ## `panel`, not `popup` (ADR-0087)
--
-- The old `xdg_popup` grab was compositor-owned: niri gave it the keyboard only if the parent held
-- it when the popup mapped, and dismissed it if the parent's `keyboard_interactivity` changed.
-- A password prompt appearing *while* the panel was open therefore could not get focus; the bar
-- had to claim the keyboard when opening any panel (ADR-0085 decision 4, retired).
--
-- A layer surface has no grab, so `keyboard_interactivity` binds only to the fact needing keys and
-- the bar never asks. `MainScreen.qml` uses the same screen-tall `PanelWindow` shape, with no
-- `xdg_popup` anywhere, letting `WlrLayershell.keyboardFocus` follow per-panel
-- `needsKeyboardFocus`, as this follows `password_ssid`.
--
-- Three things return, one is paid for:
--
--   * The catcher handles click-outside instead of compositor `popup_done`, so it knows which panel
--     closed, resolving `lib/ui_state.lua`'s `toggle_panel` ambiguity.
--   * Switching panels is one click; no grab needs breaking and re-arming.
--   * `visible` maps/unmaps an existing surface (§ 6), so no armed grab serial is needed
--     (ADR-0049).
--   * Paid for: popups had `constraint_adjustment`; layers do not, so the clamp is hand-written
--     `"SlideX"`.
--
-- ## Card height follows its panel (ADR-0110)
--
-- The old shared `theme.panel_height` fit the tallest body: four networks left over 200px of glass,
-- while history needed a second height and selection rule. The mirror uses
-- `panelItem.preferredHeight`
-- and `Math.min(contentHeight, cap)` per list. The card has no `height`; each list owns
-- `max_height`
-- and scrolls at the same cap.
local theme = require("config.theme")
local panel_card = require("components.panel_card")
local ui_state = require("lib.ui_state")

local power_menu = require("modules.bar.panels.power_menu")
local network_panel = require("modules.bar.panels.network_panel")
local bluetooth_panel = require("modules.bar.panels.bluetooth_panel")
local calendar_panel = require("modules.bar.panels.minimal_calendar")
local notification_history = require("modules.bar.panels.notification_history")
local update_panel = require("modules.bar.panels.update_panel")
local audio_panel = require("modules.bar.panels.audio_panel")

local panels = { power_menu, network_panel, bluetooth_panel, calendar_panel, notification_history, update_panel, audio_panel }

-- Build every body, but show only the matching `kind`. Invisible children contribute no size
-- (`resolve_sizes` in scene.rs), so stacked bodies cost the visible panel's height, not their sum.
local function panel_section(panel)
    return column {
        width = "Fill",
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

-- Shared width except history, updates, and audio. Those use their own widths because history is a
-- list, package rows need two version strings, and audio sliders need length.
local PANEL_WIDTHS = {
    [notification_history.kind] = theme.notification_panel_width,
    [update_panel.kind] = theme.update_panel_width,
    [audio_panel.kind] = theme.audio_panel_width,
}

local card_width = ui_state.panel_kind:map(function(kind)
    return PANEL_WIDTHS[kind] or theme.panel_width
end)

-- Position formerly came from popup `anchor_rect`/`gravity`. `popup_anchor` is the indicator's
-- `on_click` rect (ADR-0050 decision 3) in bar coordinates; both surfaces are
-- left-anchored/full-width,
-- so `x` needs no translation.
--
-- Center under the indicator, then clamp within `spacing.sm` of either edge, like
-- `calculateX`. Left-edge anchoring made a card under a button read as belonging to its right
-- neighbor. The hand-written `"SlideX"` matters near the clock/tray: a 340px card centered on a
-- 1920px output would otherwise run off by half its width. No `"FlipY"`: this starts below the bar.
--
-- `screens[1]` on a hotplugged second head is the same guess as `config/theme.lua`'s `main_screen`.
-- This follows the signal, so resolution changes move the clamp instead of stranding boot values.
-- Empty `oblisk.screens` (first evaluation and `socket.rs` harness) clamps only at zero.
local card_margin = computed({ ui_state.popup_anchor, oblisk.screens, card_width }, function(anchor, screens, width)
    local anchor_x = (anchor and anchor.x) or 0
    local anchor_width = (anchor and anchor.width) or 0
    local x = anchor_x + anchor_width / 2 - width / 2
    local screen = screens and screens[1]
    if screen and screen.width then
        x = math.min(x, screen.width - width - theme.spacing.sm)
        x = math.max(x, theme.spacing.sm)
    end
    return { left = math.floor(math.max(0, x)), top = theme.panel_gap }
end)

return panel {
    id = "panel_host",
    -- Under `Overlay`, so notifications and OSD still draw/click over an open panel. They cover a
    -- corner; this covers everything, so it must yield.
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    -- Not `"Ignore"`: reserve nothing while respecting the bar's reservation, placing the origin
    -- below it so the card offset stays a gap instead of copying `theme.bar_height`. niri
    -- configures
    -- this as 1920x1161 on a 1200px output with a 39px bar. The bar remains uncovered, so switching
    -- panels is one click on the indicator, not the catcher's surface.
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = ui_state.panel_open,
    -- `password_ssid` names the network whose `network:connect` awaits a password and is nil
    -- otherwise (§ 2.5), so this holds the keyboard only while typing is needed.
    --
    -- `"Exclusive"`, not `"OnDemand"`: the field must be typable without a click. The engine arms a
    -- focus scope's *sole* `secure_submit` field on compositor focus
    -- (`layout::secure_submit::sole_secure_submit_in_scope`), and `network_panel.lua` has that
    -- field.
    -- This surface never maps while a password is pending; the Supervisor raises the prompt after a
    -- click on an already-open panel.
    --
    -- Nil-guarded like every bare capability `:map`; this resolves once before the first push.
    --
    -- Notifications also need it: history draws the popup's always-present reply field (ADR-0109),
    -- and niri focuses an `OnDemand` layer on a click while already on demand, not on the flip.
    -- Ask on demand only while history is shown: clicks there take the keyboard, other windows give
    -- it back, and the catcher closes the panel. Network and calendar never ask without a field.
    keyboard_interactivity = computed(
        { oblisk.network, ui_state.panel_showing("notifications") },
        function(n, showing_notifications)
            -- Password stays `Exclusive`: this panel's click raised it and the catcher ends it.
            if n and n.password_ssid then
                return "Exclusive"
            end
            return showing_notifications and "OnDemand" or "None"
        end
    ),
    -- One surface, one root node (§ 6): catcher and card share a `rect`. Full-fill visibility also
    -- sets input: `wl_surface::set_input_region` follows drawn/clickable tree content
    -- (ADR-0038 decision 5, ADR-0109). The full-size catcher claims it while mapped, none while
    -- `visible` is false and unmapped.
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            -- Click-outside catcher, first so the card paints over it. `hit::descend` walks
            -- children
            -- in reverse and stops at the first containing the point, so card clicks never reach
            -- it.
            --
            -- `close_panel` answers pending passwords, not this catcher. Outside click and a second
            -- indicator click are the same edge; one writer keeps them synchronized.
            button {
                width = "Fill",
                height = "Fill",
                cursor = "default",
                on_click = ui_state.close_panel,
            },
            -- No `height`: the card is its content. `renderer/src/socket.rs`'s
            -- `the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing` checks
            -- fit.
            panel_card(sections, {
                width = card_width,
                -- A stacking child starts at its parent origin (`parse_align` defaults to `Start`);
                -- the margin above is the full placement offset.
                margin = card_margin,
                background = theme.GLASS,
                border_width = theme.border_width,
                border_color = theme.BORDER,
            }),
        },
    },
}
