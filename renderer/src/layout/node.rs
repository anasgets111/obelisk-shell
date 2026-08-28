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
//! [`SurfaceTopology`]'s four fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/
//! `parse_monitor`) and every node's optional `id` (`parse_node_id`, docs/adr/0045 decision 1)
//! are the carve-outs and keep rejecting a `Signal` outright -- see
//! [`reject_signal_in_structural_field`]'s doc comment for why.

use std::collections::HashMap;

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

fn invalid(property: &str, detail: impl Into<String>) -> LayoutError {
    LayoutError::InvalidProperty {
        property: property.to_string(),
        detail: detail.into(),
    }
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

/// Whether `property` is one [`resolve_properties`] copies through untouched on a node of this
/// `kind`, so that [`reject_signal_in_structural_field`] still sees a raw `Value::UserData` and can
/// refuse it -- see that function's doc comment for why these and nothing else. Resolving them
/// there and rejecting afterwards would be unimplementable: once a signal has been read, the value
/// in the map is indistinguishable from a literal.
///
/// Kind-aware, because a skip is only sound where a parser actually runs to do the rejecting.
/// `id` is skipped on every kind: [`parse_node_id`] reads it on every node and [`parse_surface_id`]
/// on a surface root, so a `Signal` in it is refused wherever it appears. `layer`/`anchor`/
/// `monitor` are read by [`surface_topology`] alone, which `layout::scene`'s
/// `Scene::apply_one_surface` calls on top-level surfaces and nowhere else -- so on a `rect` no
/// parser ever looks at them, and skipping them there would copy a live `Signal` handle straight
/// into `layout::scene::RetainedNode::properties` with nothing left to reject it. That would
/// falsify the "never a `Signal`" invariant `layout::scene::ResolvedNode::properties` documents and
/// hands to the paint stage, which is told it may read a colour or a radius off the map directly.
/// Below a surface these three are ordinary properties and resolve like any other.
fn is_structural_property(kind: &str, property: &str) -> bool {
    property == "id" || (kind == "surface" && matches!(property, "layer" | "anchor" | "monitor"))
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

/// The carve-outs from decision 1's "parsers resolve a `Signal`" rule: [`SurfaceTopology`]'s four
/// fields (`parse_surface_id`/`parse_layer`/`parse_anchor`/`parse_monitor`) and every node's
/// optional `id` ([`parse_node_id`], docs/adr/0045 decision 1) keep rejecting one outright, the
/// same way every parser used to (this function used to be named `reject_signal` and back every
/// one of them, then narrowed to the topology fields alone -- ADR-0044's amendment banner --
/// before widening again here to cover `id`).
///
/// The unifying reason, which is why these five and nothing else: each is read exactly once per
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
/// through raw: these five parsers are the only ones that read the un-resolved value, and they have
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
        format!("expected a number, \"Fill\", or a \"NN%\" string, got {value:?}"),
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
pub fn parse_edge_insets(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<EdgeInsets, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(EdgeInsets::default());
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            property,
            format!("expected a table, got {value:?}"),
        ));
    };
    let edge = |key: &str| -> Result<f32, LayoutError> {
        let v: Value = table
            .get(key)
            .map_err(|e| invalid(property, e.to_string()))?;
        match v {
            Value::Nil => Ok(0.0),
            other => value_as_f32(property, &other)?.ok_or_else(|| {
                invalid(property, format!("`{key}` must be a number, got {other:?}"))
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
            format!("expected a string, got {value:?}"),
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
            format!("expected a boolean, got {other:?}"),
        )),
    }
}

pub fn parse_spacing(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("spacing") else {
        return Ok(0.0);
    };
    value_as_f32("spacing", value)?
        .ok_or_else(|| invalid("spacing", format!("expected a number, got {value:?}")))
}

pub fn parse_content(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    let Some(value) = properties.get("content") else {
        return Err(invalid("content", "text node requires `content`"));
    };
    match value {
        Value::String(s) => checked_string("content", s),
        other => Err(invalid(
            "content",
            format!("expected a string, got {other:?}"),
        )),
    }
}

pub fn parse_font_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("font_size") else {
        return Ok(12.0);
    };
    value_as_f32("font_size", value)?
        .ok_or_else(|| invalid("font_size", format!("expected a number, got {value:?}")))
}

pub fn parse_icon_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("size") else {
        return Err(invalid("size", "icon node requires `size`"));
    };
    value_as_f32("size", value)?.ok_or_else(|| invalid("size", format!("expected a number, got {value:?}")))
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
        other => Err(invalid(property, format!("expected a string, got {other:?}"))),
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
        other => Err(invalid("id", format!("expected a string, got {other:?}"))),
    }
}

/// § 6.1's `layer` (`"Background"`/`"Bottom"`/`"Top"`/`"Overlay"`). Required, same shape as
/// [`parse_surface_id`] -- every existing fixture in this repo already sets it. Stored as a raw
/// string, not a validated enum: Phase 13 only needs it for topology-diff equality (`CONTEXT.md`,
/// Topology change), not for binding a real `zwlr_layer_surface_v1` yet -- see docs/adr/0024.
pub fn parse_layer(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "layer", None)
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
        return Err(invalid("anchor", format!("expected a table, got {value:?}")));
    };
    let edge = |key: &str| -> Result<bool, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("anchor", e.to_string()))?;
        match v {
            Value::Nil => Ok(false),
            Value::Boolean(b) => Ok(b),
            other => Err(invalid("anchor", format!("`{key}` must be a boolean, got {other:?}"))),
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

/// A surface's topology-relevant fields (`CONTEXT.md`, Topology change: "adds, removes, or
/// changes the layer, anchor, or monitor target of a top-level `surface` node"). Structural
/// equality on `Vec<SurfaceTopology>` (order-sensitive) is the Renderer's own topology diff --
/// see `renderer/src/socket.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceTopology {
    pub id: String,
    pub layer: String,
    pub anchor: Anchor,
    pub monitor: String,
}

pub fn surface_topology(properties: &HashMap<String, Value>) -> Result<SurfaceTopology, LayoutError> {
    Ok(SurfaceTopology {
        id: parse_surface_id(properties)?,
        layer: parse_layer(properties)?,
        anchor: parse_anchor(properties)?,
        monitor: parse_monitor(properties)?,
    })
}

/// A single-node property (`surface.child`), converted from its raw table via
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
            format!("expected a node table, got {value:?}"),
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
            format!("expected an array table, got {value:?}"),
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
    fn text_content_is_required() {
        let props = HashMap::new();
        assert!(matches!(
            parse_content(&props).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
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
        assert!(parse_single_child(&props_with_nil_signal(&lua, "surface", "child"), "child").unwrap().is_none());
        assert!(parse_children(&props_with_nil_signal(&lua, "row", "children")).unwrap().is_empty());
    }

    #[test]
    fn a_signal_resolving_to_nil_reports_a_required_property_as_missing_not_as_a_bad_value() {
        // The other half of the same rule: `content`/`size` have no default, so "absent" is still
        // an error for them -- but it must be the *missing property* error a literal omission
        // raises, not "expected a string, got Nil". Both spellings of absence agree, which is the
        // consistency argument the rule rests on (a Lua table cannot store a `nil`, so
        // `content = nil` never reaches the property map at all).
        let lua = lua();
        let content_err = parse_content(&props_with_nil_signal(&lua, "text", "content")).unwrap_err();
        assert!(
            matches!(&content_err, LayoutError::InvalidProperty { property, detail } if property == "content" && detail == "text node requires `content`"),
            "got: {content_err}"
        );
        let size_err = parse_icon_size(&props_with_nil_signal(&lua, "icon", "size")).unwrap_err();
        assert!(
            matches!(&size_err, LayoutError::InvalidProperty { property, detail } if property == "size" && detail == "icon node requires `size`"),
            "got: {size_err}"
        );
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
    fn icon_size_is_required() {
        let props = HashMap::new();
        assert!(matches!(
            parse_icon_size(&props).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
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
            .load(r#"return { kind = "surface", child = { kind = "rect" } }"#)
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
    fn layer_reads_the_string() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", layer = "Top" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_layer(&props).unwrap(), "Top");
    }

    #[test]
    fn anchor_absent_defaults_all_false() {
        let props = HashMap::new();
        assert_eq!(parse_anchor(&props).unwrap(), Anchor::default());
    }

    #[test]
    fn anchor_reads_named_edges_defaulting_absent_ones_to_false() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", anchor = { top = true, left = true } }"#).eval().unwrap();
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
        let table: mlua::Table = lua.load(r#"return { kind = "surface", monitor = "eDP-1" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_monitor(&props).unwrap(), "eDP-1");
    }

    #[test]
    fn surface_topology_combines_id_layer_anchor_and_monitor() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "surface", id = "bar", layer = "Top", anchor = { top = true }, monitor = "eDP-1" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let topology = surface_topology(&props).unwrap();
        assert_eq!(
            topology,
            SurfaceTopology {
                id: "bar".to_string(),
                layer: "Top".to_string(),
                anchor: Anchor { top: true, right: false, bottom: false, left: false },
                monitor: "eDP-1".to_string(),
            }
        );
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
        // rejects a Signal the same way SurfaceTopology's four fields already do.
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
        table.set("kind", "surface").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(parse_layer(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer"));
    }

    #[test]
    fn a_signal_in_layer_on_a_non_surface_node_resolves_instead_of_surviving_as_a_handle() {
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
    fn a_signal_in_layer_on_a_surface_still_survives_raw_for_parse_layer_to_reject() {
        // The other half of kind-awareness: on a `surface`, `parse_layer` does run and does the
        // rejecting, so the skip is still what makes that rejection reachable.
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "surface").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(&node.properties, "surface", &lua).unwrap();

        assert!(matches!(resolved.get("layer"), Some(Value::UserData(_))), "layer must survive the resolve step unresolved on a surface");
        assert!(matches!(parse_layer(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer"));
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
}
