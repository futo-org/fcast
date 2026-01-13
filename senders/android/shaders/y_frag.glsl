#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require

precision mediump float;

in vec2 tex_coord;

uniform samplerExternalOES u_texture;

layout (location = 0) out vec4 out_y;

void main() {
    vec3 rgb = texture(u_texture, tex_coord).rgb;
    float y = 0.2126 * rgb.r + 0.7152 * rgb.g + 0.0722 * rgb.b;

    out_y = vec4(y, 0.0, 0.0, 0.0);
}
