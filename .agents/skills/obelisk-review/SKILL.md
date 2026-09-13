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
1.  Issue references in commits (`#123`, `Closes #45`).
2.  User-provided path.
3.  `docs/` and the ADRs the commits cite.
*   *If missing, ask the user. If none exists, the Spec sub-agent aborts and reports "No spec available."*

## 3. Identify the Standards
Combine `AGENTS.md` with the **Baseline Smells**. 
*Rule of Law:* `AGENTS.md` overrides the baseline. Skip anything `just check` enforces (rustfmt, clippy, luafmt, doc links).

### Baseline Smells
| Smell | Definition | The Fix |
| :--- | :--- | :--- |
| **Avoidable Lines** | Added code the change does not need, or dead code it leaves behind. | Delete it. |
| **Framework Leak** | Rust code, a test or a comment depending on `dev-config`. | Inline fixture; state the engine reason. |
| **Mysterious Name** | Unclear variable, function, or type name. | Rename it. If you cannot name it, the architecture is flawed. |
| **Duplicated Code** | Repeated logic shapes across the diff. | Reuse the existing helper, or extract one function. |
| **Feature Envy** | A function reading another type's data heavily. | Move it onto the type it envies. |
| **Data Clumps** | The same 3-4 parameters travel together constantly. | Extract a struct. |
| **Primitive Obsession** | Strings/ints acting as domain concepts. | Use an enum or newtype. |
| **Repeated Switches** | Identical `match`/`if` cascades on one type. | One `match` behind a method. |
| **Shotgun Surgery** | One logical change scatters edits across 10 files. | Consolidate the logic into one module. |
| **Divergent Change** | One file edits for 5 unrelated reasons. | Split the module. |
| **Speculative Generality** | Traits/hooks built for "future needs". | **YAGNI.** Delete it. Inline until a concrete requirement exists. |
| **Middle Man** | A function or module that just delegates (Shallow Module). | Delete it. Call the target directly. |

## 4. Spawn Parallel Sub-Agents

**Agent A: Standards Review**
*   **Input:** Diff, commit list, `AGENTS.md`, Baseline Smells.
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
