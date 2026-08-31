---@meta
-- The eight geometric nodes (`oblisk-idl-api-specs.md` § 5.2) and the properties every one of them
-- shares (§ 5.1).
--
-- Every property here accepts a `Signal` in place of a literal, whether or not its type spells the
-- union out. The engine resolves the handle at layout time and then applies that property's normal
-- rules to the result. The unions below name `Signal` on the properties a config reaches for most,
-- because spelling it on all of them would drown the useful types.

---@alias Node table A node table, as one of the constructors below returns it.
---@alias Align "Start"|"Center"|"End"|"Stretch"
---@alias Edges { top?: integer, right?: integer, bottom?: integer, left?: integer }
---@alias Length integer|"Fill" Pixels in `[0, 8192]`, or fill the available space.
---@alias Color string Hex `#RRGGBB` or `#RRGGBBAA`. Strict: no shorthand, no named colours.

---@class NodeBase
---@field width? Length|Signal
---@field height? Length|Signal
---@field margin? Edges Outer spacing.
---@field padding? Edges Inner spacing.
---@field align_h? Align On a stacking parent this places the node in the content box; on a `row` it is read off the row itself as the main-axis distribution and ignored on the children.
---@field align_v? Align The same two jobs as `align_h`, swapped: main axis on a `column`, cross axis on a `row`.
---@field visible? boolean|Signal `false` keeps the node out of the constraint and paint passes, and out of its parent's spacing.
---@field opacity? number|Signal `[0, 1]`, default `1`. Inherited multiplicatively. Refused outside the range rather than clamped. A node at `0` still lays out and still takes pointer events.
---@field id? string Reconciliation hint, unique among siblings. Not addressable from Lua and has no effect on layout or paint (ADR-0045).
---@field hover? Signal The signal `hover(name)` returned. Marks this node's box as that slot's region.

---@class RectProps: NodeBase
---@field background? Color|Signal Omitted means no fill at all, which differs from `#00000000`: the first draws nothing, the second draws a transparent rectangle.
---@field radius? integer Corner rounding, default `0`.
---@field border_color? Color|Edges A bare string applies to all four edges. No default: an edge paints only where both a colour and a non-zero width say so.
---@field border_width? integer|Edges A bare number applies to all four edges. Default `0`.
---@field children? Node[]

---@class RowProps: NodeBase
---@field spacing? integer Pixels between siblings. A hidden child costs nothing, including its gap.
---@field children? Node[]
---@field scroll? Signal The signal `scroll(name)` returned. Makes this a viewport its children move inside.

---@class ColumnProps: NodeBase
---@field spacing? integer
---@field children? Node[]
---@field scroll? Signal

---@class TextProps: NodeBase
---@field content? string|Signal Default `""`, so a text bound to a capability renders empty until the first push rather than failing at boot.
---@field font_size? integer Default `12`.
---@field foreground? Color|Signal Default opaque white.
---@field elide? "None"|"End" `"End"` drops trailing characters until the run plus an ellipsis fits. A no-op on a `Content`-sized box, which was measured from this same string. Default `"None"`.
---@field text_align? "Start"|"Center"|"End" Where the glyph run sits inside this node's own box, which is a different question from `align_h`. Only visible when the box is wider than the text. Default `"Start"`.

---@class IconProps: NodeBase
---@field name? string|Signal A theme name, or an absolute path used as that path. Resolved in the renderer (ADR-0054). Carries no tint: use a glyph in a `text` node to colour by state.
---@field size? integer Bounding box diameter, default `12`.

---@class ImageProps: NodeBase
---@field source? string|Signal An absolute path. Never a theme name; that is `icon`'s job.
---@field fit? "cover"|"contain"|"stretch" Default `"cover"`. An image has no intrinsic size and takes the box `width`/`height` give it.

---@class ButtonProps: NodeBase
---@field children? Node[]
---@field background? Color|Signal
---@field radius? integer
---@field on_click? fun(rect: Rect, button: "left"|"right"|"middle") Fires on the release, and only when the release lands on the same node and the same button the press armed. A handler declaring one parameter still works.

---@class ListProps: NodeBase
---@field source Signal Must wrap a flat array table.
---@field itemfn fun(item: any): Node Built for every element.
---@field key? fun(item: any): string Maps an element to a stable string. Items reconcile by key, so inserting one rebuilds one. Duplicate keys are an error. Without it items match by index and an insertion rebuilds everything after it.
---@field direction? "Vertical"|"Horizontal" Default `"Vertical"`. Which way the generated items stack.
---@field spacing? integer
---@field scroll? Signal

---@class TextfieldProps: NodeBase
---A `textfield` parses and lays out, but nothing delivers keystrokes to it yet: `zwp_text_input_v3`
---is unwired, so neither callback below has ever fired. The properties are typed to ADR-0027's
---settled wire shape so a config written against them keeps working when the protocol lands.
---@field placeholder? string
---@field mask_character? string Capped at 1 byte. Hides typed input.
---@field secure_submit? { capability: string, action: string } Only meaningful alongside `mask_character`; without it a masked field's value is unreadable from Lua entirely (ADR-0005, ADR-0027).
---@field on_change? fun(text: string) Per committed edit batch from `wp-text-input-v3`, not per keystroke.
---@field on_submit? fun(text?: string) Takes the committed text, except when both `mask_character` and `secure_submit` are set, when it fires with no argument.

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
