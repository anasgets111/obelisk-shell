//! Memory measurement harness (ADR-0043 decision 1): `/proc/[pid]/smaps_rollup` supplies PSS/USS;
//! `/proc/[pid]/fdinfo/*` supplies DRM residency. It reports, never evicts shell state: total PSS
//! for supervisor plus live renderers (item 1, against the 50 MiB-per-monitor budget), per-renderer
//! USS (item 2), and GPU residency, never folded into PSS because real drivers omit it from
//! `smaps`.
//!
//! [`return_free_pages_to_the_kernel`] only returns allocator pages, never shell state.

use std::collections::HashMap;
use std::io;
use std::path::Path;

/// Passed in at the one production call site, [`log_sample`], instead of being reached for inside
/// the readers, so a test can point them at a tempdir of fake files. Every sysfs and procfs reader
/// in `capabilities` takes its root the same way.
const PROC_ROOT: &str = "/proc";

/// One process's `smaps_rollup`, in KiB, `/proc`'s native unit. [`report_line`] converts to MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rollup {
    pub(crate) pss: u64,
    pub(crate) uss: u64,
}

/// One DRM client's memory from `/proc/[pid]/fdinfo/[fd]`. `pdev`+`client_id` dedupes fds sharing a
/// `drm-client-id` in [`fold_drm_clients`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrmClient {
    pub(crate) pdev: String,
    pub(crate) client_id: u64,
    pub(crate) resident: u64,
    pub(crate) shared: u64,
}

/// A process's deduped GPU footprint; `clients` counts clients, not fds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gpu {
    pub(crate) resident: u64,
    pub(crate) shared: u64,
    pub(crate) clients: usize,
}

/// One process's PSS/USS from `smaps_rollup` and GPU residency from `fdinfo`.
#[derive(Debug)]
pub(crate) struct ProcessMemory {
    pub(crate) rollup: Rollup,
    pub(crate) gpu: Gpu,
}

/// Supervisor plus every renderer that answered, keyed by generation. A renderer already exited
/// at read time is absent, not an error (see [`sample`]).
#[derive(Debug)]
pub(crate) struct Sample {
    pub(crate) supervisor: ProcessMemory,
    pub(crate) renderers: Vec<(u32, ProcessMemory)>,
}

/// Parses `smaps_rollup` into PSS and USS. Missing `Pss:` yields `None`: a truncated rollup is not
/// a zero-byte process. Match the key before the first `:` exactly; `Pss_Dirty`, `Pss_Anon`,
/// `Pss_File`, `Pss_Shmem`, and `SwapPss` would fool a prefix match (ADR-0043 decision 1 item 1).
/// USS is `Private_Clean + Private_Dirty` (decision 1 item 2), pages nothing else maps.
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

/// Hands glibc's already-free pages back to the kernel across every arena.
///
/// Freeing does not shrink the process: glibc keeps chunks on free lists, and a `spawn_blocking`
/// arena is not trimmed on its own. `libalpm`'s update check parses the sync database and leaves
/// tens of MiB of dead allocations. Before this existed, memory was 31 MiB before the check, 84
/// MiB on completion, and still 84 MiB minutes later, consuming ADR-0043's 50 MiB-per-monitor
/// budget after the work ended.
///
/// Use for a job of that scale, never in a loop: it walks every arena's free lists and locks each
/// arena, so it belongs at the blocking job's end, not on a timer.
pub(crate) fn return_free_pages_to_the_kernel() {
    // SAFETY: plain one-integer FFI. `malloc_trim` locks arenas itself, is thread-safe, and only
    // `madvise`s pages the allocator already holds free.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// `smaps_rollup` values are `<number> kB`, the kernel's long-standing name for KiB. Unlike DRM
/// fdinfo, no competing unit exists here.
fn parse_rollup_kib(value: &str) -> Option<u64> {
    value.split_whitespace().next()?.parse().ok()
}

/// Parses `/proc/[pid]/fdinfo/[fd]` into [`DrmClient`], or `None` without `drm-driver:`. Fields
/// are `key:` TAB `value`; strip `:` after splitting because `drm-pdev` values such as
/// `0000:00:02.0` contain colons. Sum `drm-resident-<region>:` fields, falling back to
/// `drm-memory-<region>:` only when none exist. Never add both, or new-field drivers double count.
/// Sum `drm-shared-<region>:` likewise; ignore `drm-total-*`, `drm-active-*`, `drm-purgeable-*`,
/// and `drm-engine-*` nanoseconds.
pub(crate) fn parse_drm_client(text: &str) -> Option<DrmClient> {
    let mut has_driver = false;
    let mut pdev = None;
    let mut client_id = None;
    // Track modern naming separately from its value: an unparseable value must not look absent and
    // fall back to a legacy number; a genuine zero must not fall back either.
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

/// A `drm-resident-<region>`, `drm-shared-<region>`, or `drm-memory-<region>` value: `279968 KiB`
/// or bare `0` (real fdinfo prints zero unitless). Refuse bare nonzero and non-KiB units rather
/// than guess; reading MiB as KiB under-reports by 1024x.
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

/// Dedupes by `(pdev, client_id)` before summing. One client can hold many fds with the same id
/// (live: three fds reported `drm-client-id: 4` with identical bytes), so summing triples counts.
/// Keeps the first fd seen.
pub(crate) fn fold_drm_clients(clients: impl IntoIterator<Item = DrmClient>) -> Gpu {
    let mut kept: HashMap<(String, u64), DrmClient> = HashMap::new();
    for client in clients {
        kept.entry((client.pdev.clone(), client.client_id)).or_insert(client);
    }
    let resident = kept.values().map(|client| client.resident).sum();
    let shared = kept.values().map(|client| client.shared).sum();
    Gpu { resident, shared, clients: kept.len() }
}

/// One log line per sample. `total pss` sums supervisor and renderer PSS (ADR-0043 decision 1
/// item 1, against 50 MiB per monitor); GPU stays per renderer because `smaps` omits it. Emit one
/// `; generation N ...` clause in sample order. DRM client count shows [`fold_drm_clients`]
/// deduped rather than summed: `1` beside a plausible number is measured, not luck.
pub(crate) fn report_line(label: &str, sample: &Sample) -> String {
    let total_pss: u64 =
        sample.supervisor.rollup.pss + sample.renderers.iter().map(|(_, memory)| memory.rollup.pss).sum::<u64>();
    let mut line = format!(
        "[obelisk-memory] {label}: total pss {:.1} MiB; supervisor pss {:.1} MiB",
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

/// Reads `smaps_rollup` and readable `fdinfo` under `<proc_root>/<who>` (`who` is a pid or
/// `"self"`). A missing/malformed rollup errors; no DRM fds is a zeroed [`Gpu`].
fn read_process_memory(proc_root: &Path, who: &str) -> io::Result<ProcessMemory> {
    let process_dir = proc_root.join(who);
    let rollup_path = process_dir.join("smaps_rollup");
    let rollup_text = std::fs::read_to_string(&rollup_path)?;
    let rollup = parse_rollup(&rollup_text).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{} has no Pss: line", rollup_path.display()))
    })?;
    Ok(ProcessMemory { rollup, gpu: read_gpu(&process_dir) })
}

/// Sums DRM residency across `<process_dir>/fdinfo`. A missing directory or fd vanishing between
/// `read_dir` and `read` (normal, as fds close constantly) means no DRM memory, not an error.
fn read_gpu(process_dir: &Path) -> Gpu {
    let Ok(entries) = std::fs::read_dir(process_dir.join("fdinfo")) else {
        return fold_drm_clients(std::iter::empty());
    };
    let clients = entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok().and_then(|text| parse_drm_client(&text)));
    fold_drm_clients(clients)
}

/// Reads `<proc_root>/self`, then renderers in `renderer_pids` (`(generation_id, pid)` order). A
/// pid already exited during an ordinary generation swap handoff or crash is logged and skipped;
/// only the supervisor read is fatal.
pub(crate) fn sample(proc_root: &Path, renderer_pids: &[(u32, u32)]) -> io::Result<Sample> {
    let supervisor = read_process_memory(proc_root, "self")?;
    let mut renderers = Vec::with_capacity(renderer_pids.len());
    for &(generation_id, pid) in renderer_pids {
        match read_process_memory(proc_root, &pid.to_string()) {
            Ok(memory) => renderers.push((generation_id, memory)),
            Err(err) => eprintln!(
                "[obelisk-memory] generation {generation_id} (pid {pid}) could not be sampled, skipping: {err}"
            ),
        }
    }
    Ok(Sample { supervisor, renderers })
}

/// Builds the `--profile` steady-state sampler (ADR-0043 amendment), or `None`. `smaps_rollup` is
/// always externally readable, so an always-on timer adds only a log line; the handoff sample is
/// unconditional because no outside observer can catch a swap-only window. First tick is one
/// period out because a new Renderer is not steady; `Skip` keeps a late sampler current.
pub(crate) fn sampler_from_env() -> Option<tokio::time::Interval> {
    let period = shared::profile_interval()?;
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    Some(interval)
}

/// Reads and logs one sample across Supervisor and `renderers` (ADR-0043 decision 1). `main.rs`
/// calls it from the steady-state timer (one authoritative generation) and the generation swap
/// (both during the two-generation window). A reaped `Child` with `id() == None` is dropped, not
/// reported as zero.
pub(crate) fn log_sample(label: &str, renderers: &[(u32, &tokio::process::Child)]) {
    let pids: Vec<(u32, u32)> =
        renderers.iter().filter_map(|(generation_id, child)| child.id().map(|pid| (*generation_id, pid))).collect();
    match sample(Path::new(PROC_ROOT), &pids) {
        Ok(sample) => eprintln!("{}", report_line(label, &sample)),
        Err(err) => eprintln!("[obelisk-memory] {label} sample failed: {err}"),
    }
}

/// `select!` arm for the steady-state sampler. Off means never resolve: `Option<Interval>` cannot
/// be ticked directly, and resolving would spin the loop. `Interval::tick` and `pending` are both
/// cancel-safe as `select!` requires.
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

    /// Real machine output: Pss 208 kB, Private_Clean 48 kB, Private_Dirty 108 kB, hence
    /// pss 208 and uss 156.
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
        // Prefix matching "Pss" would find `Pss_Dirty: 108 kB` and report 108 instead of `None`.
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

    /// Real i915 output. `drm-resident-system0` plus `drm-resident-stolen-system0` is 279968, equal
    /// to `drm-total-system0` only because the buffer is fully resident, not because `drm-total-*`
    /// was read. See the total-is-not-summed test.
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
        // If both naming styles appear, resident wins instead of adding the fallback.
        let text = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t4\ndrm-resident-system0:\t1000 KiB\ndrm-memory-system0:\t9999 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 1000, shared: 0 })
        );
    }

    #[test]
    fn parse_drm_client_treats_a_bare_zero_resident_value_as_present_not_missing() {
        // If bare `0` failed to parse, this would fall back to `drm-memory-system0`'s 99999.
        let text = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t4\ndrm-resident-system0:\t0\ndrm-memory-system0:\t99999 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 0, shared: 0 })
        );
    }

    /// Select fallback only when the modern key is absent, not when its value fails to parse; a
    /// stale legacy number is wrong, not absent.
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
        // Reading `50 MiB` as 50 KiB under-reports by 1024x; drop it instead.
        let text = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t4\ndrm-resident-system0:\t50 MiB\ndrm-resident-other:\t1000 KiB\n";
        assert_eq!(
            parse_drm_client(text),
            Some(DrmClient { pdev: "0000:00:02.0".to_string(), client_id: 4, resident: 1000, shared: 0 })
        );
    }

    // ---- read_process_memory / read_gpu, against a fake proc root ----

    /// Writes `<root>/<who>/smaps_rollup`, plus one `fdinfo/<fd>` file per entry in `fds`. An
    /// empty `fds` writes no `fdinfo` directory at all, which is the process that holds no fds.
    fn write_process(root: &Path, who: &str, rollup: Option<&str>, fds: &[(&str, &str)]) {
        let dir = root.join(who);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(rollup) = rollup {
            std::fs::write(dir.join("smaps_rollup"), rollup).unwrap();
        }
        if !fds.is_empty() {
            std::fs::create_dir_all(dir.join("fdinfo")).unwrap();
            for (fd, text) in fds {
                std::fs::write(dir.join("fdinfo").join(fd), text).unwrap();
            }
        }
    }

    #[test]
    fn read_process_memory_reads_the_rollup_and_reports_no_gpu_when_the_process_has_no_fdinfo() {
        let root = tempfile::tempdir().unwrap();
        write_process(root.path(), "self", Some(ROLLUP_FIXTURE), &[]);

        let memory = read_process_memory(root.path(), "self").expect("a well-formed rollup must be read");

        assert_eq!(memory.rollup, Rollup { pss: 208, uss: 156 });
        assert_eq!(memory.gpu, Gpu { resident: 0, shared: 0, clients: 0 }, "no fdinfo is no GPU memory, not an error");
    }

    #[test]
    fn read_process_memory_errors_when_the_process_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let err = read_process_memory(root.path(), "4242").expect_err("a pid that has exited must not read as zero");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn read_process_memory_errors_on_a_truncated_rollup_rather_than_reporting_a_zero_byte_process() {
        let root = tempfile::tempdir().unwrap();
        write_process(root.path(), "self", Some("Rss:                3932 kB\n"), &[]);

        let err = read_process_memory(root.path(), "self").expect_err("a rollup without Pss: must be an error");

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("smaps_rollup"), "the message must name the file that failed: {err}");
    }

    #[test]
    fn read_gpu_dedupes_the_several_fds_one_drm_client_holds_and_skips_the_fds_that_are_not_drm() {
        // Three fds of one client plus a pipe, which is what most of `fdinfo` actually is.
        let root = tempfile::tempdir().unwrap();
        write_process(
            root.path(),
            "self",
            Some(ROLLUP_FIXTURE),
            &[
                ("0", "pos:\t0\nflags:\t02\nmnt_id:\t9\nino:\t123\n"),
                ("1", DRM_FIXTURE),
                ("2", DRM_FIXTURE),
                ("3", DRM_FIXTURE),
            ],
        );

        let memory = read_process_memory(root.path(), "self").expect("a well-formed rollup must be read");

        assert_eq!(memory.gpu, Gpu { resident: 279968, shared: 192368, clients: 1 });
    }

    // ---- fold_drm_clients ----

    #[test]
    fn fold_drm_clients_dedupes_by_pdev_and_client_id_so_one_process_is_not_triple_counted() {
        // Three live fds from one process each report `drm-client-id: 4` on the same pdev with
        // identical byte counts.
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
            "[obelisk-memory] periodic: total pss 68.0 MiB; supervisor pss 8.0 MiB; \
             generation 0 pss 50.0 MiB uss 40.0 MiB gpu 20.0 MiB (3.0 MiB shared, 1 drm client(s)); \
             generation 1 pss 10.0 MiB uss 5.0 MiB gpu 1.0 MiB (0.5 MiB shared, 1 drm client(s))"
        );
    }

    #[test]
    fn report_line_total_pss_excludes_gpu_and_uss() {
        // ADR-0043 decision 1: total PSS is a PSS sum, not PSS+USS; USS is per renderer only.
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
        assert!(report_line("check", &sample).starts_with("[obelisk-memory] check: total pss 2.0 MiB;"));
    }
}
