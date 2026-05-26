//! Off-main-thread swapchain presentation.
//!
//! Each window owns a [`Presenter`]: a dedicated thread that holds the live
//! `wgpu::Surface` and performs the blocking `get_current_texture()` +
//! `present()`. The main thread renders a frame into one of a small pool of
//! offscreen *present-source* targets (surface format, sampleable), then hands
//! that target's index to the presenter and returns immediately.
//!
//! Why: with the native macOS tab bar, each tab is a separate `NSWindow` and
//! AppKit draws the bar + performs the tab swap on the main thread. The old
//! renderer called `get_current_texture()` on that same main thread, where it
//! blocks at the swapchain for vsync (Fifo) — measured at ~97% of render time
//! under output. While the run loop sits in that wait, AppKit can't repaint the
//! tab bar or service a tab click, so the highlight visibly lags. Moving the
//! blocking acquire/present here keeps the main thread free, so switches stay
//! responsive even while a tab is busy rendering.
//!
//! ## Ordering / safety
//! The main thread submits the offscreen render (submission S1), *then* sends
//! `Frame(idx)`. The presenter receives it and submits the blit (S2). Because
//! the channel send strictly happens-after S1's `submit()` returns and the
//! presenter's `submit()` happens-after the receive, S1 is assigned a lower
//! submission index than S2 and the GPU runs the offscreen render before the
//! blit reads it — no explicit fence needed. The write-after-read hazard on
//! reuse is covered the same way: the main thread only re-renders into a target
//! after the presenter returns its index (which it does only after submitting
//! the blit), so the next render's submission index exceeds the blit's.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

/// One offscreen render target the main thread draws into and the presenter
/// blits to the swapchain. Texture + view live behind `Arc` so both threads
/// hold a handle to the same GPU resource.
#[derive(Clone)]
pub struct PresentTarget {
    pub texture: Arc<wgpu::Texture>,
    pub view: Arc<wgpu::TextureView>,
}

impl PresentTarget {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        label: &str,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
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
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture: Arc::new(texture),
            view: Arc::new(view),
        }
    }
}

/// Number of present-source targets. Two is enough to pipeline (main renders
/// frame N+1 while the presenter blits/presents frame N) without letting the
/// main thread run unboundedly ahead — when both are in flight the main thread
/// simply skips the frame and the content catches up on the next one.
pub const POOL_SIZE: usize = 2;

/// Pure bookkeeping for the present-source pool's free list, factored out of
/// the renderer so the frame-drop backpressure semantics can be tested without
/// a GPU. Holds the indices (`0..POOL_SIZE`) of targets the main thread is free
/// to render into; the presenter returns indices here as it finishes with them.
///
/// Invariants (relied on for GPU correctness — a duplicate or out-of-range
/// index would alias a target still being read by the present thread):
/// * every index handed out by [`FreePool::acquire`] was previously free and
///   is removed from the free set until [`FreePool::release`]d back;
/// * [`FreePool::acquire`] yields `None` exactly when all targets are in flight
///   (the renderer treats that as "drop this frame").
#[derive(Debug, Clone)]
pub struct FreePool {
    free: Vec<usize>,
}

impl FreePool {
    /// A pool with all `len` targets initially free (the just-created /
    /// just-resized state, where nothing is in flight on the present thread).
    pub fn new(len: usize) -> Self {
        Self {
            free: (0..len).collect(),
        }
    }

    /// Take a free target to render into, or `None` if every target is still in
    /// flight — the caller drops the frame and retries next time. Pops (not
    /// peeks): the index is removed from the free set so it can't be handed out
    /// again until the presenter [`FreePool::release`]s it back, which is what
    /// keeps the renderer from overwriting a target the present thread is still
    /// reading.
    pub fn acquire(&mut self) -> Option<usize> {
        self.free.pop()
    }

    /// Return a target the present thread has finished with to the free set.
    pub fn release(&mut self, idx: usize) {
        self.free.push(idx);
    }

}

enum Msg {
    /// Target `idx` holds a finished frame; blit it to the swapchain + present.
    Frame(usize),
    /// Surface resized: reconfigure to this size, then rebuild the per-target
    /// blit bind groups against the freshly recreated pool views.
    Resize {
        width: u32,
        height: u32,
        views: Vec<Arc<wgpu::TextureView>>,
    },
    Stop,
}

/// Handle to a window's present thread. Dropping it (via [`Presenter::stop`],
/// called from `WindowState`'s `Drop`) signals the thread to finish and joins
/// it, which drops the `wgpu::Surface` before the window is torn down.
pub struct Presenter {
    tx: Sender<Msg>,
    free_rx: Receiver<usize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Presenter {
    /// Spawn the present thread. `surface` is already configured to `config`;
    /// `views` are the initial pool target views, one per [`POOL_SIZE`].
    pub fn spawn(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        surface: wgpu::Surface,
        config: wgpu::SurfaceConfiguration,
        views: Vec<Arc<wgpu::TextureView>>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (free_tx, free_rx) = std::sync::mpsc::channel::<usize>();

        let handle = std::thread::Builder::new()
            .name("yutani-present".into())
            .spawn(move || present_loop(device, queue, surface, config, views, rx, free_tx))
            .expect("spawn present thread");

        Self {
            tx,
            free_rx,
            handle: Some(handle),
        }
    }

    /// Drain the targets the presenter has finished with since the last call,
    /// folding each back into `pool` so the renderer can then [`FreePool::acquire`]
    /// one to draw into.
    pub fn drain_free(&self, pool: &mut FreePool) {
        while let Ok(idx) = self.free_rx.try_recv() {
            pool.release(idx);
        }
    }

    /// Hand a finished frame to the presenter. The target must not be rendered
    /// into again until its index comes back from [`Presenter::drain_free`].
    pub fn present(&self, idx: usize) {
        // Send only fails if the thread is gone (shutting down) — harmless.
        let _ = self.tx.send(Msg::Frame(idx));
    }

    /// Tell the presenter the surface (and pool) resized. `views` are the new
    /// pool target views the main thread just recreated.
    pub fn resize(&self, width: u32, height: u32, views: Vec<Arc<wgpu::TextureView>>) {
        let _ = self.tx.send(Msg::Resize {
            width,
            height,
            views,
        });
    }

    /// Signal the thread to stop and join it, dropping the surface. Idempotent.
    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.tx.send(Msg::Stop);
            let _ = handle.join();
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Minimal fullscreen blit: samples a present-source target and writes it to
/// the swapchain. UV math mirrors `blur.wgsl`'s `vs_fullscreen`/`fs_blit` so the
/// image orientation matches the existing scene→swapchain composite exactly.
const BLIT_WGSL: &str = r#"
@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_smp: sampler;

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

@fragment
fn fs_blit(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, src_smp, in.uv);
}
"#;

fn build_blit(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout, wgpu::Sampler) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("present blit shader"),
        source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("present blit bgl"),
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
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("present blit sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("present blit layout"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("present blit pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_fullscreen",
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_blit",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
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
    (pipeline, bgl, sampler)
}

fn make_bind_groups(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    views: &[Arc<wgpu::TextureView>],
) -> Vec<wgpu::BindGroup> {
    views
        .iter()
        .enumerate()
        .map(|(i, view)| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&format!("present blit bg {i}")),
                layout: bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        })
        .collect()
}

fn present_loop(
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface: wgpu::Surface,
    mut config: wgpu::SurfaceConfiguration,
    views: Vec<Arc<wgpu::TextureView>>,
    rx: Receiver<Msg>,
    free_tx: Sender<usize>,
) {
    let (pipeline, bgl, sampler) = build_blit(&device, config.format);
    let mut bind_groups = make_bind_groups(&device, &bgl, &sampler, &views);
    drop(views);

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stop => break,
            Msg::Resize {
                width,
                height,
                views,
            } => {
                config.width = width;
                config.height = height;
                if width > 0 && height > 0 {
                    surface.configure(&device, &config);
                }
                bind_groups = make_bind_groups(&device, &bgl, &sampler, &views);
            }
            Msg::Frame(idx) => {
                // A zero-sized (minimized) surface can't be presented; just
                // release the target so the main thread can reuse it.
                if config.width == 0 || config.height == 0 || idx >= bind_groups.len() {
                    let _ = free_tx.send(idx);
                    continue;
                }
                // Acquire, reconfiguring on the transient Outdated/Lost that a
                // resized/relayered surface returns.
                let frame = match surface.get_current_texture() {
                    Ok(f) => Some(f),
                    Err(wgpu::SurfaceError::Outdated) | Err(wgpu::SurfaceError::Lost) => {
                        surface.configure(&device, &config);
                        surface.get_current_texture().ok()
                    }
                    Err(_) => None,
                };
                let Some(frame) = frame else {
                    let _ = free_tx.send(idx);
                    continue;
                };
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("present blit"),
                    });
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("present blit pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
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
                    pass.set_pipeline(&pipeline);
                    pass.set_bind_group(0, &bind_groups[idx], &[]);
                    pass.draw(0..3, 0..1);
                }
                queue.submit(std::iter::once(encoder.finish()));
                frame.present();
                let _ = free_tx.send(idx);
            }
        }
    }
    // Thread exit drops `surface` here, before `WindowState` drops its window.
}
