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
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) local_pos: vec2<f32>,
    @location(3) half_size: vec2<f32>,
    @location(4) radii: vec4<f32>,
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
    return out;
}

//
// Fragment shader
//
@group(0) @binding(0)
var t_diffuse: texture_2d<f32>;
@group(0) @binding(1)
var s_diffuse: sampler;

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

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let glyph = textureSample(t_diffuse, s_diffuse, in.tex_coords).r;
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
    return in.color * glyph * mask;
}
