//! Keyboard layout half of `oblisk.keyboard` (ADR-0034): a deliberately narrow
//! [`CompositorLink`] trait, picked at startup by probing `$HYPRLAND_INSTANCE_SIGNATURE`/
//! `$NIRI_SOCKET` (the env vars each compositor itself sets for every session process -- no
//! socket probing). Niri's implementor is live-tested on this dev machine (which runs niri);
//! Hyprland's implementor is built to its documented real IPC protocol but **not** independently
//! live-verified in this session -- ADR-0034 explicitly defers that to the user's own Hyprland
//! machine. Neither env var set: `active_layout` degrades to unavailable (empty string,
//! `active_layout_index`/`layout_count` at `0`), the same "degrade gracefully, don't fake
//! success" posture as every other missing-capability path in this codebase.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use super::controller::{KeyboardSignal, KeyboardState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorKind {
    Hyprland,
    Niri,
}

/// Scope is exactly what layout needs today, not widened to guess a future workspace adaptor's
/// eventual method surface (ADR-0034: the same speculative-generality call as this codebase's
/// "no cross-controller adapter trait" decision, one level down). Methods are synchronous,
/// fire-and-forget for `switch_layout` -- the real state update flows back through the
/// implementor's own event stream into `KeyboardState`, not a return value here, the same
/// "state changes flow through the signal, not the write call" shape `KeyboardController::
/// set_backlight` already established. Not `async fn` (which would make `Box<dyn CompositorLink>`
/// non-object-safe without a new dependency): each implementor spawns its own background
/// thread/task for the real I/O and returns immediately.
pub trait CompositorLink: Send + Sync {
    fn kind(&self) -> CompositorKind;
    fn switch_layout(&self, index: usize);
}

/// Probes the env vars each compositor sets for every process in its own session -- confirmed
/// real, not assumed: `NIRI_SOCKET` is genuinely set in this dev machine's own environment.
/// Hyprland checked first: a session that somehow has both set (unlikely) picks the one the ADR
/// lists first, an arbitrary but harmless tie-break since real sessions only ever run one
/// compositor.
pub fn detect_compositor() -> Option<CompositorKind> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        Some(CompositorKind::Hyprland)
    } else if std::env::var_os("NIRI_SOCKET").is_some() {
        Some(CompositorKind::Niri)
    } else {
        None
    }
}

fn apply_niri_layout(state: &Arc<Mutex<KeyboardState>>, names: &[String], idx: u8) {
    let mut guard = state.lock().unwrap();
    guard.active_layout = names.get(idx as usize).cloned().unwrap_or_default();
    guard.active_layout_index = u32::from(idx);
    guard.layout_count = names.len() as u32;
}

pub struct NiriLink;

impl NiriLink {
    /// Connects to `$NIRI_SOCKET` (`niri_ipc::socket::Socket::connect`) and spawns the
    /// event-stream reader on its own OS thread -- `Socket` is a blocking `std::net::UnixStream`
    /// wrapper, not `tokio`-aware, matching `audio::mixer::run`'s own "own OS thread, not a
    /// tokio task" precedent for a blocking event loop. `Request::EventStream`'s own documented
    /// contract ("the event stream will always give you the full current state up-front... you
    /// do not need to separately send `Request::KeyboardLayouts`") means the very first relevant
    /// event this loop sees already carries the full initial state -- no separate startup query.
    /// Degrades to `None` (logged) if the socket can't be connected to at all.
    pub fn new(state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) -> Option<Self> {
        let mut socket = match niri_ipc::socket::Socket::connect() {
            Ok(socket) => socket,
            Err(err) => {
                eprintln!("keyboard: failed to connect to the niri IPC socket; layout reporting disabled for this run: {err}");
                return None;
            }
        };
        match socket.send(niri_ipc::Request::EventStream) {
            Ok(Ok(niri_ipc::Response::Handled)) => {}
            Ok(Ok(_)) => {
                eprintln!("keyboard: unexpected reply to niri EventStream request; layout reporting disabled for this run");
                return None;
            }
            Ok(Err(msg)) => {
                eprintln!("keyboard: niri EventStream request failed: {msg}");
                return None;
            }
            Err(err) => {
                eprintln!("keyboard: failed to send the niri EventStream request: {err}");
                return None;
            }
        }

        std::thread::spawn(move || {
            let mut read_event = socket.read_events();
            let mut names: Vec<String> = Vec::new();
            loop {
                let event = match read_event() {
                    Ok(event) => event,
                    Err(err) => {
                        eprintln!("keyboard: niri event stream ended; layout will no longer update: {err}");
                        return;
                    }
                };
                let changed = match event {
                    niri_ipc::Event::KeyboardLayoutsChanged { keyboard_layouts } => {
                        names = keyboard_layouts.names;
                        apply_niri_layout(&state, &names, keyboard_layouts.current_idx);
                        true
                    }
                    niri_ipc::Event::KeyboardLayoutSwitched { idx } => {
                        apply_niri_layout(&state, &names, idx);
                        true
                    }
                    _ => false,
                };
                if changed && events.send(KeyboardSignal::Changed).is_err() {
                    return;
                }
            }
        });

        Some(Self)
    }
}

impl CompositorLink for NiriLink {
    fn kind(&self) -> CompositorKind {
        CompositorKind::Niri
    }

    /// A fresh connection per call (mirrors `idle::inhibit::LiveInhibit`'s own "rebuild the
    /// proxy per call, don't cache a fallible connection" precedent) -- `read_events` consumes
    /// and shuts down the write half of the event-stream socket, so that connection genuinely
    /// cannot also send this write; a fresh one is required, not a caching opportunity missed.
    fn switch_layout(&self, index: usize) {
        // `index` arrives from an untrusted JSON-RPC command over the renderer IPC boundary
        // (`parse_switch_layout_args` accepts any non-negative `u64`); niri's own wire protocol
        // takes a `u8`, so an out-of-range value is rejected here rather than silently truncated
        // (e.g. 256 -> 0) into a switch to the wrong layout (Correctness review).
        let Ok(index) = u8::try_from(index) else {
            eprintln!("keyboard: switch_layout index {index} is out of range for niri (must fit in a u8); ignored");
            return;
        };
        std::thread::spawn(move || {
            let mut socket = match niri_ipc::socket::Socket::connect() {
                Ok(socket) => socket,
                Err(err) => {
                    eprintln!("keyboard: failed to connect to the niri IPC socket for switch_layout: {err}");
                    return;
                }
            };
            let request = niri_ipc::Request::Action(niri_ipc::Action::SwitchLayout { layout: niri_ipc::LayoutSwitchTarget::Index(index) });
            if let Err(err) = socket.send(request) {
                eprintln!("keyboard: niri SwitchLayout request failed: {err}");
            }
        });
    }
}

/// Hyprland's real IPC protocol (documented, not reverse-engineered from scratch): two Unix
/// sockets per instance under `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/` --
/// `.socket2.sock` pushes newline-terminated `event>>payload` lines (`activelayout>>keyboard
/// name,layout name` is the one this cares about, used only as a "something changed" trigger,
/// matching ADR-0034's own description), `.socket.sock` accepts plain-text commands
/// (`hyprctl`'s own transport) for the actual read (`devices -j`) and write
/// (`switchxkblayout <device> <index>`).
///
/// **Not independently live-verified in this session** -- this dev machine runs niri, not
/// Hyprland; ADR-0034 explicitly defers Hyprland's live verification to the user's own machine.
/// Built against Hyprland's real, documented socket paths and `hyprctl -j devices` JSON shape,
/// the same rigor as every other decision in this ADR, just without this session's own live
/// confirmation step.
pub struct HyprlandLink {
    signature: String,
    /// The primary keyboard's real device name (Hyprland's own `keyboards[].name`, e.g.
    /// `"at-translated-set-2-keyboard"`), cached from the most recent resync -- real hyprctl
    /// syntax is `hyprctl switchxkblayout <device> <id>`, a genuine device name, not a wildcard
    /// keyword (no `all`/`current` special-case is documented for this dispatcher, so none is
    /// assumed here). `None` until the first successful resync; `switch_layout` is a no-op until
    /// then, since there's nothing yet to address the command to.
    device_name: Arc<Mutex<Option<String>>>,
}

/// One entry of `hyprctl -j devices`'s `"keyboards"` array, only the fields this needs.
/// `active_keymap` is the human-readable name of the currently active layout (e.g.
/// `"English (US)"`); `layout` is the XKB-code comma list of every *configured* layout (e.g.
/// `"us,ara"`) -- used only for a count, since matching `active_keymap`'s human name back to a
/// position in the XKB-code list needs a code->name table Hyprland's own JSON doesn't provide,
/// unlike niri's `KeyboardLayouts.names` (a direct list of human names with an index). Documented
/// limitation: `active_layout_index` stays `0` for Hyprland until this can be verified/refined
/// against a real Hyprland session. `main` marks the primary keyboard (ADR-0034: "single primary
/// device") -- selected in preference to just taking the array's first entry, since a session
/// with more than one attached keyboard (a laptop's built-in plus an external USB one) has no
/// guaranteed ordering otherwise.
#[derive(Debug, Clone, serde::Deserialize)]
struct HyprlandKeyboard {
    name: String,
    active_keymap: String,
    layout: String,
    #[serde(default)]
    main: bool,
}

fn parse_hyprland_devices(json: &str) -> Option<HyprlandKeyboard> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let keyboards = root.get("keyboards")?.as_array()?;
    let parsed: Vec<HyprlandKeyboard> = keyboards.iter().filter_map(|k| serde_json::from_value(k.clone()).ok()).collect();
    parsed.iter().find(|k| k.main).cloned().or_else(|| parsed.into_iter().next())
}

fn apply_hyprland_layout(state: &Arc<Mutex<KeyboardState>>, device_name: &Arc<Mutex<Option<String>>>, keyboard: &HyprlandKeyboard) {
    let mut guard = state.lock().unwrap();
    guard.active_layout = keyboard.active_keymap.clone();
    guard.layout_count = keyboard.layout.split(',').filter(|s| !s.is_empty()).count() as u32;
    // active_layout_index intentionally left at its previous value -- see HyprlandKeyboard's doc
    // comment.
    drop(guard);
    *device_name.lock().unwrap() = Some(keyboard.name.clone());
}

/// Orders concurrent [`resync_hyprland_layout`] calls (one at construction, one per
/// `activelayout>>` event -- independent tasks with no other sequencing between them) so a
/// slower-to-finish but *older* `hyprctl` call can't overwrite a faster, more recent one's result
/// (Correctness review: without this, a burst of rapid layout switches could leave `KeyboardState`
/// showing a stale layout indefinitely, since nothing re-triggers a resync on its own). `ticket()`
/// hands out a strictly increasing number per call, taken before the `hyprctl` subprocess even
/// starts (i.e. in initiation order); `claim()` only lets a result apply if its ticket is still the
/// newest one seen, so a result finishing out of order is dropped rather than applied.
struct ResyncSequence {
    next: AtomicU64,
    last_applied: AtomicU64,
}

impl ResyncSequence {
    fn new() -> Self {
        Self { next: AtomicU64::new(0), last_applied: AtomicU64::new(0) }
    }

    fn ticket(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    fn claim(&self, ticket: u64) -> bool {
        let mut current = self.last_applied.load(Ordering::SeqCst);
        loop {
            if ticket < current {
                return false;
            }
            match self.last_applied.compare_exchange(current, ticket, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }
}

async fn resync_hyprland_layout(state: &Arc<Mutex<KeyboardState>>, device_name: &Arc<Mutex<Option<String>>>, sequence: &ResyncSequence) {
    let ticket = sequence.ticket();
    let output = match tokio::process::Command::new("hyprctl").args(["-j", "devices"]).output().await {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!("keyboard: hyprctl -j devices exited with {}; layout not updated this round", output.status);
            return;
        }
        Err(err) => {
            eprintln!("keyboard: failed to run hyprctl -j devices: {err}");
            return;
        }
    };
    let Ok(text) = String::from_utf8(output.stdout) else {
        eprintln!("keyboard: hyprctl -j devices produced non-UTF-8 output; layout not updated this round");
        return;
    };
    match parse_hyprland_devices(&text) {
        Some(keyboard) => {
            if sequence.claim(ticket) {
                apply_hyprland_layout(state, device_name, &keyboard);
            } else {
                eprintln!("keyboard: dropping a stale hyprctl -j devices result (a more recent layout query already applied)");
            }
        }
        None => eprintln!("keyboard: hyprctl -j devices output didn't contain a usable keyboard entry; layout not updated this round"),
    }
}

impl HyprlandLink {
    /// `signature` is `$HYPRLAND_INSTANCE_SIGNATURE`, already confirmed present by
    /// [`detect_compositor`]. Spawns the `.socket2.sock` event-listener thread and runs one
    /// initial resync so `active_layout` isn't empty until the first `activelayout` event
    /// arrives.
    pub fn new(signature: String, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) -> Self {
        let device_name: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sequence = Arc::new(ResyncSequence::new());
        let socket_path = hyprland_socket_path(&signature, "socket2.sock");
        let initial_state = Arc::clone(&state);
        let initial_device_name = Arc::clone(&device_name);
        let initial_sequence = Arc::clone(&sequence);
        tokio::spawn(async move {
            resync_hyprland_layout(&initial_state, &initial_device_name, &initial_sequence).await;
        });

        let loop_device_name = Arc::clone(&device_name);
        let loop_sequence = Arc::clone(&sequence);
        // Captured on the constructor's own (tokio-context) thread: `std::thread::spawn` below
        // starts a plain OS thread with no tokio runtime bound to it, so the free `tokio::spawn`
        // fn (which reads that binding from a thread-local) would panic -- and, under this
        // workspace's `panic = "abort"` release profile, take down the whole process -- the first
        // time this loop actually needs to hop onto the runtime. `Handle::spawn` carries its own
        // runtime reference explicitly instead (Correctness review).
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let stream = match UnixStream::connect(&socket_path) {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!("keyboard: failed to connect to Hyprland's event socket at {}: {err}", socket_path.display());
                    return;
                }
            };
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else {
                    eprintln!("keyboard: Hyprland event socket read failed; layout will no longer update");
                    return;
                };
                if !line.starts_with("activelayout>>") {
                    continue;
                }
                let state = Arc::clone(&state);
                let device_name = Arc::clone(&loop_device_name);
                let sequence = Arc::clone(&loop_sequence);
                let events = events.clone();
                handle.spawn(async move {
                    resync_hyprland_layout(&state, &device_name, &sequence).await;
                    let _ = events.send(KeyboardSignal::Changed);
                });
            }
        });

        Self { signature, device_name }
    }
}

fn hyprland_socket_path(signature: &str, name: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("hypr").join(signature).join(name)
}

impl CompositorLink for HyprlandLink {
    fn kind(&self) -> CompositorKind {
        CompositorKind::Hyprland
    }

    fn switch_layout(&self, index: usize) {
        let Some(device) = self.device_name.lock().unwrap().clone() else {
            eprintln!("keyboard: switch_layout called before Hyprland's primary keyboard device name is known; ignored");
            return;
        };
        let socket_path = hyprland_socket_path(&self.signature, "socket.sock");
        tokio::spawn(async move {
            let mut stream = match tokio::net::UnixStream::connect(&socket_path).await {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!("keyboard: failed to connect to Hyprland's command socket at {}: {err}", socket_path.display());
                    return;
                }
            };
            let command = format!("switchxkblayout {device} {index}");
            if let Err(err) = tokio::io::AsyncWriteExt::write_all(&mut stream, command.as_bytes()).await {
                eprintln!("keyboard: failed to send switchxkblayout to Hyprland: {err}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hyprland_devices_reads_the_first_keyboards_entry() {
        let json = r#"{"mice":[],"keyboards":[{"active_keymap":"English (US)","layout":"us,ara","name":"at-translated-set-2-keyboard"}],"tablets":[]}"#;
        let keyboard = parse_hyprland_devices(json).expect("should parse");
        assert_eq!(keyboard.active_keymap, "English (US)");
        assert_eq!(keyboard.layout, "us,ara");
        assert_eq!(keyboard.name, "at-translated-set-2-keyboard");
    }

    #[test]
    fn parse_hyprland_devices_prefers_the_main_keyboard_over_array_order() {
        let json = r#"{"keyboards":[
            {"active_keymap":"English (US)","layout":"us","name":"secondary-kb","main":false},
            {"active_keymap":"Arabic (Egypt)","layout":"us,ara","name":"primary-kb","main":true}
        ]}"#;
        let keyboard = parse_hyprland_devices(json).expect("should parse");
        assert_eq!(keyboard.name, "primary-kb");
    }

    #[test]
    fn parse_hyprland_devices_is_none_when_keyboards_is_empty() {
        let json = r#"{"keyboards":[]}"#;
        assert!(parse_hyprland_devices(json).is_none());
    }

    #[test]
    fn parse_hyprland_devices_is_none_for_malformed_json() {
        assert!(parse_hyprland_devices("not json").is_none());
    }

    #[test]
    fn niri_switch_layout_index_validation_accepts_the_full_u8_range() {
        assert_eq!(u8::try_from(0usize), Ok(0));
        assert_eq!(u8::try_from(255usize), Ok(255));
    }

    #[test]
    fn niri_switch_layout_index_validation_rejects_values_that_would_truncate() {
        // Regression test (Correctness review): 256 used to truncate to 0 via `as u8` instead of
        // being rejected, silently switching to the wrong layout.
        assert!(u8::try_from(256usize).is_err());
        assert!(u8::try_from(usize::MAX).is_err());
    }

    #[test]
    fn resync_sequence_claim_accepts_tickets_in_initiation_order() {
        let sequence = ResyncSequence::new();
        let first = sequence.ticket();
        let second = sequence.ticket();
        assert!(sequence.claim(first));
        assert!(sequence.claim(second));
    }

    #[test]
    fn resync_sequence_claim_drops_a_ticket_older_than_one_already_applied() {
        // Regression test (Correctness review): an older `hyprctl` resync that finishes after a
        // newer one already applied must not be allowed to overwrite it with stale data.
        let sequence = ResyncSequence::new();
        let stale = sequence.ticket();
        let fresh = sequence.ticket();
        assert!(sequence.claim(fresh));
        assert!(!sequence.claim(stale));
    }

    #[test]
    fn hyprland_socket_path_joins_runtime_dir_hypr_signature_and_name() {
        // SAFETY: single-threaded test, no other test in this process reads XDG_RUNTIME_DIR
        // concurrently.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
        assert_eq!(hyprland_socket_path("abc123", "socket2.sock"), PathBuf::from("/run/user/1000/hypr/abc123/socket2.sock"));
    }
}
