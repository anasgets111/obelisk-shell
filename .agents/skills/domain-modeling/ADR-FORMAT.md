# Decision records

Decisions live in one file, `docs/decisions.md`, one entry each, numbered sequentially.

## Template

    ## NNNN. Short title of the decision

    One to three sentences: the context, what was decided, and why.

That is it. An entry can be a single paragraph. The value is in recording *that* a decision was
made and *why*, not in filling out sections. The 81 entries in that file were once 81 separate
files running 8,635 lines, median 99 lines for one decision. They were compressed because the
essays had buried the decisions.

## Optional structure

Only when it adds value. Most entries need none.

- A numbered list of decisions, when one entry settles several things at once. Code cites these
  as `ADR-0044 decision 2`, so the numbers are permanent once written.
- `Rejected: <alternative>, because <reason>.` Only when a future maintainer would otherwise
  propose it again.
- `Not built: <list>.` The explicit no-s are as valuable as the yes-s.

## Numbering

Take the highest `## NNNN.` in `docs/decisions.md` and add one. Never reuse or renumber: 1,233
Rust comments cite these numbers as `ADR-NNNN`.

## Superseding

Do not edit an entry to match what shipped later. Add a line to it naming the entry that
superseded it, and leave the record alone. An entry is dated evidence, not documentation.

## When to write one

All three must be true:

1. **Hard to reverse**: changing your mind later costs something real.
2. **Surprising without context**: a future reader will look at the code and wonder why.
3. **The result of a real trade-off**: there were alternatives and you picked one for a reason.

If it is easy to reverse, skip it. If it is not surprising, nobody will wonder. If there was no
alternative, there is nothing to record beyond "we did the obvious thing".

### What qualifies

- **Architectural shape.** "Renderer generations run in separate processes."
- **Integration patterns between owners.** "The supervisor and renderer use a private control protocol."
- **Technology choices that carry lock-in.** SCTK, femtovg on EGL, taffy, vendored Lua 5.4. Record
  only what would be costly to replace.
- **Scope decisions.** "The capability authority owns revisions and stale-command rejection."
- **Deliberate deviations from the obvious path.** Record choices a future maintainer might "fix".
- **Constraints not visible in the code.** Protocol and cleanup rules a caller cannot infer.
- **Rejected alternatives when the rejection is non-obvious.** Otherwise it gets proposed again.
