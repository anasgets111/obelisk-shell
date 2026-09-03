//! Argument parsing for the `oblisk` binary.
//!
//! Hand-rolled rather than derived. There are four flags and two subcommands, `clap` would be the
//! largest dependency in the workspace, and the one non-obvious rule here (`-c` may name a file,
//! and says so rather than silently taking the parent) is a line of code either way.

use std::path::{Path, PathBuf};

/// What the command line asked for.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Start the shell. The default with no arguments.
    Run,
    /// Write `.luarc.json` and the language-server stubs into the config directory.
    Init {
        force: bool,
    },
    /// Evaluate the config and report what it says, without taking a Wayland surface.
    Check,
    /// `set <name> <value>` or `toggle <name>`: write one of the running config's `state`
    /// signals from outside, for a compositor keybind (ADR-0112).
    SetState(shared::SetState),
    Version,
    Help,
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub command: Command,
    /// The `-c` directory, already absolute. `None` leaves `shared::config_dir()`'s own order in
    /// charge.
    pub config_dir: Option<PathBuf>,
}

pub const HELP: &str = "\
oblisk -- a Wayland desktop shell configured in Lua

USAGE:
    oblisk [OPTIONS]            start the shell
    oblisk init [OPTIONS]       set up a config directory for editing
    oblisk check [OPTIONS]      evaluate the config and exit
    oblisk set <NAME> <VALUE>   write the running config's state(NAME) signal
    oblisk toggle <NAME>        flip it, when it holds a boolean

OPTIONS:
    -c, --config <DIR>   the config directory, holding shell.lua. Overrides
                         $OBLISK_CONFIG_DIR and $XDG_CONFIG_HOME.
        --force          init only: overwrite files that already exist
    -V, --version
    -h, --help

The config is a directory, not a file: `require` resolves inside it, and the
shell reloads when any .lua file in it changes.

`set` and `toggle` are how a compositor keybind reaches a running config:
bind `oblisk toggle launcher_open` and the config's `state(\"launcher_open\",
false)` flips. VALUE is read as JSON (true, 3, \"text\", [1,2]); anything
that is not JSON is taken as a string, so quoting `notifications` is optional.
";

/// `-c` names a directory. A path to `shell.lua` is accepted, because that is the file someone was
/// editing when they reached for the flag, and the intent is unambiguous. It reports the
/// substitution rather than making it silently, since a config author who thinks the unit is a file
/// will be surprised by `require` and by the watcher later.
fn config_dir_from(raw: &str) -> Result<PathBuf, String> {
    let given = Path::new(raw);
    let dir = if given.is_file() {
        let parent = given
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| format!("--config {raw} is a file with no parent directory"))?;
        eprintln!("oblisk: --config takes a directory; using {} because {raw} is a file", parent.display());
        parent.to_path_buf()
    } else {
        given.to_path_buf()
    };
    // Absolute before it goes anywhere. Every Renderer inherits this through the environment, and
    // a spawned process is not promised the working directory this one had.
    std::path::absolute(&dir).map_err(|err| format!("--config {}: {err}", dir.display()))
}

pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> Result<Args, String> {
    let mut args = argv.into_iter().skip(1).peekable();
    let mut command = None;
    let mut config_dir = None;
    let mut force = false;
    let mut positional = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "init" | "check" | "set" | "toggle" if command.is_none() => {
                command = Some(match arg.as_str() {
                    "init" => "init",
                    "check" => "check",
                    "set" => "set",
                    _ => "toggle",
                });
            }
            // The words after `set`/`toggle`: a state name, and for `set` a value, which may
            // itself begin with a dash (`-1`) and so is taken before the flag arms below.
            _ if matches!(command, Some("set" | "toggle"))
                && (positional.is_empty() || command == Some("set") && positional.len() == 1) =>
            {
                positional.push(arg);
            }
            "-c" | "--config" => {
                let value = args.next().ok_or_else(|| "--config needs a directory".to_string())?;
                config_dir = Some(config_dir_from(&value)?);
            }
            "--force" => force = true,
            "-V" | "--version" => {
                return Ok(Args { command: Command::Version, config_dir });
            }
            "-h" | "--help" => {
                return Ok(Args { command: Command::Help, config_dir });
            }
            other => {
                if let Some(value) = other.strip_prefix("--config=") {
                    config_dir = Some(config_dir_from(value)?);
                } else {
                    return Err(format!("unknown argument {other}"));
                }
            }
        }
    }

    let command = match command {
        Some("init") => Command::Init { force },
        Some("check") => Command::Check,
        Some("set") => {
            let [name, value] = <[String; 2]>::try_from(positional)
                .map_err(|_| "set takes a state name and a value: `oblisk set launcher_open true`".to_string())?;
            // JSON when it parses, a string otherwise: `oblisk set panel_kind notifications` is
            // what someone types, and making them type `'"notifications"'` for it would be
            // pedantry in a keybind.
            let value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
            Command::SetState(shared::SetState { name, write: shared::StateWrite::Set(value) })
        }
        Some("toggle") => {
            let [name] = <[String; 1]>::try_from(positional)
                .map_err(|_| "toggle takes one state name: `oblisk toggle launcher_open`".to_string())?;
            Command::SetState(shared::SetState { name, write: shared::StateWrite::Toggle })
        }
        _ => Command::Run,
    };
    if force && !matches!(command, Command::Init { .. }) {
        return Err("--force is only meaningful with `init`".to_string());
    }
    Ok(Args { command, config_dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Args, String> {
        parse(std::iter::once("oblisk".to_string()).chain(args.iter().map(|a| (*a).to_string())))
    }

    #[test]
    fn no_arguments_runs_the_shell_against_the_default_config() {
        assert_eq!(parse_args(&[]).unwrap(), Args { command: Command::Run, config_dir: None });
    }

    #[test]
    fn a_config_directory_is_made_absolute() {
        let args = parse_args(&["-c", "dev-config/oblisk"]).unwrap();
        let dir = args.config_dir.expect("-c sets a directory");
        assert!(dir.is_absolute(), "a relative -c must be resolved before any Renderer inherits it");
        assert!(dir.ends_with("dev-config/oblisk"));
    }

    #[test]
    fn the_long_form_and_the_equals_form_agree() {
        let a = parse_args(&["--config", "dev-config/oblisk"]).unwrap();
        let b = parse_args(&["--config=dev-config/oblisk"]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_path_to_shell_lua_resolves_to_its_directory() {
        // The mistake worth accommodating: `-c ~/.config/oblisk/shell.lua` is what someone types
        // straight after editing that file.
        //
        // An absolute path, because the substitution turns on `is_file()` and a test's working
        // directory is its crate root rather than the workspace root.
        let shell_lua = format!("{}/../dev-config/oblisk/shell.lua", env!("CARGO_MANIFEST_DIR"));
        let args = parse_args(&["-c", &shell_lua]).unwrap();
        assert!(args.config_dir.unwrap().ends_with("dev-config/oblisk"));
    }

    #[test]
    fn subcommands_parse_with_their_own_options() {
        assert_eq!(parse_args(&["init"]).unwrap().command, Command::Init { force: false });
        assert_eq!(parse_args(&["init", "--force"]).unwrap().command, Command::Init { force: true });
        assert_eq!(parse_args(&["check"]).unwrap().command, Command::Check);
    }

    /// ADR-0112: the two verbs a keybind speaks. A value is JSON when it parses and a string when
    /// it does not, so a bare word needs no shell quoting.
    #[test]
    fn set_and_toggle_name_a_state_and_read_the_value_as_json_or_a_bare_string() {
        use shared::{SetState, StateWrite};
        assert_eq!(
            parse_args(&["set", "launcher_open", "true"]).unwrap().command,
            Command::SetState(SetState {
                name: "launcher_open".into(),
                write: StateWrite::Set(serde_json::json!(true))
            })
        );
        assert_eq!(
            parse_args(&["set", "panel_kind", "notifications"]).unwrap().command,
            Command::SetState(SetState {
                name: "panel_kind".into(),
                write: StateWrite::Set(serde_json::json!("notifications"))
            })
        );
        assert_eq!(
            parse_args(&["set", "volume_step", "-5"]).unwrap().command,
            Command::SetState(SetState { name: "volume_step".into(), write: StateWrite::Set(serde_json::json!(-5)) }),
            "a negative number is a value, not a flag"
        );
        assert_eq!(
            parse_args(&["toggle", "launcher_open"]).unwrap().command,
            Command::SetState(SetState { name: "launcher_open".into(), write: StateWrite::Toggle })
        );
        assert!(parse_args(&["set", "launcher_open"]).is_err(), "set without a value");
        assert!(parse_args(&["toggle"]).is_err(), "toggle without a name");
        assert!(parse_args(&["toggle", "a", "b"]).is_err(), "toggle with a value has misread the verb");
    }

    #[test]
    fn force_without_init_is_refused_rather_than_ignored() {
        assert!(parse_args(&["--force"]).is_err(), "a flag that does nothing is worse than an error");
    }

    #[test]
    fn config_needs_a_value_and_an_unknown_flag_is_an_error() {
        assert!(parse_args(&["-c"]).is_err());
        assert!(parse_args(&["--colour"]).is_err());
    }

    #[test]
    fn version_and_help_win_over_anything_after_them() {
        assert_eq!(parse_args(&["--version", "init"]).unwrap().command, Command::Version);
        assert_eq!(parse_args(&["-h"]).unwrap().command, Command::Help);
    }
}
