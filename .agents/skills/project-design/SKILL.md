---
name: project-design
description: Shared vocabulary for designing deep modules. Enforce Systems Thinking, leverage, locality, and YAGNI.
---

# Codebase Design

Design **deep modules**: massive implementation hidden behind a minimal interface, placed at a clean seam, fully testable. Optimize for leverage (caller efficiency) and locality (maintainer efficiency).

## Architectural Glossary

Use these exact terms. Do not substitute with "component," "service," "API," or "boundary."

| Term | Strict Definition |
| :--- | :--- |
| **Module** | Anything with an interface and implementation (function, type, module, crate). |
| **Interface** | Everything a caller must know: type signature, invariants, ordering, errors, config. |
| **Implementation** | The internal body of code hiding behind the interface. |
| **Depth** | Leverage at the interface. High depth = minimal interface + massive implementation. |
| **Seam** | The location where a module's interface lives (where behavior can be swapped). |
| **Adapter** | The concrete logic that satisfies an interface at a seam (e.g., a zbus proxy, a sysfs reader). |
| **Leverage** | Caller benefit: capabilities gained per unit of interface learned. |
| **Locality** | Maintainer benefit: bugs, logic, and tests concentrate in one place. Fix once. |

## Deep vs. Shallow

**Deep module** = small interface + deep implementation. High leverage.

```text
┌─────────────────────┐
│   Small Interface   │ ← Minimal methods/params (e.g., `invoke("connect")`)
├─────────────────────┤
│                     │
│ Deep Implementation │ ← Retries, logging, payload mapping hidden
│                     │
└─────────────────────┘
```

**Shallow module** = large interface + thin implementation. A useless pass-through. Delete it.

```text
┌─────────────────────────────────┐
│        Large Interface          │ ← Requires setting up 5 structs
├─────────────────────────────────┤
│       Thin Implementation       │ ← Just forwards to another fn
└─────────────────────────────────┘
```

## Lazy Senior Dev Principles

*   **The Deletion Test (YAGNI):** If you delete a module and complexity vanishes, it was a useless pass-through. If complexity explodes across N callers, it was earning its keep.
*   **Depth belongs to the interface.** A deep module can use small, swappable internal types. Do not expose them.
*   **The interface is the test surface.** If you have to test past the interface (mocking internal state), the module is the wrong shape.
*   **Seam Discipline:** One adapter = a hypothetical seam (YAGNI violation). Two adapters = a real seam (e.g., the session bus in prod, `p2p_pair()` in tests). Do not introduce a seam unless it varies.

## Testability via Interface

**1. Accept dependencies, do not instantiate them.**
```rust
// Testable
fn read_battery(sys_root: &Path) -> io::Result<Battery> {}

// Garbage (hidden coupling)
fn read_battery() -> io::Result<Battery> { let sys_root = Path::new("/sys"); }
```

**2. Return results, avoid hidden state mutations.**
```rust
// Testable
fn resolve(style: &Style, parent: Rect) -> Rect {}

// Garbage (hidden side effect)
fn resolve(node: &mut Node) { node.rect = ...; }
```

## Going Deeper
*   **Deepening a module:** [DEEPENING.md](DEEPENING.md).
*   **Alternative interface architectures:** [DESIGN-IT-TWICE.md](DESIGN-IT-TWICE.md).
