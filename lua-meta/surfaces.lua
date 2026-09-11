---@meta
-- The four surface roles (`lua-api.md` § 6, ADR-0040); a `wl_surface` stays inert.
-- Protocol assigns its role. This shell has one constructor per role. `shell.lua` returns the set,
-- freshly evaluated on every reload (ADR-0038).
--
-- HAND-WRITTEN, on `nodes.lua`'s terms. Its header covers generation and checks; constructor
-- `---@param props`/`---@return Node` lines are bare there too.
--
-- `id`, `layer`, `anchor`, and `monitor` reject `Signal`, unlike the rest. Read them once per
-- evaluation to choose in-place reload or full generation swap (ADR-0001); a later change would
-- strand that decision.

---@alias Rect { x: number, y: number, width: number, height: number }
---@alias PopupAnchor "Top"|"Bottom"|"Left"|"Right"|"TopLeft"|"TopRight"|"BottomLeft"|"BottomRight"|"Center"

---A surface root uses `rect`'s `NodeBase` properties, paints like one, and keeps its § 6 topology.
---@class PanelProps: NodeBase, BoxBase
---@field id string Unique. A surface targeting several outputs is one Wayland surface per output, addressed as `"{id}@{output}"`.
---@field layer "Background"|"Bottom"|"Top"|"Overlay" Required, no default: a typo'd layer that quietly stacked a bar on `Background` would be worse than an error.
---@field anchor? { top?: boolean, bottom?: boolean, left?: boolean, right?: boolean }
---@field width? Length|Bound The layer-shell `set_size` request, live like `margin`. Omit to measure the width from the content, so the surface is the box the layout pass solved for `child` rather than a number guessed against it. The room it may take is the output less this surface's own margins on the edges it is anchored to; the compositor clamps anything larger, and content that wants more than the zones other clients reserved is still cut, so `max_width` is how a growing panel is bounded on purpose. `"Fill"` and an omitted width both hand the axis to the compositor when `anchor` names both `left` and `right` -- layer-shell spans an axis anchored that way and drops any size given -- and `"Fill"` on an axis anchored to one edge or neither is a protocol error, so that surface is refused instead of created.
---@field height? Length|Bound The same on the vertical axis, against `top` and `bottom`. The two are independent: a bar spans its width and measures its height. `exclusive = true` reserves what the compositor configures, so a measured panel reserves what it grew to. `max_height` caps the measurement, which is how "as tall as the stack, but no taller" is written.
---@field exclusive? boolean|"Ignore"|Bound `true` reserves screen area along the anchored edge, derived from the size the compositor configures. `false` (default) reserves none but still sits inside what other surfaces reserved. `"Ignore"` reserves none and ignores theirs, which is what a full-screen wallpaper needs to stay behind a bar rather than below it.
---@field margin? integer|Edges|Bound Offsets from the anchored edges. Moves the surface itself, unlike `padding`. Bindable on a panel root, where it becomes a live `set_margin` on the layer surface rather than a re-layout.
---@field monitor? string An output name, or `"All"`.
---@field namespace? string What the compositor sees, for rules like Hyprland's `layerrule`. Defaults to `"oblisk-{id}"`.
---@field keyboard_interactivity? "None"|"OnDemand"|"Exclusive"|Bound Default `"None"`. Note that niri gives an `on_demand` layer surface focus the moment it maps, with no click involved.
---@field visible? boolean|Bound Unmaps without destroying. Toggling this churns no Wayland objects.
---@field child? Node|fun(output: string): Node? The one root node. A surface holds exactly one; use a `row` or `column` for more. A function is called once per output instance with that output's connector name and its return takes the child's place, so one `monitor = "All"` panel can show a different file per screen (ADR-0121); `nil` maps that instance empty. The eval-time probe calls it with `"PROBE"`.

---@class WindowProps: NodeBase, BoxBase
---@field id string Unique across the surface set. Structural: read once per evaluation to decide in-place update against generation swap, so it rejects a `Signal`.
---@field title? string|Bound What the compositor shows in a task switcher. `xdg_toplevel.set_title`, valid on a mapped window, so a `Signal` here retitles in place.
---@field app_id? string|Bound What the compositor matches rules against.
---@field min_size? { width: integer, height: integer } Advisory; the spec says a client should not rely on the compositor obeying it.
---@field max_size? { width: integer, height: integer } Advisory.
---@field on_close? fun() A request, not a command. The callback may decline by doing nothing; the window stays open until the config sets `visible = false`.
---@field visible? boolean|Bound `false` unmaps the surface without destroying it, so its state and its `id` survive. This is how a panel is opened and closed.
---@field child? Node The one root node. A surface holds exactly one; use a `row` or `column` for more.

---@class PopupProps: NodeBase, BoxBase
---@field id string Unique across the surface set. Structural, on the same terms as a `window`'s.
---@field parent string|Bound The `id` of the `panel` or `window` this anchors to.
---@field anchor_rect Rect|Bound Required and must be non-zero. Normally the rect `on_click` hands back, so a dropdown lands on the button that opened it.
---@field width? integer|Bound Omit to size the popup to its content, which is what a `Content` axis means on every other node: the surface becomes the box the layout pass measured for `child`, so a card is never cut by the surface it sits in. A number is still a number and must be within `(0, 8192]` -- `xdg_positioner::set_size` raises `invalid_input` on zero or negative. No `"Fill"` and no percent: the compositor places a popup rather than fitting it into a parent, so there is no box for either to mean anything against. A measured axis is read on the pass that opens the popup; the popup does not resize afterwards, so a change of content lands on the next open.
---@field height? integer|Bound Omit to measure, on the same terms as `width`. The two are independent: one axis may be a number while the other is measured.
---@field anchor? PopupAnchor Which edge or corner of `anchor_rect` the popup hangs from.
---@field gravity? PopupAnchor|Bound Which direction it extends from that point.
---@field constraint_adjustment? ("SlideX"|"SlideY"|"FlipX"|"FlipY"|"ResizeX"|"ResizeY")[] How the compositor may move it to keep it on screen. Defaults to `{ "FlipY", "SlideX" }`; the protocol's own default is none. Applied flip, then slide, then resize.
---@field offset? { x?: integer, y?: integer } Pixel nudge after anchor and gravity. Either axis alone is fine; the absent one is `0`.
---@field grab? boolean|Bound Default `true`. A compositor may deny the grab, in which case the popup is dismissed immediately and `on_dismiss` fires. That is a normal outcome, not an error.
---@field on_dismiss? fun() Fires when the compositor takes the popup down: a click outside, a denied grab, or the parent going away. Not called when the config unmaps it itself.
---@field visible? boolean|Bound `false` unmaps the surface without destroying it, so its state and its `id` survive. This is how a panel is opened and closed.
---@field child? Node The one root node. A surface holds exactly one; use a `row` or `column` for more.

---@class LockProps: NodeBase, BoxBase
---@field id string Unique across the surface set. Structural, on the same terms as a `window`'s.
---@field child? Node|fun(output: string): Node? The one root node. A surface holds exactly one; use a `row` or `column` for more. A function is called per output the way a `panel`'s is, since a lock surface is one per output too.

---A layer surface (`zwlr_layer_surface_v1`). Bar, dock, wallpaper, OSD, launcher.
---@param props PanelProps
---@return Node
function panel(props) end

---An `xdg_toplevel`. Settings window, standalone dialog.
---@param props WindowProps
---@return Node
function window(props) end

---An `xdg_popup` under its parent surface. Dropdown, context menu, or tooltip. Hidden popups create
---no Wayland object until shown (`visible = false`, ADR-0049).
---@param props PopupProps
---@return Node
function popup(props) end

---An `ext_session_lock_surface_v1`; focus comes from the protocol, not `keyboard_interactivity`.
---@param props LockProps
---@return Node
function lock(props) end
