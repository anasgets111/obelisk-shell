//! Typed property parsing for the layout engine (build-steps.md Phase 12,
//! `docs/oblisk-idl-api-specs.md` § 5.1). `renderer/src/lua/nodes.rs`'s `VirtualNode` deliberately
//! left every property as a raw `mlua::Value` -- this module is the "actual consumer that needs
//! typed, validated properties" that file's own doc comment named as Phase 12's job.
//!
//! A `Value::UserData` holding a `Signal` (§ 1.2) is resolved rather than rejected
//! (build-steps.md Phase 19 item 1, ADR-0044 decision 1, `CONTEXT.md`'s Signal resolution entry),
//! and every one of those reads happens in exactly one place: [`resolve_properties`], run once per
//! node per pass (build-steps.md Phase 19 item 5). The parsers below therefore take plain values
//! and no `&Lua` -- they see a map in which every `Signal` has already been read once, so two
//! parsers reading the same property in the same pass see the same signal answer.
//!
//! That is the whole of the guarantee, and it is worth stating narrowly: it covers a `Signal` and
//! nothing else. A plain Lua table carrying an `__index` metamethod is not a `Signal`, so it is
//! copied into the resolved map as the table it is, and every `table.get` a parser makes still
//! runs that metamethod afresh -- a `margin` of that shape reproduces the exact
//! measured-against-one-answer, positioned-against-another defect item 5 names, with no signal
//! involved. See [`parse_edge_insets`]'s `ponytail:` for that hole and what closing it costs.
//!
//! Resolution happens exactly once per property: if a signal's result is itself a `Signal` (a
//! fresh `Value::UserData`), that's an error rather than a second read. That guard only stops a
//! signal resolving directly to another signal; it is not a recursion bound, and it does nothing
//! for a computed signal whose getter returns a fresh table on every call (e.g. a `children`
//! signal that builds new node tables each read), which recurses as deep as the getter wants to go
//! through `resolve_and_reconcile` and `deserialize_lua_table`. That one is bounded elsewhere:
//! `layout::scene::MAX_TREE_DEPTH` caps the recursion and raises [`LayoutError::TreeTooDeep`]
//! (build-steps.md Phase 19 item 3), so it is a rejected config rather than the stack overflow it
//! used to be.
//!
//! [`SurfaceTopology`]'s five fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/
//! `parse_monitor`/`parse_namespace`) and every node's optional `id` (`parse_node_id`,
//! docs/adr/0045 decision 1) are the carve-outs and keep rejecting a `Signal` outright -- see
//! [`reject_signal_in_structural_field`]'s doc comment for why. A `panel`'s remaining § 6.1
//! properties (`keyboard_interactivity`, `exclusive`, `margin`, `width`/`height`) are *not*
//! carve-outs: layer-shell accepts each of them on a live surface, so a `Signal` in one resolves
//! normally.

use std::collections::{HashMap, HashSet};

use mlua::{Lua, Value};

use crate::lua::marshal;
use crate::lua::nodes::{VirtualNode, deserialize_lua_table};
use crate::lua::signal::Signal;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeMode {
    Pixels(f32),
    Percent(f32),
    Content,
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EdgeInsets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl EdgeInsets {
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

/// A parsed colour, four channels in `0.0..=1.0`. `f32`, not `u8`: femtovg's `Color` (the drawing
/// pass's paint target, read by `layout::paint`) stores its channels as `f32` already --
/// `Color::rgbaf` takes them as-is, while `Color::rgba` takes `u8` and immediately divides by
/// 255.0 to reach that same `f32` form internally. Storing `f32` here means the drawing pass
/// copies four fields straight into `Color`, with no `u8` round-trip to undo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("unsupported node kind `{0}`")]
    UnsupportedNodeKind(String),
    #[error("invalid value for `{property}`: {detail}")]
    InvalidProperty { property: String, detail: String },
    #[error(
        "`{0}` is a Signal handle, not a plain value -- read it via :get() before returning it from shell.lua"
    )]
    UnsupportedSignalProperty(String),
    /// build-steps.md Phase 19 item 3: `resolve_and_reconcile`'s recursion, bounded at
    /// `layout::scene::MAX_TREE_DEPTH`. Covers both a literal cyclic tree (`r.children = { r }`)
    /// and a computed `children` signal that generates fresh depth on every read -- both recurse
    /// through the same Rust call, so one cap catches both (see that constant's doc comment).
    ///
    /// `max` is the number of levels actually admitted and `depth` is the 1-based level that was
    /// refused, so `depth` is always `max + 1` -- the message states the limit the code enforces,
    /// not one adjacent to it.
    #[error(
        "node tree exceeds the maximum depth of {max} levels (at `{kind}`, level {depth}) -- a node holding itself in `children`?"
    )]
    TreeTooDeep { kind: String, depth: u32, max: u32 },
}

/// `pub(crate)` rather than private since build-steps.md Phase 20 item 4: `layout::scene`'s
/// `Scene::apply_one_instance` raises an `id`-scoped error for an instance naming an undeclared
/// surface, and every other `InvalidProperty` in this crate is built here rather than by hand.
pub(crate) fn invalid(property: &str, detail: impl Into<String>) -> LayoutError {
    LayoutError::InvalidProperty {
        property: property.to_string(),
        detail: detail.into(),
    }
}

/// Longest prefix of a rejected value's `Debug` form this file will ever put in an error message.
/// 200 bytes, not `marshal::MAX_STRING_BYTES` (64KB): that cap answers "how much of a *string
/// property* is a legitimate value," a config author's call about the accepted path. This one
/// answers "how much of a rejected value belongs in one line of `rescue`'s `error_log` (§ 2.10),"
/// a much smaller budget -- a human scrolling past it needs enough bytes to recognize the value,
/// not a paste buffer, and even a 64KB truncated dump would still read as a bad log line even
/// though it would no longer reintroduce the pathological allocation this exists to stop.
const MAX_ERROR_VALUE_PREVIEW_BYTES: usize = 200;

/// `pub(crate)` rather than private: `layout::scene`'s `list` node (build-steps.md Phase 19 item
/// 12) rejects a bad `source`/`itemfn`/`key` value from outside this module and needs the same
/// bounded preview, not a second copy of this truncation logic.
///
/// Renders a `Value` for an [`invalid`] detail without ever formatting its `Debug` form in full
/// first (docs/build-steps.md Phase 19 item 13). `format!("{value:?}")` on an oversized
/// `Value::String` allocates and escapes the whole thing before any truncation could run --
/// `rect { radius = string.rep("x", 20 * 1024 * 1024) }` would format 20 MB on the Wayland
/// dispatch thread before the error even reaches `rescue`. `marshal::check_string`'s 64KB cap
/// never runs on this path, because it lives in [`checked_string`], which a value rejected for
/// having the wrong *type* never reaches.
///
/// Measured on this machine, formatting a Lua string of each size against this function:
///
/// | size | `format!("{value:?}")` | this function |
/// |---|---|---|
/// | 1 MB | 1.64 ms | 0.0088 ms |
/// | 20 MB | 23.96 ms | 0.0057 ms |
/// | 100 MB | 93.88 ms | 0.0061 ms |
///
/// The right-hand column being flat is the point, and is what separates this from a helper that
/// formats first and truncates after: cost here is a function of the cap, not of the input. The
/// left-hand column is why it matters at all. 23.96 ms is more than a whole frame at 60fps, and
/// build-steps.md Phase 19 item 6's third commit made this per-frame rather than per-apply:
/// `layout::paint` is the first thing that ever validates a `background` or a `radius`, and it
/// does so while drawing, on the Wayland dispatch thread. One `background = 5` in one node used
/// to pay that on every frame.
///
/// A timing assertion is not the regression test for this. A naive format-then-truncate still
/// came in under a 50 ms bound at 20 MB, so a threshold loose enough to be stable on other
/// hardware is too loose to catch the bug. What catches it deterministically is that a
/// format-then-truncate cannot report the value's true length, which
/// `oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix` asserts on.
///
/// Checked against mlua 0.12's actual `Debug` impls (`value.rs`, `table.rs`, `function.rs`,
/// `userdata.rs`, `string.rs`, `thread.rs`, `types.rs`) rather than assumed: `Value::String` is
/// the only unbounded case. `Value::fmt`'s non-alternate branch writes `String({s:?})`, and
/// `LuaString`'s own `Debug` formats every byte of the string, escaped, whether as a `str` or as
/// `bstr::BStr`. Every other variant that can hold non-trivial data (`Table`, `Function`,
/// `Thread`, `UserData`) instead derives or hand-writes `debug_tuple(...).field(&self.0)` over a
/// `ValueRef`, whose own `Debug` is `Ref({:p})` -- a fixed-width pointer, regardless of how large
/// the table is or how many upvalues the closure carries. `Integer`/`Number`/`Boolean`/`Nil`/
/// `LightUserData` are already bounded by their own type. So only the `String` arm needs a
/// separate path here; every other variant formats exactly as it always has.
pub(crate) fn preview_for_error(value: &Value) -> String {
    let Value::String(s) = value else {
        return format!("{value:?}");
    };
    // `as_bytes()` borrows the Lua string's own buffer -- no copy, no escaping -- so measuring its
    // length is the O(1) check that has to run *before* any formatting decision, not after.
    let bytes = s.as_bytes();
    let total_len = bytes.len();
    if total_len <= MAX_ERROR_VALUE_PREVIEW_BYTES {
        return format!("{value:?}");
    }
    // Slice first, format second: only the bounded prefix is ever handed to a `Debug`-style
    // formatter, so a 20 MB string costs this function O(200 bytes), not O(len). The prefix is
    // rendered lossily on purpose -- this is a log preview, not an equality key, and slicing raw
    // bytes at a fixed offset can land mid-codepoint.
    let prefix = String::from_utf8_lossy(&bytes[..MAX_ERROR_VALUE_PREVIEW_BYTES]);
    format!(
        "String({prefix:?}...) -- {total_len} bytes total, truncated to the first {MAX_ERROR_VALUE_PREVIEW_BYTES} here"
    )
}

/// Runs a numeric `Value` through the marshalling boundary (`lua::marshal`, ADR-0044 decision 1)
/// before this parser's own application-level range checks (e.g. `parse_size_mode`'s `[0, 8192]`)
/// ever see it -- catches a NaN/Inf `f64` `Number` or an out-of-2^53-range `Integer`, whether it
/// arrived as a literal or came out of resolving a `Signal` via [`resolve_properties`]: both are
/// equally Lua-authored values crossing into Rust, exactly what `marshal::check_number`/
/// `check_integer` were written to guard (`renderer/src/lua/marshal.rs`'s module doc comment).
///
/// `marshal::check_number` alone isn't sufficient here, though: it only guards the `f64`
/// representation, and a finite `f64` like `1e300` sails through it and then overflows to
/// `f32::INFINITY` on the narrowing cast below. A caller with no further range check (e.g.
/// `parse_spacing`) would otherwise hand that `Inf` straight into layout arithmetic -- `inf * 0.0`
/// is `NaN`, and `snap_to_physical`'s final `as i32` silently saturates a `NaN` rect to `0` instead
/// of ever raising an error. So the finiteness check re-runs after the cast, on the `f32`, naming
/// the same property a non-finite literal would.
fn value_as_f32(property: &str, value: &Value) -> Result<Option<f32>, LayoutError> {
    match value {
        Value::Integer(i) => {
            let checked = marshal::check_integer(*i).map_err(|e| invalid(property, e.to_string()))?;
            Ok(Some(checked as f32))
        }
        Value::Number(n) => {
            let checked = marshal::check_number(*n).map_err(|e| invalid(property, e.to_string()))?;
            let narrowed = checked as f32;
            if !narrowed.is_finite() {
                return Err(invalid(
                    property,
                    format!("must be finite, got {checked} which overflows f32 to {narrowed}"),
                ));
            }
            Ok(Some(narrowed))
        }
        _ => Ok(None),
    }
}

/// Runs a Lua string through `marshal::check_string`'s 64KB cap and returns the owned `String` --
/// same "literal and resolved-`Signal` values share one check" reasoning as [`value_as_f32`].
fn checked_string(property: &str, s: &mlua::LuaString) -> Result<String, LayoutError> {
    let s = s.to_string_lossy();
    marshal::check_string(&s).map_err(|e| invalid(property, e.to_string()))?;
    Ok(s)
}

/// Strict `#RRGGBB` / `#RRGGBBAA` hex colour parsing (§ 5.2's `rect.background`, `border_color`,
/// `text.foreground`). No 3-digit shorthand, no named colours, no bare digits without `#` --
/// § 5.2 documents none of them, and accepting one here would commit the project to a convenience
/// syntax the IDL never specified.
fn parse_hex_color(property: &str, s: &str) -> Result<Rgba, LayoutError> {
    let Some(digits) = s.strip_prefix('#') else {
        return Err(invalid(
            property,
            format!("hex colour must start with `#`, got `{s}`"),
        ));
    };
    // Digit check before length check, and in that order deliberately: every byte of a multi-byte
    // UTF-8 sequence is >= 0x80 and so fails `is_ascii_hexdigit`, so non-ASCII input (`"#日本語"`)
    // is caught here with the accurate diagnosis. By the time the length check below runs, the
    // string is known to be pure ASCII, where `.len()` is the character count -- reporting a byte
    // count for `"#日本語"` (9 bytes, 3 characters) would name the wrong number entirely.
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(
            property,
            format!("hex colour must contain only hex digits, got `{s}`"),
        ));
    }
    if digits.len() != 6 && digits.len() != 8 {
        return Err(invalid(
            property,
            format!(
                "hex colour must have 6 or 8 hex digits after `#`, got {} in `{s}`",
                digits.len()
            ),
        ));
    }
    let channel = |range: std::ops::Range<usize>| -> f32 {
        u8::from_str_radix(&digits[range], 16).expect("digits validated as hex above") as f32 / 255.0
    };
    let a = if digits.len() == 8 { channel(6..8) } else { 1.0 };
    Ok(Rgba {
        r: channel(0..2),
        g: channel(2..4),
        b: channel(4..6),
        a,
    })
}

/// Whether `property` is one [`resolve_properties`] copies through untouched on a node of this
/// `kind`, so that [`reject_signal_in_structural_field`] still sees a raw `Value::UserData` and can
/// refuse it -- see that function's doc comment for why these and nothing else. Resolving them
/// there and rejecting afterwards would be unimplementable: once a signal has been read, the value
/// in the map is indistinguishable from a literal.
///
/// Kind-aware, because a skip is only sound where a parser actually runs to do the rejecting.
/// `id` is skipped on every kind: [`parse_node_id`] reads it on every node and [`parse_surface_id`]
/// on a surface root, so a `Signal` in it is refused wherever it appears. `layer`/`anchor`/
/// `monitor`/`namespace` are read by [`surface_topology`] alone, which `layout::scene`'s
/// `Scene::apply_one_surface` calls on top-level surfaces and nowhere else -- so on a `rect` no
/// parser ever looks at them, and skipping them there would copy a live `Signal` handle straight
/// into `layout::scene::RetainedNode::properties` with nothing left to reject it. That would
/// falsify the "never a `Signal`" invariant `layout::scene::ResolvedNode::properties` documents and
/// hands to the paint stage, which is told it may read a colour or a radius off the map directly.
/// Below a surface these four are ordinary properties and resolve like any other.
///
/// `namespace` joins the carve-out for the same protocol reason `monitor` is already in it
/// (`CONTEXT.md`, Topology change): `zwlr_layer_shell_v1::get_layer_surface` fixes a namespace at
/// creation and no request changes it on a live surface, so a value that could drift after the
/// topology diff would leave the compositor matching rules (Hyprland's `layerrule`) against a
/// string the config no longer says. The in-place `panel` fields -- `keyboard_interactivity`,
/// `exclusive`, `margin`, `width`/`height` -- are deliberately *not* here: layer-shell permits
/// changing each of them on a live surface, so a `Signal` in one resolves normally
/// (docs/adr/0044 decision 1) and the next pass simply applies the new value.
fn is_structural_property(kind: &str, property: &str) -> bool {
    property == "id" || (kind == "panel" && matches!(property, "layer" | "anchor" | "monitor" | "namespace"))
}

/// One node's raw property map with every `Signal` replaced by its current value (build-steps.md
/// Phase 19 item 5, ADR-0044 decision 1, `CONTEXT.md`'s Signal resolution entry). Called once per
/// node per pass, at the point that node enters reconciliation; everything downstream -- this
/// module's parsers, `layout::scene`'s sizing and positioning passes, and what the pass stores in
/// `RetainedNode::properties` -- reads the result rather than the raw map.
///
/// Once, and once is load-bearing. `Signal::get_value` runs a `computed` signal's Lua closure, and
/// a closure that is not a pure function of unchanged state (`os.clock()`, `math.random`, an
/// accumulator upvalue) answers differently on every call. `margin` used to be read four separate
/// times in one `Scene::apply` -- the parent's child loop, both `intrinsic_content_size` folds and
/// `position_children` -- so a row could be measured against one answer and position its child
/// against another, breaking the sizing-and-positioning agreement `intrinsic_content_size`'s own
/// comment names. One read per property is what makes the resolved tree a snapshot of one pass
/// instead of four disagreeing reads. It is also what stops ADR-0021's per-`get_value` 5ms budget
/// being bought four times over for one property.
///
/// The snapshot is a snapshot of the *signals*, and only of them. A value that is a plain table
/// with an `__index` metamethod is copied through as that table, and each `table.get` a parser
/// makes runs the metamethod again, so a `margin` of that shape still measures against one answer
/// and positions against another. See [`parse_edge_insets`]'s `ponytail:`.
///
/// This is **not** the memoization ADR-0044 decision 3 rejects. That decision is about caching
/// *across* pushes, which needs an invalidation rule no push has; this caches nothing beyond the
/// single pass it runs in, and the next pass resolves everything again from scratch.
///
/// Per entry:
///
/// - a key [`is_structural_property`] names for this node's `kind` is copied through raw, signal
///   and all;
/// - a `Value::UserData` holding a `Signal` is read through `Signal::get_value` and the *result*
///   stored in its place, under the same rules a parser would then apply to a literal;
/// - a result that is itself a `Signal` is an error naming the property, not a second read:
///   chasing it to a fixed point is an unbounded loop on a cyclic construction;
/// - a result of `Value::Nil` **omits the key entirely**, which is what makes ADR-0044 decision
///   1's amendment ("a signal resolving to nil means the property is absent") fall out of the map
///   itself rather than being re-checked in every parser. Two reasons it is the consistent rule,
///   and the second is why it is not merely a convenience. First: `crate::socket`'s
///   `RendererClient::run_startup_evaluation` runs before `crate::wayland::run`'s poll loop has
///   drained a single inbound frame, so every `shared::CAPABILITIES` signal still reads `nil` at
///   the first `Scene::apply`; without this a config binding a bare capability signal
///   (`visible = audio`), the exact shape decision 1 exists to enable, fails layout at boot and
///   the shell comes up blank. Second: a Lua table cannot store a `nil` value, so `visible = nil`
///   in a config drops the key before it ever reaches `properties`, which makes an explicit `Nil`
///   reachable only through a signal -- treating the two spellings of "no value here" differently
///   would be a distinction no config author could see;
/// - everything else, including a `UserData` that is not a `Signal`, is copied through unchanged.
///   A non-signal userdata is nothing this function knows how to read, so it is left for whichever
///   parser consumes it to reject with its own message.
///
/// Every property resolves, including the ones no parser reads today. That is deliberate, not an
/// oversight: the resolved map is what a later paint stage reads a colour or a radius straight off
/// (`layout::scene::ResolvedNode::properties`), and § 5.1 puts no property out of a `Signal`'s
/// reach, so there is no subset it would be safe to skip. The consequence is stated plainly rather
/// than argued away: a getter that raises fails the whole apply, even for a property nothing
/// downstream would have looked at. That is the same treatment every other bad property value
/// gets, and the alternative is unavailable anyway -- deferring the error to whoever reads the key
/// means keeping the getter around to re-run at that point, which is exactly the second read this
/// function exists to prevent.
///
/// ponytail: one fresh `HashMap` per node per pass, its `String` keys cloned with it. Honestly
/// counted, that is not a new allocation: the code before this built exactly one map per node per
/// pass too, cloning `fresh.properties` into the `RetainedNode`, and the map built here is *moved*
/// into that node rather than cloned again. What stands as the ceiling is that neither shape
/// reuses the retained node's existing allocation across passes, and that costs more now than it
/// used to, because ADR-0044 decision 2's dirty flag makes a pass a per-push event rather than a
/// per-config-edit one, on the Wayland dispatch thread. Resolving in place over the retained map
/// is the upgrade path; it needs the reconcile match to happen before resolution rather than
/// after, which is a reordering not worth doing until a profile names this.
///
/// ponytail: resolving every property means every property *that holds a `Signal`* is evaluated on
/// every pass, paint-only ones included -- `background`, `color` and `radius` are read here even
/// though no parser in this module looks at them yet. Each one is its own `Signal::get_value` and
/// therefore buys its own ADR-0021 5ms budget (docs/adr/0021, § 1.2), so a node with four
/// signal-bound paint properties can spend four budgets in a pass that ADR-0044 decision 2's dirty
/// flag now runs per capability push, on the Wayland dispatch thread. That is the price of the
/// resolved map being a *complete* snapshot rather than a snapshot of the properties layout
/// happens to consume; the alternative, resolving only what a parser asks for, is what item 5 was
/// written to end. Charging one budget per pass instead of one per property needs the whole-pass
/// budget noted in [`parse_edge_insets`]'s `ponytail:`, not a smaller resolve.
pub fn resolve_properties(
    properties: &HashMap<String, Value>,
    kind: &str,
    lua: &Lua,
) -> Result<HashMap<String, Value>, LayoutError> {
    let mut resolved = HashMap::with_capacity(properties.len());
    // Sorted, and the sort is the point: `properties` is a `HashMap` whose iteration order is
    // randomised per process, so with two failing properties on one node the error a run reported
    // was whichever the hash seed happened to reach first -- eight runs of the same broken config
    // named two different properties. This message is what `renderer/src/socket.rs` puts in the
    // `rescue` global's `error_log` for a human to read after the fact (§ 2.10, docs/adr/0024), so
    // which property a given broken config names has to be a function of the config alone, not of
    // the hash seed the process happened to boot with. Do not "optimise" this into a bare
    // `for (property, value) in properties`.
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort_unstable();
    for property in names {
        let value = &properties[property];
        if is_structural_property(kind, property) {
            resolved.insert(property.clone(), value.clone());
            continue;
        }
        let Value::UserData(ud) = value else {
            resolved.insert(property.clone(), value.clone());
            continue;
        };
        let Ok(signal) = ud.borrow::<Signal>() else {
            resolved.insert(property.clone(), value.clone());
            continue;
        };
        let value = signal
            .get_value(lua)
            .map_err(|e| invalid(property, format!("Signal getter failed: {e}")))?;
        match value {
            Value::UserData(_) => {
                return Err(invalid(
                    property,
                    "a Signal resolved to another Signal -- resolution happens exactly once, not to a fixed point",
                ));
            }
            Value::Nil => {}
            value => {
                resolved.insert(property.clone(), value);
            }
        }
    }
    Ok(resolved)
}

/// The carve-outs from decision 1's "parsers resolve a `Signal`" rule: [`SurfaceTopology`]'s five
/// fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/`parse_monitor`/`parse_namespace`) and
/// every node's optional `id` ([`parse_node_id`], docs/adr/0045 decision 1) keep rejecting one
/// outright, the same way every parser used to (this function used to be named `reject_signal` and
/// back every one of them, then narrowed to the topology fields alone -- ADR-0044's amendment
/// banner -- before widening again here to cover `id`).
///
/// The unifying reason, which is why these six and nothing else: each is read exactly once per
/// evaluation and a *structural* decision is then made from it and acted on -- where a surface is
/// placed, or which retained node a fresh one is. A `Signal` is free to change between passes, so
/// admitting one here would leave a decision already taken resting on a value that no longer
/// holds, with nothing left to re-check it. Every other property is read for the geometry or
/// appearance of the pass it was read in, so a later change simply produces different output on
/// the next pass, which is the point of a signal.
///
/// Concretely: `surface_topology` runs on every `Scene::apply` so `renderer/src/socket.rs`'s
/// `handle_reevaluate` can diff it against `applied_topology` and choose swap-versus-in-place
/// (ADR-0001) -- a surface could otherwise move layer or monitor with no swap. And `id` is
/// `pair_children_by_id_then_position`'s reconcile identity, matched once per `Scene::apply` to
/// pair a fresh child against its retained counterpart -- a value that could change between the
/// match and whatever reads it afterward would make "the same node as last time" itself
/// ambiguous. ADR-0044 decision 1 doesn't carve either out explicitly -- it's a gap in the ADR,
/// not a case the ADR considered and rejected.
///
/// This only works because [`resolve_properties`] copies the keys [`is_structural_property`] names
/// through raw: these six parsers are the only ones that read the un-resolved value, and they have
/// to, since a resolved signal is indistinguishable from a literal by the time it reaches a map.
/// That skip is scoped to the kind whose parsers actually run, which is what keeps "copied through
/// raw" from meaning "never checked by anyone" -- see [`is_structural_property`].
fn reject_signal_in_structural_field(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

/// `"NN%"` (`^\d+(\.\d+)?%$`) as `SizeMode::Percent`. Not a confirmed spec syntax -- § 5.1's base
/// property table only documents integer/`"Fill"` for width/height even though § 3.1 names
/// `Percent(f32)` as a size class without giving it a literal Lua form. See docs/adr/0023.
fn parse_percent(s: &str) -> Option<f32> {
    let digits = s.strip_suffix('%')?;
    let mut parts = digits.splitn(2, '.');
    let int_part = parts.next()?;
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(frac_part) = parts.next()
        && (frac_part.is_empty() || !frac_part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    digits.parse::<f32>().ok().map(|n| n / 100.0)
}

/// An explicit pixel value is range-checked against § 5.1's base property table
/// (`[0, 8192]`) -- ADR-0021 item 5 named this phase's layout engine as the "actual
/// consumer that needs typed, validated properties" that range check waits on.
///
/// `properties` is a [`resolve_properties`] result, here and in every parser below: an absent key
/// covers both a property the config omitted and one whose signal read `nil`, which is why none of
/// them takes a `&Lua` or handles a `Value::Nil` of its own.
pub fn parse_size_mode(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<SizeMode, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(SizeMode::Content);
    };
    if let Some(n) = value_as_f32(property, value)? {
        if !(0.0..=8192.0).contains(&n) {
            return Err(invalid(
                property,
                format!("must be within [0, 8192], got {n}"),
            ));
        }
        return Ok(SizeMode::Pixels(n));
    }
    if let Value::String(s) = value {
        let s = checked_string(property, s)?;
        if s == "Fill" {
            return Ok(SizeMode::Fill);
        }
        if let Some(pct) = parse_percent(&s) {
            return Ok(SizeMode::Percent(pct));
        }
    }
    Err(invalid(
        property,
        format!(
            "expected a number, \"Fill\", or a \"NN%\" string (Content sizing has no literal -- omit the property instead), got {}",
            preview_for_error(value)
        ),
    ))
}

/// ponytail: the four `table.get` calls below are metamethod-aware, so a resolved table carrying a
/// side-effecting `__index` still answers per read, and this parser runs once per consumer of the
/// property rather than once per node (a child's `margin` is parsed by its parent's child loop,
/// both `intrinsic_content_size` folds and `position_children`). The *signal* behind it is read
/// exactly once (build-steps.md Phase 19 item 5), which is the defect that item names; a table
/// metamethod is a narrower hole. Closing it means parsing each geometry property once into the
/// retained node too, not just resolving it once.
///
/// ponytail: those `table.get` calls also run entirely outside ADR-0021's 5ms cap, and so do
/// [`parse_anchor`]'s. `CpuBudget` installs its Lua hook inside `Signal::get_value` and drops it on
/// return (`renderer/src/lua/signal.rs`), so the only Lua a budget ever covers is a signal getter's
/// own body -- a metamethod this parser triggers afterwards runs unhooked. Measured: a `margin`
/// table whose `__index` spins 200 million iterations made one `Scene::apply` take 26.10 seconds
/// and return `Ok(())`, while the identical loop placed inside a `computed` was refused in 5.12ms.
/// That is reachable from a config using no `Signal` at all, on the thread that also answers
/// `configure` and runs the VM (docs/adr/0039), so § 1.2's "CPU runtime is capped at 5ms per
/// evaluation" is a cap on signal evaluation, not on a layout pass. The upgrade path is a budget
/// spanning the whole pass rather than one per `get_value` call, which would subsume the
/// per-property budget multiplication [`resolve_properties`]'s own `ponytail:` records; not built
/// here.
///
/// Scalar shorthand -- a bare number broadcasts to all four edges -- used to live only in front of
/// `border_width` (docs/build-steps.md Phase 19 item 15: item 6's second commit added it there and
/// nowhere else). Moved in here so `margin` and `padding` get it from the same place instead of two
/// more copies of the same wrapper. It carries no range check of its own: see
/// [`check_geometry_range`]'s doc comment for why `border_width` keeps a bound this function does
/// not apply to `margin`/`padding`.
pub fn parse_edge_insets(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<EdgeInsets, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(EdgeInsets::default());
    };
    if let Some(n) = value_as_f32(property, value)? {
        return Ok(EdgeInsets {
            top: n,
            right: n,
            bottom: n,
            left: n,
        });
    }
    let Value::Table(table) = value else {
        return Err(invalid(
            property,
            format!("expected a number or a table, got {}", preview_for_error(value)),
        ));
    };
    let edge = |key: &str| -> Result<f32, LayoutError> {
        let v: Value = table
            .get(key)
            .map_err(|e| invalid(property, e.to_string()))?;
        match v {
            Value::Nil => Ok(0.0),
            // A Signal only resolves at the top level of the property map (`resolve_properties`
            // never looks inside a table value), so one surviving into a per-edge slot is refused
            // outright rather than falling into the `other` arm below, which would misreport it as
            // "must be a number, got AnyUserData(Ref(0x...))" -- an opaque pointer and a wrong claim
            // about the type. `UnsupportedSignalProperty` already carries the right advice (read
            // it via `:get()` first); `{property}.{key}` names both the property and which edge.
            Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!(
                "{property}.{key}"
            ))),
            other => value_as_f32(property, &other)?.ok_or_else(|| {
                invalid(
                    property,
                    format!("`{key}` must be a number, got {}", preview_for_error(&other)),
                )
            }),
        }
    };
    Ok(EdgeInsets {
        top: edge("top")?,
        right: edge("right")?,
        bottom: edge("bottom")?,
        left: edge("left")?,
    })
}

/// `rect.background` (§ 5.2 item 1). Absent is `None`, not transparent black -- `layout::paint`'s
/// `fill_rect` has to be able to skip the fill entirely rather than paint an invisible one, and
/// `#RRGGBBAA` with `AA = 00` already covers "explicitly transparent" as a distinct config choice.
pub fn parse_background(properties: &HashMap<String, Value>) -> Result<Option<Rgba>, LayoutError> {
    let Some(value) = properties.get("background") else {
        return Ok(None);
    };
    let Value::String(s) = value else {
        return Err(invalid(
            "background",
            format!("expected a string, got {}", preview_for_error(value)),
        ));
    };
    let s = checked_string("background", s)?;
    Ok(Some(parse_hex_color("background", &s)?))
}

/// Shared `[0, 8192]` bound for `radius` and `border_width` -- the same range [`parse_size_mode`]
/// already enforces for `width`/`height` (§ 5.1's base property table), applied here because these
/// are geometry on the same node kind with no bound of their own otherwise. Traced in femtovg
/// 0.26: `radius = -4` silently draws square corners (`path.rs:458` treats anything under 0.1 as
/// unrounded) and `border_width = -4` clamps to 0.0 and multiplies paint alpha by zero, so both
/// negative ends fail silently rather than raising. The upper end is the one that matters most:
/// above roughly 8.4e6, `curve_divisions` (`path/cache.rs:911`) computes `acos(1.0) == 0.0`,
/// divides by it, and `inf as u32` saturates to `u32::MAX` as a stroke-loop bound in
/// `round_join`/`round_cap_start` -- billions of iterations and tens of gigabytes of vertices on
/// the Wayland dispatch thread. `[0, 8192]` alone doesn't make that reachability obvious, which is
/// worth spelling out here so a future reader doesn't widen the bound without knowing why it was
/// chosen.
///
/// **Decision (docs/build-steps.md Phase 19 item 15): this bound stays private to `radius` and
/// `border_width`, not extended to `margin`/`padding` when the latter two picked up
/// [`parse_edge_insets`]'s scalar shorthand.** § 5.1's base property table gives `margin` and
/// `padding` no "Valid Range" entry at all -- unlike `width`/`height`, whose row spells out
/// `[0, 8192]` -- so nothing in the spec asks for a bound here. `parse_edge_insets` never checked a
/// range before this change either; margin and padding both already accepted an out-of-range value,
/// including negative, since the function only rejected the wrong Lua type. `layout::scene`'s
/// `position_children` reads a negative margin the same way CSS does: it is subtracted into a
/// child's footprint and slot size on the main and cross axis, so `margin = -8` deliberately pulls a
/// child closer to (or over) its neighbor. That is layout math, not a femtovg stroke input, and
/// nothing in `position_children` special-cases a negative or reads the value as anything other than
/// an offset -- no crash mode like `border_width`'s curve-divisions blowup applies here. So a config
/// relying on negative margin to overlap or tighten siblings keeps working exactly as before: this
/// slice only adds the scalar shorthand to `margin`/`padding`, and adds no new restriction on either.
fn check_geometry_range(property: &str, n: f32) -> Result<(), LayoutError> {
    if !(0.0..=8192.0).contains(&n) {
        return Err(invalid(
            property,
            format!("must be within [0, 8192], got {n}"),
        ));
    }
    Ok(())
}

/// `rect.radius` (§ 5.2 item 1). Absent defaults to 0, an unrounded rectangle -- same shape as
/// [`parse_spacing`]/[`parse_font_size`] for an unranged numeric property with no documented
/// default of its own beyond "no effect when omitted".
pub fn parse_radius(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("radius") else {
        return Ok(0.0);
    };
    let n = value_as_f32("radius", value)?
        .ok_or_else(|| invalid("radius", format!("expected a number, got {}", preview_for_error(value))))?;
    check_geometry_range("radius", n)?;
    Ok(n)
}

/// `rect.border_color` (§ 5.2 item 1), one colour per edge. `None` on an edge means "not painted",
/// the same absence [`parse_background`] returns for a missing fill and the same zero
/// [`parse_border_width`] defaults an edge to -- an edge with width 0 needs no colour, and an edge
/// with a colour but width 0 still paints nothing, so the drawing pass can read either field first
/// and get the same answer. § 5.2 gives the table form no per-edge default colour to fall back to,
/// so an absent edge takes `None` rather than an invented default.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BorderColor {
    pub top: Option<Rgba>,
    pub right: Option<Rgba>,
    pub bottom: Option<Rgba>,
    pub left: Option<Rgba>,
}

pub fn parse_border_color(properties: &HashMap<String, Value>) -> Result<BorderColor, LayoutError> {
    let Some(value) = properties.get("border_color") else {
        return Ok(BorderColor::default());
    };
    if let Value::String(s) = value {
        let s = checked_string("border_color", s)?;
        let color = Some(parse_hex_color("border_color", &s)?);
        return Ok(BorderColor {
            top: color,
            right: color,
            bottom: color,
            left: color,
        });
    }
    let Value::Table(table) = value else {
        return Err(invalid(
            "border_color",
            format!("expected a string or a table, got {}", preview_for_error(value)),
        ));
    };
    // ponytail: `table.get` is metamethod-aware, the same hole [`parse_edge_insets`]'s `ponytail:`
    // documents for margin/padding -- see that comment for the cost and the upgrade path.
    let edge = |key: &str| -> Result<Option<Rgba>, LayoutError> {
        let v: Value = table
            .get(key)
            .map_err(|e| invalid("border_color", e.to_string()))?;
        // Every error this closure raises names the edge, `key`, not just the property --
        // see the `Value::UserData` arm below and `name_edge` for why the String arm needs help
        // to do that too, since `checked_string`/`parse_hex_color` only know the property.
        let name_edge = |e: LayoutError| match e {
            LayoutError::InvalidProperty { property, detail } => LayoutError::InvalidProperty {
                property,
                detail: format!("`{key}`: {detail}"),
            },
            other => other,
        };
        match v {
            Value::Nil => Ok(None),
            // Same hole as `parse_edge_insets`'s `edge` closure, same fix: a Signal only resolves
            // at the top level of the property map, so one nested here is refused outright rather
            // than falling into the `other` arm and being misreported as a bad hex string.
            Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!(
                "border_color.{key}"
            ))),
            Value::String(s) => {
                let s = checked_string("border_color", &s).map_err(name_edge)?;
                Ok(Some(parse_hex_color("border_color", &s).map_err(name_edge)?))
            }
            other => Err(invalid(
                "border_color",
                format!("`{key}` must be a hex colour string, got {}", preview_for_error(&other)),
            )),
        }
    };
    Ok(BorderColor {
        top: edge("top")?,
        right: edge("right")?,
        bottom: edge("bottom")?,
        left: edge("left")?,
    })
}

/// `rect.border_width` (§ 5.2 item 1), reusing [`EdgeInsets`] rather than a new per-edge type since
/// the shape (four `f32`, default 0) is already exactly that. Both the scalar and table forms
/// (docs/build-steps.md Phase 19 item 15) come straight from [`parse_edge_insets`], which also
/// defaults an absent edge to 0; the one thing this wrapper still adds is the `[0, 8192]` range
/// check, run on every edge of the result. That check stays here rather than moving into
/// `parse_edge_insets` itself -- see [`check_geometry_range`]'s doc comment for why `margin`/
/// `padding` don't get it.
pub fn parse_border_width(properties: &HashMap<String, Value>) -> Result<EdgeInsets, LayoutError> {
    let insets = parse_edge_insets(properties, "border_width")?;
    for n in [insets.top, insets.right, insets.bottom, insets.left] {
        check_geometry_range("border_width", n)?;
    }
    Ok(insets)
}

pub fn parse_align(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<Align, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(Align::Start);
    };
    let Value::String(s) = value else {
        return Err(invalid(
            property,
            format!("expected a string, got {}", preview_for_error(value)),
        ));
    };
    match checked_string(property, s)?.as_str() {
        "Start" => Ok(Align::Start),
        "Center" => Ok(Align::Center),
        "End" => Ok(Align::End),
        "Stretch" => Ok(Align::Stretch),
        other => Err(invalid(property, format!("unknown alignment `{other}`"))),
    }
}

pub fn parse_visible(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    let Some(value) = properties.get("visible") else {
        return Ok(true);
    };
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(invalid(
            "visible",
            format!("expected a boolean, got {}", preview_for_error(other)),
        )),
    }
}

pub fn parse_spacing(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("spacing") else {
        return Ok(0.0);
    };
    value_as_f32("spacing", value)?
        .ok_or_else(|| invalid("spacing", format!("expected a number, got {}", preview_for_error(value))))
}

/// Absent `content` defaults to the empty string. It used to be required, but decision 1's nil
/// rule (docs/adr/0044) means a `text` bound to a bare, not-yet-pushed capability signal --
/// `text { content = oblisk.mpris.title }`, the ADR's headline example -- resolves `content` to
/// absent at boot, since every rostered signal reads `nil` until its first `StateSnapshot` and
/// `run_startup_evaluation` runs before the poll loop drains one. Rejecting that would reject the
/// whole tree and boot a blank shell. Once nil means absent, the parser cannot tell that state
/// apart from an omitted key anyway, so a default is the only option, not one of several. See
/// docs/adr/0044's amendment banner for the full argument, and build-steps.md Phase 19 item 6.
///
/// Accepted cost: a misspelled `content` key now renders an empty node instead of being rejected.
/// That is the better failure for a shell that has to boot, and `oblisk.rescue` still exists for
/// the failures that matter.
pub fn parse_content(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    let Some(value) = properties.get("content") else {
        return Ok(String::new());
    };
    match value {
        Value::String(s) => checked_string("content", s),
        other => Err(invalid(
            "content",
            format!("expected a string, got {}", preview_for_error(other)),
        )),
    }
}

/// `text.foreground` (§ 5.2 item 4). Absent defaults to white -- `layout::paint`'s `paint_text`
/// falls back to the same white whenever this parser errors on a present-but-malformed value, so
/// the rendered result agrees whether the key was omitted or rejected.
pub fn parse_foreground(properties: &HashMap<String, Value>) -> Result<Rgba, LayoutError> {
    let Some(value) = properties.get("foreground") else {
        return Ok(Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        });
    };
    let Value::String(s) = value else {
        return Err(invalid(
            "foreground",
            format!("expected a string, got {}", preview_for_error(value)),
        ));
    };
    let s = checked_string("foreground", s)?;
    parse_hex_color("foreground", &s)
}

pub fn parse_font_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("font_size") else {
        return Ok(12.0);
    };
    value_as_f32("font_size", value)?
        .ok_or_else(|| invalid("font_size", format!("expected a number, got {}", preview_for_error(value))))
}

/// Absent `size` defaults to 12.0, same rationale and same amendment as [`parse_content`]
/// (docs/adr/0044's amendment banner, build-steps.md Phase 19 item 6): `icon` was the second
/// property the amendment names as still failing after decision 1's nil rule alone. The rule is
/// decision 1's, amended, not decision 2's -- decision 2 is the dirty flag, and landing it is only
/// what exposed the gap.
///
/// Carries the same accepted cost as [`parse_content`], stated separately because it is a separate
/// property a config can misspell: `icon { sizee = 24 }` now renders a 12.0-sized icon instead of
/// being rejected.
/// `oblisk-idl-api-specs.md` § 5.2 documents `size` with no default of its own, so the number is
/// picked to match this file's own convention instead: it is [`parse_font_size`]'s default,
/// making `text` and `icon` -- the two leaf kinds sized by one numeric property -- agree, so an
/// icon dropped inline with default-sized text lands at the same visual scale.
pub fn parse_icon_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("size") else {
        return Ok(12.0);
    };
    value_as_f32("size", value)?
        .ok_or_else(|| invalid("size", format!("expected a number, got {}", preview_for_error(value))))
}

/// Shared shape behind [`parse_surface_id`]/[`parse_layer`]/[`parse_monitor`]: fetch `property`,
/// reject a `Signal`, require it to be a string. `default` supplies the value when the property
/// is absent; `None` makes it required, erroring instead (Standards review, docs/adr/0024).
fn parse_string_property(properties: &HashMap<String, Value>, property: &str, default: Option<&str>) -> Result<String, LayoutError> {
    let value = match properties.get(property) {
        Some(value) => value,
        None => match default {
            Some(default) => return Ok(default.to_string()),
            None => return Err(invalid(property, format!("surface node requires `{property}`"))),
        },
    };
    reject_signal_in_structural_field(property, value)?;
    match value {
        Value::String(s) => Ok(s.to_string_lossy()),
        other => Err(invalid(property, format!("expected a string, got {}", preview_for_error(other)))),
    }
}

/// A top-level surface's `id`: required, unique among the surfaces in one config, and keys
/// `Scene::apply`'s `HashMap` (`docs/oblisk-layout-engine-geometry.md` § 4). Also, since
/// docs/adr/0045, this same property is the surface's *reconcile* identity -- the root of a
/// tree is the one node whose retained counterpart is found by key lookup rather than by
/// [`parse_node_id`]'s per-parent pairing, because a surface has no parent to be scoped within.
/// One property name, one meaning ("which node is this, across two applies"), read by two
/// different call sites for what happens to be two different purposes at the root versus
/// everywhere below it -- decision 5 is explicit that this is not a second mechanism to build,
/// just the existing one restated at the level below.
pub fn parse_surface_id(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "id", None)
}

/// The optional `id` base property on every node kind, one level below a surface's root
/// (`docs/oblisk-layout-engine-geometry.md` § 4, docs/adr/0045 decisions 1-2). `None` means "no
/// id" and is not an error -- `pair_children_by_id_then_position` pairs a child that carries none
/// positionally against the other id-less children, exactly ADR-0023's original rule applied to
/// that subsequence. Adding or dropping an `id` is therefore a change of identity, not a cosmetic
/// edit: the retained counterpart is retired and a new node allocated. Rejects a `Signal` via
/// [`reject_signal_in_structural_field`] for the same reason [`parse_surface_id`] already does:
/// this is a reconcile identity, decided once at match time, not a value that should be able to
/// drift between the fresh tree and whatever the match produces.
///
/// Non-UTF-8 bytes are refused rather than converted, unlike [`checked_string`]'s lossy handling
/// of display-oriented properties like `content`. An id is an *equality key*: with
/// `to_string_lossy`, `"\xFF"` and `"\xFE"` both become `U+FFFD` and two genuinely distinct ids
/// compare equal, so `pair_children_by_id_then_position`'s duplicate check would reject a valid
/// config and a fresh child could claim the wrong retained counterpart. A garbled glyph in a
/// label is cosmetic; a garbled identity silently rebinds a node's retained subtree.
///
/// Scoping ("unique among siblings, not across the tree") and duplicate rejection are
/// `pair_children_by_id_then_position`'s job, not this parser's -- a duplicate can only be
/// detected by comparing this node's id against its siblings', which this function has no
/// visibility into.
pub fn parse_node_id(properties: &HashMap<String, Value>) -> Result<Option<String>, LayoutError> {
    let Some(value) = properties.get("id") else {
        return Ok(None);
    };
    reject_signal_in_structural_field("id", value)?;
    match value {
        Value::String(s) => s
            .to_str()
            .map(|s| Some(s.to_string()))
            .map_err(|_| invalid("id", "must be valid UTF-8 -- an id is compared for equality, so it cannot be converted lossily")),
        other => Err(invalid("id", format!("expected a string, got {}", preview_for_error(other)))),
    }
}

/// § 6.1's `layer`, the layer-shell stacking level a `panel` is created on. `layout`'s own enum
/// rather than smithay-client-toolkit's `Layer`, for the same reason [`KeyboardInteractivity`]
/// below is: this module stays free of Wayland types, and `crate::wayland` maps it at its one call
/// site (build-steps.md Phase 20 item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// § 6.1's `layer` (`"Background"`/`"Bottom"`/`"Top"`/`"Overlay"`). Required, same shape as
/// [`parse_surface_id`] -- every existing fixture in this repo already sets it.
///
/// **Validating since build-steps.md Phase 20.** This used to return the raw `String`, on the
/// stated grounds that Phase 13 only needed it for topology-diff equality (`CONTEXT.md`, Topology
/// change) and nothing bound a real `zwlr_layer_surface_v1` with it -- see docs/adr/0024. Phase 20
/// is what makes that false: `crate::wayland::App::create_panels` now creates one layer surface per
/// instance straight from this value (docs/adr/0038 decision 1), so an unrecognized string is a
/// config error the author must see rather than a silent fall to some default layer. A typo'd
/// `layer = "Toop"` that quietly stacked a bar on `Background` would be a far worse failure than a
/// rejected config, because nothing on screen would say why.
pub fn parse_layer(properties: &HashMap<String, Value>) -> Result<LayerKind, LayoutError> {
    match parse_string_property(properties, "layer", None)?.as_str() {
        "Background" => Ok(LayerKind::Background),
        "Bottom" => Ok(LayerKind::Bottom),
        "Top" => Ok(LayerKind::Top),
        "Overlay" => Ok(LayerKind::Overlay),
        other => Err(invalid(
            "layer",
            format!("unknown layer `{other}` -- expected \"Background\", \"Bottom\", \"Top\", or \"Overlay\""),
        )),
    }
}

/// § 6.1's `anchor` table (`{ top, bottom, left, right }` edge booleans). Same default-to-zero
/// shape as [`EdgeInsets`], booleans instead of floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Anchor {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

pub fn parse_anchor(properties: &HashMap<String, Value>) -> Result<Anchor, LayoutError> {
    let Some(value) = properties.get("anchor") else {
        return Ok(Anchor::default());
    };
    reject_signal_in_structural_field("anchor", value)?;
    let Value::Table(table) = value else {
        return Err(invalid("anchor", format!("expected a table, got {}", preview_for_error(value))));
    };
    let edge = |key: &str| -> Result<bool, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("anchor", e.to_string()))?;
        match v {
            Value::Nil => Ok(false),
            Value::Boolean(b) => Ok(b),
            other => Err(invalid(
                "anchor",
                format!("`{key}` must be a boolean, got {}", preview_for_error(&other)),
            )),
        }
    };
    Ok(Anchor { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? })
}

/// § 6.1's `monitor` (a specific output EDID, or `"All"`). Absent defaults to `"All"` -- an
/// unqualified surface targets every monitor, matching the IDL's own documented meaning for that
/// value rather than treating the property as required.
pub fn parse_monitor(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "monitor", Some("All"))
}

/// § 6.1's `namespace`: the layer-shell namespace string the compositor sees, and the key its own
/// rules match on (Hyprland's `layerrule` for blur and animations). Defaults to `"oblisk-{id}"`,
/// which is what makes every `panel` addressable from a compositor config without the author
/// having to name one -- before docs/adr/0038 this was hardcoded per Rust-owned role
/// (`"oblisk-main-bar"`, `"oblisk-overlay-canvas"`, `"oblisk-wallpaper"`) and a user could not
/// write a rule against their own panel at all.
///
/// A [`SurfaceTopology`] field, not an in-place one: `get_layer_surface` takes the namespace at
/// creation and the protocol has no request to change it afterwards, so an edit to it is a
/// generation swap (`CONTEXT.md`, Topology change).
pub fn parse_namespace(properties: &HashMap<String, Value>, id: &str) -> Result<String, LayoutError> {
    let default = format!("oblisk-{id}");
    parse_string_property(properties, "namespace", Some(&default))
}

/// § 6.1's `keyboard_interactivity`, mapping one-for-one onto layer-shell's own field.
/// `layout`-owned rather than reusing smithay-client-toolkit's identical enum so this module keeps
/// no Wayland dependency; `crate::wayland::keyboard_interactivity_for` maps it at the single call
/// site that binds a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardInteractivity {
    /// § 6.1's default: the surface never receives key events.
    #[default]
    None,
    OnDemand,
    Exclusive,
}

/// § 6.1's `keyboard_interactivity` (`"None"` (default) / `"OnDemand"` / `"Exclusive"`). An
/// in-place field, deliberately outside [`SurfaceTopology`] and outside
/// [`is_structural_property`]'s carve-out: `zwlr_layer_surface_v1::set_keyboard_interactivity` is
/// valid on a live surface, so a `Signal` here resolves like any other property
/// (docs/adr/0044 decision 1) and an edit to it is a value change, not a swap.
pub fn parse_keyboard_interactivity(properties: &HashMap<String, Value>) -> Result<KeyboardInteractivity, LayoutError> {
    let Some(value) = properties.get("keyboard_interactivity") else {
        return Ok(KeyboardInteractivity::None);
    };
    let Value::String(s) = value else {
        return Err(invalid(
            "keyboard_interactivity",
            format!("expected a string, got {}", preview_for_error(value)),
        ));
    };
    match checked_string("keyboard_interactivity", s)?.as_str() {
        "None" => Ok(KeyboardInteractivity::None),
        "OnDemand" => Ok(KeyboardInteractivity::OnDemand),
        "Exclusive" => Ok(KeyboardInteractivity::Exclusive),
        other => Err(invalid(
            "keyboard_interactivity",
            format!("unknown keyboard interactivity `{other}` -- expected \"None\", \"OnDemand\", or \"Exclusive\""),
        )),
    }
}

/// § 6.1's `exclusive`: "Reserves physical screen area for bar if true". Default `false`, so an
/// undeclared panel floats over whatever is behind it rather than pushing windows aside.
///
/// In-place, same as [`parse_keyboard_interactivity`]: `set_exclusive_zone` is valid on a live
/// surface. The *zone* itself is not computed here -- `crate::wayland` derives it at configure
/// time from the size the compositor actually chose, which is the only point a real number exists.
pub fn parse_exclusive(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    let Some(value) = properties.get("exclusive") else {
        return Ok(false);
    };
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(invalid(
            "exclusive",
            format!("expected a boolean, got {}", preview_for_error(other)),
        )),
    }
}

/// A surface's topology-relevant fields (`CONTEXT.md`, Topology change: a config edit that adds or
/// removes a top-level `surface` node, or changes its layer, anchor, monitor target, or
/// namespace). Structural equality on `Vec<SurfaceTopology>` (order-sensitive) is the Renderer's
/// own topology diff -- see `renderer/src/socket.rs`.
///
/// This is the whole of the swap fingerprint, and [`PanelSpec`]'s other fields are deliberately
/// not in it: `margin`, `keyboard_interactivity`, `exclusive`, `width` and `height` are all
/// requests layer-shell accepts on a live surface, so changing one reloads in place
/// (docs/adr/0038 decision 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceTopology {
    pub id: String,
    pub layer: LayerKind,
    pub anchor: Anchor,
    pub monitor: String,
    pub namespace: String,
}

pub fn surface_topology(properties: &HashMap<String, Value>) -> Result<SurfaceTopology, LayoutError> {
    let id = parse_surface_id(properties)?;
    let namespace = parse_namespace(properties, &id)?;
    Ok(SurfaceTopology {
        id,
        layer: parse_layer(properties)?,
        anchor: parse_anchor(properties)?,
        monitor: parse_monitor(properties)?,
        namespace,
    })
}

/// Everything one `zwlr_layer_surface_v1` needs, read off a `panel` node's properties in one pass
/// (§ 6.1, build-steps.md Phase 20 item 3). `crate::socket`'s `panel_specs` builds one per declared
/// surface; `layout::instance::expand_instances` turns them into per-output instances, and
/// `crate::wayland::App::create_panels` is what actually binds them.
///
/// The split between `topology` and the rest is the swap-versus-in-place split itself, so it is
/// worth reading as one: `renderer/src/socket.rs`'s `handle_reevaluate` diffs *only* `topology`,
/// which is why editing a `margin` reloads in place while editing a `layer` respawns the process.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelSpec {
    /// The swap fingerprint: id, layer, anchor, monitor, namespace.
    pub topology: SurfaceTopology,
    pub keyboard_interactivity: KeyboardInteractivity,
    pub exclusive: bool,
    /// § 6.1's `margin`, which on a `panel` root is the layer-shell **anchor offset** -- how far
    /// the surface itself sits from the edges it is anchored to -- not layout spacing between the
    /// root and its child. There is no conflict with layout's own reading of the property because
    /// layout never reads it here: `layout::scene`'s `Scene::apply_one_surface` passes `None` for
    /// both parent-margin arguments when it resolves a surface root, so a root's `margin` is
    /// consumed by nobody but this field. Below a root it stays ordinary layout margin, parsed by
    /// the same [`parse_edge_insets`] and consumed by the parent's child loop.
    pub margin: EdgeInsets,
    /// § 6.1's `width`/`height`, which become the layer-shell `set_size` request rather than a
    /// layout constraint of their own. `SizeMode::Fill` is the protocol's `0` ("the anchors
    /// decide"); a percent resolves against the output, at the one call site that knows it.
    pub width: SizeMode,
    pub height: SizeMode,
}

pub fn panel_spec(properties: &HashMap<String, Value>) -> Result<PanelSpec, LayoutError> {
    Ok(PanelSpec {
        topology: surface_topology(properties)?,
        keyboard_interactivity: parse_keyboard_interactivity(properties)?,
        exclusive: parse_exclusive(properties)?,
        margin: parse_edge_insets(properties, "margin")?,
        width: parse_size_mode(properties, "width")?,
        height: parse_size_mode(properties, "height")?,
    })
}

/// A single-node property (`panel.child`), converted from its raw table via
/// `lua::nodes::deserialize_lua_table` -- not re-implemented here.
pub fn parse_single_child(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<Option<VirtualNode>, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            property,
            format!("expected a node table, got {}", preview_for_error(value)),
        ));
    };
    let node = deserialize_lua_table(table).map_err(|e| invalid(property, e.to_string()))?;
    Ok(Some(node))
}

/// An array-of-nodes property (`rect`/`row`/`column`/`button.children`).
pub fn parse_children(properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    let Some(value) = properties.get("children") else {
        return Ok(Vec::new());
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            "children",
            format!("expected an array table, got {}", preview_for_error(value)),
        ));
    };
    let mut children = Vec::new();
    for entry in table.sequence_values::<mlua::Table>() {
        let entry = entry.map_err(|e| invalid("children", e.to_string()))?;
        let node = deserialize_lua_table(&entry).map_err(|e| invalid("children", e.to_string()))?;
        children.push(node);
    }
    Ok(children)
}

/// A `list` node's children (`oblisk-idl-api-specs.md` § 5.2 item 7, docs/adr/0045 decision 3,
/// build-steps.md Phase 19 item 12). Parallels [`parse_children`]'s role for
/// `rect`/`row`/`column`/`button`, but a `list`'s children are never a literal Lua table: they are
/// generated here, once per element of `source`, by calling `itemfn(element)` and deserializing
/// the node table it returns the same way a literal child table is deserialized.
///
/// `source` arrives already resolved. `resolve_properties` treats it like any other
/// non-structural property, so a `Signal` there was read exactly once before this function ever
/// runs (build-steps.md Phase 19 item 1) -- nothing here re-reads it, which is what "resolve
/// through the existing Signal machinery" means: there is no second mechanism to build.
///
/// Without `key`, a generated child gets no `id` at all, so
/// `layout::scene::pair_children_by_id_then_position` matches list items by position -- the same
/// rule an id-less literal child already gets, and exactly what decision 3 specifies. With `key`,
/// `key(element)` -- called on the source element, never on the node `itemfn` built, so a key is
/// computable without building anything -- becomes that child's `id`, overwriting whatever `id`
/// `itemfn`'s own node table carried: a list item's identity belongs to the list, and honoring an
/// inner `id` instead would let two items that happen to declare the same one collide.
///
/// Duplicate keys are rejected here, before any `id` reaches `pair_children_by_id_then_position`,
/// so that function's own "duplicate id" message stays about a literal sibling `id` and a list
/// author gets a message naming `key`, the property they actually wrote.
///
/// ponytail: `key` makes *reconciliation* cheap, not *evaluation*. This calls `itemfn` for every
/// element on every resolve, so a 30-item tray builds 30 fresh nodes each time, and
/// `pair_children_by_id_then_position` then matches 29 of them to retained nodes and throws the
/// fresh ones away. § 5.2 calls `list` a "fast-reconciling virtual repeater", and the reconciling
/// half is what ADR-0045 delivered; the repeater half still re-runs a Lua closure per item per
/// pass. `resolve_and_reconcile` runs per `Scene::apply`, which ADR-0044 decision 2's dirty flag
/// made per poll turn rather than per config edit, so this is the same cadence change item 14's
/// text-reshaping `ponytail:` records against `intrinsic_content_size`.
///
/// The fix is to compute keys first and skip `itemfn` for an element whose key already matches a
/// retained child, which is what makes it a virtual repeater rather than a loop. It is not built
/// here because this function cannot see the retained children: `children_of` hands it only the
/// fresh node's own properties, and giving it the retained side means changing that signature and
/// the two other `children_of` arms with it. Worth doing when a real config drives a list from a
/// capability that pushes often, not before.
pub fn parse_list_children(properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    let source_value = properties
        .get("source")
        .ok_or_else(|| invalid("source", "required for `list`, got nothing"))?;
    let Value::Table(source) = source_value else {
        return Err(invalid(
            "source",
            format!("expected an array table, got {}", preview_for_error(source_value)),
        ));
    };

    let itemfn = match properties.get("itemfn") {
        Some(Value::Function(f)) => f,
        Some(other) => return Err(invalid("itemfn", format!("expected a function, got {}", preview_for_error(other)))),
        None => return Err(invalid("itemfn", "required for `list`, got nothing")),
    };

    let key_fn = match properties.get("key") {
        Some(Value::Function(f)) => Some(f),
        Some(other) => return Err(invalid("key", format!("expected a function, got {}", preview_for_error(other)))),
        None => None,
    };

    let mut children = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    for element in source.sequence_values::<Value>() {
        let element = element.map_err(|e| invalid("source", e.to_string()))?;

        let built = itemfn
            .call::<Value>(element.clone())
            .map_err(|e| invalid("itemfn", e.to_string()))?;
        let Value::Table(built_table) = built else {
            return Err(invalid(
                "itemfn",
                format!("expected a node table, got {}", preview_for_error(&built)),
            ));
        };
        let mut node = deserialize_lua_table(&built_table).map_err(|e| invalid("itemfn", e.to_string()))?;

        if let Some(key_fn) = key_fn {
            let key_value = key_fn.call::<Value>(element).map_err(|e| invalid("key", e.to_string()))?;
            let Value::String(key_str) = key_value else {
                return Err(invalid(
                    "key",
                    format!("expected key(item) to return a string, got {}", preview_for_error(&key_value)),
                ));
            };
            let key_text = key_str.to_str().map(|s| s.to_string()).map_err(|_| {
                invalid(
                    "key",
                    "must be valid UTF-8 -- a key is compared for equality, so it cannot be converted lossily",
                )
            })?;
            if !seen_keys.insert(key_text.clone()) {
                return Err(invalid("key", format!("duplicate key `{key_text}` among list items")));
            }
            // The key wins over any `id` the node itemfn built already carried -- see this
            // function's doc comment.
            node.properties.insert("id".to_string(), Value::String(key_str));
        }

        children.push(node);
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> mlua::Lua {
        mlua::Lua::new()
    }

    fn props_from_table(table: &mlua::Table) -> HashMap<String, Value> {
        deserialize_lua_table(table).unwrap().properties
    }

    #[test]
    fn width_absent_is_content() {
        let props = HashMap::new();
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Content);
    }

    #[test]
    fn width_integer_is_pixels() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = 32 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_size_mode(&props, "width").unwrap(),
            SizeMode::Pixels(32.0)
        );
    }

    #[test]
    fn width_fill_string_is_fill() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "Fill" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Fill);
    }

    #[test]
    fn width_percent_string_divides_by_100() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "50%" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_size_mode(&props, "width").unwrap(),
            SizeMode::Percent(0.5)
        );
    }

    #[test]
    fn width_above_the_8192_ceiling_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = 8193 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width").unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn a_negative_width_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = -5 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width").unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn width_garbage_string_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "banana" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width").unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn height_content_error_names_omission_as_the_spelling() {
        // Catches the message regressing to listing every mode except the one whose spelling is
        // "leave the property out" -- docs/build-steps.md Phase 19 item 15. `"Content"` is not a
        // valid literal, so an author reaching for it here needs the error itself to say so.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", height = "Content" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_size_mode(&props, "height").unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "height" && detail.contains("omit the property")),
            "must name omission as how Content sizing is spelled: {err}"
        );
    }

    #[test]
    fn margin_reads_named_edges_defaulting_absent_ones_to_zero() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", margin = { top = 4, left = 2 } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let insets = parse_edge_insets(&props, "margin").unwrap();
        assert_eq!(
            insets,
            EdgeInsets {
                top: 4.0,
                right: 0.0,
                bottom: 0.0,
                left: 2.0
            }
        );
    }

    #[test]
    fn padding_reads_named_edges_defaulting_absent_ones_to_zero() {
        // Same table form as `margin`'s equivalent test, on the sibling property that shares
        // `parse_edge_insets` -- proves the shorthand added below is additive, not a replacement.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", padding = { top = 4, left = 2 } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let insets = parse_edge_insets(&props, "padding").unwrap();
        assert_eq!(
            insets,
            EdgeInsets {
                top: 4.0,
                right: 0.0,
                bottom: 0.0,
                left: 2.0
            }
        );
    }

    #[test]
    fn margin_scalar_broadcasts_to_all_four_edges() {
        // The change itself: `margin = 10` used to be rejected as "expected a table". Catches the
        // shorthand not reaching `margin` when it moved off `border_width`'s private wrapper.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", margin = 10 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "margin").unwrap(),
            EdgeInsets { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 }
        );
    }

    #[test]
    fn padding_scalar_broadcasts_to_all_four_edges() {
        // Same change as `margin`'s scalar test, on the other property named in item 15.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", padding = 10 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "padding").unwrap(),
            EdgeInsets { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 }
        );
    }

    #[test]
    fn margin_negative_value_is_accepted() {
        // The range-check decision, tested directly: `border_width` rejects a value outside
        // [0, 8192] (see `a_negative_border_width_is_rejected`), but `margin` never gained that
        // check when it gained the scalar shorthand -- `position_children` (layout/scene.rs) reads
        // a negative margin as a deliberate pull toward a neighbor, the same as CSS. Catches the
        // range check leaking from `parse_border_width` into the shared `parse_edge_insets` and
        // breaking that pattern.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", margin = -10 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "margin").unwrap(),
            EdgeInsets { top: -10.0, right: -10.0, bottom: -10.0, left: -10.0 }
        );
    }

    #[test]
    fn padding_negative_value_is_accepted() {
        // Same decision as `margin_negative_value_is_accepted`, on `padding`. `padding` has no
        // established use for a negative value the way `margin` does, but the two share
        // `parse_edge_insets` and the decision covers the property, not a specific config pattern.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", padding = -10 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_edge_insets(&props, "padding").unwrap(),
            EdgeInsets { top: -10.0, right: -10.0, bottom: -10.0, left: -10.0 }
        );
    }

    #[test]
    fn align_h_parses_all_four_variants() {
        for (text, expected) in [
            ("Start", Align::Start),
            ("Center", Align::Center),
            ("End", Align::End),
            ("Stretch", Align::Stretch),
        ] {
            let lua = lua();
            let table: mlua::Table = lua
                .load(format!(r#"return {{ kind = "rect", align_h = "{text}" }}"#))
                .eval()
                .unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_align(&props, "align_h").unwrap(), expected);
        }
    }

    #[test]
    fn visible_absent_defaults_true() {
        let props = HashMap::new();
        assert!(parse_visible(&props).unwrap());
    }

    #[test]
    fn a_signal_userdata_in_a_geometry_slot_resolves_to_its_current_value() {
        // Replaces the old "rejected" test (docs/adr/0044 decision 1): `visible` is not a
        // topology field, so it now resolves a `Signal` instead of erroring on one.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(false), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("visible", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let resolved = resolve_properties(&node.properties, "rect", &lua).unwrap();
        assert!(
            !parse_visible(&resolved).unwrap(),
            "must read the signal's current value, not error on the handle"
        );
    }

    #[test]
    fn text_content_absent_defaults_to_the_empty_string() {
        // docs/adr/0044's amendment banner: `content` used to be required, but a `text` bound to a
        // not-yet-pushed capability signal resolves to absent (decision 1's nil rule) and must
        // still apply at boot, so absence now takes an empty-string default instead of erroring.
        let props = HashMap::new();
        assert_eq!(parse_content(&props).unwrap(), "");
    }

    #[test]
    fn a_signal_resolving_to_a_string_satisfies_content() {
        // ADR-0044 decision 1, step 1: a Signal wrapping "hello" parses as "hello".
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let hello = lua.create_string("hello").unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(hello), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("content", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(parse_content(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap(), "hello");
    }

    #[test]
    fn a_signal_resolving_to_a_table_reports_the_same_error_a_literal_table_would() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();

        let literal_table: mlua::Table = lua.load(r#"return { kind = "text", content = {} }"#).eval().unwrap();
        let literal_props = props_from_table(&literal_table);
        let literal_err = parse_content(&resolve_properties(&literal_props, "text", &lua).unwrap()).unwrap_err();

        let signal = crate::lua::signal::Signal::new_live(Value::Table(lua.create_table().unwrap()), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("content", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let signal_err = parse_content(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap_err();

        for err in [&literal_err, &signal_err] {
            assert!(matches!(
                err,
                LayoutError::InvalidProperty { property, detail }
                    if property == "content" && detail.starts_with("expected a string")
            ));
        }
    }

    #[test]
    fn a_signal_resolving_to_another_signal_is_an_error() {
        // ADR-0044 decision 1's "resolve exactly once": a Signal whose value is itself a Signal
        // userdata is an error, not a second read to a fixed point.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let inner = crate::lua::signal::Signal::new_live(Value::Integer(5), crate::lua::signal::DirtyFlag::new()).0;
        let inner_userdata = lua.create_userdata(inner).unwrap();
        let outer = crate::lua::signal::Signal::new_live(Value::UserData(inner_userdata), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", outer).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(
            resolve_properties(&node.properties, "text", &lua).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    /// A *resolved* property bag whose `property` slot held a live signal currently reading `nil`
    /// -- exactly the state every rostered capability's global is in before its first
    /// `StateSnapshot` (`renderer/src/socket.rs`'s `RendererClient::new` seeds all of
    /// `shared::CAPABILITIES` at `Value::Nil`), which is what a config binding a bare capability
    /// signal resolves at startup. Routed through [`resolve_properties`] because that is where the
    /// nil rule now lives: the key is omitted from the resolved map rather than each parser
    /// checking for a `Value::Nil` of its own (build-steps.md Phase 19 item 5).
    fn props_with_nil_signal(lua: &mlua::Lua, kind: &str, property: &str) -> HashMap<String, Value> {
        crate::lua::signal::register(lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Nil, crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", kind).unwrap();
        table.set(property, signal).unwrap();
        resolve_properties(&props_from_table(&table), kind, lua).unwrap()
    }

    #[test]
    fn a_signal_resolving_to_nil_takes_each_parsers_absent_property_default() {
        // ADR-0044 decision 1's nil rule: a signal resolving to `nil` means the property is
        // absent, so every parser's own default applies instead of it erroring on the `Nil`.
        // Without it a config binding any bare capability signal fails its very first
        // `Scene::apply`, because `run_startup_evaluation` runs before the poll loop has drained
        // a single `StateSnapshot`.
        let lua = lua();
        assert!(
            !props_with_nil_signal(&lua, "rect", "width").contains_key("width"),
            "the rule is one omitted key, not a Nil each parser re-checks"
        );
        assert_eq!(parse_size_mode(&props_with_nil_signal(&lua, "rect", "width"), "width").unwrap(), SizeMode::Content);
        assert_eq!(
            parse_edge_insets(&props_with_nil_signal(&lua, "rect", "padding"), "padding").unwrap(),
            EdgeInsets::default()
        );
        assert_eq!(parse_align(&props_with_nil_signal(&lua, "rect", "align_h"), "align_h").unwrap(), Align::Start);
        assert!(parse_visible(&props_with_nil_signal(&lua, "rect", "visible")).unwrap());
        assert_eq!(parse_spacing(&props_with_nil_signal(&lua, "row", "spacing")).unwrap(), 0.0);
        assert_eq!(parse_font_size(&props_with_nil_signal(&lua, "text", "font_size")).unwrap(), 12.0);
        assert!(parse_single_child(&props_with_nil_signal(&lua, "panel", "child"), "child").unwrap().is_none());
        assert!(parse_children(&props_with_nil_signal(&lua, "row", "children")).unwrap().is_empty());
        // `content` and `size` joined this list under docs/adr/0044's amendment banner: they used
        // to be the two properties this rule could not cover, because each was required and had no
        // default of its own -- see the deleted
        // `a_signal_resolving_to_nil_reports_a_required_property_as_missing_not_as_a_bad_value` test.
        // Without a default here, `text { content = oblisk.mpris.title }` (ADR-0044's headline
        // example) still rejects the whole tree at boot, since every rostered signal reads `nil`
        // until its first `StateSnapshot`.
        assert_eq!(parse_content(&props_with_nil_signal(&lua, "text", "content")).unwrap(), "");
        assert_eq!(parse_icon_size(&props_with_nil_signal(&lua, "icon", "size")).unwrap(), 12.0);
    }

    #[test]
    fn spacing_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        // CONFIRMED finding: marshal::check_number(1e300) is Ok (1e300 is a finite f64), but the
        // very next line's `as f32` saturates it to f32::INFINITY. `spacing` has no range check
        // of its own (unlike `parse_size_mode`'s `[0, 8192]`), so without this fix the Inf sails
        // through to `intrinsic_content_size`'s `spacing * visible.len().saturating_sub(1) as f32`
        // -- `inf * 0.0` is `NaN`, silently producing NaN geometry instead of a LayoutError.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "row", spacing = 1e300 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_spacing(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "spacing"
        ));
    }

    #[test]
    fn font_size_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        // Same defeated-guard bug as spacing, on a different unranged property: `font_size`
        // reaching `shaping.shape` as Inf would compute `line_height = inf * 1.2` instead of
        // erroring.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "text", font_size = 1e300 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_font_size(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    #[test]
    fn font_size_absent_defaults_to_twelve() {
        let props = HashMap::new();
        assert_eq!(parse_font_size(&props).unwrap(), 12.0);
    }

    #[test]
    fn a_signal_resolving_to_a_number_satisfies_font_size_through_marshals_check_number() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Number(18.0), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(parse_font_size(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap(), 18.0);
    }

    #[test]
    fn icon_size_absent_defaults_to_twelve() {
        // Same amendment as `content` (docs/adr/0044): `size` used to be required. Default value
        // matches `parse_font_size`'s own default -- see `parse_icon_size`'s doc comment for why.
        let props = HashMap::new();
        assert_eq!(parse_icon_size(&props).unwrap(), 12.0);
    }

    #[test]
    fn parse_children_walks_nested_node_tables() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "row", children = { { kind = "text", content = "a" }, { kind = "text", content = "b" } } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let children = parse_children(&props).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].kind, "text");
        assert_eq!(
            children[1]
                .properties
                .get("content")
                .unwrap()
                .as_string()
                .unwrap()
                .to_string_lossy(),
            "b"
        );
    }

    #[test]
    fn parse_single_child_converts_the_child_table() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "panel", child = { kind = "rect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let child = parse_single_child(&props, "child").unwrap();
        assert_eq!(child.unwrap().kind, "rect");
    }

    #[test]
    fn parse_single_child_absent_is_none() {
        let props = HashMap::new();
        assert!(parse_single_child(&props, "child").unwrap().is_none());
    }

    #[test]
    fn layer_is_required() {
        let props = HashMap::new();
        assert!(matches!(parse_layer(&props).unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn layer_reads_each_of_the_four_protocol_levels() {
        let lua = lua();
        for (text, expected) in [
            ("Background", LayerKind::Background),
            ("Bottom", LayerKind::Bottom),
            ("Top", LayerKind::Top),
            ("Overlay", LayerKind::Overlay),
        ] {
            let table: mlua::Table = lua.load(format!(r#"return {{ kind = "panel", layer = "{text}" }}"#)).eval().unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_layer(&props).unwrap(), expected);
        }
    }

    #[test]
    fn an_unrecognized_layer_is_a_config_error_not_a_silent_default() {
        // build-steps.md Phase 20 item 3: this used to return the raw string, so a typo reached
        // the topology diff intact and nothing ever validated it. Now `create_panels` binds a real
        // `zwlr_layer_surface_v1` with it, and a typo that quietly stacked a bar on the wrong
        // layer would be a worse failure than a rejected config -- nothing on screen would say why.
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", layer = "Toop" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_layer(&props).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "layer"), "got {err}");
        assert!(err.to_string().contains("Toop"), "the message must name the value the config wrote: {err}");
    }

    #[test]
    fn anchor_absent_defaults_all_false() {
        let props = HashMap::new();
        assert_eq!(parse_anchor(&props).unwrap(), Anchor::default());
    }

    #[test]
    fn anchor_reads_named_edges_defaulting_absent_ones_to_false() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", anchor = { top = true, left = true } }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_anchor(&props).unwrap(), Anchor { top: true, right: false, bottom: false, left: true });
    }

    #[test]
    fn monitor_absent_defaults_to_all() {
        let props = HashMap::new();
        assert_eq!(parse_monitor(&props).unwrap(), "All");
    }

    #[test]
    fn monitor_reads_the_string() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", monitor = "eDP-1" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_monitor(&props).unwrap(), "eDP-1");
    }

    #[test]
    fn surface_topology_combines_id_layer_anchor_monitor_and_namespace() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "panel", id = "bar", layer = "Top", anchor = { top = true }, monitor = "eDP-1", namespace = "my-bar" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let topology = surface_topology(&props).unwrap();
        assert_eq!(
            topology,
            SurfaceTopology {
                id: "bar".to_string(),
                layer: LayerKind::Top,
                anchor: Anchor { top: true, right: false, bottom: false, left: false },
                monitor: "eDP-1".to_string(),
                namespace: "my-bar".to_string(),
            }
        );
    }

    #[test]
    fn namespace_absent_defaults_to_oblisk_dash_id() {
        // § 6.1: "Defaults to `oblisk-{id}`". Before docs/adr/0038 the namespace was hardcoded per
        // Rust-owned role, so no compositor rule could name a user's own panel.
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", id = "launcher", layer = "Overlay" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_namespace(&props, "launcher").unwrap(), "oblisk-launcher");
        assert_eq!(surface_topology(&props).unwrap().namespace, "oblisk-launcher");
    }

    #[test]
    fn a_signal_in_namespace_on_a_panel_is_rejected_like_every_other_topology_field() {
        // `get_layer_surface` fixes the namespace at creation and no request changes it on a live
        // surface, so it is a topology field by protocol (`CONTEXT.md`, Topology change) and a
        // handle that could drift after the diff has nothing left to re-check it.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(lua.create_string("x").unwrap()), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("ns", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", id = "bar", layer = "Top", namespace = ns }"#).eval().unwrap();
        let props = props_from_table(&table);
        // `resolve_properties` must have copied it through raw for the parser to see a handle.
        let resolved = resolve_properties(&props, "panel", &lua).unwrap();
        assert!(matches!(parse_namespace(&resolved, "bar").unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "namespace"));
    }

    #[test]
    fn keyboard_interactivity_absent_defaults_to_none() {
        let props = HashMap::new();
        assert_eq!(parse_keyboard_interactivity(&props).unwrap(), KeyboardInteractivity::None);
    }

    #[test]
    fn keyboard_interactivity_reads_each_of_the_three_protocol_modes() {
        let lua = lua();
        for (text, expected) in [
            ("None", KeyboardInteractivity::None),
            ("OnDemand", KeyboardInteractivity::OnDemand),
            ("Exclusive", KeyboardInteractivity::Exclusive),
        ] {
            let table: mlua::Table = lua.load(format!(r#"return {{ kind = "panel", keyboard_interactivity = "{text}" }}"#)).eval().unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_keyboard_interactivity(&props).unwrap(), expected);
        }
    }

    #[test]
    fn an_unrecognized_keyboard_interactivity_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", keyboard_interactivity = "Always" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(parse_keyboard_interactivity(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "keyboard_interactivity"));
    }

    #[test]
    fn a_signal_in_keyboard_interactivity_resolves_rather_than_being_rejected() {
        // The other side of the namespace test: layer-shell's `set_keyboard_interactivity` is
        // valid on a live surface, so this is an in-place field and a `Signal` in it is legal
        // (docs/adr/0044 decision 1). A launcher flipping focus mode from Lua is the point.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(lua.create_string("Exclusive").unwrap()), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("mode", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", id = "bar", layer = "Top", keyboard_interactivity = mode }"#).eval().unwrap();
        let props = props_from_table(&table);
        let resolved = resolve_properties(&props, "panel", &lua).unwrap();
        assert_eq!(parse_keyboard_interactivity(&resolved).unwrap(), KeyboardInteractivity::Exclusive);
    }

    #[test]
    fn exclusive_absent_defaults_to_false() {
        let props = HashMap::new();
        assert!(!parse_exclusive(&props).unwrap());
    }

    #[test]
    fn exclusive_reads_the_boolean_and_rejects_anything_else() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", exclusive = true }"#).eval().unwrap();
        assert!(parse_exclusive(&props_from_table(&table)).unwrap());

        let bad: mlua::Table = lua.load(r#"return { kind = "panel", exclusive = 32 }"#).eval().unwrap();
        assert!(matches!(parse_exclusive(&props_from_table(&bad)).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "exclusive"));
    }

    #[test]
    fn panel_spec_reads_every_layer_surface_field_in_one_pass() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "panel", id = "dock", layer = "Bottom", anchor = { bottom = true },
                   monitor = "DP-1", namespace = "my-dock", keyboard_interactivity = "OnDemand",
                   exclusive = true, margin = { top = 4, left = 8 }, width = "Fill", height = 48 }"#,
            )
            .eval()
            .unwrap();
        let spec = panel_spec(&props_from_table(&table)).unwrap();

        assert_eq!(spec.topology.id, "dock");
        assert_eq!(spec.topology.layer, LayerKind::Bottom);
        assert_eq!(spec.topology.anchor, Anchor { top: false, right: false, bottom: true, left: false });
        assert_eq!(spec.topology.monitor, "DP-1");
        assert_eq!(spec.topology.namespace, "my-dock");
        assert_eq!(spec.keyboard_interactivity, KeyboardInteractivity::OnDemand);
        assert!(spec.exclusive);
        assert_eq!(spec.margin, EdgeInsets { top: 4.0, right: 0.0, bottom: 0.0, left: 8.0 });
        assert_eq!(spec.width, SizeMode::Fill);
        assert_eq!(spec.height, SizeMode::Pixels(48.0));
    }

    #[test]
    fn a_panel_roots_margin_is_the_anchor_offset_and_no_layout_pass_consumes_it() {
        // build-steps.md Phase 20 item 3 item 4: on a `panel` root, `margin` means the layer-shell
        // anchor offset, not layout spacing -- and there is no conflict with layout's own reading
        // of the property because `layout::scene::Scene::apply_one_surface` passes `None` for both
        // parent-margin arguments when it resolves a root, so nothing in layout consumes it. This
        // proves the second half directly: an 80-wide root with a large `margin` still resolves to
        // exactly 80 wide at exactly (0, 0), while `panel_spec` reads the same value as the offset.
        let lua = mlua::Lua::new();
        crate::lua::nodes::register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", layer = "Top", width = 80, height = 20, margin = { top = 12, left = 30 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let spec = panel_spec(&surface.properties).unwrap();
        assert_eq!(spec.margin, EdgeInsets { top: 12.0, right: 0.0, bottom: 0.0, left: 30.0 });

        let mut scene = crate::layout::Scene::new();
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        let instances = vec![crate::layout::instance::SurfaceInstance {
            instance_id: "bar@TEST".to_string(),
            declared_id: "bar".to_string(),
            output: "TEST".to_string(),
            available: crate::layout::LogicalSize { width: 1000.0, height: 500.0 },
        }];
        scene.apply(&[surface], &instances, &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        assert_eq!((root.rect.x, root.rect.y), (0.0, 0.0), "a root's margin must not offset it inside its own surface");
        assert_eq!((root.rect.width, root.rect.height), (80.0, 20.0), "a root's margin must not shrink it either");
    }

    #[test]
    fn node_id_absent_is_none() {
        let props = HashMap::new();
        assert_eq!(parse_node_id(&props).unwrap(), None);
    }

    #[test]
    fn node_id_reads_the_string() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", id = "handle" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_node_id(&props).unwrap(), Some("handle".to_string()));
    }

    #[test]
    fn a_signal_userdata_in_node_id_is_rejected() {
        // docs/adr/0045 decision 1: id is a reconcile identity, decided once at match time, so it
        // rejects a Signal the same way SurfaceTopology's five fields already do.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(parse_node_id(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id"));
    }

    #[test]
    fn resolve_properties_copies_a_structural_field_through_raw_so_it_can_still_be_rejected() {
        // How the carve-out survives resolve-once (build-steps.md Phase 19 item 5): the resolve
        // step skips a structural key entirely rather than resolving it and rejecting afterwards,
        // because a resolved signal is indistinguishable from a literal by then. The rejection
        // itself stays exactly where it was, in the five parsers that read the raw value.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(&node.properties, "rect", &lua).unwrap();

        assert!(matches!(resolved.get("id"), Some(Value::UserData(_))), "id must survive the resolve step unresolved");
        assert!(matches!(parse_node_id(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id"));
    }

    #[test]
    fn a_non_utf8_node_id_is_rejected_rather_than_lossily_converted() {
        // An id is an equality key (docs/adr/0045 decision 1), so a lossy conversion would map
        // every distinct invalid byte onto U+FFFD and make two genuinely different ids compare
        // equal: the duplicate check would reject a valid config, and a fresh child could claim
        // the wrong retained counterpart.
        let lua = lua();
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", lua.create_string(b"\xff").unwrap()).unwrap();
        let props = props_from_table(&table);
        let err = parse_node_id(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "id"),
            "a non-UTF-8 id must be a LayoutError naming the property: {err:?}"
        );
    }

    #[test]
    fn two_distinct_non_utf8_ids_do_not_collapse_onto_one_replacement_character() {
        // The specific collision the lossy conversion produced: "\xFF" and "\xFE" both became
        // U+FFFD. Both are now refused outright, so neither can stand in for the other.
        let lua = lua();
        for byte in [b"\xff".as_slice(), b"\xfe".as_slice()] {
            let table = lua.create_table().unwrap();
            table.set("kind", "rect").unwrap();
            table.set("id", lua.create_string(byte).unwrap()).unwrap();
            let props = props_from_table(&table);
            assert!(matches!(parse_node_id(&props), Err(LayoutError::InvalidProperty { ref property, .. }) if property == "id"));
        }
    }

    #[test]
    fn a_signal_userdata_in_layer_is_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "panel").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(parse_layer(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer"));
    }

    #[test]
    fn a_signal_in_layer_on_a_non_panel_node_resolves_instead_of_surviving_as_a_handle() {
        // The skip list is kind-aware because `layer`/`anchor`/`monitor` are only ever parsed by
        // `surface_topology`, which `layout::scene`'s `Scene::apply_one_surface` calls on top-level
        // surfaces alone. Skipped unconditionally, a `rect { layer = someSignal }` copied the raw
        // handle into `RetainedNode::properties` with no parser left to reject it, falsifying the
        // "never a Signal" invariant `layout::scene::ResolvedNode::properties` hands the paint
        // stage. On a `rect` these are ordinary properties, so they resolve like any other.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        for property in ["layer", "anchor", "monitor"] {
            let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
            let table = lua.create_table().unwrap();
            table.set("kind", "rect").unwrap();
            table.set(property, signal).unwrap();
            let node = deserialize_lua_table(&table).unwrap();

            let resolved = resolve_properties(&node.properties, "rect", &lua).unwrap();

            assert!(
                matches!(resolved.get(property), Some(Value::Boolean(true))),
                "`{property}` on a rect must resolve to the signal's value, got {:?}",
                resolved.get(property)
            );
        }
    }

    #[test]
    fn a_signal_in_layer_on_a_panel_still_survives_raw_for_parse_layer_to_reject() {
        // The other half of kind-awareness: on a `panel`, `parse_layer` does run and does the
        // rejecting, so the skip is still what makes that rejection reachable.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "panel").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(&node.properties, "panel", &lua).unwrap();

        assert!(matches!(resolved.get("layer"), Some(Value::UserData(_))), "layer must survive the resolve step unresolved on a panel");
        assert!(matches!(parse_layer(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer"));
    }

    #[test]
    fn background_absent_is_none() {
        let props = HashMap::new();
        assert_eq!(parse_background(&props).unwrap(), None);
    }

    #[test]
    fn background_six_digit_hex_is_opaque() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#336699" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_background(&props).unwrap(),
            Some(Rgba {
                r: 0x33 as f32 / 255.0,
                g: 0x66 as f32 / 255.0,
                b: 0x99 as f32 / 255.0,
                a: 1.0,
            })
        );
    }

    #[test]
    fn background_eight_digit_hex_carries_its_own_alpha() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#33669980" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_background(&props).unwrap(),
            Some(Rgba {
                r: 0x33 as f32 / 255.0,
                g: 0x66 as f32 / 255.0,
                b: 0x99 as f32 / 255.0,
                a: 0x80 as f32 / 255.0,
            })
        );
    }

    #[test]
    fn background_without_a_leading_hash_is_rejected_naming_the_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", background = "336699" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("must start with `#`")),
            "must name the missing `#`, not just some invalid-property error: {err}"
        );
    }

    #[test]
    fn background_with_the_wrong_digit_count_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#369" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 3")),
            "must be the digit-count rule specifically, naming 3 digits: {err}"
        );
    }

    #[test]
    fn background_with_non_hex_characters_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#zzzzzz" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("only hex digits")),
            "must be the hex-digit rule specifically, not the digit-count rule: {err}"
        );
    }

    #[test]
    fn a_non_ascii_colour_string_gets_the_hex_digit_diagnosis_not_a_byte_count() {
        // CONFIRMED finding: "#日本語" is 3 characters but 9 UTF-8 bytes, so a length check run
        // before the hex-digit check reports "got 9" -- an accurate byte count and a misleading
        // character count. Every byte of a multi-byte sequence fails `is_ascii_hexdigit`, so the
        // hex-digit check catches it first once the checks are reordered, and no digit count is
        // named at all.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#日本語" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("only hex digits") && !detail.contains("got 9")),
            "non-ASCII input must get the hex-digit diagnosis, not a byte-length count: {err}"
        );
    }

    #[test]
    fn background_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", background = {} }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("expected a string")),
            "{err}"
        );
    }

    #[test]
    fn uppercase_hex_parses_the_same_as_lowercase() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#FF0000" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_background(&props).unwrap(),
            Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 })
        );
    }

    #[test]
    fn a_seven_digit_hex_is_rejected_naming_the_digit_count() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#1234567" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 7")),
            "{err}"
        );
    }

    #[test]
    fn a_bare_hash_is_rejected_for_wrong_digit_count() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", background = "#" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_background(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 0")),
            "{err}"
        );
    }

    #[test]
    fn radius_absent_defaults_to_zero() {
        let props = HashMap::new();
        assert_eq!(parse_radius(&props).unwrap(), 0.0);
    }

    #[test]
    fn radius_reads_the_number() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = 6 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_radius(&props).unwrap(), 6.0);
    }

    #[test]
    fn radius_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = true }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("expected a number")),
            "{err}"
        );
    }

    #[test]
    fn a_negative_radius_is_rejected() {
        // Traced consequence (femtovg 0.26 `path.rs:458`): a negative radius silently draws square
        // corners instead of erroring, since anything under 0.1 is treated as unrounded.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = -4 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn radius_above_8192_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = 8193 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_absent_defaults_to_all_zero() {
        let props = HashMap::new();
        assert_eq!(parse_border_width(&props).unwrap(), EdgeInsets::default());
    }

    #[test]
    fn border_width_scalar_broadcasts_to_all_four_edges() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_width = 3 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_border_width(&props).unwrap(),
            EdgeInsets { top: 3.0, right: 3.0, bottom: 3.0, left: 3.0 }
        );
    }

    #[test]
    fn border_width_table_sets_edges_independently() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_width = { top = 2, left = 5 } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_border_width(&props).unwrap(),
            EdgeInsets { top: 2.0, right: 0.0, bottom: 0.0, left: 5.0 }
        );
    }

    #[test]
    fn border_width_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_width = true }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("expected a number or a table")),
            "{err}"
        );
    }

    #[test]
    fn a_negative_border_width_is_rejected() {
        // Traced consequence (femtovg 0.26): a negative border_width clamps to 0.0 and multiplies
        // paint alpha by zero, so the border renders fully transparent with nothing logged.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_width = -4 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_above_8192_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_width = 8193 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_table_form_out_of_range_edge_is_rejected() {
        // The table form delegates to `parse_edge_insets` and range-checks the result afterwards --
        // this is what proves that check actually runs on every edge, not just the scalar form.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_width = { top = 8193 } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_border_width(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn a_signal_nested_in_a_margin_edge_table_is_rejected_naming_the_edge() {
        // CONFIRMED finding: `resolve_properties` only unwraps a Signal at the top level of the
        // property map, so one nested inside a table value (`margin = { top = someSignal }`)
        // survives into `parse_edge_insets`'s `edge` closure, which used to misreport it as "must
        // be a number, got AnyUserData(Ref(0x...))" instead of the actionable Signal error.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal =
            crate::lua::signal::Signal::new_live(Value::Integer(4), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        let margin = lua.create_table().unwrap();
        margin.set("top", signal).unwrap();
        table.set("margin", margin).unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_edge_insets(&props, "margin").unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "margin.top"
        ));
    }

    #[test]
    fn border_color_absent_is_all_none() {
        let props = HashMap::new();
        assert_eq!(parse_border_color(&props).unwrap(), BorderColor::default());
    }

    #[test]
    fn border_color_scalar_string_broadcasts_to_all_four_edges() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", border_color = "#ff0000" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let red = Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 });
        assert_eq!(
            parse_border_color(&props).unwrap(),
            BorderColor { top: red, right: red, bottom: red, left: red }
        );
    }

    #[test]
    fn border_color_table_sets_edges_independently_leaving_absent_edges_none() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", border_color = { top = "#ff0000", left = "#00ff00" } }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_border_color(&props).unwrap(),
            BorderColor {
                top: Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
                right: None,
                bottom: None,
                left: Some(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            }
        );
    }

    #[test]
    fn border_color_malformed_hex_in_a_table_is_rejected_naming_the_edge() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_color = { top = "not-a-color" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_border_color(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("top") && detail.contains("must start with `#`")),
            "must name the failing edge, not just `border_color`: {err}"
        );
    }

    #[test]
    fn a_malformed_hex_on_a_non_top_edge_names_that_edge() {
        // Fix 4's specific case: before the fix, only the closure's catch-all `other` arm
        // interpolated `{key}` -- the `Value::String` arm (the one `checked_string`'s 64KB cap and
        // `parse_hex_color`'s own errors go through) named neither the edge nor the content.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_color = { right = "not-a-color" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_border_color(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("right")),
            "must name `right`, the edge that actually failed: {err}"
        );
    }

    #[test]
    fn a_signal_nested_in_a_border_color_edge_table_is_rejected_naming_the_edge() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let hex = lua.create_string("#ff0000").unwrap();
        let signal =
            crate::lua::signal::Signal::new_live(Value::String(hex), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        let border_color = lua.create_table().unwrap();
        border_color.set("top", signal).unwrap();
        table.set("border_color", border_color).unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_border_color(&props).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "border_color.top"
        ));
    }

    #[test]
    fn border_color_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", border_color = true }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_border_color(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("expected a string or a table")),
            "{err}"
        );
    }

    #[test]
    fn foreground_absent_defaults_to_white() {
        let props = HashMap::new();
        assert_eq!(
            parse_foreground(&props).unwrap(),
            Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }
        );
    }

    #[test]
    fn foreground_reads_a_hex_colour() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r##"return { kind = "text", foreground = "#00ff0080" }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_foreground(&props).unwrap(),
            Rgba { r: 0.0, g: 1.0, b: 0.0, a: 0x80 as f32 / 255.0 }
        );
    }

    #[test]
    fn foreground_wrong_type_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "text", foreground = 5 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_foreground(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "foreground" && detail.contains("expected a string")),
            "{err}"
        );
    }

    #[test]
    fn two_failing_properties_always_report_the_same_one() {
        // `properties` is a `HashMap`, and std's `RandomState` seeds every instance differently, so
        // iterating it in hash order made the reported property a coin flip: eight runs of one
        // broken config named `beta, alpha, alpha, alpha, alpha, beta, beta, alpha`. A message
        // landing in `rescue`'s `error_log` for a human, or in a bug report, has to be a function of
        // the config alone -- hence the sort in `resolve_properties`. A fresh map per iteration is
        // the point: re-resolving the *same* map would order the same way every time and prove
        // nothing.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                return {
                    kind = "rect",
                    alpha = computed({}, function() error("alpha boom") end),
                    beta = computed({}, function() error("beta boom") end),
                }
                "#,
            )
            .eval()
            .unwrap();
        for _ in 0..8 {
            let props = props_from_table(&table);
            let err = resolve_properties(&props, "rect", &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "alpha"),
                "the same broken config must always name the same property, got: {err}"
            );
        }
    }

    #[test]
    fn oversized_string_property_error_message_is_bounded() {
        // Catches docs/build-steps.md Phase 19 item 13's actual defect: before the fix, this
        // error's `detail` was exactly as long as the offending Lua string (20 MB), because
        // `format!("{value:?}")` formatted the whole thing into the message. A config author
        // scrolling `rescue`'s error_log should see a short line, not a 20 MB one.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = string.rep("Q", 20 * 1024 * 1024) }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { property, detail } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(property, "radius");
        assert!(
            detail.len() < 1024,
            "a 20 MB input must not produce a multi-megabyte error message, got {} bytes",
            detail.len()
        );
    }

    #[test]
    fn oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix() {
        // The length bound above is satisfiable by a helper that just drops all the diagnostic
        // content, which would be a worse fix than the bug: a config author staring at the log
        // needs to be able to tell "my string was too long" apart from "my string was the wrong
        // type entirely." This checks the truncated message still carries the original type tag,
        // a recognizable prefix of the actual bytes, and the real (untruncated) length.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = string.rep("Q", 20 * 1024 * 1024) }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert!(detail.contains("expected a number"), "{detail}");
        assert!(detail.contains("String("), "must still name the rejected type: {detail}");
        assert!(detail.contains("QQQ"), "must show a recognizable prefix of the value: {detail}");
        assert!(
            detail.contains(&(20 * 1024 * 1024).to_string()),
            "must state the real length, or a truncated preview reads as the whole value: {detail}"
        );
    }

    #[test]
    fn short_string_property_error_message_is_unchanged() {
        // The `Value::String` arm is the only one this fix touches (see `preview_for_error`'s doc
        // comment for why). This pins that an ordinary short string, well under the 200-byte
        // preview cap, still renders exactly as mlua's own `Debug` would have before the fix --
        // the common case pays nothing for the oversized-input guard.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = "banana" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(detail, "expected a number, got String(\"banana\")");
    }

    #[test]
    fn non_string_variant_error_message_is_unchanged() {
        // The helper has to pass every other `Value` variant through unchanged -- this is the
        // "shown to work across the match, not just the string case" coverage. `Boolean`'s Debug
        // is bounded by construction, so the fix has no work to do here; pinning the exact string
        // proves `preview_for_error` really does fall through rather than reformatting it.
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", radius = true }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(detail, "expected a number, got Boolean(true)");
    }
}
