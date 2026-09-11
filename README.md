# Oblisk

A Wayland desktop shell you write in Lua. You declare bars, popups, notification cards, a launcher
and a lock screen as a tree of nodes; Rust owns the platform connections, input, layout and
painting.

Status: pre-release. Nothing is published, and the Lua API changes without notice.

## Why processes

A shell reloads on every config save, and a reloaded UI has to release what the old one held. Most
shells solve that with a garbage collector. Oblisk solves it with the kernel.

A **generation** is one Renderer process and its Lua state. When a config edit changes the surface
topology, the Supervisor starts a second Renderer, waits for it to prove it has painted, transfers
authority per output, then retires the first. The retired process exits and its memory, GPU
textures, font atlases and protocol objects go back to the kernel, whether or not anything tracked
them.

Edits that leave the topology alone skip all of that and re-evaluate in place, keeping Lua state and
reconciling the retained scene.

## Requirements

| | |
| :--- | :--- |
| Compositor | Wayland with `wlr-layer-shell-v1` and `ext-session-lock-v1` |
| Blur | `ext-background-effect-v1`, ignored where absent |
| Workspaces, keyboard layout | niri or Hyprland only |
| Updates capability | pacman, through libalpm |
| Also linked | PipeWire, PAM, udev, evdev, EGL, fontconfig |

Lua 5.4 is vendored, so no system Lua is needed.

## Build

```sh
just build          # both binaries into target/debug
just check          # fmt, tests, clippy, doc links, Lua parse and types
just install PREFIX=/usr DESTDIR="$pkgdir"
```

`oblisk` finds `oblisk-renderer` as a filesystem sibling, not through Cargo, so build both. `just
run` depends on `build` for that reason.

## Start

```sh
oblisk init                  # writes shell.lua and .luarc.json into the config dir
oblisk                       # run it
oblisk check                 # evaluate the config and exit, taking no surface
```

The config is a directory, not a file: `require` resolves inside it, and any `.lua` file changing
triggers a reload. `-c DIR` beats `$OBLISK_CONFIG_DIR`, which beats `$XDG_CONFIG_HOME/oblisk`.

`oblisk init` also writes a `.luarc.json` pointing lua-language-server at the generated stubs, so an
editor completes and type-checks the config.

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
            content = oblisk.system:map(function(s)
                return os.date("%H:%M", s and s.time)
            end),
            foreground = "#cdd6f4ff",
        },
    },
}
```

Four surface roles: `panel`, `window`, `popup`, `lock`. Nodes are `row`, `column`, `text`, `image`,
`icon`, `button`, `textfield`, `scroll`, `rect`, `list`. Values that change over time are signals,
so `:map` above re-resolves that text without re-running the config.

## Capabilities

`oblisk.<name>` exposes platform state as a signal and takes actions:

applications, audio, battery, bluetooth, brightness, files, idle, keyboard, lock, mpris, network,
notifications, power, privacy, processes, storage, sysinfo, system, tray, updates, workspaces.

A capability's backend starts on first use and stays for the session.

## Keybinds

`set` and `toggle` write a running config's named state from outside, which is how a compositor
keybind reaches it:

```sh
oblisk toggle launcher_open        # flips state("launcher_open", false)
oblisk toggle modal launcher       # sets state("modal", ""), or clears it if already "launcher"
oblisk set volume_step 5
```

VALUE is read as JSON, and anything that is not JSON is taken as a string.

## Docs

| | |
| :--- | :--- |
| [Lua API](docs/lua-api.md) | what a config can declare and call |
| [Capabilities](docs/services.md) | payloads, actions and the backends behind them |
| [Decisions](docs/decisions.md) | why it is built this way, including what was rejected |
| [Roadmap](docs/roadmap.md) | known gaps, and what is deliberately out of scope |
| [CONTEXT.md](CONTEXT.md) | the vocabulary these docs use |

## License

MIT.
