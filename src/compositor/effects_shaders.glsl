// Anti-aliased rounded-corner alpha mask for a rectangle.
//
// A point on a rounded rectangle is nearest to either the boundary between the
// corner arcs or to the perimeter of the nearest corner circle. The signed
// distance to that perimeter turns directly into coverage; we smooth over one
// pixel (0.5 px falloff) for anti-aliasing.
//
// p is in [0, size], y grows downward (top-left origin), matching Smithay
// texture coordinates for the default transform. radius holds one value per
// corner: (top_left, top_right, bottom_right, bottom_left).

precision highp float;

float wisp_rounding_alpha(vec2 p, vec2 size, vec4 radius) {
    float r = radius.x;
    if (p.x >= size.x * 0.5) {
        r = radius.y;
    }
    if (p.y >= size.y * 0.5) {
        r = (p.x >= size.x * 0.5) ? radius.z : radius.w;
    }
    r = min(r, min(size.x, size.y) * 0.5);
    if (r <= 0.0) {
        return 1.0;
    }
    vec2 corner = clamp(p, vec2(r, r), vec2(size.x - r, size.y - r));
    float d = length(p - corner) - r;
    return clamp(0.5 - d, 0.0, 1.0);
}
