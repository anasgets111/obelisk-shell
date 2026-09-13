# Obelisk

Two layers, in priority order:

1. **Framework.** The Rust workspace (`renderer`, `supervisor`, `shared`): a generic Wayland shell engine
   scripted in Lua. `share/starter` is its shipped example config.
2. **Personal shell.** `dev-config/obelisk`, the owner's daily shell built on the framework.

Rust code, tests and comments never depend on or cite `dev-config`. Engine tests use inline fixtures. A
feature missing from `dev-config` is not an engine gap.

## Ponytail: lazy senior dev mode

- **Removing lines is always welcome.** Redundant, dead or duplicated code goes.
- **New lines only when nothing else works.** The smallest diff in the right place wins; the smallest diff in
  the wrong place is a second bug.
- **No walls of text.** Replies, comments, commits, docs. Lead with the answer, bullets over paragraphs, the
  number over the adjective.
- **Comments** state the non-obvious decision, never the mechanism. Rationale longer than a line belongs in an
  ADR or nowhere.
- **Commit messages** say what changed and why it is not obvious, not a session transcript.

Before writing code, trace the real flow end to end, then stop at the first rung that holds:

1. Does it need to exist? (YAGNI)
2. Does the codebase already have it? Reuse it.
3. Does std, a platform feature or an installed crate cover it? Use it.
4. Can it be one line?
5. Only then, the minimum code that works.

- Fix the root cause, not the symptom. Grep every caller and fix the shared function once.
- No new abstractions, dependencies, boilerplate or files unless required.
- Boring over clever. Between similar-sized approaches, take the edge-case-correct one.
- Mark a deliberate simplification with a `ponytail:` comment naming its ceiling and upgrade path.
- Never lazy about understanding the problem, trust-boundary validation, data loss, security, accessibility or
  real-hardware calibration.
- Non-trivial logic gets one runnable check; trivial one-liners get none.

## Testing

- **Never hardcode `/sys` or `/proc`.** Readers take `sys_root`/`proc_root`; tests point them at a tempdir.
- **Tests live beside the code** in `#[cfg(test)] mod tests`; `shared/tests` is the one exception.
- **D-Bus tests use `p2p_pair()`** (`capabilities/test_support.rs`), never the session bus.
