//
// Vertex shader
//

struct CameraUniform {
    view_projection: mat4x4<f32>,
};
@group(1) @binding(0) 
var<uniform> camera: CameraUniform;

struct InstanceInput {
    @location(5) model_matrix0: vec4<f32>,
    @location(6) model_matrix1: vec4<f32>,
    @location(7) model_matrix2: vec4<f32>,
    @location(8) model_matrix3: vec4<f32>,
    @location(9) model_tex_coords: vec4<f32>,
};

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) tex_coords: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    // @location(0) color: vec3<f32>,
    @location(0) tex_coords: vec2<f32>,
};

@vertex
fn vs_main(
    model: VertexInput,
    instance: InstanceInput,
) -> VertexOutput {
    let model_matrix = mat4x4<f32>(
        instance.model_matrix0,
        instance.model_matrix1,
        instance.model_matrix2,
        instance.model_matrix3,
    );
    var out: VertexOutput;
    out.tex_coords = model.tex_coords;
    out.clip_position = camera.view_projection * model_matrix * vec4<f32>(model.position, 1.0);
    return out;
}

//
// Fragment shader
//
@group(0) @binding(0)
var t_diffuse: texture_2d<f32>;
@group(0) @binding(1)
var s_diffuse: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    var out: vec4<f32> = vec4<f32>(textureSample(t_diffuse, s_diffuse, in.tex_coords).r);
    return out;
}


