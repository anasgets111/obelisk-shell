-- Session commands the two compositors spell differently. `systemctl reboot`/`poweroff`/`suspend`
-- are not here: logind answers to both, so they need no branch (ADR-0206).
--
-- Hyprland 0.56 parses its command socket as Lua, so a dispatcher is `hl.dsp.<name>(...)` and the
-- pre-0.56 `dispatch exit` dies in that parser. `dpms` reads `action`, one of `on`/`off`/`toggle`,
-- and toggles when passed no table, so the field is always explicit.
local COMMANDS = {
    niri = {
        logout = { cmd = "niri", args = { "msg", "action", "quit", "--skip-confirmation" } },
        displays_on = { cmd = "niri", args = { "msg", "action", "power-on-monitors" } },
        displays_off = { cmd = "niri", args = { "msg", "action", "power-off-monitors" } },
    },
    hyprland = {
        logout = { cmd = "hyprctl", args = { "dispatch", "hl.dsp.exit()" } },
        displays_on = { cmd = "hyprctl", args = { "dispatch", 'hl.dsp.dpms({ action = "on" })' } },
        displays_off = { cmd = "hyprctl", args = { "dispatch", 'hl.dsp.dpms({ action = "off" })' } },
    },
}

local compositor = {}

--- Detach `verb` for the running compositor.
---
--- `false` means nothing ran: no implementor, or `workspaces` has not answered yet (ADR-0056
--- decision 1 leaves it nil). A caller must not record the verb as done on `false`. `idle.blanked`
--- did, and a no-op then armed lock and suspend with the screen still lit.
--- @param verb "logout"|"displays_on"|"displays_off"
--- @return boolean ran
function compositor.detach(verb)
    local workspaces = obelisk.workspaces:get()
    local name = workspaces and workspaces.compositor
    local command = name and COMMANDS[name] and COMMANDS[name][verb]
    if command == nil then
        print(("obelisk: no `%s` command for compositor %s; nothing ran"):format(verb, tostring(name)))
        return false
    end
    process.detach(command.cmd, command.args)
    return true
end

return compositor
