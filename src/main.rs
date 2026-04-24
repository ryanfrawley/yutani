mod app_window;
mod font;
mod font_loader;
mod renderer;

mod ansi;
mod gpu;
mod input;
mod style;
mod terminal;

mod pty;

use winit::{
    event::*,
    event_loop::EventLoopBuilder,
    event_loop::EventLoopWindowTarget,
    platform::macos::WindowBuilderExtMacOS,
    window::{Window, WindowBuilder},
};

extern crate libc;
use nix::libc::*;

// use rand_distr::{Distribution, Normal};
// use rand::thread_rng;

use wgpu::util::DeviceExt;

const WINDOW_PADDING: f32 = 16.0;
const DECORATOR_HEIGHT: f32 = 24.0;

pub struct ViewportSize {
    char_width: usize,
    char_height: usize,
}

struct State {
    gpu: gpu::GpuContext,

    // Must be declared after `gpu` so it gets dropped after the surface —
    // the surface holds unsafe references to the window's resources.
    window: Window,

    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    font: font::Font,
    font_bind_group: wgpu::BindGroup,
    camera: renderer::camera::Camera,
    camera_uniform: renderer::camera::CameraUniform,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    atlas: font::Atlas,
    terminal: terminal::Terminal,
    input: input::InputState,
    scroll_y: f64,
    mouse_x: f64,
    mouse_y: f64,
    master: i32,
}

impl State {
    async fn new(master: i32, window: Window, mut font: font::Font) -> Self {
        let gpu = gpu::GpuContext::new(&window).await;

        // Font texture setup
        let atlas = font.build_atlas();

        let font_alpha = renderer::texture::Texture::from_memory(
            &gpu.device,
            &gpu.queue,
            &atlas.buffer,
            atlas.width as u32,
            atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture"),
        );

        let texture_bind_group_layout =
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
                ],
                label: Some("font texture bind group layout"),
            });

        let font_bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&font_alpha.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&font_alpha.sampler),
                },
            ],
            label: Some("font bind group"),
        });

        let camera = renderer::camera::Camera {};
        let mut camera_uniform = renderer::camera::CameraUniform::new();
        camera_uniform.update_view_proj(&camera, gpu.config.width as f32, gpu.config.height as f32);

        let camera_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("camera buffer"),
            contents: bytemuck::cast_slice(&[camera_uniform]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
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

        let camera_bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
            label: Some("camera bind group"),
        });

        let shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("renderer/shader.wgsl"));

        let render_pipeline_layout =
            gpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("render pipeline layout"),
                bind_group_layouts: &[&texture_bind_group_layout, &camera_bind_group_layout],
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
                    format: gpu.config.format,
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

        // Calculate console viewport & buffer sizes
        let metrics = font.face.size_metrics().unwrap();
        let viewport = State::get_viewport_size(
            gpu.config.width as f32,
            gpu.config.height as f32,
            (metrics.max_advance >> 6) as usize,
            (metrics.height >> 6) as usize,
        );
        // Each cell contributes two quads (background + glyph) = 8 verts.
        // Add slack for the cursor + worst-case input overlay.
        let area = viewport.char_height * viewport.char_width;
        let mut vertex_buf: Vec<u8> = Vec::with_capacity(
            2 * (area + 1) * std::mem::size_of::<renderer::vertex::Vertex>() * 4,
        );
        for _ in 0..vertex_buf.capacity() {
            vertex_buf.push(0);
        }
        let vertex_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: &bytemuck::cast_slice(&vertex_buf),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let mut index_buf: Vec<u8> = Vec::with_capacity(
            2 * (area + 1) * std::mem::size_of::<u16>() * 6,
        );
        for _ in 0..index_buf.capacity() {
            index_buf.push(0);
        }
        let index_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("index buffer"),
            contents: &bytemuck::cast_slice(&index_buf),
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        });

        Self {
            window,
            gpu,
            atlas,
            render_pipeline,
            vertex_buffer,
            index_buffer,
            num_indices: 0,
            font,
            font_bind_group,
            camera,
            camera_uniform,
            camera_buffer,
            camera_bind_group,
            terminal: terminal::Terminal::new(
                viewport.char_width,
                viewport.char_height,
                10000,
            ),
            input: input::InputState::new(1024),
            scroll_y: 0.0,
            mouse_x: 0.0,
            mouse_y: 0.0,
            master,
        }
    }

    fn get_viewport_size(
        width: f32,
        height: f32,
        advance_x: usize,
        line_height: usize,
    ) -> ViewportSize {
        ViewportSize {
            char_width: usize::max(1, (width - WINDOW_PADDING * 2.0) as usize / advance_x),
            char_height: usize::max(
                1,
                (height - DECORATOR_HEIGHT - WINDOW_PADDING * 2.0) as usize / line_height,
            ),
        }
    }

    fn resize_buffers(&mut self) {
        // Calculate console viewport & buffer sizes
        let metrics = self.font.face.size_metrics().unwrap();
        let viewport = State::get_viewport_size(
            self.gpu.config.width as f32,
            self.gpu.config.height as f32,
            (metrics.max_advance >> 6) as usize,
            (metrics.height >> 6) as usize,
        );
        println!("w: {} h: {}", viewport.char_width, viewport.char_height);
        let mut vertex_buf: Vec<u8> = Vec::with_capacity(
            ((2 * viewport.char_height * viewport.char_width) + 1)
                * std::mem::size_of::<renderer::vertex::Vertex>()
                * 4,
        );
        for _ in 0..vertex_buf.capacity() {
            vertex_buf.push(0);
        }
        self.vertex_buffer =
            self.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vertex buffer"),
                    contents: &bytemuck::cast_slice(&vertex_buf),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                });
        let mut index_buf: Vec<u8> = Vec::with_capacity(
            (2 * (viewport.char_height * viewport.char_width) + 1) * std::mem::size_of::<u16>() * 6,
        );
        for _ in 0..index_buf.capacity() {
            index_buf.push(0);
        }
        self.index_buffer =
            self.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("index buffer"),
                    contents: &bytemuck::cast_slice(&index_buf),
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                });
    }

    // Rebuild the vertex/index buffers for the current terminal state. Emits
    // one bg quad + one glyph quad per cell for the grid, the input overlay
    // starting at the terminal cursor, and a cursor box on top.
    fn update_vertices(&mut self) {
        let cols = self.terminal.cols;
        let rows = self.terminal.rows;
        let area = cols * rows;
        let mut vertices: Vec<renderer::vertex::Vertex> =
            Vec::with_capacity(8 * (area + self.input.text.len() + 1));
        let mut indices: Vec<u16> =
            Vec::with_capacity(12 * (area + self.input.text.len() + 1));

        let theme = self.window.theme().unwrap_or(winit::window::Theme::Light);
        let metrics = self.font.face.size_metrics().unwrap();
        let line_height = (metrics.height >> 6) as f32;
        let cell_w = (metrics.max_advance >> 6) as f32;
        let bg_h = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let descender = (metrics.descender >> 6) as f32;

        let default_fg = match theme {
            winit::window::Theme::Dark => [0.9, 0.9, 0.9, 1.0],
            winit::window::Theme::Light => [0.0, 0.0, 0.0, 1.0],
        };
        let default_bg = [0.0, 0.0, 0.0, 0.0];
        let atlas_w = self.atlas.width as f32;
        let atlas_h = self.atlas.height as f32;
        let bg_u = 1.0 / atlas_w;
        let bg_v = 1.0 / atlas_h;
        let scroll_y = self.scroll_y as f32;

        let mut push_quad =
            |verts: &mut Vec<renderer::vertex::Vertex>,
             idxs: &mut Vec<u16>,
             x: f32,
             y: f32,
             w: f32,
             h: f32,
             uv0: [f32; 2],
             uv1: [f32; 2],
             color: [f32; 4]| {
                let start = verts.len() as u16;
                verts.push(renderer::vertex::Vertex {
                    position: [x, y, 0.0],
                    tex_coords: [uv0[0], uv0[1]],
                    color,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x, y + h, 0.0],
                    tex_coords: [uv0[0], uv1[1]],
                    color,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x + w, y, 0.0],
                    tex_coords: [uv1[0], uv0[1]],
                    color,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x + w, y + h, 0.0],
                    tex_coords: [uv1[0], uv1[1]],
                    color,
                });
                idxs.extend_from_slice(&[start, start + 1, start + 2, start + 1, start + 2, start + 3]);
            };

        // Grid row `r` sits with its baseline at (r+1) * line_height; the
        // glyph box extends up by bearing_y and down by (height - bearing_y).
        let row_y = |r: usize| WINDOW_PADDING + DECORATOR_HEIGHT + (r as f32 + 1.0) * line_height;
        let col_x = |c: usize| WINDOW_PADDING + c as f32 * cell_w;

        let atlas = &self.atlas;
        let mut emit_cell = |verts: &mut Vec<renderer::vertex::Vertex>,
                             idxs: &mut Vec<u16>,
                             ch: char,
                             r: usize,
                             c: usize,
                             fg: [f32; 4],
                             bg: [f32; 4]| {
            let x = col_x(c);
            let baseline_y = row_y(r);
            // background
            let bg_y = baseline_y - bg_h - descender + scroll_y;
            push_quad(
                verts,
                idxs,
                x,
                bg_y,
                cell_w,
                bg_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                bg,
            );
            // foreground glyph — fall back to .notdef (tofu box) if the font
            // doesn't have this character, so the user sees *something*.
            let g = atlas.entries.get(&ch).unwrap_or(&atlas.notdef);
            if g.width > 0 && g.height > 0 {
                {
                    let gx = x + g.bearing_x as f32;
                    let gy = baseline_y - g.bearing_y as f32 + scroll_y;
                    let u0 = g.x as f32 / atlas_w;
                    let v0 = g.y as f32 / atlas_h;
                    let u1 = (g.x + g.width) as f32 / atlas_w;
                    let v1 = (g.y + g.height) as f32 / atlas_h;
                    push_quad(
                        verts,
                        idxs,
                        gx,
                        gy,
                        g.width as f32,
                        g.height as f32,
                        [u0, v0],
                        [u1, v1],
                        fg,
                    );
                }
            }
        };

        // 1. Terminal grid
        for r in 0..rows {
            for c in 0..cols {
                let cell = self.terminal.row(r)[c];
                let fg = cell.style.color_fg.unwrap_or(default_fg);
                let bg = cell.style.color_bg.unwrap_or(default_bg);
                emit_cell(&mut vertices, &mut indices, cell.ch, r, c, fg, bg);
            }
        }

        // 2. Input overlay at the terminal's cursor position
        let cur = self.terminal.cursor();
        for (i, ch) in self.input.text.chars().enumerate() {
            let c = cur.col + i;
            if cur.row >= rows || c >= cols {
                break;
            }
            emit_cell(&mut vertices, &mut indices, ch, cur.row, c, default_fg, default_bg);
        }

        // 3. Cursor box — drawn at (cur.row, cur.col + input.cursor_offset)
        if self.terminal.cursor_visible() {
            let cur_col = (cur.col + self.input.cursor_offset).min(cols.saturating_sub(1));
            let cur_row = cur.row.min(rows.saturating_sub(1));
            let x = col_x(cur_col);
            let y = row_y(cur_row) - bg_h - descender + scroll_y;
            let cursor_color = match theme {
                winit::window::Theme::Light => [0.1, 0.0, 0.8, 1.0],
                winit::window::Theme::Dark => [0.9, 0.9, 0.9, 1.0],
            };
            push_quad(
                &mut vertices,
                &mut indices,
                x,
                y,
                cell_w,
                bg_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                cursor_color,
            );
        }

        self.gpu
            .queue
            .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));
        self.gpu
            .queue
            .write_buffer(&self.index_buffer, 0, bytemuck::cast_slice(&indices));
        self.num_indices = indices.len() as u32;
    }

    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        self.gpu.resize(size);
        self.camera_uniform
            .update_view_proj(&self.camera, size.width as f32, size.height as f32);
        self.gpu.queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[self.camera_uniform]),
        );
        let metrics = self.font.face.size_metrics().unwrap();
        let size = State::get_viewport_size(
            self.gpu.config.width as f32,
            self.gpu.config.height as f32,
            (metrics.max_advance >> 6) as usize,
            (metrics.height >> 6) as usize,
        );
        self.terminal.resize(size.char_width, size.char_height);
        self.notify_pty_size(size.char_width, size.char_height);
        self.resize_buffers();
        self.update_vertices();
    }

    fn notify_pty_size(&self, cols: usize, rows: usize) {
        let ws = libc::winsize {
            ws_row: rows as u16,
            ws_col: cols as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master, libc::TIOCSWINSZ, &ws);
        }
    }

    fn process_input(&mut self) {
        if self.input.is_empty() {
            return;
        }

        let cmd = format!("{}\n", self.input.text);
        match nix::unistd::write(self.master, cmd.as_bytes()) {
            Ok(n) => println!("wrote {n} bytes"),
            Err(e) => panic!("{e}"),
        };
        // Don't locally echo: modern shells' line editors (zsh's zle, bash's
        // readline) do their own echoing over the PTY regardless of termios
        // ECHO, and a second copy from us lands at the wrong column.
        self.input.clear();
    }

    fn input(
        &mut self,
        event: &WindowEvent,
        elwt: &EventLoopWindowTarget<app_window::CustomEvent>,
    ) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_x = position.x;
                self.mouse_y = position.y;
            }
            // Mouse click and scroll-wheel are not wired up yet — scrollback
            // viewport isn't rendered, so there's nothing to scroll into.
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == winit::event::ElementState::Pressed {
                    match event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => {
                            self.process_input();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Backspace) => {
                            self.input.delete_left();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowUp) => {
                            // TODO: send CSI A to the PTY for shell history navigation
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowDown) => {
                            // TODO: send CSI B to the PTY
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowLeft) => {
                            self.input.cursor_left();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowRight) => {
                            self.input.cursor_right();
                        }
                        _ => {
                            let k = event.text.to_owned().unwrap_or_default();
                            if let Some(k) = k.chars().next() {
                                self.input.insert_right(k);
                            }
                        }
                    };
                    self.update_vertices();
                    self.window.request_redraw();
                    return true;
                }
            }
            _ => (),
        }
        false
    }

    fn update(&mut self) {}

    fn render(&mut self, clear: wgpu::Color) -> Result<(), wgpu::SurfaceError> {
        // println!("render");
        let output = self.gpu.surface.get_current_texture().unwrap();
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("terminal"),
                });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            render_pass.set_pipeline(&self.render_pipeline);
            render_pass.set_bind_group(0, &self.font_bind_group, &[]);
            render_pass.set_bind_group(1, &self.camera_bind_group, &[]);
            render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            render_pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
            render_pass.draw_indexed(0..self.num_indices, 0, 0..1);
        }

        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok(())
    }
}

async fn run() {
    env_logger::init();
    let event_loop = EventLoopBuilder::<app_window::CustomEvent>::with_user_event()
        .build()
        .unwrap();
    let event_loop_proxy = event_loop.create_proxy();

    // create the pty before forking so we have the handle available
    let fdm: i32;
    unsafe {
        fdm = posix_openpt(O_RDWR);
        println!("fdm: {fdm}");
        if fdm < 0 {
            panic!("Error on posix_openpt()");
        }
    }

    // Fork before the window is created so we hold the master fd across setup.
    let pty = pty::fork_pty(fdm).expect("failed to fork pty");
    std::thread::spawn(move || {
        pty.run(|data| {
            let _ = event_loop_proxy.send_event(app_window::CustomEvent::PtyInput(data.to_owned()));
        });
    });

    let transparent = false; // needed because of a shadow bug
    let window = WindowBuilder::new()
        .with_title("Terminal")
        .with_titlebar_transparent(true)
        .with_transparent(transparent)
        .with_has_shadow(!transparent)
        .with_fullsize_content_view(true)
        .with_decorations(true)
        .with_blur(transparent)
        .build(&event_loop)
        .unwrap();

    // event_loop.set_control_flow(ControlFlow::Poll);

    let mut mono_prop = font_loader::system_fonts::FontPropertyBuilder::new()
        .monospace()
        .build();
    let mut fonts = font_loader::system_fonts::query_specific(&mut mono_prop);
    fonts.dedup();

    // let family = &fonts[rand::prelude::random::<usize>() % fonts.len()];
    // Prefer FiraCode (broad symbol + PUA coverage) over FiraMono (limited).
    let family = fonts
        .iter()
        .find(|f| f.contains("FiraCode") || f.contains("Fira Code"))
        .or_else(|| fonts.iter().find(|f| f.contains("Fira")))
        .expect("no Fira font found");
    println!("selected font {}", family);

    let family_prop = font_loader::system_fonts::FontPropertyBuilder::new()
        .family(family.as_str())
        .build();
    let (family, _) = font_loader::system_fonts::get(&family_prop).unwrap();

    let mut font = font::Font::new(family);
    font.set_char_size(10.0, (window.scale_factor() * 96.0) as u32);

    let mut state = State::new(fdm, window, font).await;
    state.notify_pty_size(state.terminal.cols, state.terminal.rows);
    state.update_vertices();

    let mut theme = state.window.theme().unwrap_or(winit::window::Theme::Light);

    let _ = event_loop.run(move |event, elwt| {
        match event {
            Event::UserEvent(n) => match n {
                app_window::CustomEvent::PtyInput(z) => {
                    state.terminal.feed(&z);
                    state.update_vertices();
                    state.window.request_redraw();
                }
            },
            Event::WindowEvent { window_id, event } if window_id == state.window.id() => {
                if !state.input(&event, elwt) {
                    match event {
                        WindowEvent::ThemeChanged(new_theme) => {
                            theme = new_theme;
                            state.update_vertices();
                            state.window.request_redraw();
                        }
                        WindowEvent::CloseRequested => {
                            elwt.exit();
                        }
                        WindowEvent::Resized(size) => {
                            state.resize(size);
                            state.window.request_redraw();
                        }
                        WindowEvent::ScaleFactorChanged {
                            scale_factor: _scale_factor,
                            ..
                        } => {
                            state.window.request_redraw();
                        }
                        WindowEvent::RedrawRequested => {
                            state.update();
                            match state.render(clear_color(theme)) {
                                Ok(_) => (),
                                Err(wgpu::SurfaceError::Lost) => state.resize(state.gpu.size),
                                Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                                Err(e) => eprintln!("{:?}", e),
                            }
                        }
                        _ => (),
                    }
                }
            }
            Event::AboutToWait => {
                // Application update code.

                // Queue a RedrawRequested event.
                //
                // You only need to call this if you've determined that you need to redraw, in
                // applications which do not always need to. Applications that redraw continuously
                // can just render here instead.
                //window.request_redraw();
            }
            _ => (),
        }
    });
}

fn clear_color(theme: winit::window::Theme) -> wgpu::Color {
    match theme {
        winit::window::Theme::Light => wgpu::Color {
            r: 0.6,
            g: 0.8,
            b: 1.0,
            a: 1.0,
        },
        winit::window::Theme::Dark => wgpu::Color {
            r: 0.01,
            g: 0.01,
            b: 0.01,
            a: 1.0,
        },
    }
}

fn main() {
    pollster::block_on(run());
}
