//! `ram_percent`/`swap_percent` come from `/proc/meminfo` (ADR-0035).

/// The four `/proc/meminfo` fields needed for RAM/swap percentages. Use `mem_available` as-is,
/// the kernel's considered-free estimate, rather than rebuilding it from `Buffers`/`Cached`
/// (ADR-0035).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub mem_total: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

/// Parses `/proc/meminfo`'s `key:   value kB` lines. `None` if `MemTotal` or `MemAvailable` is
/// missing; absent `SwapTotal`/`SwapFree` default to `0`.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut values = std::collections::HashMap::new();
    for line in text.lines() {
        // Skip blank or colon-less lines; `str::lines()` yields an empty line for blanks.
        let Some((key, rest)) = line.split_once(':') else { continue };
        if let Some(value) = rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()) {
            values.insert(key, value);
        }
    }
    Some(MemInfo {
        mem_total: *values.get("MemTotal")?,
        mem_available: *values.get("MemAvailable")?,
        swap_total: values.get("SwapTotal").copied().unwrap_or(0),
        swap_free: values.get("SwapFree").copied().unwrap_or(0),
    })
}

/// RAM/swap percentage is `100 * used / total`; swap is `0` when `swap_total == 0`.
pub fn compute_percentages(info: &MemInfo) -> (u8, u8) {
    let ram_used = info.mem_total.saturating_sub(info.mem_available);
    let ram_percent = (100 * ram_used).checked_div(info.mem_total).unwrap_or(0) as u8;

    let swap_used = info.swap_total.saturating_sub(info.swap_free);
    let swap_percent = (100 * swap_used).checked_div(info.swap_total).unwrap_or(0) as u8;

    (ram_percent, swap_percent)
}

/// Reads `{proc_root}/meminfo`; injected `proc_root` lets tests use a tempdir.
pub fn read_meminfo(proc_root: &std::path::Path) -> std::io::Result<MemInfo> {
    let content = std::fs::read_to_string(proc_root.join("meminfo"))?;
    parse_meminfo(&content)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed /proc/meminfo"))
}

#[cfg(test)]
mod tests {
    /// Real `/proc/meminfo` capture from this machine, trimmed to used fields plus unrelated ones
    /// to prove they are ignored.
    fn real_meminfo() -> &'static str {
        "MemTotal:       32479404 kB\n\
         MemFree:         1114104 kB\n\
         MemAvailable:   10607160 kB\n\
         Buffers:          606856 kB\n\
         Cached:         11776708 kB\n\
         SwapCached:        56880 kB\n\
         SwapTotal:      16239612 kB\n\
         SwapFree:         284180 kB\n\
         Shmem:           1703336 kB\n\
         SReclaimable:     594852 kB\n"
    }

    #[test]
    fn parse_meminfo_reads_the_four_fields_this_module_needs() {
        let info = super::parse_meminfo(real_meminfo()).expect("a real /proc/meminfo capture must parse");
        assert_eq!(info.mem_total, 32479404);
        assert_eq!(info.mem_available, 10607160);
        assert_eq!(info.swap_total, 16239612);
        assert_eq!(info.swap_free, 284180);
    }

    #[test]
    fn parse_meminfo_rejects_text_missing_mem_available() {
        assert_eq!(super::parse_meminfo("MemTotal: 1000 kB\n"), None);
    }

    #[test]
    fn parse_meminfo_skips_a_blank_or_colon_less_line_instead_of_aborting_the_whole_parse() {
        // A blank line must not discard fields seen around it.
        let info = super::parse_meminfo("MemTotal: 1000 kB\n\nMemAvailable: 400 kB\n")
            .expect("a stray blank line must not abort the whole parse");
        assert_eq!(info.mem_total, 1000);
        assert_eq!(info.mem_available, 400);
    }

    #[test]
    fn parse_meminfo_defaults_swap_fields_to_zero_when_absent() {
        let info = super::parse_meminfo("MemTotal: 1000 kB\nMemAvailable: 400 kB\n")
            .expect("should still parse without swap fields");
        assert_eq!(info.swap_total, 0);
        assert_eq!(info.swap_free, 0);
    }

    #[test]
    fn compute_percentages_derives_ram_and_swap_usage_from_the_real_capture() {
        let info = super::parse_meminfo(real_meminfo()).unwrap();
        let (ram_percent, swap_percent) = super::compute_percentages(&info);
        // used = 21872244; 100*used/32479404 = 67.34...% -> 67
        assert_eq!(ram_percent, 67);
        // used = 15955432; 100*used/16239612 = 98.24...% -> 98
        assert_eq!(swap_percent, 98);
    }

    #[test]
    fn compute_percentages_reports_zero_swap_when_no_swap_is_configured() {
        let info = super::MemInfo { mem_total: 1000, mem_available: 500, swap_total: 0, swap_free: 0 };
        let (_, swap_percent) = super::compute_percentages(&info);
        assert_eq!(swap_percent, 0);
    }

    #[test]
    fn read_meminfo_reads_from_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("meminfo"), real_meminfo()).unwrap();
        let info = super::read_meminfo(dir.path()).expect("a well-formed meminfo file must parse");
        assert_eq!(info.mem_total, 32479404);
    }

    #[test]
    fn read_meminfo_fails_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::read_meminfo(dir.path()).is_err());
    }
}
