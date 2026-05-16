//! Two-mode glow / bloom effect.
//!
//! Pipeline:
//!   1. Bright pass — reads the rendered scene, writes a half-resolution
//!      "bright" target. A fragment contributes when EITHER (or both) of:
//!        - HSV saturation exceeds [`Glow::threshold`] (saturation mode);
//!        - HSV hue is within [`Glow::hue_tolerance`] degrees of one of the
//!          colour scheme's 8 bright ANSI variants (bright-ANSI mode).
//!      Both modes are toggled independently via `match_saturation` /
//!      `match_bright_ansi`. When both are off, `enabled()` is false and the
//!      caller can skip running the chain entirely.
//!   2. Dual-Kawase down/up chain — blurs the bright target. Owns a single
//!      quarter-resolution scratch slot. Iteration count widens the kernel
//!      without adding deeper mip levels.
//!   3. Composite — additively blends the blurred bright target into the
//!      swapchain.
//!
//! `Glow` does not own the scene texture; the caller passes the scene view
//! at construction and on resize, and `Glow` rebuilds bind groups against
//! that handle. The bright-ANSI hue table is loaded from a palette via
//! [`Glow::set_bright_palette`].

use wgpu::util::DeviceExt;

const DEFAULT_ITERATIONS: usize = 2;
pub const MAX_ITERATIONS: usize = 12;

pub const DEFAULT_THRESHOLD: f32 = 0.6;
pub const DEFAULT_INTENSITY: f32 = 0.8;
pub const DEFAULT_SOFTNESS: f32 = 0.15;
pub const DEFAULT_HUE_TOLERANCE_DEG: f32 = 18.0;
/// Bright-ANSI slots whose own saturation is below this are skipped (so
/// "bright black" and "bright white" don't drag every grey pixel into the
/// glow). Also used in the shader as the per-pixel saturation floor.
pub const DEFAULT_MIN_PALETTE_SAT: f32 = 0.25;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurParams {
    texel_size: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct GlowParams {
    threshold: f32,
    intensity: f32,
    softness: f32,
    hue_tolerance: f32,
    use_saturation: f32,
    use_bright_ansi: f32,
    min_palette_sat: f32,
    _pad: f32,
}

/// One entry per bright ANSI slot. Layout matches the WGSL `BrightHues`:
/// `(hue_deg, sat, _unused, active)`. `active = 0.0` means the slot is
/// skipped — used for the achromatic bright variants (e.g. bright black /
/// bright white in most schemes).
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct BrightHues {
    entries: [[f32; 4]; 8],
}

impl BrightHues {
    /// Empty table — all slots inactive. Safe to ship to the GPU as the
    /// starting state; the bright-ANSI weight will be zero until the caller
    /// installs a palette via [`Glow::set_bright_palette`].
    fn empty() -> Self {
        Self { entries: [[0.0; 4]; 8] }
    }
}

struct Target {
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

pub struct Glow {
    format: wgpu::TextureFormat,
    sampler: wgpu::Sampler,
    fs_bgl: wgpu::BindGroupLayout,

    bright: Target,
    scratch: Target,

    glow_uniform: wgpu::Buffer,
    bright_hues_uniform: wgpu::Buffer,
    bright_blur_uniform: wgpu::Buffer, // texel_size = 1 / scene dims
    down_uniform: wgpu::Buffer,         // texel_size = 1 / bright dims
    up_uniform: wgpu::Buffer,           // texel_size = 1 / scratch dims

    bright_pipeline: wgpu::RenderPipeline,
    down_pipeline: wgpu::RenderPipeline,
    up_pipeline: wgpu::RenderPipeline,
    pub composite_pipeline: wgpu::RenderPipeline,

    // Bind groups (group 0). All share the same layout: texture + sampler +
    // BlurParams + GlowParams + BrightHues.
    bright_bg: wgpu::BindGroup,    // samples scene
    down_bg: wgpu::BindGroup,      // samples bright
    up_bg: wgpu::BindGroup,        // samples scratch
    pub composite_bg: wgpu::BindGroup, // samples bright (final blurred)

    /// Independent mode toggles. `enabled()` returns true if either is set.
    pub match_saturation: bool,
    pub match_bright_ansi: bool,

    pub threshold: f32,
    pub intensity: f32,
    pub softness: f32,
    pub hue_tolerance: f32,
    pub min_palette_sat: f32,
    pub iterations: usize,
}

impl Glow {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        scene_view: &wgpu::TextureView,
    ) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glow sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let uniform_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let fs_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glow bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                uniform_entry(2),
                uniform_entry(3),
                uniform_entry(4),
            ],
        });

        let shader = device.create_shader_module(wgpu::include_wgsl!("glow.wgsl"));

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glow pipeline layout"),
            bind_group_layouts: &[&fs_bgl],
            push_constant_ranges: &[],
        });

        let make_pipeline = |entry: &str, label: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_fullscreen",
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: entry,
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
            })
        };

        let bright_pipeline = make_pipeline("fs_bright", "glow bright pipeline", None);
        let down_pipeline = make_pipeline("fs_down", "glow down pipeline", None);
        let up_pipeline = make_pipeline("fs_up", "glow up pipeline", None);
        // Additive composite: glow is added on top of whatever is already in
        // the framebuffer. Alpha stays as the destination's alpha so we don't
        // disturb the swapchain's PostMultiplied composition.
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let composite_pipeline =
            make_pipeline("fs_composite", "glow composite pipeline", Some(additive));

        let glow_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glow params uniform"),
            contents: bytemuck::cast_slice(&[GlowParams {
                threshold: DEFAULT_THRESHOLD,
                intensity: DEFAULT_INTENSITY,
                softness: DEFAULT_SOFTNESS,
                hue_tolerance: DEFAULT_HUE_TOLERANCE_DEG,
                use_saturation: 0.0,
                use_bright_ansi: 0.0,
                min_palette_sat: DEFAULT_MIN_PALETTE_SAT,
                _pad: 0.0,
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bright_hues_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glow bright hues uniform"),
            contents: bytemuck::cast_slice(&[BrightHues::empty()]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let make_blur_uniform = |label: &str| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(&[BlurParams {
                    texel_size: [0.0, 0.0],
                    _pad: [0.0; 2],
                }]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            })
        };
        let bright_blur_uniform = make_blur_uniform("glow bright blur uniform");
        let down_uniform = make_blur_uniform("glow down blur uniform");
        let up_uniform = make_blur_uniform("glow up blur uniform");

        let (bright, scratch, bright_bg, down_bg, up_bg, composite_bg) = build_resources(
            device,
            format,
            width,
            height,
            scene_view,
            &sampler,
            &fs_bgl,
            &glow_uniform,
            &bright_hues_uniform,
            &bright_blur_uniform,
            &down_uniform,
            &up_uniform,
        );

        Self {
            format,
            sampler,
            fs_bgl,
            bright,
            scratch,
            glow_uniform,
            bright_hues_uniform,
            bright_blur_uniform,
            down_uniform,
            up_uniform,
            bright_pipeline,
            down_pipeline,
            up_pipeline,
            composite_pipeline,
            bright_bg,
            down_bg,
            up_bg,
            composite_bg,
            match_saturation: false,
            match_bright_ansi: false,
            threshold: DEFAULT_THRESHOLD,
            intensity: DEFAULT_INTENSITY,
            softness: DEFAULT_SOFTNESS,
            hue_tolerance: DEFAULT_HUE_TOLERANCE_DEG,
            min_palette_sat: DEFAULT_MIN_PALETTE_SAT,
            iterations: DEFAULT_ITERATIONS,
        }
        // Caller must invoke `write_uniforms` and `write_glow_params` once
        // the queue and final dimensions are known.
    }

    /// True when either match mode is active. Used by the renderer to skip
    /// the offscreen scene render entirely on the fast path.
    pub fn enabled(&self) -> bool {
        self.match_saturation || self.match_bright_ansi
    }

    /// Call after [`Self::new`] or any swapchain resize. Rebuilds the bright
    /// and scratch textures at the new size and rebinds the scene-sampling
    /// bright bind group against the (potentially recreated) scene view.
    pub fn resize(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        scene_view: &wgpu::TextureView,
    ) {
        let (bright, scratch, bright_bg, down_bg, up_bg, composite_bg) = build_resources(
            device,
            self.format,
            width,
            height,
            scene_view,
            &self.sampler,
            &self.fs_bgl,
            &self.glow_uniform,
            &self.bright_hues_uniform,
            &self.bright_blur_uniform,
            &self.down_uniform,
            &self.up_uniform,
        );
        self.bright = bright;
        self.scratch = scratch;
        self.bright_bg = bright_bg;
        self.down_bg = down_bg;
        self.up_bg = up_bg;
        self.composite_bg = composite_bg;
        self.write_uniforms(queue, width, height);
    }

    /// Upload the per-pass `1/texel_size` uniforms.
    pub fn write_uniforms(&self, queue: &wgpu::Queue, scene_w: u32, scene_h: u32) {
        let scene_w = scene_w.max(1) as f32;
        let scene_h = scene_h.max(1) as f32;
        queue.write_buffer(
            &self.bright_blur_uniform,
            0,
            bytemuck::cast_slice(&[BlurParams {
                texel_size: [1.0 / scene_w, 1.0 / scene_h],
                _pad: [0.0; 2],
            }]),
        );
        queue.write_buffer(
            &self.down_uniform,
            0,
            bytemuck::cast_slice(&[BlurParams {
                texel_size: [1.0 / self.bright.width as f32, 1.0 / self.bright.height as f32],
                _pad: [0.0; 2],
            }]),
        );
        queue.write_buffer(
            &self.up_uniform,
            0,
            bytemuck::cast_slice(&[BlurParams {
                texel_size: [1.0 / self.scratch.width as f32, 1.0 / self.scratch.height as f32],
                _pad: [0.0; 2],
            }]),
        );
    }

    /// Push the current mutable parameters to the GPU. Cheap; safe to call
    /// every frame if something is animating.
    pub fn write_glow_params(&self, queue: &wgpu::Queue) {
        queue.write_buffer(
            &self.glow_uniform,
            0,
            bytemuck::cast_slice(&[GlowParams {
                threshold: self.threshold.clamp(0.0, 1.0),
                intensity: self.intensity.max(0.0),
                softness: self.softness.clamp(0.0, 1.0),
                hue_tolerance: self.hue_tolerance.clamp(0.0, 180.0),
                use_saturation: if self.match_saturation { 1.0 } else { 0.0 },
                use_bright_ansi: if self.match_bright_ansi { 1.0 } else { 0.0 },
                min_palette_sat: self.min_palette_sat.clamp(0.0, 1.0),
                _pad: 0.0,
            }]),
        );
    }

    /// Install the 8 bright ANSI variants (linear RGB, alpha ignored).
    /// Each slot is converted to (hue, sat) on the host; slots whose own
    /// saturation falls below [`Self::min_palette_sat`] are marked inactive
    /// so they don't drag achromatic pixels into the glow.
    pub fn set_bright_palette(&self, queue: &wgpu::Queue, bright: &[[f32; 4]; 8]) {
        let mut table = BrightHues::empty();
        for (i, rgba) in bright.iter().enumerate() {
            let rgb = [rgba[0], rgba[1], rgba[2]];
            let sat = hsv_saturation(rgb);
            let hue = hsv_hue(rgb);
            let active = hue.is_some() && sat >= self.min_palette_sat.clamp(0.0, 1.0);
            table.entries[i] = [
                hue.unwrap_or(0.0),
                sat,
                0.0,
                if active { 1.0 } else { 0.0 },
            ];
        }
        queue.write_buffer(&self.bright_hues_uniform, 0, bytemuck::cast_slice(&[table]));
    }

    /// Run bright pass + blur. After this, the blurred glow lives in
    /// `bright` and is sampled by `composite_bg`.
    pub fn run(&self, encoder: &mut wgpu::CommandEncoder) {
        // Bright pass: scene → bright (full/2 res).
        self.fullscreen_pass(
            encoder,
            &self.bright_pipeline,
            &self.bright_bg,
            &self.bright.view,
            "glow bright",
        );
        // One or more down/up iterations widen the kernel at the bright
        // resolution without adding extra mip levels.
        for _ in 0..self.iterations.max(1) {
            self.fullscreen_pass(
                encoder,
                &self.down_pipeline,
                &self.down_bg,
                &self.scratch.view,
                "glow down",
            );
            self.fullscreen_pass(
                encoder,
                &self.up_pipeline,
                &self.up_bg,
                &self.bright.view,
                "glow up",
            );
        }
    }

    fn fullscreen_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        pipeline: &wgpu::RenderPipeline,
        bind_group: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        label: &str,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_resources(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    scene_view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
    bgl: &wgpu::BindGroupLayout,
    glow_uniform: &wgpu::Buffer,
    bright_hues_uniform: &wgpu::Buffer,
    bright_blur_uniform: &wgpu::Buffer,
    down_uniform: &wgpu::Buffer,
    up_uniform: &wgpu::Buffer,
) -> (
    Target,
    Target,
    wgpu::BindGroup,
    wgpu::BindGroup,
    wgpu::BindGroup,
    wgpu::BindGroup,
) {
    let bright_w = (width / 2).max(1);
    let bright_h = (height / 2).max(1);
    let scratch_w = (bright_w / 2).max(1);
    let scratch_h = (bright_h / 2).max(1);

    let make_tex = |w: u32, h: u32, label: &str| {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        Target { view, width: w, height: h }
    };

    let bright = make_tex(bright_w, bright_h, "glow bright");
    let scratch = make_tex(scratch_w, scratch_h, "glow scratch");

    let make_bg = |src: &wgpu::TextureView, blur_u: &wgpu::Buffer, label: &str| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(src),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: blur_u.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: glow_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: bright_hues_uniform.as_entire_binding(),
                },
            ],
        })
    };

    let bright_bg = make_bg(scene_view, bright_blur_uniform, "glow bright bg");
    let down_bg = make_bg(&bright.view, down_uniform, "glow down bg");
    let up_bg = make_bg(&scratch.view, up_uniform, "glow up bg");
    let composite_bg = make_bg(&bright.view, bright_blur_uniform, "glow composite bg");

    (bright, scratch, bright_bg, down_bg, up_bg, composite_bg)
}

/// CPU mirror of the shader's `hsv_saturation`. Kept in lockstep with
/// `glow.wgsl` so the bright-pass behaviour is testable without a GPU.
fn hsv_saturation(rgb: [f32; 3]) -> f32 {
    let mx = rgb[0].max(rgb[1]).max(rgb[2]);
    let mn = rgb[0].min(rgb[1]).min(rgb[2]);
    if mx <= 0.0 {
        return 0.0;
    }
    (mx - mn) / mx
}

/// CPU mirror of the shader's bright-pass saturation `smoothstep` weight.
/// The `0.0001` floor on `softness` matches the shader and keeps the result
/// finite when the caller asks for a hard cutoff.
fn bright_weight(sat: f32, threshold: f32, softness: f32) -> f32 {
    let lo = threshold;
    let hi = (lo + softness.max(0.0001)).min(1.0);
    let t = ((sat - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// CPU mirror of the shader's `hsv_hue`. Returns `Some(degrees)` in
/// `[0, 360)` for chromatic colours and `None` when max == min (grey).
fn hsv_hue(rgb: [f32; 3]) -> Option<f32> {
    let mx = rgb[0].max(rgb[1]).max(rgb[2]);
    let mn = rgb[0].min(rgb[1]).min(rgb[2]);
    let d = mx - mn;
    if d <= 0.0 {
        return None;
    }
    let h = if mx == rgb[0] {
        (rgb[1] - rgb[2]) / d
    } else if mx == rgb[1] {
        2.0 + (rgb[2] - rgb[0]) / d
    } else {
        4.0 + (rgb[0] - rgb[1]) / d
    };
    let mut h = h * 60.0;
    if h < 0.0 {
        h += 360.0;
    }
    Some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-5, "expected {b}, got {a}");
    }

    fn approx_loose(a: f32, b: f32, eps: f32) {
        assert!((a - b).abs() < eps, "expected {b} ± {eps}, got {a}");
    }

    #[test]
    fn hsv_saturation_pure_grey_is_zero() {
        approx(hsv_saturation([0.5, 0.5, 0.5]), 0.0);
    }

    #[test]
    fn hsv_saturation_black_is_zero_no_div_by_zero() {
        let s = hsv_saturation([0.0, 0.0, 0.0]);
        assert!(s.is_finite());
        approx(s, 0.0);
    }

    #[test]
    fn hsv_saturation_pure_red_is_one() {
        approx(hsv_saturation([1.0, 0.0, 0.0]), 1.0);
    }

    #[test]
    fn hsv_saturation_mid_colour() {
        // (0.8 - 0.2) / 0.8 = 0.75
        approx(hsv_saturation([0.8, 0.2, 0.2]), 0.75);
    }

    #[test]
    fn bright_weight_below_threshold_is_zero() {
        approx(bright_weight(0.1, 0.5, 0.2), 0.0);
    }

    #[test]
    fn bright_weight_above_band_is_one() {
        approx(bright_weight(0.9, 0.5, 0.2), 1.0);
    }

    #[test]
    fn bright_weight_at_threshold_is_zero() {
        approx(bright_weight(0.5, 0.5, 0.2), 0.0);
    }

    #[test]
    fn bright_weight_zero_softness_is_finite() {
        let w = bright_weight(0.6, 0.5, 0.0);
        assert!(w.is_finite());
        approx(w, 1.0);
        let w_below = bright_weight(0.4, 0.5, 0.0);
        assert!(w_below.is_finite());
        approx(w_below, 0.0);
    }

    #[test]
    fn defaults_are_finite_and_in_range() {
        assert!(DEFAULT_THRESHOLD.is_finite());
        assert!((0.0..=1.0).contains(&DEFAULT_THRESHOLD));
        assert!(DEFAULT_INTENSITY.is_finite() && DEFAULT_INTENSITY >= 0.0);
        assert!(DEFAULT_SOFTNESS.is_finite());
        assert!((0.0..=1.0).contains(&DEFAULT_SOFTNESS));
        assert!((0.0..=180.0).contains(&DEFAULT_HUE_TOLERANCE_DEG));
        assert!((0.0..=1.0).contains(&DEFAULT_MIN_PALETTE_SAT));
        assert!(MAX_ITERATIONS >= 1);
    }

    #[test]
    fn hsv_hue_grey_is_none() {
        assert!(hsv_hue([0.0, 0.0, 0.0]).is_none());
        assert!(hsv_hue([0.5, 0.5, 0.5]).is_none());
        assert!(hsv_hue([1.0, 1.0, 1.0]).is_none());
    }

    #[test]
    fn hsv_hue_primaries() {
        approx(hsv_hue([1.0, 0.0, 0.0]).unwrap(), 0.0);
        approx(hsv_hue([0.0, 1.0, 0.0]).unwrap(), 120.0);
        approx(hsv_hue([0.0, 0.0, 1.0]).unwrap(), 240.0);
    }

    #[test]
    fn hsv_hue_secondaries() {
        approx(hsv_hue([1.0, 1.0, 0.0]).unwrap(), 60.0);
        approx(hsv_hue([0.0, 1.0, 1.0]).unwrap(), 180.0);
        approx(hsv_hue([1.0, 0.0, 1.0]).unwrap(), 300.0);
    }

    #[test]
    fn hsv_hue_invariant_under_aa_toward_white() {
        // A glyph at bright-red [1, 0.33, 0.33] antialiased toward white at
        // various coverages keeps its red hue — that's the whole point of
        // matching on hue: the AA fringe still registers.
        let bright_red = [1.0, 0.33, 0.33];
        let bg_white = 1.0;
        let base_hue = hsv_hue(bright_red).unwrap();
        for alpha in [1.0_f32, 0.9, 0.8, 0.7, 0.6, 0.5, 0.3] {
            let blended = [
                bright_red[0] * alpha + bg_white * (1.0 - alpha),
                bright_red[1] * alpha + bg_white * (1.0 - alpha),
                bright_red[2] * alpha + bg_white * (1.0 - alpha),
            ];
            let h = hsv_hue(blended)
                .unwrap_or_else(|| panic!("expected hue at alpha {alpha}, got grey"));
            approx_loose(h, base_hue, 0.5);
        }
    }
}
