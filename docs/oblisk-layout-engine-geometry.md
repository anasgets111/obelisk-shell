# Oblisk Layout Engine & Geometry Specification


This specification details the mathematical, algorithmic, and programmatic contracts governing the Oblisk Layout Engine. The layout engine is implemented entirely in compiled Rust within the ephemeral Renderer's scene-graph module. 

To prevent LLMs and developers from hardcoding visual products (such as bars or menus) in Rust, the engine rejects high-level widgets. It operates strictly as a one-pass geometric constraint solver that resolves primitive layout nodes into scaled physical rectangles, independently within each layer-shell surface `shell.lua` declares.

---

## 1. The Minimal Node Vocabulary

The Rust retained-scene graph accepts only the following primitive nodes. Any compound desktop component must be composed entirely in Lua using these primitives.

```
                  +-----------------------------------+
                  |            SceneGraph             |
                  +-----------------------------------+
                                    |
            +-----------------------+-----------------------+
            |                                               |
  +------------------+                            +------------------+
  |  Container Nodes |                            |   Leaf Nodes     |
  +------------------+                            +------------------+
  |  - Row           |                            |  - Text          |
  |  - Column        |                            |  - Icon          |
  |  - Button        |                            |                  |
  |  - Rect (with    |                            |  - Rect (without |
  |     children)    |                            |     children)    |
  +------------------+                            +------------------+
```

### 1.1 Layout Nodes
1.  **`Rect`**: A flexible rectangular element representing either a containment box or a solid drawing shape. If configured with nested `children`, it acts as a container supporting backgrounds, rounded corners, borders, and clipping. If declared without children, it resolves as a childless leaf shape (e.g. progress bar, slider track, or divider line).
2.  **`Row`**: A horizontal layout flex-container that distributes children along the X-axis.
3.  **`Column`**: A vertical layout flex-container that distributes children along the Y-axis.
4.  **`Button`**: An interactive wrapper container that receives focus and pointer events, executing a Lua callback when tapped.
5.  **`Text`**: A read-only text container driven by `cosmic-text`.
6.  **`Icon`**: An SVG/PNG icon container that pulls from system themes or `/dev/shm`.
7.  **`List`**: A fast-reconciling virtual repeater element that binds to a reactive array signal and repeats custom child layouts dynamically.

## 2. Geometric Core & Layout Data Structures

All calculations use single-precision floating-point coordinates (`f32`) representing logical pixels. They are converted to integer physical pixels (`i32`) only during final viewport projection.

### 2.1 Window Surface Isolation
Oblisk mounts one layer surface per `(surface, output)` pair `shell.lua` declares, created and
destroyed as the evaluated topology changes (ADR-0038). The default config declares a `main_bar`,
a transparent fullscreen `overlay_canvas`, and a per-monitor `wallpaper_layer`, but those are
config, not engine constants.
*   Every `surface` returned in your layout is solved as an independent scene-graph tree.
*   Constraints, coordinate math, and bounding dimensions are isolated. Layout changes inside one surface never trigger a redraw pass on another.

---

## 3. The One-Pass Layout Algorithm

Oblisk rejects the expensive multi-pass layout trees and cyclic binding graphs used in QtQuick/QML. Layout calculations occur in a single, top-down-bottom-up-top-down pass executed on every scene tick.

```
[Constraint Pass] (Top-Down)
      │  Parent passes maximum dimensions minus margins/padding
      ▼
[Size Resolution Pass] (Bottom-Up)
      │  Leaf nodes resolve size (e.g., text measurements via cosmic-text)
      ▼
[Position & Stretch Pass] (Top-Down)
         Parent distributes spare space and aligns coordinates
```

### 3.1 Step 1: The Constraint Pass (Top-Down)
The parent container computes the available bounding dimensions for its children. For a parent with bounds $W_{max} 	imes H_{max}$, padding $P$, and child margin $M$:

$$	ext{Inner } W_{avail} = W_{max} - (P.left + P.right) - (M.left + M.right)$$
$$	ext{Inner } H_{avail} = H_{max} - (P.top + P.bottom) - (M.top + M.bottom)$$
These bounds are clamped by the child's explicit properties:
*   `Pixels(f32)`: Sets explicit, non-flexible dimensions.
*   `Percent(f32)`: Multiplies available parent bounds by a fractional scale `[0.0, 1.0]`.
*   `Content`: Shrink-wraps the boundaries tightly around child dimensions.
*   `Fill`: Commands the child to stretch and occupy maximum available parent space.

### 3.2 Step 2: Size Resolution Pass (Bottom-Up)
Leaf nodes compute their intrinsic size bounds.
*   **Text Node Size Resolution**: Text measurement is completed using `cosmic-text`. The layout engine runs a fast glyph measurement pass to calculate height and width based on font-size and wraps text bounds when exceeding available width limits.
*   **Row Intrinsic Size**:
    $$W_{row} = \sum_{i=1}^{N} W_{child, i} + (N-1) \times \text{spacing}$$
    $$H_{row} = \max_{i=1 \dots N} (H_{child, i})$$
*   **Column Intrinsic Size**:
    $$W_{col} = \max_{i=1 \dots N} (W_{child, i})$$
    $$H_{col} = \sum_{i=1}^{N} H_{child, i} + (N-1) \times \text{spacing}$$

### 3.3 Step 3: Position & Stretch Resolution Pass (Top-Down)
Once sizes are resolved, the parent container calculates final offsets. It distributes remaining space ($S_{spare} = 	ext{Parent Inner Allocation} - 	ext{Total Child Intrinsic}$) based on alignment properties (`Start`, `Center`, `End`, `Stretch`).

---

## 4. Keyed Reconciliation and Flat Lists

For dynamic lists (like notification feeds), the layout engine executes a fast linear reconciler:
1.  **Positional Diff**: The engine iterates through the dynamic Lua tables returned by your `list` signal.
2.  **Node Reuse**: Existing nodes with matching positional indices are preserved, and their properties (such as text labels or icon paths) are updated inline.
3.  **Clean Destructions**: Outdated elements are reaped from the GPU texture memory instantly, maintaining flat CPU and memory consumption.

---

## 5. Input Region Bounding Box Calculations

A surface larger than its visible content must not block pointer events on the applications underneath, except where its UI elements are currently drawn. This applies per surface: it is load-bearing for a transparent fullscreen surface and a no-op for a bar sized tightly around its own content.

### 5.1 Bounding Box Union Merging
*   **Default State**: When none of a surface's children are visible, its input region is set to an empty rectangle via:
    `wl_surface::set_input_region(surface, empty_region)`
*   **Visible Node Scan**: During the rendering pass, the Renderer scans that surface's children and checks for nodes with `visible == true` and their coordinates solved in logical pixels.
*   **Physical Projection**: It multiplies logical coordinates and sizes by the fractional scale $S_f$ and snaps them to physical boundaries:
    $$X_{1} = \lfloor X_{logical} 	imes S_f 
floor$$
    $$Y_{1} = \lfloor Y_{logical} 	imes S_f 
floor$$
    $$X_{2} = \lceil (X_{logical} + W_{logical}) 	imes S_f 
ceil$$
    $$Y_{2} = \lceil (Y_{logical} + H_{logical}) 	imes S_f 
ceil$$
*   **Region Allocation & Update**: Rust allocates a standard Wayland region handle (`wl_region`), iterates over all visible cards, and executes:
    `wl_region::add(region, X1, Y1, X2 - X1, Y2 - Y1)`
    Finally, the merged region list is pushed back over the Wayland protocol:
    `wl_surface::set_input_region(surface, region)`
    Clicks outside the visible children pass through to whatever is underneath.
