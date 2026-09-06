---@meta
-- The eight geometric nodes (`oblisk-idl-api-specs.md` § 5.2) and their shared properties (§ 5.1).
--
-- HAND-WRITTEN. `just stubs` does not touch it. Of five `lua-meta` files, only `oblisk.lua` is
-- generated: capability payloads are `Serialize` structs, while a node's schema is 29 scattered
-- `properties.get("...")` calls in `renderer/src/layout/node/`, so it is not derivable data.
--
-- `renderer/src/lua/nodes.rs`'s `meta_stub_tests` matches constructors and each kind's `---@field`
-- names against `accepted_properties`; a property added to the engine but omitted here fails the
-- build. `just types` runs the language server over `dev-config` against these declarations, so a
-- wrong type here surfaces as a diagnostic on working config code (ADR-0081).
--
-- Constructor `---@param props`/`---@return Node` lines stay bare: the type is the sentence, and
-- twelve copies of "the properties above" add nothing.
--
-- Every property accepts a signal in place of a literal, spelled `Bound` in each union. The engine
-- resolves it once per pass (`node::resolve_properties`), then applies the property's normal rules;
-- `radius = someSignal` is as ordinary as `radius = 8`.
--
-- Unions once named `Signal` only on common properties to avoid clutter. That made 21 engine-valid
-- bindings fail under `lua-language-server --check`, because `lua-meta` is what the language server
-- reads.
--
-- Replacing those unions with `Signal` was worse: `---@class` accepts any table-shaped union
-- member, so `string|Signal` on `text.content` admitted every IDL payload table, including a
-- notification's `body` span array, which reached the engine and froze the shell on its last good
-- scene. `Bound` is `userdata`, which rejects tables and matches the runtime signal; see
-- `signals.lua`'s [`Signal`].
--
-- Structural exceptions are `id`, `hover`, `scroll`, and a panel's `layer`/`anchor`/`monitor`/
-- `namespace`: `resolve_properties` passes them raw because identities do not resolve. `hover` and
-- `scroll` still take handles, so they are `Bound`; the other structural fields do not. Callbacks
-- (`on_*`/`itemfn`/`key`) also omit the union: a signal resolves to its value, then is refused as
-- non-function, making the declared union true but useless.

---@alias Node table A node table, as one of the constructors below returns it.
---@alias Align "Start"|"Center"|"End"|"Stretch"
---@alias Cursor "default"|"pointer"|"text"|"not-allowed"|"grab"|"grabbing"|"move"|"crosshair"|"wait"|"progress"|"help"|"context-menu"|"cell"|"vertical-text"|"alias"|"copy"|"no-drop"|"zoom-in"|"zoom-out"|"all-scroll"|"col-resize"|"row-resize"|"n-resize"|"e-resize"|"s-resize"|"w-resize"|"ne-resize"|"nw-resize"|"se-resize"|"sw-resize"|"ew-resize"|"ns-resize"|"nesw-resize"|"nwse-resize" A pointer shape by its CSS name, which is also its `wp_cursor_shape_v1` name.
---@alias Edges { top?: integer, right?: integer, bottom?: integer, left?: integer } Per-edge pixels. A bare number in the same slot broadcasts to all four, which is why every field taking this also takes `integer`.
---@alias Length integer|"Fill" Pixels in `[0, 8192]`, or fill the available space.
---@alias Color string Hex `#RRGGBB` or `#RRGGBBAA`. Strict: no shorthand, no named colours.
---@alias BorderColors { top?: Color, right?: Color, bottom?: Color, left?: Color } Per-edge colours, the one edge table whose values are strings rather than pixels. A signal in an edge is refused: bind `border_color` itself instead.

---@class NodeBase
---@field width? Length|Bound Pixels, or `"Fill"` to take what the parent has left. Omitted means the node sizes to its content.
---@field height? Length|Bound The same, on the cross axis. `"Fill"` on both is how a background covers its parent.
---@field max_width? integer|Bound A ceiling in pixels on a node whose `width` is omitted: it grows with its content up to here and stops. Past it the children overflow, which a `scroll` on the same node is what turns into scrolling. Ignored beside a fixed or `"Fill"` width, which already say how wide.
---@field max_height? integer|Bound The same, on the other axis.
---@field margin? integer|Edges|Bound Outer spacing. A bare number is all four edges.
---@field padding? integer|Edges|Bound Inner spacing. A bare number is all four edges.
---@field align_h? Align|Bound On a stacking parent this places the node in the content box; on a `row` it is read off the row itself as the main-axis distribution and ignored on the children.
---@field align_v? Align|Bound The same two jobs as `align_h`, swapped: main axis on a `column`, cross axis on a `row`.
---@field visible? boolean|Bound `false` keeps the node out of the constraint and paint passes, and out of its parent's spacing.
---@field opacity? number|Bound `[0, 1]`, default `1`. Inherited multiplicatively. Refused outside the range rather than clamped. A node at `0` still lays out and still takes pointer events.
---@field id? string Reconciliation hint, unique among siblings. Not addressable from Lua and has no effect on layout or paint (ADR-0045).
---@field hover? Bound The signal `hover(name)` returned. Marks this node's box as that slot's region.
---@field cursor? Cursor|Bound The shape the pointer takes over this node. Omitted means the node decides by what it is: a `button` with an `on_click` and a link's own words are `"pointer"`, a `textfield` is `"text"`, everything else is the arrow. Set it for the exceptions: `"default"` on a control that is off, `"grab"` on a handle, `"not-allowed"` on something refused. The innermost node under the pointer that says anything wins, so a `cursor` on a card still yields to a link in its body (ADR-0107).
---@field on_hover? fun(hovered: boolean) Fires once when the pointer moves into this node's box and once when it leaves, not per motion event -- and only for a pointer that moved: a surface mapping under a resting pointer, or a list scrolling a new row under one, updates `hover(name)` but fires nothing, since the user did not cross anything (ADR-0112). Requires a `hover` slot on the same node and is refused without one: that signal is what remembers whether the node was hovered last pass, so it is also what tells one node's crossings from another's. Read `hover_rect(name)` for where the crossing happened. This is the only way to *do* something on hover -- `hover(name)` alone changes what is drawn, and a `computed` may not have side effects.

---The fill and border shared by box-painting nodes (`rect`, `row`, `column`, `button`) and all four
---surface roles. `row` and `column` add no paint properties beyond `rect`'s; a surface root paints
---the same way.
---@class BoxBase
---@field background? Color|Bound Omitted means no fill at all, which differs from `#00000000`: the first draws nothing, the second draws a transparent rectangle.
---@field radius? integer|Bound Corner rounding, default `0`.
---@field border_color? Color|BorderColors|Bound A bare string applies to all four edges. No default: an edge paints only where both a colour and a non-zero width say so.
---@field border_width? integer|Edges|Bound A bare number applies to all four edges. Default `0`.
---@field clip? "Box"|"Rounded"|Bound What this node cuts its children down to. Default `"Box"`, its rectangle with square corners, which is what a node has always done. `"Rounded"` uses `radius` instead, so a child overflowing a pill is cut by the same arc the pill's fill draws. Costs an offscreen pass, which is why `radius` alone does not imply it.

---@class RectProps: NodeBase, BoxBase
---@field children? Node[] Drawn in order. A hole in the array truncates it, since `#` is undefined on a sparse table.

---@class RowProps: NodeBase, BoxBase
---@field spacing? integer|Bound Pixels between siblings. A hidden child costs nothing, including its gap.
---@field children? Node[] Drawn in order. A hole in the array truncates it, since `#` is undefined on a sparse table.
---@field scroll? Bound The signal `scroll(name)` returned. Makes this a viewport its children move inside.

---@class ColumnProps: NodeBase, BoxBase
---@field spacing? integer|Bound Pixels between siblings, on the vertical axis here.
---@field children? Node[] Drawn in order. A hole in the array truncates it, since `#` is undefined on a sparse table.
---@field scroll? Bound The signal `scroll(name)` returned. Makes this a viewport its children move inside.

---One `text` content stretch with its own look (ADR-0104), shaped like `NotificationSpan` minus
---`kind` and `href`; body text spans pass through as received. An image span has no `text` and is
---refused, so leave it out.
---`bold` and `italic` use the family's own faces when fontconfig finds them, else regular.
---@class TextRun
---@field text string The run's text. An empty run is skipped.
---@field bold? boolean
---@field italic? boolean
---@field underline? boolean A rule just under the baseline, in the run's colour.
---@field color? Color This run's colour instead of the node's `foreground`. What a link is drawn in.
---@field href? string What a press on this run hands the node's `on_link` (ADR-0106). Carried, never opened by the engine; a body span's `href` goes straight here.

---@class TextProps: NodeBase
---@field content? string|TextRun[]|Bound One string, or an array of runs whose texts are joined and drawn in one paragraph, wrapping and eliding together (ADR-0104). Default `""`, so a text bound to a capability renders empty until the first push rather than failing at boot.
---@field font_size? integer|Bound Default `12`.
---@field font? string|Bound Font family for this node, e.g. `"JetBrainsMono Nerd Font Mono"` (ADR-0144). Absent draws in the `fonts` chain, which is most nodes. The family leads and that chain stays behind it, so CJK and emoji still resolve. Resolved on first sight through fontconfig, exactly as a `fonts` entry is; a family nothing on the system answers draws in the declared chain and says so once on stderr, since the engine cannot tell a typo from an uninstalled font.
---@field foreground? Color|Bound Default opaque white.
---@field elide? "None"|"End"|Bound `"End"` drops trailing characters until the run plus an ellipsis fits. A no-op on a `Content`-sized box, which was measured from this same string. Default `"None"`. Under `wrap = "Word"` it applies to the last line kept rather than to the whole run.
---@field wrap? "None"|"Word"|Bound `"Word"` breaks an over-wide run onto further lines, at a word boundary where there is one and mid-word for a word wider than the box. Default `"None"`, one line however long. A `Content`-sized box has no width to break against, so wrapping needs an explicit `width`, a `"Fill"`, or a stretched cross axis.
---@field max_lines? integer|Bound How many lines `wrap = "Word"` may use. `0` and absent both mean no limit, so an expander is `max_lines = expanded:map(function(e) return e and 0 or 2 end)`. Ignored without `wrap`, since an unwrapped run has one line to begin with.
---@field text_align? "Start"|"Center"|"End"|Bound Where the glyph run sits inside this node's own box, which is a different question from `align_h`. Only visible when the box is wider than the text. Default `"Start"`.
---@field on_link? fun(href: string) A click on a run carrying an `href` (ADR-0106). Wins over any `button` above this node, so a link inside a clickable card opens the page and does not also fire the card; a click on the plain words falls through to the card as before.

---@class IconProps: NodeBase
---@field name? string|Bound A theme name, or an absolute path used as that path. Resolved in the renderer (ADR-0054).
---@field size? integer|Bound Bounding box diameter, default `12`.
---@field foreground? Color|Bound What a `currentColor` fill in the resolved SVG resolves to, which is what CSS `color` means (ADR-0072). A symbolic icon is drawn in this colour; a full-colour app icon names no `currentColor` and ignores it, so it is safe to pass unconditionally. Omitted leaves the file's own colours alone, which for a KDE symbolic icon means the near-black its stylesheet ships.

---@class ImageProps: NodeBase
---@field source? string|Bound An absolute path. Never a theme name; that is `icon`'s job.
---@field fit? "cover"|"contain"|"stretch"|Bound Default `"cover"`. An image has no intrinsic size and takes the box `width`/`height` give it.
---@field async? boolean|Bound Default `false`, which decodes the file inside the frame that first draws it, so the frame is whole: right for a wallpaper, whose first paint is what the swap waits on. `true` decodes on a worker pool and draws nothing until the pixels land, then repaints (ADR-0122): for a grid of thumbnails, where forty inline decodes would freeze the shell for a second. Either way a raster is stored scaled down to cover its box, so a 4K file drawn as a tile costs a tile's worth of texture.

---@class ButtonProps: NodeBase, BoxBase
---@field children? Node[] Drawn in order. A hole in the array truncates it, since `#` is undefined on a sparse table.
---@field submit? boolean A click also sends the surface's armed `secure_submit` field, as Enter would (ADR-0114). The one way a button reaches a password, since no callback may; clickable with or without `on_click`.
---@field on_click? fun(rect: Rect, button: "left"|"right"|"middle") Fires on the release, and only when the release lands on the same node and the same button the press armed. A handler declaring one parameter still works.
---@field on_drag? fun(rect: Rect, pointer: { x: number, y: number }, phase: "start"|"move"|"end") A left press on this button holds the drag until its release (ADR-0116). `"start"` on the press, `"move"` on every motion while held, wherever the pointer has gone, `"end"` on the release or when the pointer leaves the surface. `pointer` is in the button's own coordinates and unclamped, so `pointer.x / rect.width` is the fraction along a horizontal track and `math.min(1, math.max(0, ...))` is the config's own clamp. The left click still fires on release inside the rect, so a control can take both.
---@field on_wheel? fun(rect: Rect, steps: number) One wheel event over this button, innermost against any scrollable container above or below it (ADR-0116). `steps` is in notches, positive away from the user; a touchpad swipe arrives as fractions of a notch. Vertical axis only.

---@class ListProps: NodeBase
---@field source any[]|Bound A flat array table, or a signal wrapping one. A literal array is legal and stays fixed; the signal is what makes the list rebuild.
---@field itemfn fun(item: any): Node Built for every element.
---@field key? fun(item: any): string Maps an element to a stable string. Items reconcile by key, so inserting one rebuilds one. Duplicate keys are an error. Without it items match by index and an insertion rebuilds everything after it.
---@field direction? "Vertical"|"Horizontal"|Bound Default `"Vertical"`. Which way the generated items stack.
---@field spacing? integer|Bound Pixels between generated items, along `direction`.
---@field scroll? Bound The signal `scroll(name)` returned. Makes this a viewport its children move inside.

---@class TextfieldProps: NodeBase
---Two field kinds share this node (ADR-0092). `secure_submit` makes it masked: keystrokes stay in a
---native buffer and go to a capability; no Lua value holds them (ADR-0005). `on_change`/`on_submit`
---make it plain, handing every edit straight to the callback. Both declarations remain masked; neither means never
---focus it, since nothing could read its input.
---
---Both read `wl_keyboard`, not `zwp_text_input_v3`, so neither composes CJK or dead keys.
---Text-input-v3 produces nothing without a compositor-side input method, silently swallowing a
---bare-session reply.
---
---A press focuses a plain field; its surface needs `panel.keyboard_interactivity`.
---Its draft lasts
---as long as the node (ADR-0108): keyboard exit or another press stops input and hides the caret,
---but keeps the draft; pressing the field again resumes. Only Escape with `on_cancel`, submit, or
---node removal clears it.
---@field placeholder? string|Bound Drawn in the foreground colour while the field is empty. Not the value: submitting an untouched field submits an empty string. A focused plain field shows a caret instead, so that "empty" and "empty and typing into it" do not look alike.
---@field mask_character? string|Bound Capped at 1 byte. Hides typed input.
---@field secure_submit? { capability: string, action: string } Only meaningful alongside `mask_character`; without it a masked field's value is unreadable from Lua entirely (ADR-0005, ADR-0027).
---@field on_change? fun(text: string) The whole text after each edit, not the delta. Per keystroke, since there is no input method to batch composition.
---@field on_submit? fun(text?: string) Enter. Takes the whole text and leaves the field focused and empty, so a reply box takes the next message without another click. Fires with no argument when both `mask_character` and `secure_submit` are set.
---@field autofocus? boolean A plain field that takes the keyboard the moment its surface does, with no press, and takes it empty: a search box that must be typable the instant a launcher opens. Every arm calls `on_change("")`, which is the config's "the field just opened" moment -- reset a selection or scroll a list to the top in it. Armed on the keyboard entering the surface and again whenever the tree changes under a focus already held, so a field that appears inside an open panel is covered too. Never while another plain field on the surface is typing, never over a `secure_submit` field, and never to re-take a field a press elsewhere just stopped -- that press was the answer. Two on one surface: the first in document order wins (ADR-0112).
---@field on_navigate? fun(key: "up"|"down"|"page_up"|"page_down"|"tab"|"backtab") An arrow, paging or Tab key while a plain field is typing. The keys a single-line field has no edit for, handed to the config by name so a list drawn under the field can move its selection; the text and caret stay put and `on_change` does not fire. Fires on key repeat too, so a held Down keeps moving. Without it these keys do nothing, as before (ADR-0112).
---@field on_cancel? fun() Escape, on a plain field. The buffer is cleared (`on_change("")` fires first if there was text), the field gives up the keyboard, and then this runs -- so it is safe to remove the field or drop the surface's `keyboard_interactivity` in here. Without it Escape clears and *keeps* the focus, since a config that cannot be told the field let go must not have it let go silently (ADR-0092, ADR-0102).
---@field font_size? integer|Bound Default `12`. Applies to the placeholder and to the masked content alike.
---@field foreground? Color|Bound Default opaque white.
---@field text_align? "Start"|"Center"|"End"|Bound Where the run sits inside the field's own box.

---@param props RectProps
---@return Node
function rect(props) end

---@param props RowProps
---@return Node
function row(props) end

---@param props ColumnProps
---@return Node
function column(props) end

---@param props TextProps
---@return Node
function text(props) end

---@param props IconProps
---@return Node
function icon(props) end

---@param props ImageProps
---@return Node
function image(props) end

---@param props ButtonProps
---@return Node
function button(props) end

---@param props ListProps
---@return Node
function list(props) end

---@param props TextfieldProps
---@return Node
function textfield(props) end
