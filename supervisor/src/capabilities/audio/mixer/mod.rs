//! PipeWire registry listener for audio streams and master/source devices.
//!
//! A stream's owning pid is its `application.process.id` (`PW_KEY_APP_PROCESS_ID`), not
//! `sec.pid`/`node.client-id` (not real `pipewire-rs` keys) or `pipewire.sec.pid` (the
//! `pipewire-pulse` Client pid, not the routed application's). It matches `/proc/{pid}/comm` in
//! every checked case (ADR-0016).
//!
//! `media.class == "Stream/Output/Audio"` identifies playback streams (verified in `pw-dump`).
//!
//! `on_global` filters only `media.class` and binds immediately. A `pipewire-pulse` stream's
//! `global` event precedes its `application.process.id`/`application.name`; filtering on the full
//! parse misses every such stream. The pid arrives moments later in `info`, parsed by
//! `state::build_app_stream`.
//!
//! `info` also fires for RUNNING/IDLE/SUSPENDED, params, and ports. `props()` is `Some` every time,
//! but PipeWire's C marshaller fills it only when `change_mask` has `PW_NODE_CHANGE_MASK_PROPS`;
//! other events carry a non-null empty dict. The listener gates `build_app_stream` on
//! `NodeChangeMask::PROPS` so those events do not look like a vanished stream.
//!
//! The first `info` after `registry.bind()` is guaranteed to have PROPS: upstream `global_bind`
//! (`impl-node.c`) sets `change_mask = PW_NODE_CHANGE_MASK_ALL`, and `protocol-native.c` only
//! nulls the dict when PROPS is unset. Later `emit_info_changed` events forward changed bits only.
//! Live `pw-mon` confirmed the first `paplay` `added:` block already had full `properties:`.
//!
//! ponytail: filtering runs once at `global`; a later class change is missed. Real clients set it
//! at creation. Upgrade path: bind every `ObjectType::Node` and filter in `info` if that changes.
//!
//! Master/source tracking (§ 2.4, ADR-0053 decision 3) is a second, mostly independent tracking
//! job on the same registry listener. It reads `Audio/Sink`/`Audio/Source` `Props` in `param`
//! events and resolves them through `default.audio.sink`/`source` metadata names.
//! This file wires events; [`super::master`] owns parsing, resolution, and linear/cubic conversion.

mod registry;
mod state;
mod streams;
mod write;

pub use registry::{AudioCommandSender, command_channel, run};
pub use state::{AudioCommand, PrivacySources};
pub use streams::{CaptureApp, VideoSourceApp};
// `main.rs` names this on `ensure_mixer_thread`'s sender: lazy start (ADR-0070) leaves the
// channel alive beyond thread construction, so `run` no longer supplies the item type.
pub use state::AudioState;

use std::collections::HashMap;

use pipewire::spa::utils::dict::DictRef;

/// String lookup shared by live PipeWire dicts and recorded `pw-dump` maps in tests.
pub(super) trait PropsLookup {
    fn get_prop(&self, key: &str) -> Option<&str>;
}

impl PropsLookup for DictRef {
    fn get_prop(&self, key: &str) -> Option<&str> {
        self.get(key)
    }
}

impl PropsLookup for HashMap<String, String> {
    fn get_prop(&self, key: &str) -> Option<&str> {
        self.get(key).map(String::as_str)
    }
}
