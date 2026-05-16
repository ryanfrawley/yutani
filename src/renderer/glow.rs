//! Three-mode glow / bloom effect.
//!
//! Pipeline:
//!   1. Bright pass — reads the rendered scene, writes a half-resolution
//!      "bright" target. A fragment contributes when ANY of:
//!        - HSV saturation exceeds [`Glow::threshold`] (saturation mode);
//!        - HSV hue is within [`Glow::hue_tolerance`] degrees of one of the
//!          colour scheme's 8 bright ANSI variants (bright-ANSI mode);
//!        - RGB distance to the palette's primary foreground colour is within
//!          [`Glow::fg_tolerance`] (foreground mode — catches default text
//!          even when it's achromatic, which the other two modes ignore).
//!      All three modes are toggled independently. When all are off,
//!      `enabled()` is false and the caller can skip running the chain
//!      entirely.
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
/// RGB Euclidean radius around the foreground colour. 0.12 is roughly a
/// `~30/255` per-channel slop — enough to catch antialiased glyph edges that
/// have blended a little toward the background, without bleeding into
/// neighbouring palette entries.
pub const DEFAULT_FG_TOLERANCE: f32 = 0.12;
pub const DEFAULT_SCANLINE_STRENGTH: f32 = 1.0;
/// Period in framebuffer pixels — 4 = 2 dark + 2 bright. On Retina this
/// reads as a 1-logical-pixel-on-1-logical-pixel-off pattern.
pub const DEFAULT_SCANLINE_PERIOD: f32 = 4.0;
/// White: applied multiplicatively to bright scan rows ⇒ no change to
/// the destination, reproducing the original scalar 0/1 behaviour.
pub const DEFAULT_SCANLINE_COLOR_BRIGHT: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// Black: applied multiplicatively to dark scan rows ⇒ full knockout
/// (destination goes to 0), matching the original behaviour.
pub const DEFAULT_SCANLINE_COLOR_DARK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
/// Halve the scanline effect on drawn content (glyphs and colored bg
/// cells) by default. The CRT pattern over content is naturally more
/// noticeable than over flat empty bg, so a gentle pull-back keeps
/// content readable.
pub const DEFAULT_CONTENT_SCANLINE_ATTENUATION: f32 = 0.5;

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
    use_brightness: f32,
    use_bright_ansi: f32,
    min_palette_sat: f32,
    use_foreground: f32,
    // fg_color.rgb is the foreground RGB; .a is unused. Held as vec4 so it
    // takes a 16-byte slot in WGSL's std140 layout without surprises.
    fg_color: [f32; 4],
    /// Default window background — used by the masked composite to detect
    /// whether a mask-texture pixel belongs to a colored cell (rgb differs
    /// from bg_color) versus the default empty area (rgb matches bg_color).
    /// Stored as vec4 for std140 alignment; .a is unused.
    bg_color: [f32; 4],
    fg_tolerance: f32,
    /// 0 = no CRT-scanline knockout; 1 = full knockout (alternate rows of
    /// the glow are alpha'd to zero). Sent as 0 when `match_scanlines` is
    /// off so the shader doesn't have to branch.
    scanline_strength: f32,
    /// Period of the scanline pattern in framebuffer pixels. Each cycle
    /// is half dark, half bright — so period 4 = 2px dark + 2px bright.
    scanline_period: f32,
    /// 0 = no scanlines over rendered content; 1 = full knockout. Sent
    /// as 0 when `match_content_scanlines` is off. Independent of the
    /// glow-halo strength above so each effect can toggle separately.
    content_scanline_strength: f32,
    /// Multiplier applied to the framebuffer on bright scan rows of the
    /// content overlay. White = no change (matches the original scalar
    /// behaviour); a tinted value cools/warms the bright stripes.
    /// .a is unused; vec4 layout for std140 alignment.
    scanline_color_bright: [f32; 4],
    /// Multiplier applied on dark scan rows. Black = full knockout
    /// (original behaviour); a coloured value lets dark stripes glow
    /// dimly instead of going pitch black.
    scanline_color_dark: [f32; 4],
    /// 0..1 amount the masked content overlay is *softened* over any
    /// drawn pixel — colored cell bg or fg glyph. 0 = scanlines apply
    /// at full strength on all drawn content; 1 = scanlines fully
    /// suppressed on drawn content (only fire on bg-less empty area).
    /// Read only by the masked overlay; the unmasked variant has no
    /// per-pixel knowledge of what's drawn.
    content_scanline_attenuation: f32,
    // Pad to 128-byte total (multiple of 16) to match the WGSL struct.
    _pad: [f32; 3],
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
    /// Variant of `composite_pipeline` that also samples a mask texture
    /// (group 1) and multiplies the halo by `(1 - mask.a)`. Used by the
    /// layered render path with the bg scene as the mask, so a glyph's
    /// halo doesn't paint over adjacent cells' colored backgrounds and
    /// visually shift their apparent color.
    pub composite_masked_pipeline: wgpu::RenderPipeline,
    /// Multiply-blend pass that emits the scanline factor as rgb and
    /// modulates whatever is already in the framebuffer. Used to draw
    /// scanlines over all rendered content (bg cells + glyphs), not
    /// just the glow halo. Reuses `composite_bg` for its uniform
    /// access; the bound texture is unused by the shader.
    pub scanline_overlay_pipeline: wgpu::RenderPipeline,
    /// Variant of `scanline_overlay_pipeline` that also samples a mask
    /// texture (group 1) and lerps the stripe colour to identity
    /// wherever the mask matches the window bg colour. Used to hide
    /// scanlines over the default window background. Caller is
    /// responsible for binding a sensible mask (typically the bg scene)
    /// at group(1).
    pub scanline_overlay_masked_pipeline: wgpu::RenderPipeline,
    mask_bgl: wgpu::BindGroupLayout,
    /// Superset of `mask_bgl` used by `scanline_overlay_masked_pipeline`:
    /// bg texture + sampler + fg texture. The overlay needs both layers
    /// to correctly identify "truly empty" pixels (bg = default AND fg
    /// = transparent) so it doesn't accidentally suppress scanlines
    /// over glyphs drawn on default-bg cells.
    overlay_mask_bgl: wgpu::BindGroupLayout,
    mask_sampler: wgpu::Sampler,

    // Bind groups (group 0). All share the same layout: texture + sampler +
    // BlurParams + GlowParams + BrightHues.
    bright_bg: wgpu::BindGroup,    // samples scene
    down_bg: wgpu::BindGroup,      // samples bright
    up_bg: wgpu::BindGroup,        // samples scratch
    pub composite_bg: wgpu::BindGroup, // samples bright (final blurred)

    /// Independent mode toggles. `enabled()` returns true if any is set.
    /// When true, pixels whose HSV value (max channel) exceeds
    /// [`Self::threshold`] contribute to the glow. Brightness scales
    /// the bloom: brighter source ⇒ harder halo.
    pub match_brightness: bool,
    pub match_bright_ansi: bool,
    pub match_foreground: bool,

    pub threshold: f32,
    pub intensity: f32,
    pub softness: f32,
    pub hue_tolerance: f32,
    pub min_palette_sat: f32,
    /// Foreground colour the bright pass compares against. Set via
    /// [`Glow::set_foreground`]; alpha is ignored.
    pub fg_color: [f32; 4],
    /// Window background colour the masked composite uses to detect
    /// colored cells in the mask texture. Set via [`Glow::set_background`].
    pub bg_color: [f32; 4],
    pub fg_tolerance: f32,
    /// When true, the composite applies a CRT-style scanline knockout to
    /// the halo: alternating rows are alpha'd toward zero.
    pub match_scanlines: bool,
    pub scanline_strength: f32,
    pub scanline_period: f32,
    /// When true, the dedicated `scanline_overlay_pipeline` (multiply
    /// blend) darkens alternating rows of whatever is already in the
    /// framebuffer — affecting bg colors and glyphs alike. Independent
    /// of `match_scanlines` so each layer can be toggled.
    pub match_content_scanlines: bool,
    pub content_scanline_strength: f32,
    /// Colour pair used by the content overlay's bright / dark stripes.
    /// Default (white / black) reproduces the original binary knockout;
    /// other values let stripes be tinted (e.g. amber/blue CRT phosphor
    /// or a non-pitch-black "dark" stripe).
    pub scanline_color_bright: [f32; 4],
    pub scanline_color_dark: [f32; 4],
    /// Softens the masked overlay over any drawn pixel (glyph or
    /// colored bg cell). See [`GlowParams::content_scanline_attenuation`].
    pub content_scanline_attenuation: f32,
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

        // Mask bind group layout: a single texture + sampler. Lives at
        // @group(1) in `fs_composite_masked` so the composite shader can
        // suppress halo where the mask is opaque.
        let mask_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glow mask bgl"),
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
            ],
        });
        let mask_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glow mask sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let masked_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glow masked composite layout"),
            bind_group_layouts: &[&fs_bgl, &mask_bgl],
            push_constant_ranges: &[],
        });

        // Overlay mask: superset of mask_bgl with an extra fg texture
        // at binding 2. Used by the scanline overlay's masked variant
        // — it can't share mask_bgl because composite_masked doesn't
        // (and shouldn't) bind the fg texture.
        let overlay_mask_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glow overlay mask bgl"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
            ],
        });
        let overlay_masked_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glow overlay masked layout"),
            bind_group_layouts: &[&fs_bgl, &overlay_mask_bgl],
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
        // Premultiplied-alpha composite: the halo replaces the destination
        // with the source colour proportional to the bright-pass weight,
        // rather than additively brightening it. The masked variant uses
        // the same blend but multiplies output by `(1 - suppression)` so
        // colored cells aren't tinted by the halo.
        let alpha_blend = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;
        let composite_pipeline =
            make_pipeline("fs_composite", "glow composite pipeline", Some(alpha_blend));
        // Multiply blend: result.rgb = src.rgb * dst.rgb. The overlay
        // shader emits rgb = scanline_factor (0..1), darkening every
        // pixel in dark scan rows and leaving bright rows untouched.
        // Alpha stays at the destination's value (factor Zero,One).
        let multiply_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Dst,
                dst_factor: wgpu::BlendFactor::Zero,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let scanline_overlay_pipeline = make_pipeline(
            "fs_scanline_overlay",
            "glow scanline overlay pipeline",
            Some(multiply_blend),
        );
        // Masked overlay uses the same multiply blend but needs both
        // the bg and fg textures in its group(1), so it uses the
        // dedicated overlay_masked_pipeline_layout (3-entry BGL).
        let scanline_overlay_masked_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("glow scanline overlay-masked pipeline"),
                layout: Some(&overlay_masked_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_fullscreen",
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs_scanline_overlay_masked",
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(multiply_blend),
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
            });
        // The masked composite uses a different pipeline layout (extra
        // mask bind group at @group(1)) so it can't share `make_pipeline`.
        let composite_masked_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("glow composite-masked pipeline"),
                layout: Some(&masked_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_fullscreen",
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs_composite_masked",
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(alpha_blend),
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
            });

        let glow_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glow params uniform"),
            contents: bytemuck::cast_slice(&[GlowParams {
                threshold: DEFAULT_THRESHOLD,
                intensity: DEFAULT_INTENSITY,
                softness: DEFAULT_SOFTNESS,
                hue_tolerance: DEFAULT_HUE_TOLERANCE_DEG,
                use_brightness: 0.0,
                use_bright_ansi: 0.0,
                min_palette_sat: DEFAULT_MIN_PALETTE_SAT,
                use_foreground: 0.0,
                fg_color: [0.0, 0.0, 0.0, 1.0],
                bg_color: [1.0, 1.0, 1.0, 1.0],
                fg_tolerance: DEFAULT_FG_TOLERANCE,
                scanline_strength: 0.0,
                scanline_period: DEFAULT_SCANLINE_PERIOD,
                content_scanline_strength: 0.0,
                scanline_color_bright: DEFAULT_SCANLINE_COLOR_BRIGHT,
                scanline_color_dark: DEFAULT_SCANLINE_COLOR_DARK,
                content_scanline_attenuation: DEFAULT_CONTENT_SCANLINE_ATTENUATION,
                _pad: [0.0; 3],
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
            composite_masked_pipeline,
            scanline_overlay_pipeline,
            scanline_overlay_masked_pipeline,
            mask_bgl,
            overlay_mask_bgl,
            mask_sampler,
            bright_bg,
            down_bg,
            up_bg,
            composite_bg,
            match_brightness: false,
            match_bright_ansi: false,
            match_foreground: false,
            threshold: DEFAULT_THRESHOLD,
            intensity: DEFAULT_INTENSITY,
            softness: DEFAULT_SOFTNESS,
            hue_tolerance: DEFAULT_HUE_TOLERANCE_DEG,
            min_palette_sat: DEFAULT_MIN_PALETTE_SAT,
            fg_color: [0.0, 0.0, 0.0, 1.0],
            bg_color: [1.0, 1.0, 1.0, 1.0],
            fg_tolerance: DEFAULT_FG_TOLERANCE,
            match_scanlines: false,
            scanline_strength: DEFAULT_SCANLINE_STRENGTH,
            scanline_period: DEFAULT_SCANLINE_PERIOD,
            match_content_scanlines: false,
            content_scanline_strength: DEFAULT_SCANLINE_STRENGTH,
            scanline_color_bright: DEFAULT_SCANLINE_COLOR_BRIGHT,
            scanline_color_dark: DEFAULT_SCANLINE_COLOR_DARK,
            content_scanline_attenuation: DEFAULT_CONTENT_SCANLINE_ATTENUATION,
            iterations: DEFAULT_ITERATIONS,
        }
        // Caller must invoke `write_uniforms` and `write_glow_params` once
        // the queue and final dimensions are known.
    }

    /// True when any match mode is active. Used by the renderer to skip
    /// the offscreen scene render entirely on the fast path.
    pub fn enabled(&self) -> bool {
        self.match_brightness || self.match_bright_ansi || self.match_foreground
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
                use_brightness: if self.match_brightness { 1.0 } else { 0.0 },
                use_bright_ansi: if self.match_bright_ansi { 1.0 } else { 0.0 },
                min_palette_sat: self.min_palette_sat.clamp(0.0, 1.0),
                use_foreground: if self.match_foreground { 1.0 } else { 0.0 },
                fg_color: self.fg_color,
                bg_color: self.bg_color,
                // Max distance in RGB unit cube is √3; clamp keeps values
                // sane if the user supplies something absurd.
                fg_tolerance: self.fg_tolerance.clamp(0.0, 3.0_f32.sqrt()),
                // Zero out strength when the toggle's off so the shader
                // can branch on a single uniform.
                scanline_strength: if self.match_scanlines {
                    self.scanline_strength.clamp(0.0, 1.0)
                } else {
                    0.0
                },
                // Period < 1 would alias to single-row noise; clamp up.
                scanline_period: self.scanline_period.max(1.0),
                content_scanline_strength: if self.match_content_scanlines {
                    self.content_scanline_strength.clamp(0.0, 1.0)
                } else {
                    0.0
                },
                scanline_color_bright: self.scanline_color_bright,
                scanline_color_dark: self.scanline_color_dark,
                content_scanline_attenuation: self.content_scanline_attenuation.clamp(0.0, 1.0),
                _pad: [0.0; 3],
            }]),
        );
    }

    /// Install the foreground colour the bright pass compares against.
    /// Alpha is preserved in the uniform but unused by the shader.
    pub fn set_foreground(&mut self, fg: [f32; 4]) {
        self.fg_color = fg;
    }

    /// Install the window background colour the masked composite uses to
    /// detect colored cells. Alpha is preserved in the uniform but unused.
    pub fn set_background(&mut self, bg: [f32; 4]) {
        self.bg_color = bg;
    }

    /// Bind group for `composite_masked_pipeline`'s group(1). Caller
    /// supplies the mask texture view (typically the bg scene, so
    /// halo gets suppressed wherever the bg has a colored cell). Must
    /// be rebuilt whenever the mask view is invalidated, e.g. by a
    /// swapchain resize that recreates the bg scene.
    pub fn make_mask_bind_group(
        &self,
        device: &wgpu::Device,
        mask_view: &wgpu::TextureView,
        label: &str,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.mask_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.mask_sampler),
                },
            ],
        })
    }

    /// Bind group for `scanline_overlay_masked_pipeline`'s group(1).
    /// Caller supplies the bg-layer view (compared against bg_color to
    /// detect default cells) and the fg-layer view (its alpha tells us
    /// whether anything is drawn on top). Both views should be the
    /// scene textures used by the layered render path.
    pub fn make_overlay_mask_bind_group(
        &self,
        device: &wgpu::Device,
        bg_view: &wgpu::TextureView,
        fg_view: &wgpu::TextureView,
        label: &str,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.overlay_mask_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(bg_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.mask_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(fg_view),
                },
            ],
        })
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

/// CPU mirror of the shader's `hsv_value` — max channel, used as the
/// brightness signal by the BRIGHTNESS match mode.
fn hsv_value(rgb: [f32; 3]) -> f32 {
    rgb[0].max(rgb[1]).max(rgb[2])
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

/// CPU mirror of the shader's `foreground_weight`. Returns a 0..=1 weight
/// from RGB Euclidean distance, ramped down across a `softness`-wide band
/// past `tolerance`.
fn foreground_weight(rgb: [f32; 3], fg: [f32; 3], tolerance: f32, softness: f32) -> f32 {
    let dx = rgb[0] - fg[0];
    let dy = rgb[1] - fg[1];
    let dz = rgb[2] - fg[2];
    let d = (dx * dx + dy * dy + dz * dz).sqrt();
    let lo = tolerance.max(0.0);
    let hi = lo + softness.max(0.0001);
    let t = ((d - lo) / (hi - lo)).clamp(0.0, 1.0);
    1.0 - t * t * (3.0 - 2.0 * t)
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
    fn hsv_value_is_max_channel() {
        approx(hsv_value([0.0, 0.0, 0.0]), 0.0);
        approx(hsv_value([0.2, 0.7, 0.3]), 0.7);
        approx(hsv_value([1.0, 1.0, 1.0]), 1.0);
        // Pure red, pure blue, etc. all hit 1.0 — that's the point of
        // BRIGHTNESS mode treating saturated colours and white alike.
        approx(hsv_value([1.0, 0.0, 0.0]), 1.0);
        approx(hsv_value([0.0, 0.0, 1.0]), 1.0);
    }

    #[test]
    fn brightness_drives_glow_through_bright_weight() {
        // Sanity: BRIGHTNESS mode feeds hsv_value into bright_weight.
        // Pixel at 0.4 (below threshold) ⇒ no glow.
        let dim = hsv_value([0.4, 0.4, 0.4]);
        approx(bright_weight(dim, 0.6, 0.15), 0.0);
        // Pixel at 0.9 (well above threshold + softness) ⇒ full glow.
        let bright = hsv_value([0.9, 0.2, 0.1]);
        approx(bright_weight(bright, 0.6, 0.15), 1.0);
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
        assert!(DEFAULT_FG_TOLERANCE.is_finite() && DEFAULT_FG_TOLERANCE >= 0.0);
        assert!(MAX_ITERATIONS >= 1);
    }

    #[test]
    fn foreground_weight_exact_match_is_one() {
        let fg = [0.18, 0.15, 0.10];
        approx(foreground_weight(fg, fg, 0.12, 0.15), 1.0);
    }

    #[test]
    fn foreground_weight_far_pixel_is_zero() {
        // Pure white vs near-black foreground — way past tolerance + softness.
        let fg = [0.0, 0.0, 0.0];
        approx(foreground_weight([1.0, 1.0, 1.0], fg, 0.12, 0.15), 0.0);
    }

    #[test]
    fn foreground_weight_inside_tolerance_is_one() {
        let fg = [0.5, 0.5, 0.5];
        // Distance √(0.05² × 3) ≈ 0.0866, well under tolerance = 0.12.
        let near = [0.55, 0.45, 0.55];
        approx(foreground_weight(near, fg, 0.12, 0.15), 1.0);
    }

    #[test]
    fn foreground_weight_outside_band_is_zero() {
        let fg = [0.0, 0.0, 0.0];
        // Distance √(0.3² × 3) ≈ 0.52, past tolerance 0.12 + softness 0.15.
        let far = [0.3, 0.3, 0.3];
        approx(foreground_weight(far, fg, 0.12, 0.15), 0.0);
    }

    #[test]
    fn foreground_weight_monotonic_across_band() {
        // Walking outward from the foreground colour, the weight must
        // never increase — once we leave the tolerance band, it should
        // drop monotonically to zero.
        let fg = [0.18, 0.15, 0.10];
        let mut prev = 1.0;
        for step in 0..30 {
            let t = step as f32 * 0.04;
            let probe = [fg[0] + t, fg[1] + t, fg[2] + t];
            let w = foreground_weight(probe, fg, 0.12, 0.15);
            assert!(
                w <= prev + 1e-5,
                "weight rose at step {step}: {prev} → {w}"
            );
            prev = w;
        }
        approx(prev, 0.0);
    }

    #[test]
    fn foreground_weight_zero_softness_is_finite() {
        // Hard cutoff: tolerance 0.1, softness 0. The shader uses a 0.0001
        // floor so the result stays finite either side of the boundary.
        let fg = [0.5, 0.5, 0.5];
        let just_inside = [0.55, 0.5, 0.5]; // dist 0.05 < 0.1
        let just_outside = [0.7, 0.5, 0.5]; // dist 0.2 > 0.1
        let w_in = foreground_weight(just_inside, fg, 0.1, 0.0);
        let w_out = foreground_weight(just_outside, fg, 0.1, 0.0);
        assert!(w_in.is_finite() && w_out.is_finite());
        approx(w_in, 1.0);
        approx(w_out, 0.0);
    }

    #[test]
    fn foreground_weight_catches_aa_toward_background() {
        // The whole point of this mode: an achromatic foreground glyph
        // antialiased toward the background still registers near the
        // glyph centre — catching cases the saturation and bright-ANSI
        // modes can't (both fail when fg is grey/white/black).
        //
        // Math sanity check: with fg = [0,0,0], bg = white, the RGB
        // distance from blended back to fg is (1 - alpha) * √3. At
        // alpha = 0.85 that's ≈ 0.26, which still lands in the
        // tolerance + softness band (0.12 + 0.15 = 0.27). The deeper
        // AA fringe (alpha < ~0.85) falls past the band, which is fine
        // — those pixels are visually closer to background than to fg.
        let fg = [0.0, 0.0, 0.0];
        let bg = 1.0;
        for alpha in [1.0_f32, 0.97, 0.95, 0.9, 0.86] {
            let blended = [
                fg[0] * alpha + bg * (1.0 - alpha),
                fg[1] * alpha + bg * (1.0 - alpha),
                fg[2] * alpha + bg * (1.0 - alpha),
            ];
            let w = foreground_weight(blended, fg, 0.12, 0.15);
            assert!(
                w > 0.0,
                "AA fringe at coverage {alpha} should still register, got {w}"
            );
        }
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
