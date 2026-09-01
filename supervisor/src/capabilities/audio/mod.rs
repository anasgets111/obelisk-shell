//! PipeWire-backed audio state; § 2.4's master volume/mute added per ADR-0053 decision 3.
//! `mixer` tracks the registry and every list § 2.4 names; `master`
//! holds the pure parsing/resolution logic `mixer` wires PipeWire events through.
//!
//! § 3.2's audio write actions live here too, in [`dispatch`]. Seven of the nine are built; see
//! that function's own doc comment for the two that are not.
//!
//! BlueZ codec control (§6) is later work and belongs to `bluetooth` rather than here.

pub mod master;
pub mod mixer;

use mixer::{AudioCommand, AudioCommandSender};

/// Every action `oblisk.audio:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AudioAction {
    SetVolume,
    SetMuted,
    ToggleMute,
    SetDefaultSink,
    SetDefaultSource,
    SetAppVolume,
    SetAppMuted,
}

/// `oblisk.audio`'s action dispatch (ADR-0037). Unlike every other capability's adapter, this one
/// has no controller to call: each action becomes an [`AudioCommand`] on the channel into the
/// PipeWire thread, and nothing here awaits a result.
///
/// § 3.2 lists two more audio actions this does not implement: `play_sound(sound)` and
/// `set_event_sounds_enabled(en)` need a sound player, an event-sound theme, and a place to
/// store the toggle, none of which exist anywhere in this codebase. Named here so their absence
/// is a decision rather than a gap in the match.
pub fn dispatch(commands: &AudioCommandSender, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<AudioAction>(params) else { return };
    let command = match action {
        AudioAction::SetVolume => parse_volume_arg(&params.arguments).map(AudioCommand::SetMasterVolume),
        AudioAction::SetMuted => {
            crate::capabilities::parse_bool_arg(&params.arguments).map(AudioCommand::SetMasterMuted)
        }
        AudioAction::ToggleMute => Some(AudioCommand::ToggleMasterMute),
        AudioAction::SetDefaultSink => parse_id_arg(&params.arguments).map(AudioCommand::SetDefaultSink),
        AudioAction::SetDefaultSource => parse_id_arg(&params.arguments).map(AudioCommand::SetDefaultSource),
        AudioAction::SetAppVolume => {
            parse_id_and_volume_args(&params.arguments).map(|(id, volume)| AudioCommand::SetAppVolume { id, volume })
        }
        AudioAction::SetAppMuted => {
            parse_id_and_bool_args(&params.arguments).map(|(id, muted)| AudioCommand::SetAppMuted { id, muted })
        }
    };
    let Some(command) = command else {
        return crate::log_malformed_command(params);
    };
    if commands.send(command).is_err() {
        eprintln!("audio: the PipeWire command channel is closed; {} was dropped", params.action);
    }
}

/// `[vol]`. Shape check only: § 3.2's `[0.0, 1.0]` range is clamped once, in
/// `master::cubed_channel_volumes`.
fn parse_volume_arg(arguments: &[serde_json::Value]) -> Option<f32> {
    Some(arguments.first()?.as_f64()? as f32)
}

/// `[id]`. A PipeWire registry id, so it must fit a `u32`: a larger number is rejected here
/// rather than truncated into an id naming a different node.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_arg_reads_a_float_and_an_integer_alike() {
        assert_eq!(parse_volume_arg(&[serde_json::json!(0.3)]), Some(0.3));
        // Lua has one number type, so a config writing 1 rather than 1.0 is common, not odd.
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
}
