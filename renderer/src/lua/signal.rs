//! `Signal` (`oblisk-idl-api-specs.md` § 1.2): `get`, `map`, `set`, `computed(dependencies, fn)`,
//! and `state(name, initial)` (ADR-0044 decision 5). Rust owns the userdata; `computed` calls `fn`
//! with
//! dependency values, not handles, so its body does not call `:get()` on declared deps.
//!
//! ponytail: `computed`/`map` recompute on every `get`, with no memoization or invalidation graph.
//! The Watcher must decide when cached values go stale.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use crate::lua::marshal;

/// § 1.2: "CPU runtime is capped at 5ms per evaluation."
const CPU_CAP: Duration = Duration::from_millis(5);

/// Whole-`Scene::apply` cap, not per getter. It must exceed legitimate passes that run every getter
/// and block on shaping per text measurement. A 2000-sibling row (up to 4000) measured release
/// 202ms/298ms and debug 550ms/1.10s; 2s is ~2x worst and ~300x ADR-0069's 6.14ms 500-row list.
/// A 250ms cap refused that real config.
///
/// Bounds damage, not performance: a spinning `margin` `__index` ran one `Scene::apply` for 26.10s,
/// returned `Ok(())`, and blocked the thread that answers `configure` and runs Lua (ADR-0039). Now
/// it is a 2s stall and reportable `LayoutError`. ponytail: 200ms for 2000 siblings still drops
/// frames at ADR-0044's push cadence. Upgrade to a cost-sized per-pass budget.
const LAYOUT_PASS_CAP: Duration = Duration::from_secs(2);

/// One evaluation's wall pre-filter and thread-CPU deadline. CPU is § 1.2's authority; an unexpired
/// wall deadline proves CPU is unexpired, avoiding a syscall. `Instant::now()` costs 23.6ns versus
/// `CLOCK_THREAD_CPUTIME_ID`'s 170.4ns. Wall alone charged descheduled work: on 12 threads it fired
/// 5 times in 53 suite runs for configs a quiet machine evaluates in microseconds. A parked thread
/// fires no hook, and ADR-0048 removes blocking calls (`io` absent; four `os` calls never wait).
#[derive(Clone, Copy)]
struct Deadline {
    wall: Instant,
    /// `None` when the CPU clock is unreadable; wall remains authoritative.
    cpu: Option<Duration>,
}

impl Deadline {
    /// `cap` from now on both clocks.
    fn lasting(cap: Duration) -> Self {
        Self { wall: Instant::now() + cap, cpu: thread_cpu_time().map(|used| used + cap) }
    }

    fn expired(&self) -> bool {
        if Instant::now() <= self.wall {
            return false;
        }
        // Past wall pre-filter, so CPU decides. An unreadable clock expires; an unmeasurable cap
        // must fire rather than disappear.
        self.cpu.is_none_or(|deadline| thread_cpu_time().is_none_or(|used| used > deadline))
    }
}

/// CPU used by the calling thread, as § 1.2 requires. Per thread, not process: Lua runs start to
/// finish on the entering Wayland thread (ADR-0039); process-wide time would charge shaping.
/// `crate::wayland::idle_profile` charges blocks of its loop against the same per-thread scope.
pub(crate) fn thread_cpu_time() -> Option<Duration> {
    let spent = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    Some(Duration::new(spent.tv_sec().try_into().ok()?, spent.tv_nsec().try_into().ok()?))
}

/// VM instructions between checks. `HookTriggers` warns low values have high overhead; 1000 stays
/// cheap and catches a runaway closure within roughly one batch of 5ms, not seconds later.
const CHECK_EVERY_N_INSTRUCTIONS: u32 = 1000;

/// Max live [`Signal::get_value`] calls before `mlua::Error`, matching `layout::scene`'s
/// `MAX_TREE_DEPTH`. [`CpuBudget::enter`] wraps dependency resolution, so it bounds body recursion
/// and chains (`s:map(f):map(g):...`). A 200-link chain reaches depth 200 with Rust depth 1 if the
/// deadline wraps only the closure; 5000 links abort with `fatal runtime error: stack overflow`,
/// beyond the 5ms hook because the chain does no Lua work. 32 leaves headroom; scene measurements
/// set both constants.
const MAX_SIGNAL_NESTING_DEPTH: usize = 32;

/// Shared error for hook and [`CpuBudget::check_not_exceeded`] gates.
const CPU_CAP_EXCEEDED: &str = "computed/map exceeded its 5ms CPU budget";

/// Distinct pass-budget error so config knows which limit it hit. Plain `__index` without a signal
/// reaches it, the hole this budget closes.
const LAYOUT_PASS_CAP_EXCEEDED: &str = "the layout pass exceeded its 2s CPU budget";

#[derive(Clone)]
enum SignalKind {
    // ponytail: only `try_new_direct` constructs this; no production caller yet, tests only.
    #[allow(dead_code)]
    Direct(Value),
    Computed {
        deps: Rc<Vec<Signal>>,
        func: Function,
    },
    /// Rust-overwritable value (`Signal::new_live`/`LiveSignalHandle`). `Rc<RefCell<_>>` because
    /// the Loader stays on one Wayland dispatch thread (ADR-0039).
    Live(Rc<RefCell<Value>>),
    /// Engine-written, config-read boolean from `hover(name)` (ADR-0062), separate from `Live` so
    /// only `hover_handle` can write it and `hover = oblisk.network` gets no writer. `paired_rect`
    /// links the boolean to `hover_rect(name)`'s cell; the rect half has `None` and is not a
    /// trigger.
    Hover {
        cell: Rc<RefCell<Value>>,
        paired_rect: Option<Rc<RefCell<Value>>>,
        dirty: DirtyFlag,
    },
    /// Scroll offset in logical pixels (ADR-0069), written by the wheel handler and layout clamp.
    /// Separate from `Hover` so only `scroll_handle` writes it; `scroll = oblisk.network` cannot
    /// overwrite a capability snapshot.
    Scroll {
        cell: Rc<RefCell<Value>>,
        dirty: DirtyFlag,
        /// One-shot 1-based child request from `signal:reveal(index)` (ADR-0112), consumed by the
        /// next viewport positioning pass. Separate from offset because only that pass knows child
        /// position and viewport height.
        reveal: Rc<Cell<Option<usize>>>,
    },
    /// Lua-authored writable state (ADR-0044 decision 5), built by `state`. Separate from `Live`
    /// even with
    /// identical storage: accepting `set` on `Live` would let config overwrite a pushed network
    /// SSID. The kind makes read-only capabilities a type-system fact. Carries the shared dirty
    /// flag because `set` has no `RendererClient` in reach.
    State {
        cell: Rc<RefCell<Value>>,
        dirty: DirtyFlag,
    },
}

impl SignalKind {
    /// Name used in [`Signal::set`] refusal messages.
    fn describe(&self) -> &'static str {
        match self {
            SignalKind::Direct(_) => "a direct",
            SignalKind::Computed { .. } => "a computed",
            SignalKind::Live(_) => "a capability",
            SignalKind::Hover { .. } => "a hover",
            SignalKind::Scroll { .. } => "a scroll",
            SignalKind::State { .. } => "a state",
        }
    }
}

/// Applies `marshal.rs`/§ 1.1 checks to Lua-authored `Number`/`Integer`/`String`; other shapes pass
/// unchanged. Shared by `try_new_direct`, `new_state`, and `set`; `new_live` receives Rust data.
fn check_lua_authored(value: &Value) -> Result<(), marshal::MarshalError> {
    match value {
        Value::Number(n) => {
            marshal::check_number(*n)?;
        }
        Value::Integer(i) => {
            marshal::check_integer(*i)?;
        }
        Value::String(s) => {
            marshal::check_string(&s.to_string_lossy())?;
        }
        _ => {}
    }
    Ok(())
}

/// Whether a `state` literal is comparable across evaluations. Tables, functions, and userdata use
/// pointer identity; fresh values would always look edited.
fn is_comparable_literal(value: &Value) -> bool {
    matches!(value, Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_) | Value::String(_))
}

/// Whether the `state` literal changed. `None` means no edit per ADR-0044's amendment. This keeps
/// `lib/ui_state.lua`'s table `popup_anchor` from looking edited on every reload and snapping a
/// popup to the corner. Integer/number comparison follows Lua `==`, so `0` and `0.0` match; scalar
/// types differing do not.
fn literal_was_edited(current: &Value, seeded: &Value) -> Option<bool> {
    if !is_comparable_literal(current) || !is_comparable_literal(seeded) {
        return None;
    }
    Some(match (current, seeded) {
        (Value::Integer(i), Value::Number(n)) | (Value::Number(n), Value::Integer(i)) => (*i as f64) != *n,
        _ => current != seeded,
    })
}

/// Read-only reactive value: plain (`Direct`/Rust-pushed) or recomputed Lua closure (`Computed`).
#[derive(Clone)]
pub struct Signal(SignalKind);

impl Signal {
    /// Wraps `value` as `Direct`, enforcing `marshal.rs` for `Number`/`Integer`/`String`.
    /// ponytail: tests only; production live values use `new_live`. A Lua-created Direct signal is
    /// future work.
    #[allow(dead_code)]
    pub fn try_new_direct(value: Value) -> Result<Self, marshal::MarshalError> {
        check_lua_authored(&value)?;
        Ok(Signal(SignalKind::Direct(value)))
    }

    /// Signal behind `state(name, initial)` (ADR-0044 decision 5), writable through `set`, which
    /// marks
    /// `dirty`; `initial` is Lua-authored and marshal-checked, unlike `new_live`.
    pub fn new_state(initial: Value, dirty: DirtyFlag) -> Result<Self, marshal::MarshalError> {
        check_lua_authored(&initial)?;
        Ok(Signal(SignalKind::State { cell: Rc::new(RefCell::new(initial)), dirty }))
    }

    /// Replaces a state value when the config changed its literal (ADR-0044 amendment), so the file
    /// wins over prior `set`; marks the same dirty flag. Only `State` belongs to the registry; any
    /// other kind is a caller bug, not config error.
    pub fn reseed(&self, value: Value) -> Result<(), marshal::MarshalError> {
        check_lua_authored(&value)?;
        let SignalKind::State { cell, dirty } = &self.0 else {
            debug_assert!(false, "reseed on {} signal, which the state registry cannot hold", self.0.describe());
            return Ok(());
        };
        *cell.borrow_mut() = value;
        dirty.mark();
        Ok(())
    }

    /// Rust-pushed signal via [`LiveSignalHandle`]. Values are serde-serialized Rust data, so Lua
    /// marshalling checks cannot find NaN/Inf/oversized strings. Every live signal shares one
    /// generation dirty flag, whose clone `renderer/src/socket.rs`'s `RendererClient` drains.
    pub fn new_live(initial: Value, dirty: DirtyFlag) -> (Self, LiveSignalHandle) {
        let cell = Rc::new(RefCell::new(initial));
        (Signal(SignalKind::Live(Rc::clone(&cell))), LiveSignalHandle(cell, dirty))
    }

    /// Boolean written by `wl_pointer`, read-only to Lua (ADR-0062 decision 2). Starts `false`, not
    /// nil, because `visible` treats nil as absent (ADR-0044 decision 1 amendment). `initial_rect`
    /// must be a
    /// real non-zero 1x1 table: § 6 requires non-zero tooltip `anchor_rect` before any pointer
    /// event,
    /// and this constructor lacks a Lua to build the table.
    pub fn new_hover(dirty: DirtyFlag, initial_rect: Value) -> (Self, Self) {
        let over = Rc::new(RefCell::new(Value::Boolean(false)));
        let rect = Rc::new(RefCell::new(initial_rect));
        (
            Signal(SignalKind::Hover {
                cell: Rc::clone(&over),
                paired_rect: Some(Rc::clone(&rect)),
                dirty: dirty.clone(),
            }),
            Signal(SignalKind::Hover { cell: rect, paired_rect: None, dirty }),
        )
    }

    /// Scroll offset starting at top (ADR-0069 decision 2). Plain number, not a hover-like pair: no
    /// scrollbar uses content extent yet, so the first such config can define its shape.
    pub fn new_scroll(dirty: DirtyFlag) -> Self {
        Signal(SignalKind::Scroll {
            cell: Rc::new(RefCell::new(Value::Number(0.0))),
            dirty,
            reveal: Rc::new(Cell::new(None)),
        })
    }

    /// Requests the next positioning pass scroll visible child `index` (1-based) into view, marking
    /// dirty (ADR-0112). Other kinds return false for `signal:reveal()`'s named refusal.
    pub(crate) fn request_reveal(&self, index: usize) -> bool {
        match &self.0 {
            SignalKind::Scroll { reveal, dirty, .. } => {
                reveal.set(Some(index));
                dirty.mark();
                true
            }
            _ => false,
        }
    }

    /// Consumes the reveal in `layout::scene`'s positioning pass, so a later wheel event does not
    /// fight an already honored request.
    pub(crate) fn take_reveal(&self) -> Option<usize> {
        match &self.0 {
            SignalKind::Scroll { reveal, .. } => reveal.take(),
            _ => None,
        }
    }

    /// Scroll write end for wheel and positioning clamp; `None` for other kinds keeps wheels off
    /// capability signals.
    pub(crate) fn scroll_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Scroll { cell, dirty, .. } => Some(LiveSignalHandle(Rc::clone(cell), dirty.clone())),
            _ => None,
        }
    }

    /// Scroll offset without `Lua`: `layout::scene` clamps deep in a pass holding no VM reference,
    /// and threading one through every layout frame just to read a `RefCell` would add a parameter.
    pub(crate) fn scroll_offset(&self) -> Option<f32> {
        match &self.0 {
            SignalKind::Scroll { cell, .. } => match *cell.borrow() {
                Value::Number(n) => Some(n as f32),
                Value::Integer(n) => Some(n as f32),
                _ => Some(0.0),
            },
            _ => None,
        }
    }

    /// Hover write end for `crate::wayland`; `None` for other kinds by design.
    pub(crate) fn hover_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Hover { cell, dirty, .. } => Some(LiveSignalHandle(Rc::clone(cell), dirty.clone())),
            _ => None,
        }
    }

    /// Rect write end for the boolean hover half: last node position in surface logical
    /// coordinates,
    /// consumed by tooltip `popup.anchor_rect`. `None` for other kinds and the rect half itself.
    pub(crate) fn hover_rect_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Hover { paired_rect: Some(rect), dirty, .. } => {
                Some(LiveSignalHandle(Rc::clone(rect), dirty.clone()))
            }
            _ => None,
        }
    }

    /// `map(f)` as a one-dependency `Computed`, recomputed on every read (ADR-0044 decision 3).
    /// Shared by
    /// Lua and Rust so `lua::capability::Capability` makes `oblisk.lock` read like bare
    /// capabilities.
    pub(crate) fn mapped(&self, func: Function) -> Signal {
        Signal(SignalKind::Computed { deps: Rc::new(vec![self.clone()]), func })
    }

    /// Reads current value (ADR-0044 decision 1). `layout::node` uses it to resolve signal
    /// userdata; `&Lua`
    /// is threaded because `Computed` needs it for [`CpuBudget`] and mlua 0.12 cannot recover Lua
    /// from `AnyUserData`/`Value`.
    pub(crate) fn get_value(&self, lua: &Lua) -> mlua::Result<Value> {
        match &self.0 {
            SignalKind::Direct(value) => Ok(value.clone()),
            SignalKind::Live(cell) => Ok(cell.borrow().clone()),
            SignalKind::Hover { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::Scroll { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::State { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::Computed { deps, func } => {
                // Enter before dependency resolution, not only `func.call`, so nesting depth also
                // bounds dependency chains.
                let budget = CpuBudget::enter(lua)?;

                // ponytail: no memoization (ADR-0044 decision 3) makes diamond depth N evaluate
                // `s` 2^N
                // times, 1,048,575 calls at N=20. Upgrade by memoizing each dependency per get.
                let mut args = Vec::with_capacity(deps.len());
                for dep in deps.iter() {
                    args.push(dep.get_value(lua)?);
                }
                let value = func.call::<Value>(MultiValue::from_vec(args))?;
                budget.check_not_exceeded()?;
                Ok(value)
            }
        }
    }
}

/// Rust handle for [`Signal::new_live`] storage, used for `StateSnapshot` pushes. Lua reads the
/// latest value, with no memoization.
#[derive(Clone)]
pub struct LiveSignalHandle(Rc<RefCell<Value>>, DirtyFlag);

impl LiveSignalHandle {
    /// Last value, for `CapabilityHandle::hydrate` to pass as `on_change`'s replaced value.
    pub fn get(&self) -> Value {
        self.0.borrow().clone()
    }

    /// Writes and marks the shared scene dirty (ADR-0044 decision 2); without a dependency graph
    /// (decision 3), the
    /// next poll re-resolves the whole scene.
    pub fn set(&self, value: Value) {
        *self.0.borrow_mut() = value;
        self.1.mark();
    }

    /// Writes without dirtying for `layout::scene`'s clamp, which derives the value from geometry
    /// just measured. Another pass would observe the same idempotent clamp; cost is one frame of
    /// staleness only when clamping: same-pass `scroll("x")` sees wheel input, derived readouts see
    /// the clamped value next pass. Positioning itself uses the clamped value immediately.
    pub(crate) fn set_quiet(&self, value: Value) {
        *self.0.borrow_mut() = value;
    }

    /// [`Self::set`] with equality deduplication. ADR-0062 decision 4 calls it for every
    /// device-rate `wl_pointer` motion; one mark re-resolves every surface (ADR-0044 decision 2),
    /// so compare first to
    /// re-resolve only on boundary crossings.
    pub fn set_changed(&self, value: Value) -> bool {
        let unchanged = *self.0.borrow() == value;
        if unchanged {
            return false;
        }
        *self.0.borrow_mut() = value;
        self.1.mark();
        true
    }
}

/// One ADR-0044 decision 2 scene-dirty bool shared by every generation handle and `RendererClient`,
/// not per
/// signal/surface. `Rc<Cell<bool>>` fits the single Wayland thread (ADR-0039). ponytail: every push
/// re-resolves every surface. Upgrade to per-surface flags keyed by read tracking.
#[derive(Clone)]
pub struct DirtyFlag(Rc<Cell<bool>>);

impl DirtyFlag {
    pub fn new() -> Self {
        Self(Rc::new(Cell::new(false)))
    }

    /// `configure` changing one surface's size invalidates resolved geometry like a capability
    /// push;
    /// `RendererClient::set_instance_size` marks this flag rather than adding a second mechanism
    /// (ADR-0044 decision 2).
    pub(crate) fn mark(&self) {
        self.0.set(true);
    }

    /// Reads and clears atomically: drain inbound frames, then re-resolve once
    /// (ADR-0044 decision 2).
    /// `wayland::run` coalesces a burst of `StateSnapshot` pushes into one resolve.
    pub fn take(&self) -> bool {
        self.0.replace(false)
    }
}

impl Default for DirtyFlag {
    fn default() -> Self {
        Self::new()
    }
}

impl UserData for Signal {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| this.get_value(lua));
        methods.add_method("map", |_, this, f: Function| Ok(this.mapped(f)));
        // ADR-0112: config requests a child, not a pixel offset; the pass owns pixels
        // (ADR-0069 decision 2).
        methods.add_method("reveal", |_, this, index: i64| {
            let Some(index) = usize::try_from(index).ok().filter(|index| *index >= 1) else {
                return Err(mlua::Error::runtime(format!(
                    "signal:reveal() takes a 1-based child index, and {index} is not one"
                )));
            };
            if !this.request_reveal(index) {
                return Err(mlua::Error::runtime(format!(
                    "signal:reveal() is only valid on a scroll(name) signal, and this is {} signal",
                    this.0.describe()
                )));
            }
            Ok(())
        });
        // ADR-0044 decision 5's only Lua write path. Other kinds refuse by name, so
        // `network:set(...)`
        // says why.
        methods.add_method("set", |_, this, value: Value| {
            let SignalKind::State { cell, dirty } = &this.0 else {
                return Err(mlua::Error::runtime(format!(
                    "signal:set() is only valid on a state(name, initial) signal, and this is {} signal: every other signal kind is read-only to Lua (ADR-0044 decision 5)",
                    this.0.describe()
                )));
            };
            // Check before writing; refusal preserves the value and dirty flag, matching
            // `new_state`.
            check_lua_authored(&value).map_err(|err| {
                mlua::Error::runtime(format!("signal:set() refused its value at the marshalling boundary: {err}"))
            })?;
            *cell.borrow_mut() = value;
            dirty.mark();
            Ok(())
        });
    }
}

/// Applies an `oblisk set`/`toggle` to named `state` (ADR-0112), using `set`'s marshalling and
/// dirty checks. Refuses missing state or non-boolean toggle values, the two keybind/config
/// mismatches.
pub fn write_state(lua: &Lua, set: &shared::SetState) -> Result<(), String> {
    let signal = lua
        .app_data_ref::<StateRegistry>()
        .and_then(|registry| registry.0.get(&set.name).map(|(signal, _)| signal.clone()))
        .ok_or_else(|| format!("this config declares no state({:?}, ...)", set.name))?;
    let value = match &set.write {
        shared::StateWrite::Set(json) => {
            crate::lua::json::to_lua(lua, json).map_err(|err| format!("the value does not convert to Lua: {err}"))?
        }
        shared::StateWrite::Toggle => match signal.get_value(lua) {
            Ok(Value::Boolean(current)) => Value::Boolean(!current),
            Ok(other) => return Err(format!("it holds {}, and only a boolean toggles", other.type_name())),
            Err(err) => return Err(format!("its value could not be read: {err}")),
        },
    };
    signal.reseed(value).map_err(|err| format!("refused at the marshalling boundary: {err}"))
}

/// Whether config called `hover(name)`. `crate::wayland` checks first, so configs without tooltip
/// or
/// hover expansion pay no tree clone, walk, or signal writes at pointer-report rate.
pub fn any_hover_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<HoverRegistry>().is_some_and(|registry| !registry.0.is_empty())
}

/// ADR-0044 decision 5 state registry: name preserves last-click values across in-place reloads;
/// the stored
/// literal detects an edited initial, which wins over live state (the wallpaper case). In
/// `Lua::set_app_data`, so ADR-0044 decision 4's persistent VM preserves it and a generation swap's
/// new
/// process discards it.
#[derive(Default)]
struct StateRegistry(HashMap<String, (Signal, Value)>);

/// `hover(name)` registry (ADR-0062 decision 2), name-keyed across reloads so a tooltip stays open
/// through
/// `config/theme.lua` edits. Separate from [`StateRegistry`], or `state("volume", 0)` and
/// `hover("volume")` would collide and confuse `signal:set()`.
#[derive(Default)]
struct HoverRegistry(HashMap<String, (Signal, Signal)>);

/// Name-keyed `scroll(name)` registry; reload preserves the user's offset and avoids jumping an
/// open panel to top (ADR-0069 decision 2).
#[derive(Default)]
struct ScrollRegistry(HashMap<String, Signal>);

/// RAII claim on the 5ms budget for dependency resolution plus closure call. Deadlines stack in
/// `app_data` because computed dependencies, body reads, and self/mutual cycles re-enter; a single
/// mlua hook removed by an inner call would strip the outer cap. Install on 0->1 holders, remove on
/// 1->0. `stack[0]`, not `last()`, is the outer evaluation's deadline and the minimum: LIFO pushes
/// are non-decreasing, while `last()` is freshly reset and misses expiry. `first()` is O(1) versus
/// scanning up to 32 entries per hook.
pub(crate) struct CpuBudget<'lua> {
    lua: &'lua Lua,
}

/// Number of live [`CpuBudget`] or [`LayoutPassBudget`] holders. Count, not signal-stack depth:
/// finishing a signal inside a pass must not remove the hook the pass still needs, or later
/// `__index` calls become unbounded.
#[derive(Default)]
struct HookHolders(usize);

/// Installs the instruction hook for the first holder; [`release_hook`] balances it.
fn acquire_hook(lua: &Lua) -> mlua::Result<()> {
    if lua.app_data_ref::<HookHolders>().is_none() {
        lua.set_app_data(HookHolders::default());
    }
    let first = lua.app_data_ref::<HookHolders>().expect("just ensured the counter exists").0 == 0;
    if first {
        // `set_global_hook`, not per-thread `set_hook`: coroutines otherwise ran unhooked, measured
        // 5.75s of Lua in `coroutine.create`/`resume` returning `Ok`. The global callback covers
        // whichever Lua thread fires.
        lua.set_global_hook(
            mlua::HookTriggers { every_nth_instruction: Some(CHECK_EVERY_N_INSTRUCTIONS), ..mlua::HookTriggers::new() },
            |lua, _| match expired_budget(lua) {
                Some(message) => Err(mlua::Error::runtime(message)),
                None => Ok(mlua::VmState::Continue),
            },
        )?;
    }
    lua.app_data_mut::<HookHolders>().expect("just ensured the counter exists").0 += 1;
    Ok(())
}

/// Drops one hook claim, removing the hook when the last holder lets go.
fn release_hook(lua: &Lua) {
    let remaining = {
        let mut holders = lua.app_data_mut::<HookHolders>().expect("acquire_hook always runs before its release");
        holders.0 = holders.0.saturating_sub(1);
        holders.0
    };
    if remaining == 0 {
        // Both: global removal stops inheriting coroutines; thread removal clears this thread's
        // mask.
        lua.remove_global_hook();
        lua.remove_hook();
    }
}

/// Whole-`Scene::apply` deadline, when a pass is in flight.
#[derive(Default)]
struct PassDeadline(Option<Deadline>);

/// RAII claim on [`LAYOUT_PASS_CAP`] for the whole pass. It covers metamethod-aware `Table::get`
/// after [`CpuBudget`] drops its hook (a `while true` `margin.__index` once hung Wayland), and
/// stops a margined tree buying one 5ms budget per `get_value` under ADR-0021. Runs beside
/// [`CpuBudget`]; the earlier
/// [`expired_budget`] wins, preserving § 1.2's 5ms and adding a pass ceiling.
pub(crate) struct LayoutPassBudget<'lua> {
    lua: &'lua Lua,
}

impl<'lua> LayoutPassBudget<'lua> {
    /// Starts the pass clock and holds the hook across it, putting `__index` under a budget.
    pub(crate) fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        if lua.app_data_ref::<PassDeadline>().is_none() {
            lua.set_app_data(PassDeadline::default());
        }
        // Store deadline after hook installation so failed install leaves nothing for `Drop`.
        acquire_hook(lua)?;
        lua.app_data_mut::<PassDeadline>().expect("just ensured the slot exists").0 =
            Some(Deadline::lasting(LAYOUT_PASS_CAP));
        Ok(Self { lua })
    }

    /// Rust-boundary gate: config `pcall` can swallow the hook's ordinary Lua error in `__index` or
    /// a getter, but cannot swallow this check.
    pub(crate) fn exceeded(&self) -> bool {
        self.lua.app_data_ref::<PassDeadline>().and_then(|slot| slot.0).is_some_and(|d| d.expired())
    }
}

impl Drop for LayoutPassBudget<'_> {
    fn drop(&mut self) {
        self.lua.app_data_mut::<PassDeadline>().expect("enter always runs before its Drop").0 = None;
        release_hook(self.lua);
    }
}

impl<'lua> CpuBudget<'lua> {
    /// Claims one nesting level, refusing past [`MAX_SIGNAL_NESTING_DEPTH`]. Install hook before
    /// pushing so early return cannot strand a deadline and disable the VM's cap; no Lua runs
    /// between
    /// the two, and the hook tolerates an empty stack.
    pub(crate) fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        if lua.app_data_ref::<Vec<Deadline>>().is_none() {
            lua.set_app_data(Vec::<Deadline>::new());
        }
        let depth = lua.app_data_ref::<Vec<Deadline>>().expect("just ensured the deadline stack exists").len();
        if depth >= MAX_SIGNAL_NESTING_DEPTH {
            return Err(mlua::Error::runtime(format!(
                "signal nesting exceeded its maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels -- a computed/map chain recursing into itself, or a dependency chain that long?"
            )));
        }
        acquire_hook(lua)?;
        lua.app_data_mut::<Vec<Deadline>>()
            .expect("just ensured the deadline stack exists")
            .push(Deadline::lasting(CPU_CAP));
        Ok(Self { lua })
    }

    /// Second 5ms gate at Rust boundary. A `pcall` can catch the hook and return a partial `Ok`,
    /// measured at 7.5x the cap; this check turns it into `Err`. ponytail: a body that swallows the
    /// hook and never returns still spins. VM lacks preemption; upgrade to generation-swap process
    /// boundary (ADR-0039).
    pub(crate) fn check_not_exceeded(&self) -> mlua::Result<()> {
        match expired_budget(self.lua) {
            Some(message) => Err(mlua::Error::runtime(message)),
            None => Ok(()),
        }
    }
}

impl Drop for CpuBudget<'_> {
    fn drop(&mut self) {
        self.lua.app_data_mut::<Vec<Deadline>>().expect("CpuBudget::enter always runs before its Drop").pop();
        release_hook(self.lua);
    }
}

/// Returns the earlier expired signal/pass deadline. Signal uses outermost `first()`; pass stays
/// separate so that lookup remains O(1). No deadline means no expiry, allowing hook installation.
fn expired_budget(lua: &Lua) -> Option<&'static str> {
    let signal = lua.app_data_ref::<Vec<Deadline>>().and_then(|stack| stack.first().copied());
    if signal.is_some_and(|deadline| deadline.expired()) {
        return Some(CPU_CAP_EXCEEDED);
    }
    let pass = lua.app_data_ref::<PassDeadline>().and_then(|slot| slot.0);
    pass.filter(Deadline::expired).map(|_| LAYOUT_PASS_CAP_EXCEEDED)
}

/// Shared answer for signal-like userdata and the `Signal` to resolve. It accepts [`Signal`],
/// `capability::Capability`, and wrapped `IdleMember`; every § 2 capability uses one, so live
/// bindings stay live instead of becoming literals.
///
/// This clone runs for every signal-valued property of every node, on every whole-scene resolve,
/// which is why `SignalKind::Computed` holds its dependencies behind an `Rc`: owning them outright
/// would make the clone recursive, copying a vector per link of every `map`/`computed` chain.
/// Dependencies never change after construction and the Loader stays on one thread (ADR-0039).
pub fn from_userdata(ud: &mlua::AnyUserData) -> Option<Signal> {
    if let Ok(signal) = ud.borrow::<Signal>() {
        return Some(signal.clone());
    }
    if let Ok(capability) = ud.borrow::<crate::lua::capability::Capability>() {
        return Some(capability.signal());
    }
    // `IdleMember` wraps a capability beside its three threshold methods (ADR-0141); without this
    // arm `visible = oblisk.idle` is the one unbindable capability.
    Some(ud.borrow::<crate::lua::idle::IdleMember>().ok()?.signal())
}

/// [`from_userdata`] without cloning; both must agree on signal types, tested by
/// `from_userdata_and_is_signal_agree`.
pub fn is_signal(ud: &mlua::AnyUserData) -> bool {
    ud.is::<Signal>() || ud.is::<crate::lua::capability::Capability>() || ud.is::<crate::lua::idle::IdleMember>()
}

/// Registers `computed` (§ 1.2), `state` (ADR-0044 decision 5), `hover`, `hover_rect`, and
/// `scroll`.
/// Dependencies are signal-like userdata. Pass the shared dirty flag explicitly, not via
/// `app_data`: a hidden coupling failing inside a config author's `state()` call is worse than
/// threading one argument through. `set` marks the same flag `new_live` returns and
/// `RendererClient` drains.
pub fn register(lua: &Lua, dirty: DirtyFlag) -> mlua::Result<()> {
    let hover_dirty = dirty.clone();
    let rect_dirty = dirty.clone();
    let scroll_dirty = dirty.clone();
    lua.globals().set(
        "computed",
        lua.create_function(|_, (deps, func): (Table, Function)| {
            let mut collected = Vec::new();
            for dep in deps.sequence_values::<mlua::AnyUserData>() {
                let dep = dep?;
                // Name the expected type; `borrow`'s error does not.
                let signal = from_userdata(&dep).ok_or_else(|| {
                    mlua::Error::runtime("computed() dependencies must be Signals or `oblisk` capabilities, § 1.2")
                })?;
                collected.push(signal);
            }
            Ok(Signal(SignalKind::Computed { deps: Rc::new(collected), func }))
        })?,
    )?;
    lua.globals().set(
        "state",
        lua.create_function(move |lua, (name, initial): (String, Value)| {
            if lua.app_data_ref::<StateRegistry>().is_none() {
                lua.set_app_data(StateRegistry::default());
            }
            let existing = lua
                .app_data_ref::<StateRegistry>()
                .expect("just ensured the state registry exists")
                .0
                .get(&name)
                .cloned();
            if let Some((signal, seeded)) = existing {
                // Existing name wins across reload; an edited `initial` is later than `set` and
                // reseeds it (ADR-0044 decision 5 amendment).
                if literal_was_edited(&initial, &seeded) == Some(true) {
                    signal.reseed(initial.clone()).map_err(|err| {
                        mlua::Error::runtime(format!(
                            "state(\"{name}\", ...) refused its new initial value at the marshalling boundary: {err}"
                        ))
                    })?;
                    lua.app_data_mut::<StateRegistry>()
                        .expect("just ensured the state registry exists")
                        .0
                        .insert(name, (signal.clone(), initial));
                }
                return Ok(signal);
            }
            let signal = Signal::new_state(initial.clone(), dirty.clone()).map_err(|err| {
                mlua::Error::runtime(format!(
                    "state(\"{name}\", ...) refused its initial value at the marshalling boundary: {err}"
                ))
            })?;
            lua.app_data_mut::<StateRegistry>()
                .expect("just ensured the state registry exists")
                .0
                .insert(name, (signal.clone(), initial));
            Ok(signal)
        })?,
    )?;
    lua.globals().set(
        "hover",
        lua.create_function(move |lua, name: String| {
            if lua.app_data_ref::<HoverRegistry>().is_none() {
                lua.set_app_data(HoverRegistry::default());
            }
            Ok(hover_slot(lua, &hover_dirty, name)?.0)
        })?,
    )?;
    lua.globals()
        .set("hover_rect", lua.create_function(move |lua, name: String| Ok(hover_slot(lua, &rect_dirty, name)?.1))?)?;
    lua.globals().set(
        "scroll",
        lua.create_function(move |lua, name: String| {
            if lua.app_data_ref::<ScrollRegistry>().is_none() {
                lua.set_app_data(ScrollRegistry::default());
            }
            let existing = lua
                .app_data_ref::<ScrollRegistry>()
                .expect("just ensured the scroll registry exists")
                .0
                .get(&name)
                .cloned();
            if let Some(signal) = existing {
                return Ok(signal);
            }
            let signal = Signal::new_scroll(scroll_dirty.clone());
            lua.app_data_mut::<ScrollRegistry>()
                .expect("just ensured the scroll registry exists")
                .0
                .insert(name, signal.clone());
            Ok(signal)
        })?,
    )
}

/// Name-keyed hover slot: boolean from `hover(name)`, rect from `hover_rect(name)`
/// (ADR-0062 decision 2).
/// Either global creates the pair, and reloads share it. No marshalling: pointer handler owns both
/// values, not Lua.
fn hover_slot(lua: &Lua, dirty: &DirtyFlag, name: String) -> mlua::Result<(Signal, Signal)> {
    if lua.app_data_ref::<HoverRegistry>().is_none() {
        lua.set_app_data(HoverRegistry::default());
    }
    let existing =
        lua.app_data_ref::<HoverRegistry>().expect("just ensured the hover registry exists").0.get(&name).cloned();
    if let Some(slot) = existing {
        return Ok(slot);
    }
    let slot = Signal::new_hover(dirty.clone(), Value::Table(unhovered_rect(lua)?));
    lua.app_data_mut::<HoverRegistry>().expect("just ensured the hover registry exists").0.insert(name, slot.clone());
    Ok(slot)
}

/// Pre-pointer `hover_rect(name)`: real 1x1 origin table. Non-zero because § 6 rejects zero
/// `anchor_rect`; `visible = hover(name)` stays false, so a tooltip waits invisibly at origin until
/// the pointer event supplies the real rect.
fn unhovered_rect(lua: &Lua) -> mlua::Result<mlua::Table> {
    let rect = lua.create_table()?;
    rect.set("x", 0.0)?;
    rect.set("y", 0.0)?;
    rect.set("width", 1.0)?;
    rect.set("height", 1.0)?;
    Ok(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_signal(name: &str, value: Value) -> Lua {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let signal = Signal::try_new_direct(value).unwrap();
        lua.globals().set(name, signal).unwrap();
        lua
    }

    /// VM whose `state` marks the flag `RendererClient` drains.
    fn lua_with_state() -> (Lua, DirtyFlag) {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        (lua, dirty)
    }

    #[test]
    fn hover_returns_a_read_only_boolean_signal_that_starts_false() {
        // ADR-0062 decision 2: engine-written hover starts false, not nil; `visible` treats nil as
        // absent
        // (ADR-0044 decision 1 amendment).
        let (lua, _dirty) = lua_with_state();
        let started: bool = lua.load(r#"return hover("volume"):get()"#).eval().unwrap();
        assert!(!started);
    }

    #[test]
    fn hover_hands_the_same_name_the_same_signal_so_an_in_place_reload_keeps_it_open() {
        // Name is identity across reload (ADR-0044 decision 5, ADR-0062 decision 2), so the signal
        // is reused, not
        // reset false. Check storage, not userdata `==`, which compares object identity.
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"first = hover("volume") second = hover("volume") other = hover("battery")"#).exec().unwrap();

        let first: mlua::AnyUserData = lua.globals().get("first").unwrap();
        from_userdata(&first).unwrap().hover_handle().unwrap().set(Value::Boolean(true));

        assert!(lua.load("return second:get()").eval::<bool>().unwrap(), "one name is one slot");
        assert!(!lua.load("return other:get()").eval::<bool>().unwrap(), "a different name is a different slot");
    }

    #[test]
    fn hover_rect_reads_a_real_non_zero_rect_before_anything_has_been_hovered() {
        // Live-session bug: nil rect means absent (ADR-0044 decision 1 amendment), while tooltip
        // `anchor_rect` must be non-zero (§ 6), so from the first frame each capability push made
        // every resolve refuse the popup until something hovered.
        let (lua, _dirty) = lua_with_state();
        let rect: mlua::Table = lua.load(r#"return hover_rect("volume"):get()"#).eval().unwrap();

        assert!(rect.get::<f32>("width").unwrap() > 0.0, "a zero-width anchor_rect is refused by the protocol");
        assert!(rect.get::<f32>("height").unwrap() > 0.0, "and so is a zero-height one");
        assert_eq!(rect.get::<f32>("x").unwrap(), 0.0);
        assert_eq!(rect.get::<f32>("y").unwrap(), 0.0);
    }

    #[test]
    fn a_config_cannot_write_a_hover_signal_and_the_refusal_names_it_a_hover() {
        // Own `SignalKind`, not capability kind, so refusal names hover (ADR-0062 decision 2).
        let (lua, dirty) = lua_with_state();
        let err = lua.load(r#"hover("volume"):set(true)"#).exec().unwrap_err().to_string();
        assert!(err.contains("state(name, initial)"), "the refusal points at the one writable kind: {err}");
        assert!(err.contains("a hover signal"), "the refusal has to name what it is holding: {err}");
        assert!(!dirty.take(), "a refused write marks nothing");
    }

    #[test]
    fn the_engine_writes_a_hover_signal_through_its_handle_and_only_a_hover_signal() {
        // Decision 2's other half: only hover has a writer, so `hover = oblisk.network` cannot let
        // pointer input overwrite a capability snapshot.
        let dirty = DirtyFlag::new();
        let (hovered, hovered_rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let (capability, _capability_handle) = Signal::new_live(Value::Boolean(false), dirty.clone());

        assert!(hovered.hover_handle().is_some(), "a hover signal has a write end");
        assert!(hovered.hover_rect_handle().is_some(), "and a write end for where the node was");
        assert!(capability.hover_handle().is_none(), "a capability signal must not be writable as a hover");
        assert!(
            hovered_rect.hover_rect_handle().is_none(),
            "the rect half is not itself a trigger, so it hands out no second rect"
        );
    }

    #[test]
    fn writing_the_value_already_stored_marks_nothing() {
        // ADR-0062 decision 4: pointer motion writes at device rate, and one mark re-resolves every
        // surface
        // (ADR-0044 decision 2); a stationary pointer must cause no re-resolves.
        let dirty = DirtyFlag::new();
        let (hovered, _rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let handle = hovered.hover_handle().unwrap();

        assert!(handle.set_changed(Value::Boolean(true)), "the first crossing is a real change");
        assert!(dirty.take());

        assert!(!handle.set_changed(Value::Boolean(true)), "the same value again is not a change");
        assert!(!dirty.take(), "an unchanged hover must not re-resolve the scene");

        assert!(handle.set_changed(Value::Boolean(false)), "leaving is a change again");
        assert!(dirty.take());
    }

    #[test]
    fn set_changed_cannot_dedupe_a_table_because_table_equality_is_identity() {
        // `crate::wayland::input` builds a fresh rect table per event; mlua table `PartialEq` is
        // identity, not contents. It can never dedupe identical tables, so rect writes stay on the
        // entry edge, not every motion (ADR-0062 decision 4).
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        let (_over, rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let handle = rect.hover_handle().unwrap();

        let build = || {
            let table = lua.create_table().unwrap();
            table.set("x", 1.0).unwrap();
            table.set("y", 2.0).unwrap();
            Value::Table(table)
        };
        assert!(handle.set_changed(build()));
        assert!(dirty.take());
        assert!(handle.set_changed(build()), "an identical table is still a different table");
        assert!(dirty.take(), "which is exactly the per-motion dirty mark the writer must avoid");
    }

    #[test]
    fn state_returns_a_signal_reading_back_the_initial_value_it_was_given() {
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua.load(r#"return state("count", 7):get()"#).eval().unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn set_replaces_what_a_later_get_returns() {
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                local s = state("count", 7)
                s:set(41)
                return s:get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 41, "a state signal must read back what Lua last wrote, not its initial value");
    }

    #[test]
    fn set_marks_the_scene_dirty_flag_and_a_plain_get_does_not() {
        // Reads must not re-dirty the scene during layout.
        let (lua, dirty) = lua_with_state();
        lua.load(r#"s = state("count", 0)"#).exec().unwrap();
        assert!(!dirty.take(), "constructing a state signal changes nothing that is painted");

        let _: i64 = lua.load("return s:get()").eval().unwrap();
        assert!(!dirty.take(), "reading a state signal must not mark the scene dirty");

        lua.load("s:set(1)").exec().unwrap();
        assert!(dirty.take(), "writing a state signal must mark the shared scene-dirty flag");
    }

    #[test]
    fn the_same_state_name_and_the_same_initial_keeps_the_value_written_since() {
        // ADR-0044 decision 5: reload preserves the user's last click, not the literal.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "an unedited literal must keep the current value, not reset to the initial");
    }

    #[test]
    fn a_changed_initial_re_seeds_the_signal_and_marks_dirty() {
        // D5 amendment: changed literal is a later write than `set`; this is the wallpaper path.
        let (lua, dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 99):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 99, "an edited literal must win over the value `:set()` left behind");
        assert!(dirty.take(), "a re-seed must mark the scene dirty, or nothing repaints from it");
    }

    #[test]
    fn re_seeding_twice_from_the_same_edited_literal_only_happens_once() {
        // Remember the new literal; the old one would re-seed every later evaluation and clobber
        // `set` forever.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0)
                state("open", 99)
                state("open", 99):set(7)
                return state("open", 99):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 7, "the second evaluation of an already-adopted literal is not another edit");
    }

    #[test]
    fn a_table_initial_never_counts_as_edited() {
        // `lib/ui_state.lua`'s `popup_anchor`: fresh table pointers would make every reload an edit
        // and snap the popup to the corner.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("anchor", { x = 0 }):set(5)
                return state("anchor", { x = 0 }):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "a table literal must keep the live value, since it cannot be compared");
    }

    #[test]
    fn rewriting_an_integer_literal_as_a_float_is_not_an_edit() {
        // Lua `==` says `0 == 0.0`; reformatting a number is not an edit.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("count", 0):set(5)
                return state("count", 0.0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "0 and 0.0 are the same literal");
    }

    #[test]
    fn changing_a_literals_type_is_an_edit() {
        let (lua, _dirty) = lua_with_state();
        let result: String = lua
            .load(
                r#"
                state("kind", false):set("clicked")
                return state("kind", "waiting"):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "waiting", "two scalars of different types are a different literal");
    }

    #[test]
    fn two_state_names_are_two_independent_signals() {
        let (lua, _dirty) = lua_with_state();
        let (a, b): (i64, i64) = lua
            .load(
                r#"
                state("a", 1):set(10)
                return state("a", 1):get(), state("b", 2):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!((a, b), (10, 2), "the map is keyed by name, so a write to one name must not reach another");
    }

    #[test]
    fn set_on_a_live_capability_signal_is_refused_because_capability_values_are_read_only_to_lua() {
        // Security contract: accepting `set` on `Live` would let config overwrite the Supervisor's
        // network SSID while every downstream reader believed it.
        let (lua, dirty) = lua_with_state();
        let (signal, _handle) = Signal::new_live(Value::Integer(1), dirty.clone());
        lua.globals().set("network", signal).unwrap();

        let err = lua.load(r#"network:set(2)"#).exec().unwrap_err();
        assert!(err.to_string().contains("read-only"), "the refusal must name the read-only rule: {err}");
        assert!(!dirty.take(), "a refused write must not mark the scene dirty either");

        let unchanged: i64 = lua.load("return network:get()").eval().unwrap();
        assert_eq!(unchanged, 1, "the pushed value must survive the attempt");
    }

    #[test]
    fn set_on_a_computed_signal_is_refused() {
        let (lua, _dirty) = lua_with_state();
        let err = lua
            .load(
                r#"
                local s = state("count", 1)
                s:map(function(v) return v end):set(9)
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "the refusal must name the read-only rule: {err}");
    }

    /// ADR-0112: keybind writes target config state; refuse missing names and non-boolean toggles.
    #[test]
    fn a_control_clients_write_reaches_a_declared_state_and_is_refused_otherwise() {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        lua.load(r#"OPEN = state("launcher_open", false); KIND = state("panel_kind", "none")"#).exec().unwrap();
        dirty.take();

        write_state(&lua, &shared::SetState { name: "launcher_open".into(), write: shared::StateWrite::Toggle })
            .unwrap();
        assert!(dirty.take(), "a write from outside re-resolves the scene like any other");
        assert!(lua.load("return OPEN:get()").eval::<bool>().unwrap());

        let set = shared::StateWrite::Set(serde_json::json!("notifications"));
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: set }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "notifications");
        dirty.take();

        let missing = write_state(&lua, &shared::SetState { name: "nope".into(), write: shared::StateWrite::Toggle });
        assert!(missing.unwrap_err().contains("declares no state"));
        let not_bool =
            write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: shared::StateWrite::Toggle });
        assert!(not_bool.unwrap_err().contains("only a boolean toggles"));
        assert!(!dirty.take(), "a refused write changes nothing");
    }

    #[test]
    fn state_refuses_an_initial_value_that_fails_the_marshalling_boundary() {
        // Lua-authored `state` initial crosses `marshal.rs`.
        let (lua, _dirty) = lua_with_state();
        let err = lua.load(r#"return state("bad", 0/0)"#).eval::<Value>().unwrap_err();
        assert!(err.to_string().contains("finite"), "a NaN initial must be refused by name: {err}");
    }

    #[test]
    fn set_refuses_a_value_that_fails_the_marshalling_boundary() {
        let (lua, dirty) = lua_with_state();
        let err = lua
            .load(
                r#"
                s = state("count", 0)
                s:set(0/0)
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("finite"), "a NaN write must be refused by name: {err}");
        assert!(!dirty.take(), "a refused write must not mark the scene dirty");

        let unchanged: i64 = lua.load("return s:get()").eval().unwrap();
        assert_eq!(unchanged, 0, "a refused write must leave the stored value alone");
    }

    #[test]
    fn get_returns_the_wrapped_direct_value() {
        let lua = lua_with_signal("s", Value::Number(0.75));
        let result: f64 = lua.load("return s:get()").eval().unwrap();
        assert_eq!(result, 0.75);
    }

    #[test]
    fn map_recomputes_against_the_parents_current_value() {
        let lua = lua_with_signal("s", Value::Integer(10));
        let result: i64 = lua.load("return s:map(function(v) return v * 2 end):get()").eval().unwrap();
        assert_eq!(result, 20);
    }

    #[test]
    fn computed_combines_multiple_dependencies_current_values() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(3)).unwrap()).unwrap();
        lua.globals().set("b", Signal::try_new_direct(Value::Integer(4)).unwrap()).unwrap();

        let result: i64 = lua.load("return computed({a, b}, function(x, y) return x + y end):get()").eval().unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn computed_reflects_a_later_direct_signal_reconstruction_not_a_stale_cache() {
        // No memoization: later `get` sees a rebuilt dependency, not a cached first read.
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        lua.load("doubled = computed({a}, function(x) return x * 2 end)").exec().unwrap();

        let first: i64 = lua.load("return doubled:get()").eval().unwrap();
        assert_eq!(first, 2);

        lua.globals().set("a", Signal::try_new_direct(Value::Integer(5)).unwrap()).unwrap();
        lua.load("doubled = computed({a}, function(x) return x * 2 end)").exec().unwrap();
        let second: i64 = lua.load("return doubled:get()").eval().unwrap();
        assert_eq!(second, 10);
    }

    #[test]
    fn computed_aborts_a_runaway_closure_instead_of_hanging_or_returning_a_wrong_value() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> =
            lua.load("return computed({a}, function(x) while true do end end):get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a busy-loop computed must error, not return a value");
        assert!(elapsed < Duration::from_secs(1), "the 5ms cap must abort well under a second, took {elapsed:?}");
    }

    #[test]
    fn a_nested_get_call_inside_a_computed_body_does_not_strip_the_outer_calls_cap() {
        // A body reading a second Signal re-enters the budget; inner return must preserve the outer
        // cap.
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        lua.globals().set("other", Signal::try_new_direct(Value::Integer(2)).unwrap()).unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> =
            lua.load("return computed({a}, function(x) local y = other:get(); while true do end end):get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "the outer computed must still abort even though its body read a second Signal");
        assert!(elapsed < Duration::from_secs(1), "the cap must still fire near 5ms, took {elapsed:?}");
    }

    #[test]
    fn a_live_signal_reflects_a_value_pushed_after_construction_not_a_frozen_snapshot() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let (signal, handle) = Signal::new_live(Value::Integer(1), DirtyFlag::new());
        lua.globals().set("live", signal).unwrap();

        let first: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(first, 1, "must read the value passed to new_live before any push");

        handle.set(Value::Integer(42));
        let second: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(second, 42, "must reflect the pushed value without re-registering the global");
    }

    #[test]
    fn a_self_referential_computed_is_rejected_with_a_nesting_depth_error_not_an_abort() {
        // Self-reference recurses through `get_value` beyond what CPU cap can stop; before this cap
        // the exact case ended in `fatal runtime error: stack overflow`.
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                local loop
                loop = computed({}, function() return loop:get() end)
                return loop:get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a self-referential computed must error, not abort the process");
        assert!(elapsed < Duration::from_secs(1), "the depth cap must trip well under a second, took {elapsed:?}");
    }

    #[test]
    fn a_mutually_recursive_computed_pair_is_rejected_with_a_nesting_depth_error_not_an_abort() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                local a, b
                a = computed({}, function() return b:get() end)
                b = computed({}, function() return a:get() end)
                return a:get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a mutually recursive computed pair must error, not abort the process");
        assert!(elapsed < Duration::from_secs(1), "the depth cap must trip well under a second, took {elapsed:?}");
    }

    /// `s:map(f):map(f):...` `links` deep over Direct signal `a`.
    fn map_chain_source(links: usize) -> String {
        format!(
            r#"
            local s = a
            for _ = 1, {links} do s = s:map(function(v) return v end) end
            return s:get()
            "#
        )
    }

    #[test]
    fn a_long_map_dependency_chain_is_rejected_by_the_nesting_cap_not_a_stack_overflow() {
        // Dependency chains nest `get_value` without Lua calls. Before the deadline wrapped
        // resolution, this chain ended in `fatal runtime error: stack overflow`; cap saw depth 1.
        let lua = lua_with_signal("a", Value::Integer(1));
        let err = lua.load(map_chain_source(200)).eval::<i64>().unwrap_err();
        assert!(
            err.to_string().contains("signal nesting exceeded"),
            "a 200-link map chain must trip the nesting cap: {err}"
        );
    }

    #[test]
    fn a_map_chain_at_the_nesting_cap_is_accepted_and_one_link_past_it_is_rejected() {
        // At most N levels are admitted, N+1 rejected. This distinguishes gates: CPU measures this
        // thread, not descheduled wait, so a busy machine may hit 5ms at the admitted depth;
        // nesting
        // must not reject a depth it promises.
        let lua = lua_with_signal("a", Value::Integer(7));
        match lua.load(map_chain_source(MAX_SIGNAL_NESTING_DEPTH)).eval::<i64>() {
            Ok(value) => assert_eq!(value, 7, "exactly MAX_SIGNAL_NESTING_DEPTH nested levels must be admitted"),
            Err(err) => assert!(
                err.to_string().contains(CPU_CAP_EXCEEDED),
                "at the limit only the CPU budget may fire, never the nesting cap: {err}"
            ),
        }

        let err = lua.load(map_chain_source(MAX_SIGNAL_NESTING_DEPTH + 1)).eval::<i64>().unwrap_err();
        assert!(
            err.to_string().contains(&format!("maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels")),
            "one level past the cap must be rejected, naming the limit actually enforced: {err}"
        );
    }

    #[test]
    fn a_diamond_dependency_graph_is_cut_off_by_the_shared_cpu_budget() {
        // ADR-0044 decision 3 no-memoization: 20 diamond levels made 2^20-1 calls, completed in
        // 1.9s with
        // `Ok(1048576)` before resolution was budgeted. Now the outer deadline fires around 3,200
        // calls. Building is exponential and dominates wall clock. Build separately because
        // cloning makes a 2^20-node tree, not shared DAG.
        let lua = lua_with_signal("a", Value::Integer(1));
        lua.load("for _ = 1, 20 do a = computed({a, a}, function(x, y) return x + y end) end").exec().unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> = lua.load("return a:get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "an exponential diamond graph must be cut off, not returned: {result:?}");
        assert!(elapsed < Duration::from_millis(100), "the shared 5ms budget must cut it off early, took {elapsed:?}");
    }

    #[test]
    fn a_computed_descheduled_past_its_deadline_is_not_charged_for_time_it_did_not_run() {
        // Before the fix: 5 failures in 53 renderer-suite runs, a different test each time, across
        // 630 tests/12 threads, each falsely raising the 5ms error while a quiet-machine config
        // was descheduled. `park` burns no CPU, so § 1.2's CPU cap must not fire; the later loop
        // exercises both hook and return gates.
        let lua = lua_with_signal("a", Value::Integer(7));
        let park = lua
            .create_function(|_, ()| {
                std::thread::sleep(Duration::from_millis(40));
                Ok(())
            })
            .unwrap();
        lua.globals().set("park", park).unwrap();

        let value: i64 = lua
            .load("return a:map(function(v) park() local n = 0 for i = 1, 5000 do n = n + i end return v end):get()")
            .eval()
            .unwrap();

        assert_eq!(value, 7);
    }

    #[test]
    fn a_pcall_swallowing_the_hook_error_still_fails_at_the_rust_boundary() {
        // `pcall` catches the hook's ordinary Lua error and could return partial data. Bounded loop
        // keeps a regression slow rather than hanging the runner.
        let lua = lua_with_signal("a", Value::Integer(1));
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                return computed({a}, function(x)
                    local n = 0
                    for _ = 1, 400 do
                        pcall(function() for _ = 1, 20000 do n = n + 1 end end)
                    end
                    return n
                end):get()
                "#,
            )
            .eval();

        assert!(
            result.is_err(),
            "a body that swallows the hook error must not yield a partially computed value: {result:?}"
        );
    }

    #[test]
    fn a_coroutine_body_is_covered_by_the_cpu_cap() {
        // Per-thread `Lua::set_hook` left coroutine work unhooked: measured 5.75s returning `Ok`.
        // Elapsed assertion catches the escape; Rust-boundary gate would error either way.
        let lua = lua_with_signal("a", Value::Integer(1));
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                return computed({a}, function(x)
                    local step = coroutine.wrap(function()
                        local n = 0
                        for _ = 1, 500000000 do n = n + 1 end
                        return n
                    end)
                    return step()
                end):get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "work inside a coroutine must still hit the CPU cap: {result:?}");
        assert!(elapsed < Duration::from_secs(1), "the hook must reach the coroutine, took {elapsed:?}");
    }

    #[test]
    fn a_cap_abort_does_not_leave_the_hook_installed_for_later_unrelated_evaluation() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        let _: mlua::Result<i64> = lua.load("return computed({a}, function(x) while true do end end):get()").eval();

        // A legitimate top-level script slower than 5ms must not inherit an aborted hook.
        let result: i64 = lua
            .load(
                r#"
                local sum = 0
                for i = 1, 2000000 do sum = sum + i end
                return sum
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 2_000_001_000_000);
    }

    /// Resolver must see through `Capability`, or live `oblisk.<name>` becomes a literal.
    #[test]
    fn from_userdata_sees_through_a_capability_to_its_read_signal() {
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        handle.hydrate(Value::Integer(42), 1);
        lua.globals().set("probe", capability).unwrap();

        let ud: mlua::AnyUserData = lua.load("return probe").eval().unwrap();
        let signal = from_userdata(&ud).expect("a capability must resolve like the signal it wraps");
        assert_eq!(signal.get_value(&lua).unwrap().as_i64(), Some(42));
    }

    #[test]
    fn from_userdata_and_is_signal_agree() {
        // Prevents the two checks from drifting to different type sets.
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, _handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        lua.globals().set("probe", capability).unwrap();
        lua.globals().set("plain", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        // Neither type: both checks must say no.
        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();

        for name in ["probe", "plain", "handle"] {
            let ud: mlua::AnyUserData = lua.load(format!("return {name}")).eval().unwrap();
            assert_eq!(from_userdata(&ud).is_some(), is_signal(&ud), "{name}");
        }
    }

    #[test]
    fn computed_accepts_a_capability_as_a_dependency_and_names_what_it_rejects() {
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        handle.hydrate(Value::Integer(3), 1);
        lua.globals().set("probe", capability).unwrap();

        let doubled: i64 = lua.load("return computed({probe}, function(n) return n * 2 end):get()").eval().unwrap();
        assert_eq!(doubled, 6);

        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();
        let err = lua.load("return computed({handle}, function(n) return n end)").exec().unwrap_err().to_string();
        assert!(
            err.contains("must be Signals or `oblisk` capabilities"),
            "the error must say what was expected: {err}"
        );
    }
}
