//! Argument parsing for the `obelisk` binary.
//!
//! Hand-rolled: four flags, two subcommands, and one non-obvious rule (`-c` may name a file).
//! `clap` would be the workspace's largest dependency; the rule needs custom code either way.

use std::path::{Path, PathBuf};

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
    /// `set <name> <value>`, `toggle <name>` or `toggle <name> <value>` writes a running config's
    /// `state` signal from outside for a compositor keybind (ADR-0112).
    SetState(shared::SetState),
    Version,
    Help,
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub command: Command,
    /// Absolute `-c` directory. `None` leaves `shared::config_dir()`'s own order in charge.
    pub config_dir: Option<PathBuf>,
}

pub const HELP: &str = "\
obelisk -- a Wayland desktop shell configured in Lua

USAGE:
    obelisk [OPTIONS]            start the shell
    obelisk init [OPTIONS]       set up a config directory for editing
    obelisk check [OPTIONS]      evaluate the config and exit
    obelisk set <NAME> <VALUE>   write the running config's state(NAME) signal
    obelisk toggle <NAME>        flip it, when it holds a boolean
    obelisk toggle <NAME> <VALUE>
                                set it to VALUE, or back to its declared
                                initial when it already is VALUE

OPTIONS:
    -c, --config <DIR>   the config directory, holding shell.lua. Overrides
                         $OBELISK_CONFIG_DIR and $XDG_CONFIG_HOME.
        --force          init only: overwrite files that already exist
    -V, --version
    -h, --help

The config is a directory, not a file: `require` resolves inside it, and the
shell reloads when any .lua file in it changes.

`set` and `toggle` are how a compositor keybind reaches a running config:
bind `obelisk toggle launcher_open` and the config's `state(\"launcher_open\",
false)` flips; bind `obelisk toggle modal launcher` and `state(\"modal\", \"\")`
becomes \"launcher\", or \"\" again when it already was. VALUE is read as JSON
(true, 3, \"text\", [1,2]); anything that is not JSON is taken as a string, so
quoting `notifications` is optional.
";

/// `-c` names a directory, but accepts a path to `shell.lua` because that is what someone reaches
/// for after editing it. Report the substitution: `require` and the watcher use the directory.
fn config_dir_from(raw: &str) -> Result<PathBuf, String> {
    let given = Path::new(raw);
    let dir = if given.is_file() {
        let parent = given
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| format!("--config {raw} is a file with no parent directory"))?;
        eprintln!("obelisk: --config takes a directory; using {} because {raw} is a file", parent.display());
        parent.to_path_buf()
    } else {
        given.to_path_buf()
    };
    // Resolve before handing it to Renderer through the environment; spawned processes need not
    // share this process's working directory.
    std::path::absolute(&dir).map_err(|err| format!("--config {}: {err}", dir.display()))
}

/// Whether `arg` is one of the options this parser knows, rather than a value that merely begins
/// with a dash. `-1` is a `set` value; `-c` is an option even where a value is expected.
fn is_option(arg: &str) -> bool {
    matches!(arg, "-c" | "--config" | "--force" | "-V" | "--version" | "-h" | "--help") || arg.starts_with("--config=")
}

pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> Result<Args, String> {
    let mut args = argv.into_iter().skip(1);
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
            // Take the state name and `set` value before flags; a value may begin with a dash
            // (`-1`). An option the parser knows is still an option in that slot, or
            // `obelisk toggle open -c /dir` would store the flag as the value and then choke on the
            // directory.
            _ if matches!(command, Some("set" | "toggle")) && positional.len() < 2 && !is_option(&arg) => {
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
                .map_err(|_| "set takes a state name and a value: `obelisk set launcher_open true`".to_string())?;
            // Parse JSON when possible; bare words stay strings, so keybinds need no extra quotes.
            let value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
            Command::SetState(shared::SetState { name, write: shared::StateWrite::Set(value) })
        }
        Some("toggle") => {
            let mut positional = positional.into_iter();
            let name = positional
                .next()
                .ok_or_else(|| "toggle takes a state name: `obelisk toggle launcher_open`".to_string())?;
            let write = match positional.next() {
                // The same reading as `set`: JSON when it parses, a string otherwise.
                Some(value) => shared::StateWrite::ToggleTo(
                    serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value)),
                ),
                None => shared::StateWrite::Toggle,
            };
            Command::SetState(shared::SetState { name, write })
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
        parse(std::iter::once("obelisk".to_string()).chain(args.iter().map(|a| (*a).to_string())))
    }

    /// The state name and value are taken before flags so a value like `-5` is not read as one,
    /// but an option the parser knows is still an option in that slot. Taking `-c` as the toggle
    /// value stored a flag in the state and then failed on the directory behind it.
    #[test]
    fn an_option_after_a_state_name_is_still_an_option() {
        use shared::{SetState, StateWrite};
        let dir = std::env::temp_dir();
        let path = dir.to_str().unwrap();
        let joined = format!("--config={path}");
        for args in [
            vec!["toggle", "launcher_open", "-c", path],
            vec!["toggle", "launcher_open", "--config", path],
            // The joined form is one argument, so it is the one a bare "starts with a dash" test
            // would wave through as the toggle's value.
            vec!["toggle", "launcher_open", joined.as_str()],
        ] {
            let parsed = parse_args(&args).unwrap();
            assert_eq!(
                parsed.command,
                Command::SetState(SetState { name: "launcher_open".into(), write: StateWrite::Toggle }),
                "{args:?}"
            );
            assert_eq!(parsed.config_dir.as_deref(), Some(std::path::Path::new(path)), "{args:?}");
        }
        // A value that merely begins with a dash is still a value.
        let parsed = parse_args(&["set", "volume_step", "-5"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::SetState(SetState { name: "volume_step".into(), write: StateWrite::Set(serde_json::json!(-5)) })
        );
    }

    #[test]
    fn no_arguments_runs_the_shell_against_the_default_config() {
        assert_eq!(parse_args(&[]).unwrap(), Args { command: Command::Run, config_dir: None });
    }

    #[test]
    fn a_config_directory_is_made_absolute() {
        let args = parse_args(&["-c", "dev-config/obelisk"]).unwrap();
        let dir = args.config_dir.expect("-c sets a directory");
        assert!(dir.is_absolute(), "a relative -c must be resolved before any Renderer inherits it");
        assert!(dir.ends_with("dev-config/obelisk"));
    }

    #[test]
    fn the_long_form_and_the_equals_form_agree() {
        let a = parse_args(&["--config", "dev-config/obelisk"]).unwrap();
        let b = parse_args(&["--config=dev-config/obelisk"]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_path_to_shell_lua_resolves_to_its_directory() {
        // Accommodate the common `-c ~/.config/obelisk/shell.lua` after editing that file.
        //
        // A file this test makes, not the shipped config: the rule under test is "a path to a file
        // resolves to its parent", which has nothing to do with what the sample happens to contain.
        // Absolute, because `is_file()` has to see it and tests run from the crate root.
        let dir = tempfile::tempdir().unwrap();
        let shell_lua = dir.path().join("shell.lua");
        std::fs::write(&shell_lua, "return {}").unwrap();
        let args = parse_args(&["-c", shell_lua.to_str().unwrap()]).unwrap();
        assert_eq!(args.config_dir.as_deref(), Some(dir.path()));
    }

    #[test]
    fn subcommands_parse_with_their_own_options() {
        assert_eq!(parse_args(&["init"]).unwrap().command, Command::Init { force: false });
        assert_eq!(parse_args(&["init", "--force"]).unwrap().command, Command::Init { force: true });
        assert_eq!(parse_args(&["check"]).unwrap().command, Command::Check);
    }

    /// ADR-0112: keybind verbs. Values parse as JSON when possible; bare words need no quotes.
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
        assert_eq!(
            parse_args(&["toggle", "modal", "launcher"]).unwrap().command,
            Command::SetState(SetState {
                name: "modal".into(),
                write: StateWrite::ToggleTo(serde_json::json!("launcher"))
            }),
            "a toggle with a value is a toggle to it"
        );
        assert!(parse_args(&["set", "launcher_open"]).is_err(), "set without a value");
        assert!(parse_args(&["toggle"]).is_err(), "toggle without a name");
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
