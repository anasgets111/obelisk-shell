//! [`SysinfoController`] runs three configurable poll tasks, for cpu, ram+swap, and
//! temp_cores+temp_gpu, feeding one `SysinfoState` (ADR-0035).

use std::time::Duration;

/// `oblisk.sysinfo`'s five Lua-visible fields (docs/oblisk-idl-api-specs.md §2.12), with field
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
    let read_seconds = |key: &str| -> Option<Option<u64>> {
        match table.get(key) {
            Some(value) => value.as_u64().map(Some),
            None => Some(None),
        }
    };
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
        if let Some(sec) = cfg.cpu_interval
            && self.cpu_interval.send(Duration::from_secs(sec)).is_err()
        {
            eprintln!("sysinfo: cpu task is gone, cpu_interval update dropped");
        }
        if let Some(sec) = cfg.ram_interval
            && self.ram_interval.send(Duration::from_secs(sec)).is_err()
        {
            eprintln!("sysinfo: ram task is gone, ram_interval update dropped");
        }
        if let Some(sec) = cfg.temp_interval
            && self.temp_interval.send(Duration::from_secs(sec)).is_err()
        {
            eprintln!("sysinfo: temp task is gone, temp_interval update dropped");
        }
    }

    /// Current combined state for `main.rs`'s signal-channel `select!` snapshot push.
    pub fn snapshot(&self) -> SysinfoState {
        self.state.lock().expect("sysinfo state mutex poisoned").clone()
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
                                        state.lock().expect("sysinfo state mutex poisoned").cpu_percent = percent;
                                        let _ = signal_tx.send(SysinfoSignal::Changed);
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
                                    {
                                        let mut state = state.lock().expect("sysinfo state mutex poisoned");
                                        state.ram_percent = ram_percent;
                                        state.swap_percent = swap_percent;
                                    }
                                    let _ = signal_tx.send(SysinfoSignal::Changed);
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
                            {
                                let mut state = state.lock().expect("sysinfo state mutex poisoned");
                                state.temp_cores = temp_cores;
                                state.temp_gpu = temp_gpu;
                            }
                            let _ = signal_tx.send(SysinfoSignal::Changed);
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
