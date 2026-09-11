//! `obelisk check`: evaluate config, report declared surfaces, and exit through the Renderer's Lua
//! loader. The Supervisor has no `mlua` runtime, so it re-execs this binary with
//! `shared::CHECK_ENV` and forwards the exit code.
//!
//! No Wayland, surfaces, or GPU. Matches pre-first-`StateSnapshot` evaluation: every capability
//! signal reads `nil` (ADR-0044).

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
            "{}: no shell.lua. `obelisk init -c {}` writes one.",
            shell_lua.display(),
            config_dir.display()
        ));
    }

    let dirty = DirtyFlag::new();
    let loader = Loader::new(dirty.clone(), config_dir).map_err(|err| format!("{}: {err}", shell_lua.display()))?;

    // Register `obelisk` and `process.run`: configs reach for both during evaluation, and a bare
    // `Loader` dies on the first `obelisk.` access. Capabilities read `nil`, as at real boot before
    // the first snapshot.
    //
    // Frames go into an undrained channel: without a Supervisor, `process.run` has nowhere to run.
    // Correct for a checker that evaluates, but does not start, a config.
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

    /// The shipped dev config spans thirty-odd files joined by `require`, so this checks that
    /// `obelisk check` sees what a real boot sees.
    #[test]
    fn checking_the_shipped_dev_config_reports_its_surfaces() {
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/obelisk");
        let report = super::run(&config).expect("the shipped dev config must evaluate");
        assert!(report.contains("ok,"), "{report}");
        assert!(report.contains("panel   bar"), "the bar must be in the report:\n{report}");
    }

    #[test]
    fn a_directory_with_no_shell_lua_says_so_and_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("no shell.lua"), "{err}");
        assert!(err.contains("obelisk init"), "an error a new user hits should name the way out: {err}");
    }

    #[test]
    fn a_config_that_does_not_evaluate_reports_the_lua_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), "return { panel { id = 1 } }\n").unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("shell.lua"), "the error must name the file: {err}");
    }
}
