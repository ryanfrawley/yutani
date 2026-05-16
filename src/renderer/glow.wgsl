// Brightness-, palette-, and foreground-driven bloom.
//
// Three independent match modes feed the bright pass:
//   - BRIGHTNESS: keep fragments whose HSV value (max of R, G, B) exceeds
//     `threshold` — brightness scales the glow intensity, so the brightest
//     pixels glow hardest.
//   - BRIGHT-ANSI: keep fragments whose hue matches one of the colour
//     scheme's 8 bright ANSI variants, ignoring brightness/lightness so
//     antialiased glyphs still register even when blended toward the
//     background. Greyscale bright variants (sat < `min_palette_sat`) are
//     skipped on the host side (their slot's `.w` is set to 0).
//   - FOREGROUND: keep fragments whose RGB distance to the configured
//     foreground colour is within `fg_tolerance`. Catches default text,
//     which is usually achromatic and so invisible to the other two modes.
// Per-pixel weight is the max across all enabled modes — a pixel only has
// to satisfy one to glow.
// The blur chain and additive composite below are dual-Kawase, matching
// `blur.wgsl`.

struct BlurParams {
    // 1 / source-texture dimensions, packed to 16 bytes.
    texel_size: vec2<f32>,
    _pad: vec2<f32>,
};

struct GlowParams {
    threshold: f32,        // HSV-value (brightness) cutoff for BRIGHTNESS mode.
    intensity: f32,        // Multiplier applied at additive composite.
    softness: f32,         // Smoothstep width — shared by all three modes.
    hue_tolerance: f32,    // Degrees of hue slop for BRIGHT-ANSI mode.
    use_brightness: f32,   // 0 = off, 1 = on.
    use_bright_ansi: f32,  // 0 = off, 1 = on.
    min_palette_sat: f32,  // Pixel sat below this never matches a palette hue.
    use_foreground: f32,   // 0 = off, 1 = on (FOREGROUND mode).
    fg_color: vec4<f32>,   // .rgb = foreground RGB target; .a unused.
    bg_color: vec4<f32>,   // .rgb = window default bg; used by masked composite.
    fg_tolerance: f32,     // RGB Euclidean radius around fg_color.
    scanline_strength: f32, // 0 = no CRT scanline knockout, 1 = full.
    scanline_period: f32,   // Pattern period in framebuffer pixels.
    content_scanline_strength: f32, // 0 = no overlay; 1 = full knockout.
    // RGB multipliers applied to bright vs dark scan rows by the content
    // overlay. White / black reproduce the original binary knockout;
    // tinted values give amber, blue, or other CRT phosphor looks.
    // The scalar `content_scanline_strength` interpolates between the
    // identity (vec3(1)) and the stripe colour, so strength still
    // controls how visible the effect is. Layout: 112-byte struct
    // total, matches Rust GlowParams.
    scanline_color_bright: vec4<f32>,
    scanline_color_dark: vec4<f32>,
    // 0 = scanlines apply at full strength on drawn content (colored
    // bg cells, glyphs); 1 = fully suppressed on drawn content. Empty
    // areas (default bg + no fg) are always fully suppressed by the
    // masked overlay's `empty_lerp` independent of this knob.
    content_scanline_attenuation: f32,
    _pad1: f32,
    _pad2: f32,
    _pad3: f32,
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

// HSV value = max channel. Used by BRIGHTNESS mode: brighter pixels
// glow harder, regardless of saturation. Catches near-white text and
// fully-saturated bright colours alike — both have high `mx`.
fn hsv_value(rgb: vec3<f32>) -> f32 {
    return max(max(rgb.r, rgb.g), rgb.b);
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

// Smoothstep above `threshold`, used by the BRIGHTNESS mode against the
// pixel's HSV value. Pixels at threshold contribute 0; pixels at
// threshold + softness contribute 1. The brightness itself becomes the
// glow's "intensity" — brighter source = harder bloom, because the
// bright pass keeps `rgb * weight` and brighter pixels also have
// larger rgb.
fn brightness_weight(value: f32) -> f32 {
    let lo = glow_params.threshold;
    let hi = min(lo + max(glow_params.softness, 0.0001), 1.0);
    return smoothstep(lo, hi, value);
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

// Pixel matches the configured foreground when its RGB Euclidean distance
// to `fg_color.rgb` is within `fg_tolerance`. A `softness`-wide smoothstep
// past tolerance ramps to 0 so AA fringes don't drop off abruptly.
fn foreground_weight(rgb: vec3<f32>) -> f32 {
    let d = distance(rgb, glow_params.fg_color.rgb);
    let lo = max(glow_params.fg_tolerance, 0.0);
    let hi = lo + max(glow_params.softness, 0.0001);
    return 1.0 - smoothstep(lo, hi, d);
}

@fragment
fn fs_bright(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(src_tex, src_smp, in.uv);
    var weight: f32 = 0.0;
    if (glow_params.use_brightness > 0.5) {
        weight = max(weight, brightness_weight(hsv_value(c.rgb)));
    }
    if (glow_params.use_bright_ansi > 0.5) {
        weight = max(weight, bright_ansi_weight(c.rgb));
    }
    if (glow_params.use_foreground > 0.5) {
        weight = max(weight, foreground_weight(c.rgb));
    }
    // RGB is the source colour premultiplied by weight; alpha carries the
    // weight so the composite can use it for alpha blending instead of
    // additively brightening the surroundings.
    return vec4<f32>(c.rgb * weight, weight);
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

// CRT-style scanline knockout. Binary stripe: each `period` framebuffer
// rows = half dark, half bright. `strength = 1` knocks dark rows fully
// to zero; `strength = 0` returns 1.0 (no effect) so a single uniform
// gate disables the feature without a shader branch. Strength is taken
// as a param so different callers (glow halo vs content overlay) can
// each gate independently on their own strength uniform.
fn scanline_factor(y: f32, strength: f32) -> f32 {
    let phase = fract(y / max(glow_params.scanline_period, 1.0));
    let bright = step(0.5, phase);
    return mix(1.0, bright, strength);
}

// Composite: sample the blurred bright buffer and emit RGB+alpha both
// scaled by intensity (and by the scanline factor when CRT mode is on).
// Used with premultiplied-alpha blending upstream so the halo paints
// (replaces) the surroundings with the source colour rather than
// additively brightening them.
@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    let glow = textureSample(src_tex, src_smp, in.uv);
    let k = glow_params.intensity
        * scanline_factor(in.clip_position.y, glow_params.scanline_strength);
    return vec4<f32>(glow.rgb * k, glow.a * k);
}

// Per-row stripe colour for the content overlay. `strength = 0` returns
// vec3(1) (identity multiplier), so the multiply blend is a no-op. As
// strength grows, the bright/dark rows are interpolated toward the
// configured stripe colours. White / black for the colour pair gives
// the original binary knockout; tinted values produce coloured CRT
// phosphor looks.
fn scanline_stripe_color(y: f32, strength: f32) -> vec3<f32> {
    let phase = fract(y / max(glow_params.scanline_period, 1.0));
    let is_bright = step(0.5, phase);
    let stripe = mix(
        glow_params.scanline_color_dark.rgb,
        glow_params.scanline_color_bright.rgb,
        is_bright,
    );
    return mix(vec3<f32>(1.0), stripe, strength);
}

// Content scanline overlay. Emits the per-row stripe colour and runs
// under multiply blend (`dst.rgb *= src.rgb`), tinting / darkening
// whatever is already in the framebuffer. Alpha is left at 1 — the
// multiply blend's alpha component is configured to leave dst.a alone.
@fragment
fn fs_scanline_overlay(in: VsOut) -> @location(0) vec4<f32> {
    let s = scanline_stripe_color(in.clip_position.y, glow_params.content_scanline_strength);
    return vec4<f32>(s, 1.0);
}

// Masked content overlay. The stripe colour is lerped back toward
// vec3(1) (multiply-identity, no effect) only at pixels that are
// truly "empty" — bg matches window bg AND no fg content sits on
// top. Sampling both layers is required: the bg scene alone misses
// glyphs that landed on default-bg cells, suppressing scanlines on
// the text. Group(1) here carries bg_mask + sampler + fg_mask, a
// superset of the 2-entry group used by `fs_composite_masked`.
@group(1) @binding(2) var fg_mask_tex: texture_2d<f32>;

@fragment
fn fs_scanline_overlay_masked(in: VsOut) -> @location(0) vec4<f32> {
    var s = scanline_stripe_color(in.clip_position.y, glow_params.content_scanline_strength);
    let bg_sample = textureSample(mask_tex, mask_smp, in.uv);
    let fg_sample = textureSample(fg_mask_tex, mask_smp, in.uv);
    // bg_is_default: 1.0 where bg matches the window's primary bg
    // colour (default cell) and 0 where it diverges (colored cell).
    let bg_diff = distance(bg_sample.rgb, glow_params.bg_color.rgb);
    let bg_is_default = 1.0 - smoothstep(0.02, 0.08, bg_diff);
    // fg coverage: 1.0 on a glyph pixel, 0 on empty space. Used both
    // to detect "truly empty" (suppress overlay entirely) and to
    // partially soften the overlay on drawn content.
    let fg_present = smoothstep(0.0, 0.05, fg_sample.a);
    let fg_is_empty = 1.0 - fg_present;
    // A pixel counts as "drawn content" when either the bg cell is
    // coloured or the fg has alpha. Same attenuation knob softens
    // both — scanlines stay strong only over empty terminal area.
    let content_present = max(fg_present, 1.0 - bg_is_default);
    // Lerp toward identity (no overlay) by:
    //   - 1.0 on truly empty pixels (bg default AND no fg);
    //   - `content_scanline_attenuation` on any drawn pixel;
    //   - the larger of the two so empty wins where they overlap.
    let empty_lerp = bg_is_default * fg_is_empty;
    let content_lerp = content_present * glow_params.content_scanline_attenuation;
    let lerp = max(empty_lerp, content_lerp);
    s = mix(s, vec3<f32>(1.0), lerp);
    return vec4<f32>(s, 1.0);
}

// Masked composite. Same as `fs_composite` (premultiplied alpha), but
// suppresses the halo wherever the mask texture's RGB differs from the
// configured window background colour. The bg scene is rendered with
// an OPAQUE clear of `bg_color` and then bg quads overwrite — so a
// default-bg cell's pixels stay near `bg_color` (no suppression, halo
// paints through) while a colored cell's pixels diverge (suppression,
// halo hidden). Prevents the halo from tinting adjacent cells'
// colored backgrounds and visually shifting them.
@group(1) @binding(0) var mask_tex: texture_2d<f32>;
@group(1) @binding(1) var mask_smp: sampler;

@fragment
fn fs_composite_masked(in: VsOut) -> @location(0) vec4<f32> {
    let glow = textureSample(src_tex, src_smp, in.uv);
    let mask = textureSample(mask_tex, mask_smp, in.uv);
    // Distance from this mask pixel to the window's default bg colour.
    // 0 (default cell) = no suppression; > ~0.05 (colored cell) = halo
    // ramped down to 0. The smoothstep band is narrow because cell bg
    // colours are usually well separated from the default in RGB space.
    let bg_diff = distance(mask.rgb, glow_params.bg_color.rgb);
    let suppression = smoothstep(0.02, 0.08, bg_diff);
    let k = glow_params.intensity
        * (1.0 - suppression)
        * scanline_factor(in.clip_position.y, glow_params.scanline_strength);
    return vec4<f32>(glow.rgb * k, glow.a * k);
}
