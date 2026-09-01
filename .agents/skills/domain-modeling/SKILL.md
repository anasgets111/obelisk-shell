---
name: domain-modeling
description: Build and sharpen a project's domain model. Use when discussing codebase terminology, writing or editing a CONTEXT.md, or recording or editing an ADR.
---

# Domain Modeling

Build and sharpen Oblisk's domain model while designing. Challenge terms, test them against edge cases, and record resolved language and decisions immediately. Reading `CONTEXT.md` is not enough. Use this skill when the model changes.

## File structure

Most repos have a single context:

```
/
├── CONTEXT.md
├── docs/
│   └── decisions.md
└── roadmap.md
```

If a `CONTEXT-MAP.md` exists at the root, read it before choosing a context.
The map names each context and its relationships.

Create files lazily: only when you have something to write. If no `CONTEXT.md` exists, create one when the first term is resolved. If no `docs/decisions.md` exists, create it when the first decision is worth recording.

## During the session

### Challenge against the glossary

When the user uses a term that conflicts with `CONTEXT.md`, call it out immediately. "`CONTEXT.md` defines 'Candidate' as a generation being prepared, but you used 'candidate' for the authoritative generation. Which one do you mean?"

### Sharpen fuzzy language

When the user uses vague or overloaded terms, propose a precise canonical term. "You're saying 'ready': do you mean renderer preparation, configure completion, or presentation evidence? Those are different states."

### Discuss concrete scenarios

When domain relationships are being discussed, stress-test them with specific scenarios. Invent scenarios that probe edge cases and force the user to be precise about the boundaries between concepts.

### Cross-reference with code

When the user states how something works, check whether the code agrees. If you find a contradiction, surface it: "The plan keeps the first MPRIS adapter renderer-local, but the code puts ownership in the supervisor. Which is the intended owner?"

### Update CONTEXT.md inline

When a term is resolved, update `CONTEXT.md` right there. Don't batch these up: capture them as they happen. Use the format in [CONTEXT-FORMAT.md](./CONTEXT-FORMAT.md).

`CONTEXT.md` should be totally devoid of implementation details. Do not treat `CONTEXT.md` as a spec, a scratch pad, or a repository for implementation decisions. It is a glossary and nothing else.

### Offer ADRs sparingly

Only offer to create an ADR when all three are true:

1. **Hard to reverse**: the cost of changing your mind later is meaningful
2. **Surprising without context**: a future reader will wonder "why did they do it this way?"
3. **The result of a real trade-off**: there were genuine alternatives and you picked one for specific reasons

If any of the three is missing, skip the ADR. Use the format in [ADR-FORMAT.md](./ADR-FORMAT.md).
