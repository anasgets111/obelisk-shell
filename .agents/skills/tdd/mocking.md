# When to mock

Only at system seams: the compositor, D-Bus backends, sysfs/procfs, time, process launch and signals. Never
your own modules.

How this repo does it:

- **sysfs/procfs and D-Bus:** see `AGENTS.md` § Testing.
- **Environment:** pass the values in (`shared::paths::config_dir_from`); `set_var` races every concurrent
  `getenv`.
- **Lua:** evaluate an inline fixture in a tempdir, never `dev-config`.

Prefer specific adapter operations (`play`, `pause`) over one generic `dispatch(command)`: the mock then needs
no routing logic.
