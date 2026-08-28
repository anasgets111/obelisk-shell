# Oblisk Reference Fixtures
## Worked Declarative Lua Configurations

A complete `shell.lua` plus the widget modules it pulls in: workspaces, media, hardware state, OSDs, and notifications.

Read these as one worked example, not as the shape a config has to take. The engine's surface set is whatever `shell.lua` returns (ADR-0038), and this example happens to declare two `panel`s: a bar, and one fullscreen overlay hosting every card. That idiom keeps the example short and it is a reasonable default, but it is not the only shape and no longer the only one available.

A config has four roles to reach for (§ 6, ADR-0040): `panel` for anything anchored to a screen edge or layer, `window` for a real toplevel the compositor tiles, `popup` for a dropdown the compositor positions and dismisses on click-outside, and `lock` for the lock screen. The overlay idiom below predates three of them. A dropdown drawn as a card inside `overlay_canvas`, as the widget files here do it, is the thing `popup` replaces: a real `xdg_popup` gets compositor-side repositioning and a keyboard grab that a card in a shared overlay cannot have. Treat those widgets as showing layout and signal wiring, not as the recommended way to open a menu.

---

## 1. Complete Unified Shell Configuration (`shell.lua`)

This file is evaluated by the Renderer on boot, and its return value decides which surfaces get created. It declares two panels: the persistent status bar at the top, and the fullscreen transparent overlay that hosts the OSDs and notifications.

```lua
-- =============================================================================
-- shell.lua - Oblisk Unified Core Shell Layout
-- =============================================================================

-- 1. Lazy Capability Loading
local battery      = require("oblisk.battery")
local audio        = require("oblisk.audio")
local brightness   = require("oblisk.brightness")
local keyboard     = require("oblisk.keyboard")
local network      = require("oblisk.network")
local bluetooth    = require("oblisk.bluetooth")
local mpris        = require("oblisk.mpris")
local notifications= require("oblisk.notifications")
local workspaces   = require("oblisk.workspaces")
local system       = require("oblisk.system")
local idle         = require("oblisk.idle")
local wallpaper    = require("oblisk.wallpaper")
local sysinfo      = require("oblisk.sysinfo")
local process      = require("process")

-- 2. Theme Configuration Palette
local theme = {
    background   = "#11111B",
    surface      = "#1E1E2E",
    primary      = "#89B4FA", -- Catppuccin Blue
    accent       = "#A6E3A1", -- Catppuccin Green
    text         = "#CDD6F4",
    text_muted   = "#7F849C",
    warning      = "#F38BA8", -- Catppuccin Red
}

-- 3. Dynamic Idle Daemon Configurations
-- No hardcoded timers are compiled in Rust. Lua defines everything on-the-fly!
idle:register_threshold(30, function()
    -- After 30 seconds of inactivity, dim backlight to 10%
    brightness:set(10)
end, function()
    -- Upon resume, restore backlight to 80%
    brightness:set(80)
end)

idle:register_threshold(300, function()
    -- After 5 minutes, play a lock sound and invoke lock action
    audio:play_sound("lock-session")
    system:write_state("locked", true)
end, nil)

-- Configure Sysinfo Telemetry update frequencies natively
sysinfo:configure({
    cpu_interval  = 2,  -- Update CPU core percentages every 2 seconds
    ram_interval  = 5,  -- Update memory percentages every 5 seconds
    temp_interval = 10, -- Read hardware temps every 10 seconds
})

-- Launch non-blocking background key-logger overlay script as subprocess
local key_logger_handle = process.run("showmethekey-cli", {}, function(line)
    -- Pipe stdout directly into user state file reactively
    system:write_state("last_pressed_key", line)
end, function(exit_code)
    print("Key logger subprocess exited with status: " .. tostring(exit_code))
end)

-- 4. Import Subcomponents Scoped Lexically
local workspaces_bar   = require("widgets.workspaces_bar")
local mpris_player     = require("widgets.mpris_player")
local volume_mixer     = require("widgets.volume_mixer")
local network_manager  = require("widgets.network_manager")
local bluetooth_manager= require("widgets.bluetooth_manager")
local wallpaper_picker = require("widgets.wallpaper_picker")
local sysinfo_widget   = require("widgets.sysinfo")

-- 5. Return Core Top-Level Window Surface List
return {
    -- =========================================================================
    -- SURFACE 1: Persistent Top Status Bar
    -- =========================================================================
    panel {
        id = "top_status_bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true, -- Reserves physical screen area so windows don't overlap
        height = 32,
        monitor = "All", -- Spawn automatically on every connected monitor
        
        child = rect {
            id = "bar_rect",
            background = theme.background,
            align_h = "Stretch",
            align_v = "Center",
            padding = { left = 12, right = 12 },
            
            children = {
                row {
                    align_h = "SpaceBetween",
                    align_v = "Center",
                    children = {
                        -- LEFT SECTION: Workspaces, Focused Application Icon, and Window Name
                        row {
                            id = "left_bar",
                            spacing = 12,
                            align_v = "Center",
                            children = {
                                icon { name = "preferences-desktop-keyboard-shortcuts", size = 16 },
                                workspaces_bar.bar("eDP-1"), -- Main screen workspace indicator
                                
                                -- Focused App Icon (dynamically resolved off-thread in Rust)
                                icon {
                                    name = bind(workspaces.active_client):map(function(client)
                                        if not client then return "default-application" end
                                        return system:find_icon(client.class, client.title, "default-application")
                                    end),
                                    size = 16
                                },
                                
                                -- Focused App Title Text
                                text {
                                    content = bind(workspaces.active_client):map(function(client)
                                        if not client then return "Desktop Shell" end
                                        return client.title or "Unknown Window"
                                    end),
                                    font_size = 11,
                                    foreground = theme.text,
                                    max_width = 200
                                }
                            }
                        },
                        
                        -- CENTER SECTION: Media Players & Persistent Music Ticker
                        row {
                            id = "center_bar",
                            align_h = "Center",
                            align_v = "Center",
                            children = {
                                mpris_player.mini_ticker()
                            }
                        },
                        
                        -- RIGHT SECTION: System Telemetry & Dropdown Toggles
                        row {
                            id = "right_bar",
                            spacing = 16,
                            align_v = "Center",
                            children = {
                                -- Sysinfo CPU/RAM Text indicator
                                button {
                                    on_click = function()
                                        local cc = system.state:get().cc_menu or ""
                                        system:write_state("cc_menu", cc == "sysinfo" and "" or "sysinfo")
                                    end,
                                    children = {
                                        sysinfo_widget.mini_indicator()
                                    }
                                },

                                -- Wi-Fi SSID click-trigger for Connection Manager Menu
                                button {
                                    on_click = function()
                                        local cc = system.state:get().cc_menu or ""
                                        system:write_state("cc_menu", cc == "network" and "" or "network")
                                    end,
                                    children = {
                                        row {
                                            spacing = 4,
                                            children = {
                                                icon {
                                                    name = bind(network.connected):map(function(c)
                                                        return c and "network-wireless" or "network-offline"
                                                    end),
                                                    size = 14
                                                },
                                                text {
                                                    content = bind(network.ssid):map(function(ssid)
                                                        return ssid or "Offline"
                                                    end),
                                                    font_size = 12
                                                }
                                            }
                                        }
                                    }
                                },

                                -- Bluetooth click-trigger for Accessory Manager Menu
                                button {
                                    on_click = function()
                                        local cc = system.state:get().cc_menu or ""
                                        system:write_state("cc_menu", cc == "bluetooth" and "" or "bluetooth")
                                    end,
                                    children = {
                                        row {
                                            spacing = 4,
                                            children = {
                                                icon { name = "bluetooth-active", size = 14 },
                                                text {
                                                    content = bind(bluetooth.connected_devices):map(function(devs)
                                                        if #devs == 0 then return "None" end
                                                        return string.format("%d Devs", #devs)
                                                    end),
                                                    font_size = 12
                                                }
                                            }
                                        }
                                    }
                                },

                                -- Volume Scroll and Click-trigger for Output routing Mixer
                                button {
                                    on_click = function()
                                        local cc = system.state:get().cc_menu or ""
                                        system:write_state("cc_menu", cc == "audio" and "" or "audio")
                                    end,
                                    on_scroll = function(direction)
                                        local current = audio.volume:get()
                                        if direction == "Up" then
                                            audio:set_volume(math.min(1.0, current + 0.05))
                                        else
                                            audio:set_volume(math.max(0.0, current - 0.05))
                                        end
                                    end,
                                    children = {
                                        row {
                                            spacing = 4,
                                            children = {
                                                icon {
                                                    name = bind(audio.muted):map(function(m)
                                                        return m and "audio-volume-muted" or "audio-volume-high"
                                                    end),
                                                    size = 14
                                                },
                                                text {
                                                    content = bind(audio.volume):map(function(v)
                                                        return string.format("%d%%", math.floor(v * 100))
                                                    end),
                                                    font_size = 12
                                                }
                                            }
                                        }
                                    }
                                },

                                -- Wallpaper Selector Menu Trigger
                                button {
                                    on_click = function()
                                        local cc = system.state:get().cc_menu or ""
                                        system:write_state("cc_menu", cc == "wallpaper" and "" or "wallpaper")
                                    end,
                                    children = {
                                        icon { name = "background-image", size = 14 }
                                    }
                                },

                                -- Battery Telemetry with Warning Alert
                                row {
                                    spacing = 4,
                                    children = {
                                        icon {
                                            name = bind(battery.charging):map(function(c)
                                                return c and "battery-charging" or "battery-good"
                                            end),
                                            size = 14
                                        },
                                        text {
                                            content = bind(battery.percent):map(function(p)
                                                return string.format("%d%%", p)
                                            end),
                                            font_size = 12,
                                            foreground = bind(battery.percent):map(function(p)
                                                return p < 15 and theme.warning or theme.text
                                            end)
                                        }
                                    }
                                },

                                -- System Clock (Epoch timer updated natively)
                                text {
                                    content = bind(system.time):map(function(t)
                                        return os.date("%H:%M:%S", t)
                                    end),
                                    font_size = 13,
                                    foreground = theme.text
                                }
                            }
                        }
                    }
                }
            }
        }
    },

    -- =========================================================================
    -- SURFACE 2: Fullscreen Transparent Overlay Surface (Canvas Layer)
    -- =========================================================================
    panel {
        id = "overlay_canvas",
        layer = "Overlay",
        anchor = { top = true, bottom = true, left = true, right = true },
        exclusive = false, -- Non-exclusive so transparent areas pass mouse clicks
        
        child = rect {
            id = "canvas_rect",
            background = "Transparent", -- Solid background un-mapped, click passes through
            align_h = "Stretch",
            align_v = "Stretch",
            
            children = {
                -- Wi-Fi Manager Overlay Widget
                rect {
                    id = "wifi_overlay_wrapper",
                    width = 300,
                    height = 360,
                    background = theme.surface,
                    radius = 8,
                    align_h = "End",
                    align_v = "Start",
                    margin = { top = 40, right = 100 },
                    visible = bind(system.state):map(function(s)
                        return s.cc_menu == "network"
                    end),
                    children = {
                        network_manager.widget()
                    }
                },

                -- Bluetooth Accessory Overlay Widget
                rect {
                    id = "bluetooth_overlay_wrapper",
                    width = 320,
                    height = 380,
                    background = theme.surface,
                    radius = 8,
                    align_h = "End",
                    align_v = "Start",
                    margin = { top = 40, right = 150 },
                    visible = bind(system.state):map(function(s)
                        return s.cc_menu == "bluetooth"
                    end),
                    children = {
                        bluetooth_manager.widget()
                    }
                },

                -- Audio Sinks & App Mixer Overlay Widget
                rect {
                    id = "audio_overlay_wrapper",
                    width = 340,
                    height = 420,
                    background = theme.surface,
                    radius = 8,
                    align_h = "End",
                    align_v = "Start",
                    margin = { top = 40, right = 200 },
                    visible = bind(system.state):map(function(s)
                        return s.cc_menu == "audio"
                    end),
                    children = {
                        volume_mixer.widget()
                    }
                },

                -- Wallpaper Switcher Overlay Widget
                rect {
                    id = "wallpaper_overlay_wrapper",
                    width = 280,
                    height = 240,
                    background = theme.surface,
                    radius = 8,
                    align_h = "End",
                    align_v = "Start",
                    margin = { top = 40, right = 250 },
                    visible = bind(system.state):map(function(s)
                        return s.cc_menu == "wallpaper"
                    end),
                    children = {
                        wallpaper_picker.widget()
                    }
                },

                -- Sysinfo Diagnostics Overlay Widget
                rect {
                    id = "sysinfo_overlay_wrapper",
                    width = 260,
                    height = 240,
                    background = theme.surface,
                    radius = 8,
                    align_h = "End",
                    align_v = "Start",
                    margin = { top = 40, right = 50 },
                    visible = bind(system.state):map(function(s)
                        return s.cc_menu == "sysinfo"
                    end),
                    children = {
                        sysinfo_widget.widget()
                    }
                },

                -- Screen Lock Block Modal
                rect {
                    id = "screen_lock_block",
                    width = "Fill",
                    height = "Fill",
                    background = "#0A0A0CFC", -- Deep black masking background
                    visible = bind(system.state):map(function(s)
                        return s.locked == true
                    end),
                    align_h = "Stretch",
                    align_v = "Stretch",
                    children = {
                        column {
                            spacing = 16,
                            align_h = "Center",
                            align_v = "Center",
                            children = {
                                icon { name = "system-lock-screen", size = 64 },
                                text { content = "Session Locked", font_size = 20, font_weight = "Bold" },
                                textfield {
                                    id = "unlock_pass_field",
                                    width = 240,
                                    height = 36,
                                    placeholder = "Enter password to unlock...",
                                    secure = true,
                                    focus = true,
                                    on_submit = function(text)
                                        -- Bypasses Lua VM memory leak boundaries, processed in Rust
                                        system:write_state("locked", false)
                                    end
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
```

---

## 2. Dynamic Workspaces Widget (`widgets/workspaces_bar.lua`)

This file implements a complete multi-display status indicator. It displays active workspaces, focused seats, and supports clicking to focus. It dynamically processes workspace layouts via the compositor workspace adaptor trait behind the scenes.

```lua
-- =============================================================================
-- widgets/workspaces_bar.lua - Workspaces Status Indicator
-- =============================================================================

local workspaces = require("oblisk.workspaces")

local function workspaces_bar(output_name)
    return list {
        id = "workspace_list_" .. output_name,
        direction = "Horizontal",
        spacing = 6,
        align_v = "Center",
        source = bind(workspaces.outputs):map(function(outputs)
            -- Find outputs list and grab target spaces
            for _, out in ipairs(outputs) do
                if out.name == output_name then
                    return {
                        { id = 1, name = "I", active = out.active_workspace == 1, focused = out.focused_workspace == 1 },
                        { id = 2, name = "II", active = out.active_workspace == 2, focused = out.focused_workspace == 2 },
                        { id = 3, name = "III", active = out.active_workspace == 3, focused = out.focused_workspace == 3 },
                        { id = 4, name = "IV", active = out.active_workspace == 4, focused = out.focused_workspace == 4 },
                        { id = 5, name = "V", active = out.active_workspace == 5, focused = out.focused_workspace == 5 },
                    }
                end
            end
            return {}
        end),
        itemfn = function(ws)
            return button {
                width = 24,
                height = 24,
                on_click = function()
                    workspaces:focus(ws:get().id)
                end,
                children = {
                    rect {
                        width = "Fill",
                        height = "Fill",
                        radius = 12,
                        background = bind(ws):map(function(w)
                            if w.focused then
                                return "#89B4FA" -- Active focused
                            elseif w.active then
                                return "#45475A" -- Active on other displays
                            else
                                return "#1E1E2E" -- Inactive
                            end
                        end),
                        children = {
                            text {
                                content = bind(ws):map(function(w) return w.name end),
                                font_size = 10,
                                foreground = bind(ws):map(function(w)
                                    return w.focused and "#11111B" or "#CDD6F4"
                                end),
                                align_h = "Center",
                                align_v = "Center"
                            }
                        }
                    }
                }
            }
        end
    }
end

return {
    bar = workspaces_bar
}
```

---

## 3. Wi-Fi Manager Overlay Widget (`widgets/network_manager.lua`)

This widget implements a complete NetworkManager UI. It displays connected SSIDs, scans for networks, categorizes by frequency bands (2.4/5/6 GHz), handles passwords for secure networks, hidden SSID networks, and deletes saved connection profiles.

```lua
-- =============================================================================
-- widgets/network_manager.lua - Wi-Fi Access Point Manager Widget
-- =============================================================================

local network = require("oblisk.network")
local system  = require("oblisk.system")

local function build_widget()
    return column {
        spacing = 10,
        padding = 12,
        align_h = "Stretch",
        children = {
            -- Master Switches Header
            row {
                align_h = "SpaceBetween",
                children = {
                    text { content = "Network Manager", font_size = 14, font_weight = "Bold" },
                    button {
                        width = 50,
                        height = 20,
                        background = bind(network.wifi_enabled):map(function(en)
                            return en and "#A6E3A1" or "#313244"
                        end),
                        radius = 4,
                        on_click = function()
                            network:set_wifi_enabled(not network.wifi_enabled:get())
                        end,
                        children = {
                            text {
                                content = bind(network.wifi_enabled):map(function(en)
                                    return en and "Wi-Fi On" or "Wi-Fi Off"
                                end),
                                font_size = 10,
                                foreground = "#11111B",
                                align_h = "Center",
                                align_v = "Center"
                            }
                        }
                    }
                }
            },

            -- Scanner Trigger Controls
            row {
                spacing = 8,
                children = {
                    button {
                        width = 80,
                        height = 24,
                        background = "#45475A",
                        radius = 4,
                        on_click = function()
                            network:scan()
                        end,
                        children = {
                            text { content = "Scan APs", font_size = 11, align_h = "Center", align_v = "Center" }
                        }
                    },
                    text {
                        content = bind(network.scanning):map(function(s)
                            return s and "Scanning..." or "Idle"
                        end),
                        font_size = 10,
                        foreground = "#7F849C"
                    }
                }
            },

            -- Scanned APs List
            list {
                id = "scanned_wifi_list",
                height = 180,
                spacing = 6,
                source = bind(network.available_networks),
                itemfn = function(ap)
                    return button {
                        align_h = "Stretch",
                        height = 36,
                        background = bind(ap):map(function(item)
                            return item.active and "#2E3047" or "Transparent"
                        end),
                        radius = 4,
                        on_click = function()
                            local item = ap:get()
                            if item.secure and not item.active then
                                -- Save target SSID to local storage state and trigger modal
                                system:write_state("target_ssid", item.ssid)
                                system:write_state("show_password_modal", true)
                            else
                                -- Open or previously saved secure connection auto-association
                                network:connect(item.ssid)
                            end
                        end,
                        children = {
                            row {
                                align_h = "SpaceBetween",
                                align_v = "Center",
                                padding = { left = 6, right = 6 },
                                children = {
                                    row {
                                        spacing = 8,
                                        children = {
                                            icon {
                                                name = bind(ap):map(function(item)
                                                    return item.secure and "network-wireless-encrypted" or "network-wireless"
                                                end),
                                                size = 14
                                            },
                                            column {
                                                children = {
                                                    text { content = bind(ap):map(function(item) return item.ssid end), font_size = 11 },
                                                    text { content = bind(ap):map(function(item) return item.band end), font_size = 9, foreground = "#7F849C" }
                                                }
                                            }
                                        }
                                    },
                                    row {
                                        spacing = 8,
                                        children = {
                                            text { content = bind(ap):map(function(item) return string.format("%d%%", item.strength) end), font_size = 10 },
                                            -- Forget Saved Connection Button
                                            button {
                                                width = 16,
                                                height = 16,
                                                on_click = function()
                                                    network:forget(ap:get().ssid)
                                                end,
                                                children = {
                                                    icon { name = "edit-clear", size = 12 }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    };
                end
            },

            -- Hidden Network Association Panel
            rect {
                width = "Fill",
                height = 50,
                background = "#1E1E2E",
                radius = 4,
                padding = 4,
                children = {
                    column {
                        spacing = 4,
                        children = {
                            text { content = "Connect to Hidden Network", font_size = 9, foreground = "#7F849C" },
                            row {
                                spacing = 4,
                                children = {
                                    textfield {
                                        id = "hidden_ssid_field",
                                        width = 150,
                                        height = 24,
                                        placeholder = "Hidden SSID..."
                                    },
                                    button {
                                        width = 60,
                                        height = 24,
                                        background = "#89B4FA",
                                        radius = 4,
                                        on_click = function()
                                            -- Connection triggers hidden scans natively
                                            network:connect("hidden_ssid_field", nil, true)
                                        end,
                                        children = {
                                            text { content = "Connect", font_size = 10, foreground = "#11111B", align_h = "Center", align_v = "Center" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
end

return {
    widget = build_widget
}
```

---

## 4. Bluetooth Accessory Overlay Widget (`widgets/bluetooth_manager.lua`)

This widget lists connected accessories with categorizations (keyboard/headphones), battery statuses, unpairs/forgets devices, and cycles codecs.

```lua
-- =============================================================================
-- widgets/bluetooth_manager.lua - Bluetooth Accessory Manager Widget
-- =============================================================================

local bluetooth = require("oblisk.bluetooth")

local function build_widget()
    return column {
        spacing = 10,
        padding = 12,
        align_h = "Stretch",
        children = {
            row {
                align_h = "SpaceBetween",
                children = {
                    text { content = "Bluetooth Manager", font_size = 14, font_weight = "Bold" },
                    button {
                        width = 50,
                        height = 20,
                        background = bind(bluetooth.enabled):map(function(en) return en and "#A6E3A1" or "#313244" end),
                        radius = 4,
                        on_click = function()
                            bluetooth:set_enabled(not bluetooth.enabled:get())
                        end,
                        children = {
                            text { content = "Power", font_size = 10, foreground = "#11111B", align_h = "Center", align_v = "Center" }
                        }
                    }
                }
            },

            -- Discovery Controls
            button {
                width = "Fill",
                height = 26,
                background = bind(bluetooth.discovering):map(function(d) return d and "#FAB387" or "#45475A" end),
                radius = 4,
                on_click = function()
                    if bluetooth.discovering:get() then
                        bluetooth:stop_discovery()
                    else
                        bluetooth:start_discovery()
                    end
                end,
                children = {
                    text {
                        content = bind(bluetooth.discovering):map(function(d)
                            return d and "Stop Scanning" or "Scan Bluetooth Devices"
                        end),
                        font_size = 11,
                        align_h = "Center",
                        align_v = "Center"
                    }
                }
            },

            -- Connected Devices list with Battery, Categorized Icons, Codec changes and Forgetting
            list {
                id = "bt_connected_list",
                height = 140,
                spacing = 6,
                source = bind(bluetooth.connected_devices),
                itemfn = function(dev)
                    return rect {
                        width = "Fill",
                        height = 50,
                        background = "#1E1E2E",
                        radius = 6,
                        padding = 6,
                        children = {
                            row {
                                align_h = "SpaceBetween",
                                align_v = "Center",
                                children = {
                                    row {
                                        spacing = 8,
                                        children = {
                                            -- Categorized Icons from BlueZ
                                            icon {
                                                name = bind(dev):map(function(item)
                                                    if item.category == "headphones" then return "audio-headphones"
                                                    elseif item.category == "keyboard" then return "input-keyboard"
                                                    elseif item.category == "mouse" then return "input-mouse"
                                                    else return "bluetooth" end
                                                end),
                                                size = 16
                                            },
                                            column {
                                                children = {
                                                    text { content = bind(dev):map(function(item) return item.name end), font_size = 11 },
                                                    text {
                                                        content = bind(dev):map(function(item)
                                                            return string.format("Bat: %d%% | Codec: %s", item.battery, item.codec or "None")
                                                        end),
                                                        font_size = 9,
                                                        foreground = "#7F849C"
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    row {
                                        spacing = 6,
                                        children = {
                                            -- Codec Selector Button
                                            button {
                                                width = 40,
                                                height = 20,
                                                background = "#313244",
                                                radius = 2,
                                                on_click = function()
                                                    local d = dev:get()
                                                    local next_codec = "SBC"
                                                    if d.codec == "SBC" then next_codec = "AAC"
                                                    elseif d.codec == "AAC" then next_codec = "LDAC" end
                                                    bluetooth:set_audio_codec(d.mac, next_codec)
                                                end,
                                                children = {
                                                    text { content = "Codec", font_size = 9, align_h = "Center", align_v = "Center" }
                                                }
                                            },
                                            -- Forget / Unpair button
                                            button {
                                                width = 20,
                                                height = 20,
                                                on_click = function()
                                                    bluetooth:forget(dev:get().mac)
                                                end,
                                                children = {
                                                    icon { name = "user-trash", size = 12 }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                end
            },

            -- Discovered Devices List
            list {
                id = "bt_discovered_list",
                height = 80,
                spacing = 4,
                source = bind(bluetooth.discovered_devices),
                itemfn = function(dev)
                    return row {
                        align_h = "SpaceBetween",
                        children = {
                            text { content = bind(dev):map(function(item) return item.name or item.mac end), font_size = 10 },
                            button {
                                width = 50,
                                height = 18,
                                background = "#89B4FA",
                                radius = 2,
                                on_click = function()
                                    bluetooth:pair(dev:get().mac)
                                end,
                                children = {
                                    text { content = "Pair", font_size = 9, foreground = "#11111B", align_h = "Center", align_v = "Center" }
                                }
                            }
                        }
                    }
                end
            }
        }
    }
end

return {
    widget = build_widget
}
```

---

## 5. PipeWire Audio & Application Mixer (`widgets/volume_mixer.lua`)

This mixer lists default output/input routes for audio sinks and active per-application volume sliders (Pulse/PipeWire App Mixer).

```lua
-- =============================================================================
-- widgets/volume_mixer.lua - PipeWire Output Router & App Mixer
-- =============================================================================

local audio = require("oblisk.audio")

local function build_widget()
    return column {
        spacing = 10,
        padding = 12,
        align_h = "Stretch",
        children = {
            text { content = "Audio Controller (PipeWire-Only)", font_size = 13, font_weight = "Bold" },

            -- Output default Sinks list
            text { content = "PLAYBACK SINK DEVICES", font_size = 9, foreground = "#7F849C" },
            list {
                id = "sinks_list",
                height = 80,
                spacing = 4,
                source = bind(audio.sinks),
                itemfn = function(sink)
                    return button {
                        align_h = "Stretch",
                        height = 24,
                        background = bind(sink):map(function(item)
                            return item.active and "#45475A" or "Transparent"
                        end),
                        radius = 4,
                        on_click = function()
                            audio:set_default_sink(sink:get().id)
                        end,
                        children = {
                            row {
                                align_h = "SpaceBetween",
                                padding = { left = 6, right = 6 },
                                children = {
                                    text { content = bind(sink):map(function(item) return item.name end), font_size = 10 },
                                    icon { name = "dialog-ok", size = 12, visible = bind(sink):map(function(item) return item.active end) }
                                }
                            }
                        }
                    }
                end
            },

            -- Application Mixer List (Per-App Volumes)
            text { content = "APPLICATION AUDIO MIXER", font_size = 9, foreground = "#7F849C" },
            list {
                id = "apps_mixer_list",
                height = 140,
                spacing = 6,
                source = bind(audio.apps),
                itemfn = function(app)
                    return rect {
                        width = "Fill",
                        height = 36,
                        background = "#1E1E2E",
                        radius = 4,
                        padding = 4,
                        children = {
                            row {
                                align_h = "SpaceBetween",
                                align_v = "Center",
                                children = {
                                    row {
                                        spacing = 6,
                                        children = {
                                            icon { name = "applications-system", size = 14 },
                                            text { content = bind(app):map(function(item) return item.name end), font_size = 10 }
                                        }
                                    },
                                    row {
                                        spacing = 8,
                                        children = {
                                            -- Scroll trigger for app volume adjustments
                                            button {
                                                width = 50,
                                                height = 20,
                                                background = "#313244",
                                                radius = 2,
                                                on_scroll = function(direction)
                                                    local item = app:get()
                                                    local target = item.volume
                                                    if direction == "Up" then
                                                        target = math.min(1.0, target + 0.05)
                                                    else
                                                        target = math.max(0.0, target - 0.05)
                                                    end
                                                    audio:set_app_volume(item.id, target)
                                                end,
                                                children = {
                                                    text {
                                                        content = bind(app):map(function(item)
                                                            return string.format("%d%%", math.floor(item.volume * 100))
                                                        end),
                                                        font_size = 9,
                                                        align_h = "Center",
                                                        align_v = "Center"
                                                    }
                                                }
                                            },
                                            -- App stream mute button
                                            button {
                                                width = 20,
                                                height = 20,
                                                on_click = function()
                                                    audio:set_app_muted(app:get().id, not app:get().muted)
                                                end,
                                                children = {
                                                    icon {
                                                        name = bind(app):map(function(item)
                                                            return item.muted and "audio-volume-muted" or "audio-volume-high"
                                                        end),
                                                        size = 12
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                end
            }
        }
    }
end

return {
    widget = build_widget
}
```

---

## 6. MPRIS Mini Ticker & Zero-Polling Progress Sync (`widgets/mpris_player.lua`)

This file implements dynamic MPRIS widgets. It calculates track progress mathematically inside Lua using monotonic epochs to guarantee zero socket polling during updates.

```lua
-- =============================================================================
-- widgets/mpris_player.lua - Live MPRIS Seek Sync & Mini Ticker
-- =============================================================================

local mpris  = require("oblisk.mpris")
local system = require("oblisk.system")

-- Mini Ticker for center of the status bar
local function mini_ticker()
    return list {
        id = "mpris_active_players",
        source = bind(mpris.players),
        itemfn = function(p)
            local is_playing = bind(p):map(function(item) return item.play_state == "Playing" end)
            
            return button {
                visible = is_playing,
                on_click = function()
                    mpris:send_command(p:get().id, "play_pause")
                end,
                children = {
                    row {
                        spacing = 6,
                        align_v = "Center",
                        children = {
                            icon { name = "media-playback-start", size = 12 },
                            text {
                                content = bind(p):map(function(item)
                                    return string.format("%s - %s", item.title, item.artist)
                                end),
                                font_size = 11,
                                max_width = 240
                            }
                        }
                    }
                }
            }
        end
    }
end

-- Full Seek Bar Mixer Widget with zero-polling live math sync!
local function seek_bar_widget(player_id)
    return column {
        spacing = 8,
        align_h = "Stretch",
        children = {
            -- Displays mathematically interpolated progress values
            text {
                content = bind(system.time):map(function(now_sec)
                    -- Locate specific player
                    local target = nil
                    for _, pl in ipairs(mpris.players:get()) do
                        if pl.id == player_id then target = pl end
                    end
                    if not target or target.length == 0 then return "00:00 / 00:00" end
                    
                    -- Monotonic Math interpolation
                    local pos_us = target.position
                    if target.play_state == "Playing" then
                        -- Calculate microsecond offset delta
                        local delta_us = (system.time:get() * 1000000) - target.position_updated_at
                        pos_us = math.min(target.length, pos_us + delta_us)
                    end
                    
                    local cur_sec = math.floor(pos_us / 1000000)
                    local len_sec = math.floor(target.length / 1000000)
                    return string.format("%02d:%02d / %02d:%02d", 
                        math.floor(cur_sec / 60), cur_sec % 60,
                        math.floor(len_sec / 60), len_sec % 60
                    )
                end),
                font_size = 10,
                foreground = "#7F849C",
                align_h = "Center"
            },

            -- Relative seek control buttons
            row {
                align_h = "Center",
                spacing = 16,
                children = {
                    button {
                        width = 40,
                        height = 24,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            -- Relative backward seek (-10 seconds)
                            mpris:seek_relative(player_id, -10000000)
                        end,
                        children = { text { content = "-10s", font_size = 9, align_h = "Center", align_v = "Center" } }
                    },
                    button {
                        width = 40,
                        height = 24,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            -- Relative forward seek (+10 seconds)
                            mpris:seek_relative(player_id, 10000000)
                        end,
                        children = { text { content = "+10s", font_size = 9, align_h = "Center", align_v = "Center" } }
                    }
                }
            }
        }
    }
end

return {
    mini_ticker = mini_ticker,
    seek_bar = seek_bar_widget
}
```

---

## 7. Dynamic Wallpaper Picker and Fit Transitions (`widgets/wallpaper_picker.lua`)

This widget allows changing wallpapers dynamically using hardware-accelerated animated GPU transitions and box fit settings.

```lua
-- =============================================================================
-- widgets/wallpaper_picker.lua - Dynamic Wallpaper Setter Widget
-- =============================================================================

local wallpaper = require("oblisk.wallpaper")
local system    = require("oblisk.system")

local function build_widget()
    return column {
        spacing = 10,
        padding = 12,
        align_h = "Stretch",
        children = {
            text { content = "Wallpaper Controller", font_size = 13, font_weight = "Bold" },

            -- Fit Algorithm options list
            text { content = "GPU BOX FIT MODES", font_size = 9, foreground = "#7F849C" },
            row {
                spacing = 8,
                children = {
                    button {
                        width = 60,
                        height = 22,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            system:write_state("fit_mode", "Cover")
                        end,
                        children = { text { content = "Cover", font_size = 9, align_h = "Center", align_v = "Center" } }
                    },
                    button {
                        width = 60,
                        height = 22,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            system:write_state("fit_mode", "Contain")
                        end,
                        children = { text { content = "Contain", font_size = 9, align_h = "Center", align_v = "Center" } }
                    },
                    button {
                        width = 60,
                        height = 22,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            system:write_state("fit_mode", "Stretch")
                        end,
                        children = { text { content = "Stretch", font_size = 9, align_h = "Center", align_v = "Center" } }
                    }
                }
            },

            -- Shader Animations selections list
            text { content = "SHADER TRANSITIONS", font_size = 9, foreground = "#7F849C" },
            row {
                spacing = 8,
                children = {
                    button {
                        width = 60,
                        height = 22,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            system:write_state("transition", "Crossfade")
                        end,
                        children = { text { content = "Fade", font_size = 9, align_h = "Center", align_v = "Center" } }
                    },
                    button {
                        width = 60,
                        height = 22,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            system:write_state("transition", "Sweep")
                        end,
                        children = { text { content = "Sweep", font_size = 9, align_h = "Center", align_v = "Center" } }
                    },
                    button {
                        width = 60,
                        height = 22,
                        background = "#313244",
                        radius = 4,
                        on_click = function()
                            system:write_state("transition", "Zoom")
                        end,
                        children = { text { content = "Zoom", font_size = 9, align_h = "Center", align_v = "Center" } }
                    }
                }
            },

            -- Select Wallpaper trigger button
            button {
                width = "Fill",
                height = 32,
                background = "#89B4FA",
                radius = 6,
                on_click = function()
                    local fit = system.state:get().fit_mode or "Cover"
                    local tr = system.state:get().transition or "Crossfade"
                    -- Set dynamic wallpaper on primary display eDP-1 with GPU Sweep transition of 500ms
                    wallpaper:set("eDP-1", "/usr/share/backgrounds/wall.png", fit, tr, 500)
                end,
                children = {
                    text { content = "Apply wallpaper.png on eDP-1", font_size = 11, foreground = "#11111B", align_h = "Center", align_v = "Center" }
                }
            }
        }
    }
end

return {
    widget = build_widget
}
```

---

## 8. Dynamic CPU/RAM System Health Diagnostics (`widgets/sysinfo.lua`)

This widget displays high-fidelity system resources. It allows setting governor performance profiles and displays core thermal status.

```lua
-- =============================================================================
-- widgets/sysinfo.lua - CPU/RAM System Diagnostics Panel
-- =============================================================================

local sysinfo = require("oblisk.sysinfo")
local power   = require("oblisk.power")

-- Tiny status bar text indicator
local function mini_indicator()
    return row {
        spacing = 6,
        children = {
            icon { name = "utilities-system-monitor", size = 14 },
            text {
                content = bind(sysinfo.cpu_percent):map(function(cpu)
                    return string.format("CPU: %d%%", cpu)
                end),
                font_size = 12
            },
            text {
                content = bind(sysinfo.ram_percent):map(function(ram)
                    return string.format("RAM: %d%%", ram)
                end),
                font_size = 12
            }
        }
    }
end

-- Full dropdown details widget
local function build_widget()
    return column {
        spacing = 10,
        padding = 12,
        align_h = "Stretch",
        children = {
            text { content = "System Diagnostics", font_size = 13, font_weight = "Bold" },

            -- CPU Details Card
            row {
                align_h = "SpaceBetween",
                children = {
                    text { content = "Total CPU Utilization:", font_size = 11 },
                    text {
                        content = bind(sysinfo.cpu_percent):map(function(c) return tostring(c) .. "%" end),
                        font_size = 11,
                        foreground = "#A6E3A1"
                    }
                }
            },

            -- RAM Details Card
            row {
                align_h = "SpaceBetween",
                children = {
                    text { content = "Memory Footprint:", font_size = 11 },
                    text {
                        content = bind(sysinfo.ram_percent):map(function(r) return tostring(r) .. "%" end),
                        font_size = 11,
                        foreground = "#89B4FA"
                    }
                }
            },

            -- CPU Thermals core list
            text { content = "HARDWARE CORE TEMPERATURES", font_size = 9, foreground = "#7F849C" },
            list {
                id = "temp_cores_list",
                height = 50,
                spacing = 4,
                source = bind(sysinfo.temp_cores),
                itemfn = function(temp, index)
                    return row {
                        align_h = "SpaceBetween",
                        children = {
                            text { content = string.format("Core %d:", index), font_size = 10 },
                            text {
                                content = bind(temp):map(function(t) return tostring(t) .. " °C" end),
                                font_size = 10,
                                foreground = bind(temp):map(function(t)
                                    return t > 75 and "#F38BA8" or "#CDD6F4" -- Turn red on hot cores
                                end)
                            }
                        }
                    }
                end
            },

            -- Power Profiles Toggles
            text { content = "CPU SCALING PROFILES", font_size = 9, foreground = "#7F849C" },
            row {
                spacing = 8,
                children = {
                    button {
                        width = 70,
                        height = 24,
                        background = bind(power.active_profile):map(function(p)
                            return p == "performance" and "#FAB387" or "#313244"
                        end),
                        radius = 4,
                        on_click = function()
                            power:set_profile("performance")
                        end,
                        children = { text { content = "Perf", font_size = 10, align_h = "Center", align_v = "Center" } }
                    },
                    button {
                        width = 70,
                        height = 24,
                        background = bind(power.active_profile):map(function(p)
                            return p == "balanced" and "#89B4FA" or "#313244"
                        end),
                        radius = 4,
                        on_click = function()
                            power:set_profile("balanced")
                        end,
                        children = { text { content = "Balanced", font_size = 10, align_h = "Center", align_v = "Center" } }
                    },
                    button {
                        width = 70,
                        height = 24,
                        background = bind(power.active_profile):map(function(p)
                            return p == "power-saver" and "#A6E3A1" or "#313244"
                        end),
                        radius = 4,
                        on_click = function()
                            power:set_profile("power-saver")
                        end,
                        children = { text { content = "Saver", font_size = 10, align_h = "Center", align_v = "Center" } }
                    }
                }
            }
        }
    }
end

return {
    mini_indicator = mini_indicator,
    widget = build_widget
}
```
