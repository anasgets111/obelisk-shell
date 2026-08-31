-- Mirrors KeyboardLayoutIndicator.qml, which puts the layout's two-letter code in an `IconButton`
-- rather than its full name in a pill. "EN", not "English (US, intl.)".
--
-- That deletes this file's whole width problem. The old version reserved 84px and elided into it,
-- because a layout name is however long the compositor says it is; a code is two characters and
-- fits a circle at any scale.
--
-- Caps lock is the glyph's colour rather than a " CAPS" suffix. Same information, no width, and it
-- reads from across the room, which is the only thing a caps indicator is for. It was the ground
-- until now and a solid peach disc sat on the bar shouting at a state nobody had entered.
local theme = require("config.theme")
local icon_button = require("components.icon_button")

-- First two letters of the layout name, uppercased. `KeyboardLayoutService.layoutShort` does the
-- same thing against the same strings ("English (US)" -> "EN", "Arabic (Egypt)" -> "AR").
--
-- ponytail: this is wrong for any layout whose first two letters are not its short code, which is
-- most non-Latin scripts spelled in their own language. The mirror has the same limit. The upgrade
-- is the XKB layout code, which niri knows and `oblisk.keyboard` does not carry (ADR-0056 lists
-- what the row does carry); adding it there is a capability change, not a config one.
local function layout_short(k)
    local name = (k and k.active_layout) or "?"
    return (name:gsub("[^%a]", ""):sub(1, 2)):upper()
end

local caps = oblisk.keyboard:map(function(k)
    return k ~= nil and k.caps_lock == true
end)

-- No `on_activate`, so this draws as a `row` rather than a `button`. § 3.2's keyboard row is a
-- readout with no command behind it; the mirror calls `KeyboardLayoutService.nextLayout()` here and
-- there is nothing to call.
return icon_button(oblisk.keyboard:map(layout_short), nil, {
    icon_size = theme.font.md,
    foreground = caps:map(function(on)
        return on and theme.PEACH or theme.FG
    end),
})
