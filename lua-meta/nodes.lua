---@meta
-- The eight geometric nodes (`oblisk-idl-api-specs.md` § 5.2) and the properties every one of them
-- shares (§ 5.1).
--
-- HAND-WRITTEN. `just stubs` does not touch this file. Of the five in `lua-meta`, only `oblisk.lua`
-- is generated, because a capability payload is a real `Serialize` struct to derive from. A node's
-- schema is 29 scattered `properties.get("...")` calls across `renderer/src/layout/node/`, so it is
-- control flow rather than data and there is nothing to derive from.
--
-- Two things keep it honest instead. `renderer/src/lua/nodes.rs`'s `meta_stub_tests` matches the
-- constructor roster and every kind's `---@field` names against `accepted_properties`, so a
-- property added to the engine and forgotten here fails the build. `just types` runs the language
-- server over `dev-config` with these declarations, so a *type* that is wrong shows up as a
-- diagnostic on working config code (ADR-0081).
--
-- The `---@param props` and `---@return Node` on each constructor carry no prose on purpose. The
-- type is the whole content of the sentence, and twelve copies of "the properties above" is noise.
--
-- Every property here accepts a signal in place of a literal, and every union spells it out as
-- `Bound`. The engine resolves the handle once per pass (`node::resolve_properties`) and then
-- applies that property's normal rules to the result, so `radius = someSignal` is as ordinary as
-- `radius = 8`.
--
-- The unions used to name `Signal` only on the properties a config reaches for most, on the theory
-- that spelling it everywhere would drown the useful types. That was wrong in a way worth writing
-- down: `lua-meta` is what the language server reads, so a union that omits it is not a readable
-- simplification, it is a red squiggle under working config code. Measured against
-- `lua-language-server --check`, 21 properties refused a binding the engine takes.
--
-- They then spelled it `Signal`, and that was wrong in the opposite direction, which cost more.
-- A `---@class` accepts any table-shaped value in a union, so `string|Signal` on `text.content`
-- accepted every payload table in the IDL -- a notification's `body` span array included, which
-- reached the engine and froze the shell on its last good scene. `Bound` is `userdata`, the one
-- spelling that refuses a table, and it is what a signal actually is at runtime. See
-- `signals.lua`'s [`Signal`] for the measurements.
--
-- Two kinds of exception, and both are properties the engine really does refuse. `id`, `hover`,
-- `scroll` and a `panel`'s `layer`/`anchor`/`monitor`/`namespace` are structural: `resolve_properties`
-- passes them through raw, because they are identities rather than values and an identity does not
-- resolve. `hover` and `scroll` still take a handle, so they are `Bound`; the rest name no binding
-- at all and that is correct. The `on_*`/`itemfn`/`key` callbacks name none either, for a duller
-- reason: a signal there resolves to whatever it holds and is then refused for not being a
-- function, so declaring the union would be true and useless.

---@alias Node table A node table, as one of the constructors below returns it.
---@alias Align "Start"|"Center"|"End"|"Stretch"
---@alias Edges { top?: integer, right?: integer, bottom?: integer, left?: integer }
---@alias Length integer|"Fill" Pixels in `[0, 8192]`, or fill the available space.
---@alias Color string Hex `#RRGGBB` or `#RRGGBBAA`. Strict: no shorthand, no named colours.

---@class NodeBase
---@field width? Length|Bound Pixels, or `"Fill"` to take what the parent has left. Omitted means the node sizes to its content.
---@field height? Length|Bound The same, on the cross axis. `"Fill"` on both is how a background covers its parent.
---@field margin? Edges|Bound Outer spacing.
---@field padding? Edges|Bound Inner spacing.
---@field align_h? Align|Bound On a stacking parent this places the node in the content box; on a `row` it is read off the row itself as the main-axis distribution and ignored on the children.
---@field align_v? Align|Bound The same two jobs as `align_h`, swapped: main axis on a `column`, cross axis on a `row`.
---@field visible? boolean|Bound `false` keeps the node out of the constraint and paint passes, and out of its parent's spacing.
---@field opacity? number|Bound `[0, 1]`, default `1`. Inherited multiplicatively. Refused outside the range rather than clamped. A node at `0` still lays out and still takes pointer events.
---@field id? string Reconciliation hint, unique among siblings. Not addressable from Lua and has no effect on layout or paint (ADR-0045).
---@field hover? Bound The signal `hover(name)` returned. Marks this node's box as that slot's region.

---The fill and the border, taken by every kind that paints as a box: `rect`, `row`, `column`,
---`button`, and all four surface roles. `row` and `column` have no paint properties of their own
---beyond a `rect`'s, and a surface root paints exactly like one.
---@class BoxBase
---@field background? Color|Bound Omitted means no fill at all, which differs from `#00000000`: the first draws nothing, the second draws a transparent rectangle.
---@field radius? integer|Bound Corner rounding, default `0`.
---@field border_color? Color|Edges A bare string applies to all four edges. No default: an edge paints only where both a colour and a non-zero width say so.
---@field border_width? integer|Edges A bare number applies to all four edges. Default `0`.
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

---@class TextProps: NodeBase
---@field content? string|Bound Default `""`, so a text bound to a capability renders empty until the first push rather than failing at boot.
---@field font_size? integer|Bound Default `12`.
---@field foreground? Color|Bound Default opaque white.
---@field elide? "None"|"End"|Bound `"End"` drops trailing characters until the run plus an ellipsis fits. A no-op on a `Content`-sized box, which was measured from this same string. Default `"None"`. Under `wrap = "Word"` it applies to the last line kept rather than to the whole run.
---@field wrap? "None"|"Word"|Bound `"Word"` breaks an over-wide run onto further lines, at a word boundary where there is one and mid-word for a word wider than the box. Default `"None"`, one line however long. A `Content`-sized box has no width to break against, so wrapping needs an explicit `width`, a `"Fill"`, or a stretched cross axis.
---@field max_lines? integer|Bound How many lines `wrap = "Word"` may use. `0` and absent both mean no limit, so an expander is `max_lines = expanded:map(function(e) return e and 0 or 2 end)`. Ignored without `wrap`, since an unwrapped run has one line to begin with.
---@field text_align? "Start"|"Center"|"End"|Bound Where the glyph run sits inside this node's own box, which is a different question from `align_h`. Only visible when the box is wider than the text. Default `"Start"`.

---@class IconProps: NodeBase
---@field name? string|Bound A theme name, or an absolute path used as that path. Resolved in the renderer (ADR-0054).
---@field size? integer|Bound Bounding box diameter, default `12`.
---@field foreground? Color|Bound What a `currentColor` fill in the resolved SVG resolves to, which is what CSS `color` means (ADR-0072). A symbolic icon is drawn in this colour; a full-colour app icon names no `currentColor` and ignores it, so it is safe to pass unconditionally. Omitted leaves the file's own colours alone, which for a KDE symbolic icon means the near-black its stylesheet ships.

---@class ImageProps: NodeBase
---@field source? string|Bound An absolute path. Never a theme name; that is `icon`'s job.
---@field fit? "cover"|"contain"|"stretch"|Bound Default `"cover"`. An image has no intrinsic size and takes the box `width`/`height` give it.

---@class ButtonProps: NodeBase, BoxBase
---@field children? Node[] Drawn in order. A hole in the array truncates it, since `#` is undefined on a sparse table.
---@field on_click? fun(rect: Rect, button: "left"|"right"|"middle") Fires on the release, and only when the release lands on the same node and the same button the press armed. A handler declaring one parameter still works.

---@class ListProps: NodeBase
---@field source Bound Must wrap a flat array table.
---@field itemfn fun(item: any): Node Built for every element.
---@field key? fun(item: any): string Maps an element to a stable string. Items reconcile by key, so inserting one rebuilds one. Duplicate keys are an error. Without it items match by index and an insertion rebuilds everything after it.
---@field direction? "Vertical"|"Horizontal"|Bound Default `"Vertical"`. Which way the generated items stack.
---@field spacing? integer|Bound Pixels between generated items, along `direction`.
---@field scroll? Bound The signal `scroll(name)` returned. Makes this a viewport its children move inside.

---@class TextfieldProps: NodeBase
---A `textfield` parses and lays out, but nothing delivers keystrokes to it yet: `zwp_text_input_v3`
---is unwired, so neither callback below has ever fired. The properties are typed to ADR-0027's
---settled wire shape so a config written against them keeps working when the protocol lands.
---@field placeholder? string|Bound Drawn in the foreground colour while the field is empty. Not the value: submitting an untouched field submits an empty string.
---@field mask_character? string|Bound Capped at 1 byte. Hides typed input.
---@field secure_submit? { capability: string, action: string } Only meaningful alongside `mask_character`; without it a masked field's value is unreadable from Lua entirely (ADR-0005, ADR-0027).
---@field on_change? fun(text: string) Per committed edit batch from `wp-text-input-v3`, not per keystroke.
---@field on_submit? fun(text?: string) Takes the committed text, except when both `mask_character` and `secure_submit` are set, when it fires with no argument.
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
