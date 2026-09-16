//! Raw-brightness/percent conversion for `keyboard`'s LED backlight (ADR-0034) and
//! `brightness`'s sysfs backlight (ADR-0053), both scaled from `[0, max]` to `[0, 100]`.

/// Converts raw `[0, max]` to `[0, 100]`, rounding half away from zero. `max <= 0` returns the
/// IDL's `-1` sentinel, not a divide-by-zero or fabricated `0`; `brightness` filters such devices.
pub fn percent_from_raw(brightness: i32, max: i32) -> i32 {
    if max <= 0 {
        return -1;
    }
    // i64 avoids `i32` overflow once `max`/`brightness` exceeds `i32::MAX / 100`.
    let brightness = i64::from(brightness.clamp(0, max));
    let max64 = i64::from(max);
    let scaled = 100 * brightness;
    let half = max64 / 2;
    (((scaled + half) / max64) as i32).clamp(0, 100)
}

/// Converts `[0, 100]` percent to the raw `[0, max]` scale for `SetBrightness`, rounding half
/// away from zero and clamping. `pct` is unvalidated `u64`; clamping happens here.
pub fn raw_from_percent(pct: u64, max: i32) -> i32 {
    if max <= 0 {
        return 0;
    }
    // i64 intermediates avoid the same overflow as `percent_from_raw`.
    let pct = pct.min(100) as i64;
    let max64 = i64::from(max);
    let scaled = pct * max64;
    (((scaled + 50) / 100) as i32).clamp(0, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_from_raw_scales_zero_to_max_across_zero_to_one_hundred() {
        assert_eq!(percent_from_raw(0, 3), 0);
        assert_eq!(percent_from_raw(3, 3), 100);
    }

    #[test]
    fn percent_from_raw_rounds_half_away_from_zero_on_a_coarse_scale() {
        // 1/3 -> 33; 2/3 -> 67 (this machine's real max is 3).
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

    #[test]
    fn raw_from_percent_scales_zero_to_one_hundred_across_zero_to_max() {
        assert_eq!(raw_from_percent(0, 3), 0);
        assert_eq!(raw_from_percent(100, 3), 3);
    }

    #[test]
    fn raw_from_percent_rounds_half_away_from_zero() {
        // 33% of 3 -> 1; 67% -> 2.
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
