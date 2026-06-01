//! macOS system light/dark appearance tracking, independent of any window's
//! pinned `NSAppearance`.
//!
//! Yutani pins each window's `NSAppearance` to match its color-scheme
//! background (so the OS-drawn title-bar text stays legible — see
//! `set_native_window_bg` / the `window.set_theme` call at startup). That
//! pinning, however, makes winit's own `ThemeChanged` detection useless for
//! *following the system*: winit reads the **window's** appearance, and its
//! `effectiveAppearance` KVO observer bails the moment the appearance is
//! customized (`window().appearance().is_some()`), so it never reports a
//! system light↔dark flip. `Window::theme()` likewise returns the pinned
//! value, not the OS setting.
//!
//! So to honour `auto_theme` we watch `NSApplication.effectiveAppearance`
//! ourselves — that value is never pinned (we only ever set *per-window*
//! appearances), so it tracks the OS live. [`system_is_dark`] reads it, and
//! [`install_observer`] registers a KVO observer that posts to a single-slot
//! mailbox the event loop drains (like the palette / find signals), re-applying
//! the active scheme on every `auto_theme` window when the system flips.
//!
//! Off macOS this is a stub: [`system_is_dark`] returns `None` so the caller
//! falls back to winit's `Window::theme()`, and the observer is a no-op.

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol};
    use objc2::{class, define_class, msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSObject, NSString};

    use crate::glass::SignalSlot;

    /// Pending "the system appearance changed" flag from the KVO callback (main
    /// thread) to the event loop (also main thread, draining in `about_to_wait`).
    static APPEARANCE_CHANGED: SignalSlot<()> = SignalSlot::new();
    /// Guards one-time observer registration.
    static OBSERVED: AtomicBool = AtomicBool::new(false);

    // The string *values* of the appearance-name constants. Using the literals
    // (rather than linking the `NSAppearanceName*` externs) keeps this msg_send
    // path dependency-light; they're stable API identifiers.
    const AQUA: &str = "NSAppearanceNameAqua";
    const DARK_AQUA: &str = "NSAppearanceNameDarkAqua";

    // ---- KVO observer: NSApp.effectiveAppearance -> mailbox ----

    define_class! {
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniAppearanceObserver"]
        struct AppearanceObserver;

        unsafe impl NSObjectProtocol for AppearanceObserver {}

        impl AppearanceObserver {
            #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
            fn observe_value(
                &self,
                _key_path: *mut AnyObject,
                _object: *mut AnyObject,
                _change: *mut AnyObject,
                _context: *mut core::ffi::c_void,
            ) {
                APPEARANCE_CHANGED.post(());
            }
        }
    }

    /// Best-match `appearance` against aqua / dark-aqua and report whether dark
    /// wins. Mirrors winit's own `appearance_to_theme`, so our reading of the
    /// system can't drift from what winit would have reported. `appearance` is
    /// an `NSAppearance*`; null (or no match) reads as light.
    unsafe fn appearance_is_dark(appearance: *mut AnyObject) -> bool {
        if appearance.is_null() {
            return false;
        }
        let names: *mut AnyObject = msg_send![class!(NSMutableArray), array];
        for name in [AQUA, DARK_AQUA] {
            let s = NSString::from_str(name);
            let _: () = msg_send![names, addObject: &*s];
        }
        let best: *mut AnyObject = msg_send![appearance, bestMatchFromAppearancesWithNames: names];
        if best.is_null() {
            return false;
        }
        // `best` is an NSAppearanceName (an NSString); compare to the dark name.
        let best: Retained<NSString> = Retained::retain(best.cast()).expect("appearance name");
        best.to_string() == DARK_AQUA
    }

    /// True when the OS is currently in dark mode, from
    /// `NSApplication.effectiveAppearance` (the system appearance, unaffected by
    /// any per-window pinning we do). `None` only if the main-thread marker is
    /// unavailable — callers then fall back to winit's per-window theme.
    pub fn system_is_dark() -> Option<bool> {
        let _mtm = MainThreadMarker::new()?;
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let appearance: *mut AnyObject = msg_send![app, effectiveAppearance];
            Some(appearance_is_dark(appearance))
        }
    }

    /// Register a KVO observer on `NSApp.effectiveAppearance` (once). The
    /// observer is intentionally leaked: it lives for the whole process, and
    /// `NSApplication` is itself a singleton that outlives everything, so there
    /// is nothing to balance the `addObserver:` against.
    pub fn install_observer(mtm: MainThreadMarker) {
        if OBSERVED.swap(true, Ordering::SeqCst) {
            return;
        }
        let observer: Retained<AppearanceObserver> =
            unsafe { msg_send![mtm.alloc::<AppearanceObserver>(), init] };
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let key = NSString::from_str("effectiveAppearance");
            let _: () = msg_send![
                app,
                addObserver: &*observer,
                forKeyPath: &*key,
                options: 0usize,
                context: core::ptr::null_mut::<core::ffi::c_void>(),
            ];
        }
        // Keep the observer alive for the app's lifetime.
        std::mem::forget(observer);
    }

    /// Drain a pending system-appearance change, if any. Called by the event
    /// loop; `true` means re-resolve `auto_theme` windows' schemes.
    pub fn take_change() -> bool {
        APPEARANCE_CHANGED.take().is_some()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// Off macOS we have no independent system-appearance source here; callers
    /// fall back to winit's `Window::theme()`.
    pub fn system_is_dark() -> Option<bool> {
        None
    }
    pub fn take_change() -> bool {
        false
    }
}

pub use imp::*;
