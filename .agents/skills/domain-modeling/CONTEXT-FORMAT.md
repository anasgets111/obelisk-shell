# CONTEXT.md Format

## Structure

```md
# Obelisk shell

Shared language for renderer generations, reloads, retained scenes, and capability state.

## Reloads

**Generation**:
A renderer process and its Lua state with one generation ID.
_Avoid_: instance, worker

**Candidate**:
A generation being prepared but not yet authoritative.
_Avoid_: active generation, staged shell

**Authoritative generation**:
The generation that receives input, reserves exclusive space, and owns capability routing.
_Avoid_: active generation, current process

**Presentation evidence**:
Proof that a targeted output produced its first frame or presentation feedback after configuration.
_Avoid_: readiness, activation ACK
```

## Rules

- **Be opinionated.** When multiple words exist for the same concept, pick the best one and list the others under `_Avoid_`.
- **Keep definitions tight.** One or two sentences max. Define what it IS, not what it does.
- **Only include terms specific to this project's context.** General programming concepts (timeouts, error types, utility patterns) don't belong even if the project uses them extensively. Before adding a term, ask: is this a concept unique to this context, or a general programming concept? Only the former belongs.
- **Group terms under subheadings** when natural clusters emerge. If all terms belong to a single cohesive area, a flat list is fine.

## Single vs multi-context repos

**Single context (most repos):** One `CONTEXT.md` at the repo root.

**Multiple contexts:** A `CONTEXT-MAP.md` at the repo root lists the contexts, where they live, and how they relate to each other:

```md
# Context Map

## Contexts

- [Reloads](./renderer/CONTEXT.md): stages generations and changes authority after presentation evidence
- [Capabilities](./capabilities/CONTEXT.md): publishes bounded snapshots and validates commands

## Relationships

- **Reloads → Capabilities**: The authoritative generation owns capability routing; stale generation IDs and revisions are rejected.
- **Reloads → Renderer**: The supervisor sends bounded control messages to a candidate generation.
```

The skill infers which structure applies:

- If `CONTEXT-MAP.md` exists, read it to find contexts
- If only a root `CONTEXT.md` exists, single context
- If neither exists, create a root `CONTEXT.md` lazily when the first term is resolved

Obelisk currently has one context. Do not split it into renderer and capability contexts until their terms or ownership rules genuinely diverge.
