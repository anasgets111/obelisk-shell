//! Lazy capability startup, signal routing, and command dispatch (ADR-0037, ADR-0070, ADR-0076).
//!
//! `run_supervisor` once held sixteen controllers, twenty channels, five locals each, and 800
//! lines, requiring six edits across five `main.rs` regions per capability. [`Capabilities`] now
//! owns the fields; exhaustive start, push, and dispatch matches fail at each required arm.
//!
//! No trait or boxed registry: controllers differ (`build_state`, `snapshot`, async
//! `handle_signal`, or a channel), and `main.rs` needs concrete types. ADR-0037 decision 3 chose
//! static calls; this module moves that dispatch here.
//!
//! **One flat child module per roster entry.** Shared names cover `shared::Capability`,
//! `obelisk.<name>`, and command `capability`; the former four-level `dbus/`/`hardware/` grouping
//! split `battery` and `power` without benefit. [`read_attr`] and [`parse_bool_arg`] moved here,
//! `polkit` to `crate::polkit`, and `shm_icons` beside its two consumers (ADR-0076).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::{Capability, CommandEnvelope};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::snapshot::push_snapshot;
use crate::{log_unstarted, socket};
use applications::{ApplicationsController, ApplicationsSignal};
use audio::mixer::{AudioState, PrivacySources};
use battery::{BatteryController, BatterySignal};
use bluetooth::{BluetoothController, BluetoothSignal};
use brightness::{BrightnessController, BrightnessSignal};
use files::{FilesController, FilesSignal};
use idle::{IdleController, IdleState};
use keyboard::{KeyboardController, KeyboardSignal};
use lock::LockController;
use mpris::{MprisController, MprisSignal};
use network::{NetworkController, NetworkSignal};
use notifications::{NotificationsController, NotificationsSignal};
use power::{PowerController, PowerSignal};
use privacy::{PrivacyController, PrivacySignal};
use processes::{ProcessesController, ProcessesSignal};
use storage::{StorageController, StorageSignal};
use sysinfo::{SysinfoController, SysinfoSignal};
use system::{SystemController, SystemSignal};
use tray::{TrayController, TraySignal};
use updates::{UpdatesController, UpdatesSignal};
use workspaces::{WorkspacesController, WorkspacesSignal};

/// Every Supervisor bus gets the 25s call timeout Qt, GDBus and libdbus default to; zbus has none
/// (ADR-0070 amendment).
pub async fn with_call_timeout(builder: zbus::Result<zbus::connection::Builder<'_>>) -> zbus::Result<zbus::Connection> {
    builder?.method_timeout(std::time::Duration::from_secs(25)).build().await
}

pub mod applications;
pub mod audio;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod files;
pub mod idle;
pub mod keyboard;
pub mod lock;
pub mod mpris;
pub mod network;
pub mod notifications;
pub mod polkit;
pub mod power;
pub mod privacy;
pub mod processes;
pub mod scale;
mod shm_icons;
pub mod storage;
pub mod sysinfo;
pub mod system;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tray;
pub mod updates;
pub mod workspaces;

/// Reads and trims a sysfs attribute under `entry_dir`; missing or unreadable means absent.
pub fn read_attr(entry_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(entry_dir.join(name)).ok().map(|text| text.trim().to_string())
}

/// Truncates to `max_bytes`, backing off to a UTF-8 boundary (bytes, not chars).
///
/// Every capability that copies a string out of a third party's D-Bus reply caps it here, so the
/// rule lives once: notifications for `Notify`'s properties and tray for the `StatusNotifierItem`
/// and DBusMenu text an arbitrary application supplies.
pub fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
}

/// Parses the first JSON boolean in `arguments: [en]` for `*:set_*_enabled(en)` actions.
pub fn parse_bool_arg(arguments: &[serde_json::Value]) -> Option<bool> {
    arguments.first()?.as_bool()
}

/// Binds a macro-generated zbus proxy at `path`. A generated `<Proxy>::new` ties the proxy to
/// `&Connection` even though its builder clones the connection, so stored proxies go through the
/// builder to stay `'static`.
pub async fn bind<T>(connection: &zbus::Connection, path: zbus::zvariant::OwnedObjectPath) -> zbus::Result<T>
where
    T: zbus::proxy::Defaults + From<zbus::Proxy<'static>>,
{
    zbus::proxy::Builder::new(connection).path(path)?.build().await
}

/// A received signal waiting for [`Capabilities::push`]. [`Signals::next`] only awaits `recv()`,
/// so losing the `tokio::select!` race drops no signal; the winning arm's body is not canceled.
/// This matters because `network`/`bluetooth` await while building state.
#[derive(Debug)]
pub enum Signal {
    /// Carries mixer state directly, not a controller name.
    Audio(AudioState),
    Network(NetworkSignal),
    Bluetooth(BluetoothSignal),
    Tray,
    Mpris,
    Notifications,
    Sysinfo,
    Keyboard,
    Battery,
    Brightness,
    Workspaces,
    Power,
    Applications,
    Files,
    Storage,
    Processes,
    System,
    Privacy,
    Updates,
    /// Carries inhibitor state directly; no controller reads it back (ADR-0141).
    Idle(IdleState),
}

/// Declares each capability channel once, generating [`Signals`], [`Senders`], cancel-safe
/// [`Signals::next`], and their constructor.
///
/// One macro replaces four hand-kept lists. Adding `shared::Capability` then fails `start` and
/// `dispatch`; filling them without a signal path would otherwise build while Lua reads `nil`, the
/// silent failure ADR-0076 removes.
///
/// Every roster variant appears in `channels` or `without_channel`; the exhaustive helper makes a
/// missing row an `E0004` here. `Signal` stays hand-written because variants choose their payload;
/// per-variant doc comments would flatten to punctuation in a macro row, while
/// [`Capabilities::push`] guards the mapping exhaustively.
///
/// The chain is exhaustive: a new roster variant fails `start`, `dispatch`, and the channel helper;
/// its channel row then fails on `Signal`, and that variant fails `push`. Each compiler error names
/// the next required edit instead of allowing silent loss after the second.
macro_rules! capability_channels {
    (
        channels {
            $($variant:ident => $field:ident : $payload:ty, $pattern:pat => $signal:expr;)+
        }
        without_channel { $($no_channel:ident),* $(,)? }
    ) => {
        /// Receiving half of each capability channel.
        pub struct Signals {
            $($field: UnboundedReceiver<$payload>,)+
        }

        /// Sending half handed to controllers.
        struct Senders {
            $($field: UnboundedSender<$payload>,)+
        }

        impl Senders {
            /// Builds both halves for each channel-bearing roster entry.
            fn channels() -> (Self, Signals) {
                // Bind both halves once, then partially move each into its struct.
                $(let $field = unbounded_channel();)+
                (Self { $($field: $field.0,)+ }, Signals { $($field: $field.1,)+ })
            }
        }

        impl Signals {
            /// Awaits the first signal. Bare `recv()` branches are cancel-safe; `None` requires all
            /// senders to drop, which cannot happen while [`Capabilities`] lives.
            pub async fn next(&mut self) -> Option<Signal> {
                tokio::select! {
                    $($pattern = self.$field.recv() => Some($signal),)+
                    else => None,
                }
            }
        }

        /// Exhaustiveness guard for roster entries without a channel.
        #[allow(dead_code)]
        fn every_capability_has_a_channel_row(capability: Capability) {
            match capability {
                $(Capability::$variant => {})+
                $(Capability::$no_channel => {})*
            }
        }
    };
}

capability_channels! {
    channels {
        // Mixer sends state directly (see `Signal::Audio`).
        Audio => audio: AudioState, Some(state) => Signal::Audio(state);
        // Controllers own signal semantics (ADR-0037).
        Network => network: NetworkSignal, Some(signal) => Signal::Network(signal);
        Bluetooth => bluetooth: BluetoothSignal, Some(signal) => Signal::Bluetooth(signal);
        // Other `Changed` signals collapse to unit `Signal` variants.
        Tray => tray: TraySignal, Some(TraySignal::RegistryChanged) => Signal::Tray;
        Mpris => mpris: MprisSignal, Some(MprisSignal::Changed) => Signal::Mpris;
        Notifications => notifications: NotificationsSignal,
            Some(NotificationsSignal::Changed) => Signal::Notifications;
        Sysinfo => sysinfo: SysinfoSignal, Some(SysinfoSignal::Changed) => Signal::Sysinfo;
        Keyboard => keyboard: KeyboardSignal, Some(KeyboardSignal::Changed) => Signal::Keyboard;
        Privacy => privacy: PrivacySignal, Some(PrivacySignal::Changed) => Signal::Privacy;
        Updates => updates: UpdatesSignal, Some(UpdatesSignal::Changed) => Signal::Updates;
        Battery => battery: BatterySignal, Some(BatterySignal::Changed) => Signal::Battery;
        System => system: SystemSignal, Some(SystemSignal::Changed) => Signal::System;
        Brightness => brightness: BrightnessSignal, Some(BrightnessSignal::Changed) => Signal::Brightness;
        Workspaces => workspaces: WorkspacesSignal, Some(WorkspacesSignal::Changed) => Signal::Workspaces;
        Power => power: PowerSignal, Some(PowerSignal::Changed) => Signal::Power;
        Applications => applications: ApplicationsSignal,
            Some(ApplicationsSignal::Changed) => Signal::Applications;
        Files => files: FilesSignal, Some(FilesSignal::Changed) => Signal::Files;
        Storage => storage: StorageSignal, Some(StorageSignal::Changed) => Signal::Storage;
        Processes => processes: ProcessesSignal, Some(ProcessesSignal::Changed) => Signal::Processes;
            // `idle/controller.rs`'s inhibitor watch owns and sends state, like `Audio` (ADR-0141).
        Idle => idle: IdleState, Some(state) => Signal::Idle(state);
    }
    // `lock` has no channel: boot-built in `main.rs` for relock (ADR-0060), it reports through
    // existing `LockOutcome` frames, not a `StateSnapshot` (ADR-0052 decision 4).
    without_channel { Lock, Polkit }
}

/// On-demand controllers and their inputs. `audio` starts the PipeWire mixer and stores its command
/// channel; `lock` is boot-built in `main.rs` for relock (ADR-0060) and passed to dispatch.
pub struct Capabilities {
    network: Option<NetworkController>,
    bluetooth: Option<BluetoothController>,
    tray: Option<TrayController>,
    notifications: Option<NotificationsController>,
    mpris: Option<MprisController>,
    sysinfo: Option<SysinfoController>,
    keyboard: Option<KeyboardController>,
    privacy: Option<PrivacyController>,
    updates: Option<UpdatesController>,
    battery: Option<BatteryController>,
    brightness: Option<BrightnessController>,
    workspaces: Option<WorkspacesController>,
    power: Option<PowerController>,
    system: Option<SystemController>,
    applications: Option<ApplicationsController>,
    files: Option<FilesController>,
    storage: Option<StorageController>,
    processes: Option<ProcessesController>,
    audio: Option<audio::mixer::AudioCommandSender>,
    idle: Option<IdleController>,

    senders: Senders,
    /// Event-shaped, not a roster entry: never pushes a `StateSnapshot`; events go to `main.rs`
    /// (ADR-0032).
    idle_tx: UnboundedSender<shared::IdleEvent>,
    /// Shared Supervisor system bus (ADR-0034); session-bus capabilities open their own.
    connection: zbus::Connection,
    sound_tx: std::sync::mpsc::SyncSender<PathBuf>,
    /// Mixer privacy channel (ADR-0034, ADR-0137). `audio` or `privacy` may start the mixer first;
    /// mixer owns the sender and privacy the receiver.
    privacy_tx: Option<UnboundedSender<PrivacySources>>,
    privacy_sources: Option<UnboundedReceiver<PrivacySources>>,
}

impl Capabilities {
    /// Builds channels and returns both halves. Controllers start only when config reads their
    /// `obelisk` member (ADR-0070 decision 1).
    pub fn new(
        connection: zbus::Connection,
        sound_tx: std::sync::mpsc::SyncSender<PathBuf>,
        idle_tx: UnboundedSender<shared::IdleEvent>,
    ) -> (Self, Signals) {
        let (senders, signals) = Senders::channels();
        let (privacy_tx, privacy_sources) = unbounded_channel();

        let capabilities = Self {
            network: None,
            bluetooth: None,
            tray: None,
            notifications: None,
            mpris: None,
            sysinfo: None,
            keyboard: None,
            privacy: None,
            updates: None,
            battery: None,
            brightness: None,
            workspaces: None,
            power: None,
            system: None,
            applications: None,
            files: None,
            storage: None,
            processes: None,
            audio: None,
            idle: None,
            senders,
            idle_tx,
            connection,
            sound_tx,
            privacy_tx: Some(privacy_tx),
            privacy_sources: Some(privacy_sources),
        };
        (capabilities, signals)
    }

    /// Stops every program declared with `session_process` and waits for it, the session-lifetime
    /// counterpart to `reap_all_processes`. A no-op on most shutdowns: the controller exists only
    /// once a config has read `obelisk.processes`.
    ///
    /// Awaited rather than dropped because these are the processes whose exit path was worth
    /// declaring a signal for; a shell that exits without giving them theirs is the reason the
    /// signal is configurable.
    pub async fn reap_sessions(&self) {
        if let Some(processes) = &self.processes {
            processes.reap_all().await;
        }
    }

    /// Live `idle` controller for resetting generation threshold registrations on reload
    /// (ADR-0006); `None` when no threshold was configured.
    pub fn idle(&self) -> Option<&IdleController> {
        self.idle.as_ref()
    }

    /// Live `network` controller for `secure_submit(network, connect)`; it owns pending intent, and
    /// plaintext secrets never enter this module (ADR-0029).
    pub fn network(&self) -> Option<&NetworkController> {
        self.network.as_ref()
    }

    /// Drops what a departed generation asked for. Its replacement starts with fresh state (named
    /// state survives only an in-place reload), so nothing would send the Bluetooth discovery stop
    /// or the Wi-Fi prompt cancel the old one owed, and discovery ran for the rest of the session.
    pub fn forget_departed_requests(&self) {
        if let Some(bluetooth) = &self.bluetooth {
            bluetooth.set_discovery(false);
        }
        if let Some(network) = &self.network {
            network.cancel_connect();
        }
    }

    /// ADR-0070 lazy start: inline await (decision 4), re-entrant across generation swaps (decision
    /// 3), with each arm a no-op after construction.
    pub async fn start(&mut self, capability: Capability) {
        match capability {
            Capability::Network => {
                if self.network.is_none() {
                    match NetworkController::new(self.connection.clone(), self.senders.network.clone()).await {
                        Ok(controller) => self.network = Some(controller),
                        Err(err) => {
                            eprintln!("network: NetworkManager is unreachable; capability disabled for this run: {err}")
                        }
                    }
                }
            }
            Capability::Bluetooth => {
                if self.bluetooth.is_none() {
                    self.bluetooth =
                        Some(BluetoothController::new(self.connection.clone(), self.senders.bluetooth.clone()).await);
                }
            }
            // Own session bus; missing it yields `inert`. Tray, Notifications and Mpris push once when
            // built: with no item, notification or player they never speak.
            Capability::Tray => {
                if self.tray.is_none() {
                    self.tray = Some(match with_call_timeout(zbus::connection::Builder::session()).await {
                        Ok(bus) => TrayController::new(bus, self.senders.tray.clone()).await,
                        Err(err) => {
                            eprintln!(
                                "tray: failed to connect to the session bus; tray host disabled for this run: {err}"
                            );
                            TrayController::inert(self.senders.tray.clone())
                        }
                    });
                    let _ = self.senders.tray.send(TraySignal::RegistryChanged);
                }
            }
            // Own session bus (ADR-0033); an existing notification owner makes this inert via
            // RequestName's DoNotQueue.
            Capability::Notifications => {
                if self.notifications.is_none() {
                    self.notifications = Some(match with_call_timeout(zbus::connection::Builder::session()).await {
                        Ok(bus) => {
                            NotificationsController::new(bus, self.senders.notifications.clone(), self.sound_tx.clone())
                                .await
                        }
                        Err(err) => {
                            eprintln!(
                                "notifications: failed to connect to the session bus; notifications server disabled for this run: {err}"
                            );
                            NotificationsController::inert(self.senders.notifications.clone(), self.sound_tx.clone())
                        }
                    });
                    let _ = self.senders.notifications.send(NotificationsSignal::Changed);
                }
            }
            // Own session bus (ADR-0036); `new` spawns discovery and returns.
            Capability::Mpris => {
                if self.mpris.is_none() {
                    self.mpris = Some(match with_call_timeout(zbus::connection::Builder::session()).await {
                        Ok(bus) => MprisController::new(bus, self.senders.mpris.clone()),
                        Err(err) => {
                            eprintln!(
                                "mpris: failed to connect to the session bus; player discovery disabled for this run: {err}"
                            );
                            MprisController::inert()
                        }
                    });
                    let _ = self.senders.mpris.send(MprisSignal::Changed);
                }
            }
            // Three dormant poll tasks until `sysinfo:configure` (ADR-0035).
            Capability::Sysinfo => {
                if self.sysinfo.is_none() {
                    self.sysinfo = Some(SysinfoController::new(
                        PathBuf::from("/proc"),
                        PathBuf::from("/sys/class/hwmon"),
                        self.senders.sysinfo.clone(),
                    ));
                }
            }
            // Missing KbdBacklight -> -1; missing lock source -> `false` (ADR-0034).
            Capability::Keyboard => {
                if self.keyboard.is_none() {
                    self.keyboard = Some(
                        KeyboardController::new(
                            self.connection.clone(),
                            &PathBuf::from("/sys/class/leds"),
                            self.senders.keyboard.clone(),
                        )
                        .await,
                    );
                }
            }
            // Camera `/dev/videoN` inotify plus `/proc` scan, enriched by `privacy_sources`
            // (ADR-0034); microphone/screencast share that channel (ADR-0137).
            Capability::Privacy => {
                if self.privacy.is_none() {
                    self.ensure_mixer_thread();
                    if let Some(privacy_sources) = self.privacy_sources.take() {
                        self.privacy = Some(PrivacyController::new(
                            PathBuf::from("/proc"),
                            &PathBuf::from("/sys/class/video4linux"),
                            privacy_sources,
                            self.senders.privacy.clone(),
                        ));
                    }
                }
            }
            // Separate from sysinfo's scheduler, dormant until Lua sets an interval; construction
            // detects the package manager and pushes its name immediately (ADR-0034, ADR-0134).
            Capability::Updates => {
                if self.updates.is_none() {
                    self.updates = Some(UpdatesController::new(self.senders.updates.clone()));
                }
            }
            // UPower DisplayDevice, composite across batteries; no UPower means no push
            // (ADR-0080).
            Capability::Battery => {
                if self.battery.is_none() {
                    self.battery = Some(BatteryController::new(self.connection.clone(), self.senders.battery.clone()));
                }
            }
            // Firmware > platform > raw; no device means no push (see `brightness`).
            Capability::Brightness => {
                if self.brightness.is_none() {
                    self.brightness = Some(BrightnessController::new(
                        PathBuf::from("/sys/class/backlight"),
                        self.connection.clone(),
                        self.senders.brightness.clone(),
                    ));
                }
            }
            // niri IPC via `$NIRI_SOCKET`; no implementor means no push.
            Capability::Workspaces => {
                if self.workspaces.is_none() {
                    self.workspaces = Some(WorkspacesController::new(self.senders.workspaces.clone()));
                }
            }
            // UPower supplies on_battery/energy_rate; power-profiles-daemon supplies profiles;
            // either may be missing (ADR-0053).
            Capability::Power => {
                if self.power.is_none() {
                    self.power = Some(PowerController::new(self.connection.clone(), self.senders.power.clone()));
                }
            }
            // 1Hz clock, and nothing else since ADR-0136 (ADR-0053).
            Capability::System => {
                if self.system.is_none() {
                    self.system = Some(SystemController::new(self.senders.system.clone()));
                }
            }
            // Installed `.desktop` entries; scans in background and returns before parsing starts.
            Capability::Applications => {
                if self.applications.is_none() {
                    self.applications = Some(ApplicationsController::new(
                        applications::application_dirs(
                            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
                            std::env::var("XDG_DATA_DIRS").ok(),
                            Path::new(&std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/"))),
                        ),
                        self.senders.applications.clone(),
                    ));
                }
            }
            // `files:watch` starts work; nothing is listed before it.
            Capability::Files => {
                if self.files.is_none() {
                    self.files = Some(FilesController::new(self.senders.files.clone()));
                }
            }
            // `persistent_table` opens storage on demand.
            Capability::Storage => {
                if self.storage.is_none() {
                    self.storage = Some(StorageController::new(self.senders.storage.clone()));
                }
            }
            // `session_process` declares on demand, the way `persistent_table` opens storage.
            Capability::Processes => {
                if self.processes.is_none() {
                    self.processes = Some(ProcessesController::new(self.senders.processes.clone()));
                }
            }
            Capability::Audio => self.ensure_mixer_thread(),
            // On the roster since ADR-0141. Push immediately after lazy start because a quiet
            // inhibitor watch may never speak.
            Capability::Idle => {
                if self.idle.is_none() {
                    self.idle = Some(
                        IdleController::new(self.connection.clone(), self.idle_tx.clone(), self.senders.idle.clone())
                            .await,
                    );
                }
                if let Some(idle) = &self.idle {
                    let _ = self.senders.idle.send(idle.snapshot());
                }
            }
            // `LockController` is boot-built in `main.rs` (ADR-0060); polkit starts its agent
            // there.
            Capability::Lock | Capability::Polkit => {}
        }
    }

    /// Starts the PipeWire mixer once for whichever of `audio`/`privacy` asks first.
    fn ensure_mixer_thread(&mut self) {
        if self.audio.is_some() {
            return;
        }
        let Some(privacy_tx) = self.privacy_tx.take() else { return };
        let (command_tx, command_rx) = audio::mixer::command_channel();
        let audio_tx = self.senders.audio.clone();
        std::thread::spawn(move || audio::mixer::run(audio_tx, privacy_tx, command_rx));
        self.audio = Some(command_tx);
    }

    /// Pushes one received [`Signal`] from the winning `select!` arm; `network`/`bluetooth` may
    /// await without a dropped signal.
    pub async fn push(
        &self,
        signal: Signal,
        registry: &socket::GenerationRegistry,
        generation_id: u32,
        revisions: &mut HashMap<String, u32>,
        last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    ) {
        macro_rules! push {
            ($capability:expr, $state:expr) => {
                push_snapshot(registry, generation_id, revisions, last_snapshots, $capability, $state)
            };
        }
        match signal {
            Signal::Audio(state) => push!(Capability::Audio, &state),
            // Controller owns signal semantics and answers async (ADR-0037).
            Signal::Network(signal) => {
                if let Some(network) = &self.network {
                    push!(Capability::Network, &network.handle_signal(signal).await);
                }
            }
            Signal::Bluetooth(signal) => {
                if let Some(bluetooth) = &self.bluetooth {
                    push!(Capability::Bluetooth, &bluetooth.handle_signal(signal).await);
                }
            }
            // No debounce: `build_state` already snapshots recomputed data (ADR-0031).
            Signal::Tray => {
                if let Some(tray) = &self.tray {
                    push!(Capability::Tray, &tray.build_state());
                }
            }
            // No debounce (ADR-0036).
            Signal::Mpris => {
                if let Some(mpris) = &self.mpris {
                    push!(Capability::Mpris, &mpris.build_state());
                }
            }
            // No debounce; each mutation re-derives notification state (ADR-0033).
            Signal::Notifications => {
                if let Some(notifications) = &self.notifications {
                    push!(Capability::Notifications, &notifications.build_state());
                }
            }
            // Poll task already wrote fields under its lock; clone and push (ADR-0035).
            Signal::Sysinfo => {
                if let Some(sysinfo) = &self.sysinfo {
                    push!(Capability::Sysinfo, &sysinfo.snapshot());
                }
            }
            Signal::Keyboard => {
                if let Some(keyboard) = &self.keyboard {
                    push!(Capability::Keyboard, &keyboard.snapshot());
                }
            }
            Signal::Battery => {
                if let Some(battery) = &self.battery {
                    push!(Capability::Battery, &battery.snapshot());
                }
            }
            // Only emitted when a backlight device exists (ADR-0053).
            Signal::Brightness => {
                if let Some(brightness) = &self.brightness {
                    push!(Capability::Brightness, &brightness.snapshot());
                }
            }
            // Controller already filters compositor events to real changes.
            Signal::Workspaces => {
                if let Some(workspaces) = &self.workspaces {
                    push!(Capability::Workspaces, &workspaces.snapshot());
                }
            }
            // Controller filters UPower's roughly once-per-minute EnergyRate repeats.
            Signal::Power => {
                if let Some(power) = &self.power {
                    push!(Capability::Power, &power.snapshot());
                }
            }
            Signal::Applications => {
                if let Some(applications) = &self.applications {
                    push!(Capability::Applications, &applications.snapshot());
                }
            }
            // `watch`/`unwatch` and each settled folder-change burst.
            Signal::Files => {
                if let Some(files) = &self.files {
                    push!(Capability::Files, &files.snapshot());
                }
            }
            // Every `open`/`set`, before debounced save (ADR-0136).
            Signal::Storage => {
                if let Some(storage) = &self.storage {
                    push!(Capability::Storage, &storage.snapshot());
                }
            }
            // Every declare, start, signal answered, and exit noticed.
            Signal::Processes => {
                if let Some(processes) = &self.processes {
                    push!(Capability::Processes, &processes.snapshot());
                }
            }
            // Only timer-driven capability: once per wall-clock second when epoch changes
            // (ADR-0053 decision 2).
            Signal::System => {
                if let Some(system) = &self.system {
                    push!(Capability::System, &system.snapshot());
                }
            }
            Signal::Privacy => {
                if let Some(privacy) = &self.privacy {
                    push!(Capability::Privacy, &privacy.snapshot());
                }
            }
            // Periodic checks and install progress updates.
            Signal::Updates => {
                if let Some(updates) = &self.updates {
                    push!(Capability::Updates, &updates.snapshot());
                }
            }
            // Inhibitor watch sends state directly, like `Audio`.
            Signal::Idle(state) => push!(Capability::Idle, &state),
        }
    }

    /// Routes a command to its module (ADR-0037). Optional controllers exist only after the config
    /// reads their member (ADR-0070), so missing ones call `log_unstarted`; boot-built `lock` is
    /// passed in, and read-only `battery`/`privacy`/`system` have no dispatch.
    pub async fn dispatch(&mut self, capability: Capability, envelope: &CommandEnvelope, lock: &LockController) {
        macro_rules! to {
            ($held:expr, $dispatch:path) => {
                match &$held {
                    Some(controller) => $dispatch(controller, envelope),
                    None => log_unstarted(envelope),
                }
            };
        }
        match capability {
            Capability::Network => to!(self.network, network::dispatch),
            Capability::Bluetooth => to!(self.bluetooth, bluetooth::dispatch),
            Capability::Tray => to!(self.tray, tray::dispatch),
            Capability::Notifications => to!(self.notifications, notifications::dispatch),
            Capability::Mpris => to!(self.mpris, mpris::dispatch),
            Capability::Sysinfo => to!(self.sysinfo, sysinfo::dispatch),
            Capability::Keyboard => to!(self.keyboard, keyboard::dispatch),
            Capability::Brightness => to!(self.brightness, brightness::dispatch),
            Capability::Workspaces => to!(self.workspaces, workspaces::dispatch),
            Capability::Power => to!(self.power, power::dispatch),
            Capability::Updates => to!(self.updates, updates::dispatch),
            Capability::Applications => to!(self.applications, applications::dispatch),
            Capability::Files => to!(self.files, files::dispatch),
            Capability::Storage => to!(self.storage, storage::dispatch),
            Capability::Processes => to!(self.processes, processes::dispatch),
            Capability::Audio => to!(self.audio, audio::dispatch),
            Capability::Idle => to!(self.idle, idle::dispatch),
            Capability::Lock => lock::dispatch(lock, envelope),
            // Answered in `Supervisor::dispatch_capability_command`, where its controller lives
            // beside the state push that cancel handling needs.
            Capability::Polkit => {}
            // Read-only: no action enum; a named command is malformed Renderer input.
            Capability::Battery | Capability::Privacy | Capability::System => {
                eprintln!(
                    "{capability}: read-only capability received a command from generation {}; dropping",
                    envelope.params.generation_id
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_resolves_every_roster_entry_and_nothing_else() {
        assert_eq!(Capability::from_name("audio"), Some(Capability::Audio));
        assert_eq!(Capability::from_name("polkit"), Some(Capability::Polkit));
        assert_eq!(Capability::from_name("idle"), Some(Capability::Idle), "on the roster since ADR-0141");
    }

    #[test]
    fn startable_rejects_a_name_this_supervisor_builds_nothing_for() {
        // `process` is command-addressable but never started; it must not resolve to a silent
        // start.
        assert_eq!(Capability::from_name("process"), None);
        assert_eq!(Capability::from_name("screens"), None);
        assert_eq!(Capability::from_name(""), None);
    }

    #[test]
    fn truncate_utf8_bytes_is_a_no_op_under_the_cap() {
        assert_eq!(truncate_utf8_bytes("hello", 64), "hello");
    }

    #[test]
    fn truncate_utf8_bytes_truncates_ascii_at_the_exact_cap() {
        assert_eq!(truncate_utf8_bytes("hello world", 5), "hello");
    }

    #[test]
    fn truncate_utf8_bytes_never_splits_a_multibyte_char() {
        // "héllo" -- 'é' is 2 bytes (0xc3 0xa9); a byte cap landing mid-character must back off.
        let input = "héllo";
        assert_eq!(input.len(), 6);
        // Cap of 2 bytes lands right in the middle of 'é' (byte 1 is not a char boundary).
        let truncated = truncate_utf8_bytes(input, 2);
        assert_eq!(truncated, "h");
        assert!(truncated.len() <= 2);
    }

    #[test]
    fn truncate_utf8_bytes_handles_a_cap_of_zero() {
        assert_eq!(truncate_utf8_bytes("hello", 0), "");
    }
}
