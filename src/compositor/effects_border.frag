// Rounded gradient border ring, drawn over the area the compositor allocates
// (window content rect expanded by the border width on every side).
//
// Coordinates: v_coords span the allocated area, `size` is that area in
// pixels with y growing downward from the top-left.

precision highp float;

uniform float alpha;
varying vec2 v_coords;
uniform vec2 size;

uniform vec4 color_from;
uniform vec4 color_to;
uniform vec2 grad_vec;
uniform vec2 geo_size;
uniform vec4 outer_radius;      // per-corner radius of the outer edge, px
uniform float border_width;     // px

void main() {
    vec2 p = v_coords * size;

    // Position along the gradient direction. grad_vec is unit-length, so the
    // projection alone gives the interpolation factor (clamped to [0, 1]).
    float frac = clamp(dot(p, grad_vec) / dot(grad_vec, grad_vec), 0.0, 1.0);
    vec4 color = mix(color_from, color_to, frac);

    // Keep the ring inside the rounded outer edge.
    color = color * wisp_rounding_alpha(p, geo_size, outer_radius);

    // Punch out the window content: the border is only the ring between the
    // outer edge and the inner edge inset by border_width with its own radius.
    if (border_width > 0.0) {
        vec2 inner_p = p - vec2(border_width);
        vec2 inner_size = vec2(size.x - border_width * 2.0, size.y - border_width * 2.0);
        if (0.0 <= inner_p.x && inner_p.x <= inner_size.x &&
                0.0 <= inner_p.y && inner_p.y <= inner_size.y) {
            vec4 inner_radius = max(outer_radius - vec4(border_width), 0.0);
            float inner_mask = wisp_rounding_alpha(inner_p, inner_size, inner_radius);
            color = color * (1.0 - inner_mask);
        }
    }

    color = color * alpha;
    gl_FragColor = color;
}
