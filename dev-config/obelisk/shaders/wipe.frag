// A straight edge sweeping across, `Shaders/frag/wp_Wipe.frag` reduced to its mask.
//
// The engine has already fitted both pictures and hands them back through `obelisk_from`/
// `obelisk_to`, so none of the reference's `sampleWithFillMode` prelude is needed here.
//
// params: direction 0 = new enters from the right, 1 = from the left, 2 = from the bottom,
// 3 = from the top; softness widens the blended band at the edge.
uniform float direction;
uniform float softness;

void main() {
    vec4 from = obelisk_from(v_uv);
    vec4 to = obelisk_to(v_uv);

    // Non-linear, the way the reference maps it: most of the useful range is at the low end.
    float band = mix(0.001, 0.5, softness * softness);
    // Extended past both ends, so the edge has finished leaving the screen at progress 1 rather
    // than stopping half a band short of it.
    float swept = u_progress * (1.0 + 2.0 * band) - band;

    float axis = direction < 1.5 ? v_uv.x : v_uv.y;
    bool from_far_side = direction < 0.5 || (direction >= 1.5 && direction < 2.5);
    float edge = from_far_side ? 1.0 - swept : swept;
    float across = smoothstep(edge - band, edge + band, axis);
    fragColor = from_far_side ? mix(from, to, across) : mix(to, from, across);
}
