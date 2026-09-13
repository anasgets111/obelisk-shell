# Deepening

How to safely collapse a cluster of shallow modules into one deep module. The dependency type dictates the seam and testing strategy.

## Dependency Categories

Assess the dependencies of the target module. This dictates how you test across the seam.

### 1. In-Process
Pure computation, in-memory state, zero I/O. 
*   **Action:** Always deepenable. Merge the modules. 
*   **Testing:** Test directly through the new interface. Zero adapters required.

### 2. Local-Substitutable
Dependencies with local test equivalents (e.g., a tempdir for the filesystem, `sys_root` for sysfs, `p2p_pair()` for D-Bus).
*   **Action:** Deepenable. The seam remains internal. 
*   **Testing:** Run the test suite against the local substitute. Do not extract a port/adapter for this at the module's external interface.

### 3. Remote but Owned (Ports & Adapters)
Processes you control across a socket (Supervisor ↔ Renderer).
*   **Action:** Define a **port** (interface) at the seam. The deep module owns the logic.
*   **Testing:** Inject an **adapter**: the real socket in production, an in-memory channel in tests. 

### 4. True External (Mock)
Services you do not control (e.g., the compositor, BlueZ, NetworkManager, PipeWire).
*   **Action:** The deep module accepts the external dependency as an injected port.
*   **Testing:** Tests provide a Mock adapter.

## Seam Discipline

*   **The Single Adapter Fallacy:** One adapter = a hypothetical seam. YAGNI. Do not introduce a port unless you have at least two concrete adapters (usually Prod vs. Test).
*   **Internal vs. External Seams:** A deep module can have internal seams private to its implementation. Do not expose them through the public interface just because your tests want them. 

## Testing Strategy: Replace, Don't Layer

*   **Delete Waste:** Old unit tests tied to the previous shallow modules are now technical debt. Delete them.
*   **The Interface is the Test Surface:** Write new tests hitting only the deepened module's interface. 
*   **Assert Outcomes, Not State:** Assert on observable results (returned values, sent frames, emitted commands). Do not assert on internal state or use reflection.
*   **Implementation Agnosticism:** If an internal refactor breaks your test, you tested past the interface. Fix the test.
