// https://github.com/Experience-Monks/glsl-fast-gaussian-blur

#version 330 core

out vec4 fragColor;

uniform sampler2D inputTexture;
uniform vec2 resolution;
uniform vec2 direction;

void main() {
    vec2 uv = vec2(gl_FragCoord.xy / resolution.xy);
    vec2 off1 = vec2(1.3846153846) * direction;
    vec2 off2 = vec2(3.2307692308) * direction;
    fragColor = texture(inputTexture, uv) * 0.2270270270;
    fragColor += texture(inputTexture, uv + (off1 / resolution)) * 0.3162162162;
    fragColor += texture(inputTexture, uv - (off1 / resolution)) * 0.3162162162;
    fragColor += texture(inputTexture, uv + (off2 / resolution)) * 0.0702702703;
    fragColor += texture(inputTexture, uv - (off2 / resolution)) * 0.0702702703;
}
