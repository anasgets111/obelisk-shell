-- Where this config keeps what has to survive a restart. `Config/Settings.qml`'s pair of
-- `FileView`s, as one file.
--
-- The path is this file's decision and nothing else's (ADR-0136): `persistent_table` has no
-- default location, so a config that wants `$XDG_CACHE_HOME`, a file beside `shell.lua`, or a
-- settings file separate from a cache writes that here. This one keeps the location Oblisk used to
-- hardcode, so an existing `state.json` is still read.
--
-- One file rather than the mirror's two. The mirror splits settings from cache because its settings
-- are a `JsonAdapter` a GUI writes; here the settings are `config/theme.lua` and the rest of this
-- directory, which `require` hot-reloads (ADR-0047), so the only thing left to persist is what the
-- user changed at runtime.
local home = os.getenv("HOME") or ""
local state_home = os.getenv("XDG_STATE_HOME")
if not state_home or state_home == "" then
    state_home = home .. "/.local/state"
end

-- `defaults` seeds keys the file does not have and never overwrites one it does, so this list can
-- grow without resetting anybody's shell. It is also what creates the file on a first run.
return persistent_table {
    path = state_home .. "/oblisk",
    name = "state.json",
    defaults = {
        -- One entry per output, `{ path = ..., fit = ... }`. `Settings.data.wallpapers` verbatim.
        wallpapers = {},
        -- When the last update check that succeeded finished, and the package names already
        -- announced (`UpdateService.qml`'s `lastSuccessfulCheck` and `notifiedPackagesKey`).
        updates_checked_at = 0,
        updates_notified = "",
    },
}
