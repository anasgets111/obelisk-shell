# Design It Twice

Your first interface design is usually wrong. When exploring a deepening candidate, execute this parallel sub-agent pattern to generate radically different interfaces. 

Strictly enforce the vocabulary: **module**, **interface**, **seam**, **adapter**, **leverage**.

## Execution Protocol

### 1. Frame the Problem Space
Before spawning sub-agents, define the architectural reality for the user:
*   **Constraints:** What must this interface achieve?
*   **Dependencies:** Which category do they fall into? (In-process, Local-substitutable, External/Mock).
*   **Code Sketch:** A rough Rust (or Lua caller) snippet proving the constraints. *Not a proposal, just a baseline.*

**Action:** Output this frame, pause for the user to read, then immediately proceed to Step 2.

### 2. Spawn Sub-Agents
Spawn 3 parallel agents (no more). Pass each the codebase context, the domain vocabulary (`CONTEXT.md`), and the architectural vocabulary ([SKILL.md](SKILL.md)). 

Assign distinct architectural directives:
*   **Agent 1 (The YAGNI Minimalist):** Minimize the interface (1–3 entry points max). Maximize leverage. Hide everything else.
*   **Agent 2 (The Flexible Extender):** Support multiple use cases and future extensions (e.g., a trait with several implementors).
*   **Agent 3 (The Pragmatic Caller):** Optimize for the 80% common caller. Make the default case a trivial one-liner in Rust or Lua.

**Sub-Agent Output Requirements:**
1.  **Interface:** Rust signatures (plus the Lua call, if exposed), error modes, invariants.
2.  **Usage:** 1-2 lines showing caller execution.
3.  **Concealment:** What exact logic is hidden behind the seam.
4.  **Adapters:** Strategy for testing vs. production.
5.  **Trade-offs:** Where leverage is high vs. where it degrades.

### 3. Present & Compare
Present the designs sequentially. Do not dump a menu.

*   **Contrast Metrics:** Compare based on **depth** (leverage at the interface), **locality** (where maintenance concentrates), and **seam placement**.
*   **The Recommendation:** Provide one final, highly opinionated recommendation. If elements from different designs combine well, propose a hybrid. Be decisive.
