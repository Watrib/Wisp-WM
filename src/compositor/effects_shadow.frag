// Soft rounded-rectangle drop shadow with a cut-out of the window interior so
// transparent windows are not double-darkened.
//
// The shadow of a box blurred with a gaussian integrates to a closed form
// along each axis (error-function approximation); a compact 4-sample
// integration along the y axis keeps the per-pixel cost low while remaining
// smooth. See Evan Wallace's public-domain reference for the technique.

precision highp float;

uniform float alpha;
varying vec2 v_coords;
uniform vec2 size;

uniform vec4 shadow_color;
uniform float sigma;
uniform vec2 window_offset;     // window content position inside the area, px
uniform vec2 window_size;       // window content size, px
uniform vec4 radius;            // per-corner window radius, px

float gaussian(float x, float sigma) {
    const float PI = 3.141592653589793;
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * PI) * sigma);
}

// Abramowitz-Stegun quality erf approximation, accurate enough for shadows.
vec2 erf(vec2 x) {
    vec2 s = sign(x), a = abs(x);
    x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    x *= x;
    return s - s / (x * x);
}

// Blurred mask along the x axis for a point at y offset `y` from the band.
float shadow_x(float x, float y, float sigma, float corner, vec2 half_size) {
    float delta = min(half_size.y - corner - abs(y), 0.0);
    float curved = half_size.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    vec2 integral = 0.5 + 0.5 * erf((x + vec2(-curved, curved)) * (sqrt(0.5) / sigma));
    return integral.y - integral.x;
}

// Shadow mask of the box [lower, upper] at the query point.
float box_shadow(vec2 lower, vec2 upper, vec2 point, float sigma, float corner) {
    vec2 center = (lower + upper) * 0.5;
    vec2 half_size = (upper - lower) * 0.5;
    point -= center;

    // The signal is only meaningful near the box; clamp the sample band.
    float low = point.y - half_size.y;
    float high = point.y + half_size.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);

    float y_step = (end - start) / 4.0;
    float y = start + y_step * 0.5;
    float value = 0.0;
    for (int i = 0; i < 4; i++) {
        value += shadow_x(point.x, point.y - y, sigma, corner, half_size) * gaussian(y, sigma) * y_step;
        y += y_step;
    }
    return value;
}

void main() {
    vec2 p = v_coords * size;
    vec2 wp = p - window_offset;

    vec4 color = shadow_color;

    float shadow_value;
    if (sigma < 0.1) {
        // Tiny sigma: a plain rounded window rect, hidden behind the window
        // once the interior is cut out.
        shadow_value = wisp_rounding_alpha(wp, window_size, radius);
    } else {
        shadow_value = box_shadow(
            window_offset,
            window_offset + window_size,
            p,
            sigma,
            radius.x
        );
    }
    color = color * shadow_value;

    // Cut out the window interior (clipped to the window rect).
    if (0.0 <= wp.x && wp.x <= window_size.x &&
            0.0 <= wp.y && wp.y <= window_size.y) {
        color = color * (1.0 - wisp_rounding_alpha(wp, window_size, radius));
    }

    color = color * alpha;
    gl_FragColor = color;
}
