//! Native macOS "Liquid Glass" autocomplete popup.
//!
//! Unlike the command palette and find bar, this popup must **never take
//! keyboard focus** — you're typing into the shell and it only suggests. So it
//! is a non-activating `NSPanel` that is never made key (the terminal keeps key
//! focus); navigation/accept stay in the existing winit key handling, and this
//! is a passive display driven by the completion model: it shows the visible
//! suggestion rows below the cursor with the selected one highlighted.
//!
//! On non-macOS targets this is a no-op stub so `WindowState` stays uniform.

#[cfg(not(target_os = "macos"))]
mod imp {
    use winit::window::Window;

    pub struct GlassComplete;

    impl GlassComplete {
        pub fn update(
            &mut self,
            _parent: &Window,
            _x: f64,
            _row_top: f64,
            _row_h: f64,
            _rows: &[String],
            _selected: usize,
            _dark: bool,
        ) {
        }
        pub fn hide(&mut self) {}
    }

    pub fn new() -> Option<GlassComplete> {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSColor, NSFont, NSPanel, NSShadow, NSTextField, NSView};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use winit::window::Window;

    use crate::glass;

    const MAX_ROWS: usize = 10;
    const ROW_H: f64 = 22.0;
    const FONT_SIZE: f64 = 13.0;
    const PAD_V: f64 = 4.0;
    const PAD_H: f64 = 10.0;
    const ROW_INSET: f64 = 6.0; // highlight/label inset within the card
    const CORNER_RADIUS: f64 = 10.0;
    const SHADOW_MARGIN: f64 = 30.0;
    const MIN_W: f64 = 160.0;
    const MAX_W: f64 = 560.0;
    /// Gap above the cursor row when the popup flips above it (card bottom to
    /// cursor-row top). Negative because the card's internal bottom padding
    /// already supplies the visual gap; this tucks the glass edge into the empty
    /// top of the cursor cell so the list sits snug above the prompt.
    const ABOVE_GAP: f64 = -8.0;
    /// Gap below the cursor row when the popup sits below it (card top to
    /// cursor-row bottom).
    const BELOW_GAP: f64 = 11.0;

    define_class! {
        // Non-activating so showing it never deactivates the app; the
        // canBecomeKeyWindow override returning false keeps the terminal key.
        #[unsafe(super(NSPanel))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniCompletePanel"]
        struct CompletePanel;

        impl CompletePanel {
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key_window(&self) -> bool {
                false
            }
        }
    }

    pub struct GlassComplete {
        panel: Retained<CompletePanel>,
        glass: Retained<NSView>,
        container: Retained<NSView>,
        highlight: Retained<NSView>,
        labels: Vec<Retained<NSTextField>>,
        /// Alternated each update to nudge the glass to re-sample its backdrop
        /// (it otherwise caches over Metal-rendered content).
        nudge: Cell<bool>,
        visible: bool,
    }

    pub fn new() -> Option<GlassComplete> {
        let mtm = MainThreadMarker::new()?;
        unsafe {
            // A generously-sized initial frame; update() resizes per content.
            let init = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 200.0));
            // NSWindowStyleMaskNonactivatingPanel (1<<7).
            let style: usize = 1 << 7;
            let alloc = mtm.alloc::<CompletePanel>();
            let panel: Retained<CompletePanel> = msg_send![
                alloc,
                initWithContentRect: init,
                styleMask: style,
                backing: 2usize,
                defer: false,
            ];
            glass::configure_panel(&*panel);
            // Passive: never eat clicks — they belong to the terminal.
            let _: () = msg_send![&*panel, setIgnoresMouseEvents: true];

            let container: Retained<NSView> =
                msg_send![mtm.alloc::<NSView>(), initWithFrame: init];
            let _: () = msg_send![&*container, setWantsLayer: true];
            let clayer: *mut AnyObject = msg_send![&*container, layer];
            if !clayer.is_null() {
                let _: () = msg_send![clayer, setCornerRadius: CORNER_RADIUS];
                let _: () = msg_send![clayer, setMasksToBounds: true];
            }

            // Selection highlight (rounded neutral fill), behind the labels.
            let highlight: Retained<NSView> =
                msg_send![mtm.alloc::<NSView>(), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, ROW_H))];
            let _: () = msg_send![&*highlight, setWantsLayer: true];
            let hlayer: *mut AnyObject = msg_send![&*highlight, layer];
            if !hlayer.is_null() {
                let hl: Retained<NSColor> = msg_send![
                    class!(NSColor),
                    colorWithSRGBRed: 1.0f64, green: 1.0f64, blue: 1.0f64, alpha: 0.18f64,
                ];
                // Typed accessor: a raw `msg_send![…, CGColor]` typed as an
                // object aborts under objc2 0.6's return-encoding check (the
                // method returns a `CGColorRef`). `cg` stays alive until
                // `setBackgroundColor:` retains it.
                let cg = hl.CGColor();
                let _: () = msg_send![hlayer, setBackgroundColor: Retained::as_ptr(&cg) as *mut AnyObject];
                let _: () = msg_send![hlayer, setCornerRadius: 6.0f64];
            }
            container.addSubview(&highlight);

            // Pre-create the row labels.
            let font: Retained<NSFont> = msg_send![class!(NSFont), systemFontOfSize: FONT_SIZE];
            let mut labels = Vec::with_capacity(MAX_ROWS);
            for _ in 0..MAX_ROWS {
                let label: Retained<NSTextField> = msg_send![
                    mtm.alloc::<NSTextField>(),
                    initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, ROW_H))
                ];
                let _: () = msg_send![&*label, setBezeled: false];
                let _: () = msg_send![&*label, setBordered: false];
                let _: () = msg_send![&*label, setEditable: false];
                let _: () = msg_send![&*label, setSelectable: false];
                let _: () = msg_send![&*label, setDrawsBackground: false];
                let _: () = msg_send![&*label, setFont: &*font];
                // Truncate the middle so the tail (entry name) stays visible.
                let cell: *mut AnyObject = msg_send![&*label, cell];
                if !cell.is_null() {
                    let _: () = msg_send![cell, setLineBreakMode: 5isize]; // TruncatingMiddle
                }
                container.addSubview(&label);
                labels.push(label);
            }

            let glass = glass::make_glass(init, &container, CORNER_RADIUS);
            // A small custom shadow — the shared apply_drop_shadow is deeper than
            // a compact cursor popup wants.
            let shadow_color: Retained<NSColor> = msg_send![
                class!(NSColor),
                colorWithSRGBRed: 0.0f64, green: 0.0f64, blue: 0.0f64, alpha: 0.7f64,
            ];
            let ns_shadow: Retained<NSShadow> = msg_send![class!(NSShadow), new];
            let _: () = msg_send![&*ns_shadow, setShadowColor: &*shadow_color];
            let _: () = msg_send![&*ns_shadow, setShadowOffset: NSSize::new(0.0, -2.0)];
            let _: () = msg_send![&*ns_shadow, setShadowBlurRadius: 6.0f64];
            let _: () = msg_send![&*glass, setShadow: Some(&*ns_shadow)];

            glass::set_glass_content_view(&*panel, &glass, init, mtm);

            Some(GlassComplete {
                panel,
                glass,
                container,
                highlight,
                labels,
                nudge: Cell::new(false),
                visible: false,
            })
        }
    }

    impl GlassComplete {
        /// Show/refresh the popup below the cursor with `rows` (the visible
        /// slice) and `selected` (index within `rows`). `x`/`row_top`/`row_h`
        /// are the cursor cell's left / top / height in the parent window's
        /// content coordinates (logical points, top-left origin). Empty `rows`
        /// hides it.
        pub fn update(
            &mut self,
            parent: &Window,
            x: f64,
            row_top: f64,
            row_h: f64,
            rows: &[String],
            selected: usize,
            dark: bool,
        ) {
            if rows.is_empty() {
                self.hide();
                return;
            }
            let Some(parent_ns) = glass::parent_nswindow(parent) else {
                return;
            };
            let n = rows.len().min(MAX_ROWS);
            unsafe { glass::set_panel_appearance(&*self.panel, dark) };

            unsafe {
                // Set texts, measure the widest row.
                let mut content_w = MIN_W;
                for (i, label) in self.labels.iter().enumerate() {
                    if i < n {
                        let s = NSString::from_str(&rows[i]);
                        let _: () = msg_send![&**label, setStringValue: &*s];
                        let _: () = msg_send![&**label, setHidden: false];
                        let _: () = msg_send![&**label, sizeToFit];
                        let f: NSRect = msg_send![&**label, frame];
                        content_w = content_w.max(f.size.width + (PAD_H + ROW_INSET) * 2.0);
                    } else {
                        let _: () = msg_send![&**label, setHidden: true];
                    }
                }
                let card_w = content_w.clamp(MIN_W, MAX_W);
                let card_h = n as f64 * ROW_H + PAD_V * 2.0;

                // Lay out rows top-down (card coords are y-up). The label is a
                // text-height box centered in the ROW_H slot so the text sits in
                // the middle of its highlight (NSTextField draws top-aligned).
                let label_w = card_w - (PAD_H + ROW_INSET) * 2.0;
                let th = (FONT_SIZE * 1.3).round();
                for (i, label) in self.labels.iter().enumerate().take(n) {
                    let row_bottom = card_h - PAD_V - (i as f64 + 1.0) * ROW_H;
                    let frame = NSRect::new(
                        NSPoint::new(PAD_H + ROW_INSET, row_bottom + (ROW_H - th) / 2.0),
                        NSSize::new(label_w, th),
                    );
                    let _: () = msg_send![&**label, setFrame: frame];
                }

                // Selection highlight.
                if selected < n {
                    let y = card_h - PAD_V - (selected as f64 + 1.0) * ROW_H;
                    let frame = NSRect::new(
                        NSPoint::new(PAD_H, y),
                        NSSize::new(card_w - PAD_H * 2.0, ROW_H),
                    );
                    let _: () = msg_send![&*self.highlight, setFrame: frame];
                    let _: () = msg_send![&*self.highlight, setHidden: false];
                } else {
                    let _: () = msg_send![&*self.highlight, setHidden: true];
                }

                // Resize the card stack (container/glass) and the window.
                let card = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(card_w, card_h));
                let _: () = msg_send![&*self.container, setFrame: card];
                let card_in_win =
                    NSRect::new(NSPoint::new(SHADOW_MARGIN, SHADOW_MARGIN), NSSize::new(card_w, card_h));
                let _: () = msg_send![&*self.glass, setFrame: card_in_win];
                let win_w = card_w + SHADOW_MARGIN * 2.0;
                let win_h = card_h + SHADOW_MARGIN * 2.0;

                // Position within the parent window, clamped to stay inside it.
                // The surface uses fullSizeContentView, so it fills the whole
                // window frame — map against `frame` (not contentRectForFrameRect,
                // which is shorter by the title bar and offsets the popup).
                // Prefer below the cursor; flip above when that would overflow
                // the bottom (the usual case at a shell prompt near the bottom).
                let frame: NSRect = msg_send![parent_ns, frame];
                let fw = frame.size.width;
                let fh = frame.size.height;
                let win_top_screen = frame.origin.y + fh; // surface y=0 maps here
                let margin = 8.0;

                let mut x_pos = x; // from content left
                if x_pos + card_w > fw - margin {
                    x_pos = (fw - card_w - margin).max(margin);
                }
                x_pos = x_pos.max(margin);

                // Card top measured from the surface TOP edge (y-down). Below the
                // cursor uses BELOW_GAP from the row's bottom; flipping above uses
                // ABOVE_GAP from the row's top.
                let below_top = row_top + row_h + BELOW_GAP;
                let top = if below_top + card_h <= fh - margin {
                    below_top
                } else {
                    let above_top = row_top - ABOVE_GAP - card_h;
                    if above_top >= margin {
                        above_top
                    } else {
                        (fh - card_h - margin).max(margin)
                    }
                };

                let card_screen_x = frame.origin.x + x_pos;
                let card_top_screen_y = win_top_screen - top;
                let win_origin_x = card_screen_x - SHADOW_MARGIN;
                let win_origin_y = (card_top_screen_y - card_h) - SHADOW_MARGIN;
                let win_frame = NSRect::new(
                    NSPoint::new(win_origin_x, win_origin_y),
                    NSSize::new(win_w, win_h),
                );
                let _: () = msg_send![&*self.panel, setFrame: win_frame, display: false];
                let wrapper: *mut AnyObject = msg_send![&*self.panel, contentView];
                if !wrapper.is_null() {
                    let _: () = msg_send![
                        wrapper,
                        setFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, win_h))
                    ];
                }

                // Independent floating panel (not a child window): the window
                // server renders behind-window glass by live-blurring whatever is
                // behind the panel's region — here the terminal content beneath
                // it — so it updates as the terminal changes (clear, output). A
                // child window's backdrop is sampled against the parent and goes
                // stale. The clamping above keeps it bounded inside the window, so
                // it never overhangs onto the desktop. orderFront shows it without
                // taking key/focus (non-activating panel).
                let _: () = msg_send![&*self.panel, orderFront: std::ptr::null_mut::<AnyObject>()];

                // Nudge the glass to re-sample its backdrop each update (it caches
                // over the Metal-rendered terminal otherwise). Toggling
                // cornerRadius by a hair forces a recomposite. NSGlassEffectView
                // responds to setCornerRadius:; the NSVisualEffectView fallback
                // doesn't, so guard.
                let responds: bool =
                    msg_send![&*self.glass, respondsToSelector: sel!(setCornerRadius:)];
                if responds {
                    let r = CORNER_RADIUS + if self.nudge.get() { 0.0 } else { 0.1 };
                    let _: () = msg_send![&*self.glass, setCornerRadius: r];
                    self.nudge.set(!self.nudge.get());
                }
            }
            self.visible = true;
        }

        pub fn hide(&mut self) {
            if !self.visible {
                return;
            }
            unsafe {
                let _: () = msg_send![&*self.panel, orderOut: std::ptr::null_mut::<AnyObject>()];
            }
            self.visible = false;
        }
    }
}

pub use imp::*;
