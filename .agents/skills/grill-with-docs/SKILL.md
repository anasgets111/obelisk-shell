---
name: grill-with-docs
description: Relentless interview for sharpening an Obelisk plan and recording ADRs or glossary terms.
---

# Grill with docs

Call the Skill tool twice, once for `grill-me` and once for `domain-modeling`.

- Use the exact terms in `CONTEXT.md`.
- Test the design against the ownership in `docs/services.md` and the ADRs for the area.
- Test it against the layer split: the framework stays generic, and a need of `dev-config` alone belongs in Lua.
- Update `CONTEXT.md` when a term changes. Write an ADR only for a hard-to-reverse, surprising trade-off.
- Stop after each round and wait for the user's answer.
