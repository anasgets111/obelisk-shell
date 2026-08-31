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
local theme = require("config.theme")
local icons = require("config.icons")
local icon_button = require("components.icon_button")

local SLOT = "updates"

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
end), function()
    -- Straight to the install, because it is one of the two actions `supervisor/src/updates/mod.rs`
    -- dispatches and the other configures the poll interval. There is no `check`, so the idle state
    -- has nothing to do and says so by not firing.
    --
    -- The mirror opens `UpdatePanel.qml` here, which lists the packages and asks first. That wants a
    -- panel this config does not have; until it does, an unconfirmed system upgrade behind one click
    -- is the wrong default, so the guard below is deliberate rather than a leftover.
    if (oblisk.updates:get() or {}).count == 0 then
        return
    end
    oblisk.updates:invoke("install")
end, {
    slot = SLOT,
    visible = status:map(function(s)
        return s ~= "idle"
    end),
    foreground = status:map(function(s)
        return s == "error" and theme.RED or theme.ACCENT
    end),
})
