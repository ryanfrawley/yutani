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
//! loop. Accept / dismiss are posted to a signal channel the event loop
//! drains in `about_to_wait` (like `NEW_TAB_REQUESTED`), where it drives the
//! model and reconfigures or closes the panel.
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
        pub fn enter_commands(&self) {}
        pub fn enter_argument(&self, _prompt: &str) {}
        pub fn enter_choose(&self, _prompt: &str, _choices: Vec<String>) {}
        pub fn set_appearance(&self, _dark: bool) {}
    }

    /// Always `None` off macOS; the palette falls back to the GPU overlay.
    pub fn new() -> Option<GlassPalette> {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::{Cell, RefCell};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject, Sel};
    use objc2::{
        class, define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly,
    };
    use objc2_app_kit::{
        NSColor, NSControl, NSControlTextEditingDelegate, NSFont, NSPanel, NSScrollView,
        NSTableColumn, NSTableRowView, NSTableView, NSTableViewDataSource,
        NSTableViewDelegate, NSText, NSTextField, NSTextFieldDelegate, NSView, NSWindowDelegate,
    };
    use objc2_foundation::{
        NSEdgeInsets, NSIndexSet, NSNotification, NSObject, NSPoint, NSRect, NSSize, NSString,
    };
    use winit::window::Window;

    use crate::command_palette;
    use crate::glass::{self, SignalSlot};

    /// What the user did in the native panel, handed to the event loop (which
    /// owns `WindowState`) to drive the model. Resolved to the focused window,
    /// like `NEW_TAB_REQUESTED`.
    pub enum PaletteSignal {
        /// Enter: run the selected row's text (list modes) or the field's free
        /// text (argument mode). `None` = nothing selected → no-op.
        Accept(Option<String>),
        /// Escape / cancel: back out a step or close.
        Dismiss,
        /// Lost key focus (clicked away, app switch): close outright.
        Close,
    }

    /// Single-slot mailbox from the AppKit controller (main thread) to the
    /// event loop (also main thread).
    static PALETTE_SIGNAL: SignalSlot<PaletteSignal> = SignalSlot::new();

    fn post(sig: PaletteSignal) {
        PALETTE_SIGNAL.post(sig);
    }

    /// Drain the pending palette signal, if any. Called by the event loop.
    pub fn take_palette_signal() -> Option<PaletteSignal> {
        PALETTE_SIGNAL.take()
    }

    // Geometry (points). The panel is a fixed-width card near the top of the
    // window. The *window* is larger than the card by SHADOW_MARGIN on every
    // side — a transparent gutter the deep drop shadow renders into (a window's
    // own content can't paint outside its frame). Card-relative frames (field,
    // list rows) live in the glass's content view, whose origin is the card.
    const PANEL_WIDTH: f64 = 560.0;
    const PAD: f64 = 12.0;
    const GAP: f64 = 8.0;
    const FIELD_INSET_X: f64 = 18.0;
    const FIELD_HEIGHT: f64 = 30.0;
    const ROW_HEIGHT: f64 = 28.0;
    // Left inset of a row label *within its cell*. The cell sits at the scroll
    // view's left (PAD), so PAD + ROW_LABEL_INSET lines the row text up with the
    // search field at FIELD_INSET_X.
    const ROW_LABEL_INSET: f64 = FIELD_INSET_X - PAD;
    const ROW_FONT_SIZE: f64 = 15.0;
    const TOP_INSET: f64 = 96.0;
    /// Transparent gutter around the card for the drop shadow (must exceed the
    /// shadow radius + downward offset so the shadow isn't clipped).
    const SHADOW_MARGIN: f64 = 56.0;
    /// Glass corner radius (larger = more edge refraction / curvature).
    const CORNER_RADIUS: f64 = 20.0;

    fn list_height() -> f64 {
        command_palette::PALETTE_MAX_VISIBLE as f64 * ROW_HEIGHT
    }
    /// Visible card height for a given mode: full (field + list) when a list is
    /// shown, or just the field row in free-text argument mode.
    fn card_height_for(list_mode: bool) -> f64 {
        if list_mode {
            PAD + list_height() + GAP + FIELD_HEIGHT + PAD
        } else {
            // The field draws its text top-aligned, so it already carries some
            // slack below the text; a small bottom inset keeps it visually
            // balanced without an empty band under the field.
            PAD + FIELD_HEIGHT + 4.0
        }
    }
    /// Full card height (list mode) — the construction/layout baseline.
    fn card_height() -> f64 {
        card_height_for(true)
    }
    /// Full window size (card + shadow gutter on all sides).
    fn window_size() -> NSSize {
        NSSize::new(
            PANEL_WIDTH + SHADOW_MARGIN * 2.0,
            card_height() + SHADOW_MARGIN * 2.0,
        )
    }
    fn content_rect() -> NSRect {
        NSRect::new(NSPoint::new(0.0, 0.0), window_size())
    }
    /// The card's frame within the window (offset by the shadow gutter).
    fn card_rect() -> NSRect {
        NSRect::new(
            NSPoint::new(SHADOW_MARGIN, SHADOW_MARGIN),
            NSSize::new(PANEL_WIDTH, card_height()),
        )
    }
    /// Card-local bounds (origin 0) for the glass's content view.
    fn card_bounds() -> NSRect {
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_WIDTH, card_height()))
    }
    fn field_frame() -> NSRect {
        NSRect::new(
            NSPoint::new(FIELD_INSET_X, card_height() - PAD - FIELD_HEIGHT),
            NSSize::new(PANEL_WIDTH - FIELD_INSET_X * 2.0, FIELD_HEIGHT),
        )
    }
    fn scroll_frame() -> NSRect {
        // Full-bleed to the bottom edge of the card (y = 0); spans from the
        // bottom up to just below the search field.
        let h = card_height() - PAD - FIELD_HEIGHT - GAP;
        NSRect::new(NSPoint::new(PAD, 0.0), NSSize::new(PANEL_WIDTH - PAD * 2.0, h))
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
            // controller handles Escape while typing. Let the event loop decide
            // whether to back out a step or close, so don't order out here.
            #[unsafe(method(cancelOperation:))]
            fn cancel_operation(&self, _sender: *mut AnyObject) {
                post(PaletteSignal::Dismiss);
            }

            // Fade in/out driven on the next run-loop tick (via performSelector):
            // an NSAnimationContext animation set up synchronously inside winit's
            // event-loop callbacks commits without animating.
            #[unsafe(method(yutaniFadeIn))]
            fn yutani_fade_in(&self) {
                unsafe { glass::animate_alpha(self, 1.0, 0.18) }
            }

            // Animate the panel to a new frame (carried as an NSValue, since
            // performSelector takes one object arg). Deferred to the next
            // run-loop tick so the animation isn't committed inside a callback.
            #[unsafe(method(yutaniResizeTo:))]
            fn yutani_resize_to(&self, value: *mut AnyObject) {
                unsafe {
                    let frame: NSRect = msg_send![value, rectValue];
                    let _: () = msg_send![class!(NSAnimationContext), beginGrouping];
                    let ctx: *mut AnyObject = msg_send![class!(NSAnimationContext), currentContext];
                    let _: () = msg_send![ctx, setDuration: 0.18f64];
                    let anim: *mut AnyObject = msg_send![self, animator];
                    let _: () = msg_send![anim, setFrame: frame, display: true];
                    let _: () = msg_send![class!(NSAnimationContext), endGrouping];
                }
            }

            #[unsafe(method(yutaniFadeOut))]
            fn yutani_fade_out(&self) {
                unsafe {
                    glass::animate_alpha(self, 0.0, 0.16);
                    // Order out once the fade finishes.
                    let _: () = msg_send![
                        self,
                        performSelector: sel!(orderOut:),
                        withObject: std::ptr::null_mut::<AnyObject>(),
                        afterDelay: 0.17f64,
                    ];
                }
            }
        }
    }

    // ---- Row view: forces the neutral (non-emphasized) selection fill ----

    define_class! {
        #[unsafe(super(NSTableRowView))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniPaletteRowView"]
        struct PaletteRowView;

        impl PaletteRowView {
            // The blue accent fill is the "emphasized" selection (key window).
            // Reporting unemphasized gives the neutral gray highlight the user
            // wants, even while the panel is key and a row is mouse-selected.
            #[unsafe(method(isEmphasized))]
            fn is_emphasized(&self) -> bool {
                false
            }
        }
    }

    // ---- Controller: field delegate + table data source ----

    struct ControllerIvars {
        /// All candidate display strings for the current mode (command titles
        /// or the chooser's options). Empty in argument mode.
        candidates: RefCell<Vec<String>>,
        /// Indices into `candidates`, best match first (current filter result).
        filtered: RefCell<Vec<usize>>,
        /// True in command / choose mode (Enter accepts the selected row),
        /// false in argument mode (Enter accepts the field's free text).
        list_mode: Cell<bool>,
        /// Current scroll-edge fade state, so the mask only re-animates when an
        /// edge actually crosses its threshold (not on every scroll tick).
        fade_top: Cell<bool>,
        fade_bottom: Cell<bool>,
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

        impl PaletteController {
            // Posted by the clip view when the user scrolls; refresh the edges.
            #[unsafe(method(scrollBoundsChanged:))]
            fn scroll_bounds_changed(&self, _notif: &NSNotification) {
                self.update_scroll_fade();
            }

            // Single click on a row: run it (the click already selected it).
            #[unsafe(method(rowClicked:))]
            fn row_clicked(&self, _sender: *mut AnyObject) {
                let clicked = match self.ivars().table.borrow().as_ref() {
                    Some(table) => unsafe { msg_send![&**table, clickedRow] },
                    None => -1isize,
                };
                if clicked >= 0 {
                    self.accept();
                }
            }
        }

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
                    self.accept();
                    true
                } else if command == sel!(cancelOperation:) {
                    post(PaletteSignal::Dismiss);
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
        }

        unsafe impl NSTableViewDelegate for PaletteController {
            // View-based rows: a transparent NSTextField label positioned to
            // line up with the search field and vertically centered in the row,
            // so we control inset / font / color / alignment exactly.
            #[unsafe(method_id(tableView:viewForTableColumn:row:))]
            fn view_for_row(
                &self,
                _table: &NSTableView,
                _column: *mut AnyObject,
                row: isize,
            ) -> Option<Retained<NSView>> {
                let text = {
                    let filtered = self.ivars().filtered.borrow();
                    let candidates = self.ivars().candidates.borrow();
                    filtered
                        .get(row as usize)
                        .and_then(|&i| candidates.get(i))
                        .cloned()
                };
                // Single tail expression: the method_id macro wraps the return,
                // so an early `return None` isn't allowed here.
                text.map(|text| {
                    let mtm = MainThreadMarker::from(self);
                    unsafe {
                        let w = PANEL_WIDTH - PAD * 2.0;
                        let cell_frame =
                            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, ROW_HEIGHT));
                        let container: Retained<NSView> =
                            msg_send![mtm.alloc::<NSView>(), initWithFrame: cell_frame];
                        let _: () = msg_send![&*container, setAutoresizingMask: 2usize]; // width

                        let th = (ROW_FONT_SIZE * 1.3).round();
                        let label_frame = NSRect::new(
                            NSPoint::new(ROW_LABEL_INSET, ((ROW_HEIGHT - th) / 2.0).round()),
                            NSSize::new(w - ROW_LABEL_INSET - PAD, th),
                        );
                        let label: Retained<NSTextField> =
                            msg_send![mtm.alloc::<NSTextField>(), initWithFrame: label_frame];
                        let _: () = msg_send![&*label, setEditable: false];
                        let _: () = msg_send![&*label, setSelectable: false];
                        let _: () = msg_send![&*label, setBezeled: false];
                        let _: () = msg_send![&*label, setBordered: false];
                        let _: () = msg_send![&*label, setDrawsBackground: false];
                        let font: Retained<NSFont> =
                            msg_send![class!(NSFont), systemFontOfSize: ROW_FONT_SIZE];
                        let _: () = msg_send![&*label, setFont: &*font];
                        let s = NSString::from_str(&text);
                        let _: () = msg_send![&*label, setStringValue: &*s];
                        let _: () = msg_send![&*label, setAutoresizingMask: 2usize]; // width
                        container.addSubview(&label);
                        container
                    }
                })
            }

            // Use our row view so the selection draws neutral, not blue.
            #[unsafe(method_id(tableView:rowViewForRow:))]
            fn row_view_for_row(
                &self,
                _table: &NSTableView,
                _row: isize,
            ) -> Option<Retained<NSTableRowView>> {
                let mtm = MainThreadMarker::from(self);
                let row: Retained<PaletteRowView> = unsafe { msg_send![mtm.alloc::<PaletteRowView>(), init] };
                Some(Retained::into_super(row))
            }
        }

        unsafe impl NSWindowDelegate for PaletteController {
            // Clicking outside the panel (or switching apps) makes it resign key;
            // close the palette outright rather than backing out a step.
            #[unsafe(method(windowDidResignKey:))]
            fn window_did_resign_key(&self, _notif: &NSNotification) {
                post(PaletteSignal::Close);
            }
        }
    }

    impl PaletteController {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = mtm.alloc::<PaletteController>();
            let this = this.set_ivars(ControllerIvars {
                candidates: RefCell::new(Vec::new()),
                filtered: RefCell::new(Vec::new()),
                list_mode: Cell::new(true),
                fade_top: Cell::new(false),
                fade_bottom: Cell::new(false),
                field: RefCell::new(None),
                table: RefCell::new(None),
            });
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
            self.update_scroll_fade();
        }

        /// Toggle the scroll-edge fade at each edge based on whether there is
        /// content scrolled past it (top) or still below (bottom).
        fn update_scroll_fade(&self) {
            let table = self.ivars().table.borrow();
            let Some(table) = table.as_ref() else { return };
            unsafe {
                let scroll: *mut AnyObject = msg_send![&**table, enclosingScrollView];
                if scroll.is_null() {
                    return;
                }
                let slayer: *mut AnyObject = msg_send![scroll, layer];
                if slayer.is_null() {
                    return;
                }
                let mask: *mut AnyObject = msg_send![slayer, mask];
                if mask.is_null() {
                    return;
                }
                let visible: NSRect = msg_send![scroll, documentVisibleRect];
                let doc: *mut AnyObject = msg_send![scroll, documentView];
                if doc.is_null() {
                    return;
                }
                let doc_frame: NSRect = msg_send![doc, frame];
                let fade_top = visible.origin.y > 0.5;
                let fade_bottom =
                    visible.origin.y + visible.size.height < doc_frame.size.height - 0.5;
                // Only re-set (and animate) the mask when an edge crosses its
                // threshold, so the fade animates in/out instead of snapping and
                // doesn't restart every scroll tick.
                if fade_top == self.ivars().fade_top.get()
                    && fade_bottom == self.ivars().fade_bottom.get()
                {
                    return;
                }
                self.ivars().fade_top.set(fade_top);
                self.ivars().fade_bottom.set(fade_bottom);
                let _: () = msg_send![class!(CATransaction), begin];
                let _: () = msg_send![class!(CATransaction), setAnimationDuration: 0.25f64];
                let _: () = msg_send![mask, setColors: gradient_colors(fade_top, fade_bottom)];
                let _: () = msg_send![class!(CATransaction), commit];
            }
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

        /// Enter: post the selection (or, in argument mode, the field text) so
        /// the event loop can drive the model. No-op if nothing is selected.
        fn accept(&self) {
            let signal = if self.ivars().list_mode.get() {
                let table = self.ivars().table.borrow();
                let Some(table) = table.as_ref() else { return };
                let sel = table.selectedRow();
                if sel < 0 {
                    return;
                }
                let chosen = {
                    let filtered = self.ivars().filtered.borrow();
                    let candidates = self.ivars().candidates.borrow();
                    filtered
                        .get(sel as usize)
                        .and_then(|&i| candidates.get(i))
                        .cloned()
                };
                PaletteSignal::Accept(chosen)
            } else {
                let text = self
                    .ivars()
                    .field
                    .borrow()
                    .as_ref()
                    .map(|f| f.stringValue().to_string());
                PaletteSignal::Accept(text)
            };
            post(signal);
        }
    }

    /// Owns the live AppKit objects for one window's palette.
    pub struct GlassPalette {
        panel: Retained<PalettePanel>,
        /// Retained for ownership clarity (the panel also strong-refs it via
        /// `setContentView:`); slice 4 reads it to apply the theme tint.
        #[allow(dead_code)]
        glass: Retained<NSView>,
        scroll: Retained<NSScrollView>,
        field: Retained<NSTextField>,
        controller: Retained<PaletteController>,
        visible: bool,
    }

    /// Build the panel + glass + field + results table once (lazily).
    pub fn new() -> Option<GlassPalette> {
        let mtm = MainThreadMarker::new()?;
        unsafe {
            let content = content_rect();

            // Borderless (style 0); the canBecomeKeyWindow override lets it take
            // key status so the search field accepts typing. (A nonactivating
            // panel only becomes key "when needed" — i.e. on a click — so
            // makeKeyAndOrderFront: wouldn't focus the field.)
            let style: usize = 0;
            let alloc = mtm.alloc::<PalettePanel>();
            let panel: Retained<PalettePanel> = msg_send![
                alloc,
                initWithContentRect: content,
                styleMask: style,
                backing: 2usize, // NSBackingStoreBuffered
                defer: false,
            ];
            // Borderless, transparent, key-on-demand, floating, no system shadow
            // (we render our own deeper rounded shadow into the window gutter).
            glass::configure_panel(&*panel);
            let clear = NSColor::clearColor();

            // The glass's content view is card-local (origin 0); the field and
            // results list are positioned within it. Clip it to the rounded card
            // so the full-bleed list doesn't poke past the rounded bottom corners
            // (the glass material has no opaque content here, so clipping the
            // content view doesn't affect its refraction).
            let container: Retained<NSView> =
                msg_send![mtm.alloc::<NSView>(), initWithFrame: card_bounds()];
            // Width+height sizable so it tracks the glass as the card resizes.
            let _: () = msg_send![&*container, setAutoresizingMask: 18usize];
            let _: () = msg_send![&*container, setWantsLayer: true];
            let clayer: *mut AnyObject = msg_send![&*container, layer];
            if !clayer.is_null() {
                let _: () = msg_send![clayer, setCornerRadius: CORNER_RADIUS];
                let _: () = msg_send![clayer, setMasksToBounds: true];
            }

            // Controller wires the field + table together.
            let controller = PaletteController::new(mtm);

            // Search field: borderless, transparent, large system font.
            let field: Retained<NSTextField> =
                msg_send![mtm.alloc::<NSTextField>(), initWithFrame: field_frame()];
            let _: () = msg_send![&*field, setBezeled: false];
            let _: () = msg_send![&*field, setBordered: false];
            let _: () = msg_send![&*field, setDrawsBackground: false];
            let _: () = msg_send![&*field, setFocusRingType: 1isize]; // None
            let font: Retained<NSFont> = msg_send![class!(NSFont), systemFontOfSize: 18.0f64];
            let _: () = msg_send![&*field, setFont: &*font];
            // Pin to the top, fixed height, flexible width (width + min-Y margin).
            let _: () = msg_send![&*field, setAutoresizingMask: 10usize];
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
            let _: () = msg_send![&*table, setOpaque: false];
            // No grid lines / alternating fills (they'd paint over the glass).
            let _: () = msg_send![&*table, setGridStyleMask: 0usize];
            let _: () = msg_send![&*table, setUsesAlternatingRowBackgroundColors: false];
            // NSTableViewSelectionHighlightStyleRegular == 1.
            let _: () = msg_send![&*table, setSelectionHighlightStyle: 1isize];
            table.setDataSource(Some(ProtocolObject::from_ref(&*controller)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            // Single click on a row runs it (like Enter).
            let _: () = msg_send![&*table, setTarget: &*controller];
            let _: () = msg_send![&*table, setAction: sel!(rowClicked:)];

            let scroll: Retained<NSScrollView> =
                msg_send![mtm.alloc::<NSScrollView>(), initWithFrame: scroll_frame()];
            // Pin to the bottom, flexible height (width + height + max-Y margin).
            let _: () = msg_send![&*scroll, setAutoresizingMask: 50usize];
            let _: () = msg_send![&*scroll, setDocumentView: &*table];
            let _: () = msg_send![&*scroll, setHasVerticalScroller: true];
            let _: () = msg_send![&*scroll, setDrawsBackground: false];
            let clip: *mut AnyObject = msg_send![&*scroll, contentView];
            if !clip.is_null() {
                let _: () = msg_send![clip, setDrawsBackground: false];
            }
            // NSTableView special-cases the clearColor *singleton* (treats it as
            // "use the default control background"), so a genuine zero-alpha
            // color is needed to actually make the table transparent.
            let transparent: Retained<NSColor> = msg_send![
                class!(NSColor),
                colorWithSRGBRed: 0.0f64, green: 0.0f64, blue: 0.0f64, alpha: 0.0f64,
            ];
            let _: () = msg_send![&*table, setBackgroundColor: &*transparent];
            // No auto insets; flush at the top (no padding above the first row),
            // but a bottom inset so the last row clears the bottom edge when
            // fully scrolled.
            let _: () = msg_send![&*scroll, setAutomaticallyAdjustsContentInsets: false];
            let insets = NSEdgeInsets {
                top: 0.0,
                left: 0.0,
                bottom: PAD,
                right: 0.0,
            };
            let _: () = msg_send![&*scroll, setContentInsets: insets];
            // Soft, dynamic scroll-edge fade (AppKit only exposes the real
            // NSScrollEdgeEffect for toolbar/split-view accessories, so mimic it
            // with a gradient mask updated on scroll).
            install_scroll_fade(&scroll);
            if !clip.is_null() {
                let _: () = msg_send![clip, setPostsBoundsChangedNotifications: true];
                let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
                let name = NSString::from_str("NSViewBoundsDidChangeNotification");
                let _: () = msg_send![
                    center,
                    addObserver: &*controller,
                    selector: sel!(scrollBoundsChanged:),
                    name: &*name,
                    object: clip,
                ];
            }
            container.addSubview(&scroll);

            // Hand the controller its views.
            *controller.ivars().field.borrow_mut() = Some(field.clone());
            *controller.ivars().table.borrow_mut() = Some(table.clone());

            // The glass card, positioned within the window's shadow gutter.
            let glass = glass::make_glass(card_rect(), &container, CORNER_RADIUS);
            // Fixed margins, sizable — resizes with the window, staying inset.
            let _: () = msg_send![&*glass, setAutoresizingMask: 18usize];
            // The glass casts its own shadow — nothing opaque sits behind it, so
            // it still refracts the terminal (the liquid-glass effect).
            glass::apply_drop_shadow(&glass);

            // Wrapper fills the whole window (card inset by the shadow gutter),
            // layer-backed so the glass's shadow composites.
            glass::set_glass_content_view(&*panel, &glass, content, mtm);
            // Focus the search field as soon as the panel becomes key.
            let _: () = msg_send![&*panel, setInitialFirstResponder: &*field];
            // Observe resign-key so a click outside / app switch closes it.
            panel.setDelegate(Some(ProtocolObject::from_ref(&*controller)));

            let palette = GlassPalette {
                panel,
                glass,
                scroll,
                field,
                controller,
                visible: false,
            };
            palette.enter_commands();
            Some(palette)
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

    /// Install a 4-stop vertical gradient mask on the scroll view. The two end
    /// stops (top/bottom edges) are toggled opaque/clear by `update_scroll_fade`
    /// based on scroll position, mimicking the system scroll-edge effect (which
    /// AppKit only vends for toolbar/split accessories). Starts fully opaque.
    unsafe fn install_scroll_fade(scroll: &NSScrollView) {
        let _: () = msg_send![scroll, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![scroll, layer];
        if layer.is_null() {
            return;
        }
        let bounds: NSRect = msg_send![scroll, bounds];
        let grad: *mut AnyObject = msg_send![class!(CAGradientLayer), layer];
        let _: () = msg_send![grad, setFrame: bounds];
        let _: () = msg_send![grad, setColors: gradient_colors(false, false)];
        let locs: *mut AnyObject = msg_send![class!(NSMutableArray), array];
        // Layer unit coords here run top (loc 0) -> bottom (loc 1).
        for loc in [0.0f64, 0.14, 0.86, 1.0] {
            let n: *mut AnyObject = msg_send![class!(NSNumber), numberWithDouble: loc];
            let _: () = msg_send![locs, addObject: n];
        }
        let _: () = msg_send![grad, setLocations: locs];
        let _: () = msg_send![grad, setStartPoint: NSPoint::new(0.5, 0.0)];
        let _: () = msg_send![grad, setEndPoint: NSPoint::new(0.5, 1.0)];
        let _: () = msg_send![layer, setMask: grad];
    }

    /// Build the 4 mask colors: a clear top stop when `fade_top`, a clear bottom
    /// stop when `fade_bottom`, opaque in between (and at any non-faded edge).
    unsafe fn gradient_colors(fade_top: bool, fade_bottom: bool) -> *mut AnyObject {
        let make = |a: f64| -> *mut AnyObject {
            let c: Retained<NSColor> = msg_send![
                class!(NSColor),
                colorWithSRGBRed: 1.0f64, green: 1.0f64, blue: 1.0f64, alpha: a,
            ];
            msg_send![&*c, CGColor]
        };
        let opaque = make(1.0);
        let top = if fade_top { make(0.0) } else { opaque };
        let bottom = if fade_bottom { make(0.0) } else { opaque };
        let colors: *mut AnyObject = msg_send![class!(NSMutableArray), array];
        let _: () = msg_send![colors, addObject: top];
        let _: () = msg_send![colors, addObject: opaque];
        let _: () = msg_send![colors, addObject: opaque];
        let _: () = msg_send![colors, addObject: bottom];
        colors
    }

    impl GlassPalette {
        pub fn visible(&self) -> bool {
            self.visible
        }

        /// Configure the field placeholder + candidate list + accept semantics
        /// for the current mode, clearing the query.
        fn set_mode(&self, placeholder: &str, candidates: Vec<String>, list_mode: bool) {
            unsafe {
                let ph = NSString::from_str(placeholder);
                let _: () = msg_send![&*self.field, setPlaceholderString: &*ph];
                let empty = NSString::from_str("");
                let _: () = msg_send![&*self.field, setStringValue: &*empty];
                // The list is meaningless in free-text argument mode.
                let _: () = msg_send![&*self.scroll, setHidden: !list_mode];
            }
            self.controller.ivars().list_mode.set(list_mode);
            self.controller.set_candidates(candidates);
            self.resize_panel(list_mode);
        }

        /// Animate the panel to the height for `list_mode`, keeping its top edge
        /// anchored (so it grows/shrinks downward from under the search field).
        /// No-op when the size already matches (e.g. on initial open).
        fn resize_panel(&self, list_mode: bool) {
            unsafe {
                let cur: NSRect = msg_send![&*self.panel, frame];
                let new_h = card_height_for(list_mode) + SHADOW_MARGIN * 2.0;
                if (new_h - cur.size.height).abs() < 0.5 {
                    return;
                }
                let top = cur.origin.y + cur.size.height;
                let frame = NSRect::new(
                    NSPoint::new(cur.origin.x, top - new_h),
                    NSSize::new(cur.size.width, new_h),
                );
                let value: Retained<AnyObject> = msg_send![class!(NSValue), valueWithRect: frame];
                let _: () = msg_send![
                    &*self.panel,
                    performSelector: sel!(yutaniResizeTo:),
                    withObject: &*value,
                    afterDelay: 0.0f64,
                ];
            }
        }

        /// Browse the full command list (the default mode on open).
        pub fn enter_commands(&self) {
            self.set_mode("Command palette", command_titles(), true);
        }

        /// Collect a free-text argument (e.g. a window title).
        pub fn enter_argument(&self, prompt: &str) {
            self.set_mode(prompt, Vec::new(), false);
        }

        /// Pick from a host-supplied list (e.g. color schemes).
        pub fn enter_choose(&self, prompt: &str, choices: Vec<String>) {
            self.set_mode(prompt, choices, true);
        }

        /// Match the panel's appearance (and thus its glass + label colors) to
        /// the terminal's light/dark theme, so text stays legible on the glass.
        pub fn set_appearance(&self, dark: bool) {
            unsafe { glass::set_panel_appearance(&*self.panel, dark) }
        }

        /// Centre the card near the top of the parent window's frame. The window
        /// is bigger than the card by SHADOW_MARGIN all around, so its origin is
        /// the card's desired origin shifted by the gutter.
        fn place(&self, parent_ns: *mut AnyObject) {
            let card = NSSize::new(PANEL_WIDTH, card_height());
            unsafe {
                glass::place_card(&*self.panel, parent_ns, card, window_size(), TOP_INSET, SHADOW_MARGIN);
            }
        }

        /// Position over `parent` (centered, near the top), attach as a child
        /// window, show it, and focus the search field on the command list.
        pub fn show(&mut self, parent: &Window) {
            let Some(parent_ns) = glass::parent_nswindow(parent) else {
                return;
            };
            self.enter_commands();
            self.place(parent_ns);
            unsafe {
                // Start transparent, then fade in.
                let _: () = msg_send![&*self.panel, setAlphaValue: 0.0f64];
                let _: () = msg_send![parent_ns, addChildWindow: &*self.panel, ordered: 1isize];
                let _: () = msg_send![
                    &*self.panel,
                    makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()
                ];
                let _: bool = msg_send![&*self.panel, makeFirstResponder: &*self.field];
                // Calling makeKeyAndOrderFront synchronously from inside winit's
                // key-event handler doesn't reliably make the panel key, so also
                // re-assert it on the next runloop pass.
                let _: () = msg_send![
                    &*self.panel,
                    performSelector: sel!(makeKeyAndOrderFront:),
                    withObject: std::ptr::null_mut::<AnyObject>(),
                    afterDelay: 0.0f64,
                ];
                // Defer the fade so it isn't committed inside the key handler.
                let _: () = msg_send![
                    &*self.panel,
                    performSelector: sel!(yutaniFadeIn),
                    withObject: std::ptr::null_mut::<AnyObject>(),
                    afterDelay: 0.0f64,
                ];
            }
            self.visible = true;
        }

        /// Fade out, then detach + order out once the animation finishes.
        pub fn hide(&mut self) {
            if !self.visible {
                return;
            }
            // Detach now (the window stays visible, independent, during the
            // fade); a re-show re-attaches it as a child.
            unsafe { glass::detach_and_fade_out(&*self.panel) }
            self.visible = false;
        }
    }
}

pub use imp::*;
