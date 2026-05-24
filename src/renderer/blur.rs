//! Dual-Kawase blur for the edge-fade strips.
//!
//! Split into [`BlurPipelines`] — the shader, layouts, samplers, and render
//! pipelines, which depend only on the device + surface format and are shared
//! by every window via `AppShared` — and [`BlurChain`], the per-window
//! offscreen scene target, half-resolution mip chain, bind groups, and
//! per-level uniforms. The down/up chain uses 3 levels (1/2 → 1/4 → 1/8)
//! which gives a generous "fat" glass blur.

use wgpu::util::DeviceExt;

const CHAIN_LEVELS: usize = 2;
// Default extra inner down/up passes. Each iteration adds one down
// (chain[0]→chain[1]) and one up (chain[1]→chain[0]), compounding the
// kernel at chain[0]'s resolution. Runtime-tunable via BlurChain::iterations.
const DEFAULT_BLUR_ITERATIONS: usize = 2;
pub const MAX_BLUR_ITERATIONS: usize = 12;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurParams {
    texel_size: [f32; 2],
    _pad: [f32; 2],
}

pub struct Target {
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

/// Shader, layouts, sampler, and render pipelines for the dual-Kawase blur.
/// Depend only on the device and the (process-uniform) surface format, so one
/// instance is shared by every window through `AppShared` — the blur shader is
/// compiled once per process, not once per window. The size-dependent
/// textures, bind groups, and uniforms live in [`BlurChain`] (one per window).
pub struct BlurPipelines {
    pub format: wgpu::TextureFormat,
    sampler: wgpu::Sampler,
    blur_bgl: wgpu::BindGroupLayout, // group(0) for fullscreen passes
    strip_uniform_bgl: wgpu::BindGroupLayout, // group(2) for the strip pass

    pub blit_pipeline: wgpu::RenderPipeline,
    /// Same shader as `blit_pipeline`, but with premultiplied-alpha
    /// blending so a transparent-cleared layer can be composited *over*
    /// existing framebuffer contents. Used by the layered glow render
    /// path to lay the fg scene on top of (bg scene + bg glow).
    pub blit_alpha_pipeline: wgpu::RenderPipeline,
    pub down_pipeline: wgpu::RenderPipeline,
    pub up_pipeline: wgpu::RenderPipeline,
    pub strip_pipeline: wgpu::RenderPipeline,

    /// Dummy uniform bound at group(0) binding 2 of the blit / strip-source
    /// bind groups. Its contents are unused by those shaders, but the layout
    /// requires a buffer; shared because it never holds per-window state.
    blit_uniform: wgpu::Buffer,
}

impl BlurPipelines {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        camera_bgl: &wgpu::BindGroupLayout,
        vertex_layout: wgpu::VertexBufferLayout<'static>,
    ) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("blur sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let blur_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blur bgl"),
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
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let strip_uniform_bgl =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("blur strip uniform bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let shader =
            device.create_shader_module(wgpu::include_wgsl!("blur.wgsl"));

        // --- Fullscreen pipelines (blit/down/up) share group(0). ---
        let fs_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blur fs layout"),
            bind_group_layouts: &[&blur_bgl],
            push_constant_ranges: &[],
        });

        let make_fs_pipeline = |entry: &str, label: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&fs_layout),
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

        let blit_pipeline = make_fs_pipeline("fs_blit", "blur blit pipeline", None);
        let blit_alpha_pipeline = make_fs_pipeline(
            "fs_blit",
            "blur blit-alpha pipeline",
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );
        let down_pipeline = make_fs_pipeline("fs_down", "blur down pipeline", None);
        let up_pipeline = make_fs_pipeline("fs_up", "blur up pipeline", None);

        // --- Strip pipeline (group 0 = blur tex, group 1 = camera, group 2 = uniform). ---
        let strip_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blur strip layout"),
            bind_group_layouts: &[&blur_bgl, camera_bgl, &strip_uniform_bgl],
            push_constant_ranges: &[],
        });
        let strip_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blur strip pipeline"),
            layout: Some(&strip_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_strip",
                buffers: &[vertex_layout],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_strip",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Cw,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        // Static dummy uniform — its texel_size field is unused by the blit /
        // strip-source shaders, but the bind-group layout requires a buffer.
        let blit_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("blur blit uniform"),
            contents: bytemuck::cast_slice(&[BlurParams { texel_size: [0.0, 0.0], _pad: [0.0; 2] }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        Self {
            format,
            sampler,
            blur_bgl,
            strip_uniform_bgl,
            blit_pipeline,
            blit_alpha_pipeline,
            down_pipeline,
            up_pipeline,
            strip_pipeline,
            blit_uniform,
        }
    }

    /// Bind group compatible with `blit_pipeline` / `blit_alpha_pipeline`
    /// for an arbitrary same-format texture view. Used by the layered
    /// render path to blit a second offscreen scene (the fg layer) to
    /// the swapchain with alpha blending. The shared `blit_uniform` is
    /// unused by the blit shader but the bind-group layout requires it.
    pub fn make_blit_bind_group(
        &self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        label: &str,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.blur_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.blit_uniform.as_entire_binding(),
                },
            ],
        })
    }
}

/// Per-window blur resources: the offscreen scene target, the half-resolution
/// mip chain, the bind groups that feed each pass, and the per-level uniforms.
/// Rebuilt on resize. Renders against the shared [`BlurPipelines`].
pub struct BlurChain {
    pub scene: Target,

    chain: Vec<Target>,

    // One bind group per source texture per pass — built once per resize.
    blit_scene_bg: wgpu::BindGroup,        // sample scene → swap
    down_bgs: Vec<wgpu::BindGroup>,        // src for each down pass
    up_bgs: Vec<wgpu::BindGroup>,          // src for each up pass
    pub strip_blur_bg: wgpu::BindGroup,    // sample final blur in strip pass
    pub strip_uniform_bg: wgpu::BindGroup, // strip-pass uniform (1/viewport)

    // Per-level uniforms holding the source's 1/texel_size.
    down_uniforms: Vec<wgpu::Buffer>,
    up_uniforms: Vec<wgpu::Buffer>,
    strip_uniform: wgpu::Buffer, // 1/viewport for strip pass

    pub iterations: usize,
}

impl BlurChain {
    pub fn new(
        device: &wgpu::Device,
        pipelines: &BlurPipelines,
        width: u32,
        height: u32,
    ) -> Self {
        let strip_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("blur strip uniform"),
            contents: bytemuck::cast_slice(&[BlurParams { texel_size: [0.0, 0.0], _pad: [0.0; 2] }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let mut down_uniforms = Vec::with_capacity(CHAIN_LEVELS);
        let mut up_uniforms = Vec::with_capacity(CHAIN_LEVELS);
        for _ in 0..CHAIN_LEVELS {
            down_uniforms.push(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("blur down uniform"),
                contents: bytemuck::cast_slice(&[BlurParams { texel_size: [0.0, 0.0], _pad: [0.0; 2] }]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            }));
            up_uniforms.push(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("blur up uniform"),
                contents: bytemuck::cast_slice(&[BlurParams { texel_size: [0.0, 0.0], _pad: [0.0; 2] }]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            }));
        }

        let strip_uniform_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blur strip uniform bg"),
            layout: &pipelines.strip_uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: strip_uniform.as_entire_binding(),
            }],
        });

        // Build textures + per-level bind groups.
        let (scene, chain, blit_scene_bg, down_bgs, up_bgs, strip_blur_bg) = build_resources(
            device,
            pipelines.format,
            width,
            height,
            &pipelines.sampler,
            &pipelines.blur_bgl,
            &pipelines.blit_uniform,
            &down_uniforms,
            &up_uniforms,
        );

        Self {
            scene,
            chain,
            blit_scene_bg,
            down_bgs,
            up_bgs,
            strip_blur_bg,
            strip_uniform_bg,
            down_uniforms,
            up_uniforms,
            strip_uniform,
            iterations: DEFAULT_BLUR_ITERATIONS,
        }
    }

    pub fn resize(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipelines: &BlurPipelines,
        width: u32,
        height: u32,
    ) {
        let (scene, chain, blit_scene_bg, down_bgs, up_bgs, strip_blur_bg) = build_resources(
            device,
            pipelines.format,
            width,
            height,
            &pipelines.sampler,
            &pipelines.blur_bgl,
            &pipelines.blit_uniform,
            &self.down_uniforms,
            &self.up_uniforms,
        );
        self.scene = scene;
        self.chain = chain;
        self.blit_scene_bg = blit_scene_bg;
        self.down_bgs = down_bgs;
        self.up_bgs = up_bgs;
        self.strip_blur_bg = strip_blur_bg;
        self.write_uniforms(queue, width, height);
    }

    pub fn write_uniforms(&self, queue: &wgpu::Queue, width: u32, height: u32) {
        // Each pass's source is the texture being read; offsets are in source
        // texels. Down pass i reads chain[i-1] (or scene at i=0); up pass i
        // reads chain[chain.len()-1-i] (or up's previous output) but since we
        // use the existing chain slot's size, just compute per source size.
        let scene_w = width.max(1) as f32;
        let scene_h = height.max(1) as f32;
        // Down sources: scene, chain[0], chain[1] (last down dest is chain[2]).
        let mut sizes: Vec<(f32, f32)> = Vec::with_capacity(CHAIN_LEVELS);
        sizes.push((scene_w, scene_h));
        for lvl in 0..CHAIN_LEVELS - 1 {
            sizes.push((self.chain[lvl].width as f32, self.chain[lvl].height as f32));
        }
        for (i, (w, h)) in sizes.iter().enumerate() {
            queue.write_buffer(
                &self.down_uniforms[i],
                0,
                bytemuck::cast_slice(&[BlurParams {
                    texel_size: [1.0 / *w, 1.0 / *h],
                    _pad: [0.0; 2],
                }]),
            );
        }
        // Up sources walk back up the chain: chain[N-1], chain[N-2], ..., chain[0].
        for i in 0..CHAIN_LEVELS {
            let src = &self.chain[CHAIN_LEVELS - 1 - i];
            queue.write_buffer(
                &self.up_uniforms[i],
                0,
                bytemuck::cast_slice(&[BlurParams {
                    texel_size: [1.0 / src.width as f32, 1.0 / src.height as f32],
                    _pad: [0.0; 2],
                }]),
            );
        }
        // Strip pass uses 1 / viewport so frag coord can become a UV.
        queue.write_buffer(
            &self.strip_uniform,
            0,
            bytemuck::cast_slice(&[BlurParams {
                texel_size: [1.0 / scene_w, 1.0 / scene_h],
                _pad: [0.0; 2],
            }]),
        );
    }

    /// Render the dual-Kawase chain. Source is `self.scene`, the final
    /// upsampled blur lands in `self.scene` itself? No — in the *first*
    /// chain slot (chain[0]) so the strip pass can sample it. We chose to
    /// use the last "up" target as the final, which by our walk lands in
    /// chain[0] (matches the source resolution / 2). The strip pass treats
    /// chain[0] as the blur sampler.
    pub fn run(&self, encoder: &mut wgpu::CommandEncoder, pipelines: &BlurPipelines) {
        // Down chain: scene → chain[0] → chain[1] → chain[2].
        self.fullscreen_pass(
            encoder,
            &pipelines.down_pipeline,
            &self.down_bgs[0],
            &self.chain[0].view,
            "blur down 0",
        );
        for i in 1..CHAIN_LEVELS {
            self.fullscreen_pass(
                encoder,
                &pipelines.down_pipeline,
                &self.down_bgs[i],
                &self.chain[i].view,
                "blur down i",
            );
        }
        // Up chain: chain[N-1] → chain[N-2] → ... → chain[0].
        for i in 0..CHAIN_LEVELS - 1 {
            let dst_idx = CHAIN_LEVELS - 2 - i;
            self.fullscreen_pass(
                encoder,
                &pipelines.up_pipeline,
                &self.up_bgs[i],
                &self.chain[dst_idx].view,
                "blur up i",
            );
        }
        // Iterate the inner down/up at chain[0]'s resolution to widen the
        // kernel without adding deeper levels (which were the source of
        // visible pixel-grid jumps during scroll).
        for _ in 1..self.iterations.max(1) {
            self.fullscreen_pass(
                encoder,
                &pipelines.down_pipeline,
                &self.down_bgs[CHAIN_LEVELS - 1],
                &self.chain[CHAIN_LEVELS - 1].view,
                "blur down iter",
            );
            self.fullscreen_pass(
                encoder,
                &pipelines.up_pipeline,
                &self.up_bgs[CHAIN_LEVELS - 2],
                &self.chain[0].view,
                "blur up iter",
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

    /// Bind group that samples `self.scene` (used to blit scene → swap).
    pub fn blit_bind_group(&self) -> &wgpu::BindGroup {
        &self.blit_scene_bg
    }
}

fn build_resources(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    sampler: &wgpu::Sampler,
    blur_bgl: &wgpu::BindGroupLayout,
    blit_uniform: &wgpu::Buffer,
    down_uniforms: &[wgpu::Buffer],
    up_uniforms: &[wgpu::Buffer],
) -> (
    Target,
    Vec<Target>,
    wgpu::BindGroup,
    Vec<wgpu::BindGroup>,
    Vec<wgpu::BindGroup>,
    wgpu::BindGroup,
) {
    let w = width.max(1);
    let h = height.max(1);

    let scene_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("blur scene"),
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
    let scene_view = scene_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let scene = Target {
        view: scene_view,
        width: w,
        height: h,
    };

    let mut chain: Vec<Target> = Vec::with_capacity(CHAIN_LEVELS);
    let mut cw = w;
    let mut ch = h;
    for i in 0..CHAIN_LEVELS {
        cw = (cw / 2).max(1);
        ch = (ch / 2).max(1);
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blur chain"),
            size: wgpu::Extent3d {
                width: cw,
                height: ch,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        chain.push(Target {
            view,
            width: cw,
            height: ch,
        });
        let _ = i;
    }

    let make_bg = |src: &wgpu::TextureView, uniform: &wgpu::Buffer, label: &str| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: blur_bgl,
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
                    resource: uniform.as_entire_binding(),
                },
            ],
        })
    };

    // blit: sample scene; uniform unused but layout requires it.
    let blit_scene_bg = make_bg(&scene.view, blit_uniform, "blur blit bg");

    // Down: source for level i is scene (i=0) or chain[i-1].
    let mut down_bgs = Vec::with_capacity(CHAIN_LEVELS);
    for i in 0..CHAIN_LEVELS {
        let src = if i == 0 { &scene.view } else { &chain[i - 1].view };
        down_bgs.push(make_bg(src, &down_uniforms[i], "blur down bg"));
    }

    // Up: walks back up — source for step i is chain[N-1-i].
    let mut up_bgs = Vec::with_capacity(CHAIN_LEVELS);
    for i in 0..CHAIN_LEVELS {
        let src = &chain[CHAIN_LEVELS - 1 - i].view;
        up_bgs.push(make_bg(src, &up_uniforms[i], "blur up bg"));
    }

    // Strip pass samples the final blur, which after run() lives in chain[0].
    // Reuse the blit uniform buffer (its texel_size field is unused here —
    // the strip shader has its own group-2 uniform with 1/viewport).
    let strip_blur_bg = make_bg(&chain[0].view, blit_uniform, "blur strip blur bg");

    (
        scene,
        chain,
        blit_scene_bg,
        down_bgs,
        up_bgs,
        strip_blur_bg,
    )
}
