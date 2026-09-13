---
name: grill-me
description: Relentlessly stress-test an architectural plan or idea. Enforce Systems Thinking and YAGNI.
---

# Grill

Stress-test the plan. Map the architecture as a dependency graph. Accept no assumptions. Interrogate until the root cause and optimal flow are exposed.

## Execution Protocol

Operate in strict rounds. Attack the **frontier**: questions whose prerequisites are fully settled. Do not ask downstream questions until the current blockers are resolved.

1. **The YAGNI Gate (Round 0):** Does this need to exist? Can std, an installed crate, the compositor, existing engine code, or plain Lua in the config handle this?
2. **Automated Fact-Finding:** Never ask the user for environment facts (code, system state, docs). Use tools to parse the reality. 
3. **The Grill:** Output the current frontier questions. Wait for answers. Recompute the frontier.

## Question Format

Number each question. Be brutally concise. Always provide an opinionated, boring, minimal-code recommendation.

**Q[#] - [Target System / Decision]:**
[1-2 lines detailing the constraint, missing logic, or potential edge-case failure.]

**Recommendation:** [The edge-case-correct, standard-library, or minimal abstraction solution]

## Completion Rule

Stop after outputting a round. Wait for the user's reply. Do not guess downstream answers. The grill is complete only when the frontier is empty and zero architectural assumptions remain.
