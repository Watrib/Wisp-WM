#version 100
precision highp float;
uniform sampler2D tex;
uniform vec2 half_pixel;
uniform float offset;
varying vec2 uv;
void main() {
    vec2 step_px = half_pixel * (offset + 1.0);
    vec4 center = texture2D(tex, uv) * 4.0;
    vec4 corners = texture2D(tex, uv + vec2(-step_px.x, -step_px.y))
                 + texture2D(tex, uv + vec2( step_px.x, -step_px.y))
                 + texture2D(tex, uv + vec2(-step_px.x,  step_px.y))
                 + texture2D(tex, uv + vec2( step_px.x,  step_px.y));
    gl_FragColor = (center + corners) * 0.125;
}
