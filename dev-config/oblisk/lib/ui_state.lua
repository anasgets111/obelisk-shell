-- The rect of the bar indicator the panel host hangs its card off. ADR-0049's amendment settles
-- where it comes from: not off the input-dispatch stack, but through the config, because `on_click`
-- receives the button's own rect (ADR-0050 decision 3) and writes it to a named `state` signal the
-- surface reads back.
--
-- Only `x` is read now that `modules/shell/panel_host.lua` is a layer surface rather than a popup:
-- it places the card itself, below the bar and clamped to the output, where `anchor_rect` plus
-- `gravity` used to. The other three fields stay because this is still the shape `on_click` hands
-- over, and narrowing it here would only move the destructuring somewhere less obvious. The initial
-- is the indicator's declared size, which no longer has to be non-zero to keep the evaluation alive
-- but is still the honest starting value.
local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 70, height = 24 })
local settings_open = state("settings_open", false)

-- `modules/shell/panel_host.lua`'s two signals: whether the shared surface is up, and which panel
-- it is showing. One surface for every bar panel rather than one surface each, which is what
-- Quickshell's own Modules/Shell/PanelHost.qml is for -- only one of these can be on screen at a
-- time anyway, and one slot makes that true by construction rather than by five files agreeing.
local panel_open = state("panel_open", false)
local panel_kind = state("panel_kind", "")

-- The one place the panel host stops showing anything, and so the one place that answers a prompt
-- it was showing. `network:connect` on an unsaved secured network parks an intent in the Supervisor
-- and raises `password_ssid` (ADR-0085); closing takes the field off screen without answering it,
-- and nothing in the config could clear that intent by itself. `cancel_connect` is a no-op when
-- nothing is pending, which is what lets a generic close spend it unconditionally rather than every
-- close also clearing `connect_error`.
--
-- Here rather than at its two callers -- `panel_host`'s click-outside catcher and the toggle below
-- -- for the reason this file keeps repeating: one writer per edge. A capability call inside a
-- module named `ui_state` is the price, and it is the smaller one.
local function close_panel()
    panel_open:set(false)
    oblisk.network:invoke("cancel_connect")
end

-- Clicking an indicator opens its panel; clicking the same one again closes it. A toggle, which is
-- what the mirror does and what this could not do until `panel_host` stopped being an `xdg_popup`
-- (ADR-0087).
--
-- Set-only was not a style choice under the grab. niri still delivered a click on the opening
-- button to us, because the bar was the popup's own parent surface and so inside the grab's tree,
-- so a toggle would have closed the panel it was opening; and it would have fought `on_dismiss` on
-- every click landing elsewhere, since that path already wrote false. Worse, clicking the network
-- indicator while the bluetooth panel was up was both edges at once, and `on_dismiss` carried no
-- token saying which popup it dismissed, so switching panels directly sometimes took a second
-- click.
--
-- None of that survives a layer surface. Nothing dismisses this behind our back and the click that
-- switches panels reaches the indicator directly, so the three cases are just the three cases: the
-- showing panel closes, a different one replaces it, and a closed host opens.
local function toggle_panel(kind, rect)
    if panel_open:get() and panel_kind:get() == kind then
        close_panel()
        return
    end
    popup_anchor:set(rect)
    panel_kind:set(kind)
    panel_open:set(true)
end

-- Whether `kind` is the panel currently on screen, which is `ShellUiState.isPanelOpen(kind)` in the
-- mirror and what every bar indicator binds its accent ring to. Both signals are needed:
-- `panel_kind` keeps its last value after a close, so reading it alone leaves the indicator that
-- opened the panel ringed after the panel is gone.
local function panel_showing(kind)
    return computed({ panel_open, panel_kind }, function(open, current)
        return open and current == kind
    end)
end

-- The volume/brightness OSD's state: which reading `modules/osd/popup.lua` shows, and whether the
-- corner overlay is up at all. Lives here rather than in that file because arming it is a write a
-- bar button issues, and `modules/bar/indicators/volume.lua` and `modules/bar/panels/power_menu.lua`
-- cannot `require` a module that itself `require`s them without a cycle.
local osd_kind = state("osd_kind", "")
local osd_visible = state("osd_visible", false)

-- Whether the app launcher window is up. A `state` signal rather than a bar-button toggle inline
-- (`settings_open`'s own shape): `modules/global/launcher.lua`'s close button and
-- `modules/bar/indicators/launcher_button.lua`'s open button both need to write it, the same
-- reason `settings_open` lives here instead of inside `settings.lua`.
local launcher_open = state("launcher_open", false)

-- `process.run("sleep", ...)` is this engine's only timer -- there is no signal-change event a
-- config can observe (a `:map` callback runs during scene resolution and must stay pure, since
-- ADR-0044's rollback-on-error means resolution can rerun on the same inputs) and no `on_hover` to
-- fake one with. So the auto-hide has to be armed by the same click that changes the level, not by
-- watching `oblisk.audio`/`oblisk.brightness` push.
--
-- `ProcessHandle:kill()` sends a kill command but does not cancel the queued `exit_cb`
-- (`renderer/src/lua/process.rs`), so a second click while the first sleep is still running needs
-- `modules/bar/indicators/active_window.lua`'s own `claim_title_request` guard, not a kill, or the
-- first timer's expiry would hide the OSD out from under the newer one.
local OSD_SECONDS = "2"
local osd_hide_request = 0

local function arm_osd(kind)
    osd_kind:set(kind)
    osd_visible:set(true)
    osd_hide_request = osd_hide_request + 1
    local this_request = osd_hide_request
    process.run("sleep", { OSD_SECONDS }, function() end, function()
        if this_request == osd_hide_request then
            osd_visible:set(false)
        end
    end)
end

return {
    popup_anchor = popup_anchor,
    settings_open = settings_open,
    panel_open = panel_open,
    panel_kind = panel_kind,
    toggle_panel = toggle_panel,
    close_panel = close_panel,
    panel_showing = panel_showing,
    osd_kind = osd_kind,
    osd_visible = osd_visible,
    arm_osd = arm_osd,
    launcher_open = launcher_open,
}
