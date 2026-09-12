-- Developer tooling updated behind the package manager, ported from `~/.local/bin/update`'s
-- optional steps. `modules/bar/panels/update_panel.lua` runs them and stores which are ticked.
--
-- Data, not code. A tool is a name, the binary that has to exist for it to apply, and the commands
-- to run in order. Adding one is an entry here; the Supervisor never learns these exist, so no
-- rebuild and no capability change.
--
-- `run` is a list because a tool's workflow is rarely one command. It stops at the first non-zero
-- exit, which is what makes a failed `fnm install` skip the prune that would act on the old list.
--
-- Entries are argv, so a tool only resolves if it is on the `PATH` the shell was started with. A
-- login shell does not widen that: this machine keeps its extra entries in `config.fish`, which no
-- `bash -lc` reads. A tool the session cannot see hides its row rather than failing at run time.
--
-- Every command runs as the user. Only the package manager gets `pkexec`: `composer global update`
-- as root leaves `~/.config/composer` owned by root, and the next unprivileged run cannot write it.
return {
    { name = "composer",       requires = "composer",             run = { { "composer", "global", "update" } } },
    {
        name = "node",
        requires = "fnm",
        -- Take the current LTS, make it the default, then drop every other version `fnm` manages.
        -- The prune needs a shell for its pipeline; the two before it do not, so they do not get one.
        run = {
            { "fnm", "install", "--use",     "lts-latest" },
            { "fnm", "default", "lts-latest" },
            {
                "bash",
                "-c",
                'fnm ls | awk -v current="$(fnm current)" \'$2 != "system" && $2 != current { print $2 }\''
                .. " | xargs -r -n1 fnm uninstall",
            },
        },
    },
    { name = "rust binaries",  requires = "cargo-install-update", run = { { "cargo", "install-update", "-a" } } },
    { name = "rust toolchain", requires = "rustup",               run = { { "rustup", "update" } } },
}
