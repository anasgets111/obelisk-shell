//! Subpixel-to-physical-pixel snapping math (build-steps.md Phase 4, point 3).
//!
//! build-steps.md cites `docs/oblisk-layout-engine-geometry.md § 5` for this, but that
//! section is actually "Overlay Input Region Bounding Box Calculations" (Phase 3's
//! click-through input region, already implemented in `wayland::mod`) -- a stale
//! cross-reference, not text/border snapping (see docs/adr/0012). What §5.1 *does*
//! give is the general technique Oblisk uses for snapping fractional layout
//! coordinates to physical pixel boundaries: floor the top-left corner, ceil the
//! bottom-right, so the physical box always fully contains the logical one. This
//! module is that same technique as a small reusable function, for text line boxes
//! and paint clips instead of input regions.
//!
//! Borders round differently, and the two must not be conflated. [`snap_to_physical`]
//! grows a box outward so it never shaves a pixel the thing inside it legitimately
//! covered, which is right for a containment box and wrong for a border: growing a
//! 1px border outward makes it 2px. [`snap_border_band`] rounds each edge to the
//! nearest pixel instead, so a border keeps the width the config asked for. Picking
//! the wrong one of the two is the mistake this module is shaped to make obvious.

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

/// Snaps `rect` to physical pixel boundaries at fractional output scale `scale`.
///
/// The top-left corner floors down and the bottom-right corner ceils up, so the
/// snapped rect always fully contains the logical one -- shrinking would clip a glyph
/// or a border stroke, but growing by less than one physical pixel per edge is what
/// prevents the antialiasing blur build-steps.md Phase 4 point 3 calls out.
pub fn snap_to_physical(rect: LogicalRect, scale: f32) -> PhysicalRect {
    PhysicalRect {
        x0: (rect.x * scale).floor() as i32,
        y0: (rect.y * scale).floor() as i32,
        x1: ((rect.x + rect.width) * scale).ceil() as i32,
        y1: ((rect.y + rect.height) * scale).ceil() as i32,
    }
}

/// Snaps a border band's two edges to whole physical pixels, and returns the snapped
/// `(start, thickness)`, still in logical units (docs/build-steps.md Phase 19 item 7).
///
/// `start` is the band's smaller coordinate along its thin axis (a horizontal border's
/// `y`, say) and `thickness` its extent along that axis. Both edges -- `start * scale`
/// and `(start + thickness) * scale` -- round to the nearest integer independently, then
/// divide back by `scale`. Rounding the edges, not a centerline, is what makes stroke
/// parity fall out for free: an even-width band's centerline lands on an integer, an
/// odd-width band's on a half-integer, matching how femtovg actually rasterizes a stroke
/// (measured directly against femtovg 0.26.0 on this machine's Mesa/Iris: a 1px stroke
/// centered on a half-integer is one fully-lit row, a 4px stroke centered on the same
/// half-integer blurs across five rows at partial alpha, while centered on an integer it
/// is exactly four fully-lit rows). The deleted `snap_border_to_physical` picked a
/// half-integer centerline unconditionally and was correct only for odd widths.
///
/// Returns logical units, not physical. `layout::paint` builds every path in logical
/// coordinates, and `TextPainter::resize` hardcodes a device pixel ratio of 1.0, so the
/// canvas applies no scale transform of its own -- a caller that got physical units back
/// would have to convert every one before building a path. Logical and physical
/// therefore coincide today; `scale` is threaded through for when that stops being true.
///
/// If `thickness` is positive but both edges round to the same integer, the band is
/// forced to one physical pixel instead of vanishing -- a border the config asked for
/// must not disappear because it landed between two pixels.
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
        // 1.5x: origin floors from 15.6 to 15, far edge ceils from 23.1 to 24 -- the
        // physical box (15..24) strictly contains the logical box's scaled span.
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
        // A node positioned left of/above its surface origin (e.g. mid-scroll).
        let r = LogicalRect { x: -10.6, y: 0.0, width: 5.0, height: 0.0 };
        let p = snap_to_physical(r, 1.0);
        assert_eq!(p.x0, -11); // floor(-10.6) = -11
        assert_eq!(p.x1, -5); // ceil(-5.6) = -5
    }

    #[test]
    fn border_band_already_aligned_is_unchanged() {
        // Catches a rounding function that nudges an edge already on an integer --
        // both 4.0 and 5.0 round to themselves, so start and thickness must come
        // back exactly as given.
        assert_eq!(snap_border_band(4.0, 1.0, 1.0), (4.0, 1.0));
    }

    #[test]
    fn border_band_rounds_each_edge_to_nearest() {
        // Edges 10.3 and 13.7 round independently to 10 and 14 -- catches a
        // function that snaps only one edge, or that derives the far edge from the
        // rounded near edge plus the unrounded thickness instead of rounding both.
        assert_eq!(snap_border_band(10.3, 3.4, 1.0), (10.0, 4.0));
    }

    #[test]
    fn border_band_thinner_than_a_pixel_is_forced_to_one_physical_pixel() {
        // 10.1 and 10.3 both round to 10, which would zero the band out -- catches
        // a hairline border silently disappearing instead of being forced visible.
        assert_eq!(snap_border_band(10.1, 0.2, 1.0), (10.0, 1.0));
    }

    #[test]
    fn border_band_snaps_correctly_at_fractional_scale() {
        // At 2x scale, edges 5.1*2=10.2 and 6.1*2=12.2 round to 10 and 12, dividing
        // back to logical 5.0 and thickness 1.0 -- catches `scale` applied only on
        // the way in, or forgotten on the way back out.
        assert_eq!(snap_border_band(5.1, 1.0, 2.0), (5.0, 1.0));
    }

    #[test]
    fn border_band_snapping_differs_from_containment_snapping() {
        // The two functions sit side by side now, so this pins the case where picking
        // the wrong one shows up. Take the same 1-wide band at 5.6. Containment floors
        // the near edge to 5 and ceils the far edge to 7, turning a 1px border into a
        // 2px one; band snapping rounds each edge to nearest and gets 6..7, still one
        // pixel wide. That width difference is the whole reason both functions exist.
        let scale = 1.0;
        let band = LogicalRect { x: 5.6, y: 0.0, width: 1.0, height: 0.0 };
        let containment = snap_to_physical(band, scale);
        assert_eq!((containment.x0, containment.x1), (5, 7));
        assert_eq!(containment.x1 - containment.x0, 2);

        let (start, thickness) = snap_border_band(band.x, band.width, scale);
        assert_eq!((start, thickness), (6.0, 1.0));
    }
}
