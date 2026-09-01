---
name: architecture-review
description: Scan codebase for shallow modules. Present deep refactoring targets in a visual HTML report. YAGNI-first.
disable-model-invocation: true
---

# Architecture Review

Surface architectural friction. Propose **deepening opportunities** (converting shallow modules into deep ones). Optimize for testability and locality.

**Strict Vocabulary Definition:** 
Always use: **module**, **interface**, **depth**, **seam**, **adapter**, **leverage**, **locality**. 
Never use: component, service, API, boundary. Use `CONTEXT.md` for domain terminology. Respect existing `docs/decisions.md`.

## Process

### 1. Explore (YAGNI)

Do not review stable code. Deepening only pays off if the code changes. 

- **Target Hotspots:** Run `git log --oneline` to locate frequently touched files.
- **Read Context:** Parse `CONTEXT.md` and `docs/decisions.md` for the target area.
- **Audit for Friction (Spawn Sub-Agent):**
  - Locate **shallow modules** (interface complexity ≈ implementation complexity).
  - Locate leaked abstractions (e.g., Eloquent queries bleeding into Vue components).
  - Identify pure functions lacking **locality** (extracted for tests, but real bugs hide in the untested callers).
- **The Deletion Test:** Would deleting this module concentrate complexity, or just move it? If it concentrates, it is shallow. Target it.

### 2. Confirm Every Candidate

A claim read off the source and a claim proven by a failing test are not the same claim. Confirm before writing the report, and say in the report which kind each one is.

- **Prove it if it is provable.** Write the throwaway test, run the build with the changed feature, count with a script rather than a naive grep. Then revert the probe and confirm `git status` is clean.
- **Report corrections as corrections.** A number that was wrong on the first pass stays visible as a correction. A silently fixed count reads as a count nobody checked.
- **Say `Read only` when nothing was run.** Do not dress a source read up as evidence.

### 3. Generate HTML Report

Write a self-contained HTML file to the OS temp directory. Do not pollute the repo.

- **Path:** `/tmp/architecture-review-<timestamp>.html` (Fallback: `$TMPDIR`).
- **Open:** Execute `xdg-open <path>`.
- **Format:** Refer to `HTML-REPORT.md` for the strict dark-mode visual spec. Hand-written CSS and Mermaid only. No Tailwind, no CSS framework.
- **Candidate Data:** Files, Problem, Solution, Benefits (using strict vocabulary), Recommendation Strength (`Strong`, `Worth exploring`, `Speculative`), Before/After Diagram, and how the claim was confirmed.
- **Decision Conflicts:** Only surface a conflicting candidate if the friction justifies reopening the entry in `docs/decisions.md`. Mark it explicitly.

*Do not propose new interfaces yet. Output the file, open it, and ask: "Which of these do we explore?"*

### 4. The Grilling Loop

Once the user selects a candidate, execute `grill` to stress-test the architectural decision tree.

**Inline Side Effects:**
- **Missing Domain Term?** Update `CONTEXT.md` immediately.
- **User Rejects Candidate?** Ask to record an entry in `docs/decisions.md` to prevent future re-suggestions. (Skip if the reason is ephemeral).
- **Alternative Interfaces?** Spawn parallel sub-agents to design it twice using the codebase-design principles.
