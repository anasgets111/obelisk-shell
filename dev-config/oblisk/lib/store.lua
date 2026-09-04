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
        -- `Settings.data.idleService`, one level flatter. Two profiles keyed by what UPower says
        -- about the mains, each holding three stages as an `<stage>_on`/`<stage>_sec` pair.
        --
        -- `enabled` ships false, and that is deliberate rather than a copy of the mirror, which
        -- ships true. These defaults are written into a real `state.json` on somebody's real
        -- machine the first time this config runs, and the first thing a `true` here would do is
        -- blank their screen while they were reading. The panel's master switch is one click and
        -- says "automation paused" until it is thrown.
        --
        -- Not carried over: the mirror's `lockAfterDpms`. It needs an order because its stages are
        -- three independent monitors that each wait on the others; `modules/global/idle.lua` runs
        -- one clock, so "lock at 900, blank at 300" already means blanking happens first and the
        -- two numbers are the whole answer.
        idle = {
            enabled = false,
            video_auto_inhibit = true,
            ac = { dpms_on = true, dpms_sec = 300, lock_on = true, lock_sec = 900, suspend_on = false, suspend_sec = 1800 },
            battery = { dpms_on = true, dpms_sec = 120, lock_on = true, lock_sec = 300, suspend_on = true, suspend_sec = 900 },
        },
    },
}
