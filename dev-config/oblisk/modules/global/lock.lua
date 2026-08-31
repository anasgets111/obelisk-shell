-- Mirrors Global/LockScreen.qml.
--
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- § 6.4 routes authentication through a `textfield` with `secure_submit`, and that pair is what
-- keeps the password out of this VM entirely: with both `mask_character` and `secure_submit` set,
-- keystrokes go into a native buffer on the Renderer's Wayland thread and leave as a
-- `("lock", "authenticate")` envelope, never as a Lua value (§ 5.2 item 8, ADR-0005/ADR-0027). So
-- there is deliberately no `on_change`/`on_submit` here: one would be the exact hole the design
-- exists to close.
--
-- It is the only `secure_submit` field on this surface, and that is load-bearing: the engine
-- focuses a surface's sole `secure_submit` field the moment the compositor gives that surface
-- keyboard focus, so this is typable with no click. A second such field would put the lock screen
-- back to needing a mouse, because with two destinations the engine refuses to guess.
--
-- `mask_character` is drawn, one glyph per keystroke, so typing is visible. It was not always:
-- this field reserved 28px and swallowed keystrokes showing nothing, and typing a password blind
-- is worse than it sounds. `pam_unix` answers a wrong password with a two second delay and
-- `pam_faillock` locks the account after three, so an invisible typo looked exactly like a slow
-- unlock and three of them cost ten minutes. `lock_status` below still reports what the
-- capability says; this reports what you typed.
local password_field = textfield {
    width = "Fill",
    height = 28,
    placeholder = "password",
    mask_character = "*",
    secure_submit = { capability = "lock", action = "authenticate" },
}

-- Off `oblisk.lock` rather than off `rescue`, and the split is ADR-0052 decision 4: with the lock
-- surfaces mapped the compositor shows only these, so the bar's `rescue_cell` is unreachable and the
-- capability's own state is the only channel left. `attempts` is printed because a config cannot
-- rebuild it: capability state is sampled at layout time (ADR-0044), so two identical failures in a
-- row are one unchanged `error` string and a counter written here would miss the second.
local lock_status = cell(util.label(oblisk.lock, function(l)
    if l.error == nil or l.error == "" then
        return l.active and "type your password, then Enter" or "locking..."
    end
    return string.format("%s (%d)", l.error, l.attempts or 0)
end), theme.RED)

-- The lock screen gets the clock too, because every lock screen has one and because it is the
-- cheapest possible proof that `system` keeps pushing while the session is locked.
local lock_clock = cell(util.label(oblisk.system, function(s)
    return os.date("%H:%M", s.time)
end), theme.FG, 48)

-- Declared, not open. § 6.4 gives a `lock` an `id` and a `child` and nothing else: no `visible`,
-- no `monitor`, no size, because the compositor decides when these surfaces exist and the
-- protocol requires one on every output while they do. Returning this costs one retained node
-- and zero Wayland objects until `oblisk.lock:invoke("lock")` is clicked, the same
-- declaration/lifetime split ADR-0049 made for `window` and `popup`.
return lock {
    id = "lock_screen",
    child = column {
        -- Opaque and full-bleed: this is what covers the session, so a `Content`-sized child
        -- would leave the desktop showing through everything it did not paint.
        width = "Fill",
        height = "Fill",
        background = "#11111bff",
        align_h = "Center",
        align_v = "Center",
        spacing = 18,
        children = {
            lock_clock,
            column {
                width = 380,
                padding = { top = 20, right = 20, bottom = 20, left = 20 },
                spacing = 10,
                background = theme.BG,
                radius = 12,
                border_width = 1,
                border_color = theme.SURFACE,
                children = { cell("locked", theme.FG), password_field, lock_status },
            },
            cell(util.label(oblisk.battery, function(b)
                if not b.present then
                    return ""
                end
                return string.format("battery %d%%%s", b.percent, b.charging and " charging" or "")
            end), theme.DIM, 11),
        },
    },
}
