---@meta
-- The four surface roles (`oblisk-idl-api-specs.md` § 6, ADR-0040). A `wl_surface` is inert until a
-- protocol assigns it a role; these are the four this shell exposes, one constructor each. What
-- `shell.lua` returns is the whole surface set, evaluated fresh on every reload (ADR-0038).
--
-- `id`, `layer`, `anchor` and `monitor` reject a `Signal`, unlike every other property in the IDL.
-- They are read once per evaluation to decide whether a reload is an in-place update or a full
-- generation swap (ADR-0001), and a value that moved afterwards would strand that decision.

---@alias Rect { x: number, y: number, width: number, height: number }
---@alias PopupAnchor "Top"|"Bottom"|"Left"|"Right"|"TopLeft"|"TopRight"|"BottomLeft"|"BottomRight"|"Center"

---A surface root takes every `NodeBase` property a `rect` does and paints like one, on top of its
---own § 6.1 topology.
---@class PanelProps: NodeBase, BoxBase
---@field id string Unique. A surface targeting several outputs is one Wayland surface per output, addressed as `"{id}@{output}"`.
---@field layer "Background"|"Bottom"|"Top"|"Overlay" Required, no default: a typo'd layer that quietly stacked a bar on `Background` would be worse than an error.
---@field anchor? { top?: boolean, bottom?: boolean, left?: boolean, right?: boolean }
---@field exclusive? boolean Reserves screen area along the anchored edge, derived from the height the compositor configures.
---@field margin? Edges Offsets from the anchored edges. Moves the surface itself, unlike `padding`.
---@field monitor? string An output name, or `"All"`.
---@field namespace? string What the compositor sees, for rules like Hyprland's `layerrule`. Defaults to `"oblisk-{id}"`.
---@field keyboard_interactivity? "None"|"OnDemand"|"Exclusive" Default `"None"`. Note that niri gives an `on_demand` layer surface focus the moment it maps, with no click involved.
---@field visible? boolean|Signal Unmaps without destroying. Toggling this churns no Wayland objects.
---@field child? Node

---@class WindowProps: NodeBase, BoxBase
---@field id string
---@field title? string|Signal
---@field app_id? string What the compositor matches rules against.
---@field min_size? { width: integer, height: integer } Advisory; the spec says a client should not rely on the compositor obeying it.
---@field max_size? { width: integer, height: integer } Advisory.
---@field on_close? fun() A request, not a command. The callback may decline by doing nothing; the window stays open until the config sets `visible = false`.
---@field visible? boolean|Signal
---@field child? Node

---@class PopupProps: NodeBase, BoxBase
---@field id string
---@field parent string The `id` of the `panel` or `window` this anchors to.
---@field anchor_rect Rect|Signal Required and must be non-zero. Normally the rect `on_click` hands back, so a dropdown lands on the button that opened it.
---@field width integer Required and non-zero. A popup has no `"Fill"`.
---@field height integer Required and non-zero.
---@field anchor? PopupAnchor Which edge or corner of `anchor_rect` the popup hangs from.
---@field gravity? PopupAnchor Which direction it extends from that point.
---@field constraint_adjustment? ("SlideX"|"SlideY"|"FlipX"|"FlipY"|"ResizeX"|"ResizeY")[] How the compositor may move it to keep it on screen. Defaults to `{ "FlipY", "SlideX" }`; the protocol's own default is none. Applied flip, then slide, then resize.
---@field offset? { x: integer, y: integer } Pixel nudge after anchor and gravity.
---@field grab? boolean Default `true`. A compositor may deny the grab, in which case the popup is dismissed immediately and `on_dismiss` fires. That is a normal outcome, not an error.
---@field on_dismiss? fun()
---@field visible? boolean|Signal
---@field child? Node

---@class LockProps: NodeBase, BoxBase
---@field id string
---@field child? Node

---A layer surface (`zwlr_layer_surface_v1`). Bar, dock, wallpaper, OSD, launcher.
---@param props PanelProps
---@return Node
function panel(props) end

---An `xdg_toplevel`. Settings window, standalone dialog.
---@param props WindowProps
---@return Node
function window(props) end

---An `xdg_popup`, rooted under its parent surface. Dropdown, context menu, tooltip. Costs nothing
---until shown: a popup with `visible = false` creates no Wayland object (ADR-0049).
---@param props PopupProps
---@return Node
function popup(props) end

---An `ext_session_lock_surface_v1`. Gets keyboard focus from the protocol rather than from
---`keyboard_interactivity`.
---@param props LockProps
---@return Node
function lock(props) end
