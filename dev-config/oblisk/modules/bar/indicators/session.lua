-- The closest Quickshell counterpart is Bar/Panels/PowerMenu.qml, which is a whole panel of
-- session actions. This is one button, because locking is the only session command a capability
-- exposes today (ADR-0052 decision 1).

local theme = require("config.theme")
local cell = require("components.cell")

-- A bar button is a strange place to arm a lock and it is the only place available: ADR-0052
-- decision 1 makes locking an ordinary capability command, and an input callback is the only thing
-- that can issue one today. `oblisk.idle` cannot, because a `SupervisorFrame::IdleEvent` reaches the
-- Renderer and stops there, so the idle threshold a real config would lock on has nowhere to land.
--
-- `invoke` is the one generic write path (Phase 25 item 1): it builds § 7.2's envelope from the
-- capability, action and arguments and knows nothing about locking. The day `sysinfo:configure` and
-- `audio:set_volume` land they land on this same call, not on twenty-nine more bindings.
local lock_button = button {
    width = 46,
    height = 24,
    background = theme.SURFACE,
    radius = 6,
    -- The one handler in this tree where the widened button set is not cosmetic: a right-click
    -- landing here would take over the session (docs/adr/0050's second amendment names this).
    on_click = function(_, button)
        if button ~= "left" then
            return
        end
        oblisk.lock:invoke("lock")
    end,
    children = { cell("lock", theme.MAUVE) },
}

return lock_button
