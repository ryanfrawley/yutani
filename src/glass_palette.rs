//! Native macOS "Liquid Glass" command palette panel.
//!
//! Replaces the GPU-drawn palette overlay with a real AppKit panel: a
//! borderless `NSPanel` (subclassed only so a borderless window may still
//! become key for text input) layered over the terminal window as a child,
//! its content embedded in an `NSGlassEffectView` (macOS 26+, with an
//! `NSVisualEffectView` fallback on older systems). The pure `command_palette`
//! model still owns the logic; this is the native view + input front end.
//!
//! On non-macOS targets this is a no-op stub so `WindowState` stays uniform.

#[cfg(not(target_os = "macos"))]
mod imp {
    use winit::window::Window;

    /// No-op stand-in on platforms without the native panel.
    pub struct GlassPalette;

    impl GlassPalette {
        pub fn show(&mut self, _parent: &Window) {}
        pub fn hide(&mut self) {}
        pub fn visible(&self) -> bool {
            false
        }
    }

    /// Always `None` off macOS; the palette falls back to the GPU overlay.
    pub fn new() -> Option<GlassPalette> {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{class, define_class, msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSColor, NSFont, NSPanel, NSTextField, NSView};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    use winit::window::Window;

    /// Raised by the panel's `cancelOperation:` (Escape) so the event loop can
    /// sync the Rust-side open state and tear the panel down. Polled like
    /// `NEW_TAB_REQUESTED`.
    pub static PALETTE_DISMISS_REQUESTED: AtomicBool = AtomicBool::new(false);

    // Geometry (points). The panel is a fixed-width card near the top of the
    // window; height grows with the results list in later slices.
    const PANEL_WIDTH: f64 = 560.0;
    const PANEL_HEIGHT: f64 = 56.0;
    const TOP_INSET: f64 = 96.0;
    const FIELD_INSET_X: f64 = 18.0;
    const FIELD_HEIGHT: f64 = 30.0;

    define_class! {
        // Subclassed only so a borderless window may still become key (and thus
        // accept text input); a plain borderless NSWindow returns NO below.
        #[unsafe(super(NSPanel))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniPalettePanel"]
        struct PalettePanel;

        impl PalettePanel {
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key_window(&self) -> bool {
                true
            }

            // Escape routes to cancelOperation: on the first responder chain.
            // Hide immediately and flag the loop to sync model state.
            #[unsafe(method(cancelOperation:))]
            fn cancel_operation(&self, _sender: *mut AnyObject) {
                unsafe {
                    let _: () = msg_send![self, orderOut: std::ptr::null_mut::<AnyObject>()];
                }
                PALETTE_DISMISS_REQUESTED.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Owns the live AppKit objects for one window's palette.
    pub struct GlassPalette {
        panel: Retained<PalettePanel>,
        glass: Retained<NSView>,
        container: Retained<NSView>,
        field: Retained<NSTextField>,
        visible: bool,
    }

    /// Build the panel + glass + search field once (lazily, on first open).
    pub fn new() -> Option<GlassPalette> {
        let mtm = MainThreadMarker::new()?;
        unsafe {
            let content = NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
            );

            // NSWindowStyleMaskNonactivatingPanel (1<<7): the panel can take key
            // status for text input without deactivating the owning app.
            let style: usize = 1 << 7;
            let alloc = mtm.alloc::<PalettePanel>();
            let panel: Retained<PalettePanel> = msg_send![
                alloc,
                initWithContentRect: content,
                styleMask: style,
                backing: 2usize, // NSBackingStoreBuffered
                defer: false,
            ];
            let _: () = msg_send![&*panel, setReleasedWhenClosed: false];
            let _: () = msg_send![&*panel, setOpaque: false];
            let clear = NSColor::clearColor();
            let _: () = msg_send![&*panel, setBackgroundColor: &*clear];
            let _: () = msg_send![&*panel, setHasShadow: true];
            let _: () = msg_send![&*panel, setLevel: 3isize]; // NSFloatingWindowLevel

            // Container holds the field (and, later, the results table).
            let container: Retained<NSView> =
                msg_send![mtm.alloc::<NSView>(), initWithFrame: content];

            // Search field: borderless, transparent, large system font.
            let field: Retained<NSTextField> =
                msg_send![mtm.alloc::<NSTextField>(), initWithFrame: field_frame()];
            let _: () = msg_send![&*field, setBezeled: false];
            let _: () = msg_send![&*field, setBordered: false];
            let _: () = msg_send![&*field, setDrawsBackground: false];
            let _: () = msg_send![&*field, setFocusRingType: 1isize]; // None
            let placeholder = NSString::from_str("Run a command…");
            let _: () = msg_send![&*field, setPlaceholderString: &*placeholder];
            let font: Retained<NSFont> = msg_send![class!(NSFont), systemFontOfSize: 18.0f64];
            let _: () = msg_send![&*field, setFont: &*font];
            container.addSubview(&field);

            // Glass effect (macOS 26+) wrapping the container, else a blurred
            // NSVisualEffectView with the container as a subview.
            let glass = make_glass(content, &container);
            let _: () = msg_send![&*panel, setContentView: &*glass];

            Some(GlassPalette {
                panel,
                glass,
                container,
                field,
                visible: false,
            })
        }
    }

    fn field_frame() -> NSRect {
        NSRect::new(
            NSPoint::new(FIELD_INSET_X, (PANEL_HEIGHT - FIELD_HEIGHT) / 2.0),
            NSSize::new(PANEL_WIDTH - FIELD_INSET_X * 2.0, FIELD_HEIGHT),
        )
    }

    /// Create the glass backing view embedding `content`. Prefers the real
    /// Liquid Glass material; falls back to a rounded vibrancy view.
    unsafe fn make_glass(frame: NSRect, content: &NSView) -> Retained<NSView> {
        if let Some(cls) = AnyClass::get(c"NSGlassEffectView") {
            let v: *mut AnyObject = msg_send![cls, alloc];
            let v: *mut AnyObject = msg_send![v, initWithFrame: frame];
            let _: () = msg_send![v, setCornerRadius: 14.0f64];
            let _: () = msg_send![v, setContentView: content];
            return Retained::from_raw(v as *mut NSView).expect("glass view");
        }
        // Fallback: NSVisualEffectView (rounded, HUD material, behind-window).
        let cls = class!(NSVisualEffectView);
        let v: *mut AnyObject = msg_send![cls, alloc];
        let v: *mut AnyObject = msg_send![v, initWithFrame: frame];
        let _: () = msg_send![v, setMaterial: 18isize]; // HUDWindow
        let _: () = msg_send![v, setBlendingMode: 0isize]; // BehindWindow
        let _: () = msg_send![v, setState: 1isize]; // Active
        let _: () = msg_send![v, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![v, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setCornerRadius: 14.0f64];
            let _: () = msg_send![layer, setMasksToBounds: true];
        }
        let glass = Retained::from_raw(v as *mut NSView).expect("visual effect view");
        glass.addSubview(content);
        glass
    }

    /// Read the parent terminal window's `NSWindow*` via its raw handle.
    fn parent_nswindow(window: &Window) -> Option<*mut AnyObject> {
        let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
            return None;
        };
        unsafe {
            let ns_view = handle.ns_view as *mut AnyObject;
            let ns_window: *mut AnyObject = msg_send![ns_view, window];
            (!ns_window.is_null()).then_some(ns_window)
        }
    }

    impl GlassPalette {
        pub fn visible(&self) -> bool {
            self.visible
        }

        /// Position over `parent` (centered, near the top), attach as a child
        /// window, show it, and focus the search field.
        pub fn show(&mut self, parent: &Window) {
            let Some(parent_ns) = parent_nswindow(parent) else {
                return;
            };
            unsafe {
                let pf: NSRect = msg_send![parent_ns, frame];
                let w = PANEL_WIDTH;
                let h = PANEL_HEIGHT;
                let x = pf.origin.x + (pf.size.width - w) / 2.0;
                // Screen coords are y-up: subtract from the top edge.
                let y = pf.origin.y + pf.size.height - h - TOP_INSET;
                let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));

                let local = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
                let _: () = msg_send![&*self.glass, setFrame: local];
                let _: () = msg_send![&*self.container, setFrame: local];
                let _: () = msg_send![&*self.field, setFrame: field_frame()];
                let _: () = msg_send![&*self.panel, setFrame: frame, display: true];

                // NSWindowAbove == 1.
                let _: () = msg_send![parent_ns, addChildWindow: &*self.panel, ordered: 1isize];
                let _: () = msg_send![
                    &*self.panel,
                    makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()
                ];
                let _: bool = msg_send![&*self.panel, makeFirstResponder: &*self.field];
            }
            self.visible = true;
        }

        /// Introspect live AppKit state for non-visual verification (used by the
        /// `YUTANI_PALETTE_DEMO` smoke check, since screen capture is sandboxed).
        pub fn debug_report(&self) -> String {
            unsafe {
                let visible: bool = msg_send![&*self.panel, isVisible];
                let key: bool = msg_send![&*self.panel, isKeyWindow];
                let content: *mut AnyObject = msg_send![&*self.panel, contentView];
                let cls: *const AnyClass = msg_send![content, class];
                let cls_name = if cls.is_null() {
                    "<null>".to_string()
                } else {
                    (*cls).name().to_string_lossy().into_owned()
                };
                let frame: NSRect = msg_send![&*self.panel, frame];
                format!(
                    "glass panel: visible={visible} key={key} contentView={cls_name} \
                     frame=({:.0},{:.0} {:.0}x{:.0})",
                    frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
                )
            }
        }

        /// Detach from the parent and hide.
        pub fn hide(&mut self) {
            unsafe {
                let parent: *mut AnyObject = msg_send![&*self.panel, parentWindow];
                if !parent.is_null() {
                    let _: () = msg_send![parent, removeChildWindow: &*self.panel];
                }
                let _: () = msg_send![&*self.panel, orderOut: std::ptr::null_mut::<AnyObject>()];
            }
            self.visible = false;
        }
    }
}

pub use imp::*;
