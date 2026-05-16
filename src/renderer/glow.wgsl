// Saturation- and/or palette-driven bloom.
//
// Two independent match modes feed the bright pass:
//   - SATURATION: keep fragments whose HSV saturation exceeds `threshold`.
//   - BRIGHT-ANSI: keep fragments whose hue matches one of the colour
//     scheme's 8 bright ANSI variants, ignoring brightness/lightness so
//     antialiased glyphs still register even when blended toward the
//     background. Greyscale bright variants (sat < `min_palette_sat`) are
//     skipped on the host side (their slot's `.w` is set to 0).
// When both modes are on, the per-pixel weight is `max(sat, palette)` so a
// pixel only has to satisfy one to glow.
// The blur chain and additive composite below are dual-Kawase, matching
// `blur.wgsl`.

struct BlurParams {
    // 1 / source-texture dimensions, packed to 16 bytes.
    texel_size: vec2<f32>,
    _pad: vec2<f32>,
};

struct GlowParams {
    threshold: f32,        // HSV-saturation cutoff for SATURATION mode.
    intensity: f32,        // Multiplier applied at additive composite.
    softness: f32,         // Smoothstep width above `threshold` (SAT mode).
    hue_tolerance: f32,    // Degrees of hue slop for BRIGHT-ANSI mode.
    use_saturation: f32,   // 0 = off, 1 = on.
    use_bright_ansi: f32,  // 0 = off, 1 = on.
    min_palette_sat: f32,  // Pixel sat below this never matches a palette hue.
    _pad: f32,
};

// One entry per bright ANSI slot. Stored as (hue_deg, sat, _, active):
//   active = 1.0 → slot is used; 0.0 → skipped (e.g. bright black / white).
struct BrightHues {
    entries: array<vec4<f32>, 8>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_smp: sampler;
@group(0) @binding(2) var<uniform> blur_params: BlurParams;
@group(0) @binding(3) var<uniform> glow_params: GlowParams;
@group(0) @binding(4) var<uniform> bright_hues: BrightHues;

struct VsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vid: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vid << 1u) & 2u);
    let y = f32(vid & 2u);
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

fn hsv_saturation(rgb: vec3<f32>) -> f32 {
    let mx = max(max(rgb.r, rgb.g), rgb.b);
    let mn = min(min(rgb.r, rgb.g), rgb.b);
    if (mx <= 0.0) {
        return 0.0;
    }
    return (mx - mn) / mx;
}

// HSV hue in degrees, or a negative sentinel when undefined (max == min).
fn hsv_hue(rgb: vec3<f32>) -> f32 {
    let mx = max(max(rgb.r, rgb.g), rgb.b);
    let mn = min(min(rgb.r, rgb.g), rgb.b);
    let d = mx - mn;
    if (d <= 0.0) {
        return -1.0;
    }
    var h: f32;
    if (mx == rgb.r) {
        h = (rgb.g - rgb.b) / d;
    } else if (mx == rgb.g) {
        h = 2.0 + (rgb.b - rgb.r) / d;
    } else {
        h = 4.0 + (rgb.r - rgb.g) / d;
    }
    h = h * 60.0;
    if (h < 0.0) {
        h = h + 360.0;
    }
    return h;
}

// Shortest distance between two hue angles, in degrees (0..=180).
fn hue_distance(a: f32, b: f32) -> f32 {
    let d = abs(a - b);
    return min(d, 360.0 - d);
}

fn saturation_weight(sat: f32) -> f32 {
    let lo = glow_params.threshold;
    let hi = min(lo + max(glow_params.softness, 0.0001), 1.0);
    return smoothstep(lo, hi, sat);
}

// Pixel matches a bright variant when its hue lands within `hue_tolerance`
// of the slot's hue. A small `softness` band beyond that ramps to 0 to
// avoid a hard cutoff. Returns the highest match across all 8 slots.
fn bright_ansi_weight(rgb: vec3<f32>) -> f32 {
    let sat = hsv_saturation(rgb);
    if (sat < glow_params.min_palette_sat) {
        return 0.0;
    }
    let hue = hsv_hue(rgb);
    if (hue < 0.0) {
        return 0.0;
    }
    let tol = max(glow_params.hue_tolerance, 0.0);
    let edge = max(glow_params.softness, 0.0001) * 180.0;
    var best: f32 = 0.0;
    for (var i: u32 = 0u; i < 8u; i = i + 1u) {
        let entry = bright_hues.entries[i];
        if (entry.w < 0.5) {
            continue;
        }
        let target_hue = entry.x;
        let target_sat = entry.y;
        let hd = hue_distance(hue, target_hue);
        let w = 1.0 - smoothstep(tol, tol + edge, hd);
        // Confidence rises with both pixel and target saturation — bright
        // variants are saturated by definition, and a more-saturated pixel
        // looks more like "really that colour".
        let conf = min(sat, target_sat);
        best = max(best, w * conf);
    }
    return best;
}

@fragment
fn fs_bright(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(src_tex, src_smp, in.uv);
    var weight: f32 = 0.0;
    if (glow_params.use_saturation > 0.5) {
        weight = max(weight, saturation_weight(hsv_saturation(c.rgb)));
    }
    if (glow_params.use_bright_ansi > 0.5) {
        weight = max(weight, bright_ansi_weight(c.rgb));
    }
    return vec4<f32>(c.rgb * weight, 1.0);
}

// Dual-Kawase down: centre tap + four diagonal half-texel taps. Weights = 8.
@fragment
fn fs_down(in: VsOut) -> @location(0) vec4<f32> {
    let o = blur_params.texel_size;
    var sum = textureSample(src_tex, src_smp, in.uv) * 4.0;
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(-o.x, -o.y));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>( o.x, -o.y));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>(-o.x,  o.y));
    sum += textureSample(src_tex, src_smp, in.uv + vec2<f32>( o.x,  o.y));
    return sum / 8.0;
}

// Dual-Kawase up: 8-tap diamond. Weights = 12.
@fragment
fn fs_up(in: VsOut) -> @location(0) vec4<f32> {
    let o = blur_params.texel_size;
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

// Composite: sample the blurred bright buffer and emit RGB scaled by
// intensity. Alpha = 0 so additive blending leaves the destination's alpha
// untouched (BlendComponent for alpha = Zero,One,Add upstream).
@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    let glow = textureSample(src_tex, src_smp, in.uv);
    return vec4<f32>(glow.rgb * glow_params.intensity, 0.0);
}
