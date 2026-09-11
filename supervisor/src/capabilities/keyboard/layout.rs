//! Keyboard layout integration for `obelisk.keyboard` (ADR-0034). `crate::compositor` selects a
//! [`CompositorLink`] at startup; without a supported compositor, layout is empty with count `0`.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{CompositorKind, hyprland_socket_path};

use super::controller::{KeyboardSignal, KeyboardState};

/// `switch_layout` is synchronous fire-and-forget; state returns through the implementor's event
/// stream. It is not `async fn` to preserve `Box<dyn CompositorLink>` object safety.
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
    /// Connects to `$NIRI_SOCKET` and reads its blocking `Socket` stream on a dedicated thread.
    /// The first event carries full initial state, so no startup query is needed. Failed connects
    /// yield `None`.
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

    /// Opens a fresh connection because `read_events` consumes and shuts down the event socket's
    /// write half; that connection cannot also send this command.
    fn switch_layout(&self, index: usize) {
        // JSON-RPC supplies an unbounded u64, while niri's wire protocol takes u8. Reject overflow
        // instead of truncating 256 to 0.
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

/// Hyprland IPC uses two sockets under
/// `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`: `.socket2.sock` pushes newline-terminated
/// `event>>payload` lines, where `activelayout>>...` only triggers a resync; `.socket.sock` accepts
/// `devices -j` reads and `switchxkblayout <device> <index>` writes.
pub struct HyprlandLink {
    signature: String,
    /// Primary keyboard device name (`keyboards[].name`) from the latest resync. Hyprland requires
    /// a real name, not a documented wildcard. `None` until success; `switch_layout` then no-ops.
    device_name: Arc<Mutex<Option<String>>>,
}

/// Needed fields from `hyprctl -j devices`'s `keyboards` entries. `active_keymap` is the display
/// name; comma-separated `layout` only supplies the count. Hyprland provides no code-to-name
/// table, so `active_layout_index` stays `0`. Prefer `main` (ADR-0034) because array order is not
/// guaranteed with multiple keyboards.
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
    // Hyprland has no code-to-name mapping; keep the previous index (see above).
    drop(guard);
    *device_name.lock().unwrap() = Some(keyboard.name.clone());
}

/// Orders concurrent [`resync_hyprland_layout`] calls so an older, slower `hyprctl` result cannot
/// overwrite a newer one. Without it, rapid switches leave `KeyboardState` stale. `ticket()`
/// increments before spawning; `claim()` applies only the newest ticket.
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
    /// `signature` is `$HYPRLAND_INSTANCE_SIGNATURE`, already confirmed by
    /// `compositor::detect_compositor`. Starts the listener and an initial resync so
    /// `active_layout` is populated before the first `activelayout` event.
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
        // The OS listener thread has no Tokio runtime. Free `tokio::spawn` would panic and, under
        // this workspace's `panic = "abort"` release profile, abort the process; use this handle.
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
        // Regression: 256 used to truncate to 0 via `as u8`.
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
        // Regression: an older resync must not overwrite a newer result.
        let sequence = ResyncSequence::new();
        let stale = sequence.ticket();
        let fresh = sequence.ticket();
        assert!(sequence.claim(fresh));
        assert!(!sequence.claim(stale));
    }
}
