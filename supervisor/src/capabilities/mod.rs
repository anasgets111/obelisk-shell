//! Every lazily-started capability: what is running, how each one starts, where each one's signal
//! arrives, and which command goes to which (ADR-0037, docs/adr/0070, docs/adr/0076).
//!
//! This exists because `run_supervisor` was the only scope where sixteen controllers and their
//! twenty channels coexisted, which made every one of them five locals in an 800-line function
//! and made adding a capability six edits across five regions of `main.rs`. They are fields on
//! [`Capabilities`] instead, so the three things a capability does -- start, push, accept a
//! command -- are three methods here rather than three arms there.
//!
//! Three matches over `shared::Capability`, all exhaustive, so a new roster variant fails the
//! build at exactly the arms that need code and nowhere else.
//!
//! Not a trait and not a registry of boxed objects. Controllers have genuinely different shapes
//! (`build_state` vs `snapshot` vs an `async handle_signal`, one that is a channel rather than a
//! controller at all), and `main.rs` needs the concrete types anyway; ADR-0037 decision 3 already
//! settled that the dispatch is "static calls, no registry, no trait". This is that, moved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::{Capability, CommandEnvelope};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::snapshot::push_snapshot;
use crate::{applications, audio, dbus, hardware, lock, log_unstarted, socket, updates, workspaces};
use crate::applications::{ApplicationsController, ApplicationsSignal};
use crate::audio::mixer::{AudioState, VideoSourceApp};
use crate::hardware::battery::{BatteryController, BatterySignal};
use crate::dbus::bluetooth::{BluetoothController, BluetoothSignal};
use crate::hardware::brightness::{BrightnessController, BrightnessSignal};
use crate::hardware::idle::IdleController;
use crate::hardware::keyboard::{KeyboardController, KeyboardSignal};
use crate::lock::LockController;
use crate::dbus::mpris::{MprisController, MprisSignal};
use crate::dbus::network::{NetworkController, NetworkSignal};
use crate::dbus::notifications::{NotificationsController, NotificationsSignal};
use crate::dbus::power::{PowerController, PowerSignal};
use crate::privacy::{PrivacyController, PrivacySignal};
use crate::hardware::sysinfo::{SysinfoController, SysinfoSignal};
use crate::system::{SystemController, SystemSignal};
use crate::dbus::tray::{TrayController, TraySignal};
use crate::updates::{UpdatesController, UpdatesSignal};
use crate::workspaces::{WorkspacesController, WorkspacesSignal};

/// Every name a Renderer can ask this Supervisor to start: the roster, plus the two that are
/// deliberately not on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startable {
    Capability(Capability),
    /// Event-shaped rather than snapshot state (ADR-0032), so it is not a roster entry -- but a
    /// config read still starts it, and it still takes commands.
    Idle,
    /// The one name that arrives from a `secure_submit` declaring it rather than from a
    /// capability read (docs/adr/0070 decision 5). Start only; it has no commands.
    Polkit,
}

impl Startable {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "idle" => Some(Startable::Idle),
            "polkit" => Some(Startable::Polkit),
            other => Capability::from_name(other).map(Startable::Capability),
        }
    }
}

/// One capability signal, received but not yet acted on.
///
/// The split from [`Capabilities::push`] is what keeps the main loop correct. [`Signals::next`]
/// only ever awaits `recv()`, so losing the `tokio::select!` race drops nothing but an unstarted
/// read; acting on the signal happens in the winning arm's body, which `select!` never cancels.
/// Two capabilities (`network`, `bluetooth`) `await` while building their state, and folding that
/// await into the raced future would let a busier branch discard a signal mid-flight.
#[derive(Debug)]
pub enum Signal {
    /// Carries its payload rather than naming a controller: the mixer thread sends state, it is
    /// not read back off one.
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
    System,
    Privacy,
    Updates,
}

/// The receiving half of every capability channel: what the main loop awaits.
pub struct Signals {
    audio: UnboundedReceiver<AudioState>,
    network: UnboundedReceiver<NetworkSignal>,
    bluetooth: UnboundedReceiver<BluetoothSignal>,
    tray: UnboundedReceiver<TraySignal>,
    mpris: UnboundedReceiver<MprisSignal>,
    notifications: UnboundedReceiver<NotificationsSignal>,
    sysinfo: UnboundedReceiver<SysinfoSignal>,
    keyboard: UnboundedReceiver<KeyboardSignal>,
    battery: UnboundedReceiver<BatterySignal>,
    brightness: UnboundedReceiver<BrightnessSignal>,
    workspaces: UnboundedReceiver<WorkspacesSignal>,
    power: UnboundedReceiver<PowerSignal>,
    applications: UnboundedReceiver<ApplicationsSignal>,
    system: UnboundedReceiver<SystemSignal>,
    privacy: UnboundedReceiver<PrivacySignal>,
    updates: UnboundedReceiver<UpdatesSignal>,
}

impl Signals {
    /// Awaits whichever capability speaks first. Cancel-safe: every branch is a bare `recv()`.
    ///
    /// `None` only once every sender is dropped, which cannot happen while [`Capabilities`] is
    /// alive -- it holds one of each. The main loop's `select!` therefore never disables this
    /// branch, and a capability the config never reads simply keeps a sender nobody sends on.
    pub async fn next(&mut self) -> Option<Signal> {
        // Single-variant `Changed` signals collapse to a unit `Signal`; the two that carry
        // meaning (`network`, `bluetooth`) and the one that carries a payload (`audio`) keep it.
        tokio::select! {
            Some(state) = self.audio.recv() => Some(Signal::Audio(state)),
            Some(signal) = self.network.recv() => Some(Signal::Network(signal)),
            Some(signal) = self.bluetooth.recv() => Some(Signal::Bluetooth(signal)),
            Some(TraySignal::RegistryChanged) = self.tray.recv() => Some(Signal::Tray),
            Some(MprisSignal::Changed) = self.mpris.recv() => Some(Signal::Mpris),
            Some(NotificationsSignal::Changed) = self.notifications.recv() => Some(Signal::Notifications),
            Some(SysinfoSignal::Changed) = self.sysinfo.recv() => Some(Signal::Sysinfo),
            Some(KeyboardSignal::Changed) = self.keyboard.recv() => Some(Signal::Keyboard),
            Some(BatterySignal::Changed) = self.battery.recv() => Some(Signal::Battery),
            Some(BrightnessSignal::Changed) = self.brightness.recv() => Some(Signal::Brightness),
            Some(WorkspacesSignal::Changed) = self.workspaces.recv() => Some(Signal::Workspaces),
            Some(PowerSignal::Changed) = self.power.recv() => Some(Signal::Power),
            Some(ApplicationsSignal::Changed) = self.applications.recv() => Some(Signal::Applications),
            Some(SystemSignal::Changed) = self.system.recv() => Some(Signal::System),
            Some(PrivacySignal::Changed) = self.privacy.recv() => Some(Signal::Privacy),
            Some(UpdatesSignal::Changed) = self.updates.recv() => Some(Signal::Updates),
            else => None,
        }
    }
}

/// The sending half, handed to each controller as it is constructed.
struct Senders {
    audio: UnboundedSender<AudioState>,
    network: UnboundedSender<NetworkSignal>,
    bluetooth: UnboundedSender<BluetoothSignal>,
    tray: UnboundedSender<TraySignal>,
    mpris: UnboundedSender<MprisSignal>,
    notifications: UnboundedSender<NotificationsSignal>,
    sysinfo: UnboundedSender<SysinfoSignal>,
    keyboard: UnboundedSender<KeyboardSignal>,
    battery: UnboundedSender<BatterySignal>,
    brightness: UnboundedSender<BrightnessSignal>,
    workspaces: UnboundedSender<WorkspacesSignal>,
    power: UnboundedSender<PowerSignal>,
    applications: UnboundedSender<ApplicationsSignal>,
    system: UnboundedSender<SystemSignal>,
    privacy: UnboundedSender<PrivacySignal>,
    updates: UnboundedSender<UpdatesSignal>,
    idle: UnboundedSender<shared::IdleEvent>,
}

/// Every controller that starts on demand, plus what starting one needs.
///
/// `audio` has no controller: starting it spawns the PipeWire mixer thread and keeps the command
/// channel that thread reads, so it is an `Option` of that channel. `lock` has one, but it is
/// built at boot in `main.rs` rather than here -- the Supervisor's own relock path (docs/adr/0060)
/// commands it before any config has read anything -- so [`Capabilities::dispatch`] is handed it.
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
    audio: Option<audio::mixer::AudioCommandSender>,
    idle: Option<IdleController>,

    senders: Senders,
    /// The Supervisor's system bus, shared by every controller that rides it (ADR-0034). The three
    /// session-bus capabilities open their own.
    connection: zbus::Connection,
    sound_tx: std::sync::mpsc::Sender<PathBuf>,
    /// The mixer thread's video half (docs/adr/0034), in `Option`s because starting either `audio`
    /// or `privacy` moves one end: the mixer thread owns the sender, `privacy` the receiver.
    video_tx: Option<UnboundedSender<Vec<VideoSourceApp>>>,
    video_sources: Option<UnboundedReceiver<Vec<VideoSourceApp>>>,
}

impl Capabilities {
    /// Builds every channel and returns the two halves. Nothing is constructed here: a controller
    /// exists only once the config reads its `oblisk` member (docs/adr/0070 decision 1).
    pub fn new(
        connection: zbus::Connection,
        sound_tx: std::sync::mpsc::Sender<PathBuf>,
        idle_tx: UnboundedSender<shared::IdleEvent>,
    ) -> (Self, Signals) {
        let (audio_tx, audio) = unbounded_channel();
        let (network_tx, network) = unbounded_channel();
        let (bluetooth_tx, bluetooth) = unbounded_channel();
        let (tray_tx, tray) = unbounded_channel();
        let (mpris_tx, mpris) = unbounded_channel();
        let (notifications_tx, notifications) = unbounded_channel();
        let (sysinfo_tx, sysinfo) = unbounded_channel();
        let (keyboard_tx, keyboard) = unbounded_channel();
        let (battery_tx, battery) = unbounded_channel();
        let (brightness_tx, brightness) = unbounded_channel();
        let (workspaces_tx, workspaces) = unbounded_channel();
        let (power_tx, power) = unbounded_channel();
        let (applications_tx, applications) = unbounded_channel();
        let (system_tx, system) = unbounded_channel();
        let (privacy_tx, privacy) = unbounded_channel();
        let (updates_tx, updates) = unbounded_channel();
        let (video_tx, video_sources) = unbounded_channel();

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
            audio: None,
            idle: None,
            senders: Senders {
                audio: audio_tx,
                network: network_tx,
                bluetooth: bluetooth_tx,
                tray: tray_tx,
                mpris: mpris_tx,
                notifications: notifications_tx,
                sysinfo: sysinfo_tx,
                keyboard: keyboard_tx,
                battery: battery_tx,
                brightness: brightness_tx,
                workspaces: workspaces_tx,
                power: power_tx,
                applications: applications_tx,
                system: system_tx,
                privacy: privacy_tx,
                updates: updates_tx,
                idle: idle_tx,
            },
            connection,
            sound_tx,
            video_tx: Some(video_tx),
            video_sources: Some(video_sources),
        };
        let signals = Signals {
            audio,
            network,
            bluetooth,
            tray,
            mpris,
            notifications,
            sysinfo,
            keyboard,
            battery,
            brightness,
            workspaces,
            power,
            applications,
            system,
            privacy,
            updates,
        };
        (capabilities, signals)
    }

    /// The live `idle` controller, for the one thing `main.rs` does with it directly: resetting a
    /// generation's threshold registrations across an in-place reload (ADR-0006). `None` when the
    /// config never asked for a threshold, in which case there is nothing registered to reset.
    pub fn idle(&self) -> Option<&IdleController> {
        self.idle.as_ref()
    }

    /// The live `network` controller, for `main.rs`'s `secure_submit(network, connect)` arm: the
    /// pending connect intent it consumes is the controller's, and the plaintext secret that
    /// completes it never enters this module (ADR-0029).
    pub fn network(&self) -> Option<&NetworkController> {
        self.network.as_ref()
    }

    /// docs/adr/0070: the config read `oblisk.<capability>` and this is the first time anything in
    /// this process has. Awaited by the caller inline rather than spawned -- decision 4 says why.
    ///
    /// Re-entrant by design: every generation sends its own starts, so a swap re-sends every name
    /// the previous one read (decision 3). Each arm is a no-op once its controller exists.
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
            // The tray host is a session-bus protocol, unlike NetworkManager/BlueZ/polkit, so it
            // needs its own connection. A missing session bus degrades to `TrayController::inert`.
            Capability::Tray => {
                if self.tray.is_none() {
                    self.tray = Some(match zbus::Connection::session().await {
                        Ok(bus) => TrayController::new(bus, self.senders.tray.clone()).await,
                        Err(err) => {
                            eprintln!(
                                "tray: failed to connect to the session bus; tray host disabled for this run: {err}"
                            );
                            TrayController::inert(self.senders.tray.clone())
                        }
                    });
                }
            }
            // Notifications get their own session-bus connection (ADR-0033): a real desktop may
            // already own org.freedesktop.Notifications, degrading to inert via RequestName's
            // DoNotQueue.
            Capability::Notifications => {
                if self.notifications.is_none() {
                    self.notifications = Some(match zbus::Connection::session().await {
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
                }
            }
            // MPRIS gets its own session-bus connection too (ADR-0036). `MprisController::new` is
            // not async: it spawns discovery and returns.
            Capability::Mpris => {
                if self.mpris.is_none() {
                    self.mpris = Some(match zbus::Connection::session().await {
                        Ok(bus) => MprisController::new(bus, self.senders.mpris.clone()),
                        Err(err) => {
                            eprintln!(
                                "mpris: failed to connect to the session bus; player discovery disabled for this run: {err}"
                            );
                            MprisController::inert(self.senders.mpris.clone())
                        }
                    });
                }
            }
            // Three independently-configurable poll tasks, still dormant after this until Lua
            // calls sysinfo:configure (docs/adr/0035).
            Capability::Sysinfo => {
                if self.sysinfo.is_none() {
                    self.sysinfo = Some(SysinfoController::new(
                        PathBuf::from("/proc"),
                        PathBuf::from("/sys/class/hwmon"),
                        self.senders.sysinfo.clone(),
                    ));
                }
            }
            // A missing KbdBacklight degrades in place to `backlight_pct: -1`, a missing lock
            // source to `false` (docs/adr/0034).
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
            // Kernel-level /dev/videoN open/close via inotify plus a /proc fd-scan, enriched by
            // the mixer thread's video_sources (docs/adr/0034).
            Capability::Privacy => {
                if self.privacy.is_none() {
                    self.ensure_mixer_thread();
                    if let Some(video_sources) = self.video_sources.take() {
                        self.privacy = Some(PrivacyController::new(
                            PathBuf::from("/proc"),
                            &PathBuf::from("/sys/class/video4linux"),
                            video_sources,
                            self.senders.privacy.clone(),
                        ));
                    }
                }
            }
            // alpm-based Arch update checking, separate from sysinfo's own scheduler and equally
            // dormant until Lua sets an interval (docs/adr/0034).
            Capability::Updates => {
                if self.updates.is_none() {
                    self.updates = Some(UpdatesController::new(
                        PathBuf::from("/etc/pacman.conf"),
                        PathBuf::from("/var/lib/pacman"),
                        self.senders.updates.clone(),
                    ));
                }
            }
            // The root is a parameter, not a constant, so device selection is testable against a
            // fixture directory (docs/adr/0053, § 2.2).
            Capability::Battery => {
                if self.battery.is_none() {
                    self.battery = Some(BatteryController::new(
                        PathBuf::from("/sys/class/power_supply"),
                        self.senders.battery.clone(),
                    ));
                }
            }
            // Ranked firmware over platform over raw. No device found means it never pushes --
            // see `hardware::brightness`'s module doc.
            Capability::Brightness => {
                if self.brightness.is_none() {
                    self.brightness = Some(BrightnessController::new(
                        PathBuf::from("/sys/class/backlight"),
                        self.connection.clone(),
                        self.senders.brightness.clone(),
                    ));
                }
            }
            // niri's IPC stream via $NIRI_SOCKET. A session with no implementor never pushes.
            Capability::Workspaces => {
                if self.workspaces.is_none() {
                    self.workspaces = Some(WorkspacesController::new(self.senders.workspaces.clone()));
                }
            }
            // UPower for on_battery/energy_rate, power-profiles-daemon for active_profile/
            // profiles. Either can be missing (§ 2.13, docs/adr/0053).
            Capability::Power => {
                if self.power.is_none() {
                    self.power = Some(PowerController::new(self.connection.clone(), self.senders.power.clone()));
                }
            }
            // The 1 Hz clock plus persisted state.json (docs/adr/0053, § 2.11).
            Capability::System => {
                if self.system.is_none() {
                    self.system = Some(SystemController::new(
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/"))),
                        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
                        self.senders.system.clone(),
                    ));
                }
            }
            // The installed `.desktop` entries. Scans in the background from construction, so
            // this arm returns before the first entry is parsed.
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
            Capability::Audio => self.ensure_mixer_thread(),
            // Not a controller this owns: `LockController` is a state holder built at boot in
            // `main.rs` (docs/adr/0060), so its read costs nothing here.
            Capability::Lock => {}
        }
    }

    /// `idle`'s start, kept off [`Capabilities::start`] because `idle` is not a roster variant
    /// (ADR-0032). Reached only once a Renderer is connected, so notify setup's own
    /// `spawn_blocking` task (`hardware::idle`'s module doc) cannot block the control socket.
    pub async fn start_idle(&mut self) {
        if self.idle.is_none() {
            self.idle = Some(IdleController::new(self.connection.clone(), self.senders.idle.clone()).await);
        }
    }

    /// Starts the PipeWire mixer thread once, for whichever of `audio`/`privacy` asked first.
    fn ensure_mixer_thread(&mut self) {
        if self.audio.is_some() {
            return;
        }
        let Some(video_tx) = self.video_tx.take() else { return };
        let (command_tx, command_rx) = audio::mixer::command_channel();
        let audio_tx = self.senders.audio.clone();
        std::thread::spawn(move || audio::mixer::run(audio_tx, video_tx, command_rx));
        self.audio = Some(command_tx);
    }

    /// Turns one received [`Signal`] into the snapshot push it stands for.
    ///
    /// Runs in the winning `select!` arm's body, never as a raced future, which is what lets
    /// `network` and `bluetooth` `await` here without risking a dropped signal.
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
            // The controller owns the signal's state semantics (ADR-0037), and answers async.
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
            // No debounce (docs/adr/0031): `build_state` is a synchronous snapshot of already-live
            // data the forwarder task recomputed before sending.
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
            // No debounce (ADR-0033): every mutation fully re-derives notifications state before
            // signaling.
            Signal::Notifications => {
                if let Some(notifications) = &self.notifications {
                    push!(Capability::Notifications, &notifications.build_state());
                }
            }
            // No debounce: whichever of the three poll tasks fired already wrote its field(s)
            // under its own lock (docs/adr/0035); this just clones and pushes.
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
            // Fires only when a backlight device was found (docs/adr/0053).
            Signal::Brightness => {
                if let Some(brightness) = &self.brightness {
                    push!(Capability::Brightness, &brightness.snapshot());
                }
            }
            // The controller filters the compositor's stream down to real changes already.
            Signal::Workspaces => {
                if let Some(workspaces) = &self.workspaces {
                    push!(Capability::Workspaces, &workspaces.snapshot());
                }
            }
            // UPower re-emits EnergyRate roughly once a minute; the controller filters that to
            // real changes first.
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
            // The only capability pushing on a timer, once per wall-clock second (docs/adr/0053
            // decision 2) -- emitted only when the epoch second actually changed.
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
            // Fires after a periodic check and on install progress updates.
            Signal::Updates => {
                if let Some(updates) = &self.updates {
                    push!(Capability::Updates, &updates.snapshot());
                }
            }
        }
    }

    /// Routes one command to the capability it names (ADR-0037: each module owns its own action
    /// match, argument parse, and write-action spawn).
    ///
    /// Every arm but `lock` reads an `Option`, because a controller exists only once the config
    /// has read its member (docs/adr/0070) -- see `log_unstarted`. `lock` is passed in because it
    /// is built at boot rather than started; `battery`, `system` and `privacy` are read-only and
    /// have no `dispatch` at all.
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
            Capability::Network => to!(self.network, dbus::network::dispatch),
            Capability::Bluetooth => to!(self.bluetooth, dbus::bluetooth::dispatch),
            Capability::Tray => to!(self.tray, dbus::tray::dispatch),
            Capability::Notifications => to!(self.notifications, dbus::notifications::dispatch),
            Capability::Mpris => to!(self.mpris, dbus::mpris::dispatch),
            Capability::Sysinfo => to!(self.sysinfo, hardware::sysinfo::dispatch),
            Capability::Keyboard => to!(self.keyboard, hardware::keyboard::dispatch),
            Capability::Brightness => to!(self.brightness, hardware::brightness::dispatch),
            Capability::Workspaces => to!(self.workspaces, workspaces::dispatch),
            Capability::Power => to!(self.power, dbus::power::dispatch),
            Capability::Updates => to!(self.updates, updates::dispatch),
            Capability::Applications => to!(self.applications, applications::dispatch),
            Capability::Audio => to!(self.audio, audio::dispatch),
            Capability::Lock => lock::dispatch(lock, envelope),
            // Read-only (§ 2): no action enum, so a command naming one is a Renderer that sent
            // something no Lua binding can produce.
            Capability::Battery | Capability::System | Capability::Privacy => {
                eprintln!(
                    "{capability}: read-only capability received a command from generation {}; dropping",
                    envelope.params.generation_id
                )
            }
        }
    }

    /// `idle`'s dispatch, off [`Capabilities::dispatch`] for the same reason its start is.
    pub fn dispatch_idle(&self, envelope: &CommandEnvelope) {
        match &self.idle {
            Some(idle) => hardware::idle::dispatch(idle, envelope),
            None => log_unstarted(envelope),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startable_resolves_the_roster_plus_the_two_names_deliberately_off_it() {
        assert_eq!(Startable::from_name("audio"), Some(Startable::Capability(Capability::Audio)));
        assert_eq!(Startable::from_name("workspaces"), Some(Startable::Capability(Capability::Workspaces)));
        assert_eq!(Startable::from_name("idle"), Some(Startable::Idle), "event-shaped, so off the roster (ADR-0032)");
        assert_eq!(
            Startable::from_name("polkit"),
            Some(Startable::Polkit),
            "arrives from a secure_submit, not a read (docs/adr/0070 decision 5)"
        );
    }

    #[test]
    fn startable_rejects_a_name_this_supervisor_builds_nothing_for() {
        // `process` is addressable by commands but is never started, so it must miss here rather
        // than resolve to something that would silently accept a start.
        assert_eq!(Startable::from_name("process"), None);
        assert_eq!(Startable::from_name("screens"), None);
        assert_eq!(Startable::from_name(""), None);
    }
}
