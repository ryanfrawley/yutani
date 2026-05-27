//! Shared macOS "Liquid Glass" UI plumbing for the native command palette
//! (`glass_palette`) and find bar (`glass_find`).
//!
//! Both features present the same kind of surface: a borderless `NSPanel`
//! layered over the terminal as a child window, its content embedded in an
//! `NSGlassEffectView` (macOS 26+, with an `NSVisualEffectView` fallback),
//! a deep rounded drop shadow rendered into a transparent window gutter, a
//! fade in/out, theme-matched appearance, and a single-slot mailbox handing
//! user actions to the event loop. Everything here is layout-agnostic; each
//! feature keeps its own geometry, views, and controller.
//!
//! Off macOS this module is empty — the features compile to no-op stubs that
//! never reach this code.

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::Mutex;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{class, msg_send, sel, MainThreadMarker, Message};
    use objc2_app_kit::{NSColor, NSShadow, NSView};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    use winit::window::Window;

    // The corner radius is per-feature, but the shadow and panel chrome are
    // identical everywhere, so they live here as constants.
    const SHADOW_ALPHA: f64 = 0.8;
    const SHADOW_OFFSET_Y: f64 = -10.0;
    const SHADOW_BLUR: f64 = 20.0;

    /// Single-slot mailbox from an AppKit controller (main thread) to the event
    /// loop (also main thread, draining in `about_to_wait`); the `Mutex` is just
    /// for the `'static` safety a shared static demands. One per feature, holding
    /// that feature's signal enum.
    pub struct SignalSlot<T>(Mutex<Option<T>>);

    impl<T> SignalSlot<T> {
        pub const fn new() -> Self {
            SignalSlot(Mutex::new(None))
        }

        /// Replace the pending signal (latest wins; we only ever act on one).
        pub fn post(&self, sig: T) {
            if let Ok(mut slot) = self.0.lock() {
                *slot = Some(sig);
            }
        }

        /// Drain the pending signal, if any.
        pub fn take(&self) -> Option<T> {
            self.0.lock().ok().and_then(|mut slot| slot.take())
        }
    }

    /// Create the glass backing view embedding `content`, rounded to
    /// `corner_radius`. Prefers the real Liquid Glass material (`NSGlassEffectView`,
    /// macOS 26+); falls back to a rounded `NSVisualEffectView` vibrancy view.
    ///
    /// On the glass path the layer is *not* clipped to bounds — `masksToBounds`
    /// would clip the glass's own drop shadow; `setCornerRadius:` already rounds
    /// the material. The fallback has no shadow of its own, so it clips.
    pub unsafe fn make_glass(frame: NSRect, content: &NSView, corner_radius: f64) -> Retained<NSView> {
        if let Some(cls) = AnyClass::get(c"NSGlassEffectView") {
            let v: *mut AnyObject = msg_send![cls, alloc];
            let v: *mut AnyObject = msg_send![v, initWithFrame: frame];
            let _: () = msg_send![v, setCornerRadius: corner_radius];
            let _: () = msg_send![v, setContentView: content];
            return Retained::from_raw(v as *mut NSView).expect("glass view");
        }
        let cls = class!(NSVisualEffectView);
        let v: *mut AnyObject = msg_send![cls, alloc];
        let v: *mut AnyObject = msg_send![v, initWithFrame: frame];
        let _: () = msg_send![v, setMaterial: 18isize]; // HUDWindow
        let _: () = msg_send![v, setBlendingMode: 0isize]; // BehindWindow
        let _: () = msg_send![v, setState: 1isize]; // Active
        let _: () = msg_send![v, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![v, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setCornerRadius: corner_radius];
            let _: () = msg_send![layer, setMasksToBounds: true];
        }
        let glass = Retained::from_raw(v as *mut NSView).expect("visual effect view");
        glass.addSubview(content);
        glass
    }

    /// Give the glass its deep, rounded drop shadow. Nothing opaque sits behind
    /// the glass, so it still refracts the terminal; the shadow renders into the
    /// window's transparent gutter (a window can't paint outside its frame, hence
    /// the gutter). Strength is the shadow color's alpha.
    pub unsafe fn apply_drop_shadow(glass: &NSView) {
        let shadow_color: Retained<NSColor> = msg_send![
            class!(NSColor),
            colorWithSRGBRed: 0.0f64, green: 0.0f64, blue: 0.0f64, alpha: SHADOW_ALPHA,
        ];
        let ns_shadow: Retained<NSShadow> = msg_send![class!(NSShadow), new];
        let _: () = msg_send![&*ns_shadow, setShadowColor: &*shadow_color];
        let _: () = msg_send![&*ns_shadow, setShadowOffset: NSSize::new(0.0, SHADOW_OFFSET_Y)];
        let _: () = msg_send![&*ns_shadow, setShadowBlurRadius: SHADOW_BLUR];
        let _: () = msg_send![glass, setShadow: Some(&*ns_shadow)];
    }

    /// Apply the borderless-panel chrome both features share: never released on
    /// close, transparent (clear background, not opaque), no system shadow (we
    /// draw our own), floating level, and key-on-demand so a borderless panel can
    /// still take key status for text input. The panel must already be allocated
    /// with a borderless style mask.
    pub unsafe fn configure_panel<P: Message>(panel: &P) {
        let _: () = msg_send![panel, setReleasedWhenClosed: false];
        let _: () = msg_send![panel, setOpaque: false];
        let _: () = msg_send![panel, setBecomesKeyOnlyIfNeeded: false];
        let _: () = msg_send![panel, setHasShadow: false];
        let clear = NSColor::clearColor();
        let _: () = msg_send![panel, setBackgroundColor: &*clear];
        let _: () = msg_send![panel, setLevel: 3isize]; // NSFloatingWindowLevel
    }

    /// Wrap `glass` in a layer-backed view filling the whole window (the card is
    /// inset from it by the shadow gutter) and install it as `panel`'s content
    /// view, so the glass's shadow has somewhere to composite.
    pub unsafe fn set_glass_content_view<P: Message>(
        panel: &P,
        glass: &NSView,
        window_rect: NSRect,
        mtm: MainThreadMarker,
    ) {
        let wrapper: Retained<NSView> = msg_send![mtm.alloc::<NSView>(), initWithFrame: window_rect];
        let _: () = msg_send![&*wrapper, setWantsLayer: true];
        wrapper.addSubview(glass);
        let _: () = msg_send![panel, setContentView: &*wrapper];
    }

    /// Animate a window's alpha to `to` over `duration` seconds via the animator
    /// proxy + an `NSAnimationContext` group. The window must already be on
    /// screen. Deferring this to the next run-loop tick (e.g. via
    /// `performSelector:withObject:afterDelay:`) is required: an animation set up
    /// synchronously inside winit's event-loop callbacks commits without
    /// animating.
    pub unsafe fn animate_alpha<P: Message>(panel: &P, to: f64, duration: f64) {
        let _: () = msg_send![class!(NSAnimationContext), beginGrouping];
        let ctx: *mut AnyObject = msg_send![class!(NSAnimationContext), currentContext];
        let _: () = msg_send![ctx, setDuration: duration];
        let anim: *mut AnyObject = msg_send![panel, animator];
        let _: () = msg_send![anim, setAlphaValue: to];
        let _: () = msg_send![class!(NSAnimationContext), endGrouping];
    }

    /// Match a panel's appearance (and thus its glass + label colors) to the
    /// terminal's light/dark theme, so text stays legible on the glass.
    pub unsafe fn set_panel_appearance<P: Message>(panel: &P, dark: bool) {
        let name = NSString::from_str(if dark {
            "NSAppearanceNameDarkAqua"
        } else {
            "NSAppearanceNameAqua"
        });
        let appearance: *mut AnyObject = msg_send![class!(NSAppearance), appearanceNamed: &*name];
        let _: () = msg_send![panel, setAppearance: appearance];
    }

    /// Centre a card of `card_size` near the top of `parent_ns`'s frame,
    /// `top_inset` points down from the top edge, and set `panel`'s frame to the
    /// enclosing `window_size` (bigger than the card by `shadow_margin` on every
    /// side — the transparent shadow gutter). Screen coords are y-up.
    pub unsafe fn place_card<P: Message>(
        panel: &P,
        parent_ns: *mut AnyObject,
        card_size: NSSize,
        window_size: NSSize,
        top_inset: f64,
        shadow_margin: f64,
    ) {
        let pf: NSRect = msg_send![parent_ns, frame];
        let card_x = pf.origin.x + (pf.size.width - card_size.width) / 2.0;
        let card_y = pf.origin.y + pf.size.height - card_size.height - top_inset;
        let frame = NSRect::new(
            NSPoint::new(card_x - shadow_margin, card_y - shadow_margin),
            window_size,
        );
        let _: () = msg_send![panel, setFrame: frame, display: true];
    }

    /// Detach `panel` from its parent window (it stays visible during the fade)
    /// and kick off its fade-out on the next run-loop tick. The panel must define
    /// a `yutaniFadeOut` method (both feature panels do) that fades then orders
    /// out.
    pub unsafe fn detach_and_fade_out<P: Message>(panel: &P) {
        let parent: *mut AnyObject = msg_send![panel, parentWindow];
        if !parent.is_null() {
            let _: () = msg_send![parent, removeChildWindow: panel];
        }
        let _: () = msg_send![
            panel,
            performSelector: sel!(yutaniFadeOut),
            withObject: std::ptr::null_mut::<AnyObject>(),
            afterDelay: 0.0f64,
        ];
    }

    /// Read the parent terminal window's `NSWindow*` via its raw handle.
    pub fn parent_nswindow(window: &Window) -> Option<*mut AnyObject> {
        let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
            return None;
        };
        unsafe {
            let ns_view = handle.ns_view as *mut AnyObject;
            let ns_window: *mut AnyObject = msg_send![ns_view, window];
            (!ns_window.is_null()).then_some(ns_window)
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::*;

// `SignalSlot<T>` is the one pure, host-testable piece here; everything else is
// main-thread AppKit FFI. The type is macOS-only, so gate the tests to match.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::SignalSlot;

    #[test]
    fn take_on_empty_slot_returns_none() {
        let slot: SignalSlot<i32> = SignalSlot::new();
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn post_then_take_returns_the_posted_value() {
        let slot = SignalSlot::new();
        slot.post(42);
        assert_eq!(slot.take(), Some(42));
    }

    #[test]
    fn take_clears_the_slot() {
        let slot = SignalSlot::new();
        slot.post("hello");
        assert_eq!(slot.take(), Some("hello"));
        // The value was drained, so a second take sees an empty slot.
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn post_overwrites_a_pending_unconsumed_value() {
        let slot = SignalSlot::new();
        slot.post(1);
        slot.post(2);
        slot.post(3);
        // Latest wins: only the most recent unconsumed signal survives.
        assert_eq!(slot.take(), Some(3));
        assert_eq!(slot.take(), None);
    }
}
