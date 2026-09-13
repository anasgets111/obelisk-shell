---
name: implement
description: "Implement an Obelisk slice."
---

Implement the request against `docs/lua-api.md`, the relevant `docs/decisions.md` entries and the `CONTEXT.md`
terms. `docs/roadmap.md` lists what is not built.

- Framework first: platform connections, validation, secrets, lifetimes, input and rendering stay in Rust;
  composition, appearance and policy stay in Lua. Nothing in Rust depends on `dev-config`.
- Smallest diff. Delete what the change makes redundant.
- `/tdd` at agreed seams.
- `cargo check` while working, focused `cargo test`, then `just check` before done (fmt, tests, clippy,
  doc-link baselines, Lua types).
- A `dev-config`-only change: `just lua types` and `obelisk check -c dev-config/obelisk`.
- Crossing a Wayland or capability seam: verify on the live session (`diagnosing-bugs` step 1).
- Review with `/obelisk-review`. Commit only when asked.
