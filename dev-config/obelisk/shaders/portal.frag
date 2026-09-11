// The old picture collapsing into a point, `Shaders/frag/wp_Portal.frag` reduced to its mask: the
// disc run backwards, with the old picture inside the shrinking circle instead of the new one
// inside a growing one.
//
// params: center_x, center_y in 0..1; softness widens the blended ring.
uniform float center_x;
uniform float center_y;
uniform float softness;

void main() {
    vec4 from = obelisk_from(v_uv);
    vec4 to = obelisk_to(v_uv);

    float band = mix(0.001, 0.45, softness * softness);
    float aspect = u_size.x / max(1.0, u_size.y);
    vec2 point = vec2(v_uv.x * aspect, v_uv.y);
    vec2 centre = vec2(center_x * aspect, center_y);
    float distance = length(point - centre);
    float furthest = length(vec2(max(centre.x, aspect - centre.x), max(centre.y, 1.0 - centre.y)));

    // Smoothstepped progress, so the collapse eases at both ends without the config choosing a
    // curve for it -- the `easing` on the transition still shapes the whole run.
    float eased = u_progress * u_progress * (3.0 - 2.0 * u_progress);
    // Starting a band beyond the furthest corner for the same reason `disc.frag` starts behind
    // zero: at the exact corner distance the corner is already halfway across.
    float radius = mix(-band, furthest + 2.0 * band, 1.0 - eased);
    fragColor = mix(from, to, smoothstep(radius - band, radius + band, distance));
}
