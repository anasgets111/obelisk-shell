# Recipes for building, running and gating Obelisk. `just` alone runs `check`.
#
# `run` depends on `build` because the Supervisor finds the Renderer as a filesystem sibling
# (`supervisor/src/generation.rs`), not as a Cargo dependency. `cargo run -p supervisor` rebuilds
# half the stack, launches whatever `target/debug/obelisk-renderer` happens to be, and reports the
# mismatch as a config error in `shell.lua`, the last place the fault is.
#
# `just --list` shows only the last comment line, hence `[doc(...)]` on the multi-line blocks.

default: check

# Both binaries. Required before `run`.
build:
    cargo build --workspace

release:
    cargo build --workspace --release

# The dev shell against `dev-config/obelisk`, on the binaries just built.
run: build
    OBELISK_CONFIG_DIR=dev-config/obelisk target/debug/obelisk

# Everything a change has to pass before it is done.
check: fmt-check test lint docs lua types

# Once per clone: git does not version `.git/hooks`.
hooks:
    git config core.hooksPath .githooks
    @echo "core.hooksPath -> .githooks"

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Unresolved intra-doc links, which clippy does not check. The baseline is exact in both
# directions: over it hides a link that moved, under it means someone fixed links and never
# committed the smaller number, leaving room for the next regression to sit in unreported.
[doc('Unresolved intra-doc links, against an exact per-crate baseline.')]
docs:
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(cargo doc --workspace --no-deps 2>&1)
    for pair in renderer:3 supervisor:23; do
        crate=${pair%:*} baseline=${pair#*:}
        count=$(echo "$out" | grep -A1 "unresolved link" | grep -cE "^\s*--> $crate/" || true)
        if [ "$count" -ne "$baseline" ]; then
            echo "$crate: $count unresolved doc links, baseline $baseline. Over: demote the link to a plain backtick path with the module prefix, never widen visibility for rustdoc. Under: lower the baseline here." >&2
            echo "$out" | grep -B1 -A2 "unresolved link" >&2
            exit 1
        fi
        echo "$crate: $count unresolved doc links (baseline $baseline)"
    done

# `lua` proves a file parses. This proves `dev-config` and `share/starter` agree with `lua-meta`,
# through the engine and `.luarc.json` the author's editor uses. It is why `nodes.lua` spells
# `|Signal` on all 21 unions that take one (ADR-0081).
#
# `lua-meta` is also checked alone, because a library's own diagnostics are suppressed. That hid
# `---@return Signal Read-only, like `map``, where the comma made `like` a second return type.
# Single-return prose is written `---@return T # ...`.
#
# Missing server is a failure, not a skip: a skip once let `just check` go green having checked no
# stub. The Zed glob is the only other copy on this machine.
[doc('The config and the stubs type-checked against each other.')]
types:
    #!/usr/bin/env bash
    set -euo pipefail
    luals=$(command -v lua-language-server 2>/dev/null || true)
    if [ -z "$luals" ]; then
        luals=$(ls -d ~/.local/share/zed/extensions/work/lua/lua-language-server-*/bin/lua-language-server 2>/dev/null | sort -V | tail -1 || true)
    fi
    if [ -z "$luals" ]; then
        echo "no lua-language-server on PATH or in Zed's extensions. Install it: pacman -S lua-language-server" >&2
        exit 1
    fi
    log=$(mktemp -d)
    trap 'rm -rf "$log"' EXIT
    # `share/starter` ships no `.luarc.json`: `obelisk init` writes one pointing at the *installed*
    # stubs (`setup.rs`'s `luarc_json`), which would overwrite a checked-in copy. Absolute library
    # path, because a relative one resolves against the workspace being checked.
    printf '{"runtime.version":"Lua 5.4","workspace.library":["%s/lua-meta"],"workspace.checkThirdParty":false}\n' "$PWD" >"$log/starter.luarc.json"
    # `--check` exits non-zero and prints file, line, column, source line and caret run to stdout,
    # mixed with a progress bar it redraws with carriage returns. Capture it, replay it without the
    # progress chunks only on failure. Not `--check_format=json`: the human form carries the source
    # line, and the JSON report block sat dead for months because nobody passed that flag.
    check() {
        local out
        if out=$("$luals" --check "$PWD/$1" --checklevel=Warning --logpath="$log" "${@:2}" 2>&1); then
            return 0
        fi
        echo "$1 has type diagnostics:" >&2
        printf '%s' "$out" | tr '\r' '\n' |
            sed -E '/^[[:space:]]*$/d; /^[[:space:]]*Initializing/d; /^[[:space:]]*[>=]+[[:space:]]*[0-9]+\/[0-9]+/d; /^[[:space:]]*Diagnosis complet/d' >&2
        exit 1
    }
    # No `--configpath`: `dev-config/obelisk` has its own, carrying the `runtime.path` its
    # `require`s need.
    check dev-config/obelisk
    check share/starter --configpath "$log/starter.luarc.json"
    # No library, which is the point: these files declare everything they reference.
    printf '{"runtime.version":"Lua 5.4","workspace.checkThirdParty":false}\n' >"$log/meta.luarc.json"
    check lua-meta --configpath "$log/meta.luarc.json"
    echo "lua-meta type-checks, and dev-config and the starter type-check against it"

lua_dirs := "dev-config lua-meta share"

# The formatter is here, not beside `cargo fmt`, so `lua_dirs` is written once and a Lua-only
# commit is gated by `just lua types` alone (`.githooks/pre-commit`). `tools/luafmt.py` says why
# the formatter is a language server; `.editorconfig` holds its rules.
[doc('Every Lua file parses and is formatted.')]
lua:
    #!/usr/bin/env bash
    set -euo pipefail
    find {{lua_dirs}} -name '*.lua' -print0 | xargs -0 -n1 luac -p
    echo "all lua parses"
    python3 tools/luafmt.py --check {{lua_dirs}}

# Regenerate `lua-meta/obelisk.lua` from the supervisor's payload types, then show what moved.
stubs:
    UPDATE_STUBS=1 cargo test -p supervisor stubs
    @git diff --stat -- lua-meta/obelisk.lua

# Separate from `lint` because a diff and a warning fail differently, and folding them buries the
# diff. 71266cb and c83e79e landed four unformatted files with `just check` green on both.
[doc('rustfmt as a gate. `just fmt` fixes it.')]
fmt-check:
    cargo fmt --all -- --check

# Both languages, unlike the gates, because nobody wants two commands to fix a diff.
fmt:
    cargo fmt --all
    python3 tools/luafmt.py {{lua_dirs}}

clean:
    cargo clean

# Install layout, for a real install only. `just run` is the dev path and reads `target/debug` and
# the repo's own `lua-meta`, so nothing here is on it.
#
# `prefix` is where it goes, `destdir` stages it for a package:
# `just --no-deps prefix=/usr destdir="$pkgdir" install`. Overrides are case-sensitive and go
# before the recipe name; both are errors, not silent defaults.
#
# Everything under `$PREFIX` and nothing else. `packaging/pam.d/obelisk` goes to /etc and the
# licence to `share/licenses`, so a package installs those two itself.
#
# The Renderer sits in `lib/obelisk`, off `$PATH`, and `bin/obelisk` is a symlink into it:
# `current_exe` reads symlink-resolved `/proc/self/exe`, so the Supervisor still finds its sibling.
# One command on `$PATH`, and the pair cannot drift apart.
#
# No service unit. The compositor starts it: `spawn-at-startup "obelisk"` in niri,
# `exec-once = obelisk` in Hyprland, `exec obelisk` in sway.
prefix := "/usr/local"
destdir := ""
# `assert` because `just prefix= uninstall` would otherwise `rm -rf /lib/obelisk /share/obelisk`.
root := assert(prefix != "", "prefix must not be empty") + destdir + prefix

[doc('Install under `prefix`, staged into `destdir`.')]
install: release
    install -Dm755 target/release/obelisk          "{{root}}/lib/obelisk/obelisk"
    install -Dm755 target/release/obelisk-renderer "{{root}}/lib/obelisk/obelisk-renderer"
    install -dm755                                 "{{root}}/bin"
    ln -sfn ../lib/obelisk/obelisk                 "{{root}}/bin/obelisk"
    install -Dm644 -t "{{root}}/share/obelisk/lua-meta" lua-meta/*.lua
    install -Dm644 share/starter/shell.lua         "{{root}}/share/obelisk/starter/shell.lua"

[doc('Remove what `install` put under `prefix`.')]
uninstall:
    rm -rf "{{root}}/lib/obelisk" "{{root}}/share/obelisk" "{{root}}/bin/obelisk"
