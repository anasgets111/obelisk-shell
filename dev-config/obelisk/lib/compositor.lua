-- Session commands the two compositors spell differently. `systemctl reboot`/`poweroff`/`suspend`
-- are not here: logind answers to both.
--
-- Branching belongs in config, not the Supervisor: ADR-0056 decision 1 refused a second compositor
-- trait, and ADR-0119 decision 3 publishes `workspaces.compositor` so Lua can choose policy.
--
-- Hyprland 0.56's command socket is Lua, so a dispatcher is `hl.dsp.<name>(...)`; the pre-0.56
-- `dispatch exit` reaches that parser and dies. `dpms` takes `action`, one of `on`/`off`/`toggle`,
-- and defaults to `toggle` when passed no table.
local COMMANDS = {
    niri = {
        logout = { "niri", { "msg", "action", "quit", "--skip-confirmation" } },
        displays_on = { "niri", { "msg", "action", "power-on-monitors" } },
        displays_off = { "niri", { "msg", "action", "power-off-monitors" } },
    },
    hyprland = {
        logout = { "hyprctl", { "dispatch", "hl.dsp.exit()" } },
        displays_on = { "hyprctl", { "dispatch", 'hl.dsp.dpms({ action = "on" })' } },
        displays_off = { "hyprctl", { "dispatch", 'hl.dsp.dpms({ action = "off" })' } },
    },
}

local compositor = {}

--- Detach `verb` for the running compositor. A session with neither implementor reads `nil` and
--- runs nothing, the same degradation the capability makes (ADR-0119 decision 1).
--- @param verb "logout"|"displays_on"|"displays_off"
--- @return boolean ran
function compositor.detach(verb)
    local w = obelisk.workspaces:get()
    local command = w and COMMANDS[w.compositor] and COMMANDS[w.compositor][verb]
    if command == nil then
        return false
    end
    process.detach(command[1], command[2])
    return true
end

return compositor
