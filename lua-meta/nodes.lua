---@meta
-- The nine geometric nodes and their shared properties.
--
-- HAND-WRITTEN. `just stubs` does not touch it. Of five `lua-meta` files, only `obelisk.lua` is
-- generated: capability payloads are `Serialize` structs, while a node's schema is scattered
-- `properties.get("...")` calls in `renderer/src/`, so it is not derivable data.
--
-- `renderer/src/lua/nodes.rs`'s `meta_stub_tests` matches constructors and each kind's `---@field`
-- names against `accepted_properties`; a property added to the engine but omitted here fails the
-- build. `just types` runs the language server over `dev-config` against these declarations, so a
-- wrong type here surfaces as a diagnostic on working config code (ADR-0081).
--
-- Constructor `---@param props`/`---@return Node` lines stay bare: the type is the sentence, and
-- a copy of "the properties above" per constructor adds nothing.
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
---@alias Axes { x?: number, y?: number } An `{ x, y }` pair; an absent axis takes the property's default.
---@alias EasingName "Linear"|"InQuad"|"OutQuad"|"InOutQuad"|"InCubic"|"OutCubic"|"InOutCubic"|"InQuart"|"OutQuart"|"InOutQuart"|"InQuint"|"OutQuint"|"InOutQuint"|"InSine"|"OutSine"|"InOutSine"|"InExpo"|"OutExpo"|"InOutExpo"|"InCirc"|"OutCirc"|"InOutCirc"|"InBack"|"OutBack"|"InOutBack"|"InElastic"|"OutElastic"|"InOutElastic"|"InBounce"|"OutBounce"|"InOutBounce" QML's `Easing.Type` names without the prefix. `Back`, `Elastic` and `Bounce` overshoot and are clamped to the property's range.
---@alias Easing EasingName|[number, number, number, number]|{ steps: integer } A name, CSS `cubic-bezier(x1, y1, x2, y2)` as four numbers with `x1` and `x2` within `[0, 1]`, or `{ steps = n }` for `n` jumps that land on the target only at the end (ADR-0151).
---@alias Keyframe number|string|Edges|Axes|{ value: number|string|Edges|Axes, duration?: integer, easing?: Easing } One stop in a `keyframes` list: a bare value taking the entry's timing, or a table naming its own. A `duration` of `0` is a jump rather than a stop (QML's `PropertyAction`), and a frame repeating the value before it is a hold (its `PauseAnimation`).
---@alias Spring { stiffness: number, damping: number } A mass on a spring, in place of a duration and an easing (ADR-0154). `stiffness` is the pull toward the target, within `(0, 100000]`; `damping` is the drag on the way, within `(0, 10000]`, and `2 * math.sqrt(stiffness)` is where it stops overshooting. Both are required and there is no `mass`: it divides out of the two. A spring carries its speed through a change of target, which no easing can do.
---@alias Animation integer|{ duration: integer, delay?: integer, easing?: Easing, from?: number|string|Edges, spring?: Spring, keyframes?: Keyframe[], loops?: integer|"Infinite" } A duration in milliseconds, `[1, 60000]`, with `InOutQuad` when no easing is named. `delay` is how long the property holds still first, `[0, 60000]` ms and zero by default, which is CSS's `transition-delay` (ADR-0153); on a sequence it offsets the whole run, not each cycle. `spring` replaces `duration` and `easing` rather than joining them; a `duration`, `easing`, `loops` or `keyframes` beside one is refused, and so is `loops` without `keyframes`. `from` is where a node that has never displayed the property starts: its entry animation, absent meaning the first value is taken as it is (ADR-0146). `keyframes` walks the property through at least two values instead of easing it to the one a pass resolved, `loops` times or forever (ADR-0152); at least one segment must last, since a list of nothing but jumps takes no time to walk; the entry's own presence is what starts and stops it, so bind `animate` itself to gate one. A sequence starts on its own first frame, so `from` has nothing to say beside one and naming both is refused.
---@alias Animations table<string, Animation> Which of this node's properties ease between values, and how. Any property the node has may be named; what its value is decides whether it tweens: a number, a `"NN%"` size, a hex colour or an edge table of numbers eases against a value of the same shape, and anything else (`"Fill"`, a boolean, a table of colours, a shape change) snaps.
---@alias Exit { duration: integer, delay?: integer, easing?: Easing, [string]: any } The one key of `animate` that is not a property name: a shared duration, `delay` and easing, plus the value each named property eases to once a pass stops returning the node (ADR-0150). Each target starts from what the node displays, or from the property's identity when it never set one (`1` for `opacity` and `scale`, `0` for the rest), so `exit = { duration = 150, opacity = 0 }` fades out whatever the node was showing.

---@class NodeBase
---@field width? Length|Bound Pixels, or `"Fill"` to take what the parent has left. Omitted means the node sizes to its content.
---@field height? Length|Bound The same, on the cross axis. `"Fill"` on both is how a background covers its parent.
---@field max_width? integer|Bound A ceiling in pixels on a node whose `width` is omitted: it grows with its content up to here and stops. Past it the children overflow, which a `scroll` on the same node is what turns into scrolling. Ignored beside a fixed or `"Fill"` width, which already say how wide.
---@field max_height? integer|Bound The same, on the other axis.
---@field min_width? integer|Bound A floor in pixels on a node whose `width` is omitted: it never measures narrower than this, however little it holds. What keeps a card sized by its own words looking like a card when the words are two of them. Ignored beside a fixed or `"Fill"` width; above `max_width` it wins, as in CSS, rather than being refused.
---@field min_height? integer|Bound The same, on the other axis.
---@field margin? integer|Edges|Bound Outer spacing. A bare number is all four edges.
---@field padding? integer|Edges|Bound Inner spacing. A bare number is all four edges.
---@field align_h? Align|Bound On a stacking parent this places the node in the content box; on a `row` it is read off the row itself as the main-axis distribution and ignored on the children.
---@field align_v? Align|Bound The same two jobs as `align_h`, swapped: main axis on a `column`, cross axis on a `row`.
---@field visible? boolean|Bound `false` keeps the node out of the constraint and paint passes, and out of its parent's spacing.
---@field opacity? number|Bound `[0, 1]`, default `1`. Inherited multiplicatively. Refused outside the range rather than clamped. A node at `0` still lays out and still takes pointer events.
---@field scale? number|Axes|Bound A paint-only scale about `origin` (ADR-0149): one factor for both axes, or `{ x, y }` with an absent axis at `1`. `[0, 64]`. Layout, `geometry` and siblings see the unscaled box; hit-testing and input regions follow the painted one.
---@field rotate? number|Bound Degrees clockwise about `origin`, paint-only.
---@field translate? Axes|Bound `{ x, y }` logical pixels the painted node is shifted by, after `scale` and `rotate`. Paint-only.
---@field origin? Axes|Bound Where `scale` and `rotate` pivot, as fractions of the node's box; default `{ x = 0.5, y = 0.5 }`, its centre.
---@field animate? Animations|Bound QML's `Behavior on x`: when a pass resolves a new value for a named property, the node eases from what it shows to the new value over the duration instead of snapping, and keeps easing between passes without running any Lua (ADR-0145). Only a node that already exists animates; a first value is taken as it is. Naming a property no tween carries is refused. The key `exit` is an `Exit` block rather than a duration: it is what the node eases to on its way out, kept painted but out of the layout until the tweens finish (ADR-0150).
---@field id? string Reconciliation hint, unique among siblings. Not addressable from Lua and has no effect on layout or paint (ADR-0045).
---@field hover? Bound The signal `hover(name)` returned. Marks this node's box as that slot's region.
---@field geometry? Bound The signal `geometry(name)` returned. The layout pass writes this node's absolute rect into it (ADR-0147).
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
---@field blur? boolean|Bound Ask the compositor to blur the desktop behind this node's box (ADR-0195). Default `false`, and never inferred from a translucent `background`: an invisible `#00000000` control is not asking for glass, and border-only or image-backed glass has no background alpha to read. The engine unions every asking node in a surface, following the transforms and clips the node is painted under, so a card that slides, scrolls out of a list, or fades to nothing blurs where it is drawn and nowhere else. Nothing is sent on a compositor without `ext-background-effect-v1`, or one whose blur capability is off, so this is silently nothing rather than an error. Strength, passes and xray belong to the compositor's own configuration and cannot be set from here, which is why this is a boolean.
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

---@class Transition
---@field duration integer How long the cross runs, in ms. Required: a cross with no length is a snap, which `retain` alone already does.
---@field easing? Easing Default `"InOutQuad"`. The curve `u_progress` follows.
---@field shader? string Absolute path to a GLSL ES fragment shader to cross with, instead of the built-in dissolve (ADR-0184). Name one shipped beside `shell.lua` with `obelisk.config_dir .. "/shaders/wipe.frag"`. Recompiled when the file's bytes change, so editing an effect takes a reload and not a restart.
---
--- The engine prepends `#version 300 es`, `highp` precision for floats and samplers, its own declarations and `#line 1`, so the file is a `void main()` and its compile errors carry its own line numbers. What it gets:
---
--- - `v_uv` -- this node's box, `0..1`, origin top-left, x right and y down.
--- - `u_progress` -- the eased progress, clamped to `0..1`.
--- - `u_size` -- the node's logical size in pixels, for aspect correction.
--- - `obelisk_from(uv)` and `obelisk_to(uv)` -- the outgoing and incoming pictures, premultiplied RGBA, each already placed by its `fit`, and `u_fill` (transparent) outside it. Sampling outside `0..1` is defined and returns that fill.
--- - `u_from_rect` and `u_to_rect` -- where each picture sits, as node-space `(x, y, width, height)` fractions. Under `"cover"` the origin is negative and the extent above one, because the picture is larger than the box that crops it.
---
--- Write premultiplied RGBA to `fragColor` in the encoded colour space the images arrive in; no linear-light conversion happens either side. The node's inherited `opacity` is applied by the engine after your `main` returns, so it cannot be got wrong, and the result is composited source-over under the node's clip and transform like any other draw.
---
--- Identifiers beginning `u_` or `obelisk_` are the engine's. A shader that will not compile, will not link, or declares a non-`float` parameter is reported once and that transition falls back to the built-in cross-dissolve, so a mistake costs an effect and not a frame. A shader that compiles and loops forever hangs the GPU and with it the session: this is the config's own code at the same trust level as `process.run`, and nothing sandboxes it.
---@field params? table<string, number> Values for the `uniform float`s the shader declares, by name. Finite numbers only, which is every type a config needs to parametrise an effect. Every parameter the compiled shader has is set on every draw, so one omitted here is `0` rather than whatever another node using the same shader last set. A name the shader has no uniform for is ignored, since a shader may declare one and never use it. Refused without a `shader` to reach.

---@class ImageProps: NodeBase
---@field source? string|Bound An absolute path. Never a theme name; that is `icon`'s job.
---@field fit? "cover"|"contain"|"stretch"|Bound Default `"cover"`. An image has no intrinsic size and takes the box `width`/`height` give it.
---@field async? boolean|Bound Default `false`, which decodes the file inside the frame that first draws it, so the frame is whole: right for a wallpaper, whose first paint is what the swap waits on. `true` decodes on a worker pool and draws nothing until the pixels land, then repaints (ADR-0122): for a grid of thumbnails, where forty inline decodes would freeze the shell for a second. Either way a raster is stored scaled down to cover its box, so a 4K file drawn as a tile costs a tile's worth of texture.
---@field retain? boolean|Bound Default `false`. `true` keeps drawing the source this node last had pixels for while a newly named one decodes, instead of showing the surface behind it (ADR-0180). This is what lets a wallpaper change under `async = true`: the decode leaves the render thread, and the picture on screen holds until the replacement is ready to take over in one frame. It needs a node whose identity survives the change, so give the `image` a stable `id` and change its `source` -- a node keyed by its path is a different node and has nothing to hold. Inert without `async`, since an inline decode leaves no gap. A source that fails to decode leaves the old picture up rather than blanking, and keeps it up: the node moves on only when a paint has the new texture in hand (ADR-0183).
---@field transition? Transition|Bound Cross from the picture the node is holding to the one that just landed, instead of swapping in one frame (ADR-0181). Implies `retain`, which is where the outgoing picture comes from, and like it needs `async = true` and a node whose `id` survives the change. The first picture a node ever shows appears rather than crosses, having nothing to cross from. Without a `shader` the cross is a straight dissolve, run by the engine's own fragment shader so that it composes exactly: a transparent incoming pixel does not show the outgoing one through it, and a node `opacity` below 1 reads the same density mid-cross as at either end (ADR-0186). Name a `shader` for anything else -- a wipe, a disc, a pixelate -- which is a `.frag` file your config owns rather than an effect this engine ships (ADR-0184). A shader that will not build logs once and falls back to the plain dissolve.

---@class ButtonProps: NodeBase, BoxBase
---@field children? Node[] Drawn in order. A hole in the array truncates it, since `#` is undefined on a sparse table.
---@field submit? boolean|Bound A click also sends the surface's armed `secure_submit` field, as Enter would (ADR-0114). The one way a button reaches a password, since no callback may; clickable with or without `on_click`.
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
---@field mask_character? string|Bound Hides typed input behind its first character; `•` by default, nothing for `""`.
---@field secure_submit? { capability: string, action: string }|Bound Only meaningful alongside `mask_character`; without it a masked field's value is unreadable from Lua entirely (ADR-0005, ADR-0027).
---@field on_change? fun(text: string) The whole text after each edit, not the delta. Per keystroke, since there is no input method to batch composition.
---@field on_submit? fun(text: string) Enter. Takes the whole text and leaves the field focused and empty, so a reply box takes the next message without another click. Never fires on a `secure_submit` field, whose Enter goes to the capability.
---@field autofocus? boolean|Bound A plain field that takes the keyboard the moment its surface does, with no press, and takes it empty: a search box that must be typable the instant a launcher opens. Every arm calls `on_change("")`, which is the config's "the field just opened" moment -- reset a selection or scroll a list to the top in it. Armed on the keyboard entering the surface and again whenever the tree changes under a focus already held, so a field that appears inside an open panel is covered too. Never while another plain field on the surface is typing, never over a `secure_submit` field, and never to re-take a field a press elsewhere just stopped -- that press was the answer. Two on one surface: the first in document order wins (ADR-0112).
---@field on_navigate? fun(key: "up"|"down"|"page_up"|"page_down"|"tab"|"backtab") An arrow, paging or Tab key while a plain field is typing. The keys a single-line field has no edit for, handed to the config by name so a list drawn under the field can move its selection; the text and caret stay put and `on_change` does not fire. Fires on key repeat too, so a held Down keeps moving. Without it these keys do nothing, as before (ADR-0112).
---@field on_cancel? fun(cleared: boolean) Escape, on a plain field. The buffer is cleared (`on_change("")` fires first if there was text), the field gives up the keyboard, and then this runs -- so it is safe to remove the field or drop the surface's `keyboard_interactivity` in here. `cleared` is whether that Escape had text to clear, which is the two-stage Escape a launcher wants: clear on the first press, close on the second. Do not rebuild it from `on_change`, which also fires `""` when `autofocus` arms the field on every open (ADR-0112) and so cannot tell an opened field from a cleared one. Without it Escape clears and *keeps* the focus, since a config that cannot be told the field let go must not have it let go silently (ADR-0092, ADR-0102).
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
