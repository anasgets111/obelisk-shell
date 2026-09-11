---
name: obelisk-review
description: Three-axis code review (Standards vs. Spec vs. Correctness). Parallel execution. Enforces YAGNI and systems-level discipline.
---

# Code Review

Review the diff between `HEAD` and a target fixed point across three independent axes:
1.  **Standards:** Does it pass repo conventions and the Baseline Smells?
2.  **Spec:** Does it implement the exact requested feature without scope creep?
3.  **Correctness:** Assume the diff is wrong. Find the way it's wrong.

Run axes as parallel sub-agents, each a genuinely fresh `Agent` call (`general-purpose` or a dedicated reviewer type), never a fork of the implementing session. A fork inherits the implementer's reasoning and context, which is exactly what has to stay out for axis 3 to work: it should see the diff and nothing else, not the plan behind it, not why the implementer thinks it's right.

## 1. Pin the Target
*   Ask the user for the fixed point (`HEAD~5`, `main`, commit SHA) if not provided.
*   Capture the diff: `git diff <fixed-point>...HEAD`.
*   Capture the commits: `git log <fixed-point>..HEAD --oneline`.
*   **Fail Fast:** Verify the ref exists (`git rev-parse`) and the diff is not empty before spawning agents.

## 2. Identify the Spec
Locate the originating requirement in this order:
1.  Issue references in commits (`#123`, `Closes #45`, GitLab `!67`).
2.  User-provided path.
3.  Local spec file (`docs/`, `specs/`, or `.scratch/`).
*   *If missing, ask the user. If none exists, the Spec sub-agent aborts and reports "No spec available."*

## 3. Identify the Standards
Combine repo-specific docs (`CODING_STANDARDS.md`) with the **Baseline Smells**. 
*Rule of Law:* Documented repo standards override the baseline. Skip anything enforced by automated tooling (e.g., PHP_CodeSniffer, ESLint).

### Baseline Smells
| Smell | Definition | The Fix |
| :--- | :--- | :--- |
| **Mysterious Name** | Unclear variable, function, or type name. | Rename it. If you cannot name it, the architecture is flawed. |
| **Duplicated Code** | Repeated logic shapes across the diff. | Extract to an Action, Trait, or Vue Composable. |
| **Feature Envy** | A method querying another object's data heavily. | Move the method onto the Eloquent model/object it envies. |
| **Data Clumps** | The same 3-4 parameters travel together constantly. | Extract a DTO (Data Transfer Object) or Value Object. |
| **Primitive Obsession** | Strings/Ints acting as domain concepts. | Use Enums, Value Objects, or custom Casts. |
| **Repeated Switches** | Identical `switch`/`if` cascades on one type. | Replace with Polymorphism or a Config/Match Map. |
| **Shotgun Surgery** | One logical change scatters edits across 10 files. | Consolidate the logic into a cohesive domain module. |
| **Divergent Change** | One file edits for 5 unrelated reasons. | Split the class. Enforce Single Responsibility. |
| **Speculative Generality** | Interfaces/hooks built for "future needs". | **YAGNI.** Delete it. Inline until a concrete requirement exists. |
| **Message Chains** | `a->b()->c()->d()` navigation. | Hide the walk behind a single method on the root object. |
| **Middle Man** | A class/function that just delegates (Shallow Module). | Delete it. Call the target directly. |
| **Refused Bequest** | Subclass overriding/ignoring most inherited logic. | Drop inheritance. Use Composition. |

## 4. Spawn Parallel Sub-Agents

**Agent A: Standards Review**
*   **Input:** Diff, commit list, `CODING_STANDARDS.md`, Baseline Smells.
*   **Task:** Flag standards violations and code smells. Distinguish hard repo violations from baseline judgment calls. Ignore tooling-enforced formatting.
*   **Limit:** < 400 words.

**Agent B: Spec Review**
*   **Input:** Diff, commit list, Spec document.
*   **Task:** Flag missing requirements, incorrect implementations, and **scope creep** (code written that was not requested). Quote the spec directly.
*   **Limit:** < 400 words.

**Agent C: Correctness Review**
*   **Input:** The raw diff only. No commit list, no spec, no standards doc, no summary of what the implementer was trying to do.
*   **Task:** Assume the diff is wrong. Hunt for it: memory-safety and lifetime bugs (use-after-free, dangling pointers handed to FFI/callbacks, double-free), concurrency bugs (races, deadlocks, a lock dropped before the section it guards ends), logic errors, and edge cases the diff's own control flow doesn't handle (empty input, zero, negative, the last iteration of a loop, an error path that isn't tested because it isn't hit by the happy path). Report each with `ReportFindings`: a verdict (`CONFIRMED` if you traced the exact failing execution, `PLAUSIBLE` if the shape is suspicious but you couldn't fully trace it) and a concrete failure scenario, not a vague concern.
*   **Limit:** < 400 words, excluding the `ReportFindings` payload.

## 5. Aggregate Report
Output all three reports verbatim under `## Standards`, `## Spec`, and `## Correctness`. Do not merge them. A feature can flawlessly execute the spec while introducing architectural garbage, or pass every standards check while use-after-freeing a pointer three functions away from where it looks fine.

End with a brutal one-line summary:
> **Total Findings:** [X] Standards, [Y] Spec, [Z] Correctness. **Critical Blockers:** [List the absolute worst offense in each category].
