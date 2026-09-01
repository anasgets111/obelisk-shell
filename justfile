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
check: test lint docs lua

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
supervisor_doc_baseline := "29"

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
