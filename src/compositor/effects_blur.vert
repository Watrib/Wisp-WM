#version 100
attribute vec2 vert;
varying vec2 uv;
void main() {
    uv = vert;
    gl_Position = vec4(vert * 2.0 - 1.0, 0.0, 1.0);
}
