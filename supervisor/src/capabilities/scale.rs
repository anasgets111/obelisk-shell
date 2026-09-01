//! Raw-brightness/percent conversion shared by every `hardware` capability that scales a
//! `[0, max]` device reading against a `[0, 100]` percent: `keyboard`'s UPower backlight
//! (ADR-0034) and `brightness`'s sysfs backlight (ADR-0053) both call these.

/// Converts a raw `[0, max]` brightness reading into a `[0, 100]` percent, round-half-away-
/// from-zero. `max <= 0` returns the IDL's `-1` unavailable sentinel rather than dividing by
/// zero or fabricating a `0`; `brightness` never sees it since it excludes non-positive
/// `max_brightness` devices at selection time.
pub fn percent_from_raw(brightness: i32, max: i32) -> i32 {
    if max <= 0 {
        return -1;
    }
    // i64 intermediates: `100 * brightness` in `i32` overflows once `max`/`brightness` exceeds
    // `i32::MAX / 100` -- must not panic (debug) or silently wrap (release).
    let brightness = i64::from(brightness.clamp(0, max));
    let max64 = i64::from(max);
    let scaled = 100 * brightness;
    let half = max64 / 2;
    (((scaled + half) / max64) as i32).clamp(0, 100)
}

/// The inverse of [`percent_from_raw`]: converts a `[0, 100]` percent into the raw `[0, max]`
/// scale a `SetBrightness` call expects, round-half-away-from-zero, clamped to `[0, max]`. `pct`
/// is unvalidated `u64` from the caller's `parse_*_args`; clamping happens here.
pub fn raw_from_percent(pct: u64, max: i32) -> i32 {
    if max <= 0 {
        return 0;
    }
    // i64 intermediates, same overflow reasoning as `percent_from_raw`.
    let pct = pct.min(100) as i64;
    let max64 = i64::from(max);
    let scaled = pct * max64;
    (((scaled + 50) / 100) as i32).clamp(0, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- percent_from_raw ----

    #[test]
    fn percent_from_raw_scales_zero_to_max_across_zero_to_one_hundred() {
        assert_eq!(percent_from_raw(0, 3), 0);
        assert_eq!(percent_from_raw(3, 3), 100);
    }

    #[test]
    fn percent_from_raw_rounds_half_away_from_zero_on_a_coarse_scale() {
        // 1/3 * 100 = 33.33 -> 33; 2/3 * 100 = 66.67 -> 67 (this dev machine's real max=3).
        assert_eq!(percent_from_raw(1, 3), 33);
        assert_eq!(percent_from_raw(2, 3), 67);
    }

    #[test]
    fn percent_from_raw_returns_the_unavailable_sentinel_when_max_is_not_positive() {
        assert_eq!(percent_from_raw(0, 0), -1);
        assert_eq!(percent_from_raw(5, -1), -1);
    }

    #[test]
    fn percent_from_raw_does_not_overflow_on_a_malformed_near_i32_max_reading() {
        assert_eq!(percent_from_raw(i32::MAX, i32::MAX), 100);
        assert_eq!(percent_from_raw(i32::MAX / 2, i32::MAX), 50);
    }

    #[test]
    fn percent_from_raw_clamps_an_out_of_range_brightness() {
        assert_eq!(percent_from_raw(99, 3), 100);
        assert_eq!(percent_from_raw(-5, 3), 0);
    }

    // ---- raw_from_percent ----

    #[test]
    fn raw_from_percent_scales_zero_to_one_hundred_across_zero_to_max() {
        assert_eq!(raw_from_percent(0, 3), 0);
        assert_eq!(raw_from_percent(100, 3), 3);
    }

    #[test]
    fn raw_from_percent_rounds_half_away_from_zero() {
        // 33% of 3 = 0.99 -> 1; 67% of 3 = 2.01 -> 2.
        assert_eq!(raw_from_percent(33, 3), 1);
        assert_eq!(raw_from_percent(67, 3), 2);
    }

    #[test]
    fn raw_from_percent_clamps_a_percent_above_one_hundred() {
        assert_eq!(raw_from_percent(150, 3), 3);
    }

    #[test]
    fn raw_from_percent_does_not_overflow_on_a_malformed_near_i32_max_max() {
        assert_eq!(raw_from_percent(100, i32::MAX), i32::MAX);
        assert_eq!(raw_from_percent(50, i32::MAX), i32::MAX / 2 + 1);
    }

    #[test]
    fn raw_from_percent_is_zero_when_max_is_not_positive() {
        assert_eq!(raw_from_percent(50, 0), 0);
    }
}
