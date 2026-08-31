-- The anchor rect the `popup` hangs from. ADR-0049's amendment settles where it comes from: not off
-- the input-dispatch stack, but through the config, because `on_click` receives the button's own
-- rect (ADR-0050 decision 3) and writes it to a named `state` signal the popup reads back. The
-- initial is the button's declared size, because `anchor_rect` must be non-zero before anything has
-- ever been clicked or the whole evaluation fails (§ 6.3).
local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 70, height = 24 })
local settings_open = state("settings_open", false)

-- `modules/shell/panel_host.lua`'s two signals: whether the shared popup is up, and which panel it
-- is showing. One surface for every bar panel rather than one surface each, which is what
-- Quickshell's own Modules/Shell/PanelHost.qml is for -- an `xdg_popup` costs a Wayland object and
-- a grab, and only one of these can be on screen at a time anyway.
local panel_open = state("panel_open", false)
local panel_kind = state("panel_kind", "")

-- Opening sets, it never toggles, and the reason is measured rather than assumed. Under an
-- `xdg_popup` grab, niri still delivers a click on the opening button to us, because the bar is the
-- popup's own parent surface and so inside the grab's tree, so a toggle would close it. What a
-- toggle would also do is fight `on_dismiss` on every click landing elsewhere, since that path
-- already writes false. One writer per edge.
--
-- That leaves one case with no clean answer here: clicking the network indicator while the
-- bluetooth panel is up is both edges at once. The compositor breaks the grab and delivers the
-- click, and `on_dismiss` carries no token saying which popup it dismissed
-- (`xdg_shell.rs` calls it with no arguments), so this cannot tell "the popup I am replacing" from
-- "the popup I just opened". Set-then-close and close-then-set are both possible orders, so the
-- observable cost is that switching panels directly sometimes takes a second click. Never a wrong
-- panel, and never a popup stuck open, which is what picking the unconditional close buys.
local function open_panel(kind, rect)
    popup_anchor:set(rect)
    panel_kind:set(kind)
    panel_open:set(true)
end

local function close_panel()
    panel_open:set(false)
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
    open_panel = open_panel,
    close_panel = close_panel,
    osd_kind = osd_kind,
    osd_visible = osd_visible,
    arm_osd = arm_osd,
    launcher_open = launcher_open,
}
