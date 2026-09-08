-- Restart-persistent config, replacing `Config/Settings.qml`'s two `FileView`s with one file.
-- This file owns the path (ADR-0136): `persistent_table` has no default. Keep Oblisk's former
-- hardcoded location so existing `state.json` is still read; other configs can choose
-- `$XDG_CACHE_HOME`,
-- beside `shell.lua`, or a separate settings file here.
-- One file unlike the mirror's two: its GUI writes a `JsonAdapter`, while `config/theme.lua` and
-- this directory hot-reload via `require` (ADR-0047). Only runtime user changes need persistence.
local home = os.getenv("HOME") or ""
local state_home = os.getenv("XDG_STATE_HOME")
if not state_home or state_home == "" then
    state_home = home .. "/.local/state"
end

-- `defaults` fills missing keys without resetting existing shells and creates the file on first
-- run.
return persistent_table {
    path = state_home .. "/oblisk",
    name = "state.json",
    defaults = {
        -- One entry per output, `{ path = ..., fit = ... }`. `Settings.data.wallpapers` verbatim.
        wallpapers = {},
        -- Last successful check, its package list and already-announced package names
        -- (`UpdateService.qml`'s `lastSuccessfulCheck`, `packages` and `notifiedPackagesKey`). The
        -- list travels with the time: a restart inside the interval skips its check, and without
        -- the list it would say "up to date" for the rest of the hour.
        updates_checked_at = 0,
        updates_packages = {},
        updates_notified = "",
        -- `Settings.data.idleService`, flattened and modelled as two profiles keyed by UPower mains
        -- state. Each has per-stage `<stage>_on`/`<stage>_sec` fields and both share `order`.
        -- Stage seconds start when the preceding stage fires, not when the seat goes idle. This
        -- preserves "blank after 5 minutes, then lock 10 minutes after that" when editing; absolute
        -- times silently shorten the lock gap when the blank timeout is lowered.
        -- One `order` list is policy, shared across profiles: blank before lock or vice versa, not
        -- something a cable change alters. Delays are per profile. Read validation drops unknown
        -- names and appends missing stages instead of silently skipping them.
        -- `enabled` deliberately ships false while the mirror ships true. Defaults enter a real
        -- `state.json` on first run, and true could blank a screen while its owner reads. The
        -- modal's
        -- master switch is one click and says "automation paused" until enabled.
        -- `Settings.data.screenRecorder` verbatim: the four choices `ScreenRecorderPanel.qml`
        -- offers. Kept as one table rather than four keys because the panel reads and writes them
        -- as a group, and because they belong to one subject.
        screen_recorder = { audio = "desktop", quality = "high", fps = 60, container = "mp4" },
        idle = {
            enabled = false,
            video_auto_inhibit = true,
            order = { "dpms", "lock", "suspend" },
            ac = { dpms_on = true, dpms_sec = 300, lock_on = true, lock_sec = 600, suspend_on = false, suspend_sec = 1800 },
            battery = { dpms_on = true, dpms_sec = 120, lock_on = true, lock_sec = 180, suspend_on = true, suspend_sec = 600 },
        },
    },
}
