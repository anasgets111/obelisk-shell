//! Typed property parsing for the layout engine (build-steps.md Phase 12,
//! `docs/oblisk-idl-api-specs.md` § 5.1). `lua::nodes::VirtualNode` leaves every property as a raw
//! `mlua::Value`; this module turns it into typed, validated properties.
//!
//! A `Value::UserData` holding a `Signal` (§ 1.2) is resolved rather than rejected (ADR-0044
//! decision 1), and every one of those reads happens in exactly one place: [`resolve_properties`],
//! run once per node per pass. The parsers below take plain values and no `&Lua` -- they see a map
//! where every `Signal` has already been read once, so two parsers reading the same property in the
//! same pass see the same answer.
//!
//! That guarantee covers a `Signal` and nothing else: a plain Lua table with an `__index`
//! metamethod is copied in as-is, and every `table.get` a parser makes still runs that metamethod
//! afresh -- see [`parse_edge_insets`]'s `ponytail:` for the hole this leaves and what closing it
//! costs.
//!
//! Resolution happens exactly once per property: a signal resolving to another `Signal` is an
//! error, not a second read. That guard is not a recursion bound -- a computed signal whose getter
//! returns fresh depth on every call recurses through `resolve_and_reconcile` and
//! `deserialize_lua_table` as deep as the getter wants. `layout::scene::MAX_TREE_DEPTH` is what caps
//! that and raises [`LayoutError::TreeTooDeep`].
//!
//! [`SurfaceTopology`]'s five fields and every node's optional `id` (docs/adr/0045 decision 1) are
//! structural carve-outs that keep rejecting a `Signal` outright -- see
//! [`reject_signal_in_structural_field`]. A `panel`'s remaining § 6.1 properties are not carve-outs:
//! layer-shell accepts each on a live surface, so a `Signal` in one resolves normally.

use std::collections::{HashMap, HashSet};

use mlua::{Lua, Value};

use crate::image::Fit;
use crate::lua::marshal;
use crate::lua::nodes::{VirtualNode, deserialize_lua_table};
use crate::lua::signal::{self, is_signal};
use crate::text::snap::LogicalRect;

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
/// 200 bytes, not `marshal::MAX_STRING_BYTES` (64KB): that cap answers how much of a *string
/// property* is a legitimate value; this one answers how much of a rejected value belongs in one
/// line of `rescue`'s `error_log` (§ 2.10) -- enough bytes for a human to recognize the value, not
/// a paste buffer.
const MAX_ERROR_VALUE_PREVIEW_BYTES: usize = 200;

/// `pub(crate)` since `layout::scene`'s `list` node (build-steps.md Phase 19 item 12) rejects a bad
/// `source`/`itemfn`/`key` value from outside this module and needs the same bounded preview.
///
/// Renders a `Value` for an [`invalid`] detail without ever formatting its `Debug` form in full
/// first. `format!("{value:?}")` on an oversized `Value::String` allocates and escapes the whole
/// thing before any truncation could run -- `rect { radius = string.rep("x", 20 * 1024 * 1024) }`
/// would format 20 MB on the Wayland dispatch thread before the error even reaches `rescue`.
/// `marshal::check_string`'s 64KB cap never runs here: it lives in [`checked_string`], which a
/// value rejected for having the wrong *type* never reaches.
///
/// Measured on this machine, formatting a Lua string of each size against this function:
///
/// | size | `format!("{value:?}")` | this function |
/// |---|---|---|
/// | 1 MB | 1.64 ms | 0.0088 ms |
/// | 20 MB | 23.96 ms | 0.0057 ms |
/// | 100 MB | 93.88 ms | 0.0061 ms |
///
/// Cost here is a function of the cap, not of the input. 23.96 ms is more than a whole frame at
/// 60fps, and `layout::paint` validates a `background` or `radius` while drawing, on the Wayland
/// dispatch thread -- so this runs per frame, not per apply.
///
/// `oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix` is the
/// regression test: a naive format-then-truncate still comes in under a loose timing bound, so what
/// it actually asserts on is that format-then-truncate cannot report the value's true length.
///
/// `Value::String` is the only unbounded `Debug` case in mlua 0.12: every other variant that can
/// hold non-trivial data (`Table`, `Function`, `Thread`, `UserData`) derives or hand-writes a
/// `Debug` over a `ValueRef`, whose own `Debug` is a fixed-width pointer regardless of size.
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
/// before this parser's own range checks (e.g. `parse_size_mode`'s `[0, 8192]`) ever see it --
/// catches a NaN/Inf `f64` or an out-of-2^53-range `Integer`, whether literal or resolved from a
/// `Signal` via [`resolve_properties`].
///
/// `marshal::check_number` alone is not enough: it only guards the `f64` representation, and a
/// finite `f64` like `1e300` sails through it and then overflows to `f32::INFINITY` on the
/// narrowing cast below. A caller with no further range check (e.g. `parse_spacing`) would hand
/// that `Inf` straight into layout arithmetic -- `inf * 0.0` is `NaN`, and `snap_to_physical`'s
/// final `as i32` silently saturates a `NaN` rect to `0` instead of raising an error. So the
/// finiteness check re-runs after the cast, on the `f32`, naming the same property a non-finite
/// literal would.
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
/// `text.foreground`). No 3-digit shorthand, no named colours, no bare digits without `#` -- § 5.2
/// specifies none of them.
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
/// `kind`, so [`reject_signal_in_structural_field`] still sees a raw `Value::UserData` and can
/// refuse it. Resolving then rejecting afterwards is unimplementable: once a signal has been read,
/// the value in the map is indistinguishable from a literal.
///
/// Kind-aware, because a skip is only sound where a parser actually runs to do the rejecting. `id`
/// is skipped on every kind ([`parse_node_id`]/[`parse_surface_id`] read it wherever it appears).
/// `layer`/`anchor`/`monitor`/`namespace` are read by [`surface_topology`] alone, called only on
/// top-level surfaces -- so on a `rect` no parser looks at them, and skipping them there would copy
/// a live `Signal` handle into `layout::scene::RetainedNode::properties`, falsifying the "never a
/// `Signal`" invariant `ResolvedNode::properties` hands to the paint stage. Below a surface these
/// four are ordinary properties and resolve like any other.
///
/// `namespace` joins the carve-out for the same protocol reason `monitor` is already in it:
/// `zwlr_layer_shell_v1::get_layer_surface` fixes a namespace at creation and no request changes it
/// on a live surface. The in-place `panel` fields -- `keyboard_interactivity`, `exclusive`,
/// `margin`, `width`/`height` -- are deliberately *not* here: layer-shell permits changing each on a
/// live surface, so a `Signal` in one resolves normally (docs/adr/0044 decision 1).
///
/// `window`, `popup` and `lock` add nothing to the carve-out (build-steps.md Phase 22 and 23), by
/// the same live-object test: a `window`'s `set_title`/`set_app_id`/`set_min_size`/`set_max_size`
/// are all valid requests on a mapped toplevel; a `popup`'s whole `xdg_positioner` is rebuilt on
/// every open (docs/adr/0049 decision 1), so `parent`/`anchor_rect`/`anchor`/`gravity` are meant to
/// carry a `Signal`; a `lock`'s § 6.4 property list is only `id` and `child`. All three roles' `id`
/// is already covered by the universal arm, since it is a reconcile identity rather than a protocol
/// field.
fn is_structural_property(kind: &str, property: &str) -> bool {
    property == "id" || (kind == "panel" && matches!(property, "layer" | "anchor" | "monitor" | "namespace"))
}

/// One node's raw property map with every `Signal` replaced by its current value (ADR-0044
/// decision 1). Called once per node per pass, at the point that node enters reconciliation;
/// everything downstream -- this module's parsers, `layout::scene`'s sizing/positioning passes, and
/// `RetainedNode::properties` -- reads the result rather than the raw map.
///
/// Once, and once is load-bearing. `Signal::get_value` runs a `computed` signal's Lua closure, and
/// a closure that is not a pure function of unchanged state (`os.clock()`, `math.random`, an
/// accumulator upvalue) answers differently on every call. `margin` used to be read four separate
/// times in one `Scene::apply`, so a row could be measured against one answer and position its
/// child against another. One read per property makes the resolved tree a snapshot of one pass, and
/// stops ADR-0021's per-`get_value` 5ms budget being paid four times over for one property.
///
/// The snapshot is a snapshot of the *signals*, and only of them. A plain table with an `__index`
/// metamethod is copied through as that table, and each `table.get` a parser makes runs the
/// metamethod again, so a `margin` of that shape still measures against one answer and positions
/// against another. See [`parse_edge_insets`]'s `ponytail:`.
///
/// Not the memoization ADR-0044 decision 3 rejects: that decision is about caching *across* pushes,
/// which needs an invalidation rule no push has; this caches nothing beyond the single pass it runs
/// in.
///
/// Per entry:
///
/// - a key [`is_structural_property`] names for this node's `kind` is copied through raw, signal
///   and all;
/// - a `Value::UserData` holding a `Signal` is read through `Signal::get_value` and the *result*
///   stored in its place, under the same rules a parser would apply to a literal;
/// - a result that is itself a `Signal` is an error naming the property, not a second read:
///   chasing it to a fixed point is an unbounded loop on a cyclic construction;
/// - a result of `Value::Nil` **omits the key entirely** -- ADR-0044 decision 1's amendment ("a
///   signal resolving to nil means the property is absent") falls out of the map itself rather
///   than being re-checked in every parser. This matters at boot:
///   `RendererClient::run_startup_evaluation` runs before the poll loop has drained a single
///   inbound frame, so every `shared::CAPABILITIES` signal still reads `nil` at the first
///   `Scene::apply`, and a config binding a bare capability signal must not fail layout there. It
///   also matches a Lua table's own inability to store `nil`, so `visible = nil` in a config and a
///   signal resolving to nil read the same;
/// - everything else, including a `UserData` that is not a `Signal`, is copied through unchanged,
///   left for whichever parser consumes it to reject with its own message.
///
/// Every property resolves, including ones no parser reads today: the resolved map is what the
/// paint stage reads a colour or radius straight off (`ResolvedNode::properties`), and § 5.1 puts
/// no property out of a `Signal`'s reach, so there is no subset safe to skip. A getter that raises
/// fails the whole apply, even for a property nothing downstream looked at -- deferring the error
/// would mean keeping the getter around to re-run later, which is the second read this function
/// exists to prevent.
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
        let Some(signal) = signal::from_userdata(ud) else {
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
/// outright, the same way every parser used to.
///
/// The unifying reason: each is read exactly once per evaluation and a *structural* decision is
/// then made from it and acted on -- where a surface is placed, or which retained node a fresh one
/// is. A `Signal` is free to change between passes, so admitting one here would leave a decision
/// already taken resting on a value that no longer holds, with nothing left to re-check it. Every
/// other property is read for the geometry or appearance of the pass it was read in, so a later
/// change simply produces different output on the next pass.
///
/// Concretely: `surface_topology` runs on every `Scene::apply` so `socket.rs`'s `handle_reevaluate`
/// can diff it against `applied_topology` and choose swap-versus-in-place (ADR-0001) -- a surface
/// could otherwise move layer or monitor with no swap. `id` is `pair_children_by_id_then_position`'s
/// reconcile identity, matched once per `Scene::apply` to pair a fresh child against its retained
/// counterpart -- a value that could change between the match and whatever reads it afterward would
/// make "the same node as last time" itself ambiguous. ADR-0044 decision 1 doesn't carve either out
/// explicitly; it's a gap in the ADR, not a case it considered and rejected.
///
/// This only works because [`resolve_properties`] copies the keys [`is_structural_property`] names
/// through raw: these six parsers are the only ones that read the un-resolved value, since a
/// resolved signal is indistinguishable from a literal by the time it reaches a map.
fn reject_signal_in_structural_field(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

/// Whether `property` currently holds a live [`Signal`] -- the one thing an **unresolved** property
/// map can say that a resolved one cannot: "this pass is not in a position to check it"
/// (docs/adr/0049's second amendment).
///
/// Only ever true on the evaluation-time pass. [`resolve_properties`] reads every `Signal` it is
/// handed and stores the *result* in its place, refusing a result that is itself a `Signal`, so no
/// map that has been through it can hold one -- except under a key [`is_structural_property`]
/// copies through raw, and those keys are exactly the ones
/// [`reject_signal_in_structural_field`] refuses outright instead of deferring.
///
/// A parser that consults this is applying the amendment's split: on the pass that reads raw
/// properties (`crate::socket`'s `surface_specs`, which runs before any getter has been called and
/// must not call one), a signal-bound property is skipped and left at the parser's documented
/// placeholder; the authoritative value is re-read from the resolved tree by
/// `App::apply_resolved_state` before anything is built from it. A *literal* is still fully
/// validated on that pass, so a config typo fails fast into docs/adr/0046's `rescue` log rather than
/// surfacing as an `xdg_positioner` protocol error at first open.
///
/// Without this, `anchor_rect = popup_anchor` -- the spelling docs/adr/0050 decision 3 tells a
/// config to write -- would fail the whole evaluation: every parser below rejects a raw
/// `Value::UserData` with a type error otherwise.
fn is_deferred_signal(properties: &HashMap<String, Value>, property: &str) -> bool {
    matches!(properties.get(property), Some(Value::UserData(ud)) if is_signal(ud))
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
    // Deferred on the evaluation-time pass ([`is_deferred_signal`]): § 6.1's `width`/`height` are a
    // layer-shell `set_size`, which docs/adr/0038 decision 2 lists among the requests that are valid
    // on a live surface, so `crate::wayland::App::apply_spec_change` re-derives both from the
    // resolved tree on every pass. Same placeholder an absent property gets. The guard cannot fire
    // below a surface root, where this parser is also used: `resolve_properties` has already
    // replaced every `Signal` in an inner node's map.
    if is_deferred_signal(properties, property) {
        return Ok(SizeMode::Content);
    }
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

/// One numeric field out of a table-valued property (`margin.top`, `anchor_rect.width`,
/// `offset.x`, `min_size.height`). `Ok(None)` means the key is absent, which every caller answers
/// differently: [`parse_edge_insets`] defaults an edge to 0, [`parse_anchor_rect`] defaults an
/// origin to 0 but refuses an absent extent, and [`parse_size_hint`] refuses either axis.
///
/// A `Signal` only resolves at the top level of the property map ([`resolve_properties`] never
/// looks inside a table value), so one surviving into a nested slot is refused outright rather than
/// misreported as "must be a number, got AnyUserData(Ref(0x...))" -- an opaque pointer and a wrong
/// claim about the type. `UnsupportedSignalProperty` already carries the right advice (read it via
/// `:get()` first); `{property}.{key}` names both the property and which field.
fn table_number(property: &str, table: &mlua::Table, key: &str) -> Result<Option<f32>, LayoutError> {
    let v: Value = table.get(key).map_err(|e| invalid(property, e.to_string()))?;
    match v {
        Value::Nil => Ok(None),
        Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!("{property}.{key}"))),
        other => value_as_f32(property, &other)?
            .ok_or_else(|| invalid(property, format!("`{key}` must be a number, got {}", preview_for_error(&other))))
            .map(Some),
    }
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
/// Scalar shorthand -- a bare number broadcasts to all four edges -- shared by `margin`, `padding`
/// and `border_width` (docs/build-steps.md Phase 19 item 15). Carries no range check of its own:
/// see [`check_geometry_range`]'s doc comment for why `border_width` keeps a bound this function
/// does not apply to `margin`/`padding`.
pub fn parse_edge_insets(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<EdgeInsets, LayoutError> {
    // Deferred on the evaluation-time pass, for [`parse_size_mode`]'s reason: on a `panel` root
    // `margin` is the layer-shell anchor offset, which `set_margin` changes on a live surface
    // (docs/adr/0038 decision 2). Zero insets are the placeholder an absent `margin` already takes,
    // and the same "cannot fire below a root" note applies.
    if is_deferred_signal(properties, property) {
        return Ok(EdgeInsets::default());
    }
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
    // An absent edge is 0, which is [`table_number`]'s `None` -- see that function for the nested
    // `Signal` rejection every table-valued property shares.
    let edge = |key: &str| -> Result<f32, LayoutError> { Ok(table_number(property, table, key)?.unwrap_or(0.0)) };
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
/// already enforces for `width`/`height` (§ 5.1's base property table). Traced in femtovg 0.26:
/// `radius = -4` silently draws square corners (`path.rs:458` treats anything under 0.1 as
/// unrounded) and `border_width = -4` clamps to 0.0 and multiplies paint alpha by zero, so both
/// negative ends fail silently rather than raising. The upper end matters most: above roughly
/// 8.4e6, `curve_divisions` (`path/cache.rs:911`) computes `acos(1.0) == 0.0`, divides by it, and
/// `inf as u32` saturates to `u32::MAX` as a stroke-loop bound in `round_join`/`round_cap_start` --
/// billions of iterations and tens of gigabytes of vertices on the Wayland dispatch thread.
///
/// This bound stays private to `radius` and `border_width`, not extended to `margin`/`padding`:
/// § 5.1 gives `margin`/`padding` no "Valid Range" entry at all, unlike `width`/`height`, and
/// `layout::scene`'s `position_children` reads a negative margin the same way CSS does -- subtracted
/// into a child's footprint and slot size, so `margin = -8` deliberately pulls a child closer to (or
/// over) its neighbor. That is layout math, not a femtovg stroke input, with no crash mode like
/// `border_width`'s curve-divisions blowup.
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
/// rule (docs/adr/0044) means a `text` bound to a bare, not-yet-pushed capability signal resolves
/// `content` to absent at boot, since every rostered signal reads `nil` until its first
/// `StateSnapshot` and `run_startup_evaluation` runs before the poll loop drains one. Rejecting
/// that would reject the whole tree and boot a blank shell.
///
/// Accepted cost: a misspelled `content` key now renders an empty node instead of being rejected --
/// the better failure for a shell that has to boot; `oblisk.rescue` still exists for the failures
/// that matter.
pub fn parse_content(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "content")
}

/// `icon.name` (§ 5.2 item 5): a theme name, or an absolute path, which `image::icons::resolve`
/// tells apart. Defaults to `""` for the same boot reason `content` does (docs/adr/0044): a `name`
/// bound to a capability signal is `nil` until that capability's first push, and rejecting the
/// tree over it would fail every config that binds one.
pub fn parse_icon_name(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "name")
}

/// `image.source` (docs/adr/0054 decision 3): an absolute path, never a theme name. The split from
/// [`parse_icon_name`] is the whole difference between the two node kinds, so they do not share a
/// property spelling either.
pub fn parse_image_source(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_optional_string(properties, "source")
}

/// `image.fit` (docs/adr/0055 decision 3). Absent is `cover`; a string that is not one of the three
/// modes is an error rather than a silent fallback, because `fit = "fill"` is a config author
/// reaching for a mode that does not exist and a silently-covered image would hide that.
pub fn parse_fit(properties: &HashMap<String, Value>) -> Result<Fit, LayoutError> {
    let Some(value) = properties.get("fit") else {
        return Ok(Fit::default());
    };
    let Value::String(s) = value else {
        return Err(invalid("fit", format!("expected a string, got {}", preview_for_error(value))));
    };
    let s = checked_string("fit", s)?;
    Fit::from_str(&s).ok_or_else(|| invalid("fit", format!("expected `cover`, `contain` or `stretch`, got {s:?}")))
}

/// The shared shape of every § 5.2 string property that defaults to empty when absent. One
/// function rather than three copies of the same six lines.
fn parse_optional_string(properties: &HashMap<String, Value>, property: &str) -> Result<String, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(String::new());
    };
    match value {
        Value::String(s) => checked_string(property, s),
        other => Err(invalid(property, format!("expected a string, got {}", preview_for_error(other)))),
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

/// Absent `size` defaults to 12.0, the same nil-rule rationale as [`parse_content`] (docs/adr/0044's
/// amendment banner): `icon` was the second property the amendment names as still failing after
/// decision 1's nil rule alone. Same accepted cost: `icon { sizee = 24 }` now renders a
/// 12.0-sized icon instead of being rejected.
///
/// § 5.2 documents `size` with no default of its own, so this matches [`parse_font_size`]'s
/// default: `text` and `icon` are the two leaf kinds sized by one numeric property, so an icon
/// dropped inline with default-sized text lands at the same visual scale.
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
/// `Scene::apply`'s `HashMap` (`docs/oblisk-layout-engine-geometry.md` § 4). Since docs/adr/0045,
/// this same property is also the surface's *reconcile* identity: the root of a tree is the one
/// node whose retained counterpart is found by key lookup rather than by [`parse_node_id`]'s
/// per-parent pairing, because a surface has no parent to be scoped within -- decision 5 is
/// explicit that this is the same mechanism restated at the level below, not a second one.
pub fn parse_surface_id(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "id", None)
}

/// The optional `id` base property on every node kind, one level below a surface's root
/// (docs/adr/0045 decisions 1-2). `None` means "no id" and is not an error --
/// `pair_children_by_id_then_position` pairs a child that carries none positionally against the
/// other id-less children (ADR-0023's original rule applied to that subsequence). Adding or
/// dropping an `id` is a change of identity, not a cosmetic edit: the retained counterpart is
/// retired and a new node allocated. Rejects a `Signal` via [`reject_signal_in_structural_field`]
/// for the same reason [`parse_surface_id`] does: this is a reconcile identity, decided once at
/// match time, not a value that should drift between the fresh tree and whatever the match
/// produces.
///
/// Non-UTF-8 bytes are refused rather than converted, unlike [`checked_string`]'s lossy handling of
/// display-oriented properties like `content`. An id is an *equality key*: with `to_string_lossy`,
/// `"\xFF"` and `"\xFE"` both become `U+FFFD` and two genuinely distinct ids compare equal, so
/// `pair_children_by_id_then_position`'s duplicate check would reject a valid config and a fresh
/// child could claim the wrong retained counterpart.
///
/// Scoping ("unique among siblings, not across the tree") and duplicate rejection are
/// `pair_children_by_id_then_position`'s job, not this parser's: a duplicate can only be detected
/// by comparing this node's id against its siblings', which this function has no visibility into.
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
/// [`parse_surface_id`].
///
/// Validates rather than passing the raw string through: `crate::wayland::App::create_panel`
/// creates one layer surface per instance straight from this value (docs/adr/0038 decision 1), so
/// an unrecognized string is a config error the author must see rather than a silent fall to some
/// default layer -- a typo'd `layer = "Toop"` that quietly stacked a bar on `Background` would be a
/// far worse failure than a rejected config, because nothing on screen would say why.
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
/// which makes every `panel` addressable from a compositor config without the author naming one.
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
    // Deferred on the evaluation-time pass ([`is_deferred_signal`]), same split as [`parse_title`]'s:
    // this doc comment's own argument is what makes it a deferral rather than a rejection, since a
    // field valid on a live surface is one only the resolved pass is in a position to read.
    if is_deferred_signal(properties, "keyboard_interactivity") {
        return Ok(KeyboardInteractivity::None);
    }
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
    // Deferred on the evaluation-time pass for [`parse_keyboard_interactivity`]'s reason:
    // `set_exclusive_zone` is valid on a live surface, so `exclusive = hide_bar` is a config § 5.1
    // permits and only this pass cannot read. `false` is the placeholder an absent `exclusive` takes.
    if is_deferred_signal(properties, "exclusive") {
        return Ok(false);
    }
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
/// (§ 6.1, build-steps.md Phase 20 item 3). `crate::socket`'s `surface_specs` builds one per
/// declared `panel`; `layout::instance::expand_instances` turns them into per-output instances, and
/// `crate::wayland::App::create_panel` is what actually binds them.
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

/// § 6.2's `title`, the string the compositor shows in a task bar or window list. Absent is the
/// empty string, which is exactly what a toplevel that never sends `set_title` has: § 6.2
/// documents no default, and substituting the `id` would put an internal identifier in the user's
/// task switcher.
///
/// In-place, deliberately outside [`is_structural_property`]'s carve-out: `xdg_toplevel::set_title`
/// is a request on a live toplevel, so a `Signal` here resolves like any other property
/// (docs/adr/0044 decision 1, and § 6.2 spells the `string`/`Signal` union out) and the next pass
/// simply sends the new title.
pub fn parse_title(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    // Deferred rather than rejected on the evaluation-time pass ([`is_deferred_signal`]): § 6.2
    // spells `title` as `string`/`Signal`, and `parse_string_property`'s `Signal` refusal is meant
    // for the topology fields it also backs. Same placeholder an absent `title` gets, and for the
    // same reason -- a toplevel that never sends `set_title` has no title -- since `show_window`
    // builds the real one from the resolved spec.
    if is_deferred_signal(properties, "title") {
        return Ok(String::new());
    }
    parse_string_property(properties, "title", Some(""))
}

/// § 6.2's `app_id`, the string a compositor matches its own window rules against. Defaults to
/// `"oblisk-{id}"` for exactly the reason [`parse_namespace`] defaults the same way: it is the
/// toplevel's half of the same problem, and without a default nobody could write a `windowrule`
/// against their own window without naming an app id by hand.
///
/// **Not** an [`is_structural_property`] carve-out, and the protocol decides that: `xdg-shell.xml`'s
/// own `set_app_id` description says a request "can be sent after the xdg_toplevel has been mapped
/// to update the property" -- it changes on a live object, the test `keyboard_interactivity` passes
/// and `namespace` fails (`get_layer_surface` fixes a namespace at creation; `set_app_id` fixes
/// nothing). A `window`'s `id` is a carve-out on every kind regardless: it is the reconcile
/// identity, not a protocol field (docs/adr/0045 decision 1).
pub fn parse_app_id(properties: &HashMap<String, Value>, id: &str) -> Result<String, LayoutError> {
    let default = format!("oblisk-{id}");
    // Deferred on the evaluation-time pass for [`parse_title`]'s reason: `set_app_id` is a request
    // on a live toplevel, so § 5.1's blanket "any property accepts a `Signal`" applies and only
    // this pass is unable to read it.
    if is_deferred_signal(properties, "app_id") {
        return Ok(default);
    }
    parse_string_property(properties, "app_id", Some(&default))
}

/// § 6.2's `min_size`/`max_size` value, `{ width, height }`. Advisory, in the spec's own words and
/// the protocol's: "The client should not rely on the compositor to obey the maximum size." So
/// nothing in `layout` clamps a resolved tree against these -- they are carried to
/// `set_min_size`/`set_max_size` and no further.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeHint {
    pub width: f32,
    pub height: f32,
}

/// `Ok(None)` when the property is absent, which is not the same as `Some(0, 0)`: absent means the
/// engine never sends the request at all, while a zero means "no expected maximum size in the given
/// dimension" in a request that *was* sent.
///
/// Both axes are required when the table is present. A `min_size` naming only a width is a config
/// typo, not a request to leave the height unconstrained -- that spelling is an explicit `0`.
fn parse_size_hint(properties: &HashMap<String, Value>, property: &str) -> Result<Option<SizeHint>, LayoutError> {
    // Deferred on the evaluation-time pass, and `None` is the honest placeholder: the request is
    // simply not sent from a spec built there, and `show_window` sends the resolved one.
    if is_deferred_signal(properties, property) {
        return Ok(None);
    }
    let Some(value) = properties.get(property) else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid(property, format!("expected a `{{ width, height }}` table, got {}", preview_for_error(value))));
    };
    let axis = |key: &str| -> Result<f32, LayoutError> {
        let n = table_number(property, table, key)?
            .ok_or_else(|| invalid(property, format!("`{key}` is required -- a size hint names both axes, or use 0 for an unconstrained one")))?;
        // Negative is the one value the requests refuse outright ("Using strictly negative values
        // for width or height will result in an invalid_size error"); the upper end is § 5.1's own
        // `[0, 8192]`, already enforced by `parse_size_mode` for the same quantity on the same node.
        if !(0.0..=8192.0).contains(&n) {
            return Err(invalid(property, format!("`{key}` must be within [0, 8192], got {n}")));
        }
        Ok(n)
    };
    Ok(Some(SizeHint { width: axis("width")?, height: axis("height")? }))
}

/// `set_max_size`: "Requesting a maximum size to be smaller than the minimum size of a surface is
/// illegal and will result in an invalid_size error." Zero means unset in that request, so a zero
/// maximum is not a maximum below the minimum.
///
/// Checked here, per axis, so a config typo is a [`LayoutError`] a human reads out of `rescue`'s
/// `error_log` (§ 2.10) rather than a protocol error that takes the Wayland connection -- and the
/// whole shell -- down with it. Same call build-steps.md Phase 20's `ambiguous_zero_axis` made for
/// a singly-anchored layer surface's `set_size(0)`.
fn check_max_size_above_min(min: Option<SizeHint>, max: Option<SizeHint>) -> Result<(), LayoutError> {
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(());
    };
    for (axis, min_n, max_n) in [("width", min.width, max.width), ("height", min.height, max.height)] {
        if max_n > 0.0 && max_n < min_n {
            return Err(invalid(
                "max_size",
                format!("`{axis}` is {max_n}, below `min_size`'s {min_n} -- a maximum under the minimum raises xdg_toplevel's invalid_size"),
            ));
        }
    }
    Ok(())
}

/// Everything one `xdg_toplevel` needs, read off a `window` node's properties in one pass (§ 6.2,
/// build-steps.md Phase 22), the way [`PanelSpec`] does for a layer surface.
///
/// A `window` is a **top-level** node returned from `shell.lua`, a sibling of `panel`, not
/// something nested inside a panel's child tree (docs/adr/0040 decision 1).
///
/// **No `WindowTopology`.** [`PanelSpec`] carries one because five of a panel's fields are fixed at
/// `get_layer_surface` time. A toplevel's are not: `set_title`, `set_app_id`, `set_min_size` and
/// `set_max_size` are all requests on a live toplevel, and `visible` creates and destroys the
/// object rather than swapping the generation (docs/adr/0049 decisions 1-3). What is left is `id`,
/// and an `id` changing is adding one declaration and removing another, which docs/adr/0001 and
/// docs/adr/0049 decision 3 already route to a swap on the *declared set*.
///
/// § 6.2 gives a `window` no `monitor`: the compositor places a toplevel, so unlike a `panel` one
/// declaration is one Wayland object, never one per output (docs/adr/0038 decision 3).
///
/// `on_close` and `visible` are absent here for the same reasons [`PanelSpec`] omits their
/// equivalents. A callback rides along untouched in `layout::scene::RetainedNode::properties`,
/// exactly as `button`'s `on_click` does and where docs/adr/0050's input path reads it; `visible`
/// is § 5.1 base state on every node, parsed by [`parse_visible`] in the reconcile walk.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// Advisory (§ 6.2): parsed and carried, never enforced against the resolved tree.
    pub min_size: Option<SizeHint>,
    pub max_size: Option<SizeHint>,
}

pub fn window_spec(properties: &HashMap<String, Value>) -> Result<WindowSpec, LayoutError> {
    let id = parse_surface_id(properties)?;
    let app_id = parse_app_id(properties, &id)?;
    let min_size = parse_size_hint(properties, "min_size")?;
    let max_size = parse_size_hint(properties, "max_size")?;
    check_max_size_above_min(min_size, max_size)?;
    Ok(WindowSpec {
        id,
        title: parse_title(properties)?,
        app_id,
        min_size,
        max_size,
    })
}

/// § 6.3's `anchor` and `gravity`, which share one value set. `layout`'s own enum rather than
/// `xdg_positioner`'s `Anchor`/`Gravity`, holding the same deliberate boundary [`LayerKind`] and
/// [`KeyboardInteractivity`] already hold: this module carries no Wayland types, and
/// `crate::wayland` maps at the single call site that builds a positioner.
///
/// `Center` is the one value with no entry of its own in the protocol enums, which run
/// `none`(0) / `top` / `bottom` / `left` / `right` and the four corners. It maps to `none`, and the
/// XML says why that is a translation rather than a fudge: with no edge specified the anchor point
/// is "in the center of the anchor rectangle", and a gravity of `none` centers the surface "over
/// the anchor point on any axis that had no gravity specified". § 6.3's `"Center"` is that
/// behaviour under a name a config author can guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupAnchor {
    /// The protocol's `none`, which is also its default for both requests.
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// One parser, two callers: § 6.3 gives `anchor` and `gravity` the same value set and the protocol
/// gives them identical enums, so `property` exists only to name which one a config got wrong.
///
/// Absent defaults to [`PopupAnchor::Center`], the protocol's own default rather than a choice
/// invented here -- unlike [`parse_constraint_adjustment`], where § 6.3 departs from the protocol
/// default on purpose.
pub fn parse_popup_anchor(properties: &HashMap<String, Value>, property: &str) -> Result<PopupAnchor, LayoutError> {
    if is_deferred_signal(properties, property) {
        return Ok(PopupAnchor::Center);
    }
    let Some(value) = properties.get(property) else {
        return Ok(PopupAnchor::Center);
    };
    let Value::String(s) = value else {
        return Err(invalid(property, format!("expected a string, got {}", preview_for_error(value))));
    };
    match checked_string(property, s)?.as_str() {
        "Top" => Ok(PopupAnchor::Top),
        "Bottom" => Ok(PopupAnchor::Bottom),
        "Left" => Ok(PopupAnchor::Left),
        "Right" => Ok(PopupAnchor::Right),
        "TopLeft" => Ok(PopupAnchor::TopLeft),
        "TopRight" => Ok(PopupAnchor::TopRight),
        "BottomLeft" => Ok(PopupAnchor::BottomLeft),
        "BottomRight" => Ok(PopupAnchor::BottomRight),
        "Center" => Ok(PopupAnchor::Center),
        other => Err(invalid(
            property,
            format!(
                "unknown popup anchor `{other}` -- expected \"Top\", \"Bottom\", \"Left\", \"Right\", \"TopLeft\", \"TopRight\", \"BottomLeft\", \"BottomRight\", or \"Center\""
            ),
        )),
    }
}

/// § 6.3's `constraint_adjustment`, as a set of six independent permissions rather than as the
/// array a config writes it as. The array's *order* carries no meaning and this type must not let
/// it look as though it does: the XML fixes the precedence inside the compositor ("The adjustments
/// can be combined, according to a defined precedence: 1) Flip, 2) Slide, 3) Resize") and the
/// request itself takes a bitmask, so `{ "SlideX", "FlipY" }` and `{ "FlipY", "SlideX" }` are one
/// value. Six booleans say that. A `Vec` of adjustments would preserve an ordering the protocol
/// discards and invite a reader to think the config chose the precedence.
///
/// [`Default`] is § 6.3's `{ "FlipY", "SlideX" }` -- dropdown behaviour, and deliberately **not**
/// the protocol's own default of no adjustment at all (docs/adr/0040 decision 3). An explicitly
/// empty array is how a config asks for [`ConstraintAdjustment::NONE`] back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstraintAdjustment {
    pub slide_x: bool,
    pub slide_y: bool,
    pub flip_x: bool,
    pub flip_y: bool,
    pub resize_x: bool,
    pub resize_y: bool,
}

impl ConstraintAdjustment {
    /// The protocol's own default: never move the popup, even when it falls off-screen. What an
    /// explicitly empty array parses to, and the base [`parse_constraint_adjustment`] accumulates
    /// named entries onto.
    pub const NONE: Self = Self {
        slide_x: false,
        slide_y: false,
        flip_x: false,
        flip_y: false,
        resize_x: false,
        resize_y: false,
    };
}

impl Default for ConstraintAdjustment {
    fn default() -> Self {
        Self { flip_y: true, slide_x: true, ..Self::NONE }
    }
}

pub fn parse_constraint_adjustment(properties: &HashMap<String, Value>) -> Result<ConstraintAdjustment, LayoutError> {
    if is_deferred_signal(properties, "constraint_adjustment") {
        return Ok(ConstraintAdjustment::default());
    }
    let Some(value) = properties.get("constraint_adjustment") else {
        return Ok(ConstraintAdjustment::default());
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            "constraint_adjustment",
            format!("expected an array table, got {}", preview_for_error(value)),
        ));
    };
    let mut adjustment = ConstraintAdjustment::NONE;
    for entry in table.sequence_values::<Value>() {
        let entry = entry.map_err(|e| invalid("constraint_adjustment", e.to_string()))?;
        let Value::String(s) = entry else {
            return Err(invalid(
                "constraint_adjustment",
                format!("expected a string entry, got {}", preview_for_error(&entry)),
            ));
        };
        // Setting a flag rather than pushing: a repeat is a no-op and the position in the array is
        // never read, which is the type's whole point.
        match checked_string("constraint_adjustment", &s)?.as_str() {
            "SlideX" => adjustment.slide_x = true,
            "SlideY" => adjustment.slide_y = true,
            "FlipX" => adjustment.flip_x = true,
            "FlipY" => adjustment.flip_y = true,
            "ResizeX" => adjustment.resize_x = true,
            "ResizeY" => adjustment.resize_y = true,
            other => {
                return Err(invalid(
                    "constraint_adjustment",
                    format!(
                        "unknown adjustment `{other}` -- expected \"SlideX\", \"SlideY\", \"FlipX\", \"FlipY\", \"ResizeX\", or \"ResizeY\""
                    ),
                ));
            }
        }
    }
    Ok(adjustment)
}

/// § 6.3's `offset`, the pixel nudge applied after anchor and gravity. Signed on purpose: the XML's
/// worked example adds the offset to the derived anchor point, so pulling a dropdown up or left
/// needs a negative and `set_offset` refuses neither sign.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PopupOffset {
    pub x: f32,
    pub y: f32,
}

pub fn parse_popup_offset(properties: &HashMap<String, Value>) -> Result<PopupOffset, LayoutError> {
    if is_deferred_signal(properties, "offset") {
        return Ok(PopupOffset::default());
    }
    let Some(value) = properties.get("offset") else {
        return Ok(PopupOffset::default());
    };
    let Value::Table(table) = value else {
        return Err(invalid("offset", format!("expected an `{{ x, y }}` table, got {}", preview_for_error(value))));
    };
    let axis = |key: &str| -> Result<f32, LayoutError> { Ok(table_number("offset", table, key)?.unwrap_or(0.0)) };
    Ok(PopupOffset { x: axis("x")?, y: axis("y")? })
}

/// § 6.3's `anchor_rect`, in the parent surface's logical coordinates. [`LogicalRect`] rather than
/// a type of its own: `crate::wayland`'s `rect_table` builds `on_click`'s single argument out of a
/// `LogicalRect` (docs/adr/0050 decision 3), and § 6.3 says this property is "normally passed
/// straight from the rect `button`'s `on_click` hands back", so the same rect round-trips through
/// the config and lands back in the type it left as.
///
/// Required. `x`/`y` default to 0 when the table omits them -- an origin at the parent's own
/// top-left corner is a legitimate rect -- but `width`/`height` do not, because a zero size is
/// precisely the failure this parser exists to catch. Two different protocol errors sit behind
/// that: `set_anchor_rect` itself raises `invalid_input` only on a *negative* size, while a *zero*
/// size leaves the positioner incomplete ("must have a non-zero size set by set_size, and a
/// non-zero anchor rectangle set by set_anchor_rect"), which raises `invalid_positioner` later, at
/// `get_popup`. Either way one config typo would take the Wayland connection down for the whole
/// shell, so both are a [`LayoutError`] here.
///
/// What [`parse_anchor_rect`] and [`parse_popup_extent`] answer for a property whose value is a
/// live `Signal`, which only the evaluation-time pass ever sees ([`is_deferred_signal`]). One
/// logical pixel rather than zero: `xdg_positioner`'s own description calls a zero size or a zero
/// anchor rectangle incomplete, and a placeholder that would itself be a protocol error is not a
/// placeholder.
///
/// It never reaches a compositor: docs/adr/0049's second amendment re-derives the authoritative
/// spec from the resolved tree in `App::apply_resolved_state`, before `apply_visibility` can create
/// anything from it. The one place it is observable is `expand_instances`, which seeds a popup
/// instance's `available` from the declared `width`/`height` -- a popup that signal-binds a size
/// measures its child against 1x1 until its first configure replaces `available`, and it is not on
/// screen before then, since no `xdg_popup` exists until `visible` resolves true.
const DEFERRED_POPUP_EXTENT: f32 = 1.0;

pub fn parse_anchor_rect(properties: &HashMap<String, Value>) -> Result<LogicalRect, LayoutError> {
    if is_deferred_signal(properties, "anchor_rect") {
        return Ok(LogicalRect { x: 0.0, y: 0.0, width: DEFERRED_POPUP_EXTENT, height: DEFERRED_POPUP_EXTENT });
    }
    let value = properties.get("anchor_rect").ok_or_else(|| {
        invalid(
            "anchor_rect",
            "required for `popup`, got nothing -- a popup with no anchor rectangle raises invalid_positioner at get_popup",
        )
    })?;
    let Value::Table(table) = value else {
        return Err(invalid(
            "anchor_rect",
            format!("expected an `{{ x, y, width, height }}` table, got {}", preview_for_error(value)),
        ));
    };
    let origin = |key: &str| -> Result<f32, LayoutError> { Ok(table_number("anchor_rect", table, key)?.unwrap_or(0.0)) };
    let extent = |key: &str| -> Result<f32, LayoutError> {
        let n = table_number("anchor_rect", table, key)?.ok_or_else(|| {
            invalid("anchor_rect", format!("`{key}` is required and must be greater than 0 -- a zero-size anchor rectangle raises invalid_positioner"))
        })?;
        if !(n > 0.0 && n <= 8192.0) {
            return Err(invalid(
                "anchor_rect",
                format!("`{key}` must be within (0, 8192], got {n} -- a zero or negative anchor rectangle size is a protocol error"),
            ));
        }
        Ok(n)
    };
    Ok(LogicalRect {
        x: origin("x")?,
        y: origin("y")?,
        width: extent("width")?,
        height: extent("height")?,
    })
}

/// § 6.3's `width`/`height`. **Not** [`parse_size_mode`], and that is the whole reason this is a
/// separate parser: a popup has no `"Fill"` because there is nothing for it to fill. An
/// `xdg_popup` is sized by `xdg_positioner::set_size`, which takes a concrete int and "If a zero or
/// negative size is set the invalid_input error is raised", so `"Fill"`, a percent, a `0`, and an
/// omitted axis are four config typos that would otherwise reach the compositor as a protocol
/// error rather than as a rejected config.
///
/// `f32` rather than `i32` even though § 6.3 and the request both say integer: this is the popup's
/// logical size and becomes its child's layout budget, which is `LogicalSize`'s `f32` like every
/// other budget in this engine. It quantizes once, at the `set_size` call site that also knows the
/// output scale -- the same split [`PanelSpec`]'s `width` already documents for layer-shell.
fn parse_popup_extent(properties: &HashMap<String, Value>, property: &str) -> Result<f32, LayoutError> {
    if is_deferred_signal(properties, property) {
        return Ok(DEFERRED_POPUP_EXTENT);
    }
    let value = properties.get(property).ok_or_else(|| {
        invalid(property, "required for `popup`, got nothing -- a popup has no \"Fill\", and set_size raises invalid_input on a zero size")
    })?;
    let n = value_as_f32(property, value)?
        .ok_or_else(|| invalid(property, format!("expected a number, got {} -- a popup has no \"Fill\"", preview_for_error(value))))?;
    if !(n > 0.0 && n <= 8192.0) {
        return Err(invalid(property, format!("must be within (0, 8192], got {n} -- set_size raises invalid_input on a zero or negative size")));
    }
    Ok(n)
}

/// § 6.3's `grab`, defaulting to `true`. A dropdown that cannot be dismissed by clicking outside it
/// is the whole reason docs/adr/0040 decision 2 reached for a real `xdg_popup` instead of a second
/// `panel`, so the default is the behaviour a config author expects rather than the protocol's
/// "only if you ask".
///
/// Whether the grab can actually be taken is not decided here: it needs a serial from a real input
/// event, which only exists for the length of one poll turn (docs/adr/0049's amendment), and a
/// compositor may deny it anyway, which docs/adr/0040 decision 2 records as a normal outcome.
pub fn parse_grab(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    if is_deferred_signal(properties, "grab") {
        return Ok(true);
    }
    let Some(value) = properties.get("grab") else {
        return Ok(true);
    };
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(invalid("grab", format!("expected a boolean, got {}", preview_for_error(other)))),
    }
}

/// Everything one `xdg_popup` and its `xdg_positioner` need, read off a `popup` node's properties
/// in one pass (§ 6.3, build-steps.md Phase 22).
///
/// A `popup` is a **top-level** node returned from `shell.lua`, a sibling of `panel`, and it names
/// its parent surface by id through `parent` rather than sitting inside that surface's child tree.
/// The word "popup" suggests otherwise, which is why it is said here: the protocol roots a popup
/// under a parent at creation (`xdg_surface.get_popup`, or `zwlr_layer_surface_v1.get_popup` for a
/// layer-shell parent), and that parent is a *surface*, not a node.
///
/// **No `PopupTopology`, for a stronger reason than [`WindowSpec`] has.** A popup's Wayland object
/// exists only while it is shown (docs/adr/0049 decision 1) and its `xdg_positioner` is consumed by
/// `get_popup`, so every field on this type is re-read from scratch on every open -- `parent`
/// included, which is why it is an ordinary field and not a carve-out. What remains topology is the
/// declaration itself: adding or removing a `popup` node changes the declared set (docs/adr/0001,
/// docs/adr/0049 decision 3), while opening and closing one is explicitly a value change.
///
/// `on_dismiss` and `visible` are not fields here, same as [`WindowSpec`] and [`PanelSpec`]: the
/// callback rides along in `RetainedNode::properties` like `on_click`, and `visible` is § 5.1 base
/// state read by [`parse_visible`] during the reconcile walk.
#[derive(Debug, Clone, PartialEq)]
pub struct PopupSpec {
    pub id: String,
    /// The `id` of the `panel` or `window` this popup anchors to. Read with the same lossy
    /// conversion [`parse_surface_id`] uses on the other side of the match, so the two agree.
    pub parent: String,
    pub anchor_rect: LogicalRect,
    pub width: f32,
    pub height: f32,
    pub anchor: PopupAnchor,
    pub gravity: PopupAnchor,
    pub constraint_adjustment: ConstraintAdjustment,
    pub offset: PopupOffset,
    pub grab: bool,
}

pub fn popup_spec(properties: &HashMap<String, Value>) -> Result<PopupSpec, LayoutError> {
    // The one field here that is *not* deferred when it holds a `Signal` (see
    // [`is_deferred_signal`], which every other field below consults). `parent` decides which
    // surface `get_popup` roots this popup under and docs/adr/0051 decision 1 pins that to one
    // parent instance chosen at creation, which is the structural-decision test
    // [`reject_signal_in_structural_field`] exists for -- `parse_string_property` applies it.
    let parent = parse_string_property(properties, "parent", None)?;
    if parent.is_empty() {
        return Err(invalid("parent", "must name the `id` of the `panel` or `window` this popup anchors to"));
    }
    Ok(PopupSpec {
        id: parse_surface_id(properties)?,
        parent,
        anchor_rect: parse_anchor_rect(properties)?,
        width: parse_popup_extent(properties, "width")?,
        height: parse_popup_extent(properties, "height")?,
        anchor: parse_popup_anchor(properties, "anchor")?,
        gravity: parse_popup_anchor(properties, "gravity")?,
        constraint_adjustment: parse_constraint_adjustment(properties)?,
        offset: parse_popup_offset(properties)?,
        grab: parse_grab(properties)?,
    })
}

/// § 6.4's `lock`, whose whole property list is `id` and `child` (build-steps.md Phase 23,
/// docs/adr/0052 decision 2). `child` is not a field here for the same reason it is not one on the
/// other three roles: `layout::scene::children_of` walks it into the retained tree, and a spec
/// carries what the Wayland side has to be told, not what the layout engine reads.
///
/// So this is one field, and it stays a struct rather than collapsing into a
/// `SurfaceSpec::Lock(String)`: [`lock_spec`] is where § 6.4's four refusals live, and a bare
/// `String` variant would leave them with no parser to hang off.
///
/// **No `LockTopology`, for a stronger reason than [`PopupSpec`] has.** A lock surface has *no*
/// protocol field at all that a config could set: `ext_session_lock_surface_v1` has exactly one
/// request, `ack_configure`, and the size arrives in the configure rather than being asked for.
/// There is nothing for a topology diff to compare beyond the declaration's existence, which is
/// what [`SurfaceFingerprint::Lock`] holds.
#[derive(Debug, Clone, PartialEq)]
pub struct LockSpec {
    pub id: String,
}

/// § 6.4's parser. Refuses the properties § 6.4 says a `lock` does not have, then reads the one it
/// does.
///
/// **Refusing rather than ignoring is this parser's one real decision.** `visible = false` on a
/// lock screen implies the config decides when the lock is up, and it does not: the compositor
/// creates lock surfaces after `locked` and destroys them at `unlock_and_destroy`, and obeying the
/// property mid-session would destroy a surface the compositor is still showing -- docs/adr/0042
/// records that as what makes the compositor "fall back to rendering a solid color". Ignoring it
/// silently would leave the wrong mental model in place until the author meets it from the other
/// side, locked out by a screen that did not do what they wrote. An error lands in `rescue`'s
/// `error_log` (§ 2.10, docs/adr/0046) at evaluation time, where a human is reading and the session
/// is not locked -- the cheapest place the correction can happen.
///
/// `monitor`, `anchor`, `width` and `height` get the same treatment for a weaker reason: each is
/// inert rather than dangerous (a lock surface's geometry is entirely the compositor's configure,
/// and it expands per output because the protocol says so, not because a `monitor` asked --
/// docs/adr/0052 decision 2), and a property that quietly does nothing is worse unreported than
/// reported.
///
/// The refusals run *before* `id` is read, deliberately: `lock { visible = false }` with no `id`
/// has two problems, and leading with "missing `id`" would hide the one that says the author's
/// whole mental model of the role is wrong.
///
/// Nothing here consults [`is_deferred_signal`]: a refusal tests for the *key*, so a `Signal` under
/// it is refused exactly as a literal is, and § 6.4 leaves a `lock` no movable property for the
/// two-pass split docs/adr/0049's second amendment set up for `window` and `popup` to apply to.
pub fn lock_spec(properties: &HashMap<String, Value>) -> Result<LockSpec, LayoutError> {
    for property in ["visible", "monitor", "anchor", "width", "height"] {
        if properties.contains_key(property) {
            return Err(invalid(
                property,
                format!(
                    "§ 6.4 gives a `lock` no `{property}`: a lock surface covers every connected output, for exactly as long as the compositor holds \
                     the session locked, and none of that is the config's to set (docs/adr/0042, docs/adr/0052 decision 2)"
                ),
            ));
        }
    }
    Ok(LockSpec { id: parse_surface_id(properties)? })
}

/// One declared top-level surface, parsed by whichever § 6 role its `kind` names (docs/adr/0040
/// decision 1). `crate::socket`'s `surface_specs` builds one per node the evaluation returned, and
/// this is the roster every later stage reads: `layout::instance::expand_instances` turns it into
/// surface instances and `crate::wayland::App::create_surfaces` binds them.
///
/// One enum rather than three parallel lists, because the *order* of the declarations is part of
/// the swap fingerprint (see [`SurfaceFingerprint`]) and three lists would lose the interleaving.
/// It is also what keeps a surface's role one `match` away at every consumer instead of a lookup in
/// whichever list happens to hold it.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceSpec {
    Panel(PanelSpec),
    Window(WindowSpec),
    Popup(PopupSpec),
    Lock(LockSpec),
}

impl SurfaceSpec {
    /// The `id` this surface was declared with, whatever its role -- what
    /// `layout::scene::Scene`'s apply matches a `SurfaceInstance` back to its `VirtualNode` by.
    pub fn declared_id(&self) -> &str {
        match self {
            SurfaceSpec::Panel(spec) => &spec.topology.id,
            SurfaceSpec::Window(spec) => &spec.id,
            SurfaceSpec::Popup(spec) => &spec.id,
            SurfaceSpec::Lock(spec) => &spec.id,
        }
    }

    /// This declaration's share of the swap fingerprint.
    pub fn fingerprint(&self) -> SurfaceFingerprint {
        match self {
            SurfaceSpec::Panel(spec) => SurfaceFingerprint::Panel(spec.topology.clone()),
            SurfaceSpec::Window(spec) => SurfaceFingerprint::Window(spec.id.clone()),
            SurfaceSpec::Popup(spec) => SurfaceFingerprint::Popup(spec.id.clone()),
            SurfaceSpec::Lock(spec) => SurfaceFingerprint::Lock(spec.id.clone()),
        }
    }
}

/// One declared surface's share of the topology `crate::socket`'s `handle_reevaluate` diffs to
/// choose a generation swap over an in-place reload (docs/adr/0001, `CONTEXT.md`'s Topology
/// change). Order-sensitive equality on `Vec<SurfaceFingerprint>` is that diff.
///
/// The four roles contribute different amounts, and the protocol decides how much rather than a
/// preference. A `panel` carries all five of [`SurfaceTopology`]'s fields, because
/// `get_layer_surface` fixes every one of them at creation. A `window`, a `popup` and a `lock`
/// carry their `id` alone: everything else they hold is either a request on a live object
/// (`set_title`, `set_app_id`, the two size hints -- see [`WindowSpec`]'s own "no `WindowTopology`"
/// note) or rebuilt per open (the whole `xdg_positioner`, docs/adr/0049 decision 1), so none of it
/// can strand a live object the way a changed `namespace` would. A `lock` reaches the same
/// one-field answer from the other end: § 6.4 gives it `id` and `child` alone, so the only
/// question a topology diff can ask about it is whether it is still there.
///
/// What the three `id` arms *do* catch is the case docs/adr/0049 decision 3 names: adding or
/// removing a declaration is a topology change for every role, including the three whose Wayland
/// object comes and goes inside one generation. Deleting a `lock` mid-session is the sharpest case:
/// it is a topology change, so it is a swap, so docs/adr/0042's rule queues it until unlock and a
/// live lock screen cannot lose its tree underneath it (docs/adr/0052, Consequences).
///
/// The role itself is part of the fingerprint by construction: rewriting `panel { id = "x" }` as
/// `window { id = "x" }` changes the variant, which is a different Wayland object entirely and so a
/// swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceFingerprint {
    Panel(SurfaceTopology),
    Window(String),
    Popup(String),
    Lock(String),
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
/// generated here, once per element of `source`, by calling `itemfn(element)` and deserializing the
/// node table it returns.
///
/// `source` arrives already resolved: `resolve_properties` treats it like any other non-structural
/// property, so a `Signal` there was read exactly once before this function ever runs.
///
/// Without `key`, a generated child gets no `id` at all, so
/// `layout::scene::pair_children_by_id_then_position` matches list items by position -- the same
/// rule an id-less literal child already gets, and exactly what decision 3 specifies. With `key`,
/// `key(element)` -- called on the source element, never on the node `itemfn` built -- becomes that
/// child's `id`, overwriting whatever `id` `itemfn`'s own node table carried: a list item's
/// identity belongs to the list, and honoring an inner `id` instead would let two items that happen
/// to declare the same one collide.
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

/// `textfield.secure_submit` (§ 5.2 item 8): the `{ capability, action }` pair a masked field's
/// committed buffer is addressed to once the focused field submits, instead of the value ever
/// reaching Lua (docs/adr/0005, docs/adr/0027). The submit is Enter on `wl_keyboard`, read natively
/// in `renderer/src/wayland/mod.rs` -- ADR-0027's `zwp_text_input_v3` bridge was the original
/// transport and no longer carries this path at all; see that file's `secure_key_action` for why a
/// password must not travel through an input method. This pair becomes the routing key on a
/// `RendererFrame::SecureSubmit` envelope (docs/adr/0050 decision 4), which is why both fields are
/// required rather than falling back to some default capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureSubmitTarget {
    pub capability: String,
    pub action: String,
}

/// `Ok(None)` when the property is absent -- `secure_submit` is optional even on a masked field
/// (§ 5.2 item 8's own note: without it, a masked value is just unreadable from Lua).
///
/// `secure_submit` is not in [`is_structural_property`]'s carve-out, so a signal-bound value
/// arrives here already resolved -- nothing reconciles a node by its `secure_submit`, so there is
/// no structural decision here for a live-changing signal to undermine.
///
/// `capability`/`action` are refused non-UTF-8 rather than converted lossily, the same call
/// [`parse_node_id`] makes for the same reason: this pair addresses a secret to a Supervisor
/// capability, so a lossy conversion could collapse two distinct byte strings onto the same name
/// and route a password to a capability nobody registered.
pub fn parse_secure_submit(
    properties: &HashMap<String, Value>,
) -> Result<Option<SecureSubmitTarget>, LayoutError> {
    let Some(value) = properties.get("secure_submit") else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            "secure_submit",
            format!("expected a table, got {}", preview_for_error(value)),
        ));
    };
    let field = |key: &str| -> Result<String, LayoutError> {
        let v: Value = table
            .get(key)
            .map_err(|e| invalid("secure_submit", e.to_string()))?;
        let s = match v {
            Value::Nil => return Err(invalid("secure_submit", format!("`{key}` is required"))),
            Value::String(s) => s,
            other => {
                return Err(invalid(
                    "secure_submit",
                    format!("`{key}` must be a string, got {}", preview_for_error(&other)),
                ));
            }
        };
        let s = s.to_str().map(|s| s.to_string()).map_err(|_| {
            invalid(
                "secure_submit",
                format!(
                    "`{key}` must be valid UTF-8 -- it addresses a Supervisor capability, so it cannot be converted lossily"
                ),
            )
        })?;
        if s.is_empty() {
            return Err(invalid("secure_submit", format!("`{key}` must not be empty")));
        }
        Ok(s)
    };
    Ok(Some(SecureSubmitTarget {
        capability: field("capability")?,
        action: field("action")?,
    }))
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
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        let props = HashMap::new();
        assert_eq!(parse_content(&props).unwrap(), "");
    }

    #[test]
    fn a_signal_resolving_to_a_string_satisfies_content() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();

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
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Nil, crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", kind).unwrap();
        table.set(property, signal).unwrap();
        resolve_properties(&props_from_table(&table), kind, lua).unwrap()
    }

    #[test]
    fn a_signal_resolving_to_nil_takes_each_parsers_absent_property_default() {
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
        assert_eq!(parse_content(&props_with_nil_signal(&lua, "text", "content")).unwrap(), "");
        assert_eq!(parse_icon_size(&props_with_nil_signal(&lua, "icon", "size")).unwrap(), 12.0);
        assert_eq!(parse_icon_name(&props_with_nil_signal(&lua, "icon", "name")).unwrap(), "");
        assert_eq!(parse_image_source(&props_with_nil_signal(&lua, "image", "source")).unwrap(), "");
        assert_eq!(parse_fit(&props_with_nil_signal(&lua, "image", "fit")).unwrap(), Fit::Cover);
    }

    #[test]
    fn fit_rejects_a_mode_that_does_not_exist_rather_than_covering_silently() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "image", fit = "fill" }"#).eval().unwrap();
        let err = parse_fit(&props_from_table(&table)).unwrap_err();
        assert!(format!("{err}").contains("cover"), "the error should name the modes that do exist, got {err}");

        let table: mlua::Table = lua.load(r#"return { kind = "image", fit = 3 }"#).eval().unwrap();
        assert!(parse_fit(&props_from_table(&table)).is_err());
    }

    #[test]
    fn an_image_source_that_is_not_a_string_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "image", source = 5 }"#).eval().unwrap();
        assert!(parse_image_source(&props_from_table(&table)).is_err());
        let table: mlua::Table = lua.load(r#"return { kind = "image", source = "/tmp/w.png" }"#).eval().unwrap();
        assert_eq!(parse_image_source(&props_from_table(&table)).unwrap(), "/tmp/w.png");
    }

    #[test]
    fn spacing_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
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
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Number(18.0), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(parse_font_size(&resolve_properties(&node.properties, "text", &lua).unwrap()).unwrap(), 18.0);
    }

    #[test]
    fn icon_size_absent_defaults_to_twelve() {
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
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", id = "launcher", layer = "Overlay" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_namespace(&props, "launcher").unwrap(), "oblisk-launcher");
        assert_eq!(surface_topology(&props).unwrap().namespace, "oblisk-launcher");
    }

    #[test]
    fn a_signal_in_namespace_on_a_panel_is_rejected_like_every_other_topology_field() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(lua.create_string("x").unwrap()), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("ns", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", id = "bar", layer = "Top", namespace = ns }"#).eval().unwrap();
        let props = props_from_table(&table);
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
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
    fn a_signal_in_a_panels_five_in_place_fields_is_deferred_on_the_evaluation_pass() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "panel", id = "bar", layer = "Top",
                            keyboard_interactivity = state("k", "Exclusive"), exclusive = state("e", true),
                            margin = state("m", { top = 4 }), width = state("w", 100), height = state("h", 48) }"#,
            )
            .eval()
            .unwrap();
        let spec = panel_spec(&props_from_table(&table)).unwrap();
        assert_eq!(spec.keyboard_interactivity, KeyboardInteractivity::None, "§ 6.1's default, not the signal's current value");
        assert!(!spec.exclusive);
        assert_eq!(spec.margin, EdgeInsets::default());
        assert_eq!((spec.width, spec.height), (SizeMode::Content, SizeMode::Content));
    }

    #[test]
    fn a_signal_in_a_panels_topology_fields_is_still_rejected_on_the_evaluation_pass() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        for property in ["layer", "anchor", "monitor", "namespace"] {
            let table: mlua::Table = lua
                .load(format!(r#"return {{ kind = "panel", id = "bar", layer = "Top", {property} = state("s", "Top") }}"#))
                .eval()
                .unwrap();
            assert!(
                matches!(panel_spec(&props_from_table(&table)).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == property),
                "{property} is structural"
            );
        }
    }

    #[test]
    fn a_panel_roots_margin_is_the_anchor_offset_and_no_layout_pass_consumes_it() {
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
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(parse_node_id(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id"));
    }

    #[test]
    fn resolve_properties_copies_a_structural_field_through_raw_so_it_can_still_be_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "panel").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(parse_layer(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer"));
    }

    #[test]
    fn a_signal_in_layer_on_a_non_panel_node_resolves_instead_of_surviving_as_a_handle() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
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

    #[test]
    fn secure_submit_absent_is_none() {
        let props = HashMap::new();
        assert_eq!(parse_secure_submit(&props).unwrap(), None);
    }

    #[test]
    fn secure_submit_well_formed_table_parses_capability_and_action() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "network", action = "connect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_secure_submit(&props).unwrap(),
            Some(SecureSubmitTarget {
                capability: "network".to_string(),
                action: "connect".to_string(),
            })
        );
    }

    #[test]
    fn secure_submit_missing_capability_is_invalid_property_naming_the_field() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { action = "connect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("capability")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_missing_action_is_invalid_property_naming_the_field() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "network" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("action")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_empty_capability_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "", action = "connect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("capability")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_empty_action_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "network", action = "" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("action")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_non_table_value_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = "network.connect" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_secure_submit(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "secure_submit"
        ));
    }

    #[test]
    fn secure_submit_non_utf8_capability_is_rejected_rather_than_lossily_converted() {
        let lua = lua();
        let table = lua.create_table().unwrap();
        table.set("kind", "textfield").unwrap();
        let inner = lua.create_table().unwrap();
        inner.set("capability", lua.create_string(b"\xff").unwrap()).unwrap();
        inner.set("action", "connect").unwrap();
        table.set("secure_submit", inner).unwrap();
        let props = props_from_table(&table);
        let err = parse_secure_submit(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "secure_submit"),
            "a non-UTF-8 secure_submit field must be a LayoutError naming the property: {err:?}"
        );
    }


    #[test]
    fn window_spec_reads_every_toplevel_field_in_one_pass() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "settings", title = "Oblisk Settings", app_id = "oblisk.settings",
                            min_size = { width = 320, height = 240 }, max_size = { width = 1280, height = 960 } }"#,
            )
            .eval()
            .unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(
            spec,
            WindowSpec {
                id: "settings".to_string(),
                title: "Oblisk Settings".to_string(),
                app_id: "oblisk.settings".to_string(),
                min_size: Some(SizeHint { width: 320.0, height: 240.0 }),
                max_size: Some(SizeHint { width: 1280.0, height: 960.0 }),
            }
        );
    }

    #[test]
    fn a_window_without_an_id_is_rejected_the_same_way_a_panel_is() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", title = "x" }"#).eval().unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "id"
        ));
    }

    #[test]
    fn window_title_absent_defaults_to_the_empty_string_rather_than_the_id() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "settings" }"#).eval().unwrap();
        assert_eq!(window_spec(&props_from_table(&table)).unwrap().title, "");
    }

    #[test]
    fn window_app_id_absent_defaults_to_oblisk_dash_id() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "settings" }"#).eval().unwrap();
        assert_eq!(window_spec(&props_from_table(&table)).unwrap().app_id, "oblisk-settings");
    }

    #[test]
    fn window_min_size_and_max_size_are_absent_when_undeclared() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "settings" }"#).eval().unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(spec.min_size, None);
        assert_eq!(spec.max_size, None);
    }

    #[test]
    fn a_negative_window_min_size_axis_is_a_layout_error_rather_than_invalid_size() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "window", id = "settings", min_size = { width = -1, height = 240 } }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "min_size"
        ));
    }

    #[test]
    fn a_max_size_below_min_size_is_a_layout_error_rather_than_invalid_size() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "settings", min_size = { width = 800, height = 600 },
                            max_size = { width = 400, height = 600 } }"#,
            )
            .eval()
            .unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "max_size"
        ));
    }

    #[test]
    fn a_zero_max_size_axis_means_unset_and_does_not_collide_with_min_size() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "settings", min_size = { width = 800, height = 600 },
                            max_size = { width = 0, height = 0 } }"#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            window_spec(&props_from_table(&table)).unwrap().max_size,
            Some(SizeHint { width: 0.0, height: 0.0 })
        );
    }

    #[test]
    fn a_window_size_hint_missing_an_axis_is_rejected_naming_the_axis() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "window", id = "settings", min_size = { width = 320 } }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, detail } if property == "min_size" && detail.contains("height")
        ));
    }

    #[test]
    fn a_window_size_hint_that_is_not_a_table_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "window", id = "settings", max_size = 800 }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            window_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "max_size"
        ));
    }

    #[test]
    fn a_signal_in_a_window_title_resolves_because_set_title_is_valid_on_a_live_toplevel() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("Now Playing").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("t", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "w", title = t }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "window", &lua).unwrap();
        assert_eq!(window_spec(&resolved).unwrap().title, "Now Playing");
    }

    #[test]
    fn a_signal_in_a_window_app_id_resolves_because_set_app_id_is_valid_on_a_live_toplevel() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("oblisk.later").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("a", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = "w", app_id = a }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "window", &lua).unwrap();
        assert_eq!(window_spec(&resolved).unwrap().app_id, "oblisk.later");
    }

    #[test]
    fn a_signal_in_a_window_id_is_still_rejected_because_id_is_every_kinds_reconcile_identity() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("w").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("i", signal).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "window", id = i }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "window", &lua).unwrap();
        assert!(matches!(
            window_spec(&resolved).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "id"
        ));
    }


    /// Every popup fixture needs the four required properties, so only the property under test
    /// varies. `extra` is spliced in as further table entries, and a key it repeats *overrides* the
    /// default above it -- a Lua table constructor performs its assignments in order, so
    /// `{ width = 200, width = 0 }` is a table with `width == 0`. That is how a test declares one
    /// bad value without restating the other three good ones.
    fn popup_props(lua: &mlua::Lua, extra: &str) -> HashMap<String, Value> {
        let table: mlua::Table = lua
            .load(format!(
                r#"return {{ kind = "popup", id = "menu", parent = "bar",
                             anchor_rect = {{ x = 10, y = 0, width = 24, height = 24 }},
                             width = 200, height = 300 {extra} }}"#
            ))
            .eval()
            .unwrap();
        props_from_table(&table)
    }

    #[test]
    fn popup_spec_reads_every_positioner_field_in_one_pass() {
        let lua = lua();
        let props = popup_props(
            &lua,
            r#", anchor = "BottomLeft", gravity = "BottomRight",
                constraint_adjustment = { "SlideY", "ResizeX" }, offset = { x = -4, y = 2 }, grab = false"#,
        );
        let spec = popup_spec(&props).unwrap();
        assert_eq!(
            spec,
            PopupSpec {
                id: "menu".to_string(),
                parent: "bar".to_string(),
                anchor_rect: LogicalRect { x: 10.0, y: 0.0, width: 24.0, height: 24.0 },
                width: 200.0,
                height: 300.0,
                anchor: PopupAnchor::BottomLeft,
                gravity: PopupAnchor::BottomRight,
                constraint_adjustment: ConstraintAdjustment { slide_y: true, resize_x: true, ..ConstraintAdjustment::NONE },
                offset: PopupOffset { x: -4.0, y: 2.0 },
                grab: false,
            }
        );
    }

    #[test]
    fn a_popup_without_a_parent_is_rejected() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "popup", id = "menu", anchor_rect = { width = 1, height = 1 }, width = 8, height = 8 }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            popup_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "parent"
        ));
    }

    #[test]
    fn a_popup_with_an_empty_parent_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", parent = """#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "parent"
        ));
    }

    #[test]
    fn a_popup_without_an_anchor_rect_is_a_layout_error_rather_than_invalid_positioner() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "popup", id = "menu", parent = "bar", width = 8, height = 8 }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            popup_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"
        ));
    }

    #[test]
    fn a_zero_size_anchor_rect_is_a_layout_error_rather_than_invalid_positioner() {
        let lua = lua();
        for axis in ["width", "height"] {
            let props = popup_props(&lua, &format!(r#", anchor_rect = {{ x = 0, y = 0, width = 24, height = 24, {axis} = 0 }}"#));
            assert!(
                matches!(popup_spec(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"),
                "a zero anchor_rect {axis} must be a LayoutError"
            );
        }
    }

    #[test]
    fn a_negative_anchor_rect_size_is_a_layout_error_rather_than_invalid_input() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor_rect = { x = 0, y = 0, width = -24, height = 24 }"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"
        ));
    }

    #[test]
    fn an_anchor_rect_omitting_x_and_y_defaults_them_to_zero() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor_rect = { width = 24, height = 24 }"#);
        let rect = popup_spec(&props).unwrap().anchor_rect;
        assert_eq!(rect, LogicalRect { x: 0.0, y: 0.0, width: 24.0, height: 24.0 });
    }

    #[test]
    fn an_anchor_rect_that_is_not_a_table_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor_rect = 24"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor_rect"
        ));
    }

    #[test]
    fn a_popup_missing_its_width_or_height_is_a_layout_error_rather_than_invalid_positioner() {
        let lua = lua();
        for (present, missing) in [("width", "height"), ("height", "width")] {
            let table: mlua::Table = lua
                .load(format!(
                    r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                 anchor_rect = {{ width = 24, height = 24 }}, {present} = 200 }}"#
                ))
                .eval()
                .unwrap();
            let err = popup_spec(&props_from_table(&table)).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == missing),
                "an omitted `{missing}` must be a LayoutError naming it: {err:?}"
            );
        }
    }

    #[test]
    fn a_zero_popup_width_or_height_is_a_layout_error_rather_than_invalid_input() {
        let lua = lua();
        for axis in ["width", "height"] {
            let props = popup_props(&lua, &format!(r#", {axis} = 0"#));
            assert!(
                matches!(popup_spec(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == axis),
                "a zero popup {axis} must be a LayoutError naming it"
            );
        }
    }

    #[test]
    fn a_fill_popup_width_is_rejected_because_a_popup_has_no_fill() {
        let lua = lua();
        let props = popup_props(&lua, r#", width = "Fill""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "width"
        ));
    }

    #[test]
    fn popup_anchor_and_gravity_read_the_same_nine_value_set() {
        let lua = lua();
        for (text, expected) in [
            ("Top", PopupAnchor::Top),
            ("Bottom", PopupAnchor::Bottom),
            ("Left", PopupAnchor::Left),
            ("Right", PopupAnchor::Right),
            ("TopLeft", PopupAnchor::TopLeft),
            ("TopRight", PopupAnchor::TopRight),
            ("BottomLeft", PopupAnchor::BottomLeft),
            ("BottomRight", PopupAnchor::BottomRight),
            ("Center", PopupAnchor::Center),
        ] {
            let props = popup_props(&lua, &format!(r#", anchor = "{text}", gravity = "{text}""#));
            let spec = popup_spec(&props).unwrap();
            assert_eq!(spec.anchor, expected);
            assert_eq!(spec.gravity, expected);
        }
    }

    #[test]
    fn popup_anchor_and_gravity_absent_default_to_center() {
        let lua = lua();
        let spec = popup_spec(&popup_props(&lua, "")).unwrap();
        assert_eq!(spec.anchor, PopupAnchor::Center);
        assert_eq!(spec.gravity, PopupAnchor::Center);
    }

    #[test]
    fn an_unknown_popup_anchor_is_rejected_naming_the_property() {
        let lua = lua();
        let props = popup_props(&lua, r#", anchor = "Middle""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor"
        ));
    }

    #[test]
    fn an_unknown_popup_gravity_is_rejected_naming_the_property() {
        let lua = lua();
        let props = popup_props(&lua, r#", gravity = "Downward""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "gravity"
        ));
    }

    #[test]
    fn constraint_adjustment_absent_defaults_to_flip_y_and_slide_x() {
        let lua = lua();
        assert_eq!(
            popup_spec(&popup_props(&lua, "")).unwrap().constraint_adjustment,
            ConstraintAdjustment { flip_y: true, slide_x: true, ..ConstraintAdjustment::NONE }
        );
    }

    #[test]
    fn constraint_adjustment_reads_every_named_adjustment() {
        let lua = lua();
        let props = popup_props(
            &lua,
            r#", constraint_adjustment = { "SlideX", "SlideY", "FlipX", "FlipY", "ResizeX", "ResizeY" }"#,
        );
        assert_eq!(
            popup_spec(&props).unwrap().constraint_adjustment,
            ConstraintAdjustment { slide_x: true, slide_y: true, flip_x: true, flip_y: true, resize_x: true, resize_y: true }
        );
    }

    #[test]
    fn constraint_adjustment_is_a_set_so_order_and_repetition_do_not_change_it() {
        let lua = lua();
        let ordered = popup_spec(&popup_props(&lua, r#", constraint_adjustment = { "FlipY", "SlideX" }"#)).unwrap();
        let reversed = popup_spec(&popup_props(&lua, r#", constraint_adjustment = { "SlideX", "FlipY", "SlideX" }"#)).unwrap();
        assert_eq!(ordered.constraint_adjustment, reversed.constraint_adjustment);
    }

    #[test]
    fn an_explicitly_empty_constraint_adjustment_is_the_protocols_own_no_adjustment() {
        let lua = lua();
        assert_eq!(
            popup_spec(&popup_props(&lua, r#", constraint_adjustment = {}"#)).unwrap().constraint_adjustment,
            ConstraintAdjustment::NONE
        );
    }

    #[test]
    fn an_unknown_constraint_adjustment_entry_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", constraint_adjustment = { "FlipY", "SlideZ" }"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "constraint_adjustment"
        ));
    }

    #[test]
    fn a_constraint_adjustment_that_is_not_an_array_table_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", constraint_adjustment = "FlipY""#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "constraint_adjustment"
        ));
    }

    #[test]
    fn popup_offset_absent_defaults_to_zero() {
        let lua = lua();
        assert_eq!(popup_spec(&popup_props(&lua, "")).unwrap().offset, PopupOffset { x: 0.0, y: 0.0 });
    }

    #[test]
    fn popup_offset_may_be_negative_on_either_axis() {
        let lua = lua();
        let props = popup_props(&lua, r#", offset = { x = -8, y = -2 }"#);
        assert_eq!(popup_spec(&props).unwrap().offset, PopupOffset { x: -8.0, y: -2.0 });
    }

    #[test]
    fn a_popup_offset_that_is_not_a_table_is_rejected() {
        let lua = lua();
        let props = popup_props(&lua, r#", offset = 4"#);
        assert!(matches!(
            popup_spec(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "offset"
        ));
    }

    #[test]
    fn popup_grab_absent_defaults_to_true() {
        let lua = lua();
        assert!(popup_spec(&popup_props(&lua, "")).unwrap().grab);
    }

    #[test]
    fn popup_grab_reads_the_boolean_and_rejects_anything_else() {
        let lua = lua();
        assert!(!popup_spec(&popup_props(&lua, ", grab = false")).unwrap().grab);
        assert!(matches!(
            popup_spec(&popup_props(&lua, ", grab = 1")).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "grab"
        ));
    }

    #[test]
    fn a_signal_in_a_popup_anchor_rect_resolves_because_the_positioner_is_rebuilt_on_every_open() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "popup", id = "menu", parent = "bar", width = 200, height = 300,
                            anchor_rect = state("menu_anchor", { x = 4, y = 8, width = 16, height = 24 }) }"#,
            )
            .eval()
            .unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "popup", &lua).unwrap();
        assert_eq!(
            popup_spec(&resolved).unwrap().anchor_rect,
            LogicalRect { x: 4.0, y: 8.0, width: 16.0, height: 24.0 }
        );
    }


    /// The same fixture as [`popup_props`] but with every property under test bound to a live
    /// signal instead of a literal, and *not* run through [`resolve_properties`] -- which is
    /// exactly the map `crate::socket`'s `surface_specs` parses.
    fn unresolved_popup_props(lua: &mlua::Lua, extra: &str) -> HashMap<String, Value> {
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(format!(
                r#"return {{ kind = "popup", id = "menu", parent = "bar",
                             anchor_rect = state("a", {{ x = 4, y = 8, width = 16, height = 24 }}),
                             width = 200, height = 300 {extra} }}"#
            ))
            .eval()
            .unwrap();
        props_from_table(&table)
    }

    #[test]
    fn a_signal_bound_popup_property_is_deferred_rather_than_rejected_before_it_resolves() {
        let lua = lua();
        let spec = popup_spec(&unresolved_popup_props(
            &lua,
            r#", width = state("w", 200), height = state("h", 300), anchor = state("an", "Top"),
                gravity = state("g", "Bottom"), constraint_adjustment = state("c", {}),
                offset = state("o", { x = 3, y = 3 }), grab = state("gr", false)"#,
        ))
        .unwrap();
        assert_eq!(spec.anchor_rect, LogicalRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 });
        assert_eq!((spec.width, spec.height), (1.0, 1.0));
        assert_eq!((spec.anchor, spec.gravity), (PopupAnchor::Center, PopupAnchor::Center));
        assert_eq!(spec.constraint_adjustment, ConstraintAdjustment::default());
        assert_eq!(spec.offset, PopupOffset::default());
        assert!(spec.grab, "a deferred `grab` takes § 6.3's default, not the signal's current value");
    }

    #[test]
    fn a_literal_typo_beside_a_deferred_signal_still_fails_on_the_evaluation_pass() {
        let lua = lua();
        assert!(matches!(
            popup_spec(&unresolved_popup_props(&lua, r#", anchor = "Middle""#)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor"
        ));
    }

    #[test]
    fn a_signal_in_a_popup_parent_is_still_rejected_on_the_evaluation_pass() {
        let lua = lua();
        assert!(matches!(
            popup_spec(&unresolved_popup_props(&lua, r#", parent = state("p", "bar")"#)).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "parent"
        ));
    }

    #[test]
    fn a_signal_in_a_window_title_app_id_or_size_hint_is_deferred_on_the_evaluation_pass_too() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return { kind = "window", id = "w", title = state("t", "Now Playing"),
                            app_id = state("a", "oblisk.later"),
                            min_size = state("mn", { width = 320, height = 240 }),
                            max_size = state("mx", { width = 1280, height = 800 }) }"#,
            )
            .eval()
            .unwrap();
        let spec = window_spec(&props_from_table(&table)).unwrap();
        assert_eq!(spec.title, "", "the placeholder is what a toplevel that never sends set_title has");
        assert_eq!(spec.app_id, "oblisk-w", "the same default an absent `app_id` takes");
        assert_eq!((spec.min_size, spec.max_size), (None, None), "absent means the request is simply not sent");
    }

    #[test]
    fn lock_spec_reads_the_id_and_that_is_the_whole_of_section_6_4() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "lock", id = "screen-lock", child = { kind = "rect" } }"#)
            .eval()
            .unwrap();
        assert_eq!(lock_spec(&props_from_table(&table)).unwrap(), LockSpec { id: "screen-lock".to_string() });
    }

    #[test]
    fn a_lock_without_an_id_is_rejected_the_same_way_every_other_role_is() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "lock" }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "id"
        ));
    }

    #[test]
    fn every_property_section_6_4_denies_a_lock_is_refused_by_name_rather_than_ignored() {
        let lua = lua();
        for property in ["visible", "monitor", "anchor", "width", "height"] {
            let table: mlua::Table = lua
                .load(format!(r#"return {{ kind = "lock", id = "screen-lock", {property} = 1 }}"#))
                .eval()
                .unwrap();
            let err = lock_spec(&props_from_table(&table)).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property),
                "`{property}` must be refused by name, got {err:?}"
            );
        }
    }

    #[test]
    fn a_refused_lock_property_wins_over_a_missing_id_because_it_is_the_error_that_teaches() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "lock", visible = false }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "visible"
        ));
    }

    #[test]
    fn a_signal_bound_lock_property_is_refused_on_the_evaluation_pass_like_a_literal_one() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(r#"return { kind = "lock", id = "screen-lock", visible = state("v", true) }"#)
            .eval()
            .unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "visible"
        ));
    }

    #[test]
    fn a_signal_in_a_lock_id_is_rejected_by_the_universal_structural_arm_with_no_new_carve_out() {
        let lua = lua();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(r#"return { kind = "lock", id = state("i", "screen-lock") }"#).eval().unwrap();
        let resolved = resolve_properties(&props_from_table(&table), "lock", &lua).unwrap();
        assert!(matches!(
            lock_spec(&resolved).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "id"
        ));
    }

    #[test]
    fn a_lock_fingerprints_on_its_id_alone_so_only_its_existence_is_a_topology_change() {
        let spec = SurfaceSpec::Lock(LockSpec { id: "screen-lock".to_string() });
        assert_eq!(spec.declared_id(), "screen-lock");
        assert_eq!(spec.fingerprint(), SurfaceFingerprint::Lock("screen-lock".to_string()));
        assert_ne!(spec.fingerprint(), SurfaceFingerprint::Window("screen-lock".to_string()));
    }
}
