// Dual-Kawase blur (Bjørge 2015): downsample → upsample chain. Plus a
// straight blit for compositing the offscreen scene back to the swapchain,
// and a strip pass that mixes the upsampled blur over the scene with a
// vertex-driven alpha.

struct BlurParams {
    // 1 / source-texture dimensions (in texels), packed with two unused
    // floats so the buffer is a comfy 16 bytes.
    texel_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_smp: sampler;
@group(0) @binding(2) var<uniform> params: BlurParams;

struct VsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Fullscreen triangle from a single vertex index — no buffers needed.
@vertex
fn vs_fullscreen(@builtin(vertex_index) vid: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vid << 1u) & 2u);
    let y = f32(vid & 2u);
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

// Plain textured blit (used to copy the offscreen scene to the swapchain).
@fragment
fn fs_blit(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, src_smp, in.uv);
}

// Down-sample: center tap + 4 diagonal half-texel taps. Weights sum to 1.
@fragment
fn fs_down(in: VsOut) -> @location(0) vec4<f32> {
    let o = params.texel_size;
    var sum = textureSample(src_tex, src_smp, in.uv) * 4.0;
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(-o.x, -o.y));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>( o.x, -o.y));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(-o.x,  o.y));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>( o.x,  o.y));
    return sum / 8.0;
}

// Up-sample: 8-tap diamond. No center tap. Weights sum to 12.
@fragment
fn fs_up(in: VsOut) -> @location(0) vec4<f32> {
    let o = params.texel_size;
    var sum = textureSample(src_tex, src_smp, in.uv + vec2<f32>(-o.x * 2.0, 0.0));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>( o.x * 2.0, 0.0));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(0.0, -o.y * 2.0));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(0.0,  o.y * 2.0));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(-o.x,  o.y)) * 2.0;
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>( o.x,  o.y)) * 2.0;
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(-o.x, -o.y)) * 2.0;
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>( o.x, -o.y)) * 2.0;
    return sum / 12.0;
}

//
// Strip pass: takes a quad in pixel space whose vertex `color.a` is the
// blur-mix weight (RGB ignored). Samples the upsampled blur at the
// fragment's screen position and outputs a premultiplied RGBA so the
// PREMULTIPLIED_ALPHA_BLENDING pipeline gives mix(scene, blur, alpha).
//

struct CameraUniform {
    view_projection: mat4x4<f32>,
};
@group(1) @binding(0) var<uniform> camera: CameraUniform;
@group(2) @binding(0) var<uniform> strip_params: BlurParams;

struct StripVsIn {
    @location(0) position: vec3<f32>,
    @location(1) tex_coords: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) local_pos: vec2<f32>,
    @location(4) half_size: vec2<f32>,
    @location(5) radii: vec4<f32>,
};

struct StripVsOut {
    @builtin(position) clip_position: vec4<f32>,
    // color.r = bg-tint strength (0 = pure blur, 1 = pure bg color).
    // color.a = overall strip alpha. Tint masks blur near the toolbar so
    // window-chrome contrast is preserved.
    @location(0) tint: f32,
    @location(1) alpha: f32,
};

@vertex
fn vs_strip(model: StripVsIn) -> StripVsOut {
    var out: StripVsOut;
    out.clip_position = camera.view_projection * vec4<f32>(model.position, 1.0);
    out.tint = model.color.r;
    out.alpha = model.color.a;
    return out;
}

@fragment
fn fs_strip(in: StripVsOut) -> @location(0) vec4<f32> {
    let uv = in.clip_position.xy * strip_params.texel_size;
    let blur = textureSample(src_tex, src_smp, uv);
    // Bg color is hardcoded to clear_color() in main.rs (white).
    let rgb = mix(blur.rgb, vec3<f32>(1.0), in.tint);
    return vec4<f32>(rgb * in.alpha, in.alpha);
}
