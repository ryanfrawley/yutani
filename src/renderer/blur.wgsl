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
// Strip pass only: the sharp (un-blurred) offscreen scene, so the strip can
// blur it itself with a per-fragment radius. Bound at group(0) binding 3 by
// the strip pipeline's 4-entry layout; the fullscreen blit/down/up layout has
// no binding 3 and its entry points never reference this, which wgpu allows.
@group(0) @binding(3) var scene_tex: texture_2d<f32>;

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
// Strip pass: takes a quad in pixel space.
//
//   radii.x = tint: 0 = use the blurred scene, 1 = the vertex `color`'s RGB as
//             a solid fill (the hard backing). Mixed per-fragment.
//   radii.y = blur amount, selecting *how* the scene is blurred:
//             < 0  -> sample the precomputed dual-Kawase blur (`src_tex`,
//                     chain[0]) — the constant "fat" frost used by the glass
//                     title-bar band.
//             >= 0 -> blur the sharp scene (`scene_tex`) here with a disk
//                     kernel whose radius is `STRIP_BLUR_MAX_RADIUS * amount`.
//                     0 collapses to a single sharp tap, 1 is maximum blur, and
//                     anything in between is a true intermediate radius — so an
//                     edge strip that ramps `amount` linearly toward the window
//                     edge reads as a linearly increasing blur.
//   color.a = overall strip alpha.
//
// Output is premultiplied RGBA so the PREMULTIPLIED_ALPHA_BLENDING pipeline
// gives mix(scene, result, alpha).
//

// Maximum disk-blur radius (in pixels) reached at blur amount 1.0.
const STRIP_BLUR_MAX_RADIUS: f32 = 36.0;
// Disk taps around the center. Laid out as a Vogel ("sunflower") spiral —
// even coverage with no preferred axis. The count has to keep pace with the
// radius: too few taps over a wide radius point-samples sharp content into
// discrete ghost copies that read as concentric "steps", so this is generous.
const STRIP_BLUR_TAPS: i32 = 32;
const GOLDEN_ANGLE: f32 = 2.399963; // ~137.5° in radians
const TAU: f32 = 6.2831853;

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
    // radii.x = tint (0 = blurred scene, 1 = solid `fill`).
    // radii.y = blur amount (<0 = sample precomputed blur; >=0 = disk-blur the
    //           sharp scene with radius proportional to the amount).
    // color.a = overall strip alpha; color.rgb = the solid `fill` color.
    @location(0) tint: f32,
    @location(1) alpha: f32,
    @location(2) fill: vec3<f32>,
    @location(3) blur_amount: f32,
};

@vertex
fn vs_strip(model: StripVsIn) -> StripVsOut {
    var out: StripVsOut;
    out.clip_position = camera.view_projection * vec4<f32>(model.position, 1.0);
    out.tint = model.radii.x;
    out.alpha = model.color.a;
    out.fill = model.color.rgb;
    out.blur_amount = model.radii.y;
    return out;
}

@fragment
fn fs_strip(in: StripVsOut) -> @location(0) vec4<f32> {
    let uv = in.clip_position.xy * strip_params.texel_size;
    var scene_col: vec3<f32>;
    if (in.blur_amount < 0.0) {
        // Title-bar glass band: the constant precomputed dual-Kawase blur.
        scene_col = textureSampleLevel(src_tex, src_smp, uv, 0.0).rgb;
    } else {
        // Variable-radius disk blur of the sharp scene. The radius scales
        // linearly with `blur_amount`; at 0 every tap collapses onto the
        // center, leaving the scene sharp. `strip_params.texel_size` is
        // 1 / viewport, converting the pixel radius into UV space. Explicit-LOD
        // sampling so the (non-uniform) branch above is legal — the scene and
        // blur textures are not mipmapped, so LOD 0 is the only level anyway.
        let radius = STRIP_BLUR_MAX_RADIUS * in.blur_amount;
        let texel = strip_params.texel_size;
        // Rotate the whole spiral by a per-fragment pseudo-random angle. Without
        // this every fragment samples the same fixed directions, so a sharp
        // high-contrast edge reproduces at each tap as a coherent ghost ring
        // (the "steps"). Jittering the rotation per fragment scatters those
        // ghosts into fine, low-amplitude noise that reads as a smooth blur.
        let hash = fract(sin(dot(in.clip_position.xy, vec2<f32>(12.9898, 78.233))) * 43758.5453);
        let rot = hash * TAU;
        var sum = textureSampleLevel(scene_tex, src_smp, uv, 0.0).rgb;
        for (var i = 0; i < STRIP_BLUR_TAPS; i = i + 1) {
            let fi = f32(i);
            let ang = fi * GOLDEN_ANGLE + rot;
            // sqrt keeps the spiral's samples uniformly dense across the disk
            // rather than clustering at the center.
            let dist = sqrt((fi + 0.5) / f32(STRIP_BLUR_TAPS));
            let off = vec2<f32>(cos(ang), sin(ang)) * (dist * radius) * texel;
            sum += textureSampleLevel(scene_tex, src_smp, uv + off, 0.0).rgb;
        }
        scene_col = sum / f32(STRIP_BLUR_TAPS + 1);
    }
    let rgb = mix(scene_col, in.fill, in.tint);
    return vec4<f32>(rgb * in.alpha, in.alpha);
}
