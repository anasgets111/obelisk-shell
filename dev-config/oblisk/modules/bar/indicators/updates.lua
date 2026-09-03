-- Mirrors ArchChecker.qml: one glyph whose shape says what the updater is doing and whose ground
-- says whether it wants attention.
--
-- Four states, tested in the mirror's own order, because they overlap: an error that happened
-- during a check still has a stale count sitting behind it. First match wins and the order is what
-- makes that right.
--
-- The mirror has a fifth, "checking", and this does not: `UpdatesState` in
-- `supervisor/src/updates/controller.rs` carries no in-flight flag, so a check that is running and
-- one that has not started look identical from here.
--
-- The mirror spins the glyph while installing. Nothing here animates, so the state is carried by
-- the glyph's colour alone; a rotation would need a per-frame property and there is no timer under
-- ADR-0021's 5ms cap that would drive one.
--
-- Absent when there is nothing to say, which the mirror is not. A permanent circle whose one
-- meaning is "no action available" is a control that never does anything, and it had the guard
-- below to prove it: the idle click was already a no-op. Now it is a no-op with no pixels.
--
-- The click opens the panel and nothing else. It briefly invoked `install` directly, and the day the
-- module was first switched on, one click launched a real `pkexec pacman -Syu` -- it got no further
-- than "Error creating textual authentication agent", so nothing was upgraded, but nothing about
-- that was by design either. `ArchChecker.qml` never installs from the bar for the same reason:
-- installing is a decision made in front of the package list, which is what the panel is.
--
-- The mirror also re-checks on a click when nothing is pending. That needs the button to be there
-- when nothing is pending, and this one is not (see above), so the re-check lives in the panel
-- header where there is room to say what it does.
local theme = require("config.theme")
local icons = require("config.icons")
local icon_button = require("components.icon_button")
local ui_state = require("lib.ui_state")
local update_panel = require("modules.bar.panels.update_panel")

-- Nothing checks for updates until a config names an interval (ADR-0034), so without this line the
-- capability starts, stays dormant, and the indicator below is invisible forever -- `state_of` reads
-- `idle` off a state nothing ever wrote, and `visible` hides `idle`. Here rather than in `shell.lua`
-- for `system_info.lua`'s reason: the module that wants the answer says how often.
--
-- An hour, which is the cadence a pending-updates badge is read at, not the cadence Arch moves at. A
-- check is a real `-Sy` against a mirror; anything much shorter is bandwidth spent on a number that
-- changes a few times a day. The first one runs at once rather than an hour in, and a config reload
-- inside the hour does not re-run it (ADR-0113 amendment).
oblisk.updates:invoke("configure", { interval = 3600 })

local function state_of(u)
    if u == nil then
        return "idle"
    end
    if u.installing then
        return "installing"
    end
    if u.check_error and u.check_error ~= "" then
        return "error"
    end
    if (u.count or 0) > 0 then
        return "pending"
    end
    return "idle"
end

local status = oblisk.updates:map(state_of)

return icon_button(status:map(function(s)
    if s == "installing" then
        return icons.updating
    elseif s == "error" then
        return icons.update_err
    elseif s == "pending" then
        return icons.updates
    end
    return icons.up_to_date
end), function(rect)
    ui_state.toggle_panel(update_panel.kind, rect)
end, {
    slot = "updates",
    -- The accent ring every other indicator wears while its own panel is the one on screen.
    selected = ui_state.panel_showing(update_panel.kind),
    visible = status:map(function(s)
        return s ~= "idle"
    end),
    foreground = status:map(function(s)
        return s == "error" and theme.RED or theme.ACCENT
    end),
})
