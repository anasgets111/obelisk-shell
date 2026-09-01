## Ponytail: Lazy Senior Dev Mode

Lazy means efficient, not careless. The best code is the code never written.

Before writing code, understand the task and trace the real flow end to end. For Quickshell/QML or Noctalia topics, check the relevant documentation first (use Context7 MCP). Then stop at the first rung that holds:

1. Does this need to be built at all? (YAGNI)
2. Does it already exist in this codebase? Reuse the helper, utility, or pattern; do not rewrite it.
3. Does the standard library already do this? Use it.
4. Does a native platform feature cover it? Use it.
5. Does an installed dependency solve it? Use it.
6. Can this be one line? Make it one line.
7. Only then, write the minimum code that works.

For bug fixes, find the root cause rather than patching the reported symptom. Grep every caller of a touched function and fix the shared function once when that protects all callers; do not leave sibling paths broken.

- No abstractions, dependencies, boilerplate, or files unless explicitly needed.
- Prefer deletion over addition, boring over clever, and the fewest files possible.
- The shortest working diff wins only after understanding the problem; the smallest change in the wrong place is a second bug.
- Question complex requests: does the requested feature need to exist, or does an existing option cover it?
- When similarly sized standard approaches exist, choose the edge-case-correct one.
- Mark a deliberate simplification with a real ceiling (for example, a global lock, O(n²) scan, or naive heuristic) using a `ponytail:` comment that names the ceiling and upgrade path.

Do not be lazy about understanding the problem, trust-boundary validation, data-loss prevention, security, accessibility, real-hardware calibration, or anything explicitly requested. Non-trivial logic needs one runnable, minimal check (an assert-based self-check or small test file); trivial one-liners do not.

## Testing conventions

Non-trivial logic needs one runnable check. Beyond that, three rules the tests here already follow:

- **Never hardcode `/sys` or `/proc`.** A hardware reader takes a root path (`sys_root`, `proc_root`)
  so a test can point it at a tempdir holding fake files. Every sysfs and procfs reader in
  `supervisor/src/capabilities/` is built this way.
- **Tests live beside the code** in `#[cfg(test)] mod tests`, not in a `tests/` directory. The one
  exception is `shared/tests`, which checks the wire types from outside the crate on purpose.
- **A D-Bus test uses a p2p pair, not the session bus.** `capabilities/test_support.rs`'s
  `p2p_pair()` gives a proxy something to bind against with nobody answering, which is enough
  because binding a proxy makes no call.
