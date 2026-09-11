//! Keyboard backlight proxy for `obelisk.keyboard` (ADR-0034); raw-to-percent scaling is in
//! `crate::capabilities::scale` (ADR-0053).

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
    use crate::capabilities::test_support::p2p_pair_serving;

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
            .destination("org.obelisk.test")
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
        let (set_tx, mut set_rx) = tokio::sync::mpsc::unbounded_channel();
        let brightness = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(1));
        let (caller_side, _service_side) = p2p_pair_serving(|peer| {
            peer.serve_at(
                "/org/freedesktop/UPower/KbdBacklight",
                StubKbdBacklight { brightness: brightness.clone(), max: 3, set_calls: set_tx },
            )
        })
        .await;

        let proxy = build_proxy(&caller_side).await;

        assert_eq!(proxy.get_brightness().await.expect("GetBrightness"), 1);
        assert_eq!(proxy.get_max_brightness().await.expect("GetMaxBrightness"), 3);

        proxy.set_brightness(3).await.expect("SetBrightness");
        assert_eq!(set_rx.recv().await, Some(3));
        assert_eq!(brightness.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
