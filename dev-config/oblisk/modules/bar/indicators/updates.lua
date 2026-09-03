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
-- A readout, not a button, which is `nil` where `icon_button` takes an `on_activate` (that returns a
-- `row` rather than a `button`, so there is no click to land). This *was* a button that invoked
-- `install` -- and the day the module was first switched on, one click on it launched a real
-- `pkexec pacman -Syu`. It got no further than "Error creating textual authentication agent", so
-- nothing was upgraded, but nothing about that was by design.
--
-- `ArchChecker.qml` never installs from the bar. A left click with nothing pending re-checks; a left
-- click with something pending, or a right click, opens `UpdatePanel.qml` -- the package list, the
-- download size, the last-check time, and an "Update" button under all of it. Installing is a
-- decision made in front of the list, which is the whole reason the panel exists.
--
-- Neither half is reachable from here yet. There is no `check` action to re-poll with (the
-- capability has `configure` and `install`, ADR-0034), and no panel to open. Until the panel exists,
-- a badge that says how many is the honest amount of this module.
local theme = require("config.theme")
local icons = require("config.icons")
local icon_button = require("components.icon_button")

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
end), nil, {
    -- No `slot`, and so no hover ground: `icon_button` lights one up from the same option, and a
    -- readout that brightens under the pointer and then does nothing is a button that is lying.
    visible = status:map(function(s)
        return s ~= "idle"
    end),
    foreground = status:map(function(s)
        return s == "error" and theme.RED or theme.ACCENT
    end),
})
