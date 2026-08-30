//! Lock-state (caps/num/scroll) half of `oblisk.keyboard` (ADR-0034): sysfs LED `brightness`
//! files do NOT fire inotify `MODIFY` events on this kernel when `input_leds` changes them
//! itself (confirmed live: toggling Caps Lock under `inotifywait -m` shows zero events). evdev's
//! `EV_LED` stream is therefore primary; sysfs is a permission-independent, read-once fallback.

use std::io;
use std::path::{Path, PathBuf};

/// Resolved sysfs LED node paths for all three lock indicators. All-or-nothing: if any one is
/// missing, [`resolve_lock_leds`] returns `None` for the whole triple -- real hardware exposes all three as siblings, or none at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockLeds {
    pub caps: PathBuf,
    pub num: PathBuf,
    pub scroll: PathBuf,
}

/// Scans `leds_root` for a subdirectory whose name ends with `::<suffix>` -- kernel LED-class
/// naming is `<device>::<function>`, so matching only the suffix is robust to any `<device>` prefix.
fn find_led(leds_root: &Path, suffix: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(leds_root).ok()?;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().ends_with(suffix) {
            return Some(entry.path());
        }
    }
    None
}

pub fn resolve_lock_leds(leds_root: &Path) -> Option<LockLeds> {
    Some(LockLeds { caps: find_led(leds_root, "capslock")?, num: find_led(leds_root, "numlock")?, scroll: find_led(leds_root, "scrolllock")? })
}

/// Reads one LED's `brightness` file. Kernel LED-class brightness is `0` = off, nonzero = on
/// -- `!= 0` is correct, not `== 1`, since `max_brightness` isn't guaranteed to be `1`.
pub fn read_led_on(led_dir: &Path) -> io::Result<bool> {
    let text = std::fs::read_to_string(led_dir.join("brightness"))?;
    Ok(text.trim().parse::<i64>().unwrap_or(0) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_led(dir: &Path, name: &str, brightness: &str) -> PathBuf {
        let led_dir = dir.join(name);
        std::fs::create_dir(&led_dir).unwrap();
        std::fs::write(led_dir.join("brightness"), brightness).unwrap();
        led_dir
    }

    // ---- find_led / resolve_lock_leds ----

    #[test]
    fn resolve_lock_leds_finds_all_three_by_name_suffix() {
        let root = tempfile::tempdir().unwrap();
        let caps = write_led(root.path(), "input3::capslock", "0");
        let num = write_led(root.path(), "input3::numlock", "1");
        let scroll = write_led(root.path(), "input3::scrolllock", "0");

        let leds = resolve_lock_leds(root.path()).expect("all three present");
        assert_eq!(leds, LockLeds { caps, num, scroll });
    }

    #[test]
    fn resolve_lock_leds_is_none_when_any_one_led_is_missing() {
        let root = tempfile::tempdir().unwrap();
        write_led(root.path(), "input3::capslock", "0");
        write_led(root.path(), "input3::numlock", "1");
        // scrolllock deliberately absent.

        assert_eq!(resolve_lock_leds(root.path()), None);
    }

    #[test]
    fn resolve_lock_leds_is_none_against_an_empty_or_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(resolve_lock_leds(root.path()), None);
        assert_eq!(resolve_lock_leds(&root.path().join("does-not-exist")), None);
    }

    // ---- read_led_on ----

    #[test]
    fn read_led_on_is_true_for_nonzero_brightness() {
        let root = tempfile::tempdir().unwrap();
        let led = write_led(root.path(), "input3::capslock", "1");
        assert!(read_led_on(&led).unwrap());
    }

    #[test]
    fn read_led_on_is_false_for_zero_brightness() {
        let root = tempfile::tempdir().unwrap();
        let led = write_led(root.path(), "input3::numlock", "0");
        assert!(!read_led_on(&led).unwrap());
    }

    #[test]
    fn read_led_on_treats_a_malformed_brightness_value_as_off_rather_than_erroring() {
        let root = tempfile::tempdir().unwrap();
        let led = write_led(root.path(), "input3::scrolllock", "not-a-number\n");
        assert!(!read_led_on(&led).unwrap());
    }

    #[test]
    fn read_led_on_errors_when_the_brightness_file_does_not_exist() {
        let root = tempfile::tempdir().unwrap();
        assert!(read_led_on(root.path()).is_err());
    }
}
