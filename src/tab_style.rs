//! Restyle the native macOS window tabs (a "pure visual restyle" that respects
//! native `NSWindow` tabbing).
//!
//! AppKit draws the tab bar itself and gives no API to hide it while ≥2 tabs
//! exist or to restyle the pill chrome (`NSWindowTabGroup.isTabBarVisible` is
//! read-only; `toggleTabBar:` is a no-op with multiple tabs). What it *does*
//! expose, per tab, is `NSWindowTab.attributedTitle` — so we style each tab's
//! *text*: the selected tab gets emphasized (full-strength label color,
//! semibold), the others dim into the bar (secondary label color). We also drop
//! the title-bar separator for a cleaner, more floating look.
//!
//! Each `NSWindow` owns exactly one native tab, but a window can reach every
//! sibling in its group through `tabGroup.windows` and set each one's
//! `attributedTitle`. Restyle walks the whole group: any one window's call
//! repaints every pill in the bar. This matters when the active scheme changes
//! — the global palette drives the title color, and a theme flip in the
//! focused window has to refresh the *inactive* siblings' titles too (they
//! don't get a focus event and would otherwise hold the old foreground until
//! clicked). Stateless: just functions over the window.
//!
//! Off macOS these are no-ops.

#[cfg(not(target_os = "macos"))]
mod imp {
    use winit::window::Window;

    pub fn configure_window(_window: &Window) {}
    pub fn restyle(_window: &Window) {}
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_app_kit::{NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName};
    use objc2_foundation::{NSAttributedString, NSRange, NSString};
    use winit::window::Window;

    use crate::glass::parent_nswindow;
    use crate::palette;

    /// `NSTitlebarSeparatorStyleNone` — no hairline under the title bar / tabs.
    const SEPARATOR_NONE: isize = 1;

    /// One-time per-window chrome setup: drop the title-bar separator.
    pub fn configure_window(window: &Window) {
        let Some(ns) = parent_nswindow(window) else {
            return;
        };
        unsafe {
            let _: () = msg_send![ns, setTitlebarSeparatorStyle: SEPARATOR_NONE];
        }
    }

    /// Restyle the native tab titles for every window in this window's tab
    /// group: emphasized for the selected one, dimmed for the rest. Safe to
    /// call on every selection / title change / theme change. When called by a
    /// single window after a theme flip (the focused one), sibling tabs get
    /// refreshed too — they share the process-global palette and would
    /// otherwise hold the old foreground color until clicked.
    pub fn restyle(window: &Window) {
        let Some(ns) = parent_nswindow(window) else {
            return;
        };
        unsafe {
            let tab_group: *mut AnyObject = msg_send![ns, tabGroup];
            if tab_group.is_null() {
                // Lone window with no tab group — style its own tab as active.
                restyle_one(ns, true);
                return;
            }
            let selected: *mut AnyObject = msg_send![tab_group, selectedWindow];
            let windows: *mut AnyObject = msg_send![tab_group, windows];
            if windows.is_null() {
                let active = selected.is_null() || selected == ns;
                restyle_one(ns, active);
                return;
            }
            let count: usize = msg_send![windows, count];
            for i in 0..count {
                let w: *mut AnyObject = msg_send![windows, objectAtIndex: i];
                if w.is_null() {
                    continue;
                }
                // `selectedWindow` is nil only briefly during transitions; treat
                // that as "every tab active" so nobody renders as dimmed in the
                // gap (matches the lone-window case above).
                let active = selected.is_null() || w == selected;
                restyle_one(w, active);
            }
        }
    }

    /// Set one window's tab title to the styled attributed string. Caller
    /// decides active vs inactive based on the tab group's selection.
    unsafe fn restyle_one(ns: *mut AnyObject, active: bool) {
        // Native tabs size to width and ellipsize the title themselves, so
        // pass the full title through.
        let title = window_title(ns);
        let attr = attributed_title(&title, active);
        let tab: *mut AnyObject = msg_send![ns, tab];
        if !tab.is_null() {
            let _: () = msg_send![tab, setAttributedTitle: &*attr];
        }
    }

    /// Read an `NSWindow`'s plain title as a Rust string.
    unsafe fn window_title(ns: *mut AnyObject) -> String {
        let title: *mut AnyObject = msg_send![ns, title];
        if title.is_null() {
            return String::new();
        }
        let title: &NSString = &*title.cast();
        title.to_string()
    }

    /// Title font size: a couple points below the system default so the tab
    /// titles read a touch lighter than standard chrome.
    const FONT_SIZE_DELTA: f64 = -2.0;
    /// Text alpha — the only "transparency" lever AppKit exposes for a native
    /// tab (the pill glass itself is system-drawn). The active title stays more
    /// solid; the inactive one recedes further.
    const ACTIVE_ALPHA: f64 = 0.80;
    const INACTIVE_ALPHA: f64 = 0.55;

    /// Build the styled tab title: emphasized (label color, semibold) when
    /// active, dimmed (secondary label color, regular) otherwise. Both use a
    /// slightly-smaller system font and a translucent color.
    unsafe fn attributed_title(title: &str, active: bool) -> Retained<NSAttributedString> {
        let s = NSString::from_str(title);
        let size: f64 = msg_send![class!(NSFont), systemFontSize];
        let size = (size + FONT_SIZE_DELTA).max(9.0);
        // Color the text from the active scheme's foreground (not the dynamic
        // system label color, which resolves against the wrong appearance and
        // can come out dark on dark themes). The frosted band shows the scheme
        // background, so the scheme foreground always reads against it.
        let fg = palette::get().foreground;
        let chan = |c: f32| palette::linear_to_srgb_u8(c) as f64 / 255.0;
        let alpha = if active { ACTIVE_ALPHA } else { INACTIVE_ALPHA };
        let color: Retained<NSColor> = msg_send![
            class!(NSColor),
            colorWithSRGBRed: chan(fg[0]),
            green: chan(fg[1]),
            blue: chan(fg[2]),
            alpha: alpha,
        ];
        let font: Retained<NSFont> = if active {
            // Semibold (weight 0.3) reads as emphasis without looking heavy.
            msg_send![class!(NSFont), systemFontOfSize: size, weight: 0.3f64]
        } else {
            msg_send![class!(NSFont), systemFontOfSize: size]
        };

        let astr: *mut AnyObject = msg_send![class!(NSMutableAttributedString), alloc];
        let astr: *mut AnyObject = msg_send![astr, initWithString: &*s];
        let len: usize = msg_send![astr, length];
        let range = NSRange { location: 0, length: len };
        let _: () = msg_send![astr, addAttribute: NSForegroundColorAttributeName, value: &*color, range: range];
        let _: () = msg_send![astr, addAttribute: NSFontAttributeName, value: &*font, range: range];
        Retained::from_raw(astr.cast()).expect("attributed title")
    }
}

pub use imp::*;
