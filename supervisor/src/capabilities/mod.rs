//! Every lazily-started capability: what is running, how each one starts, where each one's signal
//! arrives, and which command goes to which (ADR-0037, ADR-0070, ADR-0076).
//!
//! `run_supervisor` was the only scope where sixteen controllers and their twenty channels
//! coexisted, five locals apiece in an 800-line function, six edits across five regions of
//! `main.rs` per new capability. Fields on [`Capabilities`] instead: start, push, accept a
//! command are three exhaustive matches over `shared::Capability`, so a new variant fails the
//! build at exactly the arms that need code.
//!
//! Not a trait, not a registry of boxed objects: controllers have genuinely different shapes
//! (`build_state` vs `snapshot` vs an `async handle_signal`, one that is a channel rather than a
//! controller at all), and `main.rs` needs the concrete types anyway. ADR-0037 decision 3 already
//! settled the dispatch as "static calls, no registry, no trait"; this is that, moved.
//!
//! **One child module per roster entry, flat.** The same name appears in `shared::Capability`, in
//! `oblisk.<name>`, and in every command's `capability` field, but these modules used to sit at
//! four depths under a `dbus/` and a `hardware/` grouped by transport, splitting `battery` and
//! `power` for no visible reason. [`read_attr`] and [`parse_bool_arg`] are the two helpers that
//! grouping shared, moved here; `polkit` moved to `crate::polkit`; `shm_icons` sits beside the two
//! capabilities it serves (ADR-0076).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::{Capability, CommandEnvelope};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::snapshot::push_snapshot;
use crate::{log_unstarted, socket};
use applications::{ApplicationsController, ApplicationsSignal};
use audio::mixer::{AudioState, VideoSourceApp};
use battery::{BatteryController, BatterySignal};
use bluetooth::{BluetoothController, BluetoothSignal};
use brightness::{BrightnessController, BrightnessSignal};
use files::{FilesController, FilesSignal};
use idle::IdleController;
use keyboard::{KeyboardController, KeyboardSignal};
use lock::LockController;
use mpris::{MprisController, MprisSignal};
use network::{NetworkController, NetworkSignal};
use notifications::{NotificationsController, NotificationsSignal};
use power::{PowerController, PowerSignal};
use privacy::{PrivacyController, PrivacySignal};
use sysinfo::{SysinfoController, SysinfoSignal};
use system::{SystemController, SystemSignal};
use tray::{TrayController, TraySignal};
use updates::{UpdatesController, UpdatesSignal};
use workspaces::{WorkspacesController, WorkspacesSignal};

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
pub mod scale;
mod shm_icons;
pub mod sysinfo;
pub mod system;
#[cfg(test)]
mod test_support;
pub mod tray;
pub mod updates;
pub mod workspaces;

/// Reads and trims one sysfs attribute file under `entry_dir`. `None` for both "file missing"
/// and any other read error: unreadable is treated as absent. Shared by `battery`/`brightness`.
pub fn read_attr(entry_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(entry_dir.join(name)).ok().map(|text| text.trim().to_string())
}

/// Shared `arguments: [en]` boolean-argument parse for `*:set_*_enabled(en)`-style write actions.
/// Reads the first argument as a JSON bool, or `None`.
pub fn parse_bool_arg(arguments: &[serde_json::Value]) -> Option<bool> {
    arguments.first()?.as_bool()
}

/// Every name a Renderer can ask this Supervisor to start: the roster, plus the one not on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startable {
    Capability(Capability),
    /// Event-shaped (ADR-0032), off the roster; a config read starts it, and it takes commands.
    Idle,
}

impl Startable {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "idle" => Some(Startable::Idle),
            other => Capability::from_name(other).map(Startable::Capability),
        }
    }
}

/// One capability signal, received but not yet acted on. The split from [`Capabilities::push`]
/// keeps the main loop correct: [`Signals::next`] only ever awaits `recv()`, so losing the
/// `tokio::select!` race drops nothing but an unstarted read, and the signal is acted on in the
/// winning arm's body, which `select!` never cancels. `network`/`bluetooth` `await` while
/// building state, so folding that into the raced future could drop a signal mid-flight.
#[derive(Debug)]
pub enum Signal {
    /// Carries its payload, not a controller name: the mixer thread sends state, not read off one.
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
    System,
    Privacy,
    Updates,
}

/// Declares every capability channel once, deriving the receiving half ([`Signals`]), the sending
/// half ([`Senders`]), the cancel-safe [`Signals::next`] that races them, and the constructor.
///
/// A macro rather than four hand-kept lists, for the reason ADR-0076 gave for making the roster an
/// enum: the failure it removes was measured, not imagined. Add a variant to `shared::Capability`
/// and exactly two things fail to compile, [`Capabilities::start`] and [`Capabilities::dispatch`].
/// Fill those in and the workspace builds clean with a capability that has a Lua member, stubs, a
/// schema-check entry, a controller and command dispatch, and no way to push a `StateSnapshot`:
/// its member reads `nil` forever, the same silent failure ADR-0076 kills.
///
/// The `without_channel` section is not decoration: every roster variant appears in one section or
/// the other, and [`every_capability_has_a_channel_row`] proves it exhaustively, so a missing row
/// is an `E0004` naming this list. `Signal` itself stays hand-written above: its variants carry a
/// real decision each, which payload travels and which collapses to a unit, plus doc comments a
/// macro row would flatten to punctuation; [`Capabilities::push`]'s exhaustive match guards it.
///
/// The chain is walked, not assumed: a new roster variant fails `start`, `dispatch` and
/// [`every_capability_has_a_channel_row`] at once; a channel row for it then fails on the missing
/// `Signal` variant; adding that variant then fails `push`. Four refusals, each naming the next
/// thing to write, where before the third onward was silence.
macro_rules! capability_channels {
    (
        channels {
            $($variant:ident => $field:ident : $payload:ty, $pattern:pat => $signal:expr;)+
        }
        without_channel { $($no_channel:ident),* $(,)? }
    ) => {
        /// The receiving half of every capability channel: what the main loop awaits.
        pub struct Signals {
            $($field: UnboundedReceiver<$payload>,)+
        }

        /// The sending half, handed to each controller as it is constructed.
        struct Senders {
            $($field: UnboundedSender<$payload>,)+
        }

        impl Senders {
            /// Every channel, both halves. One `unbounded_channel()` per roster entry that has one.
            fn channels() -> (Self, Signals) {
                // One binding per field holds both halves; each struct then takes its own via a
                // partial move, avoiding a second metavariable for the receiver.
                $(let $field = unbounded_channel();)+
                (Self { $($field: $field.0,)+ }, Signals { $($field: $field.1,)+ })
            }
        }

        impl Signals {
            /// Awaits whichever speaks first. Cancel-safe: every branch is a bare `recv()`.
            /// `None` only once every sender drops, which can't happen while [`Capabilities`] is
            /// alive; an unread capability just keeps an idle sender.
            pub async fn next(&mut self) -> Option<Signal> {
                tokio::select! {
                    $($pattern = self.$field.recv() => Some($signal),)+
                    else => None,
                }
            }
        }

        /// Never called: exists so a roster variant with no channel and no stated reason is a
        /// build failure here, not a capability that starts, accepts commands, and never answers.
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
        // Payload signal, mixer thread sends state directly (see `Signal::Audio` above).
        Audio => audio: AudioState, Some(state) => Signal::Audio(state);
        // The two that carry meaning: controller owns the signal's state semantics (ADR-0037).
        Network => network: NetworkSignal, Some(signal) => Signal::Network(signal);
        Bluetooth => bluetooth: BluetoothSignal, Some(signal) => Signal::Bluetooth(signal);
        // The rest are single-variant `Changed` signals, collapsed to a unit `Signal`.
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
    }
    // `lock` has no signal channel, deliberately: built at boot in `main.rs` since the relock
    // path commands it before any config reads (ADR-0060); it reports through `LockOutcome`
    // frames `main.rs` already handles, not a `StateSnapshot` here (ADR-0052 decision 4).
    without_channel { Lock, Polkit }
}

/// Every controller that starts on demand, plus what starting one needs. `audio` has no
/// controller: starting it spawns the PipeWire mixer thread and keeps the command channel that
/// thread reads, as an `Option` of that channel. `lock` has one, but it is built at boot in
/// `main.rs` instead, since the relock path (ADR-0060) commands it before any config has read
/// anything, so [`Capabilities::dispatch`] is handed it.
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
    audio: Option<audio::mixer::AudioCommandSender>,
    idle: Option<IdleController>,

    senders: Senders,
    /// No [`Senders`] field, not a roster entry: event-shaped (ADR-0032), never pushes a
    /// `StateSnapshot`; events go to `main.rs` directly. Kept here so the bundle stays the roster.
    idle_tx: UnboundedSender<shared::IdleEvent>,
    /// The Supervisor's system bus, shared by every controller that rides it (ADR-0034). The three
    /// session-bus capabilities open their own.
    connection: zbus::Connection,
    sound_tx: std::sync::mpsc::Sender<PathBuf>,
    /// The mixer thread's video half (ADR-0034), in `Option`s because starting either `audio`
    /// or `privacy` moves one end: the mixer thread owns the sender, `privacy` the receiver.
    video_tx: Option<UnboundedSender<Vec<VideoSourceApp>>>,
    video_sources: Option<UnboundedReceiver<Vec<VideoSourceApp>>>,
}

impl Capabilities {
    /// Builds every channel and returns the two halves. Nothing is constructed here: a controller
    /// exists only once the config reads its `oblisk` member (ADR-0070 decision 1).
    pub fn new(
        connection: zbus::Connection,
        sound_tx: std::sync::mpsc::Sender<PathBuf>,
        idle_tx: UnboundedSender<shared::IdleEvent>,
    ) -> (Self, Signals) {
        let (senders, signals) = Senders::channels();
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
            files: None,
            audio: None,
            idle: None,
            senders,
            idle_tx,
            connection,
            sound_tx,
            video_tx: Some(video_tx),
            video_sources: Some(video_sources),
        };
        (capabilities, signals)
    }

    /// The live `idle` controller, for resetting a generation's threshold registrations across
    /// an in-place reload (ADR-0006). `None` when no threshold was ever configured to reset.
    pub fn idle(&self) -> Option<&IdleController> {
        self.idle.as_ref()
    }

    /// The live `network` controller, for `secure_submit(network, connect)`: the pending connect
    /// intent is the controller's, and the plaintext secret never enters this module (ADR-0029).
    pub fn network(&self) -> Option<&NetworkController> {
        self.network.as_ref()
    }

    /// ADR-0070: the config read `oblisk.<capability>`, the first time anything in this process
    /// has. Awaited inline rather than spawned (decision 4). Re-entrant by design: every
    /// generation resends its own starts on a swap (decision 3); each arm is a no-op once its
    /// controller already exists.
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
            // Own session bus, unlike NetworkManager/BlueZ/polkit; a missing one is `inert`.
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
            // Own session bus (ADR-0033): a desktop already owning org.freedesktop.Notifications
            // degrades this to inert via RequestName's DoNotQueue.
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
            // Own session bus too (ADR-0036); `new` isn't async, it spawns discovery and returns.
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
            // Three configurable poll tasks, dormant until Lua calls sysinfo:configure (ADR-0035).
            Capability::Sysinfo => {
                if self.sysinfo.is_none() {
                    self.sysinfo = Some(SysinfoController::new(
                        PathBuf::from("/proc"),
                        PathBuf::from("/sys/class/hwmon"),
                        self.senders.sysinfo.clone(),
                    ));
                }
            }
            // Missing KbdBacklight gives -1; missing lock source gives `false` (ADR-0034).
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
            // /dev/videoN open/close via inotify plus a /proc fd-scan, enriched by video_sources
            // (ADR-0034).
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
            // Separate from sysinfo's scheduler, dormant until Lua sets an interval (ADR-0034).
            // Detects this machine's package manager on the way up and pushes its name straight
            // away, so a config learns there is nothing to check here without waiting for a
            // failed check to tell it (ADR-0134).
            Capability::Updates => {
                if self.updates.is_none() {
                    self.updates = Some(UpdatesController::new(self.senders.updates.clone()));
                }
            }
            // UPower's DisplayDevice, composite across every battery; no UPower means never
            // pushes (ADR-0080, § 2.2).
            Capability::Battery => {
                if self.battery.is_none() {
                    self.battery = Some(BatteryController::new(self.connection.clone(), self.senders.battery.clone()));
                }
            }
            // Ranked firmware over platform over raw; no device found means it never pushes,
            // see `brightness`'s module doc.
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
            // profiles; either can be missing (§ 2.13, ADR-0053).
            Capability::Power => {
                if self.power.is_none() {
                    self.power = Some(PowerController::new(self.connection.clone(), self.senders.power.clone()));
                }
            }
            // The 1 Hz clock plus persisted state.json (ADR-0053, § 2.11).
            Capability::System => {
                if self.system.is_none() {
                    self.system = Some(SystemController::new(
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/"))),
                        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
                        self.senders.system.clone(),
                    ));
                }
            }
            // The installed `.desktop` entries; scans in background, returns before parsing starts.
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
            // Nothing to list until a config names a folder: `files:watch` is what starts work.
            Capability::Files => {
                if self.files.is_none() {
                    self.files = Some(FilesController::new(self.senders.files.clone()));
                }
            }
            Capability::Audio => self.ensure_mixer_thread(),
            // Not owned here: `LockController` is built at boot in `main.rs` (ADR-0060), so this
            // read is free. `polkit`'s start is its agent registration, in `main.rs`'s arm.
            Capability::Lock | Capability::Polkit => {}
        }
    }

    /// `idle`'s start, off [`Capabilities::start`] since it isn't a roster variant (ADR-0032);
    /// reached only once a Renderer connects, so notify setup's `spawn_blocking` task can't block.
    pub async fn start_idle(&mut self) {
        if self.idle.is_none() {
            self.idle = Some(IdleController::new(self.connection.clone(), self.idle_tx.clone()).await);
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

    /// Turns one received [`Signal`] into its snapshot push; runs in the winning `select!` arm's
    /// body, never raced, so `network`/`bluetooth` can `await` without a dropped signal.
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
            // No debounce (ADR-0031): `build_state` snapshots already-live data the forwarder
            // task recomputed before sending.
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
            // No debounce (ADR-0033): every mutation fully re-derives notifications state first.
            Signal::Notifications => {
                if let Some(notifications) = &self.notifications {
                    push!(Capability::Notifications, &notifications.build_state());
                }
            }
            // No debounce: whichever poll task fired already wrote its field(s) under its own
            // lock (ADR-0035); this just clones and pushes.
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
            // Fires only when a backlight device was found (ADR-0053).
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
            // UPower re-emits EnergyRate roughly once a minute; the controller filters that first.
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
            // Fires on `watch`/`unwatch` and after every settled burst of folder changes.
            Signal::Files => {
                if let Some(files) = &self.files {
                    push!(Capability::Files, &files.snapshot());
                }
            }
            // The only capability pushing on a timer, once per wall-clock second, emitted only
            // when the epoch second actually changed (ADR-0053 decision 2).
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

    /// Routes one command to the capability it names (ADR-0037: each module owns its own
    /// action match, argument parse, and write-action spawn). Every arm but `lock` reads an
    /// `Option`, since a controller exists only once the config reads its member (ADR-0070),
    /// see `log_unstarted`. `lock` is passed in since it's built at boot; `battery` and `privacy`
    /// are read-only and have no `dispatch` at all.
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
            Capability::Audio => to!(self.audio, audio::dispatch),
            Capability::System => to!(self.system, system::dispatch),
            Capability::Lock => lock::dispatch(lock, envelope),
            // Answered in `Supervisor::dispatch_capability_command` before this is reached: its
            // controller lives there, beside the state push a cancel needs.
            Capability::Polkit => {}
            // Read-only (§ 2): no action enum; a command naming one is a Renderer sending garbage.
            Capability::Battery | Capability::Privacy => {
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
            Some(idle) => idle::dispatch(idle, envelope),
            None => log_unstarted(envelope),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startable_resolves_the_roster_plus_the_one_name_deliberately_off_it() {
        assert_eq!(Startable::from_name("audio"), Some(Startable::Capability(Capability::Audio)));
        assert_eq!(Startable::from_name("polkit"), Some(Startable::Capability(Capability::Polkit)));
        assert_eq!(Startable::from_name("idle"), Some(Startable::Idle), "event-shaped, so off the roster (ADR-0032)");
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
