//! PipeWire registry listener for per-app audio streams (build-steps.md Phase 6, point 2).
//!
//! A stream node's owning process pid is `application.process.id` (`PW_KEY_APP_PROCESS_ID`),
//! set on the stream node's own properties -- not `sec.pid`/`node.client-id` (not real
//! `pipewire-rs` keys) and not `pipewire.sec.pid` (set on the Client object, but for a stream
//! routed through `pipewire-pulse` that's `pipewire-pulse`'s own pid, not the application's).
//! Matches `/proc/{pid}/comm` for the real app in every case checked. See docs/adr/0016.
//!
//! `media.class == "Stream/Output/Audio"` identifies a playback stream, verified against real
//! `pw-dump` output.
//!
//! `registry::on_global` checks `media.class` at registry `global` time and always binds a matching
//! node, without also requiring `application.process.id` yet: a `pipewire-pulse`-routed
//! stream's `global` event fires before `pipewire-pulse` pushes
//! `application.process.id`/`application.name` onto the node, so filtering on the full parse at
//! `global` time misses every such stream. The pid arrives moments later through the bound
//! node's own `info` event, which `state::build_app_stream` runs against.
//!
//! `info` fires on any change PipeWire tracks for the node, not just a props change -- state
//! transitions (RUNNING <-> IDLE/SUSPENDED), params, and ports changes all trigger it too.
//! `NodeInfoRef::props()` returns `Some(&DictRef)` on every `info` call, but PipeWire's C
//! marshaller only fills real entries into that dict when `change_mask` includes
//! `PW_NODE_CHANGE_MASK_PROPS`; any other kind of `info` event carries a non-null but empty
//! dict. So the node listener checks `NodeInfoRef::change_mask()` for `NodeChangeMask::PROPS`
//! before running `state::build_app_stream`, leaving a non-PROPS event's tracked entry untouched
//! rather than misreading it as the stream disappearing.
//!
//! The very first `info` call after `registry.bind()` is guaranteed to carry
//! `NodeChangeMask::PROPS`: upstream PipeWire's `global_bind` (`impl-node.c`) unconditionally
//! sets `change_mask = PW_NODE_CHANGE_MASK_ALL` before any other event can reach a freshly
//! bound resource, and `protocol-native.c`'s marshaller only nulls the props dict when the
//! PROPS bit is unset, so `ALL` always carries a real dict. Later `info` calls go through
//! `emit_info_changed` instead, which forwards only the bits that actually changed. Confirmed
//! live too: `pw-mon` attached before starting `paplay` showed the node's first `added:` block
//! already carrying a full `properties:` section, with no earlier emptier `info` event.
//!
//! ponytail: the `media.class` filter runs once, at `global` time. A node whose `media.class`
//! starts as something else and only later changes to `Stream/Output/Audio` won't be picked up
//! -- real clients set `media.class` once at creation and don't mutate it. If that stops being
//! true, the fix is binding every `ObjectType::Node` unconditionally and filtering inside the
//! `info` callback instead.
//!
//! Master output volume/mute (§ 2.4, docs/adr/0053 decision 3) is a second, mostly independent
//! tracking job on the same registry listener: `Audio/Sink` nodes' `SPA_PARAM_Props` param (via
//! `param` events, not `info`) and the `default` `Metadata` object's `default.audio.sink` key
//! (which names the master sink by `node.name`). This file only wires the PipeWire event
//! plumbing; [`super::master`] holds the pure parsing and resolution logic, including the linear/cubic
//! volume conversion.

mod registry;
mod state;
mod write;

pub use registry::{command_channel, run, AudioCommandSender};
pub use state::{AudioCommand, VideoSourceApp};
