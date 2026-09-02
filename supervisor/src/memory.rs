//! Memory measurement harness (ADR-0043 decision 1): reads `/proc/[pid]/smaps_rollup` for PSS/USS
//! and `/proc/[pid]/fdinfo/*` for DRM (GPU) residency, one log line per sample. It only answers
//! "where is the memory", deciding and evicting nothing. Three numbers, per the ADR's three
//! items: total PSS across the supervisor and every live renderer (item 1, vs. the 50
//! MiB-per-monitor budget), per-renderer USS (item 2), and GPU residency from DRM fdinfo, never
//! folded into PSS (not in any `smaps` number under a real driver).

use std::collections::HashMap;
use std::io;
use std::time::Duration;

/// `OBLISK_MEMORY_SAMPLE_SECS`: seconds between periodic samples, or unset/`0` to disable the
/// timer. Named here so `main.rs`'s call site and this module agree on the string.
pub(crate) const SAMPLE_SECS_ENV: &str = "OBLISK_MEMORY_SAMPLE_SECS";

/// One process's `smaps_rollup`, in KiB (`/proc`'s native unit; converted to MiB only in
/// [`report_line`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rollup {
    pub(crate) pss: u64,
    pub(crate) uss: u64,
}

/// One DRM client's memory, from a single `/proc/[pid]/fdinfo/[fd]` file. `pdev`+`client_id` is
/// the dedupe key [`fold_drm_clients`] needs: several fds of one process share a `drm-client-id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrmClient {
    pub(crate) pdev: String,
    pub(crate) client_id: u64,
    pub(crate) resident: u64,
    pub(crate) shared: u64,
}

/// A process's GPU footprint after dedupe; `clients` is the deduped count, not the fd count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gpu {
    pub(crate) resident: u64,
    pub(crate) shared: u64,
    pub(crate) clients: usize,
}

/// One process's whole picture: PSS/USS from `smaps_rollup`, GPU residency from `fdinfo`.
#[derive(Debug)]
pub(crate) struct ProcessMemory {
    pub(crate) rollup: Rollup,
    pub(crate) gpu: Gpu,
}

/// A full sample: the supervisor plus every renderer that answered, keyed by generation id. A
/// renderer already exited by read time is simply absent, not an error (see [`sample`]).
#[derive(Debug)]
pub(crate) struct Sample {
    pub(crate) supervisor: ProcessMemory,
    pub(crate) renderers: Vec<(u32, ProcessMemory)>,
}

/// Parses `smaps_rollup` text into PSS and USS, `None` if `Pss:` is missing (a truncated rollup
/// is not a zero-byte process; reporting zero would be worse than nothing). The key matches
/// exactly against text before the first `:`, not by prefix: `Pss_Dirty:`, `Pss_Anon:`,
/// `Pss_File:`, `Pss_Shmem:` and `SwapPss:` all start with "Pss" and a prefix match would wrongly
/// catch them (ADR-0043 decision 1 item 1). USS is `Private_Clean + Private_Dirty` (decision 1
/// item 2): pages nothing else maps.
pub(crate) fn parse_rollup(text: &str) -> Option<Rollup> {
    let mut pss = None;
    let mut private_clean = 0u64;
    let mut private_dirty = 0u64;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let Some(kib) = parse_rollup_kib(value) else { continue };
        match key.trim() {
            "Pss" => pss = Some(kib),
            "Private_Clean" => private_clean = kib,
            "Private_Dirty" => private_dirty = kib,
            _ => {}
        }
    }
    Some(Rollup { pss: pss?, uss: private_clean + private_dirty })
}

/// `smaps_rollup`'s value column is always `<number> kB` (the kernel's long-standing, if
/// misleadingly named, KiB convention here). Unlike DRM fdinfo below, no competing unit to misread.
fn parse_rollup_kib(value: &str) -> Option<u64> {
    value.split_whitespace().next()?.parse().ok()
}

/// Parses one `/proc/[pid]/fdinfo/[fd]` file into a [`DrmClient`], `None` without a `drm-driver:`
/// line. Fields are `key:` TAB `value`; the trailing `:` is stripped after splitting on the tab,
/// since `drm-pdev`'s value (`0000:00:02.0`) itself contains colons. Resident memory sums
/// `drm-resident-<region>:` fields (regions are disjoint: `system0` and `stolen-system0` here);
/// with none present, falls back to `drm-memory-<region>:` instead, never both, or new-field
/// drivers would double count. Shared memory sums `drm-shared-<region>:` the same way;
/// `drm-total-*`, `drm-active-*`, `drm-purgeable-*` and `drm-engine-*` (nanoseconds) are ignored.
pub(crate) fn parse_drm_client(text: &str) -> Option<DrmClient> {
    let mut has_driver = false;
    let mut pdev = None;
    let mut client_id = None;
    // saw_resident_key tracks whether the modern naming appeared at all, separate from resident's
    // value: folded together, a refused unparseable value would look like a field never reported
    // and silently fall back to a legacy number; a genuine zero must not fall back either.
    let mut saw_resident_key = false;
    let mut resident: u64 = 0;
    let mut shared: u64 = 0;
    let mut legacy_memory: u64 = 0;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('\t') else { continue };
        let key = key.trim_end_matches(':');
        let value = value.trim();
        if key == "drm-driver" {
            has_driver = true;
        } else if key == "drm-pdev" {
            pdev = Some(value.to_string());
        } else if key == "drm-client-id" {
            client_id = value.parse().ok();
        } else if key.starts_with("drm-resident-") {
            saw_resident_key = true;
            resident += parse_drm_kib(value).unwrap_or(0);
        } else if key.starts_with("drm-shared-") {
            shared += parse_drm_kib(value).unwrap_or(0);
        } else if key.starts_with("drm-memory-") {
            legacy_memory += parse_drm_kib(value).unwrap_or(0);
        }
    }

    if !has_driver {
        return None;
    }
    let resident = if saw_resident_key { resident } else { legacy_memory };
    Some(DrmClient { pdev: pdev?, client_id: client_id?, resident, shared })
}

/// A `drm-resident-<region>`/`drm-shared-<region>`/`drm-memory-<region>` value: `279968 KiB`, or a
/// bare `0` (real fdinfo prints zero unitless, everything else with a unit). A bare nonzero number
/// or non-KiB unit is refused, not guessed: misreading MiB as KiB would under-report by 1024x.
fn parse_drm_kib(value: &str) -> Option<u64> {
    if value == "0" {
        return Some(0);
    }
    let mut parts = value.split_whitespace();
    let number = parts.next()?;
    let unit = parts.next()?;
    if unit != "KiB" || parts.next().is_some() {
        return None;
    }
    number.parse().ok()
}

/// Dedupes DRM clients by `(pdev, client_id)` before summing: one client can hold many fds
/// reporting the same id (confirmed live: three fds of one process report `drm-client-id: 4` with
/// identical byte counts), so summing every fd would triple-count. Keeps the first fd seen.
pub(crate) fn fold_drm_clients(clients: impl IntoIterator<Item = DrmClient>) -> Gpu {
    let mut kept: HashMap<(String, u64), DrmClient> = HashMap::new();
    for client in clients {
        kept.entry((client.pdev.clone(), client.client_id)).or_insert(client);
    }
    let resident = kept.values().map(|client| client.resident).sum();
    let shared = kept.values().map(|client| client.shared).sum();
    Gpu { resident, shared, clients: kept.len() }
}

/// Parses [`SAMPLE_SECS_ENV`]'s already-read value into a sampling interval. `None` (unset), an
/// unparseable string, and `"0"` all mean the periodic sampler is off, one case for the caller.
pub(crate) fn interval_from_env(value: Option<&str>) -> Option<Duration> {
    let secs: u64 = value?.parse().ok()?;
    if secs == 0 { None } else { Some(Duration::from_secs(secs)) }
}

/// The one log line a sample produces: `total pss` is the supervisor's PSS plus every renderer's
/// PSS (ADR-0043 decision 1 item 1, vs. the 50 MiB-per-monitor budget); GPU residency is per
/// renderer only, never added in (not in any `smaps` number). One `; generation N ...` clause per
/// renderer, in `sample`'s order. The DRM client count is the only evidence [`fold_drm_clients`]
/// deduped rather than summed: a count of 1 next to a plausible number is a measurement, not luck.
pub(crate) fn report_line(label: &str, sample: &Sample) -> String {
    let total_pss: u64 =
        sample.supervisor.rollup.pss + sample.renderers.iter().map(|(_, memory)| memory.rollup.pss).sum::<u64>();
    let mut line = format!(
        "[oblisk-memory] {label}: total pss {:.1} MiB; supervisor pss {:.1} MiB",
        mib(total_pss),
        mib(sample.supervisor.rollup.pss)
    );
    for (generation_id, memory) in &sample.renderers {
        line.push_str(&format!(
            "; generation {generation_id} pss {:.1} MiB uss {:.1} MiB gpu {:.1} MiB ({:.1} MiB shared, {} drm client(s))",
            mib(memory.rollup.pss),
            mib(memory.rollup.uss),
            mib(memory.gpu.resident),
            mib(memory.gpu.shared),
            memory.gpu.clients,
        ));
    }
    line
}

fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

/// Reads one process's `smaps_rollup` and every readable `fdinfo` under `/proc/<who>` (`who` is a
/// pid or `"self"`). A missing/malformed rollup errors; no DRM fds is a correctly zeroed [`Gpu`].
fn read_process_memory(who: &str) -> io::Result<ProcessMemory> {
    let rollup_text = std::fs::read_to_string(format!("/proc/{who}/smaps_rollup"))?;
    let rollup = parse_rollup(&rollup_text).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("/proc/{who}/smaps_rollup has no Pss: line"))
    })?;
    Ok(ProcessMemory { rollup, gpu: read_gpu(who) })
}

/// Sums DRM residency across every fd in `/proc/<who>/fdinfo`. A missing `fdinfo` directory and an
/// fd that vanishes between `read_dir` and its `read` (fds close constantly; normal) both fold to
/// "no DRM memory found" rather than propagating.
fn read_gpu(who: &str) -> Gpu {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{who}/fdinfo")) else {
        return fold_drm_clients(std::iter::empty());
    };
    let clients = entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok().and_then(|text| parse_drm_client(&text)));
    fold_drm_clients(clients)
}

/// Reads one renderer's memory by pid, as opposed to the supervisor's own via `/proc/self`.
fn read_process(pid: u32) -> io::Result<ProcessMemory> {
    read_process_memory(&pid.to_string())
}

/// Reads a full sample: the supervisor's own numbers via `/proc/self`, then every renderer in
/// `renderer_pids` (`(generation_id, pid)` pairs, in order). A pid already exited (ordinary during
/// a PBA handoff or crash) is logged and skipped; only the supervisor's read is fatal.
pub(crate) fn sample(renderer_pids: &[(u32, u32)]) -> io::Result<Sample> {
    let supervisor = read_process_memory("self")?;
    let mut renderers = Vec::with_capacity(renderer_pids.len());
    for &(generation_id, pid) in renderer_pids {
        match read_process(pid) {
            Ok(memory) => renderers.push((generation_id, memory)),
            Err(err) => eprintln!(
                "[oblisk-memory] generation {generation_id} (pid {pid}) could not be sampled, skipping: {err}"
            ),
        }
    }
    Ok(Sample { supervisor, renderers })
}

/// Builds the steady-state sampler from the environment, or `None` to leave it off (ADR-0043's
/// sampling amendment). Opt-in: `smaps_rollup` is readable from outside any time, so an always-on
/// timer adds nothing but a log line; the handoff sample stays unconditional since nobody outside
/// can catch a swap-only window. The first tick is one period out, not immediate (a just-spawned
/// Renderer isn't steady yet); `Skip` because a sampler that fell behind wants the current reading.
pub(crate) fn sampler_from_env() -> Option<tokio::time::Interval> {
    let period = interval_from_env(std::env::var(SAMPLE_SECS_ENV).ok().as_deref())?;
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    Some(interval)
}

/// Reads and logs one memory sample across the Supervisor and the Renderers named by `renderers`
/// (ADR-0043 decision 1). Two callers in `main.rs`: the steady-state timer (single authoritative
/// generation) and the PBA arm (both generations, during the window two are alive). A `Child`
/// whose `id()` is `None` has already been reaped and is dropped, not reported as zero.
pub(crate) fn log_sample(label: &str, renderers: &[(u32, &tokio::process::Child)]) {
    let pids: Vec<(u32, u32)> =
        renderers.iter().filter_map(|(generation_id, child)| child.id().map(|pid| (*generation_id, pid))).collect();
    match sample(&pids) {
        Ok(sample) => eprintln!("{}", report_line(label, &sample)),
        Err(err) => eprintln!("[oblisk-memory] {label} sample failed: {err}"),
    }
}

/// The steady-state sampler's `select!` arm: never resolves when off (an `Option<Interval>` can't
/// be `.tick()`ed directly inside `select!`, and a disabled sampler must not resolve or it'd spin
/// the loop). Both `Interval::tick` and `pending` are cancel-safe, as `select!` requires.
pub(crate) async fn tick_sampler(interval: &mut Option<tokio::time::Interval>) -> Option<tokio::time::Instant> {
    match interval {
        Some(interval) => Some(interval.tick().await),
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_rollup ----

    /// Real `smaps_rollup` output from this machine. Pss: 208 kB; Private_Clean: 48 kB;
    /// Private_Dirty: 108 kB -> pss 208, uss 156.
    const ROLLUP_FIXTURE: &str = "\
56431addf000-7fffc2d7a000 ---p 00000000 00:00 0                          [rollup]
Rss:                3932 kB
Pss:                 208 kB
Pss_Dirty:           108 kB
Pss_Anon:            108 kB
Pss_File:            100 kB
Pss_Shmem:             0 kB
Shared_Clean:       3776 kB
Shared_Dirty:          0 kB
Private_Clean:        48 kB
Private_Dirty:       108 kB
Referenced:         3932 kB
Anonymous:           108 kB
KSM:                   0 kB
LazyFree:              0 kB
AnonHugePages:         0 kB
ShmemPmdMapped:        0 kB
FilePmdMapped:      2048 kB
Shared_Hugetlb:        0 kB
Private_Hugetlb:       0 kB
Swap:                  0 kB
SwapPss:               0 kB
Locked:                0 kB
";

    #[test]
    fn parse_rollup_reads_pss_and_sums_private_clean_and_dirty_into_uss() {
        assert_eq!(parse_rollup(ROLLUP_FIXTURE), Some(Rollup { pss: 208, uss: 156 }));
    }

    #[test]
    fn parse_rollup_does_not_let_pss_dirty_or_other_pss_prefixed_lines_masquerade_as_pss() {
        // A prefix match on "Pss" would find Pss_Dirty: 108 kB here and report 108, not None.
        let text = "Pss_Dirty:           108 kB\nPss_Anon:            108 kB\nPrivate_Clean:        48 kB\n";
        assert_eq!(parse_rollup(text), None);
    }

    #[test]
    fn parse_rollup_is_none_without_a_pss_line() {
        assert_eq!(
            parse_rollup("Rss:                3932 kB\n"),
            None,
            "a truncated rollup must not be reported as a zero-byte process"
        );
    }

    // ---- parse_drm_client ----

    /// Real `fdinfo` output from this machine's i915 GPU. `drm-resident-system0` and
    /// `drm-resident-stolen-system0` sum to 279968; that they equal `drm-total-system0` here is a
    /// coincidence of a fully-resident buffer, not evidence `drm-total-*` was read -- see the
    /// dedicated total-is-not-summed test below.
    const DRM_FIXTURE: &str = "pos:\t0\n\
flags:\t02104002\n\
mnt_id:\t357\n\
ino:\t406\n\
drm-driver:\ti915\n\
drm-client-id:\t4\n\
drm-pdev:\t0000:00:02.0\n\
drm-total-system0:\t279968 KiB\n\
drm-shared-system0:\t192368 KiB\n\
drm-active-system0:\t0\n\
drm-resident-system0:\t279968 KiB\n\
drm-purgeable-system0:\t880 KiB\n\
drm-total-stolen-system0:\t0\n\
drm-shared-stolen-system0:\t0\n\
drm-active-stolen-system0:\t0\n\
drm-resident-stolen-system0:\t0\n\
drm-purgeable-stolen-system0:\t0\n\
drm-engine-render:\t324435861152 ns\n\
drm-engine-copy:\t0 ns\n\
drm-engine-video:\t0 ns\n\
drm-engine-capacity-video:\t2\n\
drm-engine-video-enhance:\t0 ns\n";

    #[test]
    fn parse_drm_client_reads_pdev_client_id_and_sums_resident_and_shared_regions() {
        assert_eq!(
            parse_drm_client(DRM_FIXTURE),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 279968, shared: 192368 })
        );
    }

    #[test]
    fn parse_drm_client_is_none_without_a_drm_driver_line() {
        let text = "pos:\t0\nflags:\t02\nmnt_id:\t9\nino:\t123\n";
        assert_eq!(
            parse_drm_client(text),
            None,
            "most fds are not DRM fds and must not be reported as zero-byte GPU clients"
        );
    }

    #[test]
    fn parse_drm_client_ignores_drm_total_and_only_sums_resident_and_shared() {
        let text = "drm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t9\ndrm-total-system0:\t500000 KiB\ndrm-resident-system0:\t100000 KiB\ndrm-shared-system0:\t20000 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:03:00.0".to_string(), client_id: 9, resident: 100000, shared: 20000 })
        );
    }

    #[test]
    fn parse_drm_client_falls_back_to_the_older_drm_memory_naming_when_no_resident_field_exists() {
        let text = "drm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t7\ndrm-memory-vram:\t102400 KiB\ndrm-memory-gtt:\t51200 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:03:00.0".to_string(), client_id: 7, resident: 153600, shared: 0 })
        );
    }

    #[test]
    fn parse_drm_client_never_adds_the_resident_and_memory_style_fields_together() {
        // A driver could report both naming styles in the same file; resident, when present,
        // wins outright rather than being added to the fallback.
        let text = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t4\ndrm-resident-system0:\t1000 KiB\ndrm-memory-system0:\t9999 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 1000, shared: 0 })
        );
    }

    #[test]
    fn parse_drm_client_treats_a_bare_zero_resident_value_as_present_not_missing() {
        // If a bare "0" failed to parse, this would fall back to drm-memory-system0's 99999.
        let text = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t4\ndrm-resident-system0:\t0\ndrm-memory-system0:\t99999 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 0, shared: 0 })
        );
    }

    /// The fallback is selected by the modern key being absent, never by its value failing to
    /// parse -- answering with a stale legacy number would report a wrong number, not no number.
    #[test]
    fn an_unparseable_resident_value_does_not_fall_back_to_the_legacy_field() {
        let text = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t4\ndrm-resident-system0:\t50 MiB\ndrm-memory-system0:\t99999 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 0, shared: 0 })
        );
    }

    #[test]
    fn parse_drm_client_ignores_a_field_reported_in_a_unit_other_than_kib() {
        // Misreading "50 MiB" as 50 KiB would under-report by 1024x; dropped instead.
        let text = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t4\ndrm-resident-system0:\t50 MiB\ndrm-resident-other:\t1000 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 1000, shared: 0 })
        );
    }

    // ---- fold_drm_clients ----

    #[test]
    fn fold_drm_clients_dedupes_by_pdev_and_client_id_so_one_process_is_not_triple_counted() {
        // Three separate fds of one process, as observed live on this machine: each reports
        // drm-client-id: 4 on the same pdev with identical byte counts.
        let one_fd = || DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 279968, shared: 192368 };
        let gpu = fold_drm_clients([one_fd(), one_fd(), one_fd()]);
        assert_eq!(gpu, Gpu { resident: 279968, shared: 192368, clients: 1 });
    }

    #[test]
    fn fold_drm_clients_sums_distinct_clients_separately() {
        let a = DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 100, shared: 10 };
        let b = DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 5, resident: 200, shared: 20 };
        assert_eq!(fold_drm_clients([a, b]), Gpu { resident: 300, shared: 30, clients: 2 });
    }

    #[test]
    fn fold_drm_clients_of_no_clients_is_a_zeroed_gpu() {
        assert_eq!(fold_drm_clients(std::iter::empty()), Gpu { resident: 0, shared: 0, clients: 0 });
    }

    // ---- interval_from_env ----

    #[test]
    fn interval_from_env_is_none_when_unset() {
        assert_eq!(interval_from_env(None), None);
    }

    #[test]
    fn interval_from_env_is_none_for_zero() {
        assert_eq!(
            interval_from_env(Some("0")),
            None,
            "0 means the periodic sampler is off, not an interval of zero seconds"
        );
    }

    #[test]
    fn interval_from_env_is_none_for_an_unparseable_value() {
        assert_eq!(interval_from_env(Some("not-a-number")), None);
    }

    #[test]
    fn interval_from_env_parses_a_positive_integer_as_seconds() {
        assert_eq!(interval_from_env(Some("5")), Some(Duration::from_secs(5)));
    }

    // ---- report_line ----

    #[test]
    fn report_line_formats_total_supervisor_and_one_clause_per_renderer_in_order() {
        let sample = Sample {
            supervisor: ProcessMemory {
                rollup: Rollup { pss: 8192, uss: 0 },
                gpu: Gpu { resident: 0, shared: 0, clients: 0 },
            },
            renderers: vec![
                (
                    0,
                    ProcessMemory {
                        rollup: Rollup { pss: 51200, uss: 40960 },
                        gpu: Gpu { resident: 20480, shared: 3072, clients: 1 },
                    },
                ),
                (
                    1,
                    ProcessMemory {
                        rollup: Rollup { pss: 10240, uss: 5120 },
                        gpu: Gpu { resident: 1024, shared: 512, clients: 1 },
                    },
                ),
            ],
        };

        assert_eq!(
            report_line("periodic", &sample),
            "[oblisk-memory] periodic: total pss 68.0 MiB; supervisor pss 8.0 MiB; \
             generation 0 pss 50.0 MiB uss 40.0 MiB gpu 20.0 MiB (3.0 MiB shared, 1 drm client(s)); \
             generation 1 pss 10.0 MiB uss 5.0 MiB gpu 1.0 MiB (0.5 MiB shared, 1 drm client(s))"
        );
    }

    #[test]
    fn report_line_total_pss_excludes_gpu_and_uss() {
        // ADR-0043 decision 1: total pss is a PSS sum, not a PSS+USS sum -- USS is reported
        // per renderer only.
        let sample = Sample {
            supervisor: ProcessMemory {
                rollup: Rollup { pss: 1024, uss: 999_999 },
                gpu: Gpu { resident: 999_999, shared: 999_999, clients: 1 },
            },
            renderers: vec![(
                0,
                ProcessMemory {
                    rollup: Rollup { pss: 1024, uss: 999_999 },
                    gpu: Gpu { resident: 999_999, shared: 999_999, clients: 1 },
                },
            )],
        };
        assert!(report_line("check", &sample).starts_with("[oblisk-memory] check: total pss 2.0 MiB;"));
    }
}
