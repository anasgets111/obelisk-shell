//! PipeWire-backed audio state. `mixer` tracks the registry lists; `master` holds the pure
//! parsing/resolution logic (ADR-0053 decision 3).
//!
//! Write actions dispatch here, including source-side volume/mute actions that filled the
//! gap beside `set_muted`; see [`dispatch`] for the two still unbuilt actions.
//!
//! BlueZ codec control lives here as well, because PipeWire, not BlueZ, picks the codec: each
//! BlueZ device's profiles are its codecs (ADR-0030).

pub mod master;
pub mod mixer;

use mixer::{AudioCommand, AudioCommandSender};

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AudioAction {
    /// (volume: number) Sets master output volume, clamped to `[0.0, 1.5]`.
    SetVolume,
    /// (muted: boolean) Sets master output mute.
    SetMuted,
    /// () Toggles master output mute.
    ToggleMute,
    /// (balance: number) Sets default output balance, clamped to `[-1.0, 1.0]`; the louder side keeps its level and never passes the cap.
    SetBalance,
    /// (id: integer) Makes this `sinks[].id` the default output.
    SetDefaultSink,
    /// (id: integer) Makes this `sources[].id` the default input.
    SetDefaultSource,
    /// (volume: number) Sets default input volume, clamped to `[0.0, 1.0]`.
    SetSourceVolume,
    /// (muted: boolean) Sets default input mute.
    SetSourceMuted,
    /// () Toggles default input mute.
    ToggleSourceMute,
    /// (id: integer, volume: number) Sets an `apps[].id` stream's volume, clamped to `[0.0, 1.0]`.
    SetAppVolume,
    /// (id: integer, muted: boolean) Sets an `apps[].id` stream's mute.
    SetAppMuted,
    /// (device: integer, index: integer) Switches a `bluetooth[].device` to one of its `codecs[].index`.
    SetBluetoothProfile,
}

/// Unlike every other capability's adapter, this has no controller to call. It dispatches each
/// action as an [`AudioCommand`] on the PipeWire thread (ADR-0037); no result is awaited here.
///
/// `play_sound(sound)` and `set_event_sounds_enabled(en)` are not actions: they'd need a sound
/// player, event-sound theme, and toggle storage, none of which exists.
pub fn dispatch(commands: &AudioCommandSender, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<AudioAction>(params) else { return };
    let command = match action {
        AudioAction::SetVolume => parse_volume_arg(&params.arguments).map(AudioCommand::SetMasterVolume),
        AudioAction::SetMuted => {
            crate::capabilities::parse_bool_arg(&params.arguments).map(AudioCommand::SetMasterMuted)
        }
        AudioAction::ToggleMute => Some(AudioCommand::ToggleMasterMute),
        AudioAction::SetBalance => parse_volume_arg(&params.arguments).map(AudioCommand::SetBalance),
        AudioAction::SetDefaultSink => parse_id_arg(&params.arguments).map(AudioCommand::SetDefaultSink),
        AudioAction::SetDefaultSource => parse_id_arg(&params.arguments).map(AudioCommand::SetDefaultSource),
        AudioAction::SetSourceVolume => parse_volume_arg(&params.arguments).map(AudioCommand::SetSourceVolume),
        AudioAction::SetSourceMuted => {
            crate::capabilities::parse_bool_arg(&params.arguments).map(AudioCommand::SetSourceMuted)
        }
        AudioAction::ToggleSourceMute => Some(AudioCommand::ToggleSourceMute),
        AudioAction::SetAppVolume => {
            parse_id_and_volume_args(&params.arguments).map(|(id, volume)| AudioCommand::SetAppVolume { id, volume })
        }
        AudioAction::SetAppMuted => {
            parse_id_and_bool_args(&params.arguments).map(|(id, muted)| AudioCommand::SetAppMuted { id, muted })
        }
        AudioAction::SetBluetoothProfile => parse_id_and_index_args(&params.arguments)
            .map(|(device, index)| AudioCommand::SetBluetoothProfile { device, index }),
    };
    let Some(command) = command else {
        return crate::log_malformed_command(params);
    };
    if commands.send(command).is_err() {
        eprintln!("audio: the PipeWire command channel is closed; {} was dropped", params.action);
    }
}

/// Parses `[number]`; the range is clamped in `master`.
fn parse_volume_arg(arguments: &[serde_json::Value]) -> Option<f32> {
    Some(arguments.first()?.as_f64()? as f32)
}

/// Parses `[id]` as a `u32`; larger values are rejected rather than truncated into another node id.
fn parse_id_arg(arguments: &[serde_json::Value]) -> Option<u32> {
    u32::try_from(arguments.first()?.as_u64()?).ok()
}

fn parse_id_and_volume_args(arguments: &[serde_json::Value]) -> Option<(u32, f32)> {
    let id = parse_id_arg(arguments)?;
    Some((id, arguments.get(1)?.as_f64()? as f32))
}

fn parse_id_and_bool_args(arguments: &[serde_json::Value]) -> Option<(u32, bool)> {
    let id = parse_id_arg(arguments)?;
    Some((id, arguments.get(1)?.as_bool()?))
}

/// Parses `[device, index]`; an index outside `i32` is rejected rather than wrapped.
fn parse_id_and_index_args(arguments: &[serde_json::Value]) -> Option<(u32, i32)> {
    let id = parse_id_arg(arguments)?;
    Some((id, i32::try_from(arguments.get(1)?.as_i64()?).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_arg_reads_a_float_and_an_integer_alike() {
        assert_eq!(parse_volume_arg(&[serde_json::json!(0.3)]), Some(0.3));
        // Lua has one number type, so configs commonly write 1 rather than 1.0.
        assert_eq!(parse_volume_arg(&[serde_json::json!(1)]), Some(1.0));
    }

    #[test]
    fn parse_volume_arg_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_volume_arg(&[]), None);
        assert_eq!(parse_volume_arg(&[serde_json::json!("loud")]), None);
    }

    #[test]
    fn parse_id_arg_rejects_a_value_that_would_truncate_into_another_nodes_id() {
        assert_eq!(parse_id_arg(&[serde_json::json!(59)]), Some(59));
        assert_eq!(parse_id_arg(&[serde_json::json!(u64::from(u32::MAX) + 1)]), None);
        assert_eq!(parse_id_arg(&[serde_json::json!(-1)]), None);
    }

    #[test]
    fn the_two_argument_parsers_need_both_arguments_and_both_types() {
        assert_eq!(parse_id_and_volume_args(&[serde_json::json!(7), serde_json::json!(0.5)]), Some((7, 0.5)));
        assert_eq!(parse_id_and_volume_args(&[serde_json::json!(7)]), None);
        assert_eq!(parse_id_and_bool_args(&[serde_json::json!(7), serde_json::json!(true)]), Some((7, true)));
        assert_eq!(parse_id_and_bool_args(&[serde_json::json!(7), serde_json::json!(1)]), None);
    }

    #[test]
    fn parse_id_and_index_args_reads_a_device_and_a_profile_index() {
        assert_eq!(parse_id_and_index_args(&[serde_json::json!(80), serde_json::json!(2)]), Some((80, 2)));
        assert_eq!(parse_id_and_index_args(&[serde_json::json!(80)]), None);
        assert_eq!(parse_id_and_index_args(&[serde_json::json!(80), serde_json::json!(i64::from(i32::MAX) + 1)]), None);
    }
}
