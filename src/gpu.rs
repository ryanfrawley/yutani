/// Adapter-level GPU plumbing, split into the parts that are shared across all
/// windows in the process and the parts that belong to one window's surface.
///
/// `Gpu` (device + queue + the instance that minted them) is process-global:
/// every window records draws against the same device, and new windows create
/// their surfaces from the same `instance`. It lives behind an `Rc` in
/// `AppShared` so the cost of bringing up the adapter/device is paid once.
///
/// The live `wgpu::Surface` for a window is *not* held here: it's owned by that
/// window's present thread (see [`crate::present`]), which drives the swapchain
/// acquire/present off the main thread so a frame waiting on vsync can't stall
/// AppKit (and the native tab bar). The main thread keeps only [`WindowSurface`],
/// a lightweight config/size mirror that layout code reads.
pub struct Gpu {
    /// Kept so additional windows can create their surfaces from the same
    /// instance the device was built against (see [`Gpu::create_surface`]).
    pub instance: wgpu::Instance,
    /// Kept alongside the instance so a new window's surface config can be
    /// derived from the same adapter's capabilities as the first window's.
    pub adapter: wgpu::Adapter,
    /// `Arc` (not bare) so each window's present thread can hold its own handle
    /// to the same device/queue. `&self.device` still coerces to
    /// `&wgpu::Device` at call sites, so existing uses are unchanged.
    pub device: std::sync::Arc<wgpu::Device>,
    pub queue: std::sync::Arc<wgpu::Queue>,
}

/// Per-window swapchain *configuration* mirror: the config the surface was last
/// configured with and the physical size it encodes. The live `wgpu::Surface`
/// lives on the present thread; this mirror stays on the main thread because
/// lots of layout code reads `config.width/height`.
pub struct WindowSurface {
    pub config: wgpu::SurfaceConfiguration,
    pub size: winit::dpi::PhysicalSize<u32>,
}

impl Gpu {
    /// Bring up the instance, adapter, device, and queue, and build the first
    /// window's surface. Returns the shared `Gpu`, that window's config mirror,
    /// and the raw `wgpu::Surface` (which the caller hands to the window's
    /// present thread). Subsequent windows reuse the `Gpu` and call
    /// [`Gpu::create_surface`].
    pub async fn new(
        window: &winit::window::Window,
    ) -> (Self, WindowSurface, wgpu::Surface) {
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
            adapter,
            device: std::sync::Arc::new(device),
            queue: std::sync::Arc::new(queue),
        };
        let window_surface = WindowSurface { config, size };
        (gpu, window_surface, surface)
    }

    /// Build a surface for an additional window from the shared instance,
    /// configured against the same device/adapter as the first window. Returns
    /// the config mirror plus the raw surface for the window's present thread.
    /// The surface holds an unsafe reference to `window`, so the caller must
    /// keep `window` alive at least as long as the surface (and drop the
    /// surface first) — the present thread is stopped/joined in
    /// `WindowState`'s `Drop` before the window field drops.
    pub fn create_surface(
        &self,
        window: &winit::window::Window,
    ) -> (WindowSurface, wgpu::Surface) {
        let size = window.inner_size();
        let surface = unsafe { self.instance.create_surface(window) }.unwrap();
        let config = surface_config(&surface, &self.adapter, size);
        surface.configure(&self.device, &config);
        (WindowSurface { config, size }, surface)
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
