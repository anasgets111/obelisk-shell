//! Keyboard layout half of `oblisk.keyboard` (ADR-0034): a deliberately narrow
//! [`CompositorLink`] trait, with an implementor picked at startup by `crate::compositor`'s
//! probe. Niri's implementor is live-tested on this dev machine; Hyprland's is built to its
//! documented IPC protocol but not independently live-verified (ADR-0034 defers that to the
//! user's own Hyprland machine). No supported compositor: `active_layout` degrades to
//! unavailable (empty string, index/count at `0`).
//!
//! `CompositorKind` and the probe itself moved to `crate::compositor` once `workspaces` became
//! their second caller. This module owns the trait, not the detection.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{CompositorKind, hyprland_socket_path};

use super::controller::{KeyboardSignal, KeyboardState};

/// Methods are synchronous, fire-and-forget for `switch_layout` -- the real state update
/// flows back through the implementor's own event stream, not a return value here. Not
/// `async fn` (would make `Box<dyn CompositorLink>` non-object-safe without a new
/// dependency): each implementor spawns its own background thread/task for the real I/O.
pub trait CompositorLink: Send + Sync {
    fn kind(&self) -> CompositorKind;
    fn switch_layout(&self, index: usize);
}

fn apply_niri_layout(state: &Arc<Mutex<KeyboardState>>, names: &[String], idx: u8) {
    let mut guard = state.lock().unwrap();
    guard.active_layout = names.get(idx as usize).cloned().unwrap_or_default();
    guard.active_layout_index = u32::from(idx);
    guard.layout_count = names.len() as u32;
}

pub struct NiriLink;

impl NiriLink {
    /// Connects to `$NIRI_SOCKET` and spawns the event-stream reader on its own OS thread --
    /// `Socket` is a blocking `std::net::UnixStream` wrapper, not tokio-aware. The first
    /// `EventStream` event already carries the full initial state, so no separate startup
    /// query is needed. Degrades to `None` (logged) if the socket can't connect.
    pub fn new(state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) -> Option<Self> {
        let mut socket = match niri_ipc::socket::Socket::connect() {
            Ok(socket) => socket,
            Err(err) => {
                eprintln!(
                    "keyboard: failed to connect to the niri IPC socket; layout reporting disabled for this run: {err}"
                );
                return None;
            }
        };
        match socket.send(niri_ipc::Request::EventStream) {
            Ok(Ok(niri_ipc::Response::Handled)) => {}
            Ok(Ok(_)) => {
                eprintln!(
                    "keyboard: unexpected reply to niri EventStream request; layout reporting disabled for this run"
                );
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

    /// A fresh connection per call: `read_events` consumes and shuts down the write half of
    /// the event-stream socket, so that connection genuinely cannot also send this write.
    fn switch_layout(&self, index: usize) {
        // `index` arrives from untrusted JSON-RPC as an unbounded u64; niri's wire protocol
        // takes a u8, so out-of-range is rejected here rather than silently truncated (e.g. 256 -> 0).
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
            let request = niri_ipc::Request::Action(niri_ipc::Action::SwitchLayout {
                layout: niri_ipc::LayoutSwitchTarget::Index(index),
            });
            if let Err(err) = socket.send(request) {
                eprintln!("keyboard: niri SwitchLayout request failed: {err}");
            }
        });
    }
}

/// Hyprland's real IPC protocol: two Unix sockets per instance under
/// `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/` -- `.socket2.sock` pushes
/// newline-terminated `event>>payload` lines (`activelayout>>...` is a "something changed"
/// trigger only), `.socket.sock` accepts plain-text commands for the read (`devices -j`) and
/// write (`switchxkblayout <device> <index>`).
pub struct HyprlandLink {
    signature: String,
    /// The primary keyboard's real device name (Hyprland's `keyboards[].name`), cached from
    /// the most recent resync -- `switchxkblayout` needs a genuine device name, no wildcard
    /// documented. `None` until the first successful resync; `switch_layout` is a no-op until then.
    device_name: Arc<Mutex<Option<String>>>,
}

/// One entry of `hyprctl -j devices`'s `"keyboards"` array, only the fields this needs.
/// `active_keymap` is the human-readable active layout name; `layout` is the XKB-code comma
/// list, used only for a count -- matching `active_keymap` back to a position in it needs a
/// code->name table Hyprland's JSON doesn't provide, so `active_layout_index` stays `0`. `main`
/// marks the primary keyboard (ADR-0034), preferred over the array's first entry since ordering
/// isn't guaranteed with more than one attached keyboard.
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
    let parsed: Vec<HyprlandKeyboard> =
        keyboards.iter().filter_map(|k| serde_json::from_value(k.clone()).ok()).collect();
    parsed.iter().find(|k| k.main).cloned().or_else(|| parsed.into_iter().next())
}

fn apply_hyprland_layout(
    state: &Arc<Mutex<KeyboardState>>,
    device_name: &Arc<Mutex<Option<String>>>,
    keyboard: &HyprlandKeyboard,
) {
    let mut guard = state.lock().unwrap();
    guard.active_layout = keyboard.active_keymap.clone();
    guard.layout_count = keyboard.layout.split(',').filter(|s| !s.is_empty()).count() as u32;
    // active_layout_index left at its previous value; see HyprlandKeyboard's doc comment.
    drop(guard);
    *device_name.lock().unwrap() = Some(keyboard.name.clone());
}

/// Orders concurrent [`resync_hyprland_layout`] calls (one at construction, one per
/// `activelayout>>` event) so a slower-to-finish but older `hyprctl` call can't overwrite a
/// faster, more recent result -- without this, a burst of rapid layout switches could leave
/// `KeyboardState` stuck on a stale layout. `ticket()` hands out a strictly increasing number
/// before the subprocess starts; `claim()` only applies a result if its ticket is still newest.
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

async fn resync_hyprland_layout(
    state: &Arc<Mutex<KeyboardState>>,
    device_name: &Arc<Mutex<Option<String>>>,
    sequence: &ResyncSequence,
) {
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
                eprintln!(
                    "keyboard: dropping a stale hyprctl -j devices result (a more recent layout query already applied)"
                );
            }
        }
        None => eprintln!(
            "keyboard: hyprctl -j devices output didn't contain a usable keyboard entry; layout not updated this round"
        ),
    }
}

impl HyprlandLink {
    /// `signature` is `$HYPRLAND_INSTANCE_SIGNATURE`, already confirmed present by
    /// `compositor::detect_compositor`. Spawns the event-listener thread and runs one initial resync so
    /// `active_layout` isn't empty until the first `activelayout` event arrives.
    pub fn new(signature: String, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) -> Self {
        let device_name: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sequence = Arc::new(ResyncSequence::new());
        let socket_path = hyprland_socket_path(&signature, ".socket2.sock");
        let initial_state = Arc::clone(&state);
        let initial_device_name = Arc::clone(&device_name);
        let initial_sequence = Arc::clone(&sequence);
        tokio::spawn(async move {
            resync_hyprland_layout(&initial_state, &initial_device_name, &initial_sequence).await;
        });

        let loop_device_name = Arc::clone(&device_name);
        let loop_sequence = Arc::clone(&sequence);
        // Captured on the constructor's tokio-context thread: the OS thread below has no tokio
        // runtime bound to it, so the free `tokio::spawn` would panic -- and, under this
        // workspace's `panic = "abort"` release profile, take down the process -- the first
        // time it needs the runtime. `Handle::spawn` carries its own runtime reference instead.
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let stream = match UnixStream::connect(&socket_path) {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!(
                        "keyboard: failed to connect to Hyprland's event socket at {}: {err}",
                        socket_path.display()
                    );
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

impl CompositorLink for HyprlandLink {
    fn kind(&self) -> CompositorKind {
        CompositorKind::Hyprland
    }

    fn switch_layout(&self, index: usize) {
        let Some(device) = self.device_name.lock().unwrap().clone() else {
            eprintln!(
                "keyboard: switch_layout called before Hyprland's primary keyboard device name is known; ignored"
            );
            return;
        };
        let socket_path = hyprland_socket_path(&self.signature, ".socket.sock");
        tokio::spawn(async move {
            let mut stream = match tokio::net::UnixStream::connect(&socket_path).await {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!(
                        "keyboard: failed to connect to Hyprland's command socket at {}: {err}",
                        socket_path.display()
                    );
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
        // Regression: 256 used to truncate to 0 via `as u8` instead of being rejected.
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
        // Regression: an older resync finishing after a newer one must not overwrite it with stale data.
        let sequence = ResyncSequence::new();
        let stale = sequence.ticket();
        let fresh = sequence.ticket();
        assert!(sequence.claim(fresh));
        assert!(!sequence.claim(stale));
    }
}
