#version 100
precision highp float;
uniform sampler2D tex;
uniform vec2 half_pixel;
uniform float offset;
varying vec2 uv;
void main() {
    vec2 d = half_pixel * (offset + 1.0);
    vec4 axial = texture2D(tex, uv + vec2(-2.0*d.x, 0.0))
               + texture2D(tex, uv + vec2( 2.0*d.x, 0.0))
               + texture2D(tex, uv + vec2(0.0, -2.0*d.y))
               + texture2D(tex, uv + vec2(0.0,  2.0*d.y));
    vec4 diagonals = texture2D(tex, uv + vec2(-d.x, -d.y))
                   + texture2D(tex, uv + vec2( d.x, -d.y))
                   + texture2D(tex, uv + vec2(-d.x,  d.y))
                   + texture2D(tex, uv + vec2( d.x,  d.y));
    gl_FragColor = (axial + 2.0*diagonals) / 12.0;
}
