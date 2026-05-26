//! Native macOS "Liquid Glass" command palette panel.
//!
//! Replaces the GPU-drawn palette overlay with a real AppKit panel: a
//! borderless `NSPanel` (subclassed only so a borderless window may still
//! become key for text input) layered over the terminal window as a child,
//! its content embedded in an `NSGlassEffectView` (macOS 26+, with an
//! `NSVisualEffectView` fallback on older systems). A search `NSTextField`
//! sits above an `NSTableView` of fuzzy-filtered results.
//!
//! The pure `command_palette` model still owns the *logic*; this is the
//! native view + input front end. Per-keystroke filtering runs natively in
//! the controller (calling the pure `command_palette` fuzzy functions
//! directly), so it stays responsive without round-tripping to the event
//! loop. Accept / dismiss are bridged back to the loop (see slice 3).
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
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, NSObjectProtocol, ProtocolObject, Sel};
    use objc2::{
        class, define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly,
    };
    use objc2_app_kit::{
        NSColor, NSControl, NSControlTextEditingDelegate, NSFont, NSPanel, NSScrollView,
        NSTableColumn, NSTableView, NSTableViewDataSource, NSText, NSTextField,
        NSTextFieldDelegate, NSView,
    };
    use objc2_foundation::{
        NSIndexSet, NSNotification, NSObject, NSPoint, NSRect, NSSize, NSString,
    };
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    use winit::window::Window;

    use crate::command_palette;

    /// Raised by the panel's `cancelOperation:` (Escape) or an accept so the
    /// event loop can sync the Rust-side open state and tear the panel down.
    /// Polled like `NEW_TAB_REQUESTED`.
    pub static PALETTE_DISMISS_REQUESTED: AtomicBool = AtomicBool::new(false);

    // Geometry (points). The panel is a fixed-width card near the top of the
    // window: a search field above a fixed-height results list.
    const PANEL_WIDTH: f64 = 560.0;
    const PAD: f64 = 12.0;
    const GAP: f64 = 8.0;
    const FIELD_INSET_X: f64 = 18.0;
    const FIELD_HEIGHT: f64 = 30.0;
    const ROW_HEIGHT: f64 = 26.0;
    const TOP_INSET: f64 = 96.0;

    fn max_rows() -> f64 {
        command_palette::PALETTE_MAX_VISIBLE as f64
    }
    fn list_height() -> f64 {
        max_rows() * ROW_HEIGHT
    }
    fn panel_height() -> f64 {
        PAD + list_height() + GAP + FIELD_HEIGHT + PAD
    }
    fn content_rect() -> NSRect {
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(PANEL_WIDTH, panel_height()),
        )
    }
    fn field_frame() -> NSRect {
        NSRect::new(
            NSPoint::new(FIELD_INSET_X, panel_height() - PAD - FIELD_HEIGHT),
            NSSize::new(PANEL_WIDTH - FIELD_INSET_X * 2.0, FIELD_HEIGHT),
        )
    }
    fn scroll_frame() -> NSRect {
        NSRect::new(
            NSPoint::new(PAD, PAD),
            NSSize::new(PANEL_WIDTH - PAD * 2.0, list_height()),
        )
    }

    // ---- Panel: borderless NSPanel that can still become key ----

    define_class! {
        #[unsafe(super(NSPanel))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniPalettePanel"]
        struct PalettePanel;

        impl PalettePanel {
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key_window(&self) -> bool {
                true
            }

            // Fallback dismiss path when the field editor isn't focused; the
            // controller handles Escape while typing.
            #[unsafe(method(cancelOperation:))]
            fn cancel_operation(&self, _sender: *mut AnyObject) {
                unsafe {
                    let _: () = msg_send![self, orderOut: std::ptr::null_mut::<AnyObject>()];
                }
                PALETTE_DISMISS_REQUESTED.store(true, Ordering::SeqCst);
            }
        }
    }

    // ---- Controller: field delegate + table data source ----

    #[derive(Default)]
    struct ControllerIvars {
        /// All candidate display strings for the current mode (command titles
        /// or, later, the chooser's options).
        candidates: RefCell<Vec<String>>,
        /// Indices into `candidates`, best match first (current filter result).
        filtered: RefCell<Vec<usize>>,
        field: RefCell<Option<Retained<NSTextField>>>,
        table: RefCell<Option<Retained<NSTableView>>>,
    }

    define_class! {
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = ControllerIvars]
        #[name = "YutaniPaletteController"]
        struct PaletteController;

        unsafe impl NSObjectProtocol for PaletteController {}

        unsafe impl NSControlTextEditingDelegate for PaletteController {
            // Query changed: re-run the fuzzy filter and refresh the list.
            #[unsafe(method(controlTextDidChange:))]
            fn control_text_did_change(&self, _notif: &NSNotification) {
                let query = match self.ivars().field.borrow().as_ref() {
                    Some(f) => f.stringValue().to_string(),
                    None => return,
                };
                self.apply_query(&query);
            }

            // Intercept navigation / accept / cancel; let everything else
            // (text editing) fall through to the field editor.
            #[unsafe(method(control:textView:doCommandBySelector:))]
            fn do_command(
                &self,
                _control: &NSControl,
                _text_view: &NSText,
                command: Sel,
            ) -> bool {
                if command == sel!(moveUp:) {
                    self.move_selection(-1);
                    true
                } else if command == sel!(moveDown:) {
                    self.move_selection(1);
                    true
                } else if command == sel!(insertNewline:) {
                    // Slice 3 wires this to the model + run_palette_action.
                    PALETTE_DISMISS_REQUESTED.store(true, Ordering::SeqCst);
                    true
                } else if command == sel!(cancelOperation:) {
                    PALETTE_DISMISS_REQUESTED.store(true, Ordering::SeqCst);
                    true
                } else {
                    false
                }
            }
        }

        // setDelegate on the field wants an NSTextFieldDelegate; the editing
        // methods above come from its NSControlTextEditingDelegate super-protocol.
        unsafe impl NSTextFieldDelegate for PaletteController {}

        unsafe impl NSTableViewDataSource for PaletteController {
            #[unsafe(method(numberOfRowsInTableView:))]
            fn number_of_rows(&self, _table: &NSTableView) -> isize {
                self.ivars().filtered.borrow().len() as isize
            }

            #[unsafe(method_id(tableView:objectValueForTableColumn:row:))]
            fn object_value(
                &self,
                _table: &NSTableView,
                _column: *mut AnyObject,
                row: isize,
            ) -> Option<Retained<NSString>> {
                let filtered = self.ivars().filtered.borrow();
                let candidates = self.ivars().candidates.borrow();
                filtered
                    .get(row as usize)
                    .and_then(|&i| candidates.get(i))
                    .map(|s| NSString::from_str(s))
            }
        }
    }

    impl PaletteController {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = mtm.alloc::<PaletteController>();
            let this = this.set_ivars(ControllerIvars::default());
            unsafe { msg_send![super(this), init] }
        }

        /// Replace the candidate list (on mode change / open) and reset filter.
        fn set_candidates(&self, candidates: Vec<String>) {
            *self.ivars().candidates.borrow_mut() = candidates;
            self.apply_query("");
        }

        /// Fuzzy-filter the candidates by `query`, reload the table, select the
        /// top row. Reuses the pure model's matcher so ranking matches exactly.
        fn apply_query(&self, query: &str) {
            let filtered = {
                let candidates = self.ivars().candidates.borrow();
                command_palette::filter_choices(&candidates, query)
            };
            *self.ivars().filtered.borrow_mut() = filtered;
            if let Some(table) = self.ivars().table.borrow().as_ref() {
                table.reloadData();
            }
            self.select_row(0);
        }

        fn move_selection(&self, delta: isize) {
            let table = self.ivars().table.borrow();
            let Some(table) = table.as_ref() else { return };
            let n = table.numberOfRows();
            if n == 0 {
                return;
            }
            let cur = table.selectedRow(); // -1 when none selected
            let next = (cur + delta).clamp(0, n - 1);
            self.select_row(next);
        }

        fn select_row(&self, row: isize) {
            let table = self.ivars().table.borrow();
            let Some(table) = table.as_ref() else { return };
            if row < 0 || row >= table.numberOfRows() {
                return;
            }
            unsafe {
                let idx = NSIndexSet::indexSetWithIndex(row as usize);
                let _: () =
                    msg_send![&**table, selectRowIndexes: &*idx, byExtendingSelection: false];
                let _: () = msg_send![&**table, scrollRowToVisible: row];
            }
        }
    }

    /// Owns the live AppKit objects for one window's palette.
    pub struct GlassPalette {
        panel: Retained<PalettePanel>,
        /// Retained for ownership clarity (the panel also strong-refs it via
        /// `setContentView:`); slice 4 reads it to apply the theme tint.
        #[allow(dead_code)]
        glass: Retained<NSView>,
        field: Retained<NSTextField>,
        controller: Retained<PaletteController>,
        visible: bool,
    }

    /// Build the panel + glass + field + results table once (lazily).
    pub fn new() -> Option<GlassPalette> {
        let mtm = MainThreadMarker::new()?;
        unsafe {
            let content = content_rect();

            // NSWindowStyleMaskNonactivatingPanel (1<<7): takes key status for
            // text input without deactivating the owning app.
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

            let container: Retained<NSView> =
                msg_send![mtm.alloc::<NSView>(), initWithFrame: content];

            // Controller wires the field + table together.
            let controller = PaletteController::new(mtm);

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
            field.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            container.addSubview(&field);

            // Results table inside a scroll view, header hidden, single column.
            let table: Retained<NSTableView> =
                msg_send![mtm.alloc::<NSTableView>(), initWithFrame: scroll_frame()];
            let col_id = NSString::from_str("command");
            let column: Retained<NSTableColumn> =
                msg_send![mtm.alloc::<NSTableColumn>(), initWithIdentifier: &*col_id];
            let _: () = msg_send![&*column, setWidth: PANEL_WIDTH - PAD * 2.0 - 20.0];
            let _: () = msg_send![&*table, addTableColumn: &*column];
            let _: () = msg_send![&*table, setHeaderView: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![&*table, setRowHeight: ROW_HEIGHT];
            let _: () = msg_send![&*table, setBackgroundColor: &*clear];
            // NSTableViewSelectionHighlightStyleRegular == 1.
            let _: () = msg_send![&*table, setSelectionHighlightStyle: 1isize];
            table.setDataSource(Some(ProtocolObject::from_ref(&*controller)));

            let scroll: Retained<NSScrollView> =
                msg_send![mtm.alloc::<NSScrollView>(), initWithFrame: scroll_frame()];
            let _: () = msg_send![&*scroll, setDocumentView: &*table];
            let _: () = msg_send![&*scroll, setHasVerticalScroller: true];
            let _: () = msg_send![&*scroll, setDrawsBackground: false];
            container.addSubview(&scroll);

            // Hand the controller its views and seed the full command list.
            *controller.ivars().field.borrow_mut() = Some(field.clone());
            *controller.ivars().table.borrow_mut() = Some(table.clone());
            controller.set_candidates(command_titles());

            let glass = make_glass(content, &container);
            let _: () = msg_send![&*panel, setContentView: &*glass];

            Some(GlassPalette {
                panel,
                glass,
                field,
                controller,
                visible: false,
            })
        }
    }

    /// The static command titles, in registry order — the candidate list for
    /// the default (browse-commands) mode.
    fn command_titles() -> Vec<String> {
        command_palette::COMMANDS
            .iter()
            .map(|c| c.title.to_string())
            .collect()
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
        /// window, show it, and focus the search field with an empty query.
        pub fn show(&mut self, parent: &Window) {
            let Some(parent_ns) = parent_nswindow(parent) else {
                return;
            };
            // Reset to a fresh, unfiltered command list.
            unsafe {
                let empty = NSString::from_str("");
                let _: () = msg_send![&*self.field, setStringValue: &*empty];
            }
            self.controller.set_candidates(command_titles());

            unsafe {
                let pf: NSRect = msg_send![parent_ns, frame];
                let w = PANEL_WIDTH;
                let h = panel_height();
                let x = pf.origin.x + (pf.size.width - w) / 2.0;
                // Screen coords are y-up: subtract from the top edge.
                let y = pf.origin.y + pf.size.height - h - TOP_INSET;
                let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
                let _: () = msg_send![&*self.panel, setFrame: frame, display: true];

                let _: () = msg_send![parent_ns, addChildWindow: &*self.panel, ordered: 1isize];
                let _: () = msg_send![
                    &*self.panel,
                    makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()
                ];
                let _: bool = msg_send![&*self.panel, makeFirstResponder: &*self.field];
            }
            self.visible = true;
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

        /// Introspect live AppKit state for non-visual verification (used by
        /// the `YUTANI_PALETTE_DEMO` smoke check, since screen capture is
        /// sandboxed). Reports the panel state plus the row count after an
        /// optional simulated query.
        pub fn debug_report(&self, simulated_query: Option<&str>) -> String {
            unsafe {
                if let Some(q) = simulated_query {
                    self.controller.apply_query(q);
                }
                let visible: bool = msg_send![&*self.panel, isVisible];
                let content: *mut AnyObject = msg_send![&*self.panel, contentView];
                let cls: *const AnyClass = msg_send![content, class];
                let cls_name = if cls.is_null() {
                    "<null>".to_string()
                } else {
                    (*cls).name().to_string_lossy().into_owned()
                };
                let rows = self
                    .controller
                    .ivars()
                    .filtered
                    .borrow()
                    .len();
                let frame: NSRect = msg_send![&*self.panel, frame];
                format!(
                    "visible={visible} contentView={cls_name} rows={rows} \
                     query={simulated_query:?} frame=({:.0},{:.0} {:.0}x{:.0})",
                    frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
                )
            }
        }
    }
}

pub use imp::*;
