//
// Vertex shader
//

struct CameraUniform {
    view_projection: mat4x4<f32>,
};
@group(1) @binding(0)
var<uniform> camera: CameraUniform;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) tex_coords: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) local_pos: vec2<f32>,
    @location(4) half_size: vec2<f32>,
    @location(5) radii: vec4<f32>,
    @location(6) kind: f32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) local_pos: vec2<f32>,
    @location(3) half_size: vec2<f32>,
    @location(4) radii: vec4<f32>,
    @location(5) kind: f32,
};

@vertex
fn vs_main(model: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.tex_coords = model.tex_coords;
    out.clip_position = camera.view_projection * vec4<f32>(model.position, 1.0);
    out.color = model.color;
    out.local_pos = model.local_pos;
    out.half_size = model.half_size;
    out.radii = model.radii;
    out.kind = model.kind;
    return out;
}

//
// Fragment shader
//
@group(0) @binding(0)
var t_diffuse: texture_2d<f32>;
@group(0) @binding(1)
var s_diffuse: sampler;
// RGBA color-glyph atlas (emoji). Sampled only by quads tagged `kind > 0.5`.
// Stored BGRA (FreeType's pixel order) in an sRGB texture, so the sample comes
// back already sRGB-decoded; the fragment swizzles B/R into place.
@group(0) @binding(2)
var t_color: texture_2d<f32>;

// Edge-fade params: top.xy = (band_height, alpha), bottom.xy = (band_height,
// alpha), viewport.xy = (width, height), bg_uv.xy = the atlas UV for the
// solid bg sentinel slot. Only fragments that don't sample the bg slot get
// faded — so cell backgrounds stay solid and only the glyphs ramp away.
// params.x = the glyph-coverage gamma exponent (1.0 / text_gamma); params.yzw
// are reserved (0). At the default text_gamma = 1.0 this is 1.0, an identity
// pow that reproduces the pre-gamma output exactly.
struct FadeUniform {
    top: vec4<f32>,
    bottom: vec4<f32>,
    viewport: vec4<f32>,
    bg_uv: vec4<f32>,
    params: vec4<f32>,
};
@group(2) @binding(0)
var<uniform> fade: FadeUniform;

// Signed distance to a per-corner rounded box centered at the origin.
// `b` is the half-extent; `r` packs (top-right, bottom-right, top-left,
// bottom-left) radii. Returns < 0 inside, > 0 outside.
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: vec4<f32>) -> f32 {
    var rad: f32;
    if (p.x > 0.0) {
        if (p.y > 0.0) { rad = r.y; } else { rad = r.x; }
    } else {
        if (p.y > 0.0) { rad = r.w; } else { rad = r.z; }
    }
    let q = abs(p) - b + rad;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - rad;
}

// Wireframe pipeline: draw triangle edges in the per-quad vertex color
// (fg for glyph quads, bg for background quads, selection/cursor/fade
// colors for those overlays). Skips the SDF mask + glyph alpha that
// fs_main relies on. Quads with alpha=0 (e.g. default-bg cells) end up
// invisible — which is fine, the fg glyph quad still outlines the cell.
@fragment
fn fs_wire(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Color (emoji) glyph: sample the RGBA color atlas and use the texel
    // directly (premultiplied alpha). BGRA storage → swizzle to RGB. The mono
    // coverage/SDF/gamma machinery below is bypassed; only the y-position fade
    // is shared so emoji ramp away at the scrollback edges like text.
    if (in.kind > 0.5) {
        let texel = textureSample(t_color, s_diffuse, in.tex_coords);
        let emoji = vec4<f32>(texel.b, texel.g, texel.r, texel.a);
        let yy = in.clip_position.y;
        let tf = select(0.0, fade.top.y * (1.0 - smoothstep(0.0, fade.top.x, yy)), fade.top.x > 0.0);
        let bf = select(0.0, fade.bottom.y * (1.0 - smoothstep(0.0, fade.bottom.x, fade.viewport.y - yy)), fade.bottom.x > 0.0);
        let fs = clamp(max(tf, bf), 0.0, 1.0);
        return emoji * (1.0 - fs);
    }
    let glyph = textureSample(t_diffuse, s_diffuse, in.tex_coords).r;
    // Apply the glyph-coverage gamma. Solid quads (bg cells, cursor,
    // selection, fade strips) sample the fully-opaque sentinel slot, so
    // `glyph == 1.0` there and `pow(1.0, x) == 1.0` for any exponent —
    // gamma only reshapes real anti-aliased glyph edges, never the solid
    // overlays. At the default exponent 1.0 this is the identity.
    let cov = pow(glyph, fade.params.x);
    let max_r = max(max(in.radii.x, in.radii.y), max(in.radii.z, in.radii.w));
    let min_r = min(min(in.radii.x, in.radii.y), min(in.radii.z, in.radii.w));
    var mask: f32 = 1.0;
    if (min_r < 0.0) {
        // Concave fillet: the quad is filled EXCEPT for a quarter-circle bite
        // cut out at one corner. The negative-radius slot identifies which
        // corner holds the circle's center; |radius| sets the bite size.
        // Used at L-step inner corners of multi-row selections so the inner
        // angle reads as a smooth curve rather than a sharp 90°.
        let r = -min_r;
        var center: vec2<f32>;
        if (in.radii.x < 0.0) {
            center = vec2<f32>(in.half_size.x, -in.half_size.y);  // TR
        } else if (in.radii.y < 0.0) {
            center = vec2<f32>(in.half_size.x, in.half_size.y);   // BR
        } else if (in.radii.z < 0.0) {
            center = vec2<f32>(-in.half_size.x, -in.half_size.y); // TL
        } else {
            center = vec2<f32>(-in.half_size.x, in.half_size.y);  // BL
        }
        let dist = length(in.local_pos - center);
        // Selected for `dist >= r`; full alpha at the curve and outside it,
        // 1-pixel AA fade as `dist` shrinks back into the bite. Boundary
        // mask = 1 keeps the abutment with adjacent strips clean.
        mask = smoothstep(r - 1.0, r, dist);
    } else if (max_r > 0.0) {
        // Rounded corner — 1px AA biased *outward* from the geometric edge.
        // smoothstep(0, 1, sdf) gives full alpha at the edge (sdf = 0) and
        // fades to 0 over the next pixel outside the shape. Centered AA
        // (smoothstep(-0.5, 0.5, sdf)) would give 0.5 at fragments whose
        // centers land on the edge, which reads as a 1-pixel gap when two
        // selection strips abut.
        let sdf = sd_rounded_box(in.local_pos, in.half_size, in.radii);
        mask = 1.0 - smoothstep(0.0, 1.0, sdf);
    }
    let base = in.color * cov * mask;

    // Y-position fade. clip_position.y is in framebuffer pixels with origin
    // at the top, so y=0 is the window top. select() guards against div-by-0
    // when the band collapses to nothing (no fade active).
    let y = in.clip_position.y;
    let top_h = fade.top.x;
    let top_a = fade.top.y;
    let bot_h = fade.bottom.x;
    let bot_a = fade.bottom.y;
    let vh = fade.viewport.y;
    let top_fade = select(0.0, top_a * (1.0 - smoothstep(0.0, top_h, y)), top_h > 0.0);
    let bot_fade = select(0.0, bot_a * (1.0 - smoothstep(0.0, bot_h, vh - y)), bot_h > 0.0);
    let fade_strength = clamp(max(top_fade, bot_fade), 0.0, 1.0);
    // Bg quads fade partially (up to 0.75) so dark cell backgrounds lighten
    // toward the clear color near the toolbar — keeps chrome contrast
    // readable. Glyphs still fade all the way to invisible.
    let is_bg_quad = length(in.tex_coords - fade.bg_uv.xy) < 0.0005;
    let applied_fade = select(fade_strength, fade_strength * 0.75, is_bg_quad);
    return base * (1.0 - applied_fade);
}
