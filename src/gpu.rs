/// Adapter-level GPU plumbing, split into the parts that are shared across all
/// windows in the process and the parts that belong to one window's surface.
///
/// `Gpu` (device + queue + the instance that minted them) is process-global:
/// every window records draws against the same device, and new windows create
/// their surfaces from the same `instance`. It lives behind an `Rc` in
/// `AppShared` so the cost of bringing up the adapter/device is paid once.
///
/// `WindowSurface` (surface + its config + size) is per-window: the surface is
/// tied to one `NSView`, so each window owns its own and reconfigures it on
/// resize.
pub struct Gpu {
    /// Kept so additional windows can create their surfaces from the same
    /// instance/adapter the device was built against. Unused until the
    /// multi-window factory (Stage 3) creates per-window surfaces from it.
    #[allow(dead_code)]
    pub instance: wgpu::Instance,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// The surface for one window, plus the config it was last configured with and
/// the physical size that config encodes.
pub struct WindowSurface {
    pub surface: wgpu::Surface,
    pub config: wgpu::SurfaceConfiguration,
    pub size: winit::dpi::PhysicalSize<u32>,
}

impl Gpu {
    /// Bring up the instance, adapter, device, and queue, and build the first
    /// window's surface. Returns the shared `Gpu` and that window's
    /// `WindowSurface`. Subsequent windows reuse the `Gpu` and call
    /// [`Gpu::create_surface`].
    pub async fn new(window: &winit::window::Window) -> (Self, WindowSurface) {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = unsafe { instance.create_surface(window) }.unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();
        // Opt into POLYGON_MODE_LINE if the adapter offers it (Metal, Vulkan,
        // DX12 do; downlevel/WebGL don't). Drives the wireframe debug view.
        let optional = wgpu::Features::POLYGON_MODE_LINE;
        let features = adapter.features() & optional;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    features,
                    limits: if cfg!(target_arch = "wasm32") {
                        wgpu::Limits::downlevel_webgl2_defaults()
                    } else {
                        wgpu::Limits::default()
                    },
                    label: None,
                },
                None,
            )
            .await
            .unwrap();

        let config = surface_config(&surface, &adapter, size);
        surface.configure(&device, &config);

        let gpu = Self {
            instance,
            device,
            queue,
        };
        let window_surface = WindowSurface {
            surface,
            config,
            size,
        };
        (gpu, window_surface)
    }
}

/// Pick the surface config (format / present mode / alpha) for a surface on
/// this adapter at the given size. Shared so the first window and any later
/// window derive an identical config — the multi-window plan assumes a uniform
/// surface format across windows.
fn surface_config(
    surface: &wgpu::Surface,
    adapter: &wgpu::Adapter,
    size: winit::dpi::PhysicalSize<u32>,
) -> wgpu::SurfaceConfiguration {
    let surface_caps = surface.get_capabilities(adapter);
    let surface_format = surface_caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(surface_caps.formats[0]);
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: size.width,
        height: size.height,
        present_mode: surface_caps.present_modes[0],
        alpha_mode: wgpu::CompositeAlphaMode::PostMultiplied,
        view_formats: vec![],
    }
}

impl WindowSurface {
    /// Reconfigure the surface for a new physical size. No-op if either
    /// dimension is zero (minimized window). Takes the shared device since the
    /// surface no longer owns it.
    pub fn resize(&mut self, device: &wgpu::Device, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(device, &self.config);
    }
}
