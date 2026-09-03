-- Mirrors Modules/Shell/PanelHost.qml: one surface that every bar panel is shown in, rather than
-- one surface per panel.
--
-- Not just economy. Whichever panel was asked for last is the one on screen, because there is only
-- one screen slot -- five surfaces would be five objects to keep mutually shut by hand, and a
-- `kind` signal against one surface makes that impossible to get wrong.
--
-- ## Why this is a `panel` and not a `popup` (ADR-0087)
--
-- It was an `xdg_popup` with a grab until now, and the grab is what had to go. A grab is the
-- compositor's, not ours: niri hands a grabbing popup the keyboard only if its parent already held
-- it when the popup mapped, and re-evaluates focus -- dismissing the popup -- if the parent's
-- `keyboard_interactivity` moves afterwards. So a password prompt that appears *while* the panel is
-- open could not be given the keyboard by this surface at all. The bar had to claim it in advance,
-- on the pass that opened the panel, which meant every open panel took the whole keyboard whether
-- it wanted typing or not (ADR-0085 decision 4, now retired).
--
-- A layer surface has no grab and no such rule, so `keyboard_interactivity` below binds to the one
-- fact that actually wants the keyboard, and the bar goes back to never asking for it.
-- `Modules/Shell/MainScreen.qml` is the same shape and reached it the same way: it is one
-- screen-tall `PanelWindow` holding the bar and the panel host together, with no `xdg_popup`
-- anywhere, which is why its `WlrLayershell.keyboardFocus` can follow a per-panel
-- `needsKeyboardFocus` the way this one follows `password_ssid`.
--
-- Three things come back for free and one is paid for:
--
--   * Click-outside-to-close is the catcher below rather than the compositor's `popup_done`, so it
--     knows which panel it closed -- the ambiguity `lib/ui_state.lua`'s `toggle_panel` was written
--     around.
--   * Switching panels is one click. There is no grab to break and re-arm, so the click lands on
--     the bar indicator directly.
--   * `visible` is a map/unmap of a surface that already exists (§ 6.1), not a create that may only
--     happen inside a click, so nothing here depends on an armed grab serial (ADR-0049).
--   * Paid for: a popup got `constraint_adjustment` and a layer surface does not, so the
--     horizontal clamp below is `"SlideX"` written out by hand.
--
-- The cost that stays is that every panel shares one size, which is now a choice rather than the
-- protocol's: this is the largest body any panel carries. It is a small cost because every body
-- ends in a `list` with a `scroll` of its own (ADR-0069), so the shared size bounds the *viewport*
-- and a panel with more rows than fit scrolls. What still has to fit is the fixed rows above each
-- list, which is what `the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing`
-- measures.
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

-- Where the card sits, which an `xdg_popup` got from `anchor_rect` plus `gravity` and a layer
-- surface has to be told. `popup_anchor` is the rect `on_click` handed back for the indicator that
-- opened this (ADR-0050 decision 3), in the bar's logical coordinates; the bar is anchored left and
-- full width and so is this surface, so its `x` needs no translation.
--
-- The `math.min` is `constraint_adjustment = { "SlideX" }` by hand: the clock and the tray sit near
-- the right edge, and a 340px card hung off their left edge runs off a 1920px output by more than
-- half its width. `"FlipY"` needs no equivalent, because this surface starts below the bar and
-- extends down, so there is nothing to flip away from.
--
-- `screens[1]` on a hotplugged second head is a guess, and it is the same guess
-- `config/theme.lua`'s `main_screen` already makes. Unlike that one this follows the signal, so a
-- resolution change moves the clamp rather than stranding it at the boot value. An empty
-- `oblisk.screens` -- the first evaluation, and `socket.rs`'s harness -- clamps nothing, which
-- leaves the card exactly where the anchor asked.
local card_margin = computed({ ui_state.popup_anchor, oblisk.screens }, function(anchor, screens)
    local x = (anchor and anchor.x) or 0
    local screen = screens and screens[1]
    if screen and screen.width then
        x = math.min(x, math.max(0, screen.width - theme.panel_width))
    end
    return { left = math.floor(x), top = theme.panel_gap }
end)

return panel {
    id = "panel_host",
    -- Under `Overlay`, where `modules/notification/popup.lua` and `modules/osd/popup.lua` live, so
    -- a notification arriving while a panel is open still draws over it and still takes its own
    -- clicks. Both of those cover a corner; this covers everything, so being the one that yields is
    -- the only arrangement where all three work.
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    -- Deliberately not `"Ignore"`: reserving nothing while still respecting what the bar reserved
    -- is what puts this surface's origin just under the bar, so the card's own offset is a gap
    -- rather than a copy of `theme.bar_height` that would go stale the moment the bar resized.
    -- niri configures this at 1920x1161 on a 1200px output with a 39px bar, which is the measured
    -- version of that sentence. It also leaves the bar uncovered, which is why switching panels is
    -- one click -- the click reaches the indicator instead of this surface's catcher.
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = ui_state.panel_open,
    -- The whole point of the rewrite. `password_ssid` names the network whose `network:connect` is
    -- waiting on a password and is `nil` the rest of the time (§ 2.5), so this surface holds the
    -- keyboard for exactly as long as there is something to type and gives it back the moment there
    -- is not. Nothing else on the bar ever asks for it.
    --
    -- `"Exclusive"` rather than `"OnDemand"`, because the field must be typable without first
    -- clicking it: the engine arms a focus scope's *sole* `secure_submit` field when the compositor
    -- hands it keyboard focus (`layout::secure_submit`'s `sole_secure_submit_in_scope`), and
    -- `network_panel.lua`'s prompt is that field. It is safe as a binding in a way it would not be
    -- as a constant -- niri gives an `on_demand` or `exclusive` layer surface focus the moment it
    -- *maps*, and this surface never maps while a password is pending, because the prompt is raised
    -- by the Supervisor in answer to a click that only happens once a panel is already open.
    --
    -- Guarded for nil like every other bare `:map` on a capability: this resolves once before the
    -- first push.
    --
    -- Two facts want it now, not one. The notification history draws the same card the popup does,
    -- so a reply field can be open in here too, and it needs the keyboard on the same terms the
    -- password prompt does -- raised only once something has asked, never merely because a panel
    -- is showing.
    --
    -- `reply_open` alone is not that ask. It is one signal for both places the card is drawn, so a
    -- reply opened in the *popup* would otherwise raise this surface too, and a panel showing
    -- something else entirely -- the network list, the calendar -- would take the keyboard because
    -- of a notification it is not displaying. Gated on the notifications panel actually being the
    -- one on screen, which is the condition that makes the open field one of ours.
    keyboard_interactivity = computed(
        { oblisk.network, ui_state.reply_open, ui_state.panel_showing("notifications") },
        function(n, reply_open, showing_notifications)
            -- Two asks, two answers. A password prompt keeps `Exclusive`: it was raised by a
            -- click on this panel and the click-outside catcher below is how it ends, so nothing
            -- else can want a key meanwhile. A reply is `OnDemand` (ADR-0108): the keyboard
            -- follows the pointer to other windows and back, the draft stays, and the click-outside
            -- catcher below is what closes the panel and the reply together (`close_panel`).
            if n and n.password_ssid then
                return "Exclusive"
            end
            return (reply_open and showing_notifications) and "OnDemand" or "None"
        end
    ),
    -- One surface, one root node (§ 6.1), so the catcher and the card share a `rect` rather than
    -- being two children of the surface. Full-fill and visible, which is also what sets the input
    -- region: `wl_surface::set_input_region` is built from the surface root's visible direct
    -- children (ADR-0038 decision 5), so this claims the whole surface while it is mapped and none
    -- of it while `visible` above is false and the surface is unmapped.
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            -- Click-outside-to-close, declared first so the card paints over it. `hit::descend`
            -- walks children in reverse and stops at the first that contains the point, so a click
            -- landing on the card never reaches this button -- the card does not need a handler of
            -- its own to shield itself.
            --
            -- Answering a pending password prompt is `close_panel`'s job, not this one's: a click
            -- out here and a second click on the open panel's own indicator are the same event,
            -- and one writer per edge is what keeps them from drifting apart.
            button {
                width = "Fill",
                height = "Fill",
                on_click = ui_state.close_panel,
            },
            -- Sized for the tallest panel, not the current one. `renderer/src/socket.rs`'s
            -- `the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing` measures
            -- every panel against these two numbers -- the power menu overran a 150px card by 58px
            -- before it did.
            panel_card(sections, {
                width = theme.panel_width,
                height = theme.panel_height,
                -- A stacking child sits at its parent's origin unless told otherwise
                -- (`parse_align` defaults to `Start`), so the margin above is the whole of the
                -- placement rather than a nudge to it.
                margin = card_margin,
                background = theme.GLASS,
                border_width = theme.border_width,
                border_color = theme.BORDER,
            }),
        },
    },
}
