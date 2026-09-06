//! Reports where the Renderer's heap sits when `OBLISK_PROFILE_MEMORY` is set. `smaps` already
//! says how much a generation holds (`supervisor/src/memory.rs`, ADR-0043); it cannot say which
//! subsystem holds it. A 16h session measured 120.8 MB RSS against 84.8 MB fresh, 48 MB of it in
//! glibc's `[heap]` against `image`'s 16 MB `TEXTURE_BUDGET`, with no drift while idle.
//! That shape needs per-subsystem counters read at the same instant, which is what this prints.
//!
//! `mallinfo2`'s in-use/free split is the load-bearing pair: growing `in_use` is a live leak,
//! while a growing `free` under a flat `in_use` is glibc holding freed chunks a
//! `supervisor::memory::return_free_pages_to_the_kernel`-style trim could hand back. Nothing else
//! here distinguishes those two, and they have opposite fixes.
//!
//! Unset means one `Instant::elapsed` per turn and no counters read: collection is behind a
//! closure the report interval gates. Set a positive interval in seconds
//! (`OBLISK_PROFILE_MEMORY=60`); invalid input uses [`DEFAULT_INTERVAL_SECS`] rather than
//! preventing startup, matching `idle_profile`.

use std::time::{Duration, Instant};

/// Fallback for a non-positive or invalid `OBLISK_PROFILE_MEMORY`. A minute is short enough to
/// bracket one deliberate action and long enough to leave an overnight log readable.
const DEFAULT_INTERVAL_SECS: u64 = 60;

/// glibc's arena totals from `mallinfo2`, in bytes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Malloc {
    /// `arena`: bytes obtained from the kernel via `brk`, the `[heap]` mapping `smaps` shows.
    pub arena: u64,
    /// `hblkhd`: bytes in `mmap`ed blocks, which large allocations take instead of the arena.
    pub mmapped: u64,
    /// `uordblks`: bytes in chunks handed out and not yet freed. This is the shell's live heap.
    pub in_use: u64,
    /// `fordblks`: bytes glibc holds on its free lists. Freed by the shell, still charged to the
    /// process until a trim returns the pages.
    pub free: u64,
}

impl Malloc {
    /// Reads every arena's totals. Zeroed on a platform without `mallinfo2`, which loses only
    /// these columns.
    #[cfg(target_env = "gnu")]
    fn now() -> Self {
        // SAFETY: plain FFI returning a POD struct by value. `mallinfo2` takes no arguments,
        // locks the arenas itself, and only reads counters.
        let info = unsafe { libc::mallinfo2() };
        Self {
            arena: info.arena as u64,
            mmapped: info.hblkhd as u64,
            in_use: info.uordblks as u64,
            free: info.fordblks as u64,
        }
    }

    #[cfg(not(target_env = "gnu"))]
    fn now() -> Self {
        Self::default()
    }
}

/// One instant's per-subsystem byte and entry counts. Built by the caller, which owns every
/// source; this module only formats and diffs, so [`render`] stays pure and testable.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Census {
    /// `ImageCache::resident_bytes`: uploaded texture bytes across `Ready` slots.
    pub image_bytes: u64,
    pub image_ready: u64,
    pub image_pending: u64,
    pub image_failed: u64,
    /// Evicted ids awaiting `release_evicted`, and decodes awaiting upload. Both are drained
    /// every turn in a healthy loop, so a nonzero reading that persists is itself the finding.
    pub image_evicted: u64,
    pub image_landed: u64,
    /// Memoized measurements, against `SHAPE_CACHE_CAPACITY`.
    pub shape_entries: u64,
    /// Bytes of the shape cache's own strings and ranges. Excludes `HashMap` overhead, which is
    /// why this is `approx` in the report: it is a floor, not a total.
    pub shape_bytes: u64,
    /// `mlua::Lua::used_memory`: the Lua VM's own heap, which the config's tables and closures
    /// live in. Rust allocations the VM merely points at are not counted here.
    pub lua_bytes: u64,
    /// Retained surface trees, their total node count, and the lease bag's depth. `retiring` is
    /// cleared after every apply, so a nonzero reading means an apply is mid-flight.
    pub scene_surfaces: u64,
    pub scene_nodes: u64,
    pub scene_retiring: u64,
    /// Live `mlua::Value`s across every retained node's `properties` map, the one place scene
    /// growth reaches the Lua heap.
    pub scene_properties: u64,
    pub malloc: Malloc,
}

/// Accumulates nothing: each report is one instant's reading beside the previous one, because
/// every counter here is a level rather than a rate.
pub struct MemoryProfile {
    interval: Duration,
    started: Instant,
    window_started: Instant,
    /// The reading this one is diffed against, and the first one taken, so a report shows both
    /// the step since the last window and the drift since startup.
    previous: Option<Census>,
    first: Option<Census>,
}

impl MemoryProfile {
    /// `Some` only when `OBLISK_PROFILE_MEMORY` is set.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("OBLISK_PROFILE_MEMORY").ok()?;
        let secs = raw.trim().parse::<u64>().ok().filter(|s| *s > 0).unwrap_or(DEFAULT_INTERVAL_SECS);
        eprintln!("[oblisk-renderer] memory profile on, reporting every {secs}s");
        let now = Instant::now();
        Some(Self {
            interval: Duration::from_secs(secs),
            started: now,
            window_started: now,
            previous: None,
            first: None,
        })
    }

    /// Reports when the window is up, reading `collect` only then. The turn loop calls this every
    /// turn, so the closure keeps a whole-scene walk off the idle path.
    pub fn maybe_report(&mut self, collect: impl FnOnce() -> Census) {
        if self.window_started.elapsed() < self.interval {
            return;
        }
        let census = collect();
        let malloc = Malloc::now();
        let census = Census { malloc, ..census };
        eprintln!(
            "[oblisk-renderer] {}",
            render(self.started.elapsed(), &census, self.previous.as_ref(), self.first.as_ref())
        );
        self.first.get_or_insert(census);
        self.previous = Some(census);
        self.window_started = Instant::now();
    }
}

/// Pure report formatting. Prints levels, then the step since `previous`, then the drift since
/// `first`, because a leak is visible in the third column long before it is in the first.
fn render(uptime: Duration, now: &Census, previous: Option<&Census>, first: Option<&Census>) -> String {
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    let mut line = format!(
        "memory t={:.0}s: malloc arena={:.1} in_use={:.1} free={:.1} mmap={:.1} MiB \
         | image {:.1} MiB ready={} pending={} failed={} evicted={} landed={} \
         | shape entries={} approx={:.1} MiB | lua {:.1} MiB \
         | scene surfaces={} nodes={} props={} retiring={}",
        uptime.as_secs_f64(),
        mib(now.malloc.arena),
        mib(now.malloc.in_use),
        mib(now.malloc.free),
        mib(now.malloc.mmapped),
        mib(now.image_bytes),
        now.image_ready,
        now.image_pending,
        now.image_failed,
        now.image_evicted,
        now.image_landed,
        now.shape_entries,
        mib(now.shape_bytes),
        mib(now.lua_bytes),
        now.scene_surfaces,
        now.scene_nodes,
        now.scene_properties,
        now.scene_retiring,
    );
    if let Some(previous) = previous {
        line.push_str(&format!(" | step {}", deltas(now, previous)));
    }
    if let Some(first) = first {
        line.push_str(&format!(" | since_start {}", deltas(now, first)));
    }
    line
}

/// The four numbers worth watching over time, signed, in KiB because the interesting steps are
/// hundreds of KiB long before they are megabytes.
fn deltas(now: &Census, earlier: &Census) -> String {
    let kib = |now: u64, earlier: u64| (now as i64 - earlier as i64) as f64 / 1024.0;
    format!(
        "in_use={:+.0} free={:+.0} image={:+.0} lua={:+.0} KiB shape={:+} nodes={:+}",
        kib(now.malloc.in_use, earlier.malloc.in_use),
        kib(now.malloc.free, earlier.malloc.free),
        kib(now.image_bytes, earlier.image_bytes),
        kib(now.lua_bytes, earlier.lua_bytes),
        now.shape_entries as i64 - earlier.shape_entries as i64,
        now.scene_nodes as i64 - earlier.scene_nodes as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn census(in_use: u64, image_bytes: u64) -> Census {
        Census {
            image_bytes,
            shape_entries: 12,
            lua_bytes: 4 * 1024 * 1024,
            scene_nodes: 300,
            malloc: Malloc { arena: 50 * 1024 * 1024, mmapped: 0, in_use, free: 1024 * 1024 },
            ..Census::default()
        }
    }

    #[test]
    fn the_first_report_has_no_step_or_drift_columns_to_show() {
        let line = render(Duration::from_secs(60), &census(1024, 0), None, None);
        assert!(line.contains("t=60s"), "{line}");
        assert!(!line.contains("step"), "{line}");
        assert!(!line.contains("since_start"), "{line}");
    }

    #[test]
    fn a_later_report_shows_the_step_since_the_last_window_and_the_drift_since_startup() {
        let first = census(10 * 1024 * 1024, 0);
        let previous = census(11 * 1024 * 1024, 0);
        let now = census(12 * 1024 * 1024, 0);
        let line = render(Duration::from_secs(180), &now, Some(&previous), Some(&first));
        assert!(line.contains("step in_use=+1024"), "{line}");
        assert!(line.contains("since_start in_use=+2048"), "{line}");
    }

    #[test]
    fn a_shrinking_counter_reads_as_negative_rather_than_wrapping() {
        // Subtracting `u64`s directly would make a freed megabyte read as 16 exabytes.
        let line = deltas(&census(1024 * 1024, 0), &census(3 * 1024 * 1024, 0));
        assert!(line.contains("in_use=-2048"), "{line}");
    }

    #[test]
    fn in_use_and_free_are_reported_separately_because_their_fixes_differ() {
        // A flat `in_use` beside a growing `free` is retention a trim returns; the reverse is a
        // live leak. One combined number would hide which of the two is happening.
        let line = render(Duration::from_secs(60), &census(20 * 1024 * 1024, 0), None, None);
        assert!(line.contains("malloc arena=50.0 in_use=20.0 free=1.0"), "{line}");
    }
}
