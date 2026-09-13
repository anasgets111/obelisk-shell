//! `timer(ms, fn)`: config-owned one-shot callbacks (ADR-0203).
//!
//! The imperative half of the clock. `delay(signal, ms)` and `pulse(signal, ms)` are pull-based:
//! a due wake dirties the scene and the pass re-reads them, which is why `signal.rs` needs only one
//! deadline slot and no identity. A callback cannot work that way. It has to run whether or not any
//! visible node reads anything, so it needs its own list, its own identity, and its own turn
//! position (ADR-0124's hidden subtrees are never resolved, so a pull-based timer behind one would
//! silently never fire).
//!
//! Registrations last exactly one evaluation, cleared beside `action`'s for the same reason: a
//! callback kept past a reload is a closure over the previous evaluation's locals (ADR-0115).

use std::time::{Duration, Instant};

use mlua::{Function, Lua, UserData, UserDataMethods};

use super::signal::CpuBudget;

/// Range of `ms`, one millisecond to one day. Deliberately not `delay`/`pulse`'s 60-second ceiling:
/// an idle suspend delay is measured in hours.
const MIN_MS: u64 = 1;
const MAX_MS: u64 = 24 * 60 * 60 * 1000;

type TimerId = u64;

struct Entry {
    due: Instant,
    id: TimerId,
    callback: Function,
}

/// Every armed timer, earliest first, in `app_data` beside the other registries.
///
/// ponytail: a sorted `Vec`, so arming and cancelling are both O(n) in the number armed. Dispatch
/// is linear: the due prefix moves into `firing` in one drain, and each callback is taken by index
/// rather than searched for. A heap would make arming O(log n) at the cost of tombstone bookkeeping
/// on the cancellation path, which here is the common one.
#[derive(Default)]
struct TimerRegistry {
    entries: Vec<Entry>,
    /// Armed by an evaluation whose output has not been applied yet, and dropped if it never is
    /// (ADR-0203). Deadlines are absolute, so the wait costs a promoted timer no accuracy.
    staged: Vec<Entry>,
    /// The batch [`dispatch_due`] is part-way through. Held here rather than in a local so that
    /// `cancel` can still reach a timer whose turn has not come, which is the whole point of
    /// looking each one up again instead of calling a list of closures.
    firing: Vec<Option<Entry>>,
    /// Whether `arm` is being reached from an evaluation rather than from a callback.
    evaluating: bool,
    next_id: TimerId,
}

impl TimerRegistry {
    /// Keeps the list sorted by deadline, and registration order within one deadline, so two
    /// timers armed for the same moment fire in the order the config wrote them.
    fn arm(&mut self, due: Instant, callback: Function) -> mlua::Result<TimerId> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| mlua::Error::runtime("timer ids are exhausted; this VM has armed 2^64 timers"))?;
        let list = if self.evaluating { &mut self.staged } else { &mut self.entries };
        let at = list.partition_point(|entry| entry.due <= due);
        list.insert(at, Entry { due, id, callback });
        Ok(id)
    }

    /// Removes `id` and hands back its callback, or `None` when it already fired or was cancelled.
    /// One lookup serves both `cancel` and dispatch, which is what makes cancelling a timer that is
    /// gone a no-op rather than an error. Searches `staged` too, so a config cancelling at its top
    /// level reaches the timer it just armed.
    fn take(&mut self, id: TimerId) -> Option<Function> {
        if let Some(at) = self.entries.iter().position(|entry| entry.id == id) {
            return Some(self.entries.remove(at).callback);
        }
        if let Some(slot) = self.firing.iter_mut().find(|slot| slot.as_ref().is_some_and(|entry| entry.id == id)) {
            return slot.take().map(|entry| entry.callback);
        }
        let at = self.staged.iter().position(|entry| entry.id == id)?;
        Some(self.staged.remove(at).callback)
    }
}

/// The `cancel()`-able value `timer` answers with.
///
/// Holding it is not what keeps the timer armed: the registry owns the entry, so a handle the
/// config drops still fires.
struct TimerHandle(TimerId);

impl UserData for TimerHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancel", |lua, this, ()| {
            if let Some(mut registry) = lua.app_data_mut::<TimerRegistry>() {
                registry.take(this.0);
            }
            Ok(())
        });
    }
}

pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "timer",
        lua.create_function(|lua, (ms, callback): (u64, Function)| {
            if !(MIN_MS..=MAX_MS).contains(&ms) {
                return Err(mlua::Error::runtime(format!("timer({ms}) is outside {MIN_MS}..={MAX_MS} milliseconds")));
            }
            if lua.app_data_ref::<TimerRegistry>().is_none() {
                lua.set_app_data(TimerRegistry::default());
            }
            let due = Instant::now() + Duration::from_millis(ms);
            let mut registry = lua.app_data_mut::<TimerRegistry>().expect("just ensured the registry exists");
            let id = registry.arm(due, callback)?;
            drop(registry);
            lua.create_userdata(TimerHandle(id))
        })?,
    )
}

/// When the poll loop has to wake for the earliest armed timer, if any.
pub fn next_deadline(lua: &Lua) -> Option<Instant> {
    lua.app_data_ref::<TimerRegistry>().and_then(|registry| registry.entries.first().map(|entry| entry.due))
}

/// Runs every timer due at `now`, each under its own CPU budget.
///
/// Batch cost is quadratic in the number due, per `TimerRegistry`'s ceiling.
///
/// The due prefix moves into the registry's `firing` list, and each callback is taken out of it
/// immediately before it runs rather than all of them up front. That is what lets one callback
/// cancel another in the same batch: `cancel` empties the slot and its turn finds nothing. A
/// timer's own slot is emptied *before* it runs, so cancelling itself from inside is the same
/// no-op as cancelling it afterwards.
///
/// A timer armed by a callback waits for a later turn: `now` is fixed for the batch, so a fresh
/// deadline is always past the cutoff. Draining the prefix once is what holds that for one armed
/// with a deadline already behind `now`, and is why the batch cannot grow while it runs.
pub fn dispatch_due(lua: &Lua, now: Instant) {
    let batch = {
        let Some(mut registry) = lua.app_data_mut::<TimerRegistry>() else { return };
        let split = registry.entries.partition_point(|entry| entry.due <= now);
        registry.firing = registry.entries.drain(..split).map(Some).collect();
        registry.firing.len()
    };

    for index in 0..batch {
        // Re-borrowed per timer, and released before Lua runs: a callback arming or cancelling a
        // timer takes this same `RefCell`. A `clear` from inside one empties `firing`, which is why
        // the slot is read through `get_mut` rather than indexed.
        let taken = lua
            .app_data_mut::<TimerRegistry>()
            .and_then(|mut registry| registry.firing.get_mut(index).and_then(Option::take));
        let Some(entry) = taken else { continue };
        let callback = entry.callback;
        // The cap `action` and `on_change` handlers run under. ponytail: per callback, not per
        // batch, so a config arming many timers for one moment can still spend that many budgets in
        // one turn -- the same ceiling a capability with many `on_change` handlers already has.
        let outcome = CpuBudget::enter(lua).and_then(|budget| {
            callback.call::<()>(())?;
            budget.check_not_exceeded()
        });
        if let Err(err) = outcome {
            eprintln!("timer callback raised, ignoring it: {err}");
        }
    }
    if let Some(mut registry) = lua.app_data_mut::<TimerRegistry>() {
        registry.firing.clear();
    }
}

/// Drops the live set and holds back what the evaluation about to run arms, the counterpart to
/// [`super::action::clear`]. `next_id` keeps counting, so a handle from before this cancels nothing
/// armed after it.
pub fn begin_evaluation(lua: &Lua) {
    if lua.app_data_ref::<TimerRegistry>().is_none() {
        lua.set_app_data(TimerRegistry::default());
    }
    let mut registry = lua.app_data_mut::<TimerRegistry>().expect("just ensured the registry exists");
    registry.entries.clear();
    registry.staged.clear();
    registry.firing.clear();
    registry.evaluating = true;
}

/// The evaluation's output reached the screen, so what it armed becomes the live set.
pub fn promote(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<TimerRegistry>() {
        registry.entries = std::mem::take(&mut registry.staged);
        registry.evaluating = false;
    }
}

/// The evaluation's output was refused, superseded, or never produced, so what it armed goes with
/// it. Without this a generation swap leaves the outgoing process running the incoming config's
/// timers beside it. Also the "no evaluation is in flight" reset: after a failed one, nothing will
/// arrive to promote, and leaving `evaluating` set would stage a callback's timer forever.
pub fn discard(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<TimerRegistry>() {
        registry.staged.clear();
        registry.evaluating = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VM with `timer` installed and nothing else; dispatch needs no scene.
    fn lua() -> Lua {
        let lua = Lua::new();
        register(&lua).unwrap();
        lua
    }

    /// Runs whatever is due a day from now, which is every timer any of these tests arms.
    fn fire_everything(lua: &Lua) {
        dispatch_due(lua, Instant::now() + Duration::from_secs(MAX_MS / 1000));
    }

    #[test]
    fn a_due_timer_runs_its_callback_once() {
        let lua = lua();
        lua.load("fired = 0; timer(1, function() fired = fired + 1 end)").exec().unwrap();

        fire_everything(&lua);
        fire_everything(&lua);

        assert_eq!(lua.globals().get::<i64>("fired").unwrap(), 1, "a one-shot is consumed by firing");
    }

    #[test]
    fn a_timer_that_is_not_due_yet_does_not_run() {
        let lua = lua();
        lua.load("fired = false; timer(60000, function() fired = true end)").exec().unwrap();

        dispatch_due(&lua, Instant::now());

        assert!(!lua.globals().get::<bool>("fired").unwrap());
    }

    #[test]
    fn cancel_stops_a_timer_that_has_not_fired() {
        let lua = lua();
        lua.load("fired = false; timer(1, function() fired = true end):cancel()").exec().unwrap();

        fire_everything(&lua);

        assert!(!lua.globals().get::<bool>("fired").unwrap());
    }

    /// The case a cloned dispatch list gets wrong: the second timer is already in this batch's due
    /// set when the first one cancels it.
    #[test]
    fn a_callback_can_cancel_another_timer_due_in_the_same_batch() {
        let lua = lua();
        lua.load(
            r#"
            fired = false
            local second
            timer(1, function() second:cancel() end)
            second = timer(2, function() fired = true end)
        "#,
        )
        .exec()
        .unwrap();

        fire_everything(&lua);

        assert!(!lua.globals().get::<bool>("fired").unwrap(), "cancelling inside the batch must still take effect");
    }

    #[test]
    fn a_callback_cancelling_itself_is_a_no_op_rather_than_an_error() {
        let lua = lua();
        lua.load(
            r#"
            fired = 0
            local handle
            handle = timer(1, function() handle:cancel(); fired = fired + 1 end)
        "#,
        )
        .exec()
        .unwrap();

        fire_everything(&lua);

        assert_eq!(lua.globals().get::<i64>("fired").unwrap(), 1);
    }

    /// Re-arming is how a config repeats, so it must not be able to spin one turn forever.
    #[test]
    fn a_timer_armed_by_a_callback_waits_for_a_later_turn() {
        let lua = lua();
        lua.load(
            r#"
            fired = 0
            function arm() timer(1, function() fired = fired + 1; arm() end) end
            arm()
        "#,
        )
        .exec()
        .unwrap();

        fire_everything(&lua);
        assert_eq!(lua.globals().get::<i64>("fired").unwrap(), 1, "the re-armed timer is next turn's work");

        fire_everything(&lua);
        assert_eq!(lua.globals().get::<i64>("fired").unwrap(), 2);
    }

    /// One `Instant` for both, because two `timer(5, ..)` calls read the clock twice and would
    /// order correctly even if `arm` inserted before its equals instead of after.
    #[test]
    fn two_timers_due_at_the_very_same_instant_fire_in_the_order_they_were_armed() {
        let lua = lua();
        lua.load(r#"order = ""; first = function() order = order .. "a" end"#).exec().unwrap();
        lua.load(r#"second = function() order = order .. "b" end"#).exec().unwrap();
        let due = Instant::now();
        lua.set_app_data(TimerRegistry::default());
        {
            let mut registry = lua.app_data_mut::<TimerRegistry>().unwrap();
            registry.arm(due, lua.globals().get::<Function>("first").unwrap()).unwrap();
            registry.arm(due, lua.globals().get::<Function>("second").unwrap()).unwrap();
        }

        dispatch_due(&lua, due);

        assert_eq!(lua.globals().get::<String>("order").unwrap(), "ab");
    }

    #[test]
    fn the_next_deadline_is_the_earliest_armed_one() {
        let lua = lua();
        assert!(next_deadline(&lua).is_none(), "nothing armed keeps the poll loop timeout-free");

        lua.load("timer(60000, function() end); timer(10, function() end)").exec().unwrap();
        let earliest = next_deadline(&lua).expect("two are armed");

        assert!(earliest < Instant::now() + Duration::from_secs(30), "the 10ms one, not the 60s one");
    }

    /// The replacement is the point: ids must keep counting across `clear`, or the stale handle
    /// would cancel whatever reused its number.
    #[test]
    fn clear_disarms_everything_and_a_stale_handle_cannot_cancel_a_later_timer() {
        let lua = lua();
        lua.load("stale_fired = false; kept = timer(1, function() stale_fired = true end)").exec().unwrap();

        begin_evaluation(&lua);
        promote(&lua);
        lua.load("replacement_fired = false; timer(1, function() replacement_fired = true end)").exec().unwrap();
        lua.load("kept:cancel()").exec().expect("a handle outliving its registration still answers");
        fire_everything(&lua);

        assert!(!lua.globals().get::<bool>("stale_fired").unwrap(), "clear disarmed it");
        assert!(lua.globals().get::<bool>("replacement_fired").unwrap(), "the stale handle cancelled nothing");
    }

    /// `clear` from inside a callback: the rest of the batch is disarmed, and what the callback
    /// armed instead is left for a later turn like any other.
    #[test]
    fn a_callback_can_clear_the_rest_of_the_batch() {
        let lua = lua();
        lua.load("second_fired = false; armed_fired = false").exec().unwrap();
        lua.load(r#"timer(1, function() CLEAR(); timer(1, function() armed_fired = true end) end)"#).exec().unwrap();
        lua.load("timer(2, function() second_fired = true end)").exec().unwrap();
        let clear_fn = lua
            .create_function(|lua, ()| {
                begin_evaluation(lua);
                promote(lua);
                Ok(())
            })
            .unwrap();
        lua.globals().set("CLEAR", clear_fn).unwrap();

        fire_everything(&lua);

        assert!(!lua.globals().get::<bool>("second_fired").unwrap(), "cleared before its turn came");
        assert!(!lua.globals().get::<bool>("armed_fired").unwrap(), "armed mid-batch, so not this turn");
    }

    #[test]
    fn a_raising_callback_does_not_take_the_rest_of_the_batch_with_it() {
        let lua = lua();
        lua.load(
            r#"
            fired = false
            timer(1, function() error("boom") end)
            timer(2, function() fired = true end)
        "#,
        )
        .exec()
        .unwrap();

        fire_everything(&lua);

        assert!(lua.globals().get::<bool>("fired").unwrap(), "one bad callback must not take the batch with it");
    }

    /// The generation-swap case: an evaluation whose output nothing applies must not leave its
    /// timers running in the process that kept the old scene.
    #[test]
    fn a_discarded_evaluations_timers_never_fire() {
        let lua = lua();
        begin_evaluation(&lua);
        lua.load("fired = false; timer(1, function() fired = true end)").exec().unwrap();

        assert!(next_deadline(&lua).is_none(), "staged timers do not wake the poll loop");
        discard(&lua);
        fire_everything(&lua);

        assert!(!lua.globals().get::<bool>("fired").unwrap());
    }

    /// After a failed evaluation nothing arrives to promote, so the flag has to come down: a timer
    /// armed later from a callback would otherwise stage forever and never fire.
    #[test]
    fn a_discard_leaves_the_registry_ready_for_a_callback_to_arm_into() {
        let lua = lua();
        begin_evaluation(&lua);
        discard(&lua);
        lua.load("fired = false; timer(1, function() fired = true end)").exec().unwrap();

        assert!(next_deadline(&lua).is_some(), "armed live, not staged");
        fire_everything(&lua);

        assert!(lua.globals().get::<bool>("fired").unwrap());
    }

    #[test]
    fn an_applied_evaluations_timers_fire_once_promoted() {
        let lua = lua();
        begin_evaluation(&lua);
        lua.load("fired = false; timer(1, function() fired = true end)").exec().unwrap();
        promote(&lua);

        assert!(next_deadline(&lua).is_some(), "promoted timers arm the poll loop");
        fire_everything(&lua);

        assert!(lua.globals().get::<bool>("fired").unwrap());
    }

    /// A config cancelling at its top level is cancelling something still staged.
    #[test]
    fn cancel_reaches_a_timer_the_same_evaluation_armed() {
        let lua = lua();
        begin_evaluation(&lua);
        lua.load("fired = false; timer(1, function() fired = true end):cancel()").exec().unwrap();
        promote(&lua);
        fire_everything(&lua);

        assert!(!lua.globals().get::<bool>("fired").unwrap());
    }

    #[test]
    fn a_duration_outside_the_range_is_refused_rather_than_clamped() {
        let lua = lua();

        assert!(lua.load("timer(0, function() end)").exec().is_err());
        assert!(lua.load("timer(86400001, function() end)").exec().is_err());
        lua.load("timer(86400000, function() end)").exec().expect("a full day is the documented ceiling");
    }
}
