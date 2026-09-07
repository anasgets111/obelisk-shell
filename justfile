# Recipes for building, running and gating Oblisk.
#
# Two of these exist because doing them by hand goes wrong in ways nothing else catches.
#
# `run` depends on `build` because the Supervisor finds the Renderer as a filesystem sibling of its
# own binary (`supervisor/src/generation.rs`'s `renderer_binary_path`), not as a Cargo dependency.
# `cargo run -p supervisor` therefore rebuilds one half of the stack and launches whatever
# `target/debug/renderer` happens to be, and a stale Renderer reports the mismatch as a *config*
# error pointing at `shell.lua`, which is the last place the fault actually is.
#
# `docs` exists because clippy does not check intra-doc links. A doc comment pointing at an item
# that moved a module away passes every other gate silently. The baselines below predate the check;
# fix a new one by demoting the link to a plain backtick path, never by widening visibility.
#
# `lua` catches a stub that does not parse, which the language server ignores with no error anywhere.

default: check

# Both binaries. Required before `run`, and not optional.
build:
    cargo build --workspace

release:
    cargo build --workspace --release

# The dev shell against `dev-config/oblisk`, on the binaries just built.
run: build
    OBLISK_CONFIG_DIR=dev-config/oblisk target/debug/oblisk

# Everything a change has to pass before it is done.
check: fmt-check test lint docs lua types

# Point git at the tracked hooks in `.githooks`. Once per clone: git does not version `.git/hooks`,
# so a hook only exists for whoever ran this.
hooks:
    git config core.hooksPath .githooks
    @echo "core.hooksPath -> .githooks"

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

renderer_doc_baseline := "3"
supervisor_doc_baseline := "24"

# Unresolved intra-doc links per crate, against a baseline of "no new ones" rather than zero.
docs:
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(cargo doc --workspace --no-deps 2>&1)
    for crate in renderer supervisor; do
        baseline=$([ "$crate" = renderer ] && echo {{renderer_doc_baseline}} || echo {{supervisor_doc_baseline}})
        count=$(echo "$out" | grep -A1 "unresolved link" | grep -cE "^\s*--> $crate/" || true)
        if [ "$count" -gt "$baseline" ]; then
            echo "$crate: $count unresolved doc links, baseline $baseline. Fix by demoting the link to a plain backtick path with the module prefix, never by widening visibility to satisfy rustdoc." >&2
            echo "$out" | grep -B1 -A2 "unresolved link" >&2
            exit 1
        fi
        echo "$crate: $count unresolved doc links (baseline $baseline)"
    done

# The config type-checked against the stubs, and the stubs type-checked against themselves.
#
# `lua` above proves a file parses. This proves `dev-config` agrees with `lua-meta`, which is what
# an author's editor will tell them: same engine, same `.luarc.json`, same stub directory. It is
# the reason `lua-meta/nodes.lua` now spells `|Signal` on every union that takes one -- 21 of them
# did not, and each was a red squiggle under working config code (ADR-0081).
#
# `lua-meta` is checked as its own workspace as well as being the others' library, because a
# library's own diagnostics are suppressed. That hole hid a real one: `---@return T a, b` is two
# returns, so a comma in a single return's prose makes the next word a type, and
# `---@return Signal Read-only, like `map`` declared a return of type `like`. Checking the config
# said nothing, because the config was fine. Single-return prose is written `---@return T # ...`.
#
# Optional, because `lua-language-server` is not a build dependency of this workspace and there is
# no CI to install it into. Missing means skipped and said so, never a silent pass.
#
# The PATH lookup falls back to the copy Zed's Lua extension downloads for itself. Not cleverness
# for its own sake: that is the only copy on the machine this was written on, so the check reported
# "skipping" on every run for as long as it existed, and the hole `dev-config/oblisk/.luarc.json`'s
# promoted diagnostics exist to close was open the whole time. Newest version wins; the glob is
# there so a Zed update does not silently turn the check back off.
types:
    #!/usr/bin/env bash
    set -euo pipefail
    luals=$(command -v lua-language-server 2>/dev/null || true)
    if [ -z "$luals" ]; then
        luals=$(ls -d ~/.local/share/zed/extensions/work/lua/lua-language-server-*/bin/lua-language-server 2>/dev/null | sort -V | tail -1 || true)
    fi
    if [ -z "$luals" ]; then
        echo "no lua-language-server on PATH, skipping the config type check (pacman -S lua-language-server)"
        exit 0
    fi
    log=$(mktemp -d)
    trap 'rm -rf "$log"' EXIT
    # `share/starter` ships no `.luarc.json` on purpose: `oblisk init` writes one pointing at the
    # *installed* stub directory (`setup.rs`'s `luarc_json`), so a checked-in copy would be a second
    # answer that init immediately overwrites. This is that file, with an absolute library path
    # because a relative one resolves against the workspace being checked, not against this config.
    printf '{"runtime.version":"Lua 5.4","workspace.library":["%s/lua-meta"],"workspace.checkThirdParty":false}\n' "$PWD" >"$log/starter.luarc.json"
    # `--check` exits non-zero when it finds anything and prints the diagnostics -- file, line,
    # column, the offending source line and a caret run -- to stdout, mixed in with a progress bar
    # it redraws with carriage returns. So the output is captured rather than discarded, and
    # replayed only on failure with the progress chunks filtered out.
    #
    # This used to read a `$log/check.json` that was never written: the JSON report needs
    # `--check_format=json`, which was not passed, so the report block was dead and a failure
    # surfaced as `set -e` alone -- a bare "recipe failed with exit code 1" and not one word about
    # what was wrong. The human-readable form is better than the JSON here anyway, because it
    # carries the source line.
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
    # `dev-config/oblisk` has its own `.luarc.json`, which the language server finds on its own and
    # which also carries the `runtime.path` its `require`s need.
    check dev-config/oblisk
    check share/starter --configpath "$log/starter.luarc.json"
    # No library: these files declare everything they reference, which is the point of checking
    # them on their own.
    printf '{"runtime.version":"Lua 5.4","workspace.checkThirdParty":false}\n' >"$log/meta.luarc.json"
    check lua-meta --configpath "$log/meta.luarc.json"
    echo "lua-meta type-checks, and dev-config and the starter type-check against it"

# Every Lua file parses, config and stubs alike.
lua:
    #!/usr/bin/env bash
    set -euo pipefail
    find lua-meta dev-config share -name '*.lua' -print0 | xargs -0 -n1 luac -p
    echo "all lua parses"

# Regenerate `lua-meta/oblisk.lua` from the supervisor's payload types, then show what moved.
stubs:
    UPDATE_STUBS=1 cargo test -p supervisor stubs
    @git diff --stat -- lua-meta/oblisk.lua

# Formatting as a gate, not a habit. `just fmt` fixes whatever this reports.
#
# Separate from `lint` because rustfmt and clippy fail differently: one is a diff, the other is a
# warning, and folding them together buries the diff. This recipe exists because 71266cb and c83e79e
# landed four unformatted files between them with `just check` green on both. `just fmt` was there
# the whole time and nothing made anyone run it.
fmt-check:
    cargo fmt --all -- --check

fmt:
    cargo fmt --all

clean:
    cargo clean

# Install layout. `PREFIX` is where it goes, `DESTDIR` is a staging root for a package build, so a
# PKGBUILD is `just install PREFIX=/usr DESTDIR="$pkgdir"` and nothing else.
#
# The Renderer lands in `lib/oblisk`, off `$PATH`, and `bin/oblisk` is a symlink into it. That works
# because `current_exe` reads `/proc/self/exe`, which is already symlink-resolved, so the Supervisor
# still finds its sibling. One command on the user's path, and the pair cannot drift apart.
#
# No service unit. Oblisk is started from the compositor's own config, the way a bar is:
# `spawn-at-startup "oblisk"` in niri, `exec-once = oblisk` in Hyprland, `exec oblisk` in sway.
prefix := "/usr/local"
destdir := ""

install: release
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{destdir}}{{prefix}}"
    install -Dm755 target/release/oblisk          "$root/lib/oblisk/oblisk"
    install -Dm755 target/release/oblisk-renderer "$root/lib/oblisk/oblisk-renderer"
    install -dm755                                "$root/bin"
    ln -sfn ../lib/oblisk/oblisk                  "$root/bin/oblisk"
    for stub in lua-meta/*.lua; do
        install -Dm644 "$stub" "$root/share/oblisk/lua-meta/$(basename "$stub")"
    done
    install -Dm644 share/starter/shell.lua        "$root/share/oblisk/starter/shell.lua"
    echo "installed to $root"

uninstall:
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{destdir}}{{prefix}}"
    rm -rf "$root/lib/oblisk" "$root/share/oblisk" "$root/bin/oblisk"
    echo "removed from $root"
