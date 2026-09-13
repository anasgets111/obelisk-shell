//! [`SysinfoController`] runs three configurable poll tasks, for cpu, ram+swap, and
//! temp_cores+temp_gpu, feeding one `SysinfoState` (ADR-0035).

use std::time::Duration;

/// `obelisk.sysinfo`'s five Lua-visible fields, with field
/// names unchanged from the `StateSnapshot` JSON keys.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct SysinfoState {
    /// Total CPU utilization, `0` to `100`, across cores. `0` before two samples can form a delta.
    pub cpu_percent: u8,
    /// Physical memory in use, `0` to `100`.
    pub ram_percent: u8,
    /// Swap in use, `0` to `100`; `0` means either no swap or empty swap.
    pub swap_percent: u8,
    /// Per-core Celsius temperatures from one hwmon pass. Empty when none are exposed. Length is
    /// sensor count, not core count, in hwmon order.
    pub temp_cores: Vec<i64>,
    /// GPU temperature in Celsius, or `-1` without a GPU sensor. Read in the same hwmon pass as
    /// [`SysinfoState::temp_cores`], so neither is newer than the other.
    pub temp_gpu: i64,
}

impl Default for SysinfoState {
    /// Pre-first-sample sentinels (ADR-0035): `0` for the three percent fields; `temp_gpu` uses
    /// its IDL-mandated `-1`.
    fn default() -> Self {
        Self { cpu_percent: 0, ram_percent: 0, swap_percent: 0, temp_cores: Vec::new(), temp_gpu: -1 }
    }
}

/// Whether a metric task ticks or parks with zero wakeups (ADR-0035), reevaluated when its
/// `watch::Receiver` reports an interval change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollMode {
    /// `interval == 0`: no timer; await only `watch::Receiver::changed()`.
    Dormant,
    /// `interval != 0`: race `tokio::time::interval(_).tick()` against
    /// `watch::Receiver::changed()`.
    Ticking(Duration),
}

pub fn poll_mode(interval: Duration) -> PollMode {
    if interval.is_zero() { PollMode::Dormant } else { PollMode::Ticking(interval) }
}

/// Wakes `main.rs`'s `select!` to push a `StateSnapshot`; a named single variant keeps the arm
/// clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysinfoSignal {
    Changed,
}

/// Parsed `sysinfo:configure({cpu_interval, ram_interval, temp_interval})` argument (ADR-0035).
/// Present keys override intervals; absent keys stay unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SysinfoConfigure {
    pub cpu_interval: Option<u64>,
    pub ram_interval: Option<u64>,
    pub temp_interval: Option<u64>,
}

/// `sysinfo:configure(cfg)`'s `arguments: [cfg]`: `arguments[0]` is a JSON object, and any wrong
/// present-key type drops the whole call (`None`), with no partial apply.
pub fn parse_configure_args(arguments: &[serde_json::Value]) -> Option<SysinfoConfigure> {
    let table = arguments.first()?.as_object()?;
    let read_seconds = |key: &str| table.get(key).map_or(Some(None), |value| value.as_u64().map(Some));
    Some(SysinfoConfigure {
        cpu_interval: read_seconds("cpu_interval")?,
        ram_interval: read_seconds("ram_interval")?,
        temp_interval: read_seconds("temp_interval")?,
    })
}

/// Owns the three poll tasks and their state. Not `Clone`: synchronous, non-blocking `configure`
/// uses `&SysinfoController` directly.
pub struct SysinfoController {
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    cpu_interval: tokio::sync::watch::Sender<Duration>,
    ram_interval: tokio::sync::watch::Sender<Duration>,
    temp_interval: tokio::sync::watch::Sender<Duration>,
}

impl SysinfoController {
    /// Spawns all three dormant (`Duration::ZERO`) until Lua calls `configure`. They share
    /// `signal_tx` and signal only after real-tick state updates. Resolve temp chips once here;
    /// `hwmon_root` is not threaded into the task.
    pub fn new(
        proc_root: std::path::PathBuf,
        hwmon_root: std::path::PathBuf,
        signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
    ) -> Self {
        let state = std::sync::Arc::new(std::sync::Mutex::new(SysinfoState::default()));

        let (cpu_interval, cpu_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (ram_interval, ram_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (temp_interval, temp_rx) = tokio::sync::watch::channel(Duration::ZERO);

        let core_source = super::temp::resolve_temp_cores_source(&hwmon_root);
        let gpu_chip = super::temp::resolve_gpu_chip(&hwmon_root);

        tokio::spawn(run_cpu_task(proc_root.clone(), cpu_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_ram_task(proc_root, ram_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_temp_task(core_source, gpu_chip, temp_rx, std::sync::Arc::clone(&state), signal_tx));

        Self { state, cpu_interval, ram_interval, temp_interval }
    }

    /// Applies parsed `sysinfo:configure(cfg)`: present intervals wake, retime, or suspend their
    /// task at `0`; absent ones stay unchanged. `send` errors only after task panic, logged here.
    pub fn configure(&self, cfg: SysinfoConfigure) {
        let send = |seconds: Option<u64>, sender: &tokio::sync::watch::Sender<Duration>, name: &str| {
            if let Some(sec) = seconds
                && sender.send(Duration::from_secs(sec)).is_err()
            {
                eprintln!("sysinfo: {name} task is gone, {name}_interval update dropped");
            }
        };
        send(cfg.cpu_interval, &self.cpu_interval, "cpu");
        send(cfg.ram_interval, &self.ram_interval, "ram");
        send(cfg.temp_interval, &self.temp_interval, "temp");
    }

    /// Current combined state for `main.rs`'s signal-channel `select!` snapshot push.
    pub fn snapshot(&self) -> SysinfoState {
        self.state.lock().expect("sysinfo state mutex poisoned").clone()
    }
}

/// Writes one task's fields and publishes only if that changed something.
///
/// Every send hydrates a `StateSnapshot`, which marks the scene dirty and costs a whole re-resolve
/// (ADR-0044 decision 2). A machine at rest reports the same rounded percentage and the same whole
/// Celsius for minutes together, so the unconditional send was buying a re-resolve per tick for no
/// new information. The sample itself is still taken and still stored: `cpu_percent` needs the
/// counters for the next delta whether or not the rounded result moved.
fn publish_if_changed(
    state: &std::sync::Mutex<SysinfoState>,
    signal_tx: &tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
    write: impl FnOnce(&mut SysinfoState) -> bool,
) {
    // Drop the lock before sending: the receiver hydrates a snapshot and must never wait on a
    // poll task's mutex to do it.
    let changed = {
        let mut state = state.lock().expect("sysinfo state mutex poisoned");
        write(&mut state)
    };
    if changed {
        let _ = signal_tx.send(SysinfoSignal::Changed);
    }
}

/// `cpu_percent` task. Keeps the previous `/proc/stat` sample across ticks; the first tick after
/// cold start or resume stores only a sample. The three similar loops stay separate because a
/// closure returning a future borrowing its own state cannot escape stable Rust's plain `FnMut`.
async fn run_cpu_task(
    proc_root: std::path::PathBuf,
    mut interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
) {
    let mut previous: Option<super::cpu::CpuSample> = None;
    loop {
        let interval = *interval_rx.borrow_and_update();
        match poll_mode(interval) {
            PollMode::Dormant => {
                // `/proc/stat` counters are cumulative since boot; discard pre-dormancy samples or
                // the next delta is bogus.
                previous = None;
                if interval_rx.changed().await.is_err() {
                    return; // every SysinfoController that could reconfigure this task is gone
                }
            }
            PollMode::Ticking(duration) => {
                let mut ticker = tokio::time::interval(duration);
                ticker.tick().await; // tokio::time::interval's first tick fires immediately; consume it unused
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            match super::cpu::read_sample(&proc_root) {
                                Ok(sample) => {
                                    if let Some(prev) = previous.take() {
                                        let percent = super::cpu::delta_percent(&prev, &sample);
                                        publish_if_changed(&state, &signal_tx, |state| {
                                            let changed = state.cpu_percent != percent;
                                            state.cpu_percent = percent;
                                            changed
                                        });
                                    }
                                    previous = Some(sample);
                                }
                                Err(err) => eprintln!("sysinfo: failed to read /proc/stat: {err}"),
                            }
                        }
                        changed = interval_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            break; // interval reconfigured -- rebuild dormant/ticking in the outer loop
                        }
                    }
                }
            }
        }
    }
}

/// `ram_percent`/`swap_percent` task. One `/proc/meminfo` read per tick; `swap_percent` rides
/// `ram_interval` with no separate interval (ADR-0035).
async fn run_ram_task(
    proc_root: std::path::PathBuf,
    mut interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
) {
    loop {
        let interval = *interval_rx.borrow_and_update();
        match poll_mode(interval) {
            PollMode::Dormant => {
                if interval_rx.changed().await.is_err() {
                    return;
                }
            }
            PollMode::Ticking(duration) => {
                let mut ticker = tokio::time::interval(duration);
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            match super::ram::read_meminfo(&proc_root) {
                                Ok(info) => {
                                    let (ram_percent, swap_percent) = super::ram::compute_percentages(&info);
                                    publish_if_changed(&state, &signal_tx, |state| {
                                        let changed =
                                            state.ram_percent != ram_percent || state.swap_percent != swap_percent;
                                        state.ram_percent = ram_percent;
                                        state.swap_percent = swap_percent;
                                        changed
                                    });
                                }
                                Err(err) => eprintln!("sysinfo: failed to read /proc/meminfo: {err}"),
                            }
                        }
                        changed = interval_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// `temp_cores`/`temp_gpu` task. One hwmon pass per tick; `temp_gpu` rides `temp_interval`. Resolve
/// `core_source`/`gpu_chip` once in `new`: onboard sensors do not hotplug, so rescanning each tick
/// wastes work.
async fn run_temp_task(
    core_source: super::temp::CoreTempSource,
    gpu_chip: Option<std::path::PathBuf>,
    mut interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
) {
    loop {
        let interval = *interval_rx.borrow_and_update();
        match poll_mode(interval) {
            PollMode::Dormant => {
                if interval_rx.changed().await.is_err() {
                    return;
                }
            }
            PollMode::Ticking(duration) => {
                let mut ticker = tokio::time::interval(duration);
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            let temp_cores = super::temp::read_temp_cores_from(&core_source);
                            let temp_gpu = super::temp::read_temp_gpu_from(gpu_chip.as_deref());
                            publish_if_changed(&state, &signal_tx, |state| {
                                let changed = state.temp_cores != temp_cores || state.temp_gpu != temp_gpu;
                                state.temp_cores = temp_cores;
                                state.temp_gpu = temp_gpu;
                                changed
                            });
                        }
                        changed = interval_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_configure_args_reads_all_three_present_intervals() {
        let cfg = super::parse_configure_args(&[serde_json::json!({
            "cpu_interval": 2,
            "ram_interval": 5,
            "temp_interval": 10,
        })])
        .expect("a well-formed configure table must parse");
        assert_eq!(cfg.cpu_interval, Some(2));
        assert_eq!(cfg.ram_interval, Some(5));
        assert_eq!(cfg.temp_interval, Some(10));
    }

    #[test]
    fn parse_configure_args_leaves_absent_keys_as_none() {
        let cfg = super::parse_configure_args(&[serde_json::json!({ "temp_interval": 0 })])
            .expect("a partial table must still parse");
        assert_eq!(cfg.cpu_interval, None);
        assert_eq!(cfg.ram_interval, None);
        assert_eq!(cfg.temp_interval, Some(0));
    }

    #[test]
    fn parse_configure_args_rejects_a_missing_argument() {
        assert_eq!(super::parse_configure_args(&[]), None);
    }

    #[test]
    fn parse_configure_args_rejects_a_non_table_argument() {
        assert_eq!(super::parse_configure_args(&[serde_json::json!(5)]), None);
    }

    #[test]
    fn parse_configure_args_drops_the_whole_call_on_one_wrong_typed_present_key() {
        assert_eq!(
            super::parse_configure_args(&[serde_json::json!({ "cpu_interval": "fast", "ram_interval": 5 })]),
            None
        );
    }

    #[test]
    fn poll_mode_is_dormant_at_zero_and_ticking_otherwise() {
        assert_eq!(super::poll_mode(std::time::Duration::ZERO), super::PollMode::Dormant);
        assert_eq!(
            super::poll_mode(std::time::Duration::from_secs(5)),
            super::PollMode::Ticking(std::time::Duration::from_secs(5))
        );
    }

    /// `publish_if_changed` needs a state and a channel; both tasks and this test build them the
    /// same way, so a helper keeps the four cases below to their point.
    fn state_and_channel() -> (
        std::sync::Arc<std::sync::Mutex<super::SysinfoState>>,
        tokio::sync::mpsc::UnboundedSender<super::SysinfoSignal>,
        tokio::sync::mpsc::UnboundedReceiver<super::SysinfoSignal>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (std::sync::Arc::new(std::sync::Mutex::new(super::SysinfoState::default())), tx, rx)
    }

    #[test]
    fn a_sample_that_moved_a_field_publishes() {
        let (state, tx, mut rx) = state_and_channel();
        super::publish_if_changed(&state, &tx, |state| {
            let changed = state.cpu_percent != 42;
            state.cpu_percent = 42;
            changed
        });
        assert_eq!(rx.try_recv(), Ok(super::SysinfoSignal::Changed));
        assert_eq!(state.lock().unwrap().cpu_percent, 42);
    }

    #[test]
    fn a_sample_that_measured_the_same_number_publishes_nothing() {
        // The whole point: an idle machine reports the same rounded percentage tick after tick,
        // and each publish would cost a scene re-resolve (ADR-0044 decision 2).
        let (state, tx, mut rx) = state_and_channel();
        state.lock().unwrap().cpu_percent = 7;
        super::publish_if_changed(&state, &tx, |state| {
            let changed = state.cpu_percent != 7;
            state.cpu_percent = 7;
            changed
        });
        assert!(rx.try_recv().is_err(), "an unchanged sample must not hydrate a snapshot");
    }

    #[test]
    fn sysinfo_state_default_matches_the_pre_first_sample_sentinels() {
        let state = super::SysinfoState::default();
        assert_eq!(state.cpu_percent, 0);
        assert_eq!(state.ram_percent, 0);
        assert_eq!(state.swap_percent, 0);
        assert_eq!(state.temp_cores, Vec::<i64>::new());
        assert_eq!(state.temp_gpu, -1, "matches the IDL's own -1 undetected sentinel, not 0");
    }
}
