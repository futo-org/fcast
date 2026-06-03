#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require

in vec4 position;
in vec4 in_tex_coord;

uniform mat4 u_tex_matrix;

out vec2 tex_coord;

void main() {
    gl_Position = position;
    tex_coord = (u_tex_matrix * in_tex_coord).xy;
}
