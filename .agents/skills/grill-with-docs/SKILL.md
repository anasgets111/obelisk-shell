---
name: grill-with-docs
description: Relentless interview for sharpening an Oblisk plan and recording ADRs or glossary terms.
---

# Grill with docs

Call the Skill tool twice, once for `grill` and once for `domain-modeling`.

Use the exact Oblisk terms from `CONTEXT.md`: renderer generation, candidate,
authoritative generation, presentation evidence, builder, retained scene,
dependency snapshot, capability, and revision.

The grill must test the requested design against the existing ownership rules:

- The supervisor owns reload authority, deadlines, rollback, and reaping.
- A candidate needs presentation evidence from every targeted output before it
  becomes authoritative.
- The loader and watcher consume one dependency snapshot.
- The retained-scene transaction owns node identity, writes, and the removal of
  unmatched subtrees.
- The capability authority rejects stale generation IDs and revisions.

Use `domain-modeling` to update `CONTEXT.md` when a term changes. Write an ADR
only for a hard-to-reverse, surprising trade-off. Stop after each grill round
and wait for the user's answer.
