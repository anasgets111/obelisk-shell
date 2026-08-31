---@meta
-- The `oblisk` namespace: 17 capabilities the supervisor pushes, plus four members the renderer
-- sources itself.
--
-- Field names here are the ones the supervisor's `Serialize` structs actually emit, not the IDL's
-- prose. Where the two disagree the struct is right, because it is what crosses the socket.
--
-- A capability reads `nil` until its first `StateSnapshot` arrives, which is a state every config
-- sees on every boot. A signal resolving to `nil` means the property is absent, so a bound node
-- renders its default rather than failing the tree (ADR-0044). `oblisk.screens` is the exception:
-- it is seeded to an empty list, so looping over it at first evaluation iterates zero times.

---@class Capability: Signal
---A capability is a signal you can also command. `:get()` and `:map()` read the pushed payload;
---`:invoke()` sends a command the supervisor dispatches. Read-only to Lua otherwise: `:set()`
---refuses it, or a config could overwrite the SSID the supervisor just pushed.
local Capability = {}

---@param command string
---@param ... any
function Capability:invoke(command, ...) end

--- Audio -------------------------------------------------------------------------------------

---@class AudioDevice
---@field id integer
---@field name string
---@field active boolean

---@class AppStream
---@field id integer
---@field pid integer
---@field name? string
---@field process_name? string
---@field volume number `[0, 1]`, not a percentage.
---@field muted boolean

---@class AudioState
---@field volume number `[0, 1]`.
---@field muted boolean
---@field sinks AudioDevice[]
---@field sources AudioDevice[]
---@field apps AppStream[]

---@class AudioCapability: Capability
local AudioCapability = {}
---@return AudioState
function AudioCapability:get() end
---@param fn fun(value: AudioState): any
---@return Signal
function AudioCapability:map(fn) end
---@param command "set_volume"|"set_muted"|"toggle_mute"|"set_default_sink"|"set_default_source"|"set_app_volume"|"set_app_muted"
---@param ... any
function AudioCapability:invoke(command, ...) end

--- Network -----------------------------------------------------------------------------------

---@class AccessPointInfo
---@field ssid string
---@field strength integer `[0, 100]`.
---@field secure boolean
---@field band string
---@field active boolean Whether this is the connected one.

---@class NetworkState
---@field scanning boolean
---@field available_networks AccessPointInfo[]

---@class NetworkCapability: Capability
local NetworkCapability = {}
---@return NetworkState
function NetworkCapability:get() end
---@param fn fun(value: NetworkState): any
---@return Signal
function NetworkCapability:map(fn) end
---@param command "connect"|"forget"|"scan"|"set_wifi_enabled"|"set_ethernet_enabled"|"set_networking_enabled"
---@param ... any `connect` takes `(ssid, hidden)`; `hidden` is required.
function NetworkCapability:invoke(command, ...) end

--- Bluetooth ---------------------------------------------------------------------------------

---@class ConnectedDevice
---@field mac string
---@field name string
---@field battery integer `-1` when the device reports none.
---@field codec? string
---@field category string

---@class DiscoveredDevice
---@field mac string
---@field name string
---@field paired boolean

---@class BluetoothState
---@field enabled boolean
---@field discovering boolean
---@field connected_devices ConnectedDevice[]
---@field discovered_devices DiscoveredDevice[]

---@class BluetoothCapability: Capability
local BluetoothCapability = {}
---@return BluetoothState
function BluetoothCapability:get() end
---@param fn fun(value: BluetoothState): any
---@return Signal
function BluetoothCapability:map(fn) end
---@param command "connect"|"disconnect"|"pair"|"forget"|"set_enabled"|"start_discovery"|"stop_discovery"
---@param ... any
function BluetoothCapability:invoke(command, ...) end

--- Tray --------------------------------------------------------------------------------------

---@class MenuItem
---@field id integer
---@field label string
---@field enabled boolean
---@field visible boolean
---@field toggle_state? integer
---@field children? MenuItem[]

---@class TrayItem
---@field id string
---@field name string
---@field icon_name? string
---@field icon_path? string A `null` from the supervisor arrives as an absent key, not a sentinel, so `if item.icon_path then` is the right guard (ADR-0057).
---@field tooltip? string
---@field status string
---@field item_is_menu boolean
---@field menu? MenuItem[]

---@class TrayState
---@field items TrayItem[]

---@class TrayCapability: Capability
local TrayCapability = {}
---@return TrayState
function TrayCapability:get() end
---@param fn fun(value: TrayState): any
---@return Signal
function TrayCapability:map(fn) end
---@param command "activate"|"activate_menu_item"|"menu_will_show"
---@param ... any
function TrayCapability:invoke(command, ...) end

--- Notifications -----------------------------------------------------------------------------

---@class NotificationSpan
---@field kind "text"|"image"
---@field text? string Set when `kind` is `"text"`.
---@field bold? boolean
---@field italic? boolean
---@field underline? boolean
---@field href? string
---@field image_path? string Set when `kind` is `"image"`.

---@class Notification
---@field id integer
---@field app_name string
---@field summary string
---@field body NotificationSpan[] Allowlisted markup runs, already parsed (ADR-0033). Not a string.
---@field icon_path? string
---@field urgency "low"|"normal"|"critical"
---@field has_reply boolean
---@field incarnation integer

---@class NotificationsState
---@field feed Notification[]
---@field dnd boolean

---@class NotificationsCapability: Capability
local NotificationsCapability = {}
---@return NotificationsState
function NotificationsCapability:get() end
---@param fn fun(value: NotificationsState): any
---@return Signal
function NotificationsCapability:map(fn) end
---@param command "dismiss"|"reply"|"set_dnd"|"set_sound"|"low"|"normal"|"critical"
---@param ... any
function NotificationsCapability:invoke(command, ...) end

--- MPRIS -------------------------------------------------------------------------------------

---@class PlayerState
---@field id string
---@field identity string
---@field play_state string
---@field title string
---@field artist string
---@field album_art_path string
---@field position integer Microseconds, as of `position_updated_at`. Nothing interpolates it.
---@field position_updated_at integer
---@field length integer Microseconds.

---@class MprisState
---@field players PlayerState[] The first entry is the active player.

---@class MprisCapability: Capability
local MprisCapability = {}
---@return MprisState
function MprisCapability:get() end
---@param fn fun(value: MprisState): any
---@return Signal
function MprisCapability:map(fn) end
---@param command "control"|"seek"|"seek_relative"
---@param ... any
function MprisCapability:invoke(command, ...) end

--- Sysinfo -----------------------------------------------------------------------------------

---@class SysinfoState
---@field cpu_percent integer
---@field ram_percent integer
---@field swap_percent integer
---@field temp_cores integer[] Degrees C.
---@field temp_gpu integer

---@class SysinfoCapability: Capability
local SysinfoCapability = {}
---@return SysinfoState
function SysinfoCapability:get() end
---@param fn fun(value: SysinfoState): any
---@return Signal
function SysinfoCapability:map(fn) end
---@param command "configure"
---@param ... any
function SysinfoCapability:invoke(command, ...) end

--- Keyboard ----------------------------------------------------------------------------------

---@class KeyboardState
---@field backlight_pct integer `-1` when there is no keyboard backlight.
---@field caps_lock boolean
---@field num_lock boolean
---@field scroll_lock boolean
---@field active_layout string
---@field active_layout_index integer
---@field layout_count integer

---@class KeyboardCapability: Capability
local KeyboardCapability = {}
---@return KeyboardState
function KeyboardCapability:get() end
---@param fn fun(value: KeyboardState): any
---@return Signal
function KeyboardCapability:map(fn) end
---@param command "set_backlight"|"switch_layout"
---@param ... any
function KeyboardCapability:invoke(command, ...) end

--- Privacy -----------------------------------------------------------------------------------

---@class CameraUser
---@field app_name string

---@class PrivacyState
---@field camera_users CameraUser[]

---@class PrivacyCapability: Capability
local PrivacyCapability = {}
---@return PrivacyState
function PrivacyCapability:get() end
---@param fn fun(value: PrivacyState): any
---@return Signal
function PrivacyCapability:map(fn) end

--- Updates -----------------------------------------------------------------------------------

---@class UpdateCandidate
---@field name string
---@field old_version string
---@field new_version string
---@field download_size integer
---@field installed_size integer

---@class UpdatesState
---@field count integer
---@field packages UpdateCandidate[]
---@field last_successful_check? integer Unix seconds.
---@field check_error? string
---@field installing boolean
---@field install_current_step integer
---@field install_total_steps integer
---@field install_current_package string
---@field install_error? string
---@field reboot_required boolean

---@class UpdatesCapability: Capability
local UpdatesCapability = {}
---@return UpdatesState
function UpdatesCapability:get() end
---@param fn fun(value: UpdatesState): any
---@return Signal
function UpdatesCapability:map(fn) end
---@param command "configure"|"install"
---@param ... any
function UpdatesCapability:invoke(command, ...) end

--- Lock --------------------------------------------------------------------------------------

---@class LockState
---@field active boolean
---@field authenticating boolean
---@field attempts integer
---@field error string
---@field requested boolean
---@field acquisition integer

---@class LockCapability: Capability
local LockCapability = {}
---@return LockState
function LockCapability:get() end
---@param fn fun(value: LockState): any
---@return Signal
function LockCapability:map(fn) end
---@param command "lock" The only session command a capability exposes (ADR-0052). There is no logout, restart or power off.
function LockCapability:invoke(command) end

--- Battery -----------------------------------------------------------------------------------

---@class BatteryState
---@field present boolean `false` on a desktop, where `percent` and `charging` mean nothing.
---@field percent integer
---@field charging boolean

---@class BatteryCapability: Capability
local BatteryCapability = {}
---@return BatteryState
function BatteryCapability:get() end
---@param fn fun(value: BatteryState): any
---@return Signal
function BatteryCapability:map(fn) end

--- System ------------------------------------------------------------------------------------

---@class SystemState
---@field time integer Unix seconds. The clock every bar reads.
---@field state any Whatever the last `write_state` stored.

---@class SystemCapability: Capability
local SystemCapability = {}
---@return SystemState
function SystemCapability:get() end
---@param fn fun(value: SystemState): any
---@return Signal
function SystemCapability:map(fn) end

--- Brightness --------------------------------------------------------------------------------

---@class BrightnessState
---@field percent integer

---@class BrightnessCapability: Capability
local BrightnessCapability = {}
---@return BrightnessState
function BrightnessCapability:get() end
---@param fn fun(value: BrightnessState): any
---@return Signal
function BrightnessCapability:map(fn) end
---@param command "set"
---@param percent integer
function BrightnessCapability:invoke(command, percent) end

--- Workspaces --------------------------------------------------------------------------------

---@class WorkspaceEntry
---@field id integer Compositor-assigned. Stable across a rename, unlike `idx`.
---@field idx integer Position on its output, 1-based. What a bar prints.
---@field name? string

---@class OutputWorkspaces
---@field name string The output this belongs to, matching a `screens` entry.
---@field active_workspace integer
---@field focused_workspace? integer
---@field workspaces WorkspaceEntry[]

---@class ActiveClient
---@field title string
---@field class string
---@field is_floating boolean

---@class WorkspacesState
---@field outputs OutputWorkspaces[]
---@field active_client? ActiveClient

---@class WorkspacesCapability: Capability
local WorkspacesCapability = {}
---@return WorkspacesState
function WorkspacesCapability:get() end
---@param fn fun(value: WorkspacesState): any
---@return Signal
function WorkspacesCapability:map(fn) end
---@param command "focus"
---@param ... any
function WorkspacesCapability:invoke(command, ...) end

--- Power -------------------------------------------------------------------------------------

---@class PowerState
---@field active_profile? string
---@field profiles? string[]
---@field on_battery? boolean
---@field energy_rate? number Watts.

---@class PowerCapability: Capability
local PowerCapability = {}
---@return PowerState
function PowerCapability:get() end
---@param fn fun(value: PowerState): any
---@return Signal
function PowerCapability:map(fn) end
---@param command "set_profile"
---@param profile string
function PowerCapability:invoke(command, profile) end

--- Applications ------------------------------------------------------------------------------

---@class AppSummary
---@field id string The `.desktop` id.
---@field name string
---@field icon? string

---@class ApplicationsState
---@field entries AppSummary[]
---@field by_app_id table<string, AppSummary>

---@class ApplicationsCapability: Capability
local ApplicationsCapability = {}
---@return ApplicationsState
function ApplicationsCapability:get() end
---@param fn fun(value: ApplicationsState): any
---@return Signal
function ApplicationsCapability:map(fn) end
---@param command "launch"|"refresh"
---@param ... any
function ApplicationsCapability:invoke(command, ...) end

--- Renderer-sourced members -------------------------------------------------------------------

---@class Screen
---@field name string Matches a surface's `monitor`.
---@field width integer Logical pixels.
---@field height integer
---@field scale number
---@field refresh number Hz. `0` for an output with no current mode, such as a virtual one.

---@class RescueState
---@field is_rescue boolean
---@field error_log string

---@class ObliskVersion
---@field major integer
---@field minor integer
---@field patch integer

---@class Oblisk
---@field audio AudioCapability
---@field network NetworkCapability
---@field bluetooth BluetoothCapability
---@field tray TrayCapability
---@field notifications NotificationsCapability
---@field mpris MprisCapability
---@field sysinfo SysinfoCapability
---@field keyboard KeyboardCapability
---@field privacy PrivacyCapability
---@field updates UpdatesCapability
---@field lock LockCapability
---@field battery BatteryCapability
---@field system SystemCapability
---@field brightness BrightnessCapability
---@field workspaces WorkspacesCapability
---@field power PowerCapability
---@field applications ApplicationsCapability
---@field screens Signal A `Screen[]`. Renderer-sourced, seeded to an empty list, and the one signal with a value at first evaluation (ADR-0041).
---@field rescue Signal A `RescueState`. Renderer-sourced, no commands (ADR-0046).
---@field version ObliskVersion Three integers a config can compare. Not a signal.
---@field config_dir string The directory `shell.lua` was loaded from, so a config can name a file it ships beside itself. Not a signal.
oblisk = {}
