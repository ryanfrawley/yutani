mod vertex;
mod font;
mod texture;
mod font_loader;
mod camera;
mod ring_buffer;

mod console;
mod tokenizer;
mod echo;
mod pwd;

use winit::{
    event::*,
    event_loop::EventLoop,
    event_loop::EventLoopWindowTarget,
    window::{WindowBuilder, Window},
    platform::macos::WindowBuilderExtMacOS,
};

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
    surface: wgpu::Surface,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: winit::dpi::PhysicalSize<u32>,

    // The window must be declared after the surface so
    // it gets dropped after it as the surface contains
    // unsafe references to the window's resources.
    window: Window,

    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    font: font::Font,
    font_bind_group: wgpu::BindGroup,
    camera: camera::Camera,
    camera_uniform: camera::CameraUniform,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    atlas: font::Atlas,
    console: console::Console,
    scroll_y: f64,
    mouse_x: f64,
    mouse_y: f64,
}

impl State {
    async fn new(window: Window, mut font: font::Font) -> Self {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = unsafe {
            instance.create_surface(&window)
        }.unwrap();

        let adapter = instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }
        ).await.unwrap();

        let (device, queue) = adapter.request_device(
            &wgpu::DeviceDescriptor {
                features: wgpu::Features::empty(),
                limits: if cfg!(target_arch = "wasm32") {
                    wgpu::Limits::downlevel_webgl2_defaults()
                } else {
                    wgpu::Limits::default()
                },
                label: None,
            },
            None, // trace path
        ).await.unwrap();

        let surface_caps = surface.get_capabilities(&adapter);

        let surface_format = surface_caps.formats.iter()
            .copied()
            .filter(|f| f.is_srgb())
            .next()
            .unwrap_or(surface_caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: surface_caps.present_modes[0],
            alpha_mode: wgpu::CompositeAlphaMode::PostMultiplied,
            view_formats: vec![],
        };

        surface.configure(&device, &config);

        // Font texture setup
        let atlas = font.build_atlas();


        let font_alpha = texture::Texture::from_memory(
            &device,
            &queue,
            &atlas.buffer,
            atlas.width as u32,
            atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture")
        );

        let texture_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
            label: Some("font texture bind group layout")
        });

        let font_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
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

        let camera = camera::Camera {};
        let mut camera_uniform = camera::CameraUniform::new();
        camera_uniform.update_view_proj(&camera, config.width as f32, config.height as f32);

        let camera_buffer = device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("camera buffer"),
                contents: bytemuck::cast_slice(&[camera_uniform]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            },
        );

        let camera_bind_group_layout = device.create_bind_group_layout(
            &wgpu::BindGroupLayoutDescriptor {
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    }
                ],
                label: Some("camera bind group layout"),
            },
        );

        let camera_bind_group = device.create_bind_group(
            &wgpu::BindGroupDescriptor {
                layout: &camera_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: camera_buffer.as_entire_binding(),
                    },
                ],
                label: Some("camera bind group"),
            },
        );


        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

        let render_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render pipeline layout"),
            bind_group_layouts: &[
                &texture_bind_group_layout,
                &camera_bind_group_layout,
            ],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[
                    vertex::Vertex::desc(),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
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
                config.width as f32,
                config.height as f32,
                (metrics.max_advance >> 6) as usize,
                (metrics.height >> 6) as usize);
        let mut vertex_buf: Vec<u8> = Vec::with_capacity((viewport.char_height * viewport.char_width + 1) * std::mem::size_of::<vertex::Vertex>() * 4);
        for _ in 0..vertex_buf.capacity() {
            vertex_buf.push(0);
        }
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: &bytemuck::cast_slice(&vertex_buf),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let mut index_buf: Vec<u8> = Vec::with_capacity((viewport.char_height * viewport.char_width + 1) * std::mem::size_of::<u16>() * 6);
        for _ in 0..index_buf.capacity() {
            index_buf.push(0);
        }
        let index_buffer = device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("index buffer"),
                contents: &bytemuck::cast_slice(&index_buf),
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            },
        );

        Self {
            window,
            surface,
            device,
            queue,
            config,
            atlas,
            size,
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
            console: console::Console::new(100, 80, 10000, 1024),
            scroll_y: 0.0,
            mouse_x: 0.0,
            mouse_y: 0.0,
        }
    }

    fn get_viewport_size(width: f32, height: f32, advance_x: usize, line_height: usize) -> ViewportSize {
        ViewportSize {
            char_width: usize::max(1, (width - WINDOW_PADDING * 2.0) as usize / advance_x),
            char_height: usize::max(1, (height - DECORATOR_HEIGHT - WINDOW_PADDING * 2.0) as usize / line_height),
        }
    }

    fn resize_buffers(&mut self) {
        // Calculate console viewport & buffer sizes
        let metrics = self.font.face.size_metrics().unwrap();
        let viewport = State::get_viewport_size(
            self.config.width as f32,
            self.config.height as f32,
            (metrics.max_advance >> 6) as usize,
            (metrics.height >> 6) as usize);
        println!("w: {} h: {}", viewport.char_width, viewport.char_height);
        let mut vertex_buf: Vec<u8> = Vec::with_capacity(((2 * viewport.char_height * viewport.char_width) + 1) * std::mem::size_of::<vertex::Vertex>() * 4);
        for _ in 0..vertex_buf.capacity() {
            vertex_buf.push(0);
        }
        self.vertex_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: &bytemuck::cast_slice(&vertex_buf),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let mut index_buf: Vec<u8> = Vec::with_capacity((2 * (viewport.char_height * viewport.char_width) + 1) * std::mem::size_of::<u16>() * 6);
        for _ in 0..index_buf.capacity() {
            index_buf.push(0);
        }
        self.index_buffer = self.device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("index buffer"),
                contents: &bytemuck::cast_slice(&index_buf),
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            },
        );
    }

    fn update_vertices(&mut self) {
        let area = self.console.columns * self.console.rows;
        let mut vertices: Vec<vertex::Vertex> = Vec::with_capacity(4 * 2 * (area + 1));
        let mut indices: Vec<u16> = Vec::with_capacity(6 * 2 * (area + 1));

        let mut x = WINDOW_PADDING;
        let mut row = 1;

        let theme = self.window.theme().unwrap_or(winit::window::Theme::Light);
        let metrics = self.font.face.size_metrics().unwrap();
        let line_height = metrics.height >> 6;
        let mut column = 0;

        let mut draw_char = |c, r, col, color_bg, color_fg| {
            let mut column = col;
            let mut row = r;
            match c {
                '\n' => {
                    row += 1;
                    column = 0;
                    x = WINDOW_PADDING;
                },
                _ => {
                    let value = &self.atlas.entries[&c];
                    let w = value.width as f32;
                    let mut start_idx = vertices.len();
                    let u1 = value.x as f32 / self.atlas.width as f32;
                    let v1 = value.y as f32 / self.atlas.height as f32;
                    let u2 = (value.x + value.width) as f32 / self.atlas.width as f32;
                    let v2 = (value.y + value.height) as f32 / self.atlas.height as f32;
                    let y = WINDOW_PADDING + DECORATOR_HEIGHT + (row * line_height) as f32 - value.bearing_y as f32 + self.scroll_y as f32;
                    let h = value.height as f32;

                    // background
                    let bg_h = ((metrics.ascender - metrics.descender) >> 6) as f32;
                    let bg_y = WINDOW_PADDING + DECORATOR_HEIGHT + (row * line_height) as f32 - bg_h - (metrics.descender >> 6) as f32 + self.scroll_y as f32;
                    let bg_w = (metrics.max_advance >> 6) as f32;
                    let bg_uv = 1.0 / self.atlas.width as f32; // todo: split into height & width
                    vertices.push(vertex::Vertex { position: [x, bg_y, 0.0], tex_coords: [bg_uv, bg_uv], color: color_bg });
                    vertices.push(vertex::Vertex { position: [x, bg_y + bg_h, 0.0], tex_coords: [bg_uv, bg_uv], color: color_bg });
                    vertices.push(vertex::Vertex { position: [x + bg_w, bg_y, 0.0], tex_coords: [bg_uv, bg_uv], color: color_bg });
                    vertices.push(vertex::Vertex { position: [x + bg_w, bg_y + bg_h, 0.0], tex_coords: [bg_uv, bg_uv], color: color_bg });

                    for i in start_idx..(start_idx + 3) {
                        indices.push(i as u16);
                    }
                    for i in (start_idx + 1)..=(start_idx + 3) {
                        indices.push(i as u16);
                    }

                    start_idx = vertices.len();

                    // foreground
                    vertices.push(vertex::Vertex { position: [x + value.bearing_x as f32, y, 0.0], tex_coords: [u1, v1], color: color_fg });
                    vertices.push(vertex::Vertex { position: [x + value.bearing_x as f32, y + h, 0.0], tex_coords: [u1, v2], color: color_fg });
                    vertices.push(vertex::Vertex { position: [x + value.bearing_x as f32 + w, y, 0.0], tex_coords: [u2, v1], color: color_fg });
                    vertices.push(vertex::Vertex { position: [x + value.bearing_x as f32 + w, y + h, 0.0], tex_coords: [u2, v2], color: color_fg });
                    for i in start_idx..(start_idx + 3) {
                        indices.push(i as u16);
                    }
                    for i in (start_idx + 1)..=(start_idx + 3) {
                        indices.push(i as u16);
                    }

                    // advance cursor
                    x += value.advance_x as f32;
                    column += 1;
                    if column == self.console.columns {
                        column = 0;
                        x = WINDOW_PADDING;
                        row += 1;
                    }
                }
            }
            (row, column)
        };

        let color_fg = match theme {
            winit::window::Theme::Dark => [0.0, 0.9 ,0.9, 1.0],
            winit::window::Theme::Light => [0.0, 0.0, 0.0, 1.0],
        };

        // draw the buffer view

        for c in self.console.iter_view().map(|c| *c) {
            // let bg_scale = 0.5 + (column as f32 / self.console.columns as f32) * 0.5;
            (row, column) = draw_char(c, row, column, [0.0, 0.0, 0.0, 0.0], color_fg);
            if row as usize > self.console.rows {
                break;
            }
        }

        // draw the input
        for c in self.console.input.chars() {
            if row as usize > self.console.rows {
                break;
            }
            (row, column) = draw_char(c, row, column, [0.0, 0.0, 0.0, 0.0], color_fg);
        }

        // add the cursor
        x = WINDOW_PADDING + ((self.console.cursor_offset % self.console.columns) * (metrics.max_advance >> 6) as usize) as f32;
        let input_offset =
            (self.console.input.len() / self.console.columns) as i64 - 
            (self.console.cursor_offset / self.console.columns) as i64;
        let h = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let y = WINDOW_PADDING + DECORATOR_HEIGHT + ((row - input_offset) * line_height) as f32 - h - (metrics.descender >> 6) as f32 + self.scroll_y as f32;
        let w = (self.font.face.size_metrics().unwrap().max_advance >> 6) as f32;
        let cursor_color = match theme {
            winit::window::Theme::Light => [0.1, 0.0, 0.8, 1.0],
            winit::window::Theme::Dark => [0.9, 0.9, 0.9, 1.0],
        };
        let start = vertices.len();
        let bg_u = 1.0 / self.atlas.width as f32;
        let bg_v = 1.0 / self.atlas.height as f32;
        vertices.push(vertex::Vertex { position: [x, y, 0.0], tex_coords: [bg_u, bg_v], color: cursor_color });
        vertices.push(vertex::Vertex { position: [x, y + h, 0.0], tex_coords: [bg_u, bg_v], color: cursor_color });
        vertices.push(vertex::Vertex { position: [x + w, y, 0.0], tex_coords: [bg_u, bg_v], color: cursor_color });
        vertices.push(vertex::Vertex { position: [x + w, y + h, 0.0], tex_coords: [bg_u, bg_v], color: cursor_color });

        for i in start..(start + 3) {
            indices.push(i as u16);
        }
        for i in (start + 1)..=(start + 3) {
            indices.push(i as u16);
        }

        // Update buffers
        self.queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));
        self.queue.write_buffer(&self.index_buffer, 0, bytemuck::cast_slice(&indices));
        self.num_indices = indices.len() as u32;
    }

    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.camera_uniform.update_view_proj(&self.camera, size.width as f32, size.height as f32);
        self.queue.write_buffer(&self.camera_buffer, 0, bytemuck::cast_slice(&[self.camera_uniform]));
        let metrics = self.font.face.size_metrics().unwrap();
        let size = State::get_viewport_size(
            self.config.width as f32,
            self.config.height as f32,
            (metrics.max_advance >> 6) as usize,
            (metrics.height >> 6) as usize);
        self.console.resize(size.char_width, size.char_height);
        self.resize_buffers();
        self.update_vertices();
    }

    fn process_input(&mut self, elwt: &EventLoopWindowTarget<()>) {
        let args = tokenizer::tokenize(&self.console.input);
        self.console.write_input();

        if args.len() == 0 {
            return;
        }

        match args[0].as_str() {
            "pwd" => pwd::pwd(&mut self.console, &args[1..]),
            "echo" => echo::echo(&mut self.console, &args[1..]),
            "exit" => { elwt.exit(); return; },
            _ => self.console.write(&format!("unrecognized command: {}", args[0])),
        };

        self.console.write("\n");
    }

    fn input(&mut self, event: &WindowEvent, elwt: &EventLoopWindowTarget<()>) -> bool {
        match event {
            WindowEvent::MouseInput { device_id, state, button } => {
                match button {
                    MouseButton::Left => {
                        match state {
                            ElementState::Pressed => {
                                let x = 
                                    (self.mouse_x - WINDOW_PADDING as f64) /
                                    (self.config.width as f64 - 2.0 * WINDOW_PADDING as f64);

                                let y = 
                                    (self.mouse_y + self.scroll_y - (WINDOW_PADDING + DECORATOR_HEIGHT) as f64) /
                                    (self.config.height as f64 - 2.0 * WINDOW_PADDING as f64);

                                let col = (x * self.console.columns as f64).floor();
                                let row = (y * self.console.rows as f64).floor();
                            },
                            _ => (),
                        }
                    },
                    _ => (),
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_x = position.x;
                self.mouse_y = position.y;
            },
            WindowEvent::MouseWheel { delta, .. } => {
                match delta {
                    MouseScrollDelta::LineDelta(_r, d) => {
                        match d {
                            _ if d > &0.0 => {
                                for _ in 0..(d.round() as usize) {
                                    self.console.scroll_down();
                                }
                            },
                            _ => {
                                for _ in 0..(d.round().abs() as usize) {
                                    self.console.scroll_up();
                                }
                            }
                        }
                    },
                    MouseScrollDelta::PixelDelta(p) => {
                        let height = (self.font.face.size_metrics().unwrap().height >> 6) as f64;
                        let new_scroll = self.scroll_y + p.y;

                        if new_scroll >= height {
                            self.console.scroll_up();
                        } else if new_scroll.abs() >= height {
                            self.console.scroll_down();
                        }

                        let delta = new_scroll - self.scroll_y;

                        self.scroll_y = new_scroll % height as f64;

                        if self.console.scroll_y == 0 && self.scroll_y > 0.0 {
                            self.scroll_y = 0.0;
                        } else if self.console.scroll_ptr >= self.console.buffer.len() && self.scroll_y < 0.0 {
                            self.scroll_y = 0.0;
                        }
                    }
                }
                self.update_vertices();
                self.window.request_redraw();
                return true;
            },
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == winit::event::ElementState::Pressed {
                    match event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => {
                            self.process_input(elwt);
                        },
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Backspace) => {
                            self.console.delete_left();
                        },
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowUp) => {
                            // self.console.scroll_up();
                        },
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowDown) => {
                            // self.console.scroll_down();
                        },
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowLeft) => {
                            self.console.cursor_left();
                        },
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowRight) => {
                            self.console.cursor_right();
                        },
                        _ => {
                            let k = event.text.to_owned().unwrap_or_default();
                            match k.chars().next() {
                                Some(k) => { 
                                    self.console.insert_right(k);
                                },
                                None => (),
                            };
                        },
                    };
                    self.update_vertices();
                    self.window.request_redraw();
                    return true;
                }
            },
            _ => (),
        }
        false
    }

    fn update(&mut self) {

    }

    fn render(&mut self, clear: wgpu::Color) -> Result<(), wgpu::SurfaceError> {
        println!("render");
        let output = self.surface.get_current_texture().unwrap();
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("terminal")
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
                    }
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

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok(())
    }
}

async fn run() {
    env_logger::init();
    let event_loop = EventLoop::new().unwrap();
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

    let mut mono_prop = font_loader::system_fonts::FontPropertyBuilder::new().monospace().build();
    let mut fonts = font_loader::system_fonts::query_specific(&mut mono_prop);
    fonts.dedup();

    // let family = &fonts[rand::prelude::random::<usize>() % fonts.len()];
    let family = &fonts.iter().find(|f| f.contains("Fira")).unwrap();
    println!("selected font {}", family);

 	let family_prop = font_loader::system_fonts::FontPropertyBuilder::new().family(family.as_str()).build();
 	let (family, _) = font_loader::system_fonts::get(&family_prop).unwrap();

    let mut font = font::Font::new(family);
    font.set_char_size(10.0, (window.scale_factor() * 96.0) as u32);

    let mut state = State::new(window, font).await;
    state.update_vertices();

    let mut theme = state.window.theme().unwrap_or(winit::window::Theme::Light);

    let _ = event_loop.run(move |event, elwt| {
        match event {
            Event::WindowEvent { window_id, event} if window_id == state.window.id() => if !state.input(&event, elwt) {
                match event {
                    WindowEvent::ThemeChanged(new_theme) => {
                        theme = new_theme;
                        state.update_vertices();
                        state.window.request_redraw();
                    },
                    WindowEvent::CloseRequested => {
                        elwt.exit();
                    },
                    WindowEvent::Resized(size) => {
                        state.resize(size);
                        state.window.request_redraw();
                    },
                    WindowEvent::ScaleFactorChanged { scale_factor: _scale_factor, .. } => {
                        state.window.request_redraw();
                    },
                    WindowEvent::RedrawRequested => {
                        state.update();
                        match state.render(clear_color(theme)) {
                            Ok(_) => (),
                            Err(wgpu::SurfaceError::Lost) => state.resize(state.size),
                            Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                            Err(e) => eprintln!("{:?}", e)
                        }
                    },
                    _ => ()
                }
            },
            Event::AboutToWait => {
                // Application update code.

                // Queue a RedrawRequested event.
                //
                // You only need to call this if you've determined that you need to redraw, in
                // applications which do not always need to. Applications that redraw continuously
                // can just render here instead.
                //window.request_redraw();
            },
            // Event::WindowEvent {
            //     event: WindowEvent::RedrawRequested,
            //     ..
            // } => {
            //     // Redraw the application.
            //     //
            //     // It's preferable for applications that do not render continuously to render in
            //     // this event rather than in AboutToWait, since rendering in here allows
            //     // the program to gracefully handle redraws requested by the OS.
            //     println!("redraw");
            // },
            _ => ()
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
