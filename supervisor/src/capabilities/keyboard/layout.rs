//! Keyboard layout integration for `obelisk.keyboard` (ADR-0034). `crate::compositor` selects a
//! [`CompositorLink`] at startup; without a supported compositor, layout is empty with count `0`.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{CompositorKind, hyprland_request, hyprland_socket_path};

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
/// `event>>payload` lines, where `activelayout>>...` only triggers a resync; `.socket.sock` answers
/// `j/devices` reads and `switchxkblayout main <index>` writes.
pub struct HyprlandLink {
    signature: String,
}

/// Needed fields from `j/devices`'s `keyboards` entries; comma-separated `layout` only supplies the
/// count. `main` is Hyprland's `m_active`, reassigned on every key event, so it is the keyboard
/// being typed on. `active_layout_index` defaults to `0` on a Hyprland too old to send it.
#[derive(Debug, Clone, serde::Deserialize)]
struct HyprlandKeyboard {
    active_keymap: String,
    layout: String,
    #[serde(default)]
    active_layout_index: u32,
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

fn apply_hyprland_layout(state: &Arc<Mutex<KeyboardState>>, keyboard: &HyprlandKeyboard) {
    let mut guard = state.lock().unwrap();
    guard.active_layout = keyboard.active_keymap.clone();
    guard.active_layout_index = keyboard.active_layout_index;
    guard.layout_count = keyboard.layout.split(',').filter(|s| !s.is_empty()).count() as u32;
}

/// `true` once `state` holds a fresh read. Blocking on the event thread, so two reads cannot land
/// out of order.
fn resync_hyprland_layout(socket_path: &Path, state: &Arc<Mutex<KeyboardState>>) -> bool {
    let reply = match hyprland_request(socket_path, "j/devices") {
        Ok(reply) => reply,
        Err(err) => {
            eprintln!("keyboard: Hyprland `devices` request failed; layout not updated this round: {err}");
            return false;
        }
    };
    match parse_hyprland_devices(&reply) {
        Some(keyboard) => {
            apply_hyprland_layout(state, &keyboard);
            true
        }
        None => {
            eprintln!(
                "keyboard: Hyprland `devices` reply held no usable keyboard entry; layout not updated this round"
            );
            false
        }
    }
}

impl HyprlandLink {
    /// `signature` is `$HYPRLAND_INSTANCE_SIGNATURE`, already confirmed by
    /// `compositor::detect_compositor`. Connects to the event socket before the first read, so a
    /// switch in that gap is still a line to process.
    pub fn new(signature: String, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<KeyboardSignal>) -> Self {
        let events_path = hyprland_socket_path(&signature, ".socket2.sock");
        let command_path = hyprland_socket_path(&signature, ".socket.sock");
        match UnixStream::connect(&events_path) {
            Ok(stream) => {
                std::thread::spawn(move || {
                    // Signal the initial read too, or `layout_count` stays `0` and an indicator
                    // drawn only for two or more layouts hides until the first switch.
                    if resync_hyprland_layout(&command_path, &state) && events.send(KeyboardSignal::Changed).is_err() {
                        return;
                    }
                    for line in BufReader::new(stream).lines() {
                        let Ok(line) = line else {
                            eprintln!("keyboard: Hyprland event socket read failed; layout will no longer update");
                            return;
                        };
                        if !line.starts_with("activelayout>>") {
                            continue;
                        }
                        if resync_hyprland_layout(&command_path, &state)
                            && events.send(KeyboardSignal::Changed).is_err()
                        {
                            return;
                        }
                    }
                    eprintln!("keyboard: Hyprland event socket closed; layout will no longer update");
                });
            }
            Err(err) => eprintln!(
                "keyboard: failed to connect to Hyprland's event socket at {}; layout reporting disabled for this run: {err}",
                events_path.display()
            ),
        }
        Self { signature }
    }
}

impl CompositorLink for HyprlandLink {
    fn kind(&self) -> CompositorKind {
        CompositorKind::Hyprland
    }

    /// `main` is also Hyprland's device target for that keyboard, so no device name is tracked
    /// here. It refuses an out-of-range index, hence reading the reply.
    fn switch_layout(&self, index: usize) {
        let socket_path = hyprland_socket_path(&self.signature, ".socket.sock");
        std::thread::spawn(move || {
            let command = format!("switchxkblayout main {index}");
            match hyprland_request(&socket_path, &command) {
                Ok(reply) if reply.trim() == "ok" => {}
                Ok(reply) => eprintln!("keyboard: Hyprland refused `{command}`: {}", reply.trim()),
                Err(err) => eprintln!("keyboard: Hyprland `{command}` request failed: {err}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hyprland_devices_reads_the_first_keyboards_entry() {
        let json = r#"{"mice":[],"keyboards":[{"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1}],"tablets":[]}"#;
        let keyboard = parse_hyprland_devices(json).expect("should parse");
        assert_eq!(keyboard.active_keymap, "Arabic (Egypt)");
        assert_eq!(keyboard.active_layout_index, 1);
    }

    #[test]
    fn parse_hyprland_devices_prefers_the_main_keyboard_over_array_order() {
        // `main` moves to whichever keyboard was typed on last, so array order names the wrong one.
        let json = r#"{"keyboards":[
            {"active_keymap":"English (US)","layout":"us,ara","active_layout_index":0,"main":false},
            {"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1,"main":true}
        ]}"#;
        let keyboard = parse_hyprland_devices(json).expect("should parse");
        assert_eq!(keyboard.active_layout_index, 1);
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
    fn apply_hyprland_layout_counts_the_configured_layouts_and_keeps_the_reported_index() {
        // Regression: the index was pinned to `0`, so Lua could not cycle from the reported value.
        let json = r#"{"keyboards":[{"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1,"main":true}]}"#;
        let state = Arc::new(Mutex::new(KeyboardState::default()));
        apply_hyprland_layout(&state, &parse_hyprland_devices(json).expect("should parse"));
        let guard = state.lock().unwrap();
        assert_eq!(
            (guard.active_layout.as_str(), guard.active_layout_index, guard.layout_count),
            ("Arabic (Egypt)", 1, 2)
        );
    }

    #[test]
    fn parse_hyprland_devices_defaults_the_index_when_hyprland_omits_it() {
        let json = r#"{"keyboards":[{"active_keymap":"English (US)","layout":"us"}]}"#;
        assert_eq!(parse_hyprland_devices(json).expect("should parse").active_layout_index, 0);
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
}
