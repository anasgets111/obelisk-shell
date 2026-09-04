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
        -- `Settings.data.idleService`, one level flatter, and on a different model. Two profiles
        -- keyed by what UPower says about the mains, each holding a per-stage `<stage>_on` switch
        -- and a `<stage>_sec` delay, plus one `order` both profiles share.
        --
        -- A stage's seconds are counted from when the stage above it fired, not from when the seat
        -- went idle. "Blank after 5 minutes, then lock 10 minutes after that" is how anyone
        -- describes this out loud, and it is the arrangement that survives editing: with absolute
        -- times, lowering the blank timeout silently shortens the gap before the lock.
        --
        -- `order` is one list rather than one per profile. The order is a policy -- blank before
        -- locking, or lock before blanking -- and it does not change because a cable came out; the
        -- delays are what change, and those are per profile. It is validated on read, so an entry
        -- hand-edited to a name that is not a stage is dropped and a stage missing from it is
        -- appended rather than silently never running.
        --
        -- `enabled` ships false, deliberately, where the mirror ships true. These defaults are
        -- written into a real `state.json` on somebody's real machine the first time this config
        -- runs, and the first thing a `true` here would do is blank their screen while they were
        -- reading. The modal's master switch is one click and says "automation paused" until it is
        -- thrown.
        idle = {
            enabled = false,
            video_auto_inhibit = true,
            order = { "dpms", "lock", "suspend" },
            ac = { dpms_on = true, dpms_sec = 300, lock_on = true, lock_sec = 600, suspend_on = false, suspend_sec = 1800 },
            battery = { dpms_on = true, dpms_sec = 120, lock_on = true, lock_sec = 180, suspend_on = true, suspend_sec = 600 },
        },
    },
}
