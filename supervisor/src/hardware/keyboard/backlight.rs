//! Backlight half of `oblisk.keyboard` (ADR-0034, as corrected against this dev machine's real
//! `org.freedesktop.UPower.KbdBacklight` introspection): raw-brightness/percent conversion and
//! the hand-written `KbdBacklight` proxy. Split from `hardware::keyboard` -- see
//! `hardware/keyboard/mod.rs` for the module-level doc.

/// Converts a raw `[0, max]` brightness reading into a `[0, 100]` percent, round-half-away-
/// from-zero (matches `sysinfo::temp::round_milli_c`'s rounding convention). `max <= 0` (no
/// backlight hardware, or a malformed reading) returns the IDL's `-1` unavailable sentinel --
/// the same convention `temp_gpu` already established -- rather than dividing by zero or
/// fabricating a `0`.
pub fn percent_from_raw(brightness: i32, max: i32) -> i32 {
    if max <= 0 {
        return -1;
    }
    // i64 intermediates (Correctness review): `100 * brightness` in `i32` overflows once
    // `max`/`brightness` exceeds `i32::MAX / 100` -- a malformed or buggy `GetMaxBrightness`
    // reply shouldn't be able to panic (debug) or silently wrap (release) this arithmetic.
    let brightness = i64::from(brightness.clamp(0, max));
    let max64 = i64::from(max);
    let scaled = 100 * brightness;
    let half = max64 / 2;
    (((scaled + half) / max64) as i32).clamp(0, 100)
}

/// The inverse of [`percent_from_raw`]: converts a `[0, 100]` percent into the raw `[0, max]`
/// scale `SetBrightness` expects, round-half-away-from-zero, clamped to `[0, max]`. `pct` is
/// taken as `u64` straight from `parse_set_backlight_args` (never validated to `<= 100` at parse
/// time, matching every other numeric `parse_*_args` in this codebase -- e.g.
/// `idle::parse_register_args`'s unclamped seconds) -- clamping happens here, the one place that
/// actually needs the bound.
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

#[zbus::proxy(
    interface = "org.freedesktop.UPower.KbdBacklight",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/KbdBacklight"
)]
pub(crate) trait KbdBacklight {
    #[zbus(name = "GetBrightness")]
    fn get_brightness(&self) -> zbus::Result<i32>;
    #[zbus(name = "GetMaxBrightness")]
    fn get_max_brightness(&self) -> zbus::Result<i32>;
    #[zbus(name = "SetBrightness")]
    fn set_brightness(&self, value: i32) -> zbus::Result<()>;
    #[zbus(signal, name = "BrightnessChanged")]
    fn brightness_changed(&self, value: i32) -> zbus::Result<()>;
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
        // Correctness review: a buggy/malformed GetMaxBrightness reply this large must not panic
        // (debug) or silently wrap (release) the `100 * brightness` intermediate.
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

    // ---- KbdBacklightProxy (real D-Bus call, p2p pattern -- mirrors idle::inhibit's
    // Login1ManagerProxy test) ----

    use tokio::net::UnixStream;

    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder = zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p();
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    struct StubKbdBacklight {
        brightness: std::sync::Arc<std::sync::atomic::AtomicI32>,
        max: i32,
        set_calls: tokio::sync::mpsc::UnboundedSender<i32>,
    }

    #[zbus::interface(name = "org.freedesktop.UPower.KbdBacklight")]
    impl StubKbdBacklight {
        #[zbus(name = "GetBrightness")]
        fn get_brightness(&self) -> i32 {
            self.brightness.load(std::sync::atomic::Ordering::SeqCst)
        }
        #[zbus(name = "GetMaxBrightness")]
        fn get_max_brightness(&self) -> i32 {
            self.max
        }
        #[zbus(name = "SetBrightness")]
        fn set_brightness(&self, value: i32) {
            self.brightness.store(value, std::sync::atomic::Ordering::SeqCst);
            let _ = self.set_calls.send(value);
        }
    }

    async fn build_proxy(caller_side: &zbus::Connection) -> KbdBacklightProxy<'_> {
        zbus::proxy::Builder::new(caller_side)
            .destination("org.oblisk.test")
            .expect("valid destination bus name")
            .path("/org/freedesktop/UPower/KbdBacklight")
            .expect("valid object path")
            .interface("org.freedesktop.UPower.KbdBacklight")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p KbdBacklightProxy")
    }

    #[tokio::test]
    async fn kbd_backlight_proxy_reads_brightness_and_max_and_sets_a_new_value() {
        let (service_side, caller_side) = p2p_pair().await;
        let (set_tx, mut set_rx) = tokio::sync::mpsc::unbounded_channel();
        let brightness = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(1));
        service_side
            .object_server()
            .at("/org/freedesktop/UPower/KbdBacklight", StubKbdBacklight { brightness: brightness.clone(), max: 3, set_calls: set_tx })
            .await
            .expect("failed to export the stub KbdBacklight");

        let proxy = build_proxy(&caller_side).await;

        assert_eq!(proxy.get_brightness().await.expect("GetBrightness"), 1);
        assert_eq!(proxy.get_max_brightness().await.expect("GetMaxBrightness"), 3);

        proxy.set_brightness(3).await.expect("SetBrightness");
        assert_eq!(set_rx.recv().await, Some(3));
        assert_eq!(brightness.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
