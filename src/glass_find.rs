//! Native macOS "Liquid Glass" find-in-scrollback bar.
//!
//! The native counterpart to the GPU-drawn Cmd-F box: a compact borderless
//! `NSPanel` over the terminal with an `NSGlassEffectView`, a search field, and
//! a result counter ("3 / 17" / "No results"). The pure `search` model and the
//! in-terminal match highlighting stay as they are; this is just the native
//! input + counter. Mirrors the structure of `glass_palette`.
//!
//! On non-macOS targets this is a no-op stub so `WindowState` stays uniform.

#[cfg(not(target_os = "macos"))]
mod imp {
    use winit::window::Window;

    pub struct GlassFind;

    impl GlassFind {
        pub fn show(&mut self, _parent: &Window) {}
        pub fn hide(&mut self) {}
        pub fn visible(&self) -> bool {
            false
        }
        pub fn set_counter(&self, _text: &str) {}
        pub fn set_appearance(&self, _dark: bool) {}
    }

    pub fn new() -> Option<GlassFind> {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::RefCell;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject, Sel};
    use objc2::{
        class, define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly,
    };
    use objc2_app_kit::{
        NSColor, NSControl, NSControlTextEditingDelegate, NSFont, NSPanel, NSTextField,
        NSTextFieldDelegate, NSView, NSWindowDelegate,
    };
    use objc2_foundation::{NSNotification, NSObject, NSPoint, NSRect, NSSize, NSString};
    use winit::window::Window;

    use crate::glass::{self, SignalSlot};

    /// What the user did in the find bar, drained by the event loop.
    pub enum FindSignal {
        /// Query edited: re-run the search.
        Query(String),
        /// Step to the next / previous match.
        Next,
        Prev,
        /// Close the find bar.
        Close,
    }

    static FIND_SIGNAL: SignalSlot<FindSignal> = SignalSlot::new();

    fn post(sig: FindSignal) {
        FIND_SIGNAL.post(sig);
    }

    pub fn take_find_signal() -> Option<FindSignal> {
        FIND_SIGNAL.take()
    }

    // Geometry (points). A single compact row: field on the left, counter on
    // the right. The window has a transparent gutter for the drop shadow.
    const CARD_WIDTH: f64 = 460.0;
    const PAD: f64 = 12.0;
    const FIELD_INSET_X: f64 = 16.0;
    const FIELD_HEIGHT: f64 = 28.0;
    const COUNTER_WIDTH: f64 = 96.0;
    const GAP: f64 = 8.0;
    const TOP_INSET: f64 = 96.0;
    const SHADOW_MARGIN: f64 = 50.0;
    const CORNER_RADIUS: f64 = 18.0;

    fn card_height() -> f64 {
        PAD + FIELD_HEIGHT + 4.0
    }
    fn window_size() -> NSSize {
        NSSize::new(CARD_WIDTH + SHADOW_MARGIN * 2.0, card_height() + SHADOW_MARGIN * 2.0)
    }
    fn content_rect() -> NSRect {
        NSRect::new(NSPoint::new(0.0, 0.0), window_size())
    }
    fn card_rect() -> NSRect {
        NSRect::new(
            NSPoint::new(SHADOW_MARGIN, SHADOW_MARGIN),
            NSSize::new(CARD_WIDTH, card_height()),
        )
    }
    fn card_bounds() -> NSRect {
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(CARD_WIDTH, card_height()))
    }
    fn field_frame() -> NSRect {
        NSRect::new(
            NSPoint::new(FIELD_INSET_X, card_height() - PAD - FIELD_HEIGHT),
            NSSize::new(
                CARD_WIDTH - FIELD_INSET_X - COUNTER_WIDTH - GAP - PAD,
                FIELD_HEIGHT,
            ),
        )
    }
    fn counter_frame() -> NSRect {
        NSRect::new(
            NSPoint::new(CARD_WIDTH - COUNTER_WIDTH - PAD, card_height() - PAD - FIELD_HEIGHT),
            NSSize::new(COUNTER_WIDTH, FIELD_HEIGHT),
        )
    }

    define_class! {
        #[unsafe(super(NSPanel))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniFindPanel"]
        struct FindPanel;

        impl FindPanel {
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key_window(&self) -> bool {
                true
            }

            #[unsafe(method(cancelOperation:))]
            fn cancel_operation(&self, _sender: *mut AnyObject) {
                post(FindSignal::Close);
            }

            #[unsafe(method(yutaniFadeIn))]
            fn yutani_fade_in(&self) {
                unsafe { glass::animate_alpha(self, 1.0, 0.16) }
            }

            #[unsafe(method(yutaniFadeOut))]
            fn yutani_fade_out(&self) {
                unsafe {
                    glass::animate_alpha(self, 0.0, 0.14);
                    let _: () = msg_send![
                        self,
                        performSelector: sel!(orderOut:),
                        withObject: std::ptr::null_mut::<AnyObject>(),
                        afterDelay: 0.15f64,
                    ];
                }
            }
        }
    }

    struct ControllerIvars {
        field: RefCell<Option<Retained<NSTextField>>>,
    }

    define_class! {
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = ControllerIvars]
        #[name = "YutaniFindController"]
        struct FindController;

        unsafe impl NSObjectProtocol for FindController {}

        unsafe impl NSControlTextEditingDelegate for FindController {
            #[unsafe(method(controlTextDidChange:))]
            fn control_text_did_change(&self, _notif: &NSNotification) {
                if let Some(f) = self.ivars().field.borrow().as_ref() {
                    post(FindSignal::Query(f.stringValue().to_string()));
                }
            }

            #[unsafe(method(control:textView:doCommandBySelector:))]
            fn do_command(&self, _c: &NSControl, _tv: &AnyObject, command: Sel) -> bool {
                if command == sel!(insertNewline:) {
                    // Enter = next; Shift-Enter = previous.
                    post(if unsafe { shift_held() } {
                        FindSignal::Prev
                    } else {
                        FindSignal::Next
                    });
                    true
                } else if command == sel!(moveDown:) {
                    post(FindSignal::Next);
                    true
                } else if command == sel!(moveUp:) {
                    post(FindSignal::Prev);
                    true
                } else if command == sel!(cancelOperation:) {
                    post(FindSignal::Close);
                    true
                } else {
                    false
                }
            }
        }

        unsafe impl NSTextFieldDelegate for FindController {}

        unsafe impl NSWindowDelegate for FindController {
            #[unsafe(method(windowDidResignKey:))]
            fn window_did_resign_key(&self, _notif: &NSNotification) {
                post(FindSignal::Close);
            }
        }
    }

    impl FindController {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = mtm.alloc::<FindController>();
            let this = this.set_ivars(ControllerIvars {
                field: RefCell::new(None),
            });
            unsafe { msg_send![super(this), init] }
        }
    }

    /// True if the Shift modifier is currently held (read from the live event).
    unsafe fn shift_held() -> bool {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let event: *mut AnyObject = msg_send![app, currentEvent];
        if event.is_null() {
            return false;
        }
        let flags: usize = msg_send![event, modifierFlags];
        flags & 0x2_0000 != 0 // NSEventModifierFlagShift
    }

    pub struct GlassFind {
        panel: Retained<FindPanel>,
        #[allow(dead_code)]
        glass: Retained<NSView>,
        field: Retained<NSTextField>,
        counter: Retained<NSTextField>,
        /// Held so the field/window delegate stays alive (delegates are weak).
        #[allow(dead_code)]
        controller: Retained<FindController>,
        visible: bool,
    }

    pub fn new() -> Option<GlassFind> {
        let mtm = MainThreadMarker::new()?;
        unsafe {
            let content = content_rect();
            let style: usize = 0; // borderless
            let alloc = mtm.alloc::<FindPanel>();
            let panel: Retained<FindPanel> = msg_send![
                alloc,
                initWithContentRect: content,
                styleMask: style,
                backing: 2usize,
                defer: false,
            ];
            glass::configure_panel(&*panel);

            let container: Retained<NSView> =
                msg_send![mtm.alloc::<NSView>(), initWithFrame: card_bounds()];
            let _: () = msg_send![&*container, setWantsLayer: true];

            let controller = FindController::new(mtm);

            // Query field.
            let field: Retained<NSTextField> =
                msg_send![mtm.alloc::<NSTextField>(), initWithFrame: field_frame()];
            let _: () = msg_send![&*field, setBezeled: false];
            let _: () = msg_send![&*field, setBordered: false];
            let _: () = msg_send![&*field, setDrawsBackground: false];
            let _: () = msg_send![&*field, setFocusRingType: 1isize];
            let placeholder = NSString::from_str("Find");
            let _: () = msg_send![&*field, setPlaceholderString: &*placeholder];
            let font: Retained<NSFont> = msg_send![class!(NSFont), systemFontOfSize: 16.0f64];
            let _: () = msg_send![&*field, setFont: &*font];
            field.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            container.addSubview(&field);

            // Result counter (right-aligned, secondary color).
            let counter: Retained<NSTextField> =
                msg_send![mtm.alloc::<NSTextField>(), initWithFrame: counter_frame()];
            let _: () = msg_send![&*counter, setBezeled: false];
            let _: () = msg_send![&*counter, setBordered: false];
            let _: () = msg_send![&*counter, setEditable: false];
            let _: () = msg_send![&*counter, setSelectable: false];
            let _: () = msg_send![&*counter, setDrawsBackground: false];
            // NSTextAlignmentRight == 2.
            let _: () = msg_send![&*counter, setAlignment: 2isize];
            let cfont: Retained<NSFont> = msg_send![class!(NSFont), systemFontOfSize: 13.0f64];
            let _: () = msg_send![&*counter, setFont: &*cfont];
            let secondary: Retained<NSColor> = msg_send![class!(NSColor), secondaryLabelColor];
            let _: () = msg_send![&*counter, setTextColor: &*secondary];
            let empty = NSString::from_str("");
            let _: () = msg_send![&*counter, setStringValue: &*empty];
            container.addSubview(&counter);

            *controller.ivars().field.borrow_mut() = Some(field.clone());

            let glass = glass::make_glass(card_rect(), &container, CORNER_RADIUS);
            // The glass casts its own shadow (nothing opaque behind it).
            glass::apply_drop_shadow(&glass);

            glass::set_glass_content_view(&*panel, &glass, content, mtm);
            let _: () = msg_send![&*panel, setInitialFirstResponder: &*field];
            panel.setDelegate(Some(ProtocolObject::from_ref(&*controller)));

            Some(GlassFind {
                panel,
                glass,
                field,
                counter,
                controller,
                visible: false,
            })
        }
    }

    impl GlassFind {
        pub fn visible(&self) -> bool {
            self.visible
        }

        /// Set the result-counter text (e.g. "3 / 17", "No results", "").
        pub fn set_counter(&self, text: &str) {
            unsafe {
                let s = NSString::from_str(text);
                let _: () = msg_send![&*self.counter, setStringValue: &*s];
            }
        }

        pub fn set_appearance(&self, dark: bool) {
            unsafe { glass::set_panel_appearance(&*self.panel, dark) }
        }

        fn place(&self, parent_ns: *mut AnyObject) {
            let card = NSSize::new(CARD_WIDTH, card_height());
            unsafe {
                glass::place_card(&*self.panel, parent_ns, card, window_size(), TOP_INSET, SHADOW_MARGIN);
            }
        }

        pub fn show(&mut self, parent: &Window) {
            let Some(parent_ns) = glass::parent_nswindow(parent) else {
                return;
            };
            unsafe {
                let empty = NSString::from_str("");
                let _: () = msg_send![&*self.field, setStringValue: &*empty];
            }
            self.set_counter("");
            self.place(parent_ns);
            unsafe {
                let _: () = msg_send![&*self.panel, setAlphaValue: 0.0f64];
                let _: () = msg_send![parent_ns, addChildWindow: &*self.panel, ordered: 1isize];
                let _: () = msg_send![
                    &*self.panel,
                    makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()
                ];
                let _: bool = msg_send![&*self.panel, makeFirstResponder: &*self.field];
                let _: () = msg_send![
                    &*self.panel,
                    performSelector: sel!(yutaniFadeIn),
                    withObject: std::ptr::null_mut::<AnyObject>(),
                    afterDelay: 0.0f64,
                ];
            }
            self.visible = true;
        }

        pub fn hide(&mut self) {
            if !self.visible {
                return;
            }
            unsafe { glass::detach_and_fade_out(&*self.panel) }
            self.visible = false;
        }
    }
}

pub use imp::*;
