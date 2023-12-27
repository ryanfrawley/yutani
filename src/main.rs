mod vertex;
mod font;
mod texture;
mod font_loader;
mod instance;
mod camera;

use winit::{
    event::*,
    event_loop::EventLoop,
    window::{WindowBuilder, Window},
    platform::macos::{WindowBuilderExtMacOS, WindowExtMacOS},
};

use wgpu::util::DeviceExt;


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
    font_bind_group: wgpu::BindGroup,
    font_texture: texture::Texture,
    // instances: Vec<instance::Instance>,
    // instance_buffer: wgpu::Buffer,
    camera: camera::Camera,
    camera_uniform: camera::CameraUniform,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
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
            // alpha_mode: surface_caps.alpha_modes[0],
            alpha_mode: wgpu::CompositeAlphaMode::PostMultiplied,
            view_formats: vec![],
        };

        surface.configure(&device, &config);

        // Font texture setup
        let atlas = font.build_atlas();

        let mut vertices: Vec<vertex::Vertex> = Vec::with_capacity(26 * 4);
        let mut indices: Vec<u16> = Vec::new();

        let mut x = 30.0;
        let mut row = 0;
        let color = [1.0, 0.3, 0.75, 1.0];
        for value in atlas.entries.values() {
            let start_idx = vertices.len();
            let u1 = value.x as f32 / 1024.0;
            let v1 = value.y as f32 / 1024.0;
            let u2 = (value.x + value.width) as f32 / 1024.0;
            let v2 = (value.y + value.height) as f32 / 1024.0;
            let y = 60.0 + 26.0 + (row * font.face.height() >> 6) as f32 - value.offset_y as f32;
            let w = value.width as f32;
            let h = value.height as f32;
            println!("{} {}", x, w);
            vertices.push(vertex::Vertex { position: [x, y, 0.0], tex_coords: [u1, v1], color });
            vertices.push(vertex::Vertex { position: [x, y + h, 0.0], tex_coords: [u1, v2], color });
            vertices.push(vertex::Vertex { position: [x + w, y, 0.0], tex_coords: [u2, v1], color });
            vertices.push(vertex::Vertex { position: [x + w, y + h, 0.0], tex_coords: [u2, v2], color });
            for i in start_idx..(start_idx + 3) {
                indices.push(i as u16);
            }
            for i in (start_idx + 1)..=(start_idx + 3) {
                indices.push(i as u16);
            }
            x += value.advance_x as f32;
            if x > 300.0 {
                x = 30.0;
                row += 1;
            }
        }

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

        // TODO
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
                    blend: Some(wgpu::BlendState::REPLACE),
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

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let index_buffer = device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("index buffer"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            },
        );

        let num_indices = indices.len() as u32;

        Self {
            window,
            surface,
            device,
            queue,
            config,
            size,
            render_pipeline,
            vertex_buffer,
            index_buffer,
            num_indices,
            font_bind_group,
            font_texture: font_alpha,
            // instances,
            // instance_buffer,
            camera,
            camera_uniform,
            camera_buffer,
            camera_bind_group,
        }
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
    }

    fn input(&mut self, event: &WindowEvent) -> bool {
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


            // self.instances[0].position.x += 0.001;
            // println!("{}", self.instances[0].position.x);


            // let instance_data = self.instances.iter().map(instance::Instance::to_raw).collect::<Vec<_>>();

            // self.instance_buffer = self.device.create_buffer_init(
            //     &wgpu::util::BufferInitDescriptor {
            //         label: Some("instance buffer"),
            //         contents: bytemuck::cast_slice(&instance_data),
            //         usage: wgpu::BufferUsages::VERTEX,
            //     },
            // );


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
        .with_blur(true)
        .build(&event_loop)
        .unwrap();


    // event_loop.set_control_flow(ControlFlow::Poll);

    let mut mono_prop = font_loader::system_fonts::FontPropertyBuilder::new().monospace().build();
    let mut fonts = font_loader::system_fonts::query_specific(&mut mono_prop);
    fonts.dedup();

    // for name in fonts {
    //     println!("{}", name);
    // }

    let family = &fonts[rand::prelude::random::<usize>() % fonts.len()];
    println!("selected font {}", family);

 	let family_prop = font_loader::system_fonts::FontPropertyBuilder::new().family(family.as_str()).build();
 	let (family, _) = font_loader::system_fonts::get(&family_prop).unwrap();

    let mut font = font::Font::new(family);
    font.set_char_size(13.0, 192);

    let mut state = State::new(window, font).await;

    // let atlas = font.build_atlas();
    // println!("w: {}, h: {}", atlas.width, atlas.height);
    // for y in 0..atlas.height {
    //     for x in 0..atlas.width {
    //         let c = atlas.buffer[(y * atlas.width + x) as usize];
    //         print!("{}", match c {
    //             0 => ' ',
    //             1..=63 => '░',
    //             64..=127 => '▒',
    //             128..=191 => '▓',
    //             192..=255 => '█',
    //         });
    //     }
    //     println!("");
    // }

    let mut theme = state.window.theme().unwrap_or(winit::window::Theme::Light);

    event_loop.run(move |event, elwt| {
        match event {
            Event::WindowEvent { window_id, event} if window_id == state.window.id() => if !state.input(&event) {
                match event {
                    WindowEvent::ThemeChanged(new_theme) => {
                        theme = new_theme;
                        state.window.request_redraw();
                    },
                    WindowEvent::CloseRequested => {
                        elwt.exit();
                    },
                    WindowEvent::Resized(size) => {
                        state.resize(size);
                        state.window.request_redraw();
                    },
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        // state.resize(scale_factor); // TODO
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
            r: 0.1,
            g: 0.2,
            b: 0.4,
            a: 0.6,
        },
        winit::window::Theme::Dark => wgpu::Color {
            r: 0.01,
            g: 0.01,
            b: 0.01,
            a: 0.9,
        },
    }

}

fn main() {
    pollster::block_on(run());
}
