// Rounded-corner clipping fragment for the texture-program override path.
// Coordinates: v_coords are pure texture coordinates; `input_to_geo` maps them
// into the window geometry, normalized [0, 1] over `geo_size`.

precision highp float;

uniform sampler2D tex;
uniform float alpha;
varying vec2 v_coords;

uniform vec2 geo_size;
uniform vec4 corner_radius; // (top_left, top_right, bottom_right, bottom_left) px
uniform mat3 input_to_geo;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec3 coords_geo = input_to_geo * vec3(v_coords, 1.0);
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x ||
            coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        // Outside the window geometry: nothing drawn.
        color = vec4(0.0);
    } else {
        // Corner rounding mask inside the geometry.
        color = color * wisp_rounding_alpha(coords_geo.xy * geo_size, geo_size, corner_radius);
    }

    color = color * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
