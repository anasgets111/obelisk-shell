//! `temp_cores`/`temp_gpu` source `/sys/class/hwmon/`, resolving chips by name preference
//! (ADR-0035).

use std::path::{Path, PathBuf};

/// Converts milli-Celsius to whole Celsius, rounding half away from zero. Plain division makes
/// `-500 / 1000 == 0`, silently reporting `0°C` instead of `-1°C`.
fn round_milli_c(milli_c: i64) -> i64 {
    if milli_c >= 0 { (milli_c + 500) / 1000 } else { (milli_c - 500) / 1000 }
}

/// Resolves the first `hwmon_root` chip whose `name` matches `preference`, trying that list in
/// order (ADR-0035); preference order beats directory order.
pub fn resolve_chip(hwmon_root: &Path, preference: &[&str]) -> Option<PathBuf> {
    let entries: Vec<PathBuf> =
        std::fs::read_dir(hwmon_root).ok()?.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    for wanted in preference {
        for dir in &entries {
            if let Ok(name) = std::fs::read_to_string(dir.join("name"))
                && name.trim() == *wanted
            {
                return Some(dir.clone());
            }
        }
    }
    None
}

/// Reads `tempN_input` sensors whose paired `tempN_label` matches `Core \d+`, converting to Celsius
/// and sorting by core index. Excludes package aggregates and unlabeled sensors.
pub fn read_cores(chip_dir: &Path) -> Vec<i64> {
    let core_label = regex::Regex::new(r"^Core (\d+)$").expect("static regex must compile");
    let Ok(entries) = std::fs::read_dir(chip_dir) else {
        return Vec::new();
    };

    let mut cores: Vec<(u32, i64)> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else { continue };
        let Some(rest) = file_name.strip_suffix("_input") else { continue };
        let label_path = chip_dir.join(format!("{rest}_label"));
        let Ok(label) = std::fs::read_to_string(&label_path) else { continue };
        let Some(captures) = core_label.captures(label.trim()) else { continue };
        let Ok(core_index) = captures[1].parse::<u32>() else { continue };
        let Ok(value) = std::fs::read_to_string(entry.path()) else { continue };
        let Ok(milli_c) = value.trim().parse::<i64>() else { continue };
        cores.push((core_index, milli_c));
    }
    cores.sort_by_key(|(core_index, _)| *core_index);
    cores.into_iter().map(|(_, milli_c)| round_milli_c(milli_c)).collect()
}

/// Reads the lowest-numbered `tempN_input`, regardless of label, for `acpitz` fallback and
/// `temp_gpu`. `None` when no such file exists.
fn read_primary_sensor(chip_dir: &Path) -> Option<i64> {
    let entries = std::fs::read_dir(chip_dir).ok()?;
    let lowest = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name().to_str().and_then(|n| n.strip_suffix("_input")?.strip_prefix("temp")?.parse::<u32>().ok())
        })
        .min()?;
    let value = std::fs::read_to_string(chip_dir.join(format!("temp{lowest}_input"))).ok()?;
    value.trim().parse::<i64>().ok().map(round_milli_c)
}

/// Controller-construction preference for `temp_cores` (ADR-0035).
const CPU_TEMP_PREFERENCE: &[&str] = &["k10temp", "coretemp"];
/// Generic ACPI thermal-zone fallback when neither `k10temp` nor `coretemp` exists; every machine
/// has this, and its one sensor becomes a one-element array.
const GENERIC_TEMP_FALLBACK: &str = "acpitz";
/// Controller-construction preference for `temp_gpu` (ADR-0035).
const GPU_TEMP_PREFERENCE: &[&str] = &["amdgpu", "nouveau", "nvidia"];

/// Resolved `temp_cores` source (ADR-0035): chip resolution happens at construction, never per
/// tick (see [`resolve_temp_cores_source`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreTempSource {
    /// CPU chip (`k10temp`/`coretemp`) resolved; read every per-core sensor.
    PerCore(PathBuf),
    /// No CPU chip; generic `acpitz` fallback resolved to one sensor.
    Single(PathBuf),
    /// Neither resolved; `temp_cores` stays empty.
    Unavailable,
}

/// Resolves `temp_cores`: CPU preference, then `acpitz`, then unavailable (ADR-0035). Call once at
/// construction; onboard chips do not hotplug, so per-tick scans waste work.
pub fn resolve_temp_cores_source(hwmon_root: &Path) -> CoreTempSource {
    if let Some(chip_dir) = resolve_chip(hwmon_root, CPU_TEMP_PREFERENCE) {
        return CoreTempSource::PerCore(chip_dir);
    }
    match resolve_chip(hwmon_root, &[GENERIC_TEMP_FALLBACK]) {
        Some(chip_dir) => CoreTempSource::Single(chip_dir),
        None => CoreTempSource::Unavailable,
    }
}

/// Reads `temp_cores` from an already-resolved [`CoreTempSource`] (ADR-0035); no directory scan.
pub fn read_temp_cores_from(source: &CoreTempSource) -> Vec<i64> {
    match source {
        CoreTempSource::PerCore(chip_dir) => read_cores(chip_dir),
        CoreTempSource::Single(chip_dir) => read_primary_sensor(chip_dir).into_iter().collect(),
        CoreTempSource::Unavailable => Vec::new(),
    }
}

/// Resolves `temp_gpu`'s chip once at construction (ADR-0035); chips do not hotplug.
pub fn resolve_gpu_chip(hwmon_root: &Path) -> Option<PathBuf> {
    resolve_chip(hwmon_root, GPU_TEMP_PREFERENCE)
}

/// Reads `temp_gpu` from an already-resolved chip, or IDL sentinel `-1` when none resolved
/// (ADR-0035); no per-tick resolution.
pub fn read_temp_gpu_from(gpu_chip: Option<&Path>) -> i64 {
    gpu_chip.and_then(read_primary_sensor).unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only resolve-and-read convenience; production resolves once and reads per tick.
    fn resolve_and_read_temp_cores(hwmon_root: &Path) -> Vec<i64> {
        read_temp_cores_from(&resolve_temp_cores_source(hwmon_root))
    }

    /// Test-only resolve-and-read convenience for `temp_gpu`.
    fn resolve_and_read_temp_gpu(hwmon_root: &Path) -> i64 {
        read_temp_gpu_from(resolve_gpu_chip(hwmon_root).as_deref())
    }

    /// Builds fake `hwmon_root` chips under `dir`, each with only a `name` file, for
    /// [`resolve_chip`] tests.
    fn write_chip(dir: &Path, dir_name: &str, chip_name: &str) {
        let chip_dir = dir.join(dir_name);
        std::fs::create_dir_all(&chip_dir).unwrap();
        std::fs::write(chip_dir.join("name"), format!("{chip_name}\n")).unwrap();
    }

    /// Writes one `tempN_input`/`tempN_label` pair in milli-Celsius, the real hwmon convention;
    /// values came from this machine's `coretemp`.
    fn write_sensor(chip_dir: &Path, n: u32, label: &str, milli_c: i64) {
        std::fs::write(chip_dir.join(format!("temp{n}_input")), format!("{milli_c}\n")).unwrap();
        std::fs::write(chip_dir.join(format!("temp{n}_label")), format!("{label}\n")).unwrap();
    }

    #[test]
    fn read_cores_extracts_and_sorts_by_core_index_not_filename_or_insertion_order() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon6");
        std::fs::create_dir_all(&chip_dir).unwrap();
        // Captured values are deliberately misordered: temp30/Core 28 and temp6/Core 4 prove
        // sorting uses core index, not filename.
        write_sensor(&chip_dir, 1, "Package id 0", 92000); // excluded: not a per-core sensor
        write_sensor(&chip_dir, 30, "Core 28", 65000);
        write_sensor(&chip_dir, 2, "Core 0", 57000);
        write_sensor(&chip_dir, 6, "Core 4", 78000);

        assert_eq!(read_cores(&chip_dir), vec![57, 78, 65]);
    }

    #[test]
    fn read_cores_is_empty_for_a_chip_with_no_core_labeled_sensors() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon1");
        std::fs::create_dir_all(&chip_dir).unwrap();
        write_sensor(&chip_dir, 1, "", 92000); // acpitz: unlabeled, single zone
        assert_eq!(read_cores(&chip_dir), Vec::<i64>::new());
    }

    #[test]
    fn read_primary_sensor_reads_the_lowest_numbered_input_regardless_of_label() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon1");
        std::fs::create_dir_all(&chip_dir).unwrap();
        // acpitz-shaped: one unlabeled captured sensor.
        write_sensor(&chip_dir, 1, "", 92000);
        assert_eq!(read_primary_sensor(&chip_dir), Some(92));
    }

    #[test]
    fn read_primary_sensor_picks_the_lowest_input_number_when_several_exist() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon3");
        std::fs::create_dir_all(&chip_dir).unwrap();
        // nvme-shaped captured values, written out of numeric order.
        write_sensor(&chip_dir, 3, "Sensor 2", 39850);
        write_sensor(&chip_dir, 1, "Composite", 35850);
        write_sensor(&chip_dir, 2, "Sensor 1", 35850);
        // Lowest input is temp1 (Composite), 35850 milli-C = 35.85°C -> 36.
        assert_eq!(read_primary_sensor(&chip_dir), Some(36));
    }

    #[test]
    fn round_milli_c_rounds_to_the_nearest_whole_degree_including_negative_readings() {
        assert_eq!(round_milli_c(57000), 57);
        assert_eq!(round_milli_c(35850), 36); // .85 rounds up
        assert_eq!(round_milli_c(35499), 35); // .499 rounds down
        assert_eq!(round_milli_c(0), 0);
        assert_eq!(round_milli_c(-500), -1); // -0.5degC rounds away from zero, not toward 0degC
        assert_eq!(round_milli_c(-499), 0);
    }

    #[test]
    fn read_primary_sensor_is_none_for_a_chip_directory_with_no_sensors() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon4");
        std::fs::create_dir_all(&chip_dir).unwrap();
        assert_eq!(read_primary_sensor(&chip_dir), None);
    }

    #[test]
    fn read_temp_cores_prefers_coretemp_over_acpitz() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon1", "acpitz");
        write_sensor(&dir.path().join("hwmon1"), 1, "", 92000);
        write_chip(dir.path(), "hwmon6", "coretemp");
        write_sensor(&dir.path().join("hwmon6"), 1, "Package id 0", 92000);
        write_sensor(&dir.path().join("hwmon6"), 2, "Core 0", 57000);

        assert_eq!(resolve_and_read_temp_cores(dir.path()), vec![57]);
    }

    #[test]
    fn read_temp_cores_falls_back_to_acpitz_as_a_one_element_array_when_no_cpu_chip_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon1", "acpitz");
        write_sensor(&dir.path().join("hwmon1"), 1, "", 92000);

        assert_eq!(resolve_and_read_temp_cores(dir.path()), vec![92]);
    }

    #[test]
    fn read_temp_cores_is_empty_when_neither_cpu_chip_nor_acpitz_exist() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon7", "mt7921_phy0");
        assert_eq!(resolve_and_read_temp_cores(dir.path()), Vec::<i64>::new());
    }

    #[test]
    fn read_temp_gpu_reads_the_matching_chips_primary_sensor() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon2", "amdgpu");
        write_sensor(&dir.path().join("hwmon2"), 1, "edge", 45000);
        assert_eq!(resolve_and_read_temp_gpu(dir.path()), 45);
    }

    #[test]
    fn read_temp_gpu_is_the_idl_sentinel_when_no_gpu_chip_is_present() {
        // This machine has integrated graphics only, with no amdgpu/nouveau/nvidia chip.
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon6", "coretemp");
        assert_eq!(resolve_and_read_temp_gpu(dir.path()), -1);
    }

    #[test]
    fn resolve_temp_cores_source_picks_per_core_over_the_acpitz_fallback() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon1", "acpitz");
        write_chip(dir.path(), "hwmon6", "coretemp");

        assert_eq!(resolve_temp_cores_source(dir.path()), CoreTempSource::PerCore(dir.path().join("hwmon6")));
    }

    #[test]
    fn resolve_temp_cores_source_falls_back_to_single_when_no_cpu_chip_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon1", "acpitz");

        assert_eq!(resolve_temp_cores_source(dir.path()), CoreTempSource::Single(dir.path().join("hwmon1")));
    }

    #[test]
    fn resolve_temp_cores_source_is_unavailable_when_nothing_matches() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon7", "mt7921_phy0");

        assert_eq!(resolve_temp_cores_source(dir.path()), CoreTempSource::Unavailable);
    }

    #[test]
    fn resolve_gpu_chip_finds_a_matching_chip() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon2", "amdgpu");
        write_chip(dir.path(), "hwmon6", "coretemp");

        assert_eq!(resolve_gpu_chip(dir.path()), Some(dir.path().join("hwmon2")));
    }

    #[test]
    fn resolve_gpu_chip_returns_none_when_no_gpu_chip_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon6", "coretemp");

        assert_eq!(resolve_gpu_chip(dir.path()), None);
    }

    #[test]
    fn read_temp_cores_from_reads_an_already_resolved_source() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon6");
        std::fs::create_dir_all(&chip_dir).unwrap();
        write_sensor(&chip_dir, 2, "Core 0", 57000);

        assert_eq!(read_temp_cores_from(&CoreTempSource::PerCore(chip_dir)), vec![57]);
        assert_eq!(read_temp_cores_from(&CoreTempSource::Unavailable), Vec::<i64>::new());
    }

    #[test]
    fn read_temp_gpu_from_reads_an_already_resolved_chip_or_the_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon2");
        std::fs::create_dir_all(&chip_dir).unwrap();
        write_sensor(&chip_dir, 1, "edge", 45000);

        assert_eq!(read_temp_gpu_from(Some(&chip_dir)), 45);
        assert_eq!(read_temp_gpu_from(None), -1);
    }

    #[test]
    fn resolve_chip_finds_the_only_chip_matching_the_preference_list() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon0", "acpitz");
        write_chip(dir.path(), "hwmon5", "coretemp");

        let resolved = resolve_chip(dir.path(), &["k10temp", "coretemp"]).expect("coretemp should resolve");
        assert_eq!(resolved, dir.path().join("hwmon5"));
    }

    #[test]
    fn resolve_chip_prefers_earlier_preference_entries_over_directory_order() {
        let dir = tempfile::tempdir().unwrap();
        // hwmon0 is lexically earlier but lower preference; preference order must win.
        write_chip(dir.path(), "hwmon0", "coretemp");
        write_chip(dir.path(), "hwmon1", "k10temp");

        let resolved = resolve_chip(dir.path(), &["k10temp", "coretemp"]).expect("k10temp should win");
        assert_eq!(resolved, dir.path().join("hwmon1"));
    }

    #[test]
    fn resolve_chip_returns_none_when_nothing_in_the_preference_list_is_present() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon0", "acpitz");
        write_chip(dir.path(), "hwmon7", "mt7921_phy0");

        assert_eq!(resolve_chip(dir.path(), &["amdgpu", "nouveau", "nvidia"]), None);
    }
}
