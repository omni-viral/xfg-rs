#version 450 core
#extension GL_ARB_separate_shader_objects : enable

layout(binding=0, set=0) uniform VertexArgs {
    mat4 proj;
    mat4 view;
    mat4 model;
};

layout(location = 0) in vec3 position;
layout(location = 1) in vec3 normal;

layout(location = 0) out VertexData {
    vec4 position;
    vec3 normal;
} vertex;

void main() {
    vertex.position = model * vec4(position, 1.0);
    vertex.normal = mat3(model) * normal;
    gl_Position = proj * view * vertex.position;
}
