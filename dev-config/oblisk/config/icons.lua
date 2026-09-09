-- Glyphs the bar draws by name. An `icon` is a themed raster looked up by name (ADR-0054).
-- It paints in its theme artwork's colours. These private-use codepoints use `text`,
-- because `PaintStyle::Icon` has no tint and `layout::paint` applies none.
-- `text` accepts `foreground`, `font_size` and `opacity`, so a state can turn one glyph accent or
-- red. `shell.lua` puts "CaskaydiaCove Nerd Font Propo" first; fallback is per glyph (ADR-0043
-- decision 2), so codepoints choose Nerd Font and Latin text chooses Noto Sans. Without it, these
-- render as tofu.
--
-- `\u{...}` keeps the source ASCII; the codepoint is lookupable at nerdfonts.com/cheat-sheet.
-- Names are this config's; values match `~/.config/quickshell` for the same state.
local icons           = {}

-- Left zone.
icons.power           = "\u{EAD2}"  -- nf-cod-debug_restart_frame, the reference's restart action glyph
icons.logout          = "\u{F0343}"
icons.shutdown        = "\u{23FB}"  -- IEC 5009 power symbol, not a Nerd Font glyph in the mirror either
icons.lock            = "\u{F033E}"
icons.sleep           = "\u{F04B2}" -- nf-md-power_sleep
icons.settings        = "\u{F0493}"
icons.launcher        = "\u{F035C}"
icons.web             = "\u{F059F}" -- nf-md-web, the launcher's open-link/search row
icons.wallpaper       = "\u{F02E9}"

-- Updates, in `ArchChecker.qml` test order.
icons.updating        = "\u{F0996}"
icons.update_err      = "\u{F0159}"
icons.checking        = "\u{F085}"
icons.updates         = "\u{F019}"
icons.up_to_date      = "\u{F00AA}"

-- Battery: `levels` is 1..5, empty to full, matching `BatteryIndicator.qml`'s
-- `icons[min(floor(fraction * 5), 4)]` after Lua's 1-based indexing.
icons.battery_ac      = "\u{F1E6}"
icons.battery_pending = "\u{F0084}"
icons.battery_levels  = { "\u{F244}", "\u{F243}", "\u{F242}", "\u{F241}", "\u{F240}" }

-- OSD glyphs used by themed icons. The OSD tints its icon with the accent; themed icons cannot be
-- tinted.
icons.brightness      = "\u{F00DE}"
icons.keyboard        = "\u{F030C}"
icons.caps_lock       = "\u{F0A9B}"
icons.num_lock        = "\u{F03A0}"
icons.lan             = "\u{F0317}"
icons.lan_off         = "\u{F0318}"
icons.speaker         = "\u{F04C3}"
icons.check           = "\u{F012C}"
icons.mixer           = "\u{F04E1}"
icons.music_note      = "\u{F075A}"
icons.headphones      = "\u{F02CB}"
icons.headset         = "\u{F02CE}"
icons.phone           = "\u{F03F2}"

-- Audio levels, plus the muted toggle glyph.
icons.vol_muted       = "\u{F075F}"
icons.vol_zero        = "\u{F0581}"
icons.vol_low         = "\u{F057F}"
icons.vol_mid         = "\u{F0580}"
icons.vol_high        = "\u{F057E}"

-- Network: `wifi` is indexed 1..4, weakest to strongest, matching `NetworkService.getWifiIcon`.
icons.wifi            = { "\u{F091F}", "\u{F0922}", "\u{F0925}", "\u{F0928}" }
icons.wifi_off        = "\u{F092E}"
icons.wifi_none       = "\u{F092D}"
-- The row that starts a hidden join, the same glyph `NetworkPanel.qml` puts on it. A network that
-- broadcasts no SSID has no scanned row to click, so this stands for the one that is not listed.
icons.wifi_hidden     = "\u{F05AA}"
icons.ethernet        = "\u{F0200}"

icons.bt_off          = "\u{F00B2}"
icons.bt_on           = "\u{F00AF}"
icons.bt_conn         = "\u{F00B1}"

-- Notifications, plus `bell_off` for the clock glyph when there are none.
icons.bell            = "\u{F0A2}"
icons.bell_active     = "\u{F116B}"
icons.bell_off        = "\u{F009B}"

-- Privacy uses Font Awesome, matching `PrivacyIndicator.qml`'s ``, `` and ``.
icons.mic_on          = "\u{F130}"
icons.mic_off         = "\u{F131}"
icons.camera          = "\u{F030}"
icons.screenshare     = "\u{F108}"

icons.play            = "\u{F040A}"
icons.pause           = "\u{F03E4}"
-- `MediaPanel.qml`'s transport row. Its note glyph is F0386, not the F075A `music_note` an audio
-- stream row uses: the panel means "the media player", the stream row means "a sound".
icons.media           = "\u{F0386}"
icons.previous        = "\u{F04AE}"
icons.next            = "\u{F04AD}"
icons.rewind          = "\u{F11F9}"
icons.fast_forward    = "\u{F11F8}"
icons.player_switch   = "\u{F0CB0}"
-- `SystemInfoWidget.qml`'s `MetricTile` icons, and its two are the way round they look: F035B is
-- the square processor with pins, F061A the DIMM stick. This file had them swapped, so the system
-- readout labelled a memory module "CPU".
icons.cpu             = "\u{F035B}"
icons.ram             = "\u{F061A}"
icons.gpu             = "\u{F08AE}"
icons.disk            = "\u{F02CA}"

-- Bluetooth device categories, one per § 2.6 `category`, avoid using the generic glyph for a mouse
-- or headset.
icons.device          = {
    keyboard   = "\u{F030C}",
    mouse      = "\u{F037D}",
    headphones = "\u{F02CB}",
    headset    = "\u{F02CE}",
    phone      = "\u{F011C}",
    computer   = "\u{F0322}",
    generic    = "\u{F00AF}",
}

-- Idle: `idle` means nothing holds the system awake, `awake` is the coffee cup, and the bar swaps
-- them when a manual hold starts. `display` is the monitor DPMS powers down; suspend reuses `sleep`
-- instead of adding the mirror's near-identical second power-sleep glyph.
icons.idle            = "\u{F0FAA}"
icons.awake           = "\u{F0176}"
icons.display         = "\u{F0379}"

icons.refresh         = "\u{F0450}"
icons.clear_all       = "\u{F0234}"
icons.info            = "\u{F02FD}"

-- List-row actions, matching `PanelActionIcon`: delete a saved network or paired device, or cut a
-- live connection.
icons.trash           = "\u{F0A7A}"
icons.disconnect      = "\u{F1616}"

-- Screen recorder. `ScreenRecorder.qml` uses three states on the bar -- idle, recording, paused --
-- and `ScreenRecorderPanel.qml` names the two captures it can start. `record` is the header's
-- glyph; `record_start` is the bar's idle circle, which is the one that says "this button records".
icons.record          = "\u{F044A}"
icons.record_start    = "\u{F07A1}"
icons.record_stop     = "\u{F04DB}"
icons.record_paused   = "\u{F03E7}"
icons.region          = "\u{F019E}"
icons.folder          = "\u{F024B}"
-- The three quality words, ranked by the same needle the mirror uses: a slow gauge for the
-- smallest files, a fast one for the sharpest.
icons.quality_low     = "\u{F0F86}"
icons.quality_medium  = "\u{F0F85}"
icons.quality_high    = "\u{F04C5}"
icons.file_mp4        = "\u{F022B}"
icons.file_mkv        = "\u{F0FCE}"

icons.warning         = "\u{F0026}"
icons.close           = "\u{F0156}"

-- Notification card controls: chevrons expand/collapse a group or message; `reply` opens the inline
-- field and `send` submits its text.
icons.chevron_up      = "\u{F0143}"
icons.chevron_down    = "\u{F0140}"
-- `SystemInfoWidget.qml` and `PanelRow.qml` point right while collapsed rather than up, because
-- the row opens downwards and nothing above it moves.
icons.chevron_right   = "\u{F0142}"
icons.reply           = "\u{F0468}"
icons.send            = "\u{F048A}"
icons.plus            = "\u{F0415}"
icons.minus           = "\u{F0374}"

return icons
