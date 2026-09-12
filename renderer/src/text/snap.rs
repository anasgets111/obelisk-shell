//! Subpixel-to-physical-pixel snapping math.
//!
//! Floor the top-left corner and ceil the bottom-right, so the physical box always fully
//! contains the logical one.
//!
//! Borders round differently, and the two must not be conflated. [`snap_to_physical`] grows a
//! box outward, right for a containment box and wrong for a border: growing a 1px border
//! outward makes it 2px. [`snap_border_band`] rounds each edge to the nearest pixel instead, so
//! a border keeps the width the config asked for.

/// A rectangle in logical (fractional, DPI-independent) pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A rectangle snapped to physical (integer) pixel boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// The coordinate ceiling a snapped edge saturates to, well inside `i32` so that a caller may
/// subtract two of them.
///
/// `as i32` saturates rather than wrapping, so composed transforms or an oversized compositor
/// configure can land `x0` on `i32::MIN` and `x1` on `i32::MAX`. `x1 - x0` then overflows, and
/// release builds set `overflow-checks`, so the Renderer aborts. Region walks push those
/// differences straight into `wl_region::add` (`wayland::surface`) and `scene::push_rounded_rect`
/// without an intervening clip.
const COORD_LIMIT: f32 = 1_048_576.0;

/// Snaps `rect` to physical pixel boundaries at fractional output scale `scale`.
///
/// The top-left corner floors down and the bottom-right corner ceils up, so the snapped rect
/// always fully contains the logical one -- shrinking would clip a glyph or a border stroke.
/// Both corners saturate at [`COORD_LIMIT`], which is 128x the largest box a config can ask for.
pub fn snap_to_physical(rect: LogicalRect, scale: f32) -> PhysicalRect {
    let clamp = |n: f32| n.clamp(-COORD_LIMIT, COORD_LIMIT) as i32;
    PhysicalRect {
        x0: clamp((rect.x * scale).floor()),
        y0: clamp((rect.y * scale).floor()),
        x1: clamp(((rect.x + rect.width) * scale).ceil()),
        y1: clamp(((rect.y + rect.height) * scale).ceil()),
    }
}

/// Snaps a border band's two edges to whole physical pixels, and returns the snapped
/// `(start, thickness)`, still in logical units.
///
/// `start` is the band's smaller coordinate along its thin axis, `thickness` its extent. Both
/// edges round to the nearest integer independently, then divide back by `scale`. Rounding the
/// edges rather than a centerline is what makes stroke parity fall out for free: measured
/// against femtovg 0.26.0 on this machine's Mesa/Iris, a 1px stroke centered on a half-integer
/// is one fully-lit row, a 4px stroke centered the same way blurs across five rows at partial
/// alpha, while centered on an integer it is exactly four fully-lit rows.
///
/// Returns logical units, not physical: `layout::paint` builds every path in logical
/// coordinates and `TextPainter::resize` hardcodes a device pixel ratio of 1.0, so logical and
/// physical coincide today; `scale` is threaded through for when that stops being true.
///
/// If `thickness` is positive but both edges round to the same integer, the band is forced to
/// one physical pixel instead of vanishing.
pub fn snap_border_band(start: f32, thickness: f32, scale: f32) -> (f32, f32) {
    let near_edge = (start * scale).round();
    let far_edge = ((start + thickness) * scale).round();
    let far_edge = if thickness > 0.0 && far_edge == near_edge { near_edge + 1.0 } else { far_edge };
    (near_edge / scale, (far_edge - near_edge) / scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_scale_is_exact() {
        let r = LogicalRect { x: 10.0, y: 20.0, width: 30.0, height: 40.0 };
        assert_eq!(snap_to_physical(r, 1.0), PhysicalRect { x0: 10, y0: 20, x1: 40, y1: 60 });
    }

    #[test]
    fn already_pixel_aligned_rect_is_unchanged() {
        let r = LogicalRect { x: 4.0, y: 4.0, width: 8.0, height: 8.0 };
        assert_eq!(snap_to_physical(r, 1.0), PhysicalRect { x0: 4, y0: 4, x1: 12, y1: 12 });
    }

    #[test]
    fn fractional_scale_rounds_outward_never_inward() {
        let r = LogicalRect { x: 10.4, y: 0.0, width: 5.0, height: 0.0 };
        let p = snap_to_physical(r, 1.5);
        assert_eq!(p.x0, 15);
        assert_eq!(p.x1, 24);
    }

    #[test]
    fn zero_size_rect_snaps_to_zero_width_box() {
        let r = LogicalRect { x: 3.0, y: 3.0, width: 0.0, height: 0.0 };
        let p = snap_to_physical(r, 2.0);
        assert_eq!(p.x0, 6);
        assert_eq!(p.x1, 6);
    }

    #[test]
    fn negative_coordinates_snap_consistently() {
        let r = LogicalRect { x: -10.6, y: 0.0, width: 5.0, height: 0.0 };
        let p = snap_to_physical(r, 1.0);
        assert_eq!(p.x0, -11); // floor(-10.6) = -11
        assert_eq!(p.x1, -5); // ceil(-5.6) = -5
    }

    #[test]
    fn border_band_already_aligned_is_unchanged() {
        assert_eq!(snap_border_band(4.0, 1.0, 1.0), (4.0, 1.0));
    }

    #[test]
    fn border_band_rounds_each_edge_to_nearest() {
        // Catches a function that snaps only one edge, or derives the far edge from the
        // rounded near edge plus the unrounded thickness instead of rounding both.
        assert_eq!(snap_border_band(10.3, 3.4, 1.0), (10.0, 4.0));
    }

    #[test]
    fn border_band_thinner_than_a_pixel_is_forced_to_one_physical_pixel() {
        assert_eq!(snap_border_band(10.1, 0.2, 1.0), (10.0, 1.0));
    }

    #[test]
    fn border_band_snaps_correctly_at_fractional_scale() {
        assert_eq!(snap_border_band(5.1, 1.0, 2.0), (5.0, 1.0));
    }

    #[test]
    fn border_band_snapping_differs_from_containment_snapping() {
        // A 1px band at 5.6: containment floors/ceils to 5..7 (2px, wrong for a border);
        // band snapping rounds each edge to nearest and gets 6..7, still 1px wide.
        let scale = 1.0;
        let band = LogicalRect { x: 5.6, y: 0.0, width: 1.0, height: 0.0 };
        let containment = snap_to_physical(band, scale);
        assert_eq!((containment.x0, containment.x1), (5, 7));
        assert_eq!(containment.x1 - containment.x0, 2);

        let (start, thickness) = snap_border_band(band.x, band.width, scale);
        assert_eq!((start, thickness), (6.0, 1.0));
    }
}
