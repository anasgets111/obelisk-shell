-- The glyphs the bar draws itself with, by name.
--
-- These are Nerd Font codepoints in the private use area, rendered as `text` rather than as `icon`
-- nodes, and that is the whole reason the bar can be one colour per state. An `icon` is a themed
-- raster looked up by name (docs/adr/0054) and paints in whatever colours the theme's artwork has;
-- `PaintStyle::Icon` carries no tint and `layout::paint` never applies one. A glyph is a `text`
-- node, so it takes `foreground`, `font_size` and `opacity` like any other string and a caller can
-- turn it accent-coloured on connect or red on error with one property.
--
-- They resolve because `shell.lua` declares "CaskaydiaCove Nerd Font Propo" first in the font chain
-- and the chain falls back per glyph (docs/adr/0043 decision 2): a codepoint in this file picks the
-- Nerd Font, and the Latin text beside it picks Noto Sans, with no node saying which face it wants.
-- Without that declaration every one of these is tofu.
--
-- Written as `\u{...}` escapes rather than pasted literals so the source stays ASCII and the
-- codepoint is the thing a reader can look up (nerdfonts.com/cheat-sheet). The name on the left is
-- this config's, the codepoint on the right is the mirror's: every value here is the one
-- `~/.config/quickshell` already uses for the same state, so the two bars read the same.
local icons = {}

-- Left zone.
icons.power       = "\u{EAD2}"  -- nf-cod-debug_restart_frame, the reference's restart action glyph
icons.logout      = "\u{F0343}"
icons.shutdown    = "\u{23FB}"  -- IEC 5009 power symbol, not a Nerd Font glyph in the mirror either
icons.lock        = "\u{F033E}"
icons.settings    = "\u{F0493}"
icons.launcher    = "\u{F035C}"
icons.wallpaper   = "\u{F02E9}"

-- Updates, in the order `ArchChecker.qml` tests them.
icons.updating    = "\u{F0996}"
icons.update_err  = "\u{F0159}"
icons.checking    = "\u{F085}"
icons.updates     = "\u{F019}"
icons.up_to_date  = "\u{F00AA}"

-- Battery. `levels` is indexed 1..5 from empty to full, which is `BatteryIndicator.qml`'s own
-- `icons[min(floor(fraction * 5), 4)]` with Lua's 1-based indexing folded in.
icons.battery_ac      = "\u{F1E6}"
icons.battery_pending = "\u{F0084}"
icons.battery_levels  = { "\u{F244}", "\u{F243}", "\u{F242}", "\u{F241}", "\u{F240}" }

-- Audio, by loudness, plus the muted glyph the mute toggle swaps in.
icons.vol_muted   = "\u{F075F}"
icons.vol_zero    = "\u{F0581}"
icons.vol_low     = "\u{F057F}"
icons.vol_mid     = "\u{F0580}"
icons.vol_high    = "\u{F057E}"

-- Network. `wifi` is indexed 1..4 weakest to strongest, matching `NetworkService.getWifiIcon`.
icons.wifi        = { "\u{F091F}", "\u{F0922}", "\u{F0925}", "\u{F0928}" }
icons.wifi_off    = "\u{F092E}"
icons.wifi_none   = "\u{F092D}"
icons.ethernet    = "\u{F0200}"

-- Bluetooth: off, on, connected.
icons.bt_off      = "\u{F00B2}"
icons.bt_on       = "\u{F00AF}"
icons.bt_conn     = "\u{F00B1}"

-- Notifications, and the clock's own glyph when there are none.
icons.bell        = "\u{F0A2}"
icons.bell_active = "\u{F116B}"
icons.bell_off    = "\u{F009B}"

-- Privacy. Font Awesome rather than Material here, which is the mirror's own choice: these three
-- are the glyphs `PrivacyIndicator.qml` names as ``, `` and ``.
icons.mic_on      = "\u{F130}"
icons.mic_off     = "\u{F131}"
icons.camera      = "\u{F030}"
icons.screenshare = "\u{F108}"

-- Media transport, and the readouts in the system-info widget.
icons.play        = "\u{F040A}"
icons.pause       = "\u{F03E4}"
icons.cpu         = "\u{F061A}"
icons.ram         = "\u{F035B}"
icons.disk        = "\u{F02CA}"

-- Bluetooth device categories, one per § 2.6 `category`, so a mouse and a headset do not both draw
-- the generic bluetooth glyph.
icons.device = {
    keyboard    = "\u{F030C}",
    mouse       = "\u{F037D}",
    headphones  = "\u{F02CB}",
    headset     = "\u{F02CE}",
    phone       = "\u{F011C}",
    computer    = "\u{F0322}",
    generic     = "\u{F00AF}",
}

icons.refresh     = "\u{F0450}"
icons.clear_all   = "\u{F0234}"
icons.info        = "\u{F02FD}"

-- Alerts.
icons.warning     = "\u{F0026}"
icons.close       = "\u{F0156}"
icons.plus        = "\u{F0415}"
icons.minus       = "\u{F0374}"

return icons
