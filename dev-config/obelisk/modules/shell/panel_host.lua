-- Mirrors PanelHost.qml: one surface displays every bar panel.
--
-- One screen slot shows the last-requested panel. Five surfaces would need manual mutual exclusion;
-- one `kind` signal makes that impossible to get wrong.
--
-- ## `panel`, not `popup` (ADR-0087)
--
-- The old `xdg_popup` grab was compositor-owned: niri gave the keyboard only if its parent held it.
-- when the popup mapped, and dismissed it if the parent's `keyboard_interactivity` changed. A
-- password prompt appearing *while* the panel was open could not get focus; the bar had to
-- claim the keyboard when opening any panel (ADR-0085 decision 4, retired).
--
-- A layer surface has no grab, so `keyboard_interactivity` binds only to the fact needing keys and
-- the bar never asks. `MainScreen.qml` uses the same screen-tall `PanelWindow` shape, with no
-- `xdg_popup`, letting `WlrLayershell.keyboardFocus` follow per-panel `needsKeyboardFocus`, as this
-- follows `password_ssid`.
--
-- Three things return, one is paid for:
--
--   * The catcher handles click-outside instead of compositor `popup_done`, so it knows which panel
--     closed and resolves `lib/ui_state.lua`'s `toggle_panel` ambiguity.
--   * Switching panels is one click; no grab needs breaking and re-arming.
--   * `visible` maps/unmaps an existing surface (§ 6), so no armed grab serial is needed
--   * Cost: popups had `constraint_adjustment`; layers do not, so the clamp is hand-written
--     `"SlideX"`.
--
-- ## Card height follows its panel (ADR-0110)
--
-- The old shared `theme.panel_height` fit the tallest body: four networks left over 200px of glass,
-- while history needed a second height and selection rule. The mirror uses
-- `panelItem.preferredHeight` and `Math.min(contentHeight, cap)` apply per list.
-- The card has no `height`; each list owns `max_height` and scrolls at the same cap.
local theme = require("config.theme")
local util = require("lib.util")
local panel_card = require("components.panel_card")
local ui_state = require("lib.ui_state")

local power_menu = require("modules.bar.panels.power_menu")
local network_panel = require("modules.bar.panels.network_panel")
local bluetooth_panel = require("modules.bar.panels.bluetooth_panel")
local notification_history = require("modules.bar.panels.notification_history")
local update_panel = require("modules.bar.panels.update_panel")
local audio_panel = require("modules.bar.panels.audio_panel")
local media_panel = require("modules.bar.panels.media_panel")
local tray_menu = require("modules.bar.panels.tray_menu")
local screen_recorder_panel = require("modules.bar.panels.screen_recorder_panel")

local panels = { power_menu, network_panel, bluetooth_panel, notification_history, update_panel, audio_panel,
    media_panel, tray_menu, screen_recorder_panel }

-- Build every body, but show only the matching `kind`. Invisible children contribute no size
-- (`resolve_sizes` in scene.rs), so stacked bodies cost the visible panel's height, not their sum.
-- Each section measures itself (`geometry`, ADR-0147). A hidden section keeps its last rect, so the
-- reveal knows its height before it shows, matching the mirror's `panelItem.preferredHeight`; a
-- section never shown yet reads zero.
local sections = {}
local section_rects = {}
for _, panel in ipairs(panels) do
    local rect = geometry("panel-section-" .. panel.kind)
    table.insert(section_rects, rect)
    table.insert(sections, column {
        width = "Fill",
        spacing = theme.spacing.xs,
        visible = ui_state.panel_kind:map(function(kind)
            return kind == panel.kind
        end),
        geometry = rect,
        children = panel.body,
    })
end

-- Shared width except history, updates, audio, and media. Those use their own widths because
-- history is a list, package rows need two version strings, audio sliders need length, and media
-- puts artwork beside the track rather than above it.
local PANEL_WIDTHS = {
    [notification_history.kind] = theme.notification_panel_width,
    [update_panel.kind] = theme.update_panel_width,
    [audio_panel.kind] = theme.audio_panel_width,
    [media_panel.kind] = theme.media_panel_width,
    [tray_menu.kind] = theme.tray_menu_width,
}

local card_width = ui_state.panel_kind:map(function(kind)
    return PANEL_WIDTHS[kind] or theme.panel_width
end)

-- Position formerly came from popup `anchor_rect`/`gravity`. `popup_anchor` is the indicator's
-- `on_click` rect (ADR-0050 decision 3) in bar coordinates; both surfaces are
-- left-anchored/full-width, so `x` needs no translation. Center under the indicator, then clamp
-- within `spacing.sm` of either edge, like `calculateX`. Left-edge anchoring made a card under a
-- button read as belonging to its right neighbor. The hand-written `"SlideX"` matters near the
-- clock/tray: a 340px card centered on a 1920px output would otherwise run off by half its width.
--
-- No `"FlipY"`: this starts below the bar. `screens[1]` on a hotplugged second head is the same
-- guess as `config/theme.lua`'s `main_screen`. This follows the signal, so resolution changes move
-- the clamp instead of stranding boot values. Empty `obelisk.screens` (first evaluation and
-- `socket.rs` harness) clamps only at zero.
local card_x = computed({ ui_state.popup_anchor, obelisk.screens, card_width }, function(anchor, screens, width)
    local anchor_x = (anchor and anchor.x) or 0
    local anchor_width = (anchor and anchor.width) or 0
    local x = anchor_x + anchor_width / 2 - width / 2
    local screen = screens and screens[1]
    if screen and screen.width then
        x = math.min(x, screen.width - width - theme.spacing.sm)
        x = math.max(x, theme.spacing.sm)
    end
    return math.floor(math.max(0, x))
end)

-- `PanelHost.qml`'s `revealProgress`: the card drops from just above the surface's top edge, the
-- bar's bottom, and retracts the same way while `linger` keeps the surface mapped for exit
-- (ADR-0146). No fade: the mirror moves `y` only, and the surface edge cuts the card as its `clip`
-- cuts `hiddenY`. Travel is the shown section's measured height plus card chrome, the mirror's
-- `-height`, read from the section so a closed switch retracts to the *next* card's height instead
-- starting a taller card part-visible. An unlaid section reads zero and falls back to the tallest
-- card, so its first open in a session drops from further up.
--
-- The horizontal placement is on a wrapper, not the card: a tween carries a whole edge table, and
-- `left` on the same node as `top` would slide the card sideways from the previous indicator's
-- anchor. The mirror animates `y` alone; `x` snaps.
--
-- Switching kinds while open does not retract: `PanelHost.qml` morphs the card in place with
-- `NumberTransition`s on `width` and `height`. The card height is the shown section's measured
-- height plus its chrome; a pass that changes a measurement earns one follow-up pass (ADR-0147),
-- keeping the card from sitting one pass behind a section that grew. An unmeasured section leaves
-- the card content-sized, which snaps that once.
--
-- `NotificationHistoryPanel.qml` sets `readonly property int padding: Theme.spacingMd` on all four
-- sides (`anchors.margins: root.padding`), sizing itself as
-- `contentColumn.implicitHeight + padding * 2`.
-- `panel_card`'s default is `sm` top and bottom, `md` left and right, which left every panel's
-- first line -- the greeting, a section heading -- on the card's top edge. The mirror's number is
-- on every edge.
local CARD_PADDING = theme.spacing.md
local CARD_CHROME = CARD_PADDING * 2 + theme.border_width * 2
local card_height = computed({ ui_state.panel_kind, table.unpack(section_rects) }, function(kind, ...)
    for index, panel in ipairs(panels) do
        if panel.kind == kind then
            local height = select(index, ...).height
            return height > 0 and height + CARD_CHROME or nil
        end
    end
    return nil
end)
local hidden_top = card_height:map(function(height)
    return height and -(height + theme.panel_gap) or -theme.panel_slide
end)
local card_margin = computed({ hidden_top, ui_state.panel_open }, function(hidden, open)
    return { top = open and theme.panel_gap or hidden }
end)
-- A signal makes `from` carry the hidden position, so the first open of a kind slides rather
-- than appears (a first value is taken as it is without one).
local card_animate = hidden_top:map(function(hidden)
    return {
        margin = { duration = theme.animation_ms, easing = "OutQuad", from = { top = hidden } },
        width = { duration = theme.animation_ms, easing = "OutCubic" },
        height = { duration = theme.animation_ms, easing = "OutCubic" },
    }
end)

return panel {
    id = "panel_host",
    -- Under `Overlay`, so notifications and OSD still draw/click over an open panel. They cover a
    -- corner; this covers everything, so it must yield.
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    -- Not `"Ignore"`: reserve nothing while respecting the bar reservation; the origin stays below
    -- it, so the card offset stays a gap, not `theme.bar_height`. niri configures this as
    -- 1920x1161 on a 1200px output with a 39px bar. The bar remains uncovered, so switching panels
    -- is one click on the indicator, not the catcher's surface.
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = util.linger(ui_state.panel_open, theme.animation_ms),
    -- The network panel's credential sheet asks for the keyboard at every step: a name, then a
    -- password. `ui_state.credential_step` is the whole question, and `clear_network_prompts` ends
    -- the sheet on every closing edge, so it cannot leave this surface holding the keyboard.
    --
    -- Hold it across the wait between the two. `"None"` in the gap would hand the keyboard back to
    -- whatever is behind the panel for as long as the Supervisor takes to answer.
    --
    -- `"Exclusive"`, not `"OnDemand"`: both fields must be typable without a click. The engine arms
    -- a scope's *sole* `secure_submit` field and first `autofocus` plain field on compositor focus
    -- (`layout::secure_submit`, `wayland::input`); the sheet shows one at a time. This surface
    -- never maps while either is pending; the sheet is raised by a click on an already-open panel.
    --
    -- Notifications also need it: history draws the popup's always-present reply field (ADR-0109),
    -- and niri focuses an `OnDemand` layer on a click while already on demand, not on the flip.
    -- Ask on demand only while history is shown: clicks there take the keyboard, other windows give
    -- it back, and the catcher closes the panel. Network and calendar never ask without a field.
    keyboard_interactivity = computed(
        { ui_state.credential_step, ui_state.panel_showing("notifications") },
        function(step, showing_notifications)
            -- The sheet stays `Exclusive`: this panel's click raised it and the catcher ends it.
            if step ~= "" then
                return "Exclusive"
            end
            return showing_notifications and "OnDemand" or "None"
        end
    ),
    -- One surface, one root node (§ 6): catcher and card share a `rect`. Full-fill visibility also
    -- sets input: `wl_surface::set_input_region` follows drawn/clickable tree content (ADR-0038
    -- decision 5, ADR-0109). The full-size catcher claims it while mapped, none while `visible` is
    -- false and unmapped.
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            -- Click-outside catcher first; the card paints over it. `hit::descend` walks children;
            -- reverse order stops at the first containing the point, so card clicks never reach it.
            --
            -- `close_panel` answers pending passwords, not this catcher. Outside click and a second
            -- indicator click are the same edge; one writer keeps them synchronized.
            button {
                width = "Fill",
                height = "Fill",
                cursor = "default",
                on_click = ui_state.close_panel,
            },
            -- No `height`: the card is its content.
            -- A stacking child starts at its parent origin (`parse_align` defaults to `Start`); the
            -- wrapper's margin is the full horizontal placement, the card's the vertical.
            column {
                margin = card_x:map(function(x)
                    return { left = x }
                end),
                children = {
                    panel_card(sections, {
                        width = card_width,
                        height = card_height,
                        margin = card_margin,
                        animate = card_animate,
                        background = theme.GLASS,
                        -- The card, not the surface: screen-sized `panel_host` has a blank catcher.
                        -- nothing, so the region is the card alone (ADR-0195). History cards sit on
                        -- this glass and do not ask again.
                        blur = true,
                        border_width = theme.border_width,
                        border_color = theme.BORDER,
                        padding = {
                            top = CARD_PADDING,
                            right = CARD_PADDING,
                            bottom = CARD_PADDING,
                            left = CARD_PADDING,
                        },
                    }),
                },
            },
        },
    },
}
