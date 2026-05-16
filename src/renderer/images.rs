//! Textured-quad pipeline for image placements.
//!
//! Slice 1 of the image-rendering feature. The pipeline samples an
//! `Rgba8UnormSrgb` texture per placement and draws into whichever target
//! the caller hands it — currently `blur.scene` (so images participate in
//! the existing glow + edge-blur passes alongside cell backgrounds).
//!
//! - Vertices are in framebuffer pixel coords; reuses the grid camera at
//!   bind group 1, so positions map 1:1 to the window.
//! - Each `GpuImage` owns its texture + view + a per-image bind group at
//!   group 0. Bind groups are cheap to build but expensive to rebuild every
//!   frame, so we cache them on the `GpuImage` itself.
//! - All quads share one vertex/index buffer, repopulated on the host each
//!   frame from the placement list. Index count per placement is fixed at 6
//!   (two triangles), so draws are `pass.draw_indexed(i*6..(i+1)*6, …)`.

use wgpu::util::DeviceExt;

use super::texture::Texture;

/// Soft cap on placements drawn per frame. Vertex/index buffers are sized
/// for this; if a frame ever needs more we'd grow on the fly (not yet
/// implemented — phase 3 territory).
pub const MAX_PLACEMENTS_PER_FRAME: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageVertex {
    position: [f32; 3],
    uv: [f32; 2],
}

impl ImageVertex {
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

/// One decoded image living on the GPU. Holds the texture + a pre-built bind
/// group keyed against the pipeline's per-image bgl, so draws are
/// `set_bind_group(0, &img.bind_group, …)`.
pub struct GpuImage {
    pub width_px: u32,
    pub height_px: u32,
    #[allow(dead_code)]
    pub texture: Texture,
    pub bind_group: wgpu::BindGroup,
}

/// Where to draw a `GpuImage` this frame. Framebuffer-pixel coords, top-left
/// origin (matches the camera's ortho).
#[derive(Clone, Copy)]
pub struct ImageDraw<'a> {
    pub image: &'a GpuImage,
    pub x_px: f32,
    pub y_px: f32,
    pub w_px: f32,
    pub h_px: f32,
    /// UV sub-rect to sample, as `(u0, v0, u1, v1)`. `None` defaults to the
    /// full `(0, 0, 1, 1)` quad — preserves phase 1 "draw the whole image"
    /// behavior. Set by callers that converted a `Placement.src_rect`
    /// (pixel coords) to UVs against this `GpuImage`'s known size.
    pub uv_rect: Option<(f32, f32, f32, f32)>,
}

pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    image_bgl: wgpu::BindGroupLayout,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
}

impl ImagePipeline {
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        camera_bgl: &wgpu::BindGroupLayout,
    ) -> Self {
        let image_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image bgl"),
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image pipeline layout"),
            bind_group_layouts: &[&image_bgl, camera_bgl],
            push_constant_ranges: &[],
        });

        let shader = device.create_shader_module(wgpu::include_wgsl!("images.wgsl"));

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[ImageVertex::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Cw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
        });

        let sampler_linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler (linear)"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let sampler_nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler (nearest)"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image vertex buffer"),
            size: (MAX_PLACEMENTS_PER_FRAME * 4 * std::mem::size_of::<ImageVertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Six indices per placement, never changes — fill once at construction.
        let mut indices: Vec<u16> = Vec::with_capacity(MAX_PLACEMENTS_PER_FRAME * 6);
        for i in 0..MAX_PLACEMENTS_PER_FRAME {
            let base = (i * 4) as u16;
            indices.extend_from_slice(&[
                base, base + 1, base + 2,
                base, base + 2, base + 3,
            ]);
        }
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("image index buffer"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        Self {
            pipeline,
            image_bgl,
            sampler_linear,
            sampler_nearest,
            vertex_buffer,
            index_buffer,
        }
    }

    /// Linear vs nearest sampler for new uploads. Config flip; existing
    /// `GpuImage`s keep whichever sampler their bind group was built with.
    fn sampler(&self, nearest: bool) -> &wgpu::Sampler {
        if nearest { &self.sampler_nearest } else { &self.sampler_linear }
    }

    /// Upload a decoded RGBA8 image to the GPU and pre-build its bind group.
    /// Caller is responsible for sizing/decoding (slice 3 does this in
    /// `crate::images::Store`); this is the GPU-side step in isolation.
    pub fn upload_rgba(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rgba: &[u8],
        width: u32,
        height: u32,
        nearest_filter: bool,
        label: Option<&str>,
    ) -> GpuImage {
        debug_assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4);
        let texture = Texture::from_memory(
            device,
            queue,
            rgba,
            width,
            height,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            label,
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image bind group"),
            layout: &self.image_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(self.sampler(nearest_filter)),
                },
            ],
        });
        GpuImage { width_px: width, height_px: height, texture, bind_group }
    }

    /// Issue one render pass that draws every `draws` quad into `target`.
    /// `load` lets the caller decide whether to clear (standalone pass) or
    /// preserve existing pixels (compose over bg cells).
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        queue: &wgpu::Queue,
        camera_bind_group: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        load: wgpu::LoadOp<wgpu::Color>,
        draws: &[ImageDraw<'_>],
    ) {
        if draws.is_empty() {
            // Still emit a clear pass when asked, so the caller can rely on
            // this for "initialize the layer" semantics.
            if matches!(load, wgpu::LoadOp::Clear(_)) {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("image pass (clear-only)"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
            }
            return;
        }

        let n = draws.len().min(MAX_PLACEMENTS_PER_FRAME);
        let mut verts: Vec<ImageVertex> = Vec::with_capacity(n * 4);
        for d in &draws[..n] {
            let (u0, v0, u1, v1) = d.uv_rect.unwrap_or((0.0, 0.0, 1.0, 1.0));
            push_quad_verts(&mut verts, d.x_px, d.y_px, d.w_px, d.h_px, u0, v0, u1, v1);
        }
        queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&verts));

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("image pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(1, camera_bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        for (i, d) in draws[..n].iter().enumerate() {
            pass.set_bind_group(0, &d.image.bind_group, &[]);
            let base = (i * 6) as u32;
            pass.draw_indexed(base..base + 6, 0, 0..1);
        }
    }
}

/// Push four `ImageVertex`es for one quad in CW winding (TL, TR, BR, BL),
/// with UV corners `(u0,v0)`..`(u1,v1)`. UVs are caller-supplied so a
/// `Placement.src_rect` translates straight to a textured sub-rect without
/// touching the shader. Phase 1 callers pass `0..1` for full-image draws.
#[allow(clippy::too_many_arguments)]
fn push_quad_verts(
    out: &mut Vec<ImageVertex>,
    x: f32, y: f32, w: f32, h: f32,
    u0: f32, v0: f32, u1: f32, v1: f32,
) {
    let l = x;
    let t = y;
    let r = x + w;
    let b = y + h;
    out.push(ImageVertex { position: [l, t, 0.0], uv: [u0, v0] });
    out.push(ImageVertex { position: [r, t, 0.0], uv: [u1, v0] });
    out.push(ImageVertex { position: [r, b, 0.0], uv: [u1, v1] });
    out.push(ImageVertex { position: [l, b, 0.0], uv: [u0, v1] });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5
    }

    #[test]
    fn quad_verts_cw_winding_and_corner_uvs() {
        let mut v = Vec::new();
        push_quad_verts(&mut v, 10.0, 20.0, 100.0, 50.0, 0.0, 0.0, 1.0, 1.0);
        assert_eq!(v.len(), 4);

        // Positions: TL, TR, BR, BL (CW from top-left in screen space where
        // y grows downward — matches the grid pipeline's FrontFace::Cw).
        assert!(approx([v[0].position[0], v[0].position[1]], [10.0, 20.0]));
        assert!(approx([v[1].position[0], v[1].position[1]], [110.0, 20.0]));
        assert!(approx([v[2].position[0], v[2].position[1]], [110.0, 70.0]));
        assert!(approx([v[3].position[0], v[3].position[1]], [10.0, 70.0]));

        // Z is unused (camera ortho ignores it for placement) — all on plane.
        for vert in &v {
            assert_eq!(vert.position[2], 0.0);
        }

        // UVs match corners — (0,0) at TL through (0,1) at BL. Image data
        // arrives top-row-first from the `image` crate so this orientation
        // makes the texture read upright on screen.
        assert!(approx(v[0].uv, [0.0, 0.0]));
        assert!(approx(v[1].uv, [1.0, 0.0]));
        assert!(approx(v[2].uv, [1.0, 1.0]));
        assert!(approx(v[3].uv, [0.0, 1.0]));
    }

    #[test]
    fn quad_verts_append_to_existing_buffer() {
        // Same buffer is reused across placements within a frame; second
        // call must append (not stomp) so per-placement draws hit the right
        // vertex offsets.
        let mut v = Vec::new();
        push_quad_verts(&mut v, 0.0, 0.0, 10.0, 10.0, 0.0, 0.0, 1.0, 1.0);
        push_quad_verts(&mut v, 50.0, 60.0, 20.0, 30.0, 0.0, 0.0, 1.0, 1.0);
        assert_eq!(v.len(), 8);
        assert!(approx([v[4].position[0], v[4].position[1]], [50.0, 60.0]));
        assert!(approx([v[6].position[0], v[6].position[1]], [70.0, 90.0]));
    }

    #[test]
    fn quad_verts_handles_zero_size() {
        // Degenerate but legal — a placement collapsed by resize math should
        // produce a degenerate quad, not panic. The render pass will draw it
        // and the rasterizer drops it.
        let mut v = Vec::new();
        push_quad_verts(&mut v, 5.0, 7.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0);
        for vert in &v {
            assert_eq!(vert.position[0], 5.0);
            assert_eq!(vert.position[1], 7.0);
        }
    }

    #[test]
    fn quad_verts_custom_uv_rect_lands_on_corners() {
        // A `Placement.src_rect` resolved against a 100x100 image with
        // (10,10,50,30) becomes UVs (0.1, 0.1, 0.6, 0.4). Pin the UV-per-corner
        // mapping so a future refactor that reorders the corner pushes (or
        // flips which UV the BR corner gets) trips this test.
        let mut v = Vec::new();
        push_quad_verts(&mut v, 0.0, 0.0, 200.0, 100.0, 0.1, 0.1, 0.6, 0.4);
        assert!(approx(v[0].uv, [0.1, 0.1])); // TL
        assert!(approx(v[1].uv, [0.6, 0.1])); // TR
        assert!(approx(v[2].uv, [0.6, 0.4])); // BR
        assert!(approx(v[3].uv, [0.1, 0.4])); // BL
    }

    #[test]
    fn quad_verts_degenerate_uv_rect_does_not_panic() {
        // A `src_rect` of (0,0,0,0) collapses to u0==u1, v0==v1 — every
        // corner samples the same texel and the quad is a UV point. Must
        // not panic; renderer feeds whatever the parser produced.
        let mut v = Vec::new();
        push_quad_verts(&mut v, 0.0, 0.0, 50.0, 50.0, 0.0, 0.0, 0.0, 0.0);
        for vert in &v {
            assert_eq!(vert.uv, [0.0, 0.0]);
        }
    }

    #[test]
    fn image_vertex_size_matches_layout_stride() {
        // bytemuck cast slice in render() relies on this being correct;
        // mismatch would mean we send garbage to the GPU silently.
        assert_eq!(
            std::mem::size_of::<ImageVertex>(),
            ImageVertex::desc().array_stride as usize,
        );
    }
}
