-- Your shell. Everything on screen is declared here or in a file this requires.
--
-- `oblisk.screens` is the one signal with a value at first evaluation, so a bar per output is a
-- loop over it. Every other capability reads `nil` until its first push, which is why `content`
-- takes a signal rather than a string here -- and why the closure below reads `s and s.time`
-- rather than `s.time`. That first evaluation runs against a nil `s`, and indexing it there fails
-- the whole apply: the bar binds, paints nothing, and prints a Lua error, until the next push a
-- second later resolves the scene for real. `os.date` with a nil time is now, so the guard makes
-- that first frame right instead of missing.
fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
}

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true,
        width = "Fill",
        height = 34,
        background = "#1e1e2e80",
        child = row {
            width = "Fill",
            height = "Fill",
            align_h = "End",
            align_v = "Center",
            padding = { left = 12, right = 12 },
            children = {
                text {
                    content = oblisk.system:map(function(s)
                        return os.date("%H:%M", s and s.time)
                    end),
                    font_size = 13,
                    foreground = "#cdd6f4ff",
                },
            },
        },
    },
}
