// The new picture arriving as blocks that shrink to pixels, `Shaders/frag/wp_Pixelate.frag`.
//
// params: softness sets how large the first blocks are, as a fraction of the shorter edge.
uniform float softness;

void main() {
    vec4 from = oblisk_from(v_uv);
    vec4 to = oblisk_to(v_uv);

    // Cell size in node pixels, shrinking to one as the run finishes. Screen-relative, so the
    // blocks read the same size on a laptop panel and a 4K one.
    float shortest = min(max(1.0, u_size.x), max(1.0, u_size.y));
    float first = mix(shortest * 0.10, shortest * 0.80, clamp(softness, 0.0, 1.0));
    float cell = mix(first, 1.0, u_progress);

    vec2 grid = max(vec2(1.0), u_size / max(1.0, cell));
    vec2 quantised = (floor(v_uv * grid) + 0.5) / grid;
    vec4 blocky = oblisk_to(quantised);

    // Sharpen only at the end, or the last blocks pop rather than resolve.
    vec4 arriving = mix(blocky, to, smoothstep(0.75, 1.0, u_progress));
    fragColor = mix(from, arriving, u_progress * u_progress * (3.0 - 2.0 * u_progress));
}
