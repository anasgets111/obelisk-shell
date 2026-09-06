-- Mirrors KeyboardLayoutIndicator.qml: the layout's two-letter code in an `IconButton`, "EN" rather
-- than "English (US, intl.)".
--
-- The old version reserved 84px and elided the compositor-provided layout name. Two characters fit
-- a circle at any scale.
--
-- Caps lock changes the glyph colour instead of adding a " CAPS" suffix. It was previously a
-- ground change, so a solid peach disc appeared for a state nobody had entered.
local theme = require("config.theme")
local icon_button = require("components.icon_button")

-- First two letters of the layout name, uppercased. `KeyboardLayoutService.layoutShort` does the
-- same for "English (US)" -> "EN" and "Arabic (Egypt)" -> "AR".
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

-- No `on_activate`, so `icon_button` draws a `row` rather than a `button`: § 3.2's keyboard row is
-- a readout with no command. The mirror calls
-- `KeyboardLayoutService.nextLayout()`, but there is no corresponding capability action.
return icon_button(oblisk.keyboard:map(layout_short), nil, {
    icon_size = theme.font.md,
    foreground = caps:map(function(on)
        return on and theme.PEACH or theme.FG
    end),
})
