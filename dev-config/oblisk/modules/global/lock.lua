-- Mirrors Global/LockScreen.qml.
--
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- § 6 routes authentication through a `textfield` with `secure_submit`. With `mask_character` too,
-- keystrokes stay in a native buffer on the Renderer's Wayland thread and leave as a
-- `("lock", "authenticate")` envelope, never a Lua value (§ 5.2 item 8, ADR-0005/ADR-0027). No
-- `on_change`/`on_submit`: either callback would reopen the closed path.
--
-- It is the surface's only `secure_submit` field. The engine focuses the sole field on compositor
-- keyboard focus, so no click is needed; with two fields it refuses to guess and needs a mouse.
--
-- `mask_character` draws one glyph per keystroke. The old field reserved 28px and showed nothing;
-- blind typing made `pam_unix`'s two-second wrong-password delay indistinguishable from a slow
-- unlock, and `pam_faillock` locked the account after three, costing ten minutes. `lock_status`
-- reports capability state; this reports what was typed.
local password_field = textfield {
    width = "Fill",
    height = theme.control.md,
    placeholder = "password",
    mask_character = "*",
    secure_submit = { capability = "lock", action = "authenticate" },
}

-- Read from `oblisk.lock`, not `rescue`, per ADR-0052 decision 4: while lock surfaces are mapped
-- the
-- bar's `rescue_cell` is unreachable. Print `attempts` because capability state is sampled at
-- layout
-- time (ADR-0044); identical consecutive `error` strings would otherwise hide the second failure.
local lock_status = cell(util.label(oblisk.lock, function(l)
    if l.error == nil or l.error == "" then
        return l.active and "type your password, then Enter" or "locking..."
    end
    return string.format("%s (%d)", l.error, l.attempts or 0)
-- Wrap: the card is 380px and an unavailable-authentication line does not fit on one. Clipped, it
-- read "could not start authentication: pam worker f" and the user had to guess the rest.
end), theme.RED, nil, { width = "Fill", wrap = "Word", max_lines = 4 })

-- Include the clock, both because lock screens have one and because it proves `system` still pushes
-- while locked.
local lock_clock = cell(util.label(oblisk.system, function(s)
    return os.date("%H:%M", s.time)
end), theme.FG, theme.font.hero)

-- Declared, not open. § 6 gives `lock` only `id` and `child`: the compositor creates one per output
-- while locked, with no `visible`, monitor, or size. This retains one node and zero Wayland objects
-- until `oblisk.lock:invoke("lock")`, the declaration/lifetime split of ADR-0049.
return lock {
    id = "lock_screen",
    child = column {
        -- Opaque and full-bleed: a `Content`-sized child would leave unpainted desktop visible.
        width = "Fill",
        height = "Fill",
        background = "#11111bff",
        align_h = "Center",
        align_v = "Center",
        spacing = theme.spacing.lg,
        children = {
            lock_clock,
            column {
                width = theme.s(380, 300),
                padding = {
                    top = theme.spacing.xl,
                    right = theme.spacing.xl,
                    bottom = theme.spacing.xl,
                    left = theme.spacing.xl,
                },
                spacing = theme.spacing.md,
                background = theme.BG,
                radius = theme.radius.md,
                border_width = theme.border_width,
                border_color = theme.BORDER,
                children = { cell("locked", theme.FG), password_field, lock_status },
            },
            cell(util.label(oblisk.battery, function(b)
                if not b.present then
                    return ""
                end
                return string.format("battery %d%% %s", b.percent, util.battery_phrase(b.state))
            end), theme.DIM, theme.font.xs),
        },
    },
}
