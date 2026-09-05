//! `oblisk check`: evaluate config, report declared surfaces, and exit.
//!
//! Evaluates via the Renderer's Lua loader. The Supervisor has no `mlua` runtime, so it re-execs
//! the renderer binary with `shared::CHECK_ENV` set and forwards the exit code.
//!
//! Runs without Wayland, surfaces, or GPU. Matches the cold evaluation before the first
//! `StateSnapshot` arrives: every capability signal reads `nil` (ADR-0044).

use std::path::Path;

use crate::layout::node::SurfaceSpec;
use crate::lua::capability::CommandSender;
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::DirtyFlag;
use crate::lua::{Loader, namespace, surfaces::evaluate_and_specs};

fn role_of(spec: &SurfaceSpec) -> &'static str {
    match spec {
        SurfaceSpec::Panel(_) => "panel",
        SurfaceSpec::Window(_) => "window",
        SurfaceSpec::Popup(_) => "popup",
        SurfaceSpec::Lock(_) => "lock",
    }
}

/// Evaluates `shell.lua` under `config_dir` and returns the report, or the error a config author
/// needs to read.
pub fn run(config_dir: &Path) -> Result<String, String> {
    let shell_lua = config_dir.join("shell.lua");
    if !shell_lua.is_file() {
        return Err(format!(
            "{}: no shell.lua. `oblisk init -c {}` writes one.",
            shell_lua.display(),
            config_dir.display()
        ));
    }

    let dirty = DirtyFlag::new();
    let loader = Loader::new(dirty.clone(), config_dir).map_err(|err| format!("{}: {err}", shell_lua.display()))?;

    // The `oblisk` namespace and `process.run`, because a config reaches for both at evaluation
    // time and a bare `Loader` dies on the first `oblisk.` anything. Every capability reads `nil`
    // here, which is the state a real boot evaluates in too: no snapshot has arrived yet.
    //
    // Frames go into a channel nobody drains. There is no Supervisor to send them to, and a
    // `process.run` fired during evaluation has nowhere to run. That is correct: this evaluates a
    // config, it does not start one.
    let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let commands = CommandSender::new(0, outbound_tx.clone());
    loader.register_process(ProcessRegistry::new(0, outbound_tx)).map_err(|err| err.to_string())?;
    namespace::build(&loader, &dirty, &commands, &shell_lua).map_err(|err| err.to_string())?;

    let (_output, specs) =
        evaluate_and_specs(&loader, &shell_lua).map_err(|err| format!("{}: {err}", shell_lua.display()))?;

    let mut report = format!("{}: ok, {} surface(s)\n", shell_lua.display(), specs.len());
    for spec in &specs {
        report.push_str(&format!("  {:<7} {}\n", role_of(spec), spec.declared_id()));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Against the shipped dev config, which is thirty-odd files reaching each other through
    /// `require`, so this is also a check that `oblisk check` sees what a real boot sees.
    #[test]
    fn checking_the_shipped_dev_config_reports_its_surfaces() {
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk");
        let report = super::run(&config).expect("the shipped dev config must evaluate");
        assert!(report.contains("ok,"), "{report}");
        assert!(report.contains("panel   bar"), "the bar must be in the report:\n{report}");
    }

    #[test]
    fn a_directory_with_no_shell_lua_says_so_and_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("no shell.lua"), "{err}");
        assert!(err.contains("oblisk init"), "an error a new user hits should name the way out: {err}");
    }

    #[test]
    fn a_config_that_does_not_evaluate_reports_the_lua_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), "return { panel { id = 1 } }\n").unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("shell.lua"), "the error must name the file: {err}");
    }
}
