//! Yutani — a GPU-accelerated terminal emulator (macOS-first, wgpu).
//!
//! Module map (where to look for what):
//!
//! - **Terminal model** — [`terminal`] holds the grid/cell ring buffer,
//!   scrollback, cursor, scroll regions/margins, the VT/CSI/SGR parser
//!   ([`ansi`]) dispatch, and shell-integration OSC handlers. The inline-image
//!   protocols (iTerm2 `OSC 1337`, Kitty graphics) live in its
//!   `terminal/image_protocol.rs` submodule.
//! - **Window state** — the per-window [`WindowState`] god-struct is defined
//!   here in `main.rs`; its methods are organized by topic across the
//!   `state_*` extension files (all `impl WindowState`): `state_render`
//!   (vertex/quad emission + frame composition), `state_input` (key/IME
//!   routing), `state_pointer` (mouse/selection/hover), `state_completion`
//!   (autocomplete popup), `state_image` (inline-image upload/placement),
//!   `state_theme` (scheme/appearance sync), `state_anim` (smooth-scroll and
//!   fade animation). Process-shared GPU resources live in `AppShared`.
//! - **GPU/rendering** — [`renderer`] wraps wgpu: `renderer::blur`,
//!   `renderer::glow`, `renderer::images`, plus shared helpers in
//!   `renderer/mod.rs` (e.g. [`renderer::uniform_buffer`]); [`gpu`] /
//!   [`present`] own surface + frame presentation.
//! - **Overlays/UI chrome** — [`command_palette`], [`completion`], [`search`],
//!   [`onboard`] are the cross-platform popup models; the `glass_*` files
//!   (`glass`, `glass_palette`, `glass_find`, `glass_complete`) are the
//!   macOS-native Liquid Glass panels (deliberately kept as separate AppKit
//!   controllers). [`palette`] is color/scheme math, not a UI element.
//! - **Fonts/text** — [`font`], [`shaper`], [`font_loader`] (per-platform),
//!   [`box_drawing`]. **Config/theming** — [`config`], [`bundled_schemes`],
//!   [`style`]. **Platform glue** — [`app_window`], [`event_loop`],
//!   [`pty`], [`touchid`], [`paths`].

mod app_window;
mod box_drawing;
mod command_palette;
mod completion;
mod glass;
mod glass_about;
mod glass_complete;
mod glass_find;
mod glass_palette;
mod tab_style;
mod search;
mod font;
mod font_loader;
mod renderer;
mod present;

mod ansi;
mod gpu;
mod images;
mod input;
mod onboard;
mod bundled_schemes;
mod palette;
mod shaper;
mod shell_integration;
mod emoji;
mod style;
mod terminal;
mod width;

mod pty;
mod config;
mod url;
mod font_data;
mod paths;
mod touchid;
mod state_completion;
mod state_anim;
mod state_theme;
mod state_image;
mod state_pointer;
mod state_input;
mod state_render;
mod event_loop;

use winit::{
    event::*,
    event_loop::ActiveEventLoop,
    platform::macos::WindowAttributesExtMacOS,
    platform::modifier_supplement::KeyEventExtModifierSupplement,
    window::Window,
};

extern crate libc;
use nix::libc::*;

use std::cell::RefCell;
use std::rc::Rc;

// use rand_distr::{Distribution, Normal};
// use rand::thread_rng;

use wgpu::util::DeviceExt;
use config::*;
use url::*;
use font_data::*;
use paths::*;
use event_loop::run;

/// Backing scale the fixed-pixel UI metrics in this codebase are authored
/// against. Constants like [`WINDOW_PADDING`], [`DECORATOR_HEIGHT`], the glow
/// scanline period, popup corner radii, and the drag threshold were all tuned
/// on a 2× Retina display. A constant *physical* pixel count looks twice as
/// large on a 1× monitor as on 2× — so wherever apparent (logical) size should
/// stay constant across DPIs, we rescale via [`dpi_px`]. Anchoring to 2× means
/// the Retina look is preserved exactly and only lower-DPI displays change.
pub(crate) const UI_REFERENCE_SCALE: f32 = 2.0;

/// Scale a reference-2× physical-pixel metric to the physical pixels that hold
/// its apparent size constant at `dpi` (= backing_scale × 96; see
/// [`WindowState::set_dpi_from_scale`]). At `dpi == 192` (2×) this is the
/// identity; at `dpi == 96` (1×) it halves the value, so e.g. 16 px of padding
/// authored on Retina draws as 8 px on a 1× screen — the same apparent width.
#[inline]
pub(crate) fn dpi_px(value: f32, dpi: u32) -> f32 {
    value * (dpi as f32 / 96.0) / UI_REFERENCE_SCALE
}

const WINDOW_PADDING: f32 = 16.0;
const DECORATOR_HEIGHT: f32 = 24.0;
/// Maximum rows the completion popup shows at once; longer lists scroll. Shared
/// by the draw block and the keyboard-nav handler so the two can't drift.
const COMPLETION_MAX_VISIBLE: usize = 10;
/// Smallest grid height (in rows) we ever report to the PTY, regardless of how
/// short the window is dragged. Sized to keep a typical multi-line shell prompt
/// resident so resizing never spills it — see the floor in `get_viewport_size`.
const MIN_GRID_ROWS: usize = 4;

const DEFAULT_FONT_SIZE: f32 = 10.0;

/// Byte size of the grid's vertex and index buffers for a viewport of
/// `cols × rows`. `(vertex_bytes, index_bytes)`. Single source of
/// truth so the `WindowState::new` and `resize_buffers` paths can't drift.
///
/// Capacity model: each cell emits two quads (background + glyph), so
/// `area = cols * rows` cells contribute `2 * area` quads. The slack
/// term covers the per-row content the renderer emits *outside* the
/// visible grid — `update_vertices` walks `r_lo..r_hi` where
/// `r_lo = -2, r_hi = rows + 2`, i.e. two phantom rows top + two
/// bottom. Each phantom row contributes up to `cols` cells × 2 quads
/// each, so the total phantom-row contribution is `4 * cols * 2 = 8 *
/// cols` quads worth of vertices. The `+5` covers the cursor quad
/// plus a handful of edge-fade and decorator overlays.
///
/// The band also widens during a smooth scroll slide: by up to
/// `SCROLL_ON_OUTPUT_MAX_ROWS` rows *per side* (the band is symmetric in
/// `|scroll_y|`), on top of the fixed ±2. So the phantom budget is
/// `2 + SCROLL_ON_OUTPUT_MAX_ROWS` rows per side.
///
/// `top_inset_rows` is the *extra* widening `phantom_row_band` applies to the
/// top edge alone when the native tab bar is shown (`ceil(chrome_extra_top /
/// line_height)`, 0 with no bar). It stacks on the scroll slide there, so the
/// worst-case top strip is `2 + SCROLL_ON_OUTPUT_MAX_ROWS + top_inset_rows`
/// rows — the buffer must reserve those extra rows or a full-band scroll with
/// the tab bar up overruns `queue.write_buffer`.
///
/// Expressed as `2 * (area + extra_quads)` where
/// `extra_quads = phantom_rows_per_side * 2 * cols + 5` — one phantom-row
/// strip per side (its rows × `cols` cells × 2 quads) plus the fixed extras,
/// doubled to cover both top and bottom strips — plus the asymmetric
/// `top_inset_rows` strip (its rows × `cols` cells × 2 quads, top only).
fn grid_buffer_byte_sizes(cols: usize, rows: usize, top_inset_rows: usize) -> (usize, usize) {
    let area = cols * rows;
    let phantom_rows_per_side = 2 + SCROLL_ON_OUTPUT_MAX_ROWS;
    let extra_quads = phantom_rows_per_side * 2 * cols + 5;
    let top_inset_quads = top_inset_rows * 2 * cols;
    let quads = 2 * area + 2 * extra_quads + top_inset_quads;
    let vertex_bytes = quads * std::mem::size_of::<renderer::vertex::Vertex>() * 4;
    let index_bytes = quads * std::mem::size_of::<u32>() * 6;
    (vertex_bytes, index_bytes)
}

/// Emit quads for `text` as a left-to-right monospace run starting at pixel
/// (`x`, `baseline_y`), advancing by `cell_w` per character. Pushes into the
/// same `vertices`/`indices` the grid uses, so it must be called within the FG
/// portion of `update_vertices` (after `num_bg_indices` is recorded) to draw on
/// top. Returns the final x advance.
///
/// Reuses the glyph-atlas lookup + bearing math from `emit_fg_for_cell`'s
/// ordinary (non-cell-filling) glyph path. We deliberately *duplicate* that
/// minimal math here rather than refactor the grid closure: the closure
/// captures `&self.atlas`/`scroll_y` and folds in box-drawing UV-clipping and
/// ligature substitution that don't apply to plain popup text, so factoring it
/// into a shared free fn would be a larger, riskier change. Callers must have
/// rasterized the glyphs into `atlas` beforehand (via `ensure_char`) so the
/// lookups hit.
///
/// Characters whose advance would push the glyph past `max_x` are skipped
/// (truncation); the run stops there.
#[allow(clippy::too_many_arguments)]
fn emit_text_run(
    atlas: &font::Atlas,
    vertices: &mut Vec<renderer::vertex::Vertex>,
    indices: &mut Vec<u32>,
    mut x: f32,
    baseline_y: f32,
    text: &str,
    color: [f32; 4],
    atlas_w: f32,
    atlas_h: f32,
    cell_w: f32,
    max_x: f32,
) -> f32 {
    for ch in text.chars() {
        // Truncate once the next cell would overflow the box.
        if x + cell_w > max_x {
            break;
        }
        if ch != ' ' {
            let g = atlas.lookup(ch, font::FaceVariant::Regular);
            if g.width > 0 && g.height > 0 {
                let bx = g.bearing_x as f32;
                let by = g.bearing_y as f32;
                let gx = x + bx;
                let gy = baseline_y - by;
                let gw = g.width as f32;
                let gh = g.height as f32;
                let u0 = g.x as f32 / atlas_w;
                let u1 = (g.x as f32 + gw) / atlas_w;
                let v0 = g.y as f32 / atlas_h;
                let v1 = (g.y as f32 + gh) / atlas_h;
                let start = vertices.len() as u32;
                let hx = gw * 0.5;
                let hy = gh * 0.5;
                let half_size = [hx, hy];
                vertices.push(renderer::vertex::Vertex {
                    position: [gx, gy, 0.0],
                    tex_coords: [u0, v0],
                    color,
                    local_pos: [-hx, -hy],
                    half_size,
                    radii: [0.0; 4],
                    kind: 0.0,
                });
                vertices.push(renderer::vertex::Vertex {
                    position: [gx, gy + gh, 0.0],
                    tex_coords: [u0, v1],
                    color,
                    local_pos: [-hx, hy],
                    half_size,
                    radii: [0.0; 4],
                    kind: 0.0,
                });
                vertices.push(renderer::vertex::Vertex {
                    position: [gx + gw, gy, 0.0],
                    tex_coords: [u1, v0],
                    color,
                    local_pos: [hx, -hy],
                    half_size,
                    radii: [0.0; 4],
                    kind: 0.0,
                });
                vertices.push(renderer::vertex::Vertex {
                    position: [gx + gw, gy + gh, 0.0],
                    tex_coords: [u1, v1],
                    color,
                    local_pos: [hx, hy],
                    half_size,
                    radii: [0.0; 4],
                    kind: 0.0,
                });
                indices.extend_from_slice(&[
                    start,
                    start + 1,
                    start + 2,
                    start + 1,
                    start + 2,
                    start + 3,
                ]);
            }
        }
        x += cell_w;
    }
    x
}

pub struct ViewportSize {
    char_width: usize,
    char_height: usize,
}

/// Minimal offscreen render target — texture + view + dims. Used by the
/// layered glow path for the FG scene; the BG scene lives inside
/// `BlurChain` already.
struct SceneTarget {
    _tex: wgpu::Texture,
    view: wgpu::TextureView,
}

impl SceneTarget {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        label: &str,
    ) -> Self {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
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
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        Self { _tex: tex, view }
    }
}

/// Where an in-flight image decode is supposed to land once the worker
/// thread finishes. Two modes:
///
/// - **Deferred (Cmd-Shift-I path):** `preplaced_image_id == None`. The
///   placement hasn't been created yet; `poll_pending_images` computes
///   cell extent from the image's pixel size and calls
///   `Terminal::insert_placement` on success. Failure just logs.
///
/// - **Pre-placed (OSC 1337 path):** `preplaced_image_id == Some(id)`. The
///   placement is already in `Grid::placements` because the OSC handler
///   needed to advance the cursor synchronously. Success is a no-op (the
///   renderer's next `peek` finds the freshly-uploaded pixels); failure
///   calls `Terminal::remove_placements_with_image` to drop the orphan.
struct PendingImagePlacement {
    request: images::PendingId,
    row: isize,
    col: isize,
    preplaced_image_id: Option<images::ImageId>,
}

/// Whether `poll_pending_images` should re-arm a redraw so the render
/// loop keeps ticking until every in-flight decode resolves.
///
/// Three independent reasons to keep going:
///   - `pending_placements` still has deferred placements (Cmd-Shift-I
///     paste, OSC 1337) waiting on their decode,
///   - this poll produced `results` to act on, or
///   - the store still holds undrained decodes (`store_pending > 0`).
///
/// The last one is load-bearing for the Kitty Unicode-placeholder path
/// (`a=T,U=1`, what `icat` emits under tmux) and animation frames
/// (`a=f`): those bump the store's queue without ever touching
/// `pending_placements`. Omitting it lets a single-burst transmit whose
/// decode lands after this frame's poll stall the loop, leaving the
/// image blank until an unrelated event wakes it.
fn should_rearm_image_poll(
    pending_placements_empty: bool,
    results_empty: bool,
    store_pending: usize,
) -> bool {
    !pending_placements_empty || !results_empty || store_pending > 0
}

/// Resources shared by every window in the process: the GPU device/queue, the
/// font stack, and the pipelines/layouts that depend only on the device (and
/// the shared surface format). Built once at startup; future windows borrow
/// this instead of re-initializing the adapter, re-loading fonts, or
/// recompiling shaders. Single-threaded (the winit event loop), so the
/// not-`Send` font lives behind `Rc<RefCell>` rather than `Arc<Mutex>`.
struct AppShared {
    gpu: Rc<gpu::Gpu>,
    /// FreeType faces. `Rc<RefCell>` because faces aren't `Send` but every
    /// window lives on the one event-loop thread. Borrow discipline is
    /// load-bearing: access through [`AppShared::with_font_at`] /
    /// [`AppShared::with_font_mut_at`], never the raw `font.borrow*()`. Those
    /// accessors do two things on every entry: (1) re-tune the shared faces
    /// to the caller's `(pt, dpi)` so a second window on a different monitor
    /// can't observe glyphs sized for the first one's DPI — see
    /// [`font::Font::tune_to`]; and (2) hand the closure a `&Font` /
    /// `&mut Font` rather than a `Ref` / `RefMut`, so no borrow leaks past
    /// the call. That second part guards the overlapping borrow in
    /// `update_vertices` that would otherwise panic at runtime (and the test
    /// suite, using `Font` directly, wouldn't catch it).
    font: Rc<RefCell<font::Font>>,
    /// rustybuzz shaper, used during update_vertices to detect programming
    /// ligatures (`->`, `=>`, `!=`, …) so the renderer can draw them as a
    /// single wide glyph instead of two adjacent characters. Read-only after
    /// startup; shared like the font.
    shaper: Rc<RefCell<shaper::Shaper>>,
    render_pipeline: wgpu::RenderPipeline,
    /// Wireframe debug pipeline — same vertex shader but PolygonMode::Line
    /// and a flat-color fragment. `None` if the adapter doesn't expose
    /// POLYGON_MODE_LINE; the toggle becomes a no-op there.
    wireframe_pipeline: Option<wgpu::RenderPipeline>,
    /// Layout for the font texture + sampler. Kept so each window can build
    /// its own `font_bind_group` (and rebind after a font-size change).
    font_bind_group_layout: wgpu::BindGroupLayout,
    /// Layouts shared by the render pipeline (above) and each window's
    /// per-window camera / fade bind groups, so a window can build those
    /// against the same layout the pipeline expects.
    camera_bind_group_layout: wgpu::BindGroupLayout,
    fade_bind_group_layout: wgpu::BindGroupLayout,
    /// Dual-Kawase blur pipelines (shader compiled once per process). The
    /// per-window textures/bind-groups live in `WindowState::blur`.
    blur_pipelines: renderer::blur::BlurPipelines,
    /// Glow/bloom pipelines (shared by both the bg and fg `Glow` instances of
    /// every window). The per-window/per-layer resources live in `WindowState::glow`
    /// / `WindowState::glow_fg`.
    glow_pipelines: renderer::glow::GlowPipelines,
}

impl AppShared {
    /// Build the once-per-process resources: device/queue (already created),
    /// the font stack, the bind-group layouts, and every shader pipeline that
    /// depends only on the device + surface format. Windows are then built by
    /// [`WindowState::create_window`] against the returned `Rc<AppShared>`.
    fn new(
        gpu: gpu::Gpu,
        surface_format: wgpu::TextureFormat,
        font: font::Font,
        shaper: shaper::Shaper,
    ) -> Self {
        let gpu = Rc::new(gpu);

        let font_bind_group_layout =
            gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
                    // RGBA color-glyph (emoji) atlas.
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
                label: Some("font texture bind group layout"),
            });

        let camera_bind_group_layout =
            gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
                label: Some("camera bind group layout"),
            });

        let fade_bind_group_layout =
            gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
                label: Some("fade bind group layout"),
            });

        let shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("renderer/shader.wgsl"));

        let render_pipeline_layout =
            gpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("render pipeline layout"),
                bind_group_layouts: &[
                    &font_bind_group_layout,
                    &camera_bind_group_layout,
                    &fade_bind_group_layout,
                ],
                push_constant_ranges: &[],
            });

        let render_pipeline = gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[renderer::vertex::Vertex::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
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

        let wireframe_pipeline = if gpu
            .device
            .features()
            .contains(wgpu::Features::POLYGON_MODE_LINE)
        {
            Some(gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("wireframe pipeline"),
                layout: Some(&render_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_main",
                    buffers: &[renderer::vertex::Vertex::desc()],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs_wire",
                    targets: &[Some(wgpu::ColorTargetState {
                        format: surface_format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Line,
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
            }))
        } else {
            None
        };

        let blur_pipelines = renderer::blur::BlurPipelines::new(
            &gpu.device,
            surface_format,
            &camera_bind_group_layout,
            renderer::vertex::Vertex::desc(),
        );
        let glow_pipelines = renderer::glow::GlowPipelines::new(&gpu.device, surface_format);

        Self {
            gpu,
            font: Rc::new(RefCell::new(font)),
            shaper: Rc::new(RefCell::new(shaper)),
            render_pipeline,
            wireframe_pipeline,
            font_bind_group_layout,
            camera_bind_group_layout,
            fade_bind_group_layout,
            blur_pipelines,
            glow_pipelines,
        }
    }

    /// Re-tune the shared faces to `(pt, dpi)` and borrow the font immutably
    /// for the duration of `f`. The tune is idempotent (no-op when already
    /// matching, the common single-DPI case), and the returned reference is
    /// a `&Font` — not a `Ref` — so the borrow can't be widened past the
    /// call. Two windows on differently-scaled monitors share one `Font`;
    /// every read of metrics / cell-width / faces must funnel through here
    /// so the cache mirror and FreeType state both belong to the caller's
    /// tune. See [`font::Font::tune_to`] for the underlying contract.
    fn with_font_at<R>(&self, pt: f32, dpi: u32, f: impl FnOnce(&font::Font) -> R) -> R {
        let mut font = self.font.borrow_mut();
        font.tune_to(pt, dpi);
        // Downgrade to a shared reference for the closure — `tune_to`
        // already did the only mutation we needed.
        f(&*font)
    }

    /// Mutable counterpart to [`with_font_at`]: re-tunes, then hands `f` a
    /// `&mut Font`. Used by the few sites that mutate the font directly —
    /// `build_atlas`, `Atlas::ensure_char`, `Atlas::ensure_glyph_id` — every
    /// one of which calls `face.load_char` / `face.load_glyph`, so the
    /// pre-tune is what stops a window-A rasterization from landing into
    /// window-B's atlas with the wrong pixel size.
    fn with_font_mut_at<R>(&self, pt: f32, dpi: u32, f: impl FnOnce(&mut font::Font) -> R) -> R {
        let mut font = self.font.borrow_mut();
        font.tune_to(pt, dpi);
        f(&mut *font)
    }
}

/// One shell session and the interaction state bound to it. Everything here
/// scrolls, selects, or completes against a single PTY + `Terminal`; none of
/// it is coupled to the window's GPU surface, atlas, or buffers (so a tab can
/// later move between windows). A window owns a
/// `Vec<TabState>` and renders only the active one. First cut: exactly one tab
/// per window; the tab UI is a later follow-up.
struct TabState {
    /// Process-unique id this tab's PTY reader thread tags its events with.
    /// The event loop resolves it to the owning window via `tab_to_window`.
    tab_id: app_window::TabId,
    /// Shared buffer the reader thread appends decoded output to; the event
    /// loop drains it (capped per turn) on each `PtyInput` wake.
    pty_outbox: app_window::PtyOutbox,
    /// PTY master fd. The reader thread owns its own copy (reads + reaps);
    /// this copy lets the main thread `close()` it to unblock that read on
    /// tab close.
    master: i32,
    /// Forked child pid. Kept so tab close can `kill(child, SIGHUP)` — the
    /// blocking `read(master)` only returns once the child exits, so closing a
    /// tab running e.g. `vim` needs both `close(master)` and the signal. See
    /// [`close_tab_pty`].
    child: i32,
    terminal: terminal::Terminal,
    /// Decode + GPU residency cache for this tab's images. Per-tab: each shell
    /// has its own placements + scrollback. Mark-and-sweep eviction keyed on
    /// the live + scrollback placement set runs at the start of each `render`.
    image_store: images::Store,
    /// Cell anchors waiting on async decode, matched back to the tab's
    /// `Terminal` by `PendingId` when `Store::poll` yields the result.
    pending_placements: Vec<PendingImagePlacement>,
    scroll_y: f64,
    /// In-flight smooth slide for an explicit alt-screen scroll captured from
    /// the running app (`Terminal::take_alt_scroll`).
    alt_scroll_anim: Option<AltScrollAnim>,
    /// In-flight smooth scroll-on-output slide for the primary screen, started
    /// when new output pushes lines into scrollback on the live view
    /// (`Terminal::take_primary_scroll`).
    primary_scroll_anim: Option<PrimaryScrollAnim>,
    /// Pixel accumulator for the PTY mouse-tracking wheel path (tmux, vim,
    /// less, htop). Drained per `line_height` like `scroll_y`.
    wheel_pty_accum: f64,
    /// Drop in-flight trackpad momentum once a newer command has overridden
    /// the user's scroll intent. See `last_wheel_at`.
    scroll_suppressed: bool,
    last_wheel_at: Option<std::time::Instant>,
    /// Last cell a motion event was reported for — coalesces per-pixel motion
    /// down to per-cell transitions for the host.
    last_reported_cell: Option<(u16, u16)>,
    /// Smooth cursor motion: eases the rendered cursor quad toward the logical
    /// cursor over `config.cursor_anim_secs`. `None` off-screen / pre-first-frame.
    cursor_anim: Option<CursorAnim>,
    /// Snapshot of the previous frame's visible cells (+ a viewport key) used
    /// to spawn fade-out ghosts when the cursor retargets across deleted glyphs.
    prev_visible: Option<GridSnapshot>,
    /// Glyphs fading out at their old cell position after the cursor moved off.
    cursor_ghosts: Vec<CursorGhost>,
    /// Completion popup suggestions, recomputed only when the shell's reported
    /// input changes (path completion does disk I/O).
    completions: Vec<completion::Suggestion>,
    /// The (buffer, cursor) the cached `completions` were computed from.
    completions_input: Option<(String, usize)>,
    /// Highlighted row in the completion popup (index into `completions`).
    selected_completion: usize,
    /// First visible popup row when `completions` exceeds MAX_VISIBLE.
    completion_scroll: usize,
    /// When true the popup stays closed as the shell re-reports input — set by
    /// Enter/Esc, cleared by the next real keystroke.
    completion_dismissed: bool,
    /// Past command lines for history-based completion, most-recent-first and
    /// deduped. Seeded from $HISTFILE (OSC 2124), grown at OSC 133 `C`.
    command_history: Vec<String>,
    /// Active local text selection in (absolute_line, col) coordinates so it
    /// stays anchored to content as the grid scrolls.
    selection: Option<Selection>,
    /// Granularity for the active drag (set on press from click_count).
    selection_mode: SelectionMode,
    /// Cell where the current drag started; recomputes word/line selections as
    /// the head moves. `None` when no button is being dragged.
    press_cell: Option<(isize, usize)>,
    /// Pixel position of the mouse-down — suppresses a Cell-mode selection
    /// until the cursor moves at least DRAG_THRESHOLD_PX.
    press_pixel: Option<(f64, f64)>,
    /// Last left-button press, for multi-click detection (cell + time window).
    last_click: Option<(std::time::Instant, (isize, usize))>,
    click_count: u32,
    /// URL under the mouse while Cmd is held. Drives the underline overlay and
    /// the Cmd-click open behavior.
    hover_url: Option<HoverUrl>,
    /// Cached cell geometry (`RowVerts`) keyed by **absolute line**.
    /// `update_vertices` reuses an entry whenever that line's terminal content
    /// wasn't damaged and the cache key still matches, re-emitting only the
    /// changed lines; a line that merely scrolled is shifted, not rebuilt.
    /// Pruned to the visible abs-line range each frame. Per-tab so tabs don't
    /// share geometry.
    row_cache: std::collections::HashMap<isize, RowVerts>,
    /// The `RowCacheKey` every current `row_cache` entry was built against. A
    /// mismatch on the next frame drops the whole cache.
    row_cache_key: Option<RowCacheKey>,
    /// Last frame's selection range (absolute-line coords). Used to invalidate
    /// the rows a `selection_fg` recolor entered/left when the scheme defines a
    /// selection foreground (otherwise selection is a pure overlay and doesn't
    /// touch cached cell colors).
    prev_selection_range: Option<((isize, usize), (isize, usize))>,
}

struct WindowState {
    /// Per-window GPU surface. Declared first so it drops before `window` —
    /// the surface holds unsafe references to the window's resources.
    surface: gpu::WindowSurface,

    /// Drives swapchain presentation on a dedicated thread so the blocking
    /// vsync wait never parks the main run loop AppKit uses to draw the native
    /// tab bar. Declared before `window`: its `Drop` joins the present thread
    /// (dropping the live `wgpu::Surface`) before the window's NSView is gone.
    presenter: present::Presenter,
    /// Offscreen, surface-format targets the renderer draws into and the
    /// presenter blits to the swapchain. Recreated on resize.
    present_pool: Vec<present::PresentTarget>,
    /// Free-list bookkeeping for `present_pool`: which targets the presenter
    /// has finished with and the renderer may draw into. A frame is skipped
    /// when none are free (every target still in flight).
    free_targets: present::FreePool,
    /// A redraw was requested but deferred — either the frame-pacing throttle
    /// hadn't elapsed or no present target was free. Rather than do the ~7ms
    /// vertex rebuild now, the event loop re-arms the redraw when it's due,
    /// keeping the main thread idle (and AppKit's native tab bar responsive).
    render_pending: bool,
    /// When the last frame was actually rendered, for the [`MIN_FRAME_INTERVAL`]
    /// pacing gate.
    last_render_at: std::time::Instant,

    window: Window,

    /// Process-shared GPU device/queue, font stack, and pipelines. Held via
    /// `Rc` so every window shares one instance; declared after `surface` so
    /// the shared device outlives the surface configured against it.
    shared: Rc<AppShared>,

    /// Toggled by Cmd-Shift-W. When true, render() picks
    /// `shared.wireframe_pipeline`.
    wireframe: bool,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    /// Boundary inside `index_buffer`: indices `0..num_bg_indices` are the
    /// per-cell background quads (bg layer); `num_bg_indices..num_indices`
    /// are glyphs + cursor + selection + URL underline (fg layer). The
    /// composite pass draws them in two `draw_indexed` calls so glow can
    /// bloom each layer independently.
    num_bg_indices: u32,
    // Separate buffer for the edge-fade strip quads. Drawn in the composite
    // pass with the blur sampler bound, so the strips are filled with the
    // dual-Kawase blur of the scene rather than a flat white tint.
    strip_vertex_buffer: wgpu::Buffer,
    strip_index_buffer: wgpu::Buffer,
    num_strip_indices: u32,
    /// Set during `update_vertices` when any emitted strip samples the scene
    /// blur (`tint = 0`) — e.g. the frosted glass title-bar band. Gates the
    /// blur pass so a frame with only solid-fill strips skips it.
    strip_blur_needed: bool,
    /// Textured-quad pipeline for image placements. Owns the per-frame
    /// vertex/index buffers and is invoked once per frame between the bg
    /// and fg cell passes (when there are placements to draw).
    image_pipeline: renderer::images::ImagePipeline,
    blur: renderer::blur::BlurChain,
    /// Saturation-threshold bloom. When `glow.enabled` is true, the scene is
    /// always rendered to the offscreen `blur.scene` texture so the glow
    /// pass can sample it, even when no edge-fade strips are active. This
    /// instance handles the BG layer; `glow_fg` mirrors it for the FG
    /// layer (glyphs + cursor + selection + URL underline). Both share
    /// the same parameters and palette/foreground installations; only
    /// their source textures differ.
    glow: renderer::glow::Glow,
    /// Second Glow instance, bound to `scene_fg.view`. Always kept in
    /// param-sync with `glow` — toggling either match mode toggles both.
    glow_fg: renderer::glow::Glow,
    /// Offscreen render target for the FG layer (glyphs + cursor +
    /// overlays). Same format/dimensions as `blur.scene`. Cleared
    /// transparent before fg quads draw so the BG layer (which already
    /// landed in the swapchain) shows through everywhere fg is absent.
    scene_fg: SceneTarget,
    /// Bind group for blit_pipeline / blit_alpha_pipeline that samples
    /// `scene_fg`. Rebuilt on resize when the texture is recreated.
    scene_fg_blit_bg: wgpu::BindGroup,
    /// Mask bind groups for the masked composite path. Both glows use
    /// the bg scene as their mask so the halo paints only where the bg
    /// is transparent — preventing the glow from tinting adjacent
    /// cells' colored backgrounds. Per-glow because the mask bgl is
    /// owned per `Glow` instance.
    glow_bg_mask: wgpu::BindGroup,
    glow_fg_mask: wgpu::BindGroup,
    /// Bind group for the scanline overlay's masked variant. Binds
    /// both the bg scene (binding 0) and the fg scene (binding 2) so
    /// the shader can detect "truly empty" pixels and only suppress
    /// the overlay there. Rebuilt on resize when either texture is
    /// recreated.
    scanline_overlay_mask: wgpu::BindGroup,
    font_bind_group: wgpu::BindGroup,
    /// The actual font atlas texture. Kept around so on-demand-rasterized
    /// ligature glyphs can be uploaded incrementally via queue.write_texture
    /// without recreating the texture or bind group.
    font_texture: renderer::texture::Texture,
    /// RGBA color-glyph (emoji) atlas texture, bound at slot 2 of the font bind
    /// group. Uploaded incrementally like `font_texture` when emoji are packed.
    emoji_texture: renderer::texture::Texture,
    /// Current font size in points; mutated by Cmd-+ / Cmd--.
    pt_size: f32,
    dpi: u32,
    config: Config,
    camera: renderer::camera::Camera,
    camera_uniform: renderer::camera::CameraUniform,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    fade_buffer: wgpu::Buffer,
    fade_bind_group: wgpu::BindGroup,
    atlas: font::Atlas,
    /// The window's tabs and which one is active. Per-tab state (shell,
    /// scroll, selection, completions, images) lives in `TabState`; the
    /// window renders only `tabs[active]`. First cut: always exactly one tab.
    tabs: Vec<TabState>,
    active: usize,
    modifiers: winit::keyboard::ModifiersState,
    mouse_x: f64,
    mouse_y: f64,
    // Currently-held mouse button (in xterm code). `None` when no button down.
    held_button: Option<input::MouseButton>,
    // Cursor blink. `blink_on` is the visible phase; `last_blink` anchors the
    // timer so user input can reset it (cursor stays solid while typing).
    blink_on: bool,
    last_blink: std::time::Instant,
    /// Window focus + occlusion, tracked from winit's `Focused`/`Occluded`
    /// events. The event loop is single-threaded and shared across every
    /// window, so a backgrounded window that kept blinking its cursor or
    /// running animations would drive `request_redraw`s whose vsync-blocking
    /// renders serialize on the one thread — adding latency to whatever window
    /// the user is actually typing in. We gate blink on `focused` (an
    /// unfocused terminal shows a steady cursor, the usual convention) and
    /// skip animation/redraw entirely while `occluded`.
    focused: bool,
    occluded: bool,
    /// Edge fade animations: phase ramps 0→1 in TOP_FADE_ANIM duration as
    /// soon as the view scrolls away from the corresponding boundary, and
    /// 1→0 when it returns. Decoupled from scroll distance so the fade
    /// slides in at a constant rate regardless of scroll speed.
    top_fade_phase: f32,
    bottom_fade_phase: f32,
    last_anim_tick: std::time::Instant,
    /// The command palette overlay (Cmd-Shift-P). While `open`, it owns the
    /// keyboard: keystrokes filter/drive it instead of reaching the PTY. Holds
    /// its own search/argument text field and selection state; see
    /// `command_palette.rs`.
    command_palette: command_palette::CommandPalette,
    /// The native Liquid Glass palette panel (macOS). `None` until first
    /// opened; a no-op stub off macOS. Built lazily because it needs a
    /// main-thread AppKit context. See `glass_palette.rs`.
    glass_palette: Option<glass_palette::GlassPalette>,
    /// The find-in-scrollback overlay (Cmd-F). While `open`, it owns the
    /// keyboard like the command palette; holds the query field and the
    /// list of matches across the buffer. See `search.rs`.
    search: search::Search,
    /// The native Liquid Glass find bar (macOS). `None` until first opened; a
    /// no-op stub off macOS. The `search` model + in-terminal match highlights
    /// are unchanged — this is just the native input + counter.
    glass_find: Option<glass_find::GlassFind>,
    /// The native Liquid Glass autocomplete popup (macOS). `None` until first
    /// needed; a non-activating panel that never takes focus. Driven passively
    /// by the completion model. See `glass_complete.rs`.
    glass_complete: Option<glass_complete::GlassComplete>,
    /// Cursor anchor captured during render for positioning the native popup:
    /// `(left_x, row_top, line_height)` in physical px, or `None` when the
    /// cursor is off-screen / the popup shouldn't show.
    completion_anchor: Option<(f32, f32, f32)>,
    // Whether the pointer is currently in the title-bar band. Tracked so a
    // crossing back into the grid can restore the I-beam exactly once.
    over_toolbar: bool,
    /// Height (physical px) of the title-bar / toolbar chrome band — the region
    /// where pointer input drives the window (drag / zoom / traffic lights) and
    /// the cursor is the arrow, not the grid's I-beam. Derived from the live
    /// native title-bar height (which scales with DPI) plus a small margin, not
    /// the renderer's fixed `WINDOW_PADDING + DECORATOR_HEIGHT` reserve — see
    /// `refresh_chrome_band`. Recomputed on resize / scale-factor change.
    chrome_band_px: f64,
    /// The chrome band height (physical px) when the native tab
    /// bar is *not* shown — i.e. the title bar alone. Recorded by
    /// `refresh_chrome_band` whenever the bar is hidden; the live tab-bar height
    /// is then `chrome_band_px - titlebar_only_px`, which the grid reserves so
    /// content sits below the bar. Seeded equal to `chrome_band_px`.
    titlebar_only_px: f64,
    /// Program-set window title (OSC 0/2). When `Some` it wins over the
    /// cwd-derived title; cleared back to `None` by an empty OSC 0/2 payload.
    /// Per-window so each window tracks its own shell's title.
    manual_title: Option<String>,
    /// This window's current appearance, from `WindowEvent::ThemeChanged`.
    /// Per-window so windows on differently-themed monitors (or after an
    /// independent override) clear/redraw to their own background.
    theme: winit::window::Theme,
    /// Set by Cmd-N / the palette's "New window" action; drained by the event
    /// loop, which owns the registry + `AppShared` and actually spawns the
    /// window in-process. (A window can't build its sibling itself — it has no
    /// handle to the shared resources or the window map.)
    pending_new_window: bool,
    /// Set by Cmd-T; drained like `pending_new_window` but spawns the new
    /// window into *this* window's native tab group (shared `tabbingIdentifier`)
    /// so AppKit draws it as a tab rather than a separate window.
    pending_new_tab: bool,
    /// Set by Cmd-W; drained in the event loop, which tears this window
    /// (a native tab) down via the same path as the OS close button. Any
    /// running-command confirmation has already been resolved by the time
    /// this is set, so the event loop closes unconditionally.
    pending_close: bool,
    /// Set by `persist_and_apply` when a palette command (Set theme / Toggle
    /// follow system / etc.) mutated and saved this window's config; drained
    /// by the event loop, which fans the change out by calling `reload_config`
    /// on every other window in the registry. Without this, the inactive
    /// siblings in a native tab group keep their old palette in terminal
    /// cells, glow uniforms, `NSWindow.backgroundColor`, and effective
    /// appearance — revealing them on a tab switch then flashes the stale
    /// background for a frame before any subsequent refresh catches up.
    pending_theme_broadcast: bool,
    perf: PerfLog,
    /// Set whenever something invalidates the vertex/index buffers (PTY input,
    /// scroll, selection, blink, animation tick). Cleared by `flush_vertices`,
    /// which the redraw handler calls before drawing. Lets winit coalesce a
    /// burst of N events into one rebuild + one frame.
    vertices_dirty: bool,
    /// Set (via `invalidate_scroll`) when only the global scroll offset eased
    /// and grid content is unchanged. `flush_vertices` then slides the existing
    /// geometry with a camera-uniform write instead of rebuilding every cell.
    /// `vertices_dirty` outranks it — a content change always forces the full
    /// rebuild.
    scroll_only_dirty: bool,
    /// Monotonic generation for the per-row vertex cache. Bumped whenever the
    /// atlas is rebuilt (font-size / DPI change moves glyph UVs) or the palette
    /// swaps (cell colors change) — both make every cached `RowVerts` stale.
    /// Folded into each tab's `RowCacheKey`, so a bump drops all row caches.
    row_cache_epoch: u64,
}

const DOUBLE_CLICK_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(500);

/// Burst-scoped timing aggregator. Accumulates work caused by a run of PTY
/// chunks + the frames that draw them, then prints a one-line summary once
/// the activity has settled (>= PERF_FLUSH_IDLE since the last sample).
/// Disabled unless `PERFLOG=1` is set in the environment so the steady-state
/// terminal stays quiet.
const PERF_FLUSH_IDLE: std::time::Duration = std::time::Duration::from_millis(150);

struct PerfLog {
    enabled: bool,
    burst_start: Option<std::time::Instant>,
    last_event: std::time::Instant,
    pty_chunks: u32,
    pty_bytes: usize,
    feed_ns: u128,
    update_ns: u128,
    update_calls: u32,
    // Phase split of update_vertices: `shape` = the ligature/shaping pass +
    // atlas re-upload; `body` = the rest (per-cell bg/fg quad emission +
    // overlays). Prologue = update_ns - shape - body.
    up_shape_ns: u128,
    up_body_ns: u128,
    render_ns: u128,
    render_calls: u32,
    fast_calls: u32,
    fast_ns: u128,
    slow_calls: u32,
    slow_ns: u128,
    surface_wait_ns: u128,
}

impl PerfLog {
    fn new() -> Self {
        Self {
            enabled: std::env::var("PERFLOG").map(|v| !v.is_empty() && v != "0").unwrap_or(false),
            burst_start: None,
            last_event: std::time::Instant::now(),
            pty_chunks: 0,
            pty_bytes: 0,
            feed_ns: 0,
            update_ns: 0,
            update_calls: 0,
            up_shape_ns: 0,
            up_body_ns: 0,
            render_ns: 0,
            render_calls: 0,
            fast_calls: 0,
            fast_ns: 0,
            slow_calls: 0,
            slow_ns: 0,
            surface_wait_ns: 0,
        }
    }

    fn note_pty(&mut self, bytes: usize, feed: std::time::Duration) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        if self.burst_start.is_none() {
            self.burst_start = Some(now);
        }
        self.last_event = now;
        self.pty_chunks += 1;
        self.pty_bytes += bytes;
        self.feed_ns += feed.as_nanos();
    }

    fn note_update(&mut self, dur: std::time::Duration) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        if self.burst_start.is_none() {
            self.burst_start = Some(now);
        }
        self.last_event = now;
        self.update_ns += dur.as_nanos();
        self.update_calls += 1;
    }

    /// Record the shape/body phase split of one `update_vertices` call.
    fn note_update_phases(&mut self, shape: std::time::Duration, body: std::time::Duration) {
        if !self.enabled {
            return;
        }
        self.up_shape_ns += shape.as_nanos();
        self.up_body_ns += body.as_nanos();
    }

    fn note_render(
        &mut self,
        dur: std::time::Duration,
        surface_wait: std::time::Duration,
        fast: bool,
    ) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        if self.burst_start.is_none() {
            self.burst_start = Some(now);
        }
        self.last_event = now;
        self.render_ns += dur.as_nanos();
        self.render_calls += 1;
        self.surface_wait_ns += surface_wait.as_nanos();
        if fast {
            self.fast_calls += 1;
            self.fast_ns += dur.as_nanos();
        } else {
            self.slow_calls += 1;
            self.slow_ns += dur.as_nanos();
        }
    }

    /// Wake-up time the event loop should arm to so we can print the summary
    /// soon after the burst goes quiet. `None` when no burst is pending.
    fn next_wake(&self) -> Option<std::time::Instant> {
        if !self.enabled || self.burst_start.is_none() {
            return None;
        }
        Some(self.last_event + PERF_FLUSH_IDLE)
    }

    fn maybe_flush(&mut self) {
        if !self.enabled || self.burst_start.is_none() {
            return;
        }
        let now = std::time::Instant::now();
        if now.duration_since(self.last_event) < PERF_FLUSH_IDLE {
            return;
        }
        let total = now.duration_since(self.burst_start.unwrap());
        let accounted_ns = self.feed_ns + self.update_ns + self.render_ns;
        let avg_ns = |total_ns: u128, n: u32| {
            if n == 0 { 0.0 } else { total_ns as f64 / n as f64 / 1e6 }
        };
        eprintln!(
            "[perf] burst {:>6.1}ms wall | pty {:>2}c {:>6}B | feed {:>5.2}ms | update {:>6.2}ms x{:>2} (shape {:>6.2} / body {:>6.2}) | render {:>6.2}ms x{:>2} (fast x{:>2} avg{:>4.2} / slow x{:>2} avg{:>4.2}) | swait {:>6.2}ms | acc {:>4.1}%",
            total.as_secs_f64() * 1e3,
            self.pty_chunks,
            self.pty_bytes,
            self.feed_ns as f64 / 1e6,
            self.update_ns as f64 / 1e6,
            self.update_calls,
            self.up_shape_ns as f64 / 1e6,
            self.up_body_ns as f64 / 1e6,
            self.render_ns as f64 / 1e6,
            self.render_calls,
            self.fast_calls,
            avg_ns(self.fast_ns, self.fast_calls),
            self.slow_calls,
            avg_ns(self.slow_ns, self.slow_calls),
            self.surface_wait_ns as f64 / 1e6,
            if total.as_nanos() > 0 {
                (accounted_ns as f64 / total.as_nanos() as f64) * 100.0
            } else {
                0.0
            },
        );
        self.burst_start = None;
        self.pty_chunks = 0;
        self.pty_bytes = 0;
        self.feed_ns = 0;
        self.update_ns = 0;
        self.update_calls = 0;
        self.up_shape_ns = 0;
        self.up_body_ns = 0;
        self.render_ns = 0;
        self.render_calls = 0;
        self.fast_calls = 0;
        self.fast_ns = 0;
        self.slow_calls = 0;
        self.slow_ns = 0;
        self.surface_wait_ns = 0;
    }
}

/// Minimum pixel distance the mouse must travel after mouse-down before a
/// Cell-mode drag begins to paint a selection. Below this, a press-and-release
/// counts as a plain click and never flashes a single-cell highlight.
const DRAG_THRESHOLD_PX: f64 = 4.0;

#[derive(Copy, Clone, Debug, PartialEq)]
enum SelectionMode {
    Cell,
    Word,
    Line,
}

/// Classification of a single corner of a selection strip relative to the
/// strip in the row above (for top corners) or below (for bottom corners).
/// `Convex` rounds outward; `Concave` is an inner step that gets a fillet
/// quad in the unselected quadrant; `Straight` is on a continuous edge.
#[derive(Copy, Clone, Debug, PartialEq)]
enum CornerType {
    Convex,
    Straight,
    Concave,
}

/// Pick a corner type given this strip's column adjacent to the corner and
/// the neighbor strip's range, when looking at the LEFT side of either strip
/// (TL and BL corners). For RIGHT side (TR/BR), call with `mirror = true`
/// so the same logic applies symmetrically.
fn classify_corner_with_neighbor(
    col: usize,
    neighbor: Option<(usize, usize)>,
    side: HorizSide,
) -> CornerType {
    let Some((nf, nt)) = neighbor else {
        return CornerType::Convex;
    };
    match side {
        HorizSide::Left => {
            // The corner sits at `col`. Neighbor "covers further left" if its
            // strip starts before `col` (i.e., includes col - 1).
            let covers_outer = nf < col;
            // Neighbor "covers the same column" if `col` is inside its range.
            let covers_at = nf <= col && col <= nt;
            if covers_outer && covers_at {
                CornerType::Concave
            } else if covers_at {
                CornerType::Straight
            } else {
                CornerType::Convex
            }
        }
        HorizSide::Right => {
            // Mirror image: outer side is "to the right of `col`".
            let covers_outer = nt > col;
            let covers_at = nf <= col && col <= nt;
            if covers_outer && covers_at {
                CornerType::Concave
            } else if covers_at {
                CornerType::Straight
            } else {
                CornerType::Convex
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum HorizSide {
    Left,
    Right,
}

/// Logical-point offset applied to each cascaded window, matching the macOS
/// convention of stepping a new window down-and-right from its parent. Roughly
/// a title-bar height so successive windows stack like a fanned deck. Applied
/// by [`spawn_window_in_process`] off the spawning window's live position.
const WINDOW_CASCADE_STEP: f64 = 28.0;

const BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const ANIM_FRAME: std::time::Duration = std::time::Duration::from_millis(16);

/// Minimum wall-clock spacing between main-thread frame renders (~62fps). The
/// present thread vsync-paces what actually reaches the display, so rendering
/// faster than this on the main thread just burns CPU and — crucially — starves
/// AppKit's run loop, leaving no gap to repaint the native tab bar or service a
/// tab switch. Pacing renders here guarantees that gap even under a flood of
/// output. Text at 62fps is indistinguishable from higher rates.
pub(crate) const MIN_FRAME_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(16);

/// Max bytes of buffered PTY output fed to the terminal in a single event-loop
/// turn. A flood (e.g. `seq 1 2000000`) can queue tens of MB; feeding it all at
/// once pins the main thread for seconds, freezing renders and input. Capping
/// the slice lets the loop feed ≤this, repaint, service a click/keystroke, then
/// self-wake for the rest — so the UI stays live through the burst. ~128 KiB is
/// roughly one frame's worth of parse work at observed feed throughput.
pub(crate) const PTY_FEED_CAP: usize = 128 * 1024;
/// Duration of the smooth-scroll slide for an explicit alt-screen scroll
/// (SU/SD/line-feed) captured from the running app. Kept short so the terminal
/// stays responsive — the final frame is reached this many seconds after the
/// scroll lands, regardless of distance.
///
/// EXPERIMENTAL: set to 0 to disable the alt-screen slide entirely. With it
/// off, `maybe_start_alt_scroll` returns early and the scroll snaps into place
/// instead of animating.
const ALT_SCROLL_ANIM_SECS: f32 = 0.0;

/// Maximum displayed offset, in whole line-heights, for the primary
/// scroll-on-output slide. The renderer widens its phantom row band by
/// `ceil(|scroll_y| / line_height)` per side, and `grid_buffer_byte_sizes`
/// reserves exactly enough GPU buffer for the fixed ±2 strips plus this much
/// extra widening — so the cap and the buffer sizing must move together. A
/// 1-line push (the common case) slides fully; larger bursts slide this far
/// then snap the rest, which beats an unreadable full-distance blur.
const SCROLL_ON_OUTPUT_MAX_ROWS: usize = 16;

/// An in-flight alt-screen scroll animation. `total_px` is the full slide
/// distance; the rendered offset eases from `total_px` down to 0 over
/// `ALT_SCROLL_ANIM_SECS`. `up` mirrors the captured scroll direction and
/// picks the sign of the offset applied to `scroll_y`.
#[derive(Copy, Clone)]
struct AltScrollAnim {
    up: bool,
    rows: usize,
    region_top: usize,
    region_bottom: usize,
    total_px: f32,
    started: std::time::Instant,
}

/// An in-flight smooth scroll-on-output slide for the primary screen. When new
/// output pushes lines into scrollback on the live view, `scroll_y` is set to
/// `total_px` (the departing rows fill the gap, drawn from real scrollback) and
/// eased back to 0 over `config.scroll_on_output_secs`. Always an upward slide,
/// so unlike `AltScrollAnim` there's no direction flag.
#[derive(Copy, Clone)]
struct PrimaryScrollAnim {
    total_px: f32,
    started: std::time::Instant,
}

/// Eased cursor position in cell-space (col, visual_row) floats. Lerp-with-
/// retarget chase: when the logical cursor moves while an ease is still in
/// flight, `from` is rebased to the currently-displayed position so the new
/// segment starts where the eye last saw the quad.
#[derive(Copy, Clone)]
struct CursorAnim {
    from: (f32, f32),
    to: (f32, f32),
    started_at: std::time::Instant,
}

impl CursorAnim {
    fn snapped(target: (f32, f32)) -> Self {
        Self {
            from: target,
            to: target,
            started_at: std::time::Instant::now(),
        }
    }

    /// Smoothstep `t*t*(3 - 2t)` — symmetric ease-in-out, no overshoot.
    /// Snaps `from = to` once elapsed crosses `duration` so the next
    /// `animating()` call returns false — without that snap the event loop
    /// could stop ticking with the last drawn frame at `t < duration`
    /// (cursor a few pixels short of target) because the previous
    /// `WaitUntil` landed after the animation ended.
    fn current(&mut self, duration: f32) -> (f32, f32) {
        if duration <= 0.0 || self.started_at.elapsed().as_secs_f32() >= duration {
            self.from = self.to;
            return self.to;
        }
        let t = self.started_at.elapsed().as_secs_f32() / duration;
        let e = t * t * (3.0 - 2.0 * t);
        (
            self.from.0 + (self.to.0 - self.from.0) * e,
            self.from.1 + (self.to.1 - self.from.1) * e,
        )
    }

    /// "Still chasing": `from != to`. Stays true even past `duration`
    /// until a render calls `current()` and snaps `from = to`, so the
    /// event loop is guaranteed to render at least one frame past the
    /// end of the ease (where `current()` returns `to`) before parking.
    fn animating(&self, _duration: f32) -> bool {
        (self.from.0 - self.to.0).abs() > f32::EPSILON
            || (self.from.1 - self.to.1).abs() > f32::EPSILON
    }

    /// Point the ease at a new target, rebasing `from` to whatever is
    /// currently rendered so the motion is continuous.
    fn retarget(&mut self, new_target: (f32, f32), duration: f32) {
        if (new_target.0 - self.to.0).abs() < f32::EPSILON
            && (new_target.1 - self.to.1).abs() < f32::EPSILON
        {
            return;
        }
        self.from = self.current(duration);
        self.to = new_target;
        self.started_at = std::time::Instant::now();
    }
}

/// Identifies which viewport a `GridSnapshot` was taken from. Mismatch on
/// any field means cell coordinates aren't comparable across frames (the
/// whole grid was repainted), so ghost detection is skipped.
#[derive(Copy, Clone, PartialEq, Eq)]
struct ViewportKey {
    rows: usize,
    cols: usize,
    view_offset: usize,
    on_alt_screen: bool,
}

struct GridSnapshot {
    cells: Vec<Vec<style::Cell>>,
    key: ViewportKey,
}

/// Cached cell geometry for one line: the background quads and the foreground
/// glyph quads, as raw vertices (indices are regenerated at assembly since
/// they're position-dependent). Both `Vec`s are a multiple of 4 vertices — one
/// quad each. Reused frame-to-frame for lines whose rendered content didn't
/// change (see `Terminal::row_damage`), so a keystroke only re-emits the
/// cursor's line instead of every visible cell.
///
/// Keyed by **absolute line** (stable as content scrolls into scrollback), so
/// a scroll reuses every unchanged line. `baked_row` records the visual row the
/// vertices' `y` was last positioned for; when the line lands on a different
/// visual row (it scrolled), the cached vertices are shifted by the row delta
/// instead of re-emitted.
#[derive(Clone)]
struct RowVerts {
    bg: Vec<renderer::vertex::Vertex>,
    fg: Vec<renderer::vertex::Vertex>,
    baked_row: isize,
}

/// Identifies the geometry assumptions a `RowVerts` was built under. Any change
/// invalidates every cached row: `viewport` covers grid size / scroll / screen,
/// `anim_active` covers the alt-screen slide (which bakes a per-row offset into
/// the geometry, making it frame-dependent), and `epoch` covers atlas rebuilds
/// (font-size / DPI change → glyph UVs move) and palette swaps (cell colors
/// change). When this key changes the whole `row_cache` is dropped.
#[derive(Clone, PartialEq)]
struct RowCacheKey {
    viewport: ViewportKey,
    anim_active: bool,
    epoch: u64,
}

/// A glyph being faded out at its old cell position to bridge the gap
/// between an instantaneous cell clear (e.g. backspace overwriting with a
/// space) and the cursor's animated slide across that cell. Stored in
/// buffer-row coordinates so the ghost stays anchored to the underlying
/// cell when the user scrolls; the visual row is recomputed each frame
/// from the current `live_grid_offset`.
struct CursorGhost {
    ch: char,
    style: style::Style,
    buffer_row: usize,
    col: usize,
    started_at: std::time::Instant,
}

fn is_blank_cell(cell: &style::Cell) -> bool {
    matches!(cell.ch, ' ' | '\0')
}

/// Cell-range selection in absolute-line coordinates. `anchor` is where the
/// drag started, `head` is where it currently is — they may be in either
/// order, so callers normalize via `range()` before iterating.
#[derive(Copy, Clone, Debug)]
struct Selection {
    anchor: (isize, usize),
    head: (isize, usize),
}

impl Selection {
    /// Endpoints in (start, end) reading order, inclusive on both ends.
    fn range(&self) -> ((isize, usize), (isize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

}

impl WindowState {
    /// Build one window against the shared, already-constructed `AppShared`,
    /// adopting `initial_tab` as its (sole, for now) tab. Builds only the
    /// per-window resources: the glyph atlas + font texture/bind-group, camera
    /// + fade uniforms/bind-groups, vertex/index buffers, and the blur/glow
    /// textures. Reused verbatim by Cmd-N (Stage 4) and tab tear-off (later).
    fn create_window(
        shared: Rc<AppShared>,
        window: Window,
        surface: gpu::WindowSurface,
        // The live surface, handed straight to this window's present thread.
        surface_raw: wgpu::Surface,
        config: Config,
        dpi: u32,
        initial_tab: TabState,
    ) -> Self {
        let _sw = std::time::Instant::now();
        let _timing = std::env::var_os("YUTANI_STARTUP_TIMING").is_some();
        macro_rules! sub { ($l:expr) => { if _timing { eprintln!("[startup]   ... create_window {:>7.1}ms  {}", _sw.elapsed().as_secs_f64()*1000.0, $l); } } }
        let pt_size = config.font_size;
        let window_theme = window.theme().unwrap_or(winit::window::Theme::Light);

        // Per-window glyph atlas, rasterized on demand from the shared faces.
        // Tune to this window's (pt, dpi) first so the initial pack matches
        // its monitor's scale even if another window built last at a
        // different DPI.
        let atlas = shared.with_font_mut_at(pt_size, dpi, |f| f.build_atlas());
        sub!("build_atlas");

        let font_texture = renderer::texture::Texture::from_memory(
            &shared.gpu.device,
            &shared.gpu.queue,
            &atlas.buffer,
            atlas.width as u32,
            atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture"),
        );

        // RGBA color-glyph (emoji) atlas. sRGB so the BGRA samples decode to
        // linear before the premultiplied-alpha blend, matching how the
        // monochrome text path feeds linear color into the sRGB scene target.
        let emoji_texture = renderer::texture::Texture::from_memory(
            &shared.gpu.device,
            &shared.gpu.queue,
            &atlas.color_buffer,
            atlas.color_width as u32,
            atlas.color_height as u32,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            Some("emoji texture"),
        );

        let font_bind_group = shared.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &shared.font_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&font_texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&font_texture.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&emoji_texture.view),
                },
            ],
            label: Some("font bind group"),
        });

        let camera = renderer::camera::Camera {};
        let mut camera_uniform = renderer::camera::CameraUniform::new();
        camera_uniform.update_view_proj(&camera, surface.config.width as f32, surface.config.height as f32);

        let camera_buffer = shared.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("camera buffer"),
            contents: bytemuck::cast_slice(&[camera_uniform]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let camera_bind_group = shared.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &shared.camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
            label: Some("camera bind group"),
        });

        // Edge-fade uniform: layout matches FadeUniform in shader.wgsl —
        // top.xy + bottom.xy + viewport.xy + bg_uv.xy + params.xy = 5*vec4 =
        // 80 bytes. `params.x` carries the text-gamma exponent (1/text_gamma).
        let fade_buffer = shared.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fade uniform"),
            size: 80,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let fade_bind_group = shared.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &shared.fade_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: fade_buffer.as_entire_binding(),
            }],
            label: Some("fade bind group"),
        });

        // Buffers are sized to the adopted tab's grid so the vertex builder
        // (which iterates `terminal.cols/rows`) can't overrun them. The tab
        // was created at this window's computed viewport (see `run()` /
        // Cmd-N), so these dims match.
        let cols = initial_tab.terminal.cols;
        let rows = initial_tab.terminal.rows;
        // Each cell contributes two quads (background + glyph) = 8 verts.
        // Slack covers four phantom rows (two top + two bottom) used during
        // smooth scrolling, the cursor quad, and the two edge-fade quads.
        // The exact formula lives in `grid_buffer_byte_sizes` so the init
        // and resize paths can't drift apart (which they did — pre-fix the
        // resize path allocated half the slack, and the next time
        // `update_vertices` ran near a scroll edge, `queue.write_buffer`
        // panicked with a "Copy ... would end up overrunning" validation
        // error).
        // No native tab bar exists yet at construction (the chrome band is
        // seeded title-bar-only below), so the top inset is zero here; the
        // bar's reflow drives `resize_buffers`, which re-sizes with the inset.
        let (vbuf_bytes, ibuf_bytes) =
            grid_buffer_byte_sizes(cols, rows, 0);
        let vertex_buf: Vec<u8> = vec![0; vbuf_bytes];
        let vertex_buffer = shared.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: &bytemuck::cast_slice(&vertex_buf),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let index_buf: Vec<u8> = vec![0; ibuf_bytes];
        let index_buffer = shared.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("index buffer"),
            contents: &bytemuck::cast_slice(&index_buf),
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        });

        // Strip quads: the soft top edge is emitted as several gradient
        // segments, the bottom fade, plus the two frosted glass-titlebar band
        // quads. Sized for up to 32 quads (128 vertices, 192 indices) —
        // comfortably above the current segment count so resize never reallocs.
        let strip_vertex_buffer = shared.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strip vertex buffer"),
            size: (128 * std::mem::size_of::<renderer::vertex::Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let strip_index_buffer = shared.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strip index buffer"),
            size: (192 * std::mem::size_of::<u16>()) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let blur = renderer::blur::BlurChain::new(
            &shared.gpu.device,
            &shared.blur_pipelines,
            surface.config.width,
            surface.config.height,
        );
        blur.write_uniforms(&shared.gpu.queue, surface.config.width, surface.config.height);
        sub!("BlurChain::new");

        let mut glow = renderer::glow::Glow::new(
            &shared.gpu.device,
            &shared.glow_pipelines,
            surface.config.width,
            surface.config.height,
            &blur.scene.view,
        );
        // Second offscreen scene for the FG layer (glyphs + cursor + overlays).
        // Same format/size as `blur.scene` so the same blit and glow shaders
        // can sample either one without pipeline divergence.
        let scene_fg = SceneTarget::new(
            &shared.gpu.device,
            surface.config.format,
            surface.config.width,
            surface.config.height,
            "scene fg",
        );
        let scene_fg_blit_bg =
            shared.blur_pipelines.make_blit_bind_group(&shared.gpu.device, &scene_fg.view, "scene fg blit bg");

        // Two Glow instances — one bound to the BG scene, one to the FG
        // scene. Identical params/palette/foreground, written below.
        let mut glow_fg = renderer::glow::Glow::new(
            &shared.gpu.device,
            &shared.glow_pipelines,
            surface.config.width,
            surface.config.height,
            &scene_fg.view,
        );
        sub!("Glow::new x2 + scene_fg");
        let initial_overrides = palette::get().glow;
        for g in [&mut glow, &mut glow_fg] {
            apply_glow_config(g, &config, &initial_overrides);
        }
        // Bright-ANSI matching needs the palette's hue table; foreground
        // matching needs the foreground RGB. Palette is installed before
        // WindowState::new (see `run()`), so this reads the active scheme — or
        // the defaults if no scheme was configured.
        {
            let p = palette::get();
            let bright: [[f32; 4]; 8] = [
                p.ansi[8], p.ansi[9], p.ansi[10], p.ansi[11],
                p.ansi[12], p.ansi[13], p.ansi[14], p.ansi[15],
            ];
            for g in [&mut glow, &mut glow_fg] {
                g.set_bright_palette(&shared.gpu.queue, &bright);
                g.set_foreground(p.foreground);
                // Masked composite needs the window bg colour to detect
                // colored cells in the mask texture.
                g.set_background(p.background);
                // Scale the CRT scanline period and bloom radius to this
                // window's backing scale so they look the same size regardless
                // of monitor DPI. `write_uniforms` (below) reads it for the
                // bloom kernel; `write_glow_params` reads it for the period.
                g.set_dpi_scale(dpi_px(1.0, dpi));
            }
        }
        for g in [&glow, &glow_fg] {
            g.write_uniforms(&shared.gpu.queue, surface.config.width, surface.config.height);
            g.write_glow_params(&shared.gpu.queue);
        }
        // Both glows mask against the bg scene: the halo only appears
        // where bg is transparent (the window's default background), so
        // it can't paint over colored cell backgrounds and visually
        // shift their apparent colour.
        let glow_bg_mask = shared.glow_pipelines.make_mask_bind_group(
            &shared.gpu.device,
            &blur.scene.view,
            "glow bg mask (bg scene)",
        );
        let glow_fg_mask = shared.glow_pipelines.make_mask_bind_group(
            &shared.gpu.device,
            &blur.scene.view,
            "glow fg mask (bg scene)",
        );
        // Scanline overlay's masked mask samples both layers so it can
        // tell glyphs on default-bg cells from truly empty pixels.
        let scanline_overlay_mask = shared.glow_pipelines.make_overlay_mask_bind_group(
            &shared.gpu.device,
            &blur.scene.view,
            &scene_fg.view,
            "scanline overlay mask (bg + fg)",
        );

        // Image pipeline — drawn into `blur.scene` between bg cells and the
        // fg layer, so images participate in glow + edge blur the same way
        // colored bg cells do.
        let image_pipeline = renderer::images::ImagePipeline::new(
            &shared.gpu.device,
            surface.config.format,
            &shared.camera_bind_group_layout,
        );
        sub!("ImagePipeline::new");

        // Present-source pool + the thread that owns the live surface and does
        // the blocking acquire/present off the main thread.
        let present_pool: Vec<present::PresentTarget> = (0..present::POOL_SIZE)
            .map(|i| {
                present::PresentTarget::new(
                    &shared.gpu.device,
                    surface.config.format,
                    surface.config.width,
                    surface.config.height,
                    &format!("present source {i}"),
                )
            })
            .collect();
        let present_views = present_pool.iter().map(|t| t.view.clone()).collect();
        let presenter = present::Presenter::spawn(
            shared.gpu.device.clone(),
            shared.gpu.queue.clone(),
            surface_raw,
            surface.config.clone(),
            present_views,
        );
        sub!("Presenter::spawn");

        Self {
            surface,
            presenter,
            present_pool,
            free_targets: present::FreePool::new(present::POOL_SIZE),
            render_pending: false,
            // In the past, so the first frame paints immediately.
            last_render_at: std::time::Instant::now() - MIN_FRAME_INTERVAL,
            window,
            shared,
            atlas,
            wireframe: false,
            vertex_buffer,
            index_buffer,
            num_indices: 0,
            num_bg_indices: 0,
            strip_vertex_buffer,
            strip_index_buffer,
            num_strip_indices: 0,
            strip_blur_needed: false,
            image_pipeline,
            blur,
            glow,
            glow_fg,
            scene_fg,
            scene_fg_blit_bg,
            glow_bg_mask,
            glow_fg_mask,
            scanline_overlay_mask,
            font_bind_group,
            font_texture,
            emoji_texture,
            pt_size,
            dpi,
            config,
            camera,
            camera_uniform,
            camera_buffer,
            camera_bind_group,
            fade_buffer,
            fade_bind_group,
            tabs: vec![initial_tab],
            active: 0,
            modifiers: winit::keyboard::ModifiersState::empty(),
            mouse_x: 0.0,
            mouse_y: 0.0,
            held_button: None,
            blink_on: true,
            last_blink: std::time::Instant::now(),
            // A freshly-spawned window comes up key/visible; winit will correct
            // either flag via Focused/Occluded if that's not so.
            focused: true,
            occluded: false,
            top_fade_phase: 0.0,
            bottom_fade_phase: 0.0,
            last_anim_tick: std::time::Instant::now(),
            command_palette: command_palette::CommandPalette::default(),
            glass_palette: None,
            search: search::Search::default(),
            glass_find: None,
            glass_complete: None,
            completion_anchor: None,
            over_toolbar: false,
            // Seeded with the renderer's reserve; refresh_chrome_band() below
            // (and on every resize / scale change) replaces it with the real
            // DPI-scaled native title-bar height.
            chrome_band_px: (WINDOW_PADDING + DECORATOR_HEIGHT) as f64,
            titlebar_only_px: (WINDOW_PADDING + DECORATOR_HEIGHT) as f64,
            manual_title: None,
            theme: window_theme,
            pending_new_window: false,
            pending_new_tab: false,
            pending_close: false,
            pending_theme_broadcast: false,
            perf: PerfLog::new(),
            vertices_dirty: true,
            scroll_only_dirty: false,
            row_cache_epoch: 0,
        }
    }

    /// Invalidate every tab's per-row vertex cache by bumping the epoch. Call
    /// after anything that changes cached cell geometry independently of the
    /// terminal's own damage tracking: an atlas rebuild (font size / DPI moves
    /// glyph UVs) or a palette/scheme swap (cell colors change). The next
    /// `update_vertices` sees the epoch mismatch and re-emits all rows.
    fn invalidate_row_cache(&mut self) {
        self.row_cache_epoch = self.row_cache_epoch.wrapping_add(1);
    }

    /// The active tab (read-only). First cut: always `tabs[0]`.
    #[inline]
    fn active_tab(&self) -> &TabState {
        &self.tabs[self.active]
    }

    /// The active tab (mutable).
    #[inline]
    fn active_tab_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active]
    }

    /// Whether this window is genuinely off-screen and so safe to skip
    /// rendering. A *focused* window is always the foreground native tab, hence
    /// visible — even if a stale `occluded` flag still says otherwise. macOS
    /// coalesces and delays occlusion notifications, so on a rapid A→B→A tab
    /// switch the `Occluded(true)` from the first switch can land on window A
    /// *after* it's been re-selected, leaving `occluded` wrongly true. Gating on
    /// `!focused` makes that ordering irrelevant: the foreground tab keeps
    /// rendering regardless, so the switch never appears to stall.
    fn hidden(&self) -> bool {
        self.occluded && !self.focused
    }

    /// Mark the vertex buffer stale and ask winit to redraw. Repeated calls
    /// inside one event-loop turn coalesce into a single RedrawRequested,
    /// and `surface.get_current_texture()` blocks at the swapchain to keep
    /// us aligned with the display's vsync cadence.
    fn invalidate(&mut self) {
        self.vertices_dirty = true;
        // While a frame is already deferred (waiting on the pacing deadline /
        // present pool), don't re-request a redraw on every PTY chunk — that
        // floods the loop with cheap no-op RedrawRequested round-trips.
        // `about_to_wait` arms exactly one redraw when the deferred frame is
        // due, so the marked-dirty state is picked up then.
        if !self.render_pending {
            self.window.request_redraw();
        }
    }

    /// Render one frame immediately, bypassing the frame-pacing and hidden-
    /// window guards in the normal `RedrawRequested` path. Used by the theme
    /// broadcast: after `reload_config` re-pushes the new palette into every
    /// hidden sibling's terminal cells, glow uniforms, and
    /// `NSWindow.backgroundColor`, the sibling's wgpu surface still holds
    /// the previously rendered frame painted from the *old* palette. Revealing
    /// the window on a tab switch composites that stale drawable for a frame
    /// before the next `RedrawRequested` catches up — a jarring background
    /// flash on dark↔light flips. Re-rendering now lands the new frame on
    /// the IOSurface so the reveal composites the right theme from frame
    /// zero. No-op when no present target is free; the next user interaction
    /// would replace the stale frame on its own anyway.
    pub(crate) fn render_now(&mut self) {
        if !self.present_target_available() {
            return;
        }
        self.render_pending = false;
        self.last_render_at = std::time::Instant::now();
        self.update();
        self.prepare_frame();
        let _ = self.render(clear_color(self.theme));
    }

    /// Whether a present-source target is free to render into right now. Drains
    /// any the present thread has finished with first. The redraw handler calls
    /// this *before* the vertex rebuild so a frame that would just be dropped
    /// (every target still in flight) costs nothing on the main thread.
    fn present_target_available(&mut self) -> bool {
        self.presenter.drain_free(&mut self.free_targets);
        self.free_targets.has_free()
    }

    /// Like `invalidate`, but for a frame where only the global scroll offset
    /// eased (scroll-on-output slide, scrollback smooth-scroll) and the grid
    /// *content* is unchanged. The renderer can then slide the existing
    /// geometry via the camera uniform instead of rebuilding every cell — see
    /// `WindowState::refresh_scroll_uniforms`. A pending full rebuild wins, so
    /// this never downgrades a content invalidation.
    fn invalidate_scroll(&mut self) {
        // The command palette / find overlays are screen-fixed but live in the
        // scrolled geometry buffer with a baked camera-offset compensation, so
        // they must be rebuilt as the offset eases — take the full path while
        // either is open. (The cursor-anchored completion popup rides the
        // camera correctly and doesn't need this.)
        if self.command_palette.open || self.search.open {
            self.vertices_dirty = true;
        } else if !self.vertices_dirty {
            self.scroll_only_dirty = true;
        }
        self.window.request_redraw();
    }

    /// Per-frame setup. Runs in the redraw handler between `update` (input
    /// processing) and `render` (GPU encode). Resolves the image store's
    /// async state, then rebuilds vertices if stale.
    ///
    /// The ordering matters: poll → retain → vertex build → render. Both
    /// the vertex builder (half-block fallback path) and `render` (GPU
    /// image_draws) consult `Store::peek` for the same set of placements.
    /// If they disagreed on `peek`'s return value within a frame, a decode
    /// that resolved between the two would either double-draw (half-block
    /// behind GPU pixels) or vanish for a frame (vertex builder saw Some,
    /// then mark-and-sweep dropped the slot before render). Doing both
    /// store mutations before either consumer reads guarantees one snapshot
    /// per frame.
    ///
    /// **Invariant:** no caller may mutate `terminal.placements` between
    /// `prepare_frame` and `render` — `retain` has already been computed
    /// against the snapshot we hand to `render`, and any new placement
    /// would slip past it.
    fn prepare_frame(&mut self) {
        // Resolve worker decodes that completed since the last frame. May
        // call `insert_placement` (deferred path success) or
        // `remove_placements_with_image` (pre-placed path failure), both of
        // which mark vertices dirty internally.
        self.poll_pending_images();
        // Mark-and-sweep AFTER poll so a freshly-landed image referenced
        // by a placement created in this same `poll_pending_images` call
        // is kept alive.
        let referenced = self.active_tab().terminal.referenced_image_ids();
        self.active_tab_mut().image_store.retain(&referenced);
        self.flush_vertices();
    }

    /// Rebuild the vertex/index buffers if they're stale, recording the
    /// cost in `perf`. Called from `prepare_frame`; production code should
    /// not call this directly — the image-store snapshot has to be set up
    /// first.
    fn flush_vertices(&mut self) {
        if self.vertices_dirty {
            let t = std::time::Instant::now();
            self.update_vertices();
            self.perf.note_update(t.elapsed());
            self.vertices_dirty = false;
            self.scroll_only_dirty = false;
        } else if self.scroll_only_dirty {
            // Content unchanged; only the global scroll offset eased. Slide the
            // existing geometry via the camera + refresh the edge fades —
            // skipping the per-cell rebuild entirely.
            self.refresh_scroll_uniforms();
            self.scroll_only_dirty = false;
        }
    }

    fn get_viewport_size(
        width: f32,
        height: f32,
        advance_x: usize,
        line_height: usize,
        // Extra top reserve for the native tab bar when shown
        // (0 otherwise), so the bottom row never lands under the window edge.
        extra_top: f32,
        // Backing-scale DPI (= scale_factor × 96). The window padding and
        // decorator reserve are DPI-scaled so the grid's inset holds a constant
        // apparent size; the rendering and hit-test paths apply the identical
        // `dpi_px` scaling, so cells land exactly where the grid math reserved.
        dpi: u32,
    ) -> ViewportSize {
        let window_padding = dpi_px(WINDOW_PADDING, dpi);
        let decorator_height = dpi_px(DECORATOR_HEIGHT, dpi);
        ViewportSize {
            char_width: usize::max(1, (width - window_padding * 2.0) as usize / advance_x),
            // Content extends full-height (behind the translucent title bar
            // on macOS's fullsize_content_view), gaining ~1–2 rows of
            // scrollable area at the top. Reserve DECORATOR_HEIGHT in the
            // row count so the boundary push-down (see `decorator_offset`
            // in `update_vertices`) never shoves the bottom row past the
            // window edge when the height isn't an integer multiple of
            // `line_height`.
            //
            // Floor at MIN_GRID_ROWS, well above 1. Multi-line shell prompts
            // (a powerline/segment bar, sometimes a blank separator, then the
            // input line — three rows is common) don't fit in a 1–2 row grid.
            // When the window is dragged shorter than the prompt, the prompt
            // spills into scrollback and the shell keeps repainting it in the
            // cramped viewport, scrolling a line into scrollback on every
            // WINCH; growing back then refills all that churn as stray blank /
            // duplicate rows above the prompt. Keeping a few rows resident
            // stops the prompt from ever spilling during a resize. A grid this
            // short is unusable as a terminal anyway, so clipping the bottom of
            // an even shorter window costs nothing real.
            char_height: usize::max(
                MIN_GRID_ROWS,
                (height - window_padding * 2.0 - decorator_height - extra_top).max(0.0) as usize
                    / line_height,
            ),
        }
    }

    /// Read the shared font at *this window's* current `(pt_size, dpi)`. Thin
    /// wrapper over [`AppShared::with_font_at`] that supplies the tune so call
    /// sites don't repeat `self.pt_size, self.dpi`. Use this for every font
    /// read (metrics, cell width, line height) so a sibling window on a
    /// different-DPI monitor can't leave the faces tuned to its size.
    fn with_font<R>(&self, f: impl FnOnce(&font::Font) -> R) -> R {
        self.shared.with_font_at(self.pt_size, self.dpi, f)
    }

    /// Mutable counterpart to [`with_font`] for the sites that rasterize
    /// (`build_atlas`). Re-tunes to this window's `(pt_size, dpi)` first, so
    /// the glyphs land at the right pixel size regardless of which sibling
    /// window last touched the shared faces.
    fn with_font_mut<R>(&self, f: impl FnOnce(&mut font::Font) -> R) -> R {
        self.shared.with_font_mut_at(self.pt_size, self.dpi, f)
    }

    fn resize_buffers(&mut self) {
        // Calculate console viewport & buffer sizes
        let metrics = self.with_font(|f| f.metrics());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as usize;
        let viewport = WindowState::get_viewport_size(
            self.surface.config.width as f32,
            self.surface.config.height as f32,
            self.with_font(|f| f.cell_width()),
            line_height,
            self.chrome_extra_top(),
            self.dpi,
        );
        // Mirror `phantom_row_band`'s top widening so the buffer reserves the
        // extra rows the tab bar's chrome inset pushes into the band.
        let top_inset_rows = (self.chrome_extra_top() / line_height.max(1) as f32).ceil() as usize;
        let (vbuf_bytes, ibuf_bytes) =
            grid_buffer_byte_sizes(viewport.char_width, viewport.char_height, top_inset_rows);
        let vertex_buf: Vec<u8> = vec![0; vbuf_bytes];
        self.vertex_buffer =
            self.shared.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vertex buffer"),
                    contents: &bytemuck::cast_slice(&vertex_buf),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                });
        let index_buf: Vec<u8> = vec![0; ibuf_bytes];
        self.index_buffer =
            self.shared.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("index buffer"),
                    contents: &bytemuck::cast_slice(&index_buf),
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                });
    }

    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        // A move to a display with a different scale factor changes the native
        // title bar's physical height; keep the chrome band in step.
        self.refresh_chrome_band();
        if size.width > 0 && size.height > 0 {
            // Update the config/size mirror (the live surface is reconfigured
            // on the present thread, below) and recreate the present-source
            // pool at the new size.
            self.surface.size = size;
            self.surface.config.width = size.width;
            self.surface.config.height = size.height;
            self.present_pool = (0..present::POOL_SIZE)
                .map(|i| {
                    present::PresentTarget::new(
                        &self.shared.gpu.device,
                        self.surface.config.format,
                        size.width,
                        size.height,
                        &format!("present source {i}"),
                    )
                })
                .collect();
            self.free_targets = present::FreePool::new(present::POOL_SIZE);
            let views = self.present_pool.iter().map(|t| t.view.clone()).collect();
            self.presenter.resize(size.width, size.height, views);
        }
        if size.width > 0 && size.height > 0 {
            self.blur.resize(
                &self.shared.gpu.device,
                &self.shared.gpu.queue,
                &self.shared.blur_pipelines,
                size.width,
                size.height,
            );
            // FG scene mirrors the BG scene's size/format. Recreate the
            // texture and rebuild every bind group that samples it.
            self.scene_fg = SceneTarget::new(
                &self.shared.gpu.device,
                self.surface.config.format,
                size.width,
                size.height,
                "scene fg",
            );
            self.scene_fg_blit_bg = self.shared.blur_pipelines.make_blit_bind_group(
                &self.shared.gpu.device,
                &self.scene_fg.view,
                "scene fg blit bg",
            );
            // Each Glow's bright-pass bind group is bound to a specific
            // scene view — resize rebuilds it against the (potentially
            // recreated) texture handle.
            self.glow.resize(
                &self.shared.gpu.device,
                &self.shared.gpu.queue,
                &self.shared.glow_pipelines,
                size.width,
                size.height,
                &self.blur.scene.view,
            );
            self.glow_fg.resize(
                &self.shared.gpu.device,
                &self.shared.gpu.queue,
                &self.shared.glow_pipelines,
                size.width,
                size.height,
                &self.scene_fg.view,
            );
            // Mask bind groups sample the (just-recreated) bg scene
            // texture, so they have to be rebuilt against the new view.
            self.glow_bg_mask = self.shared.glow_pipelines.make_mask_bind_group(
                &self.shared.gpu.device,
                &self.blur.scene.view,
                "glow bg mask (bg scene)",
            );
            self.glow_fg_mask = self.shared.glow_pipelines.make_mask_bind_group(
                &self.shared.gpu.device,
                &self.blur.scene.view,
                "glow fg mask (bg scene)",
            );
            self.scanline_overlay_mask = self.shared.glow_pipelines.make_overlay_mask_bind_group(
                &self.shared.gpu.device,
                &self.blur.scene.view,
                &self.scene_fg.view,
                "scanline overlay mask (bg + fg)",
            );
        }
        self.camera_uniform
            .update_view_proj(&self.camera, size.width as f32, size.height as f32);
        self.shared.gpu.queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[self.camera_uniform]),
        );
        let metrics = self.with_font(|f| f.metrics());
        let size = WindowState::get_viewport_size(
            self.surface.config.width as f32,
            self.surface.config.height as f32,
            self.with_font(|f| f.cell_width()),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
            self.chrome_extra_top(),
            self.dpi,
        );
        // Only touch the PTY winsize when the character grid actually
        // changes. macOS raises SIGWINCH on any TIOCSWINSZ whose winsize
        // differs from the old one (a full bcmp), and ws_xpixel/ws_ypixel
        // shift on every pixel of a live drag. Notifying on pixel-only
        // changes floods the foreground process with SIGWINCH; shells that
        // repaint their prompt on WINCH (powerlevel10k &c.) then stack a
        // fresh prompt per frame, so growing the window appears to push the
        // prompt downward. Gating on rows/cols collapses a drag back to one
        // signal per row boundary crossed.
        let grid_changed = size.char_width != self.active_tab().terminal.cols
            || size.char_height != self.active_tab().terminal.rows;
        self.active_tab_mut().terminal.resize(size.char_width, size.char_height);
        if grid_changed {
            self.notify_pty_size(size.char_width, size.char_height);
        }
        self.resize_buffers();
        self.active_tab_mut().cursor_anim = None;
        // Resizing the window (e.g. dragging its resize handle) doesn't make the
        // palette resign key, so dismiss it here instead of leaving it floating
        // over a now-differently-sized window.
        if self.glass_palette.as_ref().is_some_and(|gp| gp.visible()) {
            self.close_glass_palette();
        }
        if self.glass_find.as_ref().is_some_and(|gf| gf.visible()) {
            self.find_close();
        }
        self.invalidate();
    }

    fn notify_pty_size(&self, cols: usize, rows: usize) {
        // Pixel dimensions are what `kitty +kitten icat` (and any other
        // image-protocol-aware tool that reads `TIOCGWINSZ`) uses to
        // discover the cell-pixel size. Zero here would make those
        // tools refuse to send images with "Terminal does not support
        // reporting screen sizes in pixels."
        let metrics = self.with_font(|f| f.metrics());
        let cell_w = self.with_font(|f| f.cell_width()) as u32;
        let line_h = ((metrics.ascender - metrics.descender) >> 6) as u32;
        let xpixel = (cols as u32).saturating_mul(cell_w).min(u16::MAX as u32) as u16;
        let ypixel = (rows as u32).saturating_mul(line_h).min(u16::MAX as u32) as u16;
        let ws = libc::winsize {
            ws_row: rows as u16,
            ws_col: cols as u16,
            ws_xpixel: xpixel,
            ws_ypixel: ypixel,
        };
        unsafe {
            libc::ioctl(self.active_tab().master, libc::TIOCSWINSZ, &ws);
        }
    }

    fn write_pty(&self, bytes: &[u8]) {
        if let Err(e) = nix::unistd::write(self.active_tab().master, bytes) {
            eprintln!("pty write failed: {e}");
        }
    }

    fn paste_from_clipboard(&self) {
        let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("clipboard read failed: {e}");
                return;
            }
        };
        if self.active_tab().terminal.bracketed_paste() {
            self.write_pty(b"\x1b[200~");
            self.write_pty(text.as_bytes());
            self.write_pty(b"\x1b[201~");
        } else {
            self.write_pty(text.as_bytes());
        }
    }

    fn update(&mut self) {}

}

/// Mint the next process-unique `TabId`. Monotonic; never reused, so a freed
/// tab's id can't collide with a later one (a just-closed tab's reader thread
/// may still deliver one final event — the resolver treats unknown ids as a
/// no-op).
fn next_tab_id() -> app_window::TabId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    app_window::TabId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A fresh, process-unique native tab-group identifier.
/// A window built with a brand-new id opens standalone; Cmd-T reuses the
/// source window's id (read back via `WindowExtMacOS::tabbing_identifier`) so
/// the new window joins that group as a native tab.
fn next_tab_group_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("yutani-tabgroup-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Map a Cmd-`d` digit ('1'..='9') to a 0-based tab index for `num_tabs` tabs:
/// '1'..'8' are absolute positions, returning `None` when the position doesn't
/// exist, and '9' is always the last tab (the Chrome/iTerm convention).
/// `num_tabs` is assumed ≥ 1.
fn tab_index_for_digit(d: char, num_tabs: usize) -> Option<usize> {
    if d == '9' {
        return Some(num_tabs.saturating_sub(1));
    }
    let idx = d as usize - '1' as usize;
    (idx < num_tabs).then_some(idx)
}

/// Create a new tab: open a PTY, fork `program` onto it, spawn the reader
/// thread (tagging every event with the new `TabId`), and build the
/// `TabState`. The grid is sized to `cols`×`rows` — the caller passes the
/// owning window's viewport so the window's vertex buffers match. The reader
/// thread takes the `Pty` by value (it reads + reaps the child); `TabState`
/// keeps copies of `master`+`child` so the tab can be closed cleanly later
/// (`close(master)` + `kill(child, SIGHUP)`).
fn create_tab(
    proxy: &winit::event_loop::EventLoopProxy<app_window::CustomEvent>,
    program: pty::ChildProgram,
    zdotdir: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    cols: usize,
    rows: usize,
    image_mem_cap_bytes: usize,
) -> (app_window::TabId, TabState) {
    let fdm = unsafe { posix_openpt(O_RDWR) };
    if fdm < 0 {
        panic!("Error on posix_openpt()");
    }
    let pty = pty::fork_pty(fdm, program, zdotdir, cwd).expect("failed to fork pty");
    let tab_id = next_tab_id();
    let master = pty.master;
    let child = pty.child;
    let proxy = proxy.clone();
    // Shared output buffer: the reader appends decoded text here and only
    // posts a wake when the loop isn't already due to drain, so a burst of
    // reads collapses to ~one event per drain instead of one per read.
    let outbox = app_window::PtyOutbox::new();
    let reader_outbox = outbox.clone();
    std::thread::spawn(move || {
        let code = pty.run(|data| {
            if reader_outbox.push(data) {
                let _ = proxy.send_event(app_window::CustomEvent::PtyInput(tab_id));
            }
        });
        // `run` returns once the shell has exited and been reaped; tell the
        // loop so it reacts instead of leaving a frozen tab.
        let _ = proxy.send_event(app_window::CustomEvent::PtyExit(tab_id, code));
    });
    let tab = TabState {
        tab_id,
        pty_outbox: outbox,
        master,
        child,
        terminal: terminal::Terminal::new(cols, rows, 10000),
        image_store: images::Store::new(image_mem_cap_bytes),
        pending_placements: Vec::new(),
        scroll_y: 0.0,
        alt_scroll_anim: None,
        primary_scroll_anim: None,
        wheel_pty_accum: 0.0,
        scroll_suppressed: false,
        last_wheel_at: None,
        last_reported_cell: None,
        cursor_anim: None,
        prev_visible: None,
        cursor_ghosts: Vec::new(),
        completions: Vec::new(),
        completions_input: None,
        selected_completion: 0,
        completion_scroll: 0,
        completion_dismissed: false,
        command_history: Vec::new(),
        selection: None,
        selection_mode: SelectionMode::Cell,
        press_cell: None,
        press_pixel: None,
        last_click: None,
        click_count: 0,
        hover_url: None,
        row_cache: std::collections::HashMap::new(),
        row_cache_key: None,
        prev_selection_range: None,
    };
    (tab_id, tab)
}

/// Tear down a tab's PTY. The reader thread's `read(master)` only returns once
/// the child exits, so we `kill(child, SIGHUP)` to end the shell *and*
/// `close(master)` to unblock the read — the thread then reaps the child and
/// exits, firing a final `PtyExit` for this (now-unmapped) tab that the
/// resolver drops. Safe on an already-exited child (the `kill` just returns
/// `ESRCH`).
fn close_tab_pty(tab: &TabState) {
    unsafe {
        libc::kill(tab.child, libc::SIGHUP);
        libc::close(tab.master);
    }
}

/// Pure predicate behind [`tab_command_running`]: given a terminal's
/// foreground process-group id and the shell's pid, decide whether a
/// *foreign* command (anything other than the shell) holds the foreground.
/// A non-positive `fg_pgrp` (e.g. `tcgetpgrp` failed because the shell
/// already exited) means "nothing running".
fn fg_is_foreign_command(fg_pgrp: i32, shell_pid: i32) -> bool {
    fg_pgrp > 0 && fg_pgrp != shell_pid
}

/// Whether a foreground command is running in this tab's PTY. The shell was
/// forked with `setsid()` (see `pty::fork_pty`), so it leads its own process
/// group and that group's id equals `tab.child`. Under job control the shell
/// hands the terminal's foreground group to each command it launches, so a
/// foreground group differing from `tab.child` means a command is in
/// progress. Used to decide whether Cmd-W needs to confirm before closing.
fn tab_command_running(tab: &TabState) -> bool {
    fg_is_foreign_command(unsafe { libc::tcgetpgrp(tab.master) }, tab.child)
}

/// Open a new window in this process (Cmd-N / palette "New window"). Builds the
/// `NSWindow` from the live event loop, a sibling surface from the shared
/// instance, and a fresh tab, then registers both in the window/tab maps. The
/// new window cascades down-and-right off `origin` (the spawner's top-left, in
/// logical points) and its shell opens in `cwd` (the spawner's shell cwd).
#[allow(clippy::too_many_arguments)]
fn spawn_window_in_process(
    elwt: &ActiveEventLoop,
    shared: &Rc<AppShared>,
    proxy: &winit::event_loop::EventLoopProxy<app_window::CustomEvent>,
    windows: &mut std::collections::HashMap<winit::window::WindowId, WindowState>,
    tab_to_window: &mut std::collections::HashMap<app_window::TabId, winit::window::WindowId>,
    config: &Config,
    zdotdir: &Option<std::path::PathBuf>,
    cwd: Option<String>,
    origin: Option<(f64, f64)>,
    // The window's native tab-group id. Matching an open
    // window's id → AppKit adds this as a tab in that group; a fresh id → a
    // standalone window.
    tabbing_id: &str,
    // The spawning window's measured bar-free title-bar height, in physical px,
    // when this window joins an existing group as a native tab (`None` for a
    // standalone window). A window born into a visible tab bar can't measure its
    // own title-bar-only height — `contentLayoutRect` already excludes the bar —
    // so without this baseline its `chrome_extra_top` is derived against the
    // coarse `WINDOW_PADDING + DECORATOR_HEIGHT` seed and the grid sits at a
    // different offset than its siblings. The title bar is the same height on
    // every window of the group's display, so inherit the spawner's.
    inherit_titlebar_px: Option<f64>,
) {
    let title = effective_title(None, cwd.as_deref());
    let transparent = false; // matches the first window (shadow bug)
    let mut attrs = Window::default_attributes()
        .with_title(&title)
        .with_titlebar_transparent(true)
        .with_tabbing_identifier(tabbing_id)
        .with_transparent(transparent)
        .with_has_shadow(!transparent)
        .with_fullsize_content_view(true)
        .with_decorations(true)
        .with_blur(transparent);
    if let Some((x, y)) = origin {
        attrs = attrs.with_position(winit::dpi::LogicalPosition::new(
            x + WINDOW_CASCADE_STEP,
            y + WINDOW_CASCADE_STEP,
        ));
    }
    let window = match elwt.create_window(attrs) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("new window: build failed: {e}");
            return;
        }
    };
    // Match the OS title-bar appearance + native bg to the active palette,
    // exactly as the first window does, so it doesn't flash a wrong fill.
    window.set_theme(Some(theme_for_bg(palette::get().background)));
    set_native_window_bg(&window, palette::get().background);
    suppress_layer_resize_animations(&window);
    window.set_cursor(winit::window::CursorIcon::Text);

    let (surface, surface_raw) = shared.gpu.create_surface(&window);
    let dpi = (window.scale_factor() * 96.0) as u32;
    let (cols, rows) = {
        let (cell_w, line_h) = shared.with_font_at(config.font_size, dpi, |f| {
            let m = f.metrics();
            (f.cell_width(), ((m.ascender - m.descender) >> 6) as usize)
        });
        let vp = WindowState::get_viewport_size(
            surface.config.width as f32,
            surface.config.height as f32,
            cell_w,
            line_h,
            // Brand-new window; tab-bar reserve (if it joins a group) lands via
            // the follow-up resize/focus once it's grouped.
            0.0,
            dpi,
        );
        (vp.char_width, vp.char_height)
    };
    // New windows always get a normal shell — onboarding is first-run only.
    let (tab_id, tab) = create_tab(
        proxy,
        pty::ChildProgram::Shell,
        zdotdir.clone(),
        cwd.map(std::path::PathBuf::from),
        cols,
        rows,
        config.images_memory_cap_mb * 1024 * 1024,
    );
    // SPIKE: give every window (native tab) its own scroll view.
    #[cfg(target_os = "macos")]
    {
        install_scrollview_spike(&window);
    }
    let mut state =
        WindowState::create_window(shared.clone(), window, surface, surface_raw, config.clone(), dpi, tab);
    // Mirror the post-construction setup `run()` does for the first window.
    state.notify_pty_size(state.active_tab().terminal.cols, state.active_tab().terminal.rows);
    // A tab born into an already-visible bar can't measure its own bar-free
    // title-bar height; seed it from the spawner so `refresh_chrome_band`
    // (which only re-records the baseline while the bar is hidden) derives the
    // same `chrome_extra_top` as its siblings instead of one based on the
    // coarse reserve seed.
    if let Some(px) = inherit_titlebar_px {
        state.titlebar_only_px = px;
    }
    // Reflow now that the (possibly visible) bar's height is known, so the grid
    // is sized correctly on the first frame rather than after the focus event.
    state.reflow_for_tab_bar();
    state.sync_theme_colors();
    state.configure_native_tabs();
    let keep = state.config.images_in_scrollback;
    state.active_tab_mut().terminal.set_keep_placements_in_scrollback(keep);
    state.sync_terminal_cell_size();
    state.invalidate();

    let wid = state.window.id();
    windows.insert(wid, state);
    tab_to_window.insert(tab_id, wid);
}

/// Serialize a linear-space RGBA back to a `0xRRGGBB` literal so the
/// scanline-colour config round-trips cleanly. Reuses palette's sRGB
/// conversion so the byte we emit matches the byte the user typed.
fn format_hex_rgb(c: [f32; 4]) -> String {
    let [r, g, b] = palette::color_to_srgb_u8(c);
    format!("0x{:02x}{:02x}{:02x}", r, g, b)
}

/// Pick a window NSAppearance to match a background color. Title-bar text is
/// drawn by the OS using that appearance, so a dark scheme must report Dark
/// or "Yutani" comes out black on near-black.
/// Paint the native `NSWindow` background to match the terminal's bg color.
/// The window is opaque, so during AppKit-driven frame changes — most visibly
/// the title-bar double-click zoom animation — any area exposed before our
/// Window title for an OSC 7 working directory: just the path, with `$HOME`
/// collapsed to `~`. An empty/`/` path falls back to the bare app name.
fn title_for_cwd(cwd: &str) -> String {
    let display = if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if cwd == home {
            "~".to_string()
        } else if let Some(rest) = cwd.strip_prefix(&format!("{home}/")) {
            format!("~/{rest}")
        } else {
            cwd.to_string()
        }
    } else {
        cwd.to_string()
    };
    if display.is_empty() {
        "Yutani".to_string()
    } else {
        display
    }
}

/// Resolve the effective window title: a program-set title (OSC 0/2) wins;
/// otherwise fall back to the cwd-derived title, then the bare app name.
fn effective_title(manual: Option<&str>, cwd: Option<&str>) -> String {
    match manual {
        Some(t) => t.to_string(),
        None => cwd.map(title_for_cwd).unwrap_or_else(|| "Yutani".to_string()),
    }
}

/// Upper bound on retained command-history entries, to keep memory bounded for
/// long-running shells with huge `$HISTFILE`s.
const COMMAND_HISTORY_CAP: usize = 10_000;

/// Insert `cmd` at the front of `history` (most-recent-first), removing any
/// existing equal entry first so it stays deduped, then cap the length.
fn dedup_prepend(history: &mut Vec<String>, cmd: String) {
    history.retain(|c| c != &cmd);
    history.insert(0, cmd);
    history.truncate(COMMAND_HISTORY_CAP);
}

/// Build an `NSString` from a Rust `&str` for objc calls that take text. The
/// returned `Retained` keeps the string alive for the duration of the calls we
/// hand it to; bind it to a local so it outlives those calls.
#[cfg(target_os = "macos")]
fn ns_string(s: &str) -> objc2::rc::Retained<objc2_foundation::NSString> {
    objc2_foundation::NSString::from_str(s)
}

/// Ask, via a native modal alert, whether to close a tab whose shell still
/// has a command running. Returns `true` to close (terminating the command),
/// `false` to keep the tab. `NSAlert.runModal` spins its own modal loop on
/// the main thread, which is fine here: the user explicitly pressed Cmd-W and
/// nothing else should happen until they answer.
#[cfg(target_os = "macos")]
fn confirm_close_running_command() -> bool {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        // `new` is +1; the `Retained` releases it when this scope ends.
        let alert: Retained<AnyObject> = msg_send![class!(NSAlert), new];
        let msg = ns_string("Close this tab?");
        let info = ns_string(
            "A process is still running in this tab. Closing it will terminate that process.",
        );
        let _: () = msg_send![&*alert, setMessageText: &*msg];
        let _: () = msg_send![&*alert, setInformativeText: &*info];
        // NSAlertStyleWarning.
        let _: () = msg_send![&*alert, setAlertStyle: 0usize];
        // The first button added is the default (rightmost, fires on Return).
        let _: *mut AnyObject = msg_send![&*alert, addButtonWithTitle: &*ns_string("Close Tab")];
        let _: *mut AnyObject = msg_send![&*alert, addButtonWithTitle: &*ns_string("Cancel")];
        let response: isize = msg_send![&*alert, runModal];
        // NSAlertFirstButtonReturn == 1000 → the "Close Tab" button.
        response == 1000
    }
}

/// Off-macOS there's no dialog; treat Cmd-W as an unconditional close.
#[cfg(not(target_os = "macos"))]
fn confirm_close_running_command() -> bool {
    true
}

/// Paint both the NSWindow and the backing `CAMetalLayer` with the theme
/// background.
///
/// The window's `backgroundColor` fills any pixels Cocoa composites without a
/// drawable contribution; the layer's own `backgroundColor` fills any pixel
/// the layer covers but the Metal drawable doesn't yet — which happens during
/// a backing-scale change (cross-monitor drag) where the OS animates the
/// layer's `bounds` from old to new physical size before our reconfigured
/// surface produces a fresh drawable. Without the layer color, that
/// in-between frame composites against `kCGColorWhite`, flashing white on
/// dark themes. `bg` is stored linear (the surface is sRGB), so re-encode
/// each channel to sRGB for `NSColor` (which expects sRGB components).
#[cfg(target_os = "macos")]
fn set_native_window_bg(window: &Window, bg: [f32; 4]) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return;
    };
    let chan = palette::linear_to_srgb_f64;
    unsafe {
        let ns_view = handle.ns_view as *mut AnyObject;
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return;
        }
        let color: Retained<AnyObject> = msg_send![
            class!(NSColor),
            colorWithSRGBRed: chan(bg[0]),
            green: chan(bg[1]),
            blue: chan(bg[2]),
            alpha: bg[3] as f64,
        ];
        let _: () = msg_send![ns_window, setBackgroundColor: &*color];
        // Mirror onto the CAMetalLayer that wgpu installed. `layer` is the
        // view's backing layer (the view is layer-hosted because the Metal
        // surface needs `wantsLayer: YES`). `setBackgroundColor:` on a
        // CALayer takes a `CGColorRef`, not `NSColor`, so go through
        // `-CGColor`. Null-check both: a non-layer-backed view or a fresh
        // NSColor that failed to bridge would otherwise crash.
        let layer: *mut AnyObject = msg_send![ns_view, layer];
        if !layer.is_null() {
            let cg: *mut AnyObject = msg_send![&*color, CGColor];
            if !cg.is_null() {
                let _: () = msg_send![layer, setBackgroundColor: cg];
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn set_native_window_bg(_window: &Window, _bg: [f32; 4]) {}

/// Suppress the implicit CoreAnimation actions that fire when a CALayer's
/// `bounds` / `contents` / `contentsScale` / `position` / `sublayers` change
/// — i.e. exactly the keys CoreAnimation animates when the OS drags the
/// window onto a monitor with a different backing scale. By default
/// CoreAnimation interpolates the old drawable to the new bounds over
/// ~0.25s, which reads as a visible "zoom" of the terminal contents during
/// the cross-display drag. Setting these actions to `NSNull` swaps the
/// implicit `CABasicAnimation` for a no-op, so the layer hops straight from
/// old to new state in one frame — matching what the user expects when the
/// pointer moves across the bezel.
///
/// Called once per window after the wgpu surface (and therefore the
/// `CAMetalLayer`) exists. Safe to call multiple times — `setActions:` just
/// replaces the dictionary.
#[cfg(target_os = "macos")]
fn suppress_layer_resize_animations(window: &Window) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return;
    };
    unsafe {
        let ns_view = handle.ns_view as *mut AnyObject;
        let layer: *mut AnyObject = msg_send![ns_view, layer];
        if layer.is_null() {
            return;
        }
        let null: *mut AnyObject = msg_send![class!(NSNull), null];
        let actions: *mut AnyObject = msg_send![class!(NSMutableDictionary), dictionary];
        // The five keys CoreAnimation animates on a layer during a
        // window-bounds / DPI transition. `bounds` + `position` cover the
        // geometric resize; `contents` and `contentsScale` cover the Metal
        // drawable swap; `sublayers` covers any child-layer reshuffle (none
        // today, but cheap insurance against future glass panels).
        for key in ["bounds", "position", "contents", "contentsScale", "sublayers"] {
            let key_bytes = std::ffi::CString::new(key).unwrap();
            let key_ns: *mut AnyObject = msg_send![
                class!(NSString),
                stringWithUTF8String: key_bytes.as_ptr()
            ];
            let _: () = msg_send![actions, setObject: null, forKey: key_ns];
        }
        let _: () = msg_send![layer, setActions: actions];
    }
}

#[cfg(not(target_os = "macos"))]
fn suppress_layer_resize_animations(_window: &Window) {}

/// Force the system arrow cursor onto `NSCursor` immediately.
///
/// winit applies `set_cursor_icon` lazily: it stores the cursor and lets the
/// content view's `cursorUpdate:` push it the next time AppKit decides to.
/// AppKit does *not* fire `cursorUpdate:` while the pointer is over the native
/// title-bar overlay (which sits above our `fullsize_content_view` content
/// view), so the I-beam last applied down in the grid stays frozen on screen
/// up there — `set_cursor_icon(Default)` alone has no visible effect. Pushing
/// the arrow straight onto `[NSCursor set]` sidesteps `cursorUpdate:` and lands
/// the change now. Called on every move within the chrome band, so even if a
/// later `cursorUpdate:` re-applied something, the next move re-asserts it.
#[cfg(target_os = "macos")]
fn force_native_arrow_cursor(window: &Window) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(_handle) = window.raw_window_handle() else {
        return;
    };
    unsafe {
        // Shared cursor (+0); a raw pointer avoids needlessly retaining it.
        let cursor: *mut AnyObject = msg_send![class!(NSCursor), arrowCursor];
        if cursor.is_null() {
            return;
        }
        let _: () = msg_send![cursor, set];
    }
}

#[cfg(not(target_os = "macos"))]
fn force_native_arrow_cursor(_window: &Window) {}

/// Margin (physical px) added below the native title bar when sizing the chrome
/// band. macOS stops delivering pointer-moved events the instant the pointer
/// crosses into the title bar, so the lowest move we ever see sits just *below*
/// the bar, in the top grid row. That boundary event is our only chance to flip
/// the cursor to the arrow (which then sticks as the pointer continues up into
/// the event-dead bar). The margin pulls the band down far enough to include
/// it. Kept tiny so it barely reaches into real content.
const CHROME_BAND_MARGIN_PX: f64 = 4.0;

/// Height of the native `NSWindow` title bar in *physical* pixels, or `None` if
/// the handle isn't AppKit. This is the region macOS owns: it drives window
/// drag / zoom / traffic lights and swallows our pointer-moved events. Unlike
/// the chrome band's fallback floor — the raw, unscaled `WINDOW_PADDING +
/// DECORATOR_HEIGHT` sum, a fixed physical-px lower bound — the title bar is a
/// fixed number of *points*, so on a Retina display it's physically taller
/// than that reserve — which is why a
/// band sized to the reserve never reached the bar and left the grid's I-beam
/// frozen over it. `contentLayoutRect` excludes the title bar even under
/// `fullsize_content_view`, so `frame.height - contentLayoutRect.height` is the
/// bar height in points; scale to physical.
#[cfg(target_os = "macos")]
fn native_titlebar_height_physical(window: &Window) -> Option<f64> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSRect;
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return None;
    };
    unsafe {
        let ns_view = handle.ns_view as *mut AnyObject;
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return None;
        }
        let frame: NSRect = msg_send![ns_window, frame];
        let content: NSRect = msg_send![ns_window, contentLayoutRect];
        let scale: f64 = msg_send![ns_window, backingScaleFactor];
        let titlebar_pts = frame.size.height - content.size.height;
        if titlebar_pts <= 0.0 || scale <= 0.0 {
            return None;
        }
        Some(titlebar_pts * scale)
    }
}

#[cfg(not(target_os = "macos"))]
fn native_titlebar_height_physical(_window: &Window) -> Option<f64> {
    None
}

/// Whether the window's native tab bar is currently shown (`[[window tabGroup]
/// isTabBarVisible]`). True once a window is grouped with ≥1 sibling tab (or
/// when the user's "always show tab bar" setting forces it). `contentLayoutRect`
/// already excludes the bar when it's up, so `chrome_band_px` grows by the bar's
/// height — `refresh_chrome_band` uses this to record the bar-free height and
/// derive the bar's height as the difference.
#[cfg(target_os = "macos")]
fn native_tab_bar_visible(window: &Window) -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return false;
    };
    unsafe {
        let ns_view = handle.ns_view as *mut AnyObject;
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return false;
        }
        let tab_group: *mut AnyObject = msg_send![ns_window, tabGroup];
        if tab_group.is_null() {
            return false;
        }
        let visible: bool = msg_send![tab_group, isTabBarVisible];
        visible
    }
}

#[cfg(not(target_os = "macos"))]
fn native_tab_bar_visible(_window: &Window) -> bool {
    false
}

/// Set by the native tab bar's `+` button (see `install_new_tab_action`).
/// Polled + cleared by the event loop, which opens a tab in the key window's
/// group. An `AtomicBool` because the AppKit action and the poll are decoupled
/// (both run on the main thread, so no ordering subtlety beyond the flag).
pub(crate) static NEW_TAB_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The `newWindowForTab:` implementation AppKit invokes when the `+` button is
/// clicked. Its mere presence on the responder chain is also what makes AppKit
/// *show* the button. We don't open the window here (no access to our state on
/// this objc call) — just raise a flag the event loop drains.
#[cfg(target_os = "macos")]
extern "C" fn yutani_new_window_for_tab(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
) {
    NEW_TAB_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Our `sendEvent:` override on the window class. AppKit calls this for every
/// event the window receives, *synchronously*, before anything is dispatched to
/// views.
///
/// We use that synchronicity to fix native-drag latency. winit doesn't deliver
/// `mouseDown` synchronously — its view queues a `WindowEvent` that we only see
/// at the next `kCFRunLoopBeforeWaiting` drain. By then we're outside AppKit's
/// `mouseDown:`, so `Window::drag_window()` (which is just
/// `performWindowDragWithEvent:` on `NSApp.currentEvent`) no longer has the live
/// mouse-down to hand AppKit, and the drag hesitates for up to ~500ms.
///
/// Here we're still *inside* event delivery with the real event in hand, so a
/// single left mouse-down on the empty title-bar strip goes straight to
/// `performWindowDragWithEvent:` and the window follows the cursor instantly.
/// A *double*-click on that same strip we handle ourselves
/// (`perform_titlebar_double_click_action`): `performWindowDragWithEvent:` only
/// applies the system zoom/minimize action when the press lands on a real
/// title-bar view, which our full-size content view isn't — so without this the
/// gesture only worked over the title *text* (a sibling view that misses our
/// hit-test and falls through to the default path below). Everything else —
/// clicks on the traffic lights, the tab bar, or the terminal body — falls
/// through to the normal `NSWindow` path (and on to winit).
#[cfg(target_os = "macos")]
extern "C" fn yutani_send_event(
    this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    event: *mut objc2::runtime::AnyObject,
) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_app_kit::{NSEvent, NSEventType};
    use objc2_foundation::{NSPoint, NSRect};

    unsafe {
        // A left mouse-down is the only event we ever intercept; let `NSEvent`'s
        // typed `r#type()` read it (the bare `type` selector can't go through the
        // macro — `type` is a Rust keyword).
        let is_left_down = !event.is_null() && {
            let ns_event: &NSEvent = &*event.cast();
            ns_event.r#type() == NSEventType::LeftMouseDown
        };
        if is_left_down {
            let ns_event: &NSEvent = &*event.cast();
            let loc: NSPoint = ns_event.locationInWindow();
            // winit installs its view as the window's contentView. Hit-test from
            // the frame view (its superview) so traffic-light buttons and the tab
            // bar — siblings layered above — are detected and left alone; only a
            // press that lands on the bare content view is a candidate to drag.
            let content_view: *mut AnyObject = msg_send![this, contentView];
            if !content_view.is_null() {
                let frame_view: *mut AnyObject = msg_send![content_view, superview];
                let hit: *mut AnyObject = if frame_view.is_null() {
                    std::ptr::null_mut()
                } else {
                    msg_send![frame_view, hitTest: loc]
                };
                // The bare strip is anything that hit-tests into winit's content
                // view *or one of its descendants* — `hitTest:` returns the
                // deepest subview (winit's layer-backed render view), never the
                // content view itself, so a strict `hit == content_view` never
                // matches. The traffic lights, the native tab bar, and the title
                // text are AppKit siblings outside that subtree, so they still
                // fall through to the default path below (and keep their native
                // behavior, including AppKit's own title-text double-click zoom).
                let mut within_content = false;
                {
                    let mut v = hit;
                    while !v.is_null() {
                        if v == content_view {
                            within_content = true;
                            break;
                        }
                        v = msg_send![v, superview];
                    }
                }
                if within_content {
                    // contentLayoutRect excludes the title bar (and the tab bar
                    // when shown); it sits at the bottom of the window's flipped
                    // coords, so anything above its top edge is the draggable
                    // strip.
                    let frame: NSRect = msg_send![this, frame];
                    let content: NSRect = msg_send![this, contentLayoutRect];
                    let titlebar = frame.size.height - content.size.height;
                    if titlebar > 0.0 && loc.y > content.size.height {
                        if ns_event.clickCount() >= 2 {
                            perform_titlebar_double_click_action(this);
                        } else {
                            let _: () = msg_send![this, performWindowDragWithEvent: event];
                        }
                        return;
                    }
                }
            }
        }
        // Default NSWindow dispatch for everything we didn't claim.
        let this_ref: &AnyObject = &*this;
        let _: () = msg_send![super(this_ref, class!(NSWindow)), sendEvent: event];
    }
}

/// The macOS "Double-click a window's title bar to" behavior.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TitlebarDoubleClickAction {
    Zoom,
    Minimize,
    None,
}

/// Map the `NSGlobalDomain` `AppleActionOnDoubleClick` setting (System Settings ›
/// Desktop & Dock) to the action a title-bar double-click should take. `setting`
/// is the raw default string, or `None` when the key is absent. `"Minimize"`
/// miniaturizes, `"None"` does nothing, and anything else — including the absent
/// default — zooms (the modern macOS default). Free function so the mapping is
/// unit-testable without `NSUserDefaults`; `perform_titlebar_double_click_action`
/// reads the live default and delegates here.
fn titlebar_double_click_action(setting: Option<&str>) -> TitlebarDoubleClickAction {
    match setting {
        Some("Minimize") => TitlebarDoubleClickAction::Minimize,
        Some("None") => TitlebarDoubleClickAction::None,
        _ => TitlebarDoubleClickAction::Zoom,
    }
}

/// Apply the system "Double-click a window's title bar to" action to `window`.
/// We invoke this for double-clicks on the bare title-bar strip — see
/// `yutani_send_event` for why AppKit doesn't do it for us there.
///
/// We route through the `performZoom:` / `performMiniaturize:` action methods
/// rather than `zoom:` / `miniaturize:` so the window gets to validate/animate
/// exactly as the green button and the Window menu do.
#[cfg(target_os = "macos")]
unsafe fn perform_titlebar_double_click_action(window: *mut objc2::runtime::AnyObject) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    let nil = std::ptr::null_mut::<AnyObject>();
    let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
    let setting: Option<objc2::rc::Retained<NSString>> = if defaults.is_null() {
        None
    } else {
        let key = NSString::from_str("AppleActionOnDoubleClick");
        msg_send![defaults, stringForKey: &*key]
    };
    let setting = setting.map(|s| s.to_string());
    match titlebar_double_click_action(setting.as_deref()) {
        TitlebarDoubleClickAction::Minimize => {
            let _: () = msg_send![window, performMiniaturize: nil];
        }
        TitlebarDoubleClickAction::None => {}
        TitlebarDoubleClickAction::Zoom => {
            let _: () = msg_send![window, performZoom: nil];
        }
    }
}

/// Install `newWindowForTab:` on the window's class so AppKit draws the native
/// tab bar's `+` button and routes its clicks to us. Idempotent: the method is
/// added to the (process-wide) window class once; later calls are no-ops.
#[cfg(target_os = "macos")]
fn install_new_tab_action(window: &Window) {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::sel;
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    use std::sync::Once;
    static ONCE: Once = Once::new();
    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return;
    };

    // `v@:@` — void return; self, _cmd, and one object argument (the sender /
    // the NSEvent). Both methods we add share this signature.
    type ImpAbi = extern "C" fn(*mut AnyObject, Sel, *mut AnyObject);

    unsafe {
        let ns_view = handle.ns_view as *mut AnyObject;
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return;
        }
        ONCE.call_once(|| {
            let cls =
                objc2::ffi::object_getClass(ns_window.cast()) as *mut AnyClass;
            let types = c"v@:@".as_ptr();
            objc2::ffi::class_addMethod(
                cls,
                sel!(newWindowForTab:),
                std::mem::transmute::<ImpAbi, unsafe extern "C-unwind" fn()>(
                    yutani_new_window_for_tab,
                ),
                types,
            );
            // Override sendEvent: so we can start a native title-bar drag on the
            // live mouse-down, before winit's deferred event queue swallows it.
            objc2::ffi::class_addMethod(
                cls,
                sel!(sendEvent:),
                std::mem::transmute::<ImpAbi, unsafe extern "C-unwind" fn()>(
                    yutani_send_event,
                ),
                types,
            );
        });
    }
}

#[cfg(not(target_os = "macos"))]
fn install_new_tab_action(_window: &Window) {}

/// Force the process name to the proper-noun "Yutani" before winit builds its
/// default menu. winit titles every app-menu item ("About …", "Hide …",
/// "Quit …") from `NSProcessInfo.processName`, which otherwise defaults to the
/// lowercase executable name (`yutani`) when running unbundled — so the menu
/// reads "About yutani". Must run before `run_app` (the menu is built during
/// `applicationDidFinishLaunching`). No-op in the .app bundle, where the name is
/// already "Yutani", but harmless to set either way.
#[cfg(target_os = "macos")]
fn set_app_process_name() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    unsafe {
        let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        if info.is_null() {
            return;
        }
        let name = NSString::from_str("Yutani");
        let _: () = msg_send![info, setProcessName: &*name];
    }
}

#[cfg(not(target_os = "macos"))]
fn set_app_process_name() {}

fn theme_for_bg(bg: [f32; 4]) -> winit::window::Theme {
    // Rec. 709 luma in linear-light. <0.18 is roughly perceptual midgray
    // (sRGB 0.5). Below that, dark chrome reads better.
    let luma = 0.2126 * bg[0] + 0.7152 * bg[1] + 0.0722 * bg[2];
    if luma < 0.18 {
        winit::window::Theme::Dark
    } else {
        winit::window::Theme::Light
    }
}

/// Window-relative `py` (physical pixels) falls inside the title bar / toolbar
/// chrome band of height `band_px`. Free function so the boundary is
/// unit-testable without standing up a full `WindowState`; `WindowState::in_top_toolbar`
/// delegates here, passing the live `chrome_band_px`. See `in_top_toolbar` for
/// why the band tracks the native title-bar height rather than the
/// scroll-animated decorator offset.
fn py_in_top_toolbar(py: f64, band_px: f64) -> bool {
    py < band_px
}

/// Choose the chrome band height from the live native title-bar height (if
/// queryable) and the renderer's fixed `reserve`. Extracted from
/// `WindowState::refresh_chrome_band` so the selection arithmetic is unit-testable
/// without a real `Window`: add `margin` (the DPI-scaled `CHROME_BAND_MARGIN_PX`)
/// to the native height, but never go below the reserve (and fall back to the
/// reserve when the query failed). See `refresh_chrome_band` for the rationale.
fn chrome_band_from(native: Option<f64>, reserve: f64, margin: f64) -> f64 {
    native
        .map(|h| h + margin)
        .filter(|band| *band >= reserve)
        .unwrap_or(reserve)
}

// ---------------------------------------------------------------------------
// SPIKE: native NSScrollView overscroll. Reparent winit's Metal-backed view
// into an NSScrollView so we get the native scroller + elastic rubber-band for
// free, then mirror the scroll offset into our own viewport. This block is
// exploratory; it lives behind `install_scrollview_spike`, called once for the
// first window.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
thread_local! {
    /// Per-window spike NSScrollViews, keyed by the `NSWindow` pointer. Each
    /// window (native tab) gets its own scroll view; the poll drives only the
    /// focused window from its *own* scroll view.
    static SPIKE_SCROLLVIEWS: std::cell::RefCell<
        std::collections::HashMap<usize, *mut objc2::runtime::AnyObject>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());

    /// Per-window last-synced `(scroll_origin_points, view_offset)`, to tell a
    /// user scroll (origin moved) from the terminal scrolling itself (view_offset
    /// moved) so the two stay in sync without fighting.
    static SPIKE_LAST: std::cell::RefCell<std::collections::HashMap<usize, (f64, usize)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Read `window`'s last-synced `(origin_points, view_offset)`.
#[cfg(target_os = "macos")]
fn spike_last(window: &Window) -> Option<(f64, usize)> {
    let key = ns_window_key(window);
    SPIKE_LAST.with(|m| m.borrow().get(&key).copied())
}

/// Record `window`'s last-synced `(origin_points, view_offset)`.
#[cfg(target_os = "macos")]
fn set_spike_last(window: &Window, origin: f64, view_offset: usize) {
    let key = ns_window_key(window);
    SPIKE_LAST.with(|m| m.borrow_mut().insert(key, (origin, view_offset)));
}

/// SPIKE: event-loop proxy for waking winit from the AppKit bounds-change
/// observer. Set once at startup.
#[cfg(target_os = "macos")]
static SPIKE_PROXY: std::sync::OnceLock<
    winit::event_loop::EventLoopProxy<app_window::CustomEvent>,
> = std::sync::OnceLock::new();

#[cfg(target_os = "macos")]
pub(crate) fn set_spike_proxy(
    proxy: winit::event_loop::EventLoopProxy<app_window::CustomEvent>,
) {
    let _ = SPIKE_PROXY.set(proxy);
}

/// Selector callback for `NSViewBoundsDidChangeNotification`: a scroll view's
/// clip view moved (user scroll, momentum, or elastic bounce). Wake the loop so
/// `about_to_wait` mirrors the offset — no busy-poll.
#[cfg(target_os = "macos")]
extern "C" fn yutani_scroll_did_change(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _note: *mut objc2::runtime::AnyObject,
) {
    if let Some(p) = SPIKE_PROXY.get() {
        let _ = p.send_event(app_window::CustomEvent::ScrollSync);
    }
}

/// Lazily build and return the shared scroll observer object (an `NSObject`
/// subclass with one `scrollDidChange:` method that wakes the loop).
#[cfg(target_os = "macos")]
fn spike_scroll_observer() -> *mut objc2::runtime::AnyObject {
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::{class, msg_send, sel};
    use std::sync::OnceLock;
    static OBSERVER: OnceLock<usize> = OnceLock::new();
    let ptr = *OBSERVER.get_or_init(|| unsafe {
        type ImpAbi = extern "C" fn(*mut AnyObject, Sel, *mut AnyObject);
        // Register a fresh NSObject subclass with our notification method.
        let superclass = class!(NSObject);
        let name = c"YutaniScrollObserver";
        let cls = objc2::ffi::objc_allocateClassPair(superclass, name.as_ptr(), 0)
            as *mut AnyClass;
        objc2::ffi::class_addMethod(
            cls,
            sel!(scrollDidChange:),
            std::mem::transmute::<ImpAbi, unsafe extern "C-unwind" fn()>(
                yutani_scroll_did_change,
            ),
            c"v@:@".as_ptr(),
        );
        objc2::ffi::objc_registerClassPair(cls);
        let obj: *mut AnyObject = msg_send![cls as *const AnyClass, new];
        obj as usize
    });
    ptr as *mut AnyObject
}

/// The `NSWindow` pointer backing `window`, as a map key. 0 if unavailable.
#[cfg(target_os = "macos")]
fn ns_window_key(window: &Window) -> usize {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return 0;
    };
    unsafe {
        let ns_view = handle.ns_view as *mut AnyObject;
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        ns_window as usize
    }
}

/// Wrap winit's Metal view in an NSScrollView with a tall dummy document view,
/// so AppKit draws the native overlay scroller and provides elastic overscroll.
/// The Metal view is added as a *floating* subview so it stays pinned to the
/// viewport while the (empty) document scrolls underneath. Returns whether the
/// wrap was installed.
#[cfg(target_os = "macos")]
fn install_scrollview_spike(window: &Window) -> bool {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSPoint, NSRect};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return false;
    };
    unsafe {
        let mtl_view = handle.ns_view as *mut AnyObject;
        let ns_window: *mut AnyObject = msg_send![mtl_view, window];
        if ns_window.is_null() {
            return false;
        }
        let content: *mut AnyObject = msg_send![ns_window, contentView];
        if content.is_null() {
            return false;
        }
        let bounds: NSRect = msg_send![content, bounds];

        // Scroll view filling the content area.
        let sv: *mut AnyObject = msg_send![class!(NSScrollView), alloc];
        let sv: *mut AnyObject = msg_send![sv, initWithFrame: bounds];
        let _: () = msg_send![sv, setDrawsBackground: false];
        // The window uses a full-size content view under a transparent title
        // bar, so AppKit otherwise auto-insets the clip view by the title/tab-bar
        // height — which reads as a few lines of phantom scroll (a gap above the
        // prompt). Pin the resting scroll origin at 0 instead.
        let _: () = msg_send![sv, setAutomaticallyAdjustsContentInsets: false];
        let _: () = msg_send![sv, setHasVerticalScroller: true];
        let _: () = msg_send![sv, setHasHorizontalScroller: false];
        // NSScrollElasticityAllowed = 2.
        let _: () = msg_send![sv, setVerticalScrollElasticity: 2isize];
        let _: () = msg_send![sv, setAutohidesScrollers: true];
        // NSScrollerStyleOverlay = 1.
        let _: () = msg_send![sv, setScrollerStyle: 1isize];

        // Dummy document view. Start it at exactly the viewport height (no
        // scroll range) so a fresh tab opens parked at the bottom (newest); the
        // per-frame sync grows it to the scrollback depth. A taller initial doc
        // would start scrolled to its top and read as a big overscroll gap.
        let doc: *mut AnyObject = msg_send![class!(NSView), alloc];
        let doc_frame = NSRect::new(NSPoint::new(0.0, 0.0), bounds.size);
        let doc: *mut AnyObject = msg_send![doc, initWithFrame: doc_frame];
        let _: () = msg_send![sv, setDocumentView: doc];

        // Less invasive variant: keep winit's Metal view as the content view
        // (so winit's contentView/responder assumptions hold) and add the scroll
        // view as a transparent subview on top, purely for the native scroller +
        // elastic bounds. NSViewWidthSizable(2) | NSViewHeightSizable(16) = 18.
        let _: () = msg_send![sv, setAutoresizingMask: 18usize];
        let _: () = msg_send![mtl_view, addSubview: sv];

        // Wake the loop on scroll instead of busy-polling: have the clip view
        // post bounds-change notifications and observe them.
        let clip: *mut AnyObject = msg_send![sv, contentView];
        let _: () = msg_send![clip, setPostsBoundsChangedNotifications: true];
        let observer = spike_scroll_observer();
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        let name = objc2_foundation::NSString::from_str("NSViewBoundsDidChangeNotification");
        let _: () = msg_send![
            center,
            addObserver: observer,
            selector: objc2::sel!(scrollDidChange:),
            name: &*name,
            object: clip,
        ];

        let key = ns_window as usize;
        SPIKE_SCROLLVIEWS.with(|m| m.borrow_mut().insert(key, sv));
        true
    }
}

/// The spike scroll view for `window`, or null.
#[cfg(target_os = "macos")]
fn scrollview_for(window: &Window) -> *mut objc2::runtime::AnyObject {
    let key = ns_window_key(window);
    SPIKE_SCROLLVIEWS.with(|m| m.borrow().get(&key).copied().unwrap_or(std::ptr::null_mut()))
}

/// `(clip_origin_y, viewport_height, doc_height)` of `window`'s spike scroll
/// view, in AppKit points. `clip_origin_y` is the scroll position (0 = bottom of
/// the non-flipped document); the elastic overscroll pushes it below 0 / above
/// `doc_height - viewport_height`. None if the spike isn't installed.
#[cfg(target_os = "macos")]
fn poll_scrollview_metrics(window: &Window) -> Option<(f64, f64, f64)> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSRect;
    let sv = scrollview_for(window);
    if sv.is_null() {
        return None;
    }
    unsafe {
        let clip: *mut AnyObject = msg_send![sv, contentView];
        let doc: *mut AnyObject = msg_send![sv, documentView];
        if clip.is_null() || doc.is_null() {
            return None;
        }
        let cb: NSRect = msg_send![clip, bounds];
        let db: NSRect = msg_send![doc, frame];
        Some((cb.origin.y, cb.size.height, db.size.height))
    }
}

/// Show or hide `window`'s scroll view. Hiding lets wheel events fall through to
/// winit's view (e.g. on the alt screen, where the running app should receive
/// them) instead of the scroll view swallowing them.
#[cfg(target_os = "macos")]
fn set_scrollview_hidden(window: &Window, hidden: bool) {
    use objc2::msg_send;
    let sv = scrollview_for(window);
    if sv.is_null() {
        return;
    }
    unsafe {
        let cur: bool = msg_send![sv, isHidden];
        if cur != hidden {
            let _: () = msg_send![sv, setHidden: hidden];
        }
    }
}

/// Enable/disable `window`'s vertical scroller (hides the bar when off, e.g.
/// while a find / palette overlay is open).
#[cfg(target_os = "macos")]
fn set_scrollview_scroller(window: &Window, enabled: bool) {
    use objc2::msg_send;
    let sv = scrollview_for(window);
    if sv.is_null() {
        return;
    }
    unsafe {
        let cur: bool = msg_send![sv, hasVerticalScroller];
        if cur != enabled {
            let _: () = msg_send![sv, setHasVerticalScroller: enabled];
        }
    }
}

/// Programmatically move `window`'s scroll view to vertical offset `y` (points),
/// e.g. to follow the terminal when it scrolls itself (output / jump-to-bottom).
#[cfg(target_os = "macos")]
fn set_scrollview_origin(window: &Window, y: f64) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSPoint;
    let sv = scrollview_for(window);
    if sv.is_null() {
        return;
    }
    unsafe {
        let clip: *mut AnyObject = msg_send![sv, contentView];
        if clip.is_null() {
            return;
        }
        let _: () = msg_send![clip, scrollToPoint: NSPoint::new(0.0, y.max(0.0))];
        let _: () = msg_send![sv, reflectScrolledClipView: clip];
    }
}

/// Release `window`'s scroll view + observer registration and drop the map
/// entry, on window close.
#[cfg(target_os = "macos")]
fn remove_scrollview_spike(window: &Window) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    let key = ns_window_key(window);
    let sv = SPIKE_SCROLLVIEWS.with(|m| m.borrow_mut().remove(&key)).unwrap_or(std::ptr::null_mut());
    if sv.is_null() {
        return;
    }
    unsafe {
        let clip: *mut AnyObject = msg_send![sv, contentView];
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        let _: () = msg_send![center, removeObserver: spike_scroll_observer(), name: std::ptr::null::<AnyObject>(), object: clip];
        let _: () = msg_send![sv, removeFromSuperview];
    }
}

/// Resize the spike document view so the scroller thumb reflects the real
/// scrollback depth: `height` should be the viewport plus the scrollable range.
#[cfg(target_os = "macos")]
fn set_scrollview_doc_height(window: &Window, height: f64) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSRect, NSSize};
    let sv = scrollview_for(window);
    if sv.is_null() {
        return;
    }
    unsafe {
        let doc: *mut AnyObject = msg_send![sv, documentView];
        if doc.is_null() {
            return;
        }
        let f: NSRect = msg_send![doc, frame];
        if (f.size.height - height).abs() > 0.5 {
            let _: () = msg_send![doc, setFrameSize: NSSize::new(f.size.width.max(1.0), height.max(1.0))];
        }
    }
}

/// Draw a renderer-side frosted-glass band across the title-bar chrome, behind
/// the system tabs. Blurs the terminal content that bleeds up under the chrome,
/// so the title bar reads as Liquid Glass while content still flows behind it
/// (AppKit glass can't sample our Metal layer, so we frost it ourselves). Costs
/// a per-frame blur pass while on.
pub(crate) const GLASS_TITLEBAR: bool = true;
/// Overall opacity of the frosted band (1.0 = fully frosted; lower lets a bit
/// of the sharp content read through).
pub(crate) const GLASS_TITLEBAR_ALPHA: f32 = 1.0;
/// When the native tab bar is shown, extend the frosted band's dissolve this
/// fraction of the tab-bar height past the title-bar bottom (= the tab tops),
/// so content passing through the gap between the title bar and the tabs reads
/// a little blurred there rather than snapping sharp right at the tab tops.
pub(crate) const GLASS_TITLEBAR_TAB_DISSOLVE: f32 = 0.5;

fn clear_color(_theme: winit::window::Theme) -> wgpu::Color {
    let bg = palette::get().background;
    wgpu::Color {
        r: bg[0] as f64,
        g: bg[1] as f64,
        b: bg[2] as f64,
        a: bg[3] as f64,
    }
}

fn main() {
    // First-run onboarding runs as the PTY child (see `run`), re-invoking this
    // same binary with `--onboard`. In that mode we are a thin console program
    // talking to our host terminal over stdin/stdout, not a GUI — so branch
    // before any window / GPU setup. `onboard::run` never returns: it execs the
    // user's shell in-place when done, so the same PTY flows straight into the
    // shell with no second window.
    if std::env::args().skip(1).any(|a| a == "--onboard") {
        onboard::run();
    }
    run();
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;