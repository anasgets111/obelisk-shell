// The new picture opening out of a point, `Shaders/frag/wp_Disc.frag` reduced to its mask.
//
// params: center_x, center_y in 0..1; softness widens the blended ring. A config randomising the
// centre per change is what `WallpaperService.qml` does.
uniform float center_x;
uniform float center_y;
uniform float softness;

void main() {
    vec4 from = obelisk_from(v_uv);
    vec4 to = obelisk_to(v_uv);

    float band = mix(0.001, 0.5, softness * softness);
    // Distances measured in an aspect-corrected space, or the disc is an ellipse on any screen
    // that is not square.
    float aspect = u_size.x / max(1.0, u_size.y);
    vec2 point = vec2(v_uv.x * aspect, v_uv.y);
    vec2 centre = vec2(center_x * aspect, center_y);
    float distance = length(point - centre);

    // The far corner, so the disc has covered everything at progress 1 wherever its centre is.
    float furthest = length(vec2(max(centre.x, aspect - centre.x), max(centre.y, 1.0 - centre.y)));
    float widened = band * max(1.0, aspect);
    // Opening from one band *behind* zero, not from zero: a symmetric smoothing band around a
    // radius of exactly zero already has the centre pixel halfway across at progress 0.
    float radius = mix(-widened, furthest + widened, u_progress);
    fragColor = mix(to, from, smoothstep(radius - widened, radius + widened, distance));
}
