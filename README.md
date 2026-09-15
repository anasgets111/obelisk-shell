# Obelisk

A Wayland desktop shell you write in Lua. You declare the bars, popups, launcher and lock screen as
a tree of nodes; Rust owns the platform connections, input, layout and painting.

Status: pre-release. Nothing is published, and the Lua API changes without notice.



https://github.com/user-attachments/assets/770bd04b-bc43-43b9-a388-eccda8d9528f



https://github.com/user-attachments/assets/038ee763-d7b6-4df9-9f79-2f131d4f0dcd




## Why processes

A shell reloads on every config save, and the reloaded UI has to release what the old one held.
Most shells use a garbage collector for that. Obelisk uses the kernel.

A **generation** is one Renderer process and its Lua state. An edit that changes the surface
topology starts a second Renderer, waits for it to prove it has painted, transfers authority per
output, then retires the first. The retired process exits, and its memory, textures and font
atlases go back to the kernel whether or not anything tracked them.

An edit that leaves the topology alone skips all of that and re-evaluates in place, keeping Lua
state and reconciling the retained scene.

## Requirements

| What | Needs |
| :--- | :--- |
| Compositor | Wayland with `wlr-layer-shell-v1` and `ext-session-lock-v1` |
| Blur | `ext-background-effect-v1`, ignored where absent |
| Workspaces, keyboard layout | niri or Hyprland |
| Updates capability | pacman, through libalpm |
| Linked at build | PipeWire, PAM, udev, EGL, xkbcommon, libwayland-client, libwayland-egl |

Lua 5.4 is vendored, so no system Lua is needed.

## Build and run

```sh
just build                              # both binaries into target/debug
just check                              # fmt, tests, clippy, doc links, Lua parse and types
just install PREFIX=/usr DESTDIR="$pkgdir"

obelisk init     # writes shell.lua, plus a .luarc.json pointing the LSP at the stubs
obelisk          # run it
obelisk check    # evaluate the config and exit, taking no surface
```

The config is a directory, not a file: `require` resolves inside it, and any `.lua` file changing
triggers a reload. `-c DIR` beats `$OBELISK_CONFIG_DIR`, which beats `$XDG_CONFIG_HOME/obelisk`. A debug build
tries this repo's `dev-config/obelisk` right after `-c`.

## A config

```lua
return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true,
        height = 34,
        background = "#1e1e2e80",
        child = text {
            content = obelisk.system:map(function(s)
                return os.date("%H:%M", s and s.time)
            end),
            foreground = "#cdd6f4ff",
        },
    },
}
```

Surfaces are `panel`, `window`, `popup`, `lock`. Nodes are `row`, `column`, `text`, `image`,
`icon`, `button`, `textfield`, `rect`, `list`. Anything that changes over time is a
signal, so the `:map` above re-resolves that clock without re-running the config.

## Capabilities

`obelisk.<name>` exposes platform state as a signal and takes actions. A backend starts on first use
and stays for the session.

applications, audio, battery, bluetooth, brightness, files, idle, keyboard, lock, mpris, network,
notifications, polkit, power, privacy, processes, storage, sysinfo, system, tray, updates, workspaces.

## Keybinds

`set` and `toggle` write a running config's named state from outside, which is how a compositor
keybind reaches it. VALUE is read as JSON, and anything that is not JSON is taken as a string.

```sh
obelisk toggle launcher_open     # flips state("launcher_open", false)
obelisk toggle modal launcher    # sets state("modal", ""), or clears it if already "launcher"
```

## Docs

| Doc | Holds |
| :--- | :--- |
| [Lua API](docs/lua-api.md) | what a config can declare and call |
| [Services](docs/services.md) | capability payloads, actions and their backends |
| [Decisions](docs/decisions.md) | why it is built this way, including what was rejected |
| [Roadmap](docs/roadmap.md) | known gaps, and what is deliberately out of scope |
| [CONTEXT.md](CONTEXT.md) | the vocabulary all four use |

## License

MIT.
