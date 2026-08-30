//! `cpu_percent` sourcing: `/proc/stat`'s aggregate `cpu` line (docs/adr/0035).

/// One `/proc/stat` aggregate-line sample: enough to compute a busy percentage against a
/// later sample, not every individual field (docs/adr/0035's `busy = total - (idle+iowait)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSample {
    /// Sum of every field on the line (`user+nice+system+idle+iowait+irq+softirq+steal+guest+guest_nice`).
    pub total: u64,
    /// `idle + iowait` -- the two fields that count as "not busy" (docs/adr/0035).
    pub idle_total: u64,
}

/// Parses `/proc/stat`'s aggregate `cpu` line (the first line; `cpuN` per-core lines are
/// rejected). Tolerant of fewer than the full 10 fields (older kernels), as long as at least
/// `user nice system idle` (4) are present, matching `/proc/stat`'s documented minimum.
pub fn parse_stat_line(line: &str) -> Option<CpuSample> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let values: Vec<u64> = fields.map(|f| f.parse::<u64>().ok()).collect::<Option<Vec<u64>>>()?;
    if values.len() < 4 {
        return None;
    }
    let total = values.iter().sum();
    let idle_total = values[3] + values.get(4).copied().unwrap_or(0);
    Some(CpuSample { total, idle_total })
}

/// The busy-percentage delta between two samples (docs/adr/0035: `busy = total -
/// (idle+iowait)`, `percent = 100 * busy_delta / total_delta`). `0` if no time elapsed
/// (`total_delta == 0`) rather than dividing by zero.
pub fn delta_percent(prev: &CpuSample, current: &CpuSample) -> u8 {
    let total_delta = current.total.saturating_sub(prev.total);
    if total_delta == 0 {
        return 0;
    }
    let idle_delta = current.idle_total.saturating_sub(prev.idle_total);
    let busy_delta = total_delta.saturating_sub(idle_delta);
    ((100 * busy_delta) / total_delta) as u8
}

/// Reads and parses `{proc_root}/stat`'s aggregate `cpu` line. `proc_root` is a parameter,
/// never hardcoded `/proc` (docs/oblisk-tdd-test-harness.md's mandate).
pub fn read_sample(proc_root: &std::path::Path) -> std::io::Result<CpuSample> {
    let content = std::fs::read_to_string(proc_root.join("stat"))?;
    let line = content.lines().next().unwrap_or("");
    parse_stat_line(line).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed /proc/stat aggregate cpu line"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_stat_line_parses_the_real_aggregate_cpu_line() {
        let line = "cpu  37016160 481710 3444448 618688042 1432704 2023206 1140343 0 0 0";
        let sample = super::parse_stat_line(line).expect("real /proc/stat aggregate line must parse");
        assert_eq!(sample.total, 37016160 + 481710 + 3444448 + 618688042 + 1432704 + 2023206 + 1140343);
        assert_eq!(sample.idle_total, 618688042 + 1432704);
    }

    #[test]
    fn parse_stat_line_rejects_a_per_core_line() {
        assert_eq!(super::parse_stat_line("cpu0 764261 30925 178799 31420264 97776 155846 457625 0 0 0"), None);
    }

    #[test]
    fn delta_percent_computes_the_busy_fraction_between_two_samples() {
        let prev = super::CpuSample { total: 1000, idle_total: 800 };
        let current = super::CpuSample { total: 2000, idle_total: 1000 };
        // total_delta=1000, idle_delta=200, busy_delta=800 -> 80% busy.
        assert_eq!(super::delta_percent(&prev, &current), 80);
    }

    #[test]
    fn delta_percent_is_zero_when_no_time_has_elapsed_between_samples() {
        let sample = super::CpuSample { total: 1000, idle_total: 800 };
        assert_eq!(super::delta_percent(&sample, &sample), 0);
    }

    #[test]
    fn read_sample_reads_the_aggregate_line_from_a_real_stat_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stat"),
            "cpu  37016160 481710 3444448 618688042 1432704 2023206 1140343 0 0 0\n\
             cpu0 764261 30925 178799 31420264 97776 155846 457625 0 0 0\n",
        )
        .unwrap();

        let sample = super::read_sample(dir.path()).expect("a well-formed stat file must parse");
        assert_eq!(sample.total, 37016160 + 481710 + 3444448 + 618688042 + 1432704 + 2023206 + 1140343);
        assert_eq!(sample.idle_total, 618688042 + 1432704);
    }

    #[test]
    fn read_sample_fails_when_stat_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::read_sample(dir.path()).is_err());
    }
}
