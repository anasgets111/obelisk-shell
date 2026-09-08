// Interleaved bands sweeping in opposite directions, `Shaders/frag/wp_Stripes.frag` reduced to its
// mask. The reference's per-stripe delay, edge shadow and vignette are dropped: they are three
// effects wearing one name, and the band motion is what reads.
//
// params: count is how many stripes; angle turns them, in degrees; softness widens each edge.
uniform float count;
uniform float angle;
uniform float softness;

void main() {
    vec4 from = oblisk_from(v_uv);
    vec4 to = oblisk_to(v_uv);

    float band = mix(0.001, 0.3, softness * softness);
    float radians = radians(angle);
    float across = cos(radians);
    float along = sin(radians);

    // Along the stripes, and perpendicular to them. The perpendicular axis is what each stripe's
    // edge travels down, so its range has to cover the rotated box's whole extent.
    float over = v_uv.x * across + v_uv.y * along;
    float down = -v_uv.x * along + v_uv.y * across;
    float reach = abs(across) + abs(along);

    float stripes = max(1.0, count);
    bool odd = mod(floor(over * stripes), 2.0) != 0.0;
    float margin = band * 2.0;
    float travel = 2.0 * reach + margin * 2.0;
    float edge = odd ? reach + margin - u_progress * travel : -reach - margin + u_progress * travel;
    float mask = smoothstep(edge - band, edge + band, down);
    fragColor = mix(from, to, odd ? mask : 1.0 - mask);
}
