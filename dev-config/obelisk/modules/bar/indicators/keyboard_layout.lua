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
-- is the XKB layout code, which niri knows and `obelisk.keyboard` does not carry (ADR-0056 lists
-- what the row does carry); adding it there is a capability change, not a config one.
local function layout_short(k)
    local name = (k and k.active_layout) or "?"
    return (name:gsub("[^%a]", ""):sub(1, 2)):upper()
end

local caps = obelisk.keyboard:map(function(k)
    return k ~= nil and k.caps_lock == true
end)

-- No `on_activate`, so `icon_button` returns a readout `row`, not a `button`; this indicator just
-- doesn't call `SwitchLayout`, the capability action behind the mirror's
-- `KeyboardLayoutService.nextLayout()`.
return icon_button(obelisk.keyboard:map(layout_short), nil, {
    icon_size = theme.font.md,
    foreground = caps:map(function(on)
        return on and theme.PEACH or theme.FG
    end),
    -- `visible: KeyboardLayoutService.hasMultipleLayouts`. One configured layout has nothing to
    -- switch to and nothing to disambiguate, so the code is noise; `layout_count` says so directly.
    visible = obelisk.keyboard:map(function(k)
        -- `or 0` because a payload that predates the compositor's answer carries no count at all,
        -- and `nil >= 2` raises rather than reading as false. `caps` above guards the same way.
        return k ~= nil and (k.layout_count or 0) >= 2
    end),
})
