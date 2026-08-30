---
name: implement
description: "Implement an Oblisk slice from the plan or build steps."
---

Implement the requested Oblisk slice against `docs/build-steps.md`, the
relevant ADRs in `docs/adr/`, and the user's request. Use the canonical terms
in `CONTEXT.md`.

Use `/tdd` where possible, at pre-agreed module seams. Keep Rust ownership in
the engine and supervisor, and keep Lua focused on generation configuration and
scene composition.

Run `cargo check --workspace` regularly. Run
`cargo clippy --workspace --all-targets --all-features -- -D warnings` before
considering any slice done, not just at the very end. Run focused `cargo test`
commands for the touched crate or test filter, then run `cargo test --workspace`
once at the end. Run the headless Wayland or real-session check required by the
build step when the change crosses a protocol or capability seam.

If you moved code between modules, run `cargo doc --workspace --no-deps` and
compare the `unresolved link` count against the one before your change. Clippy
does not check intra-doc links, so a doc comment pointing at an item that moved
a module away passes every other gate silently. Fix one by demoting the link to
a plain backtick path with the module prefix, never by widening an item's
visibility to satisfy rustdoc. The counts to beat are 29 in the supervisor and
3 in the renderer, all predating this note.

Once done, use `/oblisk-review` to review the work against the requested slice.

Do not commit unless the user asks for a commit.
