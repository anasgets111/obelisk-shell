//! `obelisk.processes` owns the programs declared with `session_process`: long-running things whose
//! lifetime is the shell's rather than a generation's.
//!
//! Sibling of `storage` in shape -- a config declares a name, the Supervisor owns what sits behind
//! it, and the state comes back keyed by that name -- and its opposite in what it holds. `storage`
//! keeps a file the config could have read itself; this keeps a handle the config *cannot* hold,
//! because the VM holding it is replaced on every generation swap.

pub mod controller;

pub use controller::{ProcessesController, ProcessesSignal};

use nix::sys::signal::Signal;

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProcessesAction {
    /// (name: string, stop_signal?: "TERM"|"INT"|"HUP"|"QUIT"|"USR1"|"USR2"|"KILL"|"STOP"|"CONT") Default `TERM`.
    Declare,
    /// (name: string, cmd: string, args?: string[]) Starts a declared program without a shell.
    Start,
    /// (name: string, signal: string) Sends one of `declare`'s signal names.
    Signal,
    /// (name: string) Stops a program with its stop signal.
    Stop,
}

/// The signal names a config may write, without the `SIG` prefix.
///
/// A closed list rather than a number: a config asking for signal 9 by number is asking for
/// something it cannot have meant, and every name here is one a program documents as an interface
/// -- `INT` to finish and save, `USR1`/`USR2` for whatever the program says, `HUP` to reload.
/// `KILL` is included because a config that has decided to be rid of something should not have to
/// go through `process.run` to say so.
fn parse_signal(name: &str) -> Option<Signal> {
    Some(match name {
        "TERM" => Signal::SIGTERM,
        "INT" => Signal::SIGINT,
        "HUP" => Signal::SIGHUP,
        "QUIT" => Signal::SIGQUIT,
        "USR1" => Signal::SIGUSR1,
        "USR2" => Signal::SIGUSR2,
        "KILL" => Signal::SIGKILL,
        "STOP" => Signal::SIGSTOP,
        "CONT" => Signal::SIGCONT,
        _ => return None,
    })
}

/// A declared name: one non-empty string, used as a state key and nothing else. Rejecting the
/// empty string here keeps `sessions` from growing a key no config could read back.
fn parse_name(argument: Option<&serde_json::Value>) -> Option<String> {
    let name = argument?.as_str()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// `declare(name, stop_signal)`. A missing or null signal is `TERM`, the same default the rest of
/// the Supervisor reaps with.
pub fn parse_declare_args(arguments: &[serde_json::Value]) -> Option<(String, Signal)> {
    let name = parse_name(arguments.first())?;
    let stop_signal = match arguments.get(1) {
        None | Some(serde_json::Value::Null) => Signal::SIGTERM,
        Some(value) => parse_signal(value.as_str()?)?,
    };
    Some((name, stop_signal))
}

/// `start(name, cmd, args)`. `args` is already split, like `process.run`'s: there is no shell, so
/// one element is one argument however many spaces it holds.
pub fn parse_start_args(arguments: &[serde_json::Value]) -> Option<(String, String, Vec<String>)> {
    let name = parse_name(arguments.first())?;
    let cmd = arguments.get(1)?.as_str()?.to_string();
    if cmd.is_empty() {
        return None;
    }
    let args = match arguments.get(2) {
        None | Some(serde_json::Value::Null) => Vec::new(),
        // mlua marshals an empty Lua table as `{}`, and the `start(cmd)` wrapper sends one.
        Some(serde_json::Value::Object(fields)) if fields.is_empty() => Vec::new(),
        Some(value) => {
            value.as_array()?.iter().map(|arg| arg.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?
        }
    };
    Some((name, cmd, args))
}

/// `signal(name, signal)`.
pub fn parse_signal_args(arguments: &[serde_json::Value]) -> Option<(String, Signal)> {
    let name = parse_name(arguments.first())?;
    let signal = parse_signal(arguments.get(1)?.as_str()?)?;
    Some((name, signal))
}

/// `obelisk.processes` action dispatch (ADR-0037). Synchronous: each action touches the entry map
/// and hands the work to the per-program task, which is where every await lives.
pub fn dispatch(controller: &ProcessesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<ProcessesAction>(params) else { return };
    match action {
        ProcessesAction::Declare => match parse_declare_args(&params.arguments) {
            Some((name, stop_signal)) => controller.declare(&name, stop_signal),
            None => crate::log_malformed_command(params),
        },
        ProcessesAction::Start => match parse_start_args(&params.arguments) {
            Some((name, cmd, args)) => controller.start(&name, &cmd, &args),
            None => crate::log_malformed_command(params),
        },
        ProcessesAction::Signal => match parse_signal_args(&params.arguments) {
            Some((name, signal)) => controller.signal(&name, signal),
            None => crate::log_malformed_command(params),
        },
        ProcessesAction::Stop => match parse_name(params.arguments.first()) {
            Some(name) => controller.stop(&name),
            None => crate::log_malformed_command(params),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn declare_defaults_the_stop_signal_to_term() {
        assert_eq!(parse_declare_args(&[json!("recorder")]), Some(("recorder".to_string(), Signal::SIGTERM)));
        assert_eq!(
            parse_declare_args(&[json!("recorder"), json!("INT")]),
            Some(("recorder".to_string(), Signal::SIGINT))
        );
    }

    #[test]
    fn a_signal_is_named_without_its_sig_prefix_and_an_unknown_one_is_refused() {
        assert_eq!(parse_signal("USR2"), Some(Signal::SIGUSR2));
        assert_eq!(parse_signal("SIGUSR2"), None, "the prefix is the Supervisor's spelling, not the config's");
        assert_eq!(parse_signal("9"), None, "signals are named, so a config cannot mean 9 by accident");
        assert_eq!(parse_declare_args(&[json!("recorder"), json!("PROF")]), None);
    }

    #[test]
    fn an_empty_name_is_refused_rather_than_keyed() {
        assert_eq!(parse_name(Some(&json!(""))), None);
        assert_eq!(parse_name(Some(&json!(7))), None);
        assert_eq!(parse_name(None), None);
    }

    #[test]
    fn start_takes_a_command_and_an_already_split_argument_list() {
        assert_eq!(
            parse_start_args(&[json!("recorder"), json!("gpu-screen-recorder"), json!(["-w", "DP-1"])]),
            Some((
                "recorder".to_string(),
                "gpu-screen-recorder".to_string(),
                vec!["-w".to_string(), "DP-1".to_string()]
            ))
        );
    }

    #[test]
    fn start_without_arguments_is_a_bare_command_and_an_empty_one_is_refused() {
        assert_eq!(
            parse_start_args(&[json!("recorder"), json!("true")]),
            Some(("recorder".to_string(), "true".to_string(), Vec::new()))
        );
        assert_eq!(
            parse_start_args(&[json!("recorder"), json!("true"), json!({})]),
            Some(("recorder".to_string(), "true".to_string(), Vec::new()))
        );
        assert_eq!(parse_start_args(&[json!("recorder"), json!("")]), None);
        assert_eq!(parse_start_args(&[json!("recorder"), json!("cmd"), json!([1])]), None, "arguments are strings");
    }
}
