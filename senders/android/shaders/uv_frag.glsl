#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require

precision mediump float;

in vec2 tex_coord;

uniform samplerExternalOES u_texture;
uniform vec2 u_src_size;

layout (location = 0) out vec4 out_u;
layout (location = 1) out vec4 out_v;

void main() {
    vec2 step = 1.0 / u_src_size;
    vec3 q1 = texture(u_texture, tex_coord).rgb;
    vec3 q2 = texture(u_texture, tex_coord + vec2(step.x, 0.0)).rgb;
    vec3 q3 = texture(u_texture, tex_coord + vec2(0.0, step.y)).rgb;
    vec3 q4 = texture(u_texture, tex_coord + vec2(step.x, step.y)).rgb;

    vec3 sampl = (q1 + q2 + q3 + q4) * 0.25;
    float u = -0.1146 * sampl.r - 0.3854 * sampl.g + 0.5 * sampl.b + 0.5;
    float v = 0.5 * sampl.r - 0.4542 * sampl.g - 0.0458 * sampl.b + 0.5;

    out_u = vec4(u, 0.0, 0.0, 0.0);
    out_v = vec4(v, 0.0, 0.0, 0.0);
}
