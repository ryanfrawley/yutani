//! Native macOS "Liquid Glass" About panel.
//!
//! Replaces the stock `orderFrontStandardAboutPanel:` box that winit's default
//! menu wires up. Same surface as the command palette / find bar: a borderless
//! `NSPanel` backed by an `NSGlassEffectView` (macOS 26+, with the
//! `NSVisualEffectView` fallback) and our deep rounded drop shadow — but instead
//! of being a child of a terminal window it floats centred on the active screen,
//! showing the app icon, the name "Yutani", and the version.
//!
//! The menu item can't reach the event loop directly, so picking "About Yutani"
//! posts to a `SignalSlot` drained in `about_to_wait`, which then shows the
//! (lazily built, process-global) panel. Closing is self-managed: the panel
//! fades itself out on Escape or when it loses key (a click elsewhere), so no
//! close signal travels back.
//!
//! On non-macOS targets this is a no-op stub so `App` stays uniform.

#[cfg(not(target_os = "macos"))]
mod imp {
    pub struct GlassAbout;

    impl GlassAbout {
        pub fn show(&mut self, _dark: bool) {}
    }

    pub fn new() -> Option<GlassAbout> {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject, Sel};
    use objc2::{
        class, define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly,
    };
    use objc2_app_kit::{NSColor, NSFont, NSPanel, NSTextField, NSView, NSWindowDelegate};
    use objc2_foundation::{NSNotification, NSObject, NSPoint, NSRect, NSSize, NSString};

    use crate::glass::{self, SignalSlot};

    /// Set when the user picks "About Yutani"; drained by the event loop, which
    /// shows the panel. The payload is unit — the only signal is "show it".
    static ABOUT_SIGNAL: SignalSlot<()> = SignalSlot::new();

    /// Post a request to show the About panel (called from the menu action).
    pub fn request_about() {
        ABOUT_SIGNAL.post(());
    }

    /// Drain a pending show-About request.
    pub fn take_about_request() -> bool {
        ABOUT_SIGNAL.take().is_some()
    }

    // Geometry (points). The card centres on screen; the window around it has a
    // transparent gutter (`SHADOW_MARGIN`) for the drop shadow.
    const CARD_WIDTH: f64 = 300.0;
    const CARD_HEIGHT: f64 = 240.0;
    const ICON: f64 = 96.0;
    const TOP_PAD: f64 = 30.0;
    const SHADOW_MARGIN: f64 = 50.0;
    const CORNER_RADIUS: f64 = 20.0;
    /// The project URL, shown as a clickable link in place of a plain tagline.
    const PROJECT_URL: &str = "https://yutani.sh";

    fn window_size() -> NSSize {
        NSSize::new(CARD_WIDTH + SHADOW_MARGIN * 2.0, CARD_HEIGHT + SHADOW_MARGIN * 2.0)
    }
    fn content_rect() -> NSRect {
        NSRect::new(NSPoint::new(0.0, 0.0), window_size())
    }
    fn card_rect() -> NSRect {
        NSRect::new(
            NSPoint::new(SHADOW_MARGIN, SHADOW_MARGIN),
            NSSize::new(CARD_WIDTH, CARD_HEIGHT),
        )
    }
    fn card_bounds() -> NSRect {
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(CARD_WIDTH, CARD_HEIGHT))
    }

    /// A frame `h` points tall whose top edge sits `y_top` points below the
    /// card's top edge. Card coords are y-up, so flip through `CARD_HEIGHT`.
    fn from_top(x: f64, w: f64, y_top: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, CARD_HEIGHT - y_top - h), NSSize::new(w, h))
    }

    /// Fade `panel` out over ~0.14s, then order it off screen on the next tick.
    /// Shared by Escape, an in-card click, and the resign-key delegate; a free
    /// fn so all three paths agree and none has to call a sibling method.
    unsafe fn fade_out<P: objc2::Message>(panel: &P) {
        glass::animate_alpha(panel, 0.0, 0.14);
        let _: () = msg_send![
            panel,
            performSelector: sel!(orderOut:),
            withObject: std::ptr::null_mut::<AnyObject>(),
            afterDelay: 0.15f64,
        ];
    }

    define_class! {
        #[unsafe(super(NSPanel))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniAboutPanel"]
        struct AboutPanel;

        impl AboutPanel {
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key_window(&self) -> bool {
                true
            }

            #[unsafe(method(cancelOperation:))]
            fn cancel_operation(&self, _sender: *mut AnyObject) {
                unsafe { fade_out(self) }
            }

            // A click anywhere on the card dismisses it, the way the stock About
            // box does. (Clicks off the panel make it resign key — see the
            // delegate — which also closes it.)
            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, _event: *mut AnyObject) {
                unsafe { fade_out(self) }
            }

            #[unsafe(method(yutaniFadeIn))]
            fn yutani_fade_in(&self) {
                unsafe { glass::animate_alpha(self, 1.0, 0.16) }
            }

            #[unsafe(method(yutaniFadeOut))]
            fn yutani_fade_out(&self) {
                unsafe { fade_out(self) }
            }
        }
    }

    struct AboutControllerIvars {
        /// Set just before opening the project URL so the resign-key that the
        /// browser's activation triggers doesn't also dismiss the panel. Consumed
        /// (cleared) by the next `windowDidResignKey:`.
        suppress_resign: Cell<bool>,
    }

    // Holds the action methods the About menu item and the URL link target, and
    // closes the panel when it loses key focus. Both the menu target and the
    // window delegate are weak references AppKit-side, so this is leaked / held
    // for the process life.
    define_class! {
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = AboutControllerIvars]
        #[name = "YutaniAboutController"]
        struct AboutController;

        unsafe impl NSObjectProtocol for AboutController {}

        impl AboutController {
            // Target/action for the retargeted "About Yutani" menu item.
            #[unsafe(method(yutaniShowAbout:))]
            fn yutani_show_about(&self, _sender: *mut AnyObject) {
                request_about();
            }

            // Target/action for the project-URL link. Opens the URL in the
            // default browser. The click is consumed by the button (NSControl
            // swallows its mouse-down during tracking), so unlike a click
            // anywhere else on the card it does *not* trigger the panel's
            // dismiss-on-click; and we suppress the one resign-key that the
            // browser's activation provokes, so the About window stays put.
            #[unsafe(method(yutaniOpenURL:))]
            fn yutani_open_url(&self, _sender: *mut AnyObject) {
                self.ivars().suppress_resign.set(true);
                unsafe {
                    let s = NSString::from_str(PROJECT_URL);
                    let url: *mut AnyObject = msg_send![class!(NSURL), URLWithString: &*s];
                    if !url.is_null() {
                        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
                        let _: bool = msg_send![ws, openURL: url];
                    }
                }
            }
        }

        unsafe impl NSWindowDelegate for AboutController {
            #[unsafe(method(windowDidResignKey:))]
            fn window_did_resign_key(&self, notif: &NSNotification) {
                // A resign-key that immediately follows opening the link is the
                // browser stealing focus — keep the panel open and consume the
                // one-shot flag.
                if self.ivars().suppress_resign.get() {
                    self.ivars().suppress_resign.set(false);
                    return;
                }
                // Otherwise fade out the panel that just lost key (the
                // notification's object), the same self-close the palette/find
                // bars do.
                unsafe {
                    let panel: *mut AnyObject = msg_send![notif, object];
                    if !panel.is_null() {
                        let _: () = msg_send![panel, performSelector: sel!(yutaniFadeOut)];
                    }
                }
            }
        }
    }

    impl AboutController {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = mtm.alloc::<AboutController>();
            let this = this.set_ivars(AboutControllerIvars {
                suppress_resign: Cell::new(false),
            });
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Build the clickable project-URL link: a borderless `NSButton` whose
    /// attributed title is the URL in link blue + underline, centred, firing
    /// `controller`'s `yutaniOpenURL:` on click.
    unsafe fn link_button(
        frame: NSRect,
        font: &NSFont,
        controller: &AboutController,
    ) -> Retained<NSView> {
        let para: *mut AnyObject = msg_send![class!(NSMutableParagraphStyle), new];
        let _: () = msg_send![para, setAlignment: 1isize]; // NSTextAlignmentCenter
        let link_color: Retained<NSColor> = msg_send![class!(NSColor), linkColor];
        // NSUnderlineStyleSingle == 1.
        let underline: *mut AnyObject = msg_send![class!(NSNumber), numberWithInteger: 1isize];
        let attrs: *mut AnyObject = msg_send![class!(NSMutableDictionary), dictionary];
        // Legacy attribute-name string values (stable): foreground colour, font,
        // underline style, paragraph style.
        let _: () = msg_send![attrs, setObject: &*link_color, forKey: &*NSString::from_str("NSColor")];
        let _: () = msg_send![attrs, setObject: font, forKey: &*NSString::from_str("NSFont")];
        let _: () = msg_send![attrs, setObject: underline, forKey: &*NSString::from_str("NSUnderline")];
        let _: () = msg_send![attrs, setObject: para, forKey: &*NSString::from_str("NSParagraphStyle")];
        let title: *mut AnyObject = msg_send![class!(NSAttributedString), alloc];
        let title: *mut AnyObject =
            msg_send![title, initWithString: &*NSString::from_str(PROJECT_URL), attributes: attrs];

        let button: *mut AnyObject = msg_send![class!(NSButton), alloc];
        let button: *mut AnyObject = msg_send![button, initWithFrame: frame];
        let _: () = msg_send![button, setBordered: false];
        let _: () = msg_send![button, setButtonType: 5isize]; // MomentaryChange — no bezel push
        let _: () = msg_send![button, setFocusRingType: 1isize]; // none
        let _: () = msg_send![button, setAttributedTitle: title];
        let _: () = msg_send![button, setTarget: controller];
        let _: () = msg_send![button, setAction: sel!(yutaniOpenURL:)];
        Retained::from_raw(button.cast::<NSView>()).expect("link button")
    }

    /// Build a centred, non-editable label.
    unsafe fn label(
        mtm: MainThreadMarker,
        frame: NSRect,
        text: &str,
        font: &NSFont,
        color: &NSColor,
    ) -> Retained<NSTextField> {
        let f: Retained<NSTextField> = msg_send![mtm.alloc::<NSTextField>(), initWithFrame: frame];
        let _: () = msg_send![&*f, setBezeled: false];
        let _: () = msg_send![&*f, setBordered: false];
        let _: () = msg_send![&*f, setEditable: false];
        let _: () = msg_send![&*f, setSelectable: false];
        let _: () = msg_send![&*f, setDrawsBackground: false];
        let _: () = msg_send![&*f, setAlignment: 1isize]; // NSTextAlignmentCenter
        let _: () = msg_send![&*f, setFont: font];
        let _: () = msg_send![&*f, setTextColor: color];
        let s = NSString::from_str(text);
        let _: () = msg_send![&*f, setStringValue: &*s];
        f
    }

    pub struct GlassAbout {
        panel: Retained<AboutPanel>,
        #[allow(dead_code)]
        glass: Retained<NSView>,
        /// Held so the window delegate stays alive (delegates are weak).
        #[allow(dead_code)]
        controller: Retained<AboutController>,
    }

    pub fn new() -> Option<GlassAbout> {
        let mtm = MainThreadMarker::new()?;
        unsafe {
            let style: usize = 0; // borderless
            let panel: Retained<AboutPanel> = msg_send![
                mtm.alloc::<AboutPanel>(),
                initWithContentRect: content_rect(),
                styleMask: style,
                backing: 2usize,
                defer: false,
            ];
            glass::configure_panel(&*panel);

            let container: Retained<NSView> =
                msg_send![mtm.alloc::<NSView>(), initWithFrame: card_bounds()];
            let _: () = msg_send![&*container, setWantsLayer: true];

            // App icon, centred near the top.
            let icon_frame = from_top((CARD_WIDTH - ICON) / 2.0, ICON, TOP_PAD, ICON);
            let image_view: *mut AnyObject = msg_send![class!(NSImageView), alloc];
            let image_view: *mut AnyObject = msg_send![image_view, initWithFrame: icon_frame];
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let app_icon: *mut AnyObject = msg_send![app, applicationIconImage];
            let _: () = msg_send![image_view, setImage: app_icon];
            // NSImageScaleProportionallyUpOrDown == 3.
            let _: () = msg_send![image_view, setImageScaling: 3isize];
            let image_view = Retained::from_raw(image_view.cast::<NSView>()).expect("image view");
            container.addSubview(&image_view);

            // Name.
            let name_font: Retained<NSFont> = msg_send![class!(NSFont), boldSystemFontOfSize: 22.0f64];
            let name_color: Retained<NSColor> = msg_send![class!(NSColor), labelColor];
            let name = label(
                mtm,
                from_top(0.0, CARD_WIDTH, TOP_PAD + ICON + 16.0, 28.0),
                "Yutani",
                &name_font,
                &name_color,
            );
            container.addSubview(&name);

            // Version (secondary colour), then the clickable project URL.
            let small_font: Retained<NSFont> = msg_send![class!(NSFont), systemFontOfSize: 12.0f64];
            let secondary: Retained<NSColor> = msg_send![class!(NSColor), secondaryLabelColor];
            let version_text = format!("Version {}", env!("CARGO_PKG_VERSION"));
            let version = label(
                mtm,
                from_top(0.0, CARD_WIDTH, TOP_PAD + ICON + 16.0 + 30.0, 16.0),
                &version_text,
                &small_font,
                &secondary,
            );
            container.addSubview(&version);

            let controller = AboutController::new(mtm);

            let link = link_button(
                from_top(0.0, CARD_WIDTH, TOP_PAD + ICON + 16.0 + 30.0 + 22.0, 16.0),
                &small_font,
                &controller,
            );
            container.addSubview(&link);

            let glass = glass::make_glass(card_rect(), &container, CORNER_RADIUS);
            glass::apply_drop_shadow(&glass);
            glass::set_glass_content_view(&*panel, &glass, content_rect(), mtm);
            panel.setDelegate(Some(ProtocolObject::from_ref(&*controller)));

            Some(GlassAbout {
                panel,
                glass,
                controller,
            })
        }
    }

    impl GlassAbout {
        /// Centre the card on the active screen's visible frame, biased slightly
        /// above centre (where modal panels read best).
        unsafe fn place(&self) {
            let screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
            if screen.is_null() {
                return;
            }
            let vf: NSRect = msg_send![screen, visibleFrame];
            let card_x = vf.origin.x + (vf.size.width - CARD_WIDTH) / 2.0;
            let card_y = vf.origin.y + (vf.size.height - CARD_HEIGHT) / 2.0 + 60.0;
            let frame = NSRect::new(
                NSPoint::new(card_x - SHADOW_MARGIN, card_y - SHADOW_MARGIN),
                window_size(),
            );
            let _: () = msg_send![&*self.panel, setFrame: frame, display: true];
        }

        /// Show (or re-show) the panel, matched to the terminal's light/dark theme.
        pub fn show(&mut self, dark: bool) {
            unsafe {
                glass::set_panel_appearance(&*self.panel, dark);
                self.place();
                let _: () = msg_send![&*self.panel, setAlphaValue: 0.0f64];
                let _: () = msg_send![
                    &*self.panel,
                    makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()
                ];
                let _: () = msg_send![
                    &*self.panel,
                    performSelector: sel!(yutaniFadeIn),
                    withObject: std::ptr::null_mut::<AnyObject>(),
                    afterDelay: 0.0f64,
                ];
            }
        }
    }

    /// Retarget winit's default "About <name>" menu item — which fires the stock
    /// `orderFrontStandardAboutPanel:` — to show our glass panel instead. Builds
    /// and leaks a controller to serve as the item's (weakly-held) target. Call
    /// once, after the default menu exists (i.e. from `resumed`). No-op if the
    /// menu or the About item can't be found.
    pub fn install_about_menu_action() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let main_menu: *mut AnyObject = msg_send![app, mainMenu];
            if main_menu.is_null() {
                return;
            }
            let app_item: *mut AnyObject = msg_send![main_menu, itemAtIndex: 0isize];
            if app_item.is_null() {
                return;
            }
            let app_menu: *mut AnyObject = msg_send![app_item, submenu];
            if app_menu.is_null() {
                return;
            }
            let count: isize = msg_send![app_menu, numberOfItems];
            let about_sel = sel!(orderFrontStandardAboutPanel:);
            for i in 0..count {
                let item: *mut AnyObject = msg_send![app_menu, itemAtIndex: i];
                if item.is_null() {
                    continue;
                }
                let action: Option<Sel> = msg_send![item, action];
                if action == Some(about_sel) {
                    // Leak the controller: the menu lives for the whole process
                    // and holds only a weak target reference.
                    let controller = AboutController::new(mtm);
                    let target: *const AboutController = Retained::into_raw(controller);
                    let _: () = msg_send![item, setTarget: target];
                    let _: () = msg_send![item, setAction: sel!(yutaniShowAbout:)];
                    break;
                }
            }
        }
    }

    // `from_top` is the one pure, deterministic piece worth covering — the rest
    // is main-thread AppKit FFI. Tests live inside `mod imp` so they can see the
    // private helper and the `CARD_HEIGHT` constant; gated to macOS to match the
    // module (mirroring `glass::SignalSlot`'s tests).
    #[cfg(all(test, target_os = "macos"))]
    mod tests {
        use super::*;

        #[test]
        fn from_top_flips_y_through_card_height() {
            // A frame `h` tall, `y_top` below the card's top edge, lands with its
            // origin.y at CARD_HEIGHT - y_top - h (the y-up card space).
            let r = from_top(10.0, 80.0, 30.0, 20.0);
            assert_eq!(r.origin.y, CARD_HEIGHT - 30.0 - 20.0);
        }

        #[test]
        fn from_top_passes_x_width_height_through_unchanged() {
            let r = from_top(12.5, 200.0, 40.0, 18.0);
            assert_eq!(r.origin.x, 12.5);
            assert_eq!(r.size.width, 200.0);
            assert_eq!(r.size.height, 18.0);
        }

        #[test]
        fn from_top_zero_top_inset_anchors_below_the_top_edge() {
            // No inset: a frame of height `h` sits flush under the top edge, so
            // its origin.y is CARD_HEIGHT - h.
            let h = 28.0;
            let r = from_top(0.0, CARD_WIDTH, 0.0, h);
            assert_eq!(r.origin.y, CARD_HEIGHT - h);
        }

        #[test]
        fn from_top_full_height_frame_sits_at_origin() {
            // A frame spanning the whole card height with no inset bottoms out at
            // y = 0.
            let r = from_top(0.0, CARD_WIDTH, 0.0, CARD_HEIGHT);
            assert_eq!(r.origin.y, 0.0);
        }

        #[test]
        fn about_request_round_trips_then_drains() {
            // Process-global slot: keep this to one test to avoid cross-test
            // ordering flakiness. Drain any pre-existing signal first.
            let _ = take_about_request();
            assert!(!take_about_request(), "slot should start empty");
            request_about();
            assert!(take_about_request(), "a posted request is observed once");
            assert!(!take_about_request(), "and is drained after taking");
        }
    }
}

pub use imp::*;
