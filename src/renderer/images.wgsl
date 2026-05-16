// Textured-quad pipeline for image placements (Sixel / iTerm2 / Kitty
// payloads land here once decoded). Vertices are in framebuffer pixels;
// `camera.view_projection` is the same ortho the grid pipeline uses, so we
// share group(1) with the cell renderer.

struct CameraUniform {
    view_projection: mat4x4<f32>,
};
@group(1) @binding(0)
var<uniform> camera: CameraUniform;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(model: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.uv = model.uv;
    out.clip_position = camera.view_projection * vec4<f32>(model.position, 1.0);
    return out;
}

@group(0) @binding(0)
var t_image: texture_2d<f32>;
@group(0) @binding(1)
var s_image: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Source is straight-alpha RGBA8 (whatever the `image` crate decoded).
    // Premultiply on the way out so the pipeline can use
    // `PREMULTIPLIED_ALPHA_BLENDING` and composite cleanly over the bg layer.
    let s = textureSample(t_image, s_image, in.uv);
    return vec4<f32>(s.rgb * s.a, s.a);
}
