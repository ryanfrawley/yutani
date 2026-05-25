//! `WindowState` keyboard input: the main key dispatcher plus the
//! command-palette and find-in-scrollback overlays that capture the keyboard
//! while open.

use crate::*;

impl WindowState {
    /// Drive the command palette from a key press while it's open. Always
    /// consumes the event (returns `true`): the palette owns the keyboard, so
    /// nothing here reaches the PTY. Cmd-Shift-P (open/close) is handled by the
    /// caller before this; everything else — navigation, text editing, accept,
    /// dismiss — is handled here.
    pub(crate) fn command_palette_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        use command_palette::Outcome;
        use winit::keyboard::{Key, NamedKey};
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                if self.command_palette.escape() == Outcome::Close {
                    self.command_palette.close();
                }
            }
            Key::Named(NamedKey::Enter) => match self.command_palette.accept() {
                Outcome::Run { action, arg } => {
                    self.command_palette.close();
                    self.run_palette_action(action, arg);
                }
                Outcome::RequestChoices { action, prompt } => {
                    // Only the host can enumerate the candidates; feed them back
                    // so the palette can present a filtered picker.
                    let choices = self.palette_choices(action);
                    self.command_palette.enter_choose(action, prompt, choices);
                }
                Outcome::Close => self.command_palette.close(),
                Outcome::Stay => {}
            },
            Key::Named(NamedKey::ArrowDown) => self.command_palette.move_down(),
            Key::Named(NamedKey::ArrowUp) => self.command_palette.move_up(),
            Key::Named(NamedKey::Backspace) => self.command_palette.backspace(),
            Key::Named(NamedKey::Delete) => self.command_palette.input.delete(),
            Key::Named(NamedKey::ArrowLeft) => self.command_palette.input.left(),
            Key::Named(NamedKey::ArrowRight) => self.command_palette.input.right(),
            Key::Named(NamedKey::Home) => self.command_palette.input.home(),
            Key::Named(NamedKey::End) => self.command_palette.input.end(),
            // Ctrl-N / Ctrl-P mirror ArrowDown / ArrowUp so the selection can
            // be moved without leaving the home row.
            Key::Character(s)
                if self.modifiers.control_key()
                    && !self.modifiers.super_key()
                    && !self.modifiers.alt_key()
                    && s.eq_ignore_ascii_case("n") =>
            {
                self.command_palette.move_down()
            }
            Key::Character(s)
                if self.modifiers.control_key()
                    && !self.modifiers.super_key()
                    && !self.modifiers.alt_key()
                    && s.eq_ignore_ascii_case("p") =>
            {
                self.command_palette.move_up()
            }
            _ => {
                // Printable text: insert it, unless a Cmd/Ctrl/Alt chord is
                // held (those aren't text input). winit hands us the composed
                // text in `event.text`.
                let plain = !self.modifiers.super_key()
                    && !self.modifiers.control_key()
                    && !self.modifiers.alt_key();
                if plain {
                    if let Some(text) = &event.text {
                        for c in text.chars() {
                            if !c.is_control() {
                                self.command_palette.type_char(c);
                            }
                        }
                    }
                }
            }
        }
        self.invalidate();
        true
    }

    /// Enumerate the candidate list for a pick-from-list palette command (one
    /// with `choose: true`), answering [`Outcome::RequestChoices`]. Kept on the
    /// host because the options come from outside the pure palette module — for
    /// `SetTheme`, the schemes on disk.
    pub(crate) fn palette_choices(&self, action: command_palette::PaletteAction) -> Vec<String> {
        use command_palette::PaletteAction as A;
        match action {
            // All three theme pickers offer the same list: the on-disk schemes
            // plus the synthetic "Default (built-in)" entry.
            A::SetTheme | A::SetLightTheme | A::SetDarkTheme => {
                theme_picker_choices(list_scheme_names())
            }
            _ => Vec::new(),
        }
    }

    /// Drive the find overlay from a key press while it's open. Always consumes
    /// the event (returns `true`): like the palette, the overlay owns the
    /// keyboard so nothing reaches the PTY. Cmd-F (open/close) is handled by the
    /// caller before this. Enter / Down steps to the next match, Shift-Enter /
    /// Up to the previous; editing the query re-runs the search live.
    pub(crate) fn search_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                // Close but leave the viewport where it is, so the match the
                // user found stays in view.
                self.search.close();
            }
            Key::Named(NamedKey::Enter) => {
                if self.modifiers.shift_key() {
                    self.search.prev();
                } else {
                    self.search.next();
                }
                self.focus_current_match();
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.search.next();
                self.focus_current_match();
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.search.prev();
                self.focus_current_match();
            }
            Key::Named(NamedKey::Backspace) => {
                self.search.backspace();
                self.run_search();
            }
            Key::Named(NamedKey::Delete) => {
                self.search.input.delete();
                self.run_search();
            }
            Key::Named(NamedKey::ArrowLeft) => self.search.input.left(),
            Key::Named(NamedKey::ArrowRight) => self.search.input.right(),
            Key::Named(NamedKey::Home) => self.search.input.home(),
            Key::Named(NamedKey::End) => self.search.input.end(),
            _ => {
                // Printable text drives the query, unless a Cmd/Ctrl/Alt chord
                // is held (those aren't text input). winit hands us composed
                // text in `event.text`.
                let plain = !self.modifiers.super_key()
                    && !self.modifiers.control_key()
                    && !self.modifiers.alt_key();
                if plain {
                    if let Some(text) = &event.text {
                        for c in text.chars() {
                            if !c.is_control() {
                                self.search.type_char(c);
                            }
                        }
                    }
                    self.run_search();
                }
            }
        }
        self.invalidate();
        true
    }

    /// Re-scan the whole buffer (scrollback + live grid) for the current query
    /// and refresh the match list, then jump to the current match. An empty
    /// query clears the matches. Called on every query edit.
    pub(crate) fn run_search(&mut self) {
        let query = self.search.input.value.clone();
        if query.is_empty() {
            self.search.set_matches(Vec::new());
            self.invalidate();
            return;
        }
        let case_sensitive = search::smart_case(&query);
        let total = self.active_tab().terminal.scrollback_len() + self.active_tab().terminal.rows;
        let mut matches = Vec::new();
        for abs in 0..total as isize {
            let Some(cells) = self.active_tab().terminal.line_at(abs) else {
                continue;
            };
            // One char per column. Drop trailing blanks so padding spaces don't
            // bloat the scan; leading offsets stay intact so columns line up.
            let last = cells
                .iter()
                .rposition(|c| c.ch != ' ')
                .map(|i| i + 1)
                .unwrap_or(0);
            if last == 0 {
                continue;
            }
            let line: String = cells[..last].iter().map(|c| c.ch).collect();
            for (start, end) in search::match_line(&line, &query, case_sensitive) {
                matches.push(search::Match {
                    line: abs,
                    start_col: start,
                    end_col: end,
                });
            }
        }
        self.search.set_matches(matches);
        self.focus_current_match();
    }

    /// Scroll the viewport so the current match is visible, then redraw.
    pub(crate) fn focus_current_match(&mut self) {
        if let Some(m) = self.search.current_match() {
            self.active_tab_mut().terminal.scroll_line_into_view(m.line);
            self.active_tab_mut().scroll_y = 0.0;
        }
        self.invalidate();
    }

    /// This window's top-left in logical points, for a Cmd-N window to cascade
    /// off (see [`spawn_window_in_process`]). `None` if the platform can't
    /// report the position — the new window then keeps the OS default spot.
    pub(crate) fn window_origin(&self) -> Option<(f64, f64)> {
        let phys = self.window.outer_position().ok()?;
        let logical: winit::dpi::LogicalPosition<f64> =
            phys.to_logical(self.window.scale_factor());
        Some((logical.x, logical.y))
    }

    /// Execute a command chosen in the palette. Every arm reuses behaviour that
    /// already exists elsewhere — the palette is a discoverable front end, not
    /// new functionality.
    pub(crate) fn run_palette_action(
        &mut self,
        action: command_palette::PaletteAction,
        arg: Option<String>,
    ) {
        use command_palette::PaletteAction as A;
        match action {
            // Route through the OSC-0/2 path so the existing title plumbing
            // (take_title_update -> effective_title) applies uniformly; the
            // event loop picks the change up after this returns.
            A::SetTitle => self.active_tab_mut()
                .terminal
                .set_window_title(arg.as_deref().unwrap_or("")),
            A::ClearTitle => self.active_tab_mut().terminal.set_window_title(""),
            A::ReloadConfig => self.reload_config(),
            // The picker hands back a label; map it to a scheme slot value
            // ("Default (built-in)" / empty -> None, revert to built-in).
            // "Set theme" is contextual: while following the system it assigns
            // the slot for the current appearance, otherwise the single
            // color_scheme.
            A::SetTheme => {
                let val = scheme_value_from_pick(arg);
                if self.config.auto_theme {
                    if self.system_is_dark() {
                        self.config.dark_scheme = val;
                    } else {
                        self.config.light_scheme = val;
                    }
                } else {
                    self.config.color_scheme = val;
                }
                self.persist_and_apply();
            }
            A::SetLightTheme => {
                self.config.light_scheme = scheme_value_from_pick(arg);
                self.persist_and_apply();
            }
            A::SetDarkTheme => {
                self.config.dark_scheme = scheme_value_from_pick(arg);
                self.persist_and_apply();
            }
            A::ToggleFollowSystem => {
                self.config.auto_theme = !self.config.auto_theme;
                self.persist_and_apply();
            }
            A::ZoomIn => self.change_font_size(1.0),
            A::ZoomOut => self.change_font_size(-1.0),
            A::ToggleWireframe => {
                if self.shared.wireframe_pipeline.is_some() {
                    self.wireframe = !self.wireframe;
                }
            }
            A::CopyLastOutput => {
                self.select_last_command_output();
            }
            A::NewWindow => self.pending_new_window = true,
            // Re-arm first-run and relaunch into it. Each window owns a single
            // PTY (forked at startup, already attached to a shell), so there's
            // no in-place way to swap the running shell for onboarding —
            // instead we lower the marker and start a fresh Yutani, which boots
            // straight into onboarding-in-PTY with live preview, then exit this
            // one. This ends the current shell session, which is why the
            // command is spelled out plainly in the palette.
            A::RunOnboarding => {
                rearm_onboarding();
                let relaunched = std::env::current_exe()
                    .and_then(|exe| std::process::Command::new(exe).spawn());
                match relaunched {
                    Ok(_) => std::process::exit(0),
                    // Couldn't relaunch — leave this session alone. The marker
                    // is already re-armed, so the next manual launch onboards.
                    Err(e) => eprintln!("onboarding: failed to relaunch: {e}"),
                }
            }
            // The popup gate (`config.autocomplete`) is read live on every
            // keystroke, so flipping it takes effect immediately; we only need
            // to persist so the choice survives a restart.
            A::ToggleAutocomplete => {
                self.config.autocomplete = !self.config.autocomplete;
                self.config.save();
            }
        }
        self.invalidate();
    }

    pub(crate) fn input(
        &mut self,
        event: &WindowEvent,
        _elwt: &EventLoopWindowTarget<app_window::CustomEvent>,
    ) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_x = position.x;
                self.mouse_y = position.y;
                // Pointer over the title bar / toolbar: swallow it so motion
                // never becomes a mouse report or extends a selection into the
                // chrome. Show the arrow instead of the grid's I-beam, and drop
                // any hovered-URL highlight since nothing hoverable is up there.
                if self.in_top_toolbar(position.y) {
                    // Re-assert the arrow on *every* move within the band, not
                    // just on entry. set_cursor_icon keeps winit's stored cursor
                    // in sync (and covers non-macOS), but on macOS it's applied
                    // lazily via cursorUpdate:, which never fires over the native
                    // title-bar overlay — so we also push the arrow straight onto
                    // NSCursor here, or the grid's I-beam stays frozen on screen
                    // while the pointer is up here. See force_native_arrow_cursor.
                    self.over_toolbar = true;
                    self.window
                        .set_cursor_icon(winit::window::CursorIcon::Default);
                    force_native_arrow_cursor(&self.window);
                    if self.active_tab_mut().hover_url.take().is_some() {
                        self.invalidate();
                    }
                    return true;
                }
                if self.over_toolbar {
                    // Crossed back into the grid: restore the I-beam.
                    // update_hover_url below upgrades it to a pointer if the
                    // cursor is over a Cmd-hovered URL.
                    self.over_toolbar = false;
                    self.window
                        .set_cursor_icon(winit::window::CursorIcon::Text);
                }
                // Mouse-mode reporting takes precedence unless the user is
                // shift-overriding it for local selection.
                let mouse_mode_active =
                    self.active_tab().terminal.mouse_protocol().enabled() && !self.modifiers.shift_key();
                if mouse_mode_active {
                    if let Some(b) = self.held_button {
                        self.report_mouse(b, true, true);
                    } else if self.active_tab().terminal.mouse_protocol().any_motion {
                        // Per xterm, "no button" motion uses code 3 (release-ish).
                        self.report_mouse(3, true, true);
                    }
                } else if self.held_button == Some(input::MOUSE_LEFT) {
                    self.handle_mouse_drag();
                    self.invalidate();
                }
                self.update_hover_url();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Press/release over the title bar / toolbar drives the window
                // chrome (drag, traffic lights), never the shell. Clear any
                // held button so a press that began here can't seed a phantom
                // selection or motion report once the pointer moves down.
                if self.in_top_toolbar(self.mouse_y) {
                    self.held_button = None;
                    if *button == MouseButton::Left && *state == ElementState::Pressed {
                        let now = std::time::Instant::now();
                        let double = self
                            .last_toolbar_click
                            .map_or(false, |t| now.duration_since(t) < DOUBLE_CLICK_THRESHOLD);
                        if double {
                            // Double-click the title bar zooms the window, the
                            // standard macOS gesture. We have to do this
                            // ourselves: drag_window below consumes the first
                            // click's mouseDown in a modal tracking loop, so the
                            // OS never sees the pair as a double-click.
                            self.last_toolbar_click = None;
                            self.window.set_maximized(!self.window.is_maximized());
                        } else {
                            self.last_toolbar_click = Some(now);
                            // Kick off a native window drag immediately. Our
                            // content view spans the title bar
                            // (fullsize_content_view) and consumes this
                            // mouseDown, so without this AppKit falls back to its
                            // slow drag path — the window only starts following
                            // the cursor after a ~1s hesitation. Routing the live
                            // mouseDown into performWindowDragWithEvent: makes the
                            // drag begin on the first movement. Clicks on the
                            // traffic lights don't reach us (system subviews on
                            // top), so this only fires on the empty draggable
                            // strip.
                            let _ = self.window.drag_window();
                        }
                    }
                    return true;
                }
                let code = match button {
                    MouseButton::Left => Some(input::MOUSE_LEFT),
                    MouseButton::Middle => Some(input::MOUSE_MIDDLE),
                    MouseButton::Right => Some(input::MOUSE_RIGHT),
                    _ => None,
                };
                if let Some(code) = code {
                    let press = *state == ElementState::Pressed;
                    if press {
                        self.held_button = Some(code);
                    } else {
                        self.held_button = None;
                    }
                    // Cmd-click on a hovered URL opens it. Done before the
                    // mouse-mode check so the gesture works even when an app
                    // (vim, less) has grabbed mouse tracking, matching the
                    // behavior every other macOS terminal ships.
                    if press
                        && code == input::MOUSE_LEFT
                        && self.modifiers.super_key()
                    {
                        if let Some(hu) = self.active_tab().hover_url.clone() {
                            if is_safe_url(&hu.url) {
                                open_url(&hu.url);
                            }
                            // Consume the click either way: a Cmd-click on a
                            // link shouldn't also fall through to selection.
                            return true;
                        }
                    }
                    let mouse_mode_active = self.active_tab().terminal.mouse_protocol().enabled()
                        && !self.modifiers.shift_key();
                    if mouse_mode_active {
                        self.report_mouse(code, press, false);
                        return true;
                    }
                    // Local selection: left-down anchors a fresh range,
                    // left-up either keeps the drag-built range or drops a
                    // bare click.
                    if code == input::MOUSE_LEFT {
                        if press {
                            self.handle_mouse_press();
                        } else {
                            self.handle_mouse_release();
                        }
                        self.invalidate();
                        return true;
                    }
                }
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                // Wheel/scroll while the pointer sits over the title bar /
                // toolbar shouldn't reach the shell's wheel reporting nor the
                // local scrollback — swallow it like the other chrome events.
                if self.in_top_toolbar(self.mouse_y) {
                    return true;
                }
                let m = self.shared.with_font(|f| f.face().size_metrics().unwrap());
                let line_height = ((m.ascender - m.descender) >> 6) as f64;
                // A `Started` after a real idle gap is the user putting fingers
                // back on the trackpad — that supersedes any prior suppression.
                // Without the gap check, momentum's own Started (which fires
                // ~one frame after the previous gesture's Ended) would clear
                // the flag and let the tail of the flick re-scroll the view
                // after a key snap.
                const FRESH_GESTURE_GAP: std::time::Duration =
                    std::time::Duration::from_millis(100);
                let now = std::time::Instant::now();
                let gap = self.active_tab().last_wheel_at.map(|t| now.duration_since(t));
                if matches!(phase, TouchPhase::Started)
                    && gap.map_or(true, |g| g >= FRESH_GESTURE_GAP)
                {
                    self.active_tab_mut().scroll_suppressed = false;
                }
                if self.active_tab().scroll_suppressed {
                    // Don't advance `last_wheel_at` on suppressed events —
                    // otherwise the steady stream of momentum ticks keeps
                    // resetting the idle gap, and a real fresh gesture that
                    // arrives mid-momentum still looks like a 16ms follow-up.
                    return true;
                }
                self.active_tab_mut().last_wheel_at = Some(now);
                // Scroll-wheel forwarding to the PTY when an app has asked
                // for mouse tracking (vim, less, htop). Otherwise the wheel
                // drives our own scrollback viewport.
                if self.active_tab().terminal.mouse_protocol().enabled() {
                    // Accumulate pixels so a slow trackpad gesture (many
                    // sub-line events) still produces wheel reports instead
                    // of truncating every event to 0. LineDelta synthesizes
                    // pixels at line_height so both inputs share the drain.
                    let pixels = match delta {
                        MouseScrollDelta::LineDelta(_, d) => *d as f64 * line_height,
                        MouseScrollDelta::PixelDelta(p) => p.y,
                    };
                    let notches = input::drain_wheel_accum(
                        &mut self.active_tab_mut().wheel_pty_accum,
                        pixels,
                        line_height,
                    );
                    for _ in 0..notches.up {
                        self.report_mouse(input::MOUSE_WHEEL_UP, true, false);
                    }
                    for _ in 0..notches.down {
                        self.report_mouse(input::MOUSE_WHEEL_DOWN, true, false);
                    }
                    return true;
                }
                // Alt screen has no scrollback to navigate. With DEC mode
                // ?1007 (alternate scroll) — on by default, reachable here only
                // because mouse reporting is off (that path returned above) —
                // translate wheel motion into cursor-key presses so pagers
                // (less, man) and other full-screen apps scroll. When ?1007 is
                // disabled, swallow the wheel: full-screen apps provide their
                // own keyboard motion, and without this guard trackpad pixels
                // would accumulate in scroll_y and drift the grid past bounds.
                if self.active_tab().terminal.on_alt_screen() {
                    if self.active_tab().terminal.alternate_scroll() {
                        let pixels = match delta {
                            MouseScrollDelta::LineDelta(_, d) => *d as f64 * line_height,
                            MouseScrollDelta::PixelDelta(p) => p.y,
                        };
                        let notches = input::drain_wheel_accum(
                            &mut self.active_tab_mut().wheel_pty_accum,
                            pixels,
                            line_height,
                        );
                        let app_cursor = self.active_tab().terminal.app_cursor_keys();
                        for _ in 0..notches.up {
                            self.write_pty(&input::alt_scroll_key(true, app_cursor));
                        }
                        for _ in 0..notches.down {
                            self.write_pty(&input::alt_scroll_key(false, app_cursor));
                        }
                        // The app's response (a scroll op) will arrive on the
                        // next feed and (re)start the slide via
                        // `maybe_start_alt_scroll`; leave `scroll_y` to the
                        // animation rather than zeroing it here.
                    } else {
                        // No alternate scroll and no scrollback to navigate —
                        // swallow the wheel so trackpad pixels can't drift the
                        // grid via an accumulated offset.
                        self.active_tab_mut().scroll_y = 0.0;
                    }
                    return true;
                }
                // The user is taking over scrollback navigation — drop any
                // in-flight scroll-on-output slide so it doesn't fight the
                // gesture for `scroll_y` on the next frame.
                self.finish_primary_scroll();
                match delta {
                    MouseScrollDelta::LineDelta(_, d) => {
                        let n = d.round().abs() as usize;
                        if *d > 0.0 {
                            self.active_tab_mut().terminal.scroll_up(n);
                        } else if *d < 0.0 {
                            self.active_tab_mut().terminal.scroll_down(n);
                        }
                        // Discrete scrolls snap — don't leave a sub-line offset.
                        self.active_tab_mut().scroll_y = 0.0;
                    }
                    MouseScrollDelta::PixelDelta(p) => {
                        self.active_tab_mut().scroll_y += p.y;
                        // Drain accumulated pixels into discrete line scrolls.
                        // Zero the residue if scroll_up/down refused so scroll_y
                        // can't accumulate past a viewport boundary regardless
                        // of what at_top/at_bottom report.
                        while self.active_tab().scroll_y >= line_height {
                            if !self.active_tab_mut().terminal.scroll_up(1) {
                                self.active_tab_mut().scroll_y = 0.0;
                                break;
                            }
                            self.active_tab_mut().scroll_y -= line_height;
                        }
                        while self.active_tab().scroll_y <= -line_height {
                            if !self.active_tab_mut().terminal.scroll_down(1) {
                                self.active_tab_mut().scroll_y = 0.0;
                                break;
                            }
                            self.active_tab_mut().scroll_y += line_height;
                        }
                        // Hard-stop at viewport boundaries: no elastic overscroll.
                        if self.active_tab().scroll_y > 0.0 && self.active_tab().terminal.at_top() {
                            self.active_tab_mut().scroll_y = 0.0;
                        }
                        if self.active_tab().scroll_y < 0.0 && self.active_tab().terminal.at_bottom() {
                            self.active_tab_mut().scroll_y = 0.0;
                        }
                    }
                }
                self.invalidate();
                // Content slid under the pointer — the URL (if any) might be
                // different now.
                self.update_hover_url();
                return true;
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                // Pressing/releasing Cmd flips URL-hover affordances on or
                // off, even though the mouse hasn't moved.
                self.update_hover_url();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == winit::event::ElementState::Pressed {
                    // Cmd-F toggles the find-in-scrollback overlay. Checked
                    // first (and gated on !shift so it never fires for a
                    // Cmd-Shift chord) so it both opens the overlay and, while
                    // open, closes it before the overlay's key handler below
                    // swallows the keystroke. Closing leaves the viewport where
                    // it is so the found match stays in view.
                    if self.modifiers.super_key()
                        && !self.modifiers.shift_key()
                        && !self.command_palette.open
                    {
                        if let winit::keyboard::Key::Character(s) = &event.logical_key {
                            if s.eq_ignore_ascii_case("f") {
                                self.search.toggle();
                                self.invalidate();
                                return true;
                            }
                        }
                    }
                    // While the find overlay is open it owns the keyboard.
                    if self.search.open {
                        return self.search_key(&event);
                    }
                    // Cmd-Shift-P toggles the command palette. Checked before
                    // everything else so it both opens the palette and, while
                    // it's open, closes it (the palette's own key handler below
                    // otherwise swallows the keystroke).
                    if self.modifiers.super_key() && self.modifiers.shift_key() {
                        if let winit::keyboard::Key::Character(s) = &event.logical_key {
                            if s.eq_ignore_ascii_case("p") {
                                self.command_palette.toggle();
                                self.invalidate();
                                return true;
                            }
                        }
                    }
                    // While the palette is open it owns the keyboard: every
                    // keystroke filters/drives it and nothing reaches the PTY.
                    if self.command_palette.open {
                        return self.command_palette_key(&event);
                    }
                    // Cmd+C / Cmd+V: copy / paste through the system
                    // clipboard. Done before encode_key so the super_key
                    // check there doesn't drop them.
                    if self.modifiers.super_key() {
                        // Cmd-Shift-Up / Cmd-Shift-Down: jump the viewport to
                        // the previous / next shell prompt (OSC 133 marks).
                        // No-op when the shell isn't emitting marks or on the
                        // alt screen. Arrow keys arrive as Named, not
                        // Character, so handle them before the Character match.
                        if self.modifiers.shift_key() {
                            use winit::keyboard::{Key, NamedKey};
                            if event.logical_key == Key::Named(NamedKey::ArrowUp) {
                                if self.active_tab_mut().terminal.scroll_to_prev_prompt() {
                                    self.active_tab_mut().scroll_y = 0.0;
                                    self.invalidate();
                                    self.update_hover_url();
                                }
                                return true;
                            }
                            if event.logical_key == Key::Named(NamedKey::ArrowDown) {
                                if self.active_tab_mut().terminal.scroll_to_next_prompt() {
                                    self.active_tab_mut().scroll_y = 0.0;
                                    self.invalidate();
                                    self.update_hover_url();
                                }
                                return true;
                            }
                        }
                        if let winit::keyboard::Key::Character(s) = &event.logical_key {
                            if s.eq_ignore_ascii_case("c") {
                                self.copy_selection();
                                return true;
                            }
                            if s.eq_ignore_ascii_case("v") {
                                self.paste_from_clipboard();
                                return true;
                            }
                            // Cmd-N: launch a new Yutani window. It's a fresh
                            // process (one window per process), opened in the
                            // current shell's working directory. Guard on
                            // !shift so Cmd-Shift-N stays free for a future
                            // binding.
                            if !self.modifiers.shift_key() && s.eq_ignore_ascii_case("n") {
                                // Request an in-process window; the event loop
                                // (which owns AppShared + the window map) does
                                // the actual spawn after `input` returns.
                                self.pending_new_window = true;
                                return true;
                            }
                            // Cmd-+ / Cmd-= zoom in, Cmd-- zooms out. macOS
                            // delivers `=` for the unshifted key and `+` when
                            // shift is held, so handle both as "increase".
                            if s.as_ref() == "+" || s.as_ref() == "=" {
                                self.change_font_size(1.0);
                                return true;
                            }
                            if s.as_ref() == "-" || s.as_ref() == "_" {
                                self.change_font_size(-1.0);
                                return true;
                            }
                            // Cmd-Shift-W: toggle wireframe debug view.
                            // Shift makes "w" arrive as "W"; check both for
                            // safety across keyboard layouts.
                            if self.modifiers.shift_key()
                                && (s.eq_ignore_ascii_case("w"))
                            {
                                if self.shared.wireframe_pipeline.is_some() {
                                    self.wireframe = !self.wireframe;
                                    self.window.request_redraw();
                                }
                                return true;
                            }
                            // Cmd-Shift-R: re-read the config file and
                            // swap the color scheme without restarting.
                            // Other config fields (font, pipeline knobs)
                            // still need a restart — see `reload_config`.
                            if self.modifiers.shift_key()
                                && (s.eq_ignore_ascii_case("r"))
                            {
                                self.reload_config();
                                return true;
                            }
                            // Cmd-Shift-I: load and place a debug image at
                            // the cursor. Path comes from `YUTANI_DEBUG_IMAGE`
                            // or `~/.config/yutani/debug_image.png`. Silent
                            // no-op (with stderr message) if neither exists.
                            if self.modifiers.shift_key()
                                && (s.eq_ignore_ascii_case("i"))
                            {
                                if let Some(path) = Self::debug_image_path() {
                                    let cur = self.active_tab().terminal.cursor();
                                    let row = cur.row as isize;
                                    let col = cur.col as isize;
                                    let path_str = path.to_string_lossy().to_string();
                                    self.load_image_at_cell(
                                        &path_str,
                                        row,
                                        col,
                                        "debug image (Cmd-Shift-I)",
                                    );
                                }
                                return true;
                            }
                            // Cmd-Shift-O: select and copy the last completed
                            // command's output (OSC 133 shell integration).
                            if self.modifiers.shift_key() && s.eq_ignore_ascii_case("o") {
                                self.select_last_command_output();
                                return true;
                            }
                            // Cmd-[ / Cmd-] tune the dual-Kawase iteration
                            // count live so the user can scrub through blur
                            // radii without recompiling.
                            if s.as_ref() == "[" || s.as_ref() == "{" {
                                self.blur.iterations = self.blur.iterations.saturating_sub(1).max(1);
                                self.config.blur_iterations = self.blur.iterations;
                                self.config.save();
                                println!("blur iterations: {}", self.blur.iterations);
                                self.window.request_redraw();
                                return true;
                            }
                            if s.as_ref() == "]" || s.as_ref() == "}" {
                                self.blur.iterations = (self.blur.iterations + 1)
                                    .min(renderer::blur::MAX_BLUR_ITERATIONS);
                                self.config.blur_iterations = self.blur.iterations;
                                self.config.save();
                                println!("blur iterations: {}", self.blur.iterations);
                                self.window.request_redraw();
                                return true;
                            }
                        }
                    }
                    // Ctrl+Space manually summons/refreshes the completion popup
                    // (autocomplete slice K14). Placed after the Cmd shortcuts and
                    // before the popup-active interception so it works whether or
                    // not a popup is currently open, and before the general
                    // encode_key path that would otherwise send NUL to the shell.
                    if self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key()
                    {
                        use winit::keyboard::{Key, NamedKey};
                        // winit 0.29 may deliver Space as Named(Space) (see
                        // input::named_key) or as a Character(" ") (folded to NUL
                        // by ctrl_byte); match both so the trigger is robust.
                        let is_space = matches!(&event.logical_key, Key::Named(NamedKey::Space))
                            || matches!(&event.logical_key, Key::Character(s) if s.as_str() == " ");
                        if is_space {
                            // Consume the key so the usual NUL byte doesn't reach
                            // the shell; summon/refresh the popup instead.
                            self.trigger_completion();
                            return true;
                        }
                    }
                    // Completion popup keyboard interaction (autocomplete slice
                    // K11). Only acts when the popup is genuinely active (cached
                    // suggestions + live view) and no Ctrl/Alt/Super is held;
                    // Shift is allowed so Shift-Tab can navigate up. Intercepting
                    // here — after the Cmd shortcuts, before the encode_key path —
                    // means these keys reach the shell normally when no popup is
                    // open, and only steer the popup while it is.
                    let popup_active =
                        !self.active_tab().completions.is_empty() && self.active_tab().terminal.view_offset() == 0;
                    let plain = !self.modifiers.control_key()
                        && !self.modifiers.alt_key()
                        && !self.modifiers.super_key();
                    if popup_active && plain {
                        use winit::keyboard::{Key, NamedKey};
                        let len = self.active_tab().completions.len();
                        match &event.logical_key {
                            Key::Named(NamedKey::ArrowDown) => {
                                self.active_tab_mut().selected_completion =
                                    (self.active_tab().selected_completion + 1).min(len - 1);
                                self.active_tab_mut().completion_scroll = completion::visible_window_start(
                                    self.active_tab().selected_completion,
                                    self.active_tab().completion_scroll,
                                    COMPLETION_MAX_VISIBLE,
                                );
                                self.invalidate();
                                return true;
                            }
                            // ArrowUp, and Shift-Tab, move the selection up.
                            Key::Named(NamedKey::ArrowUp) => {
                                self.active_tab_mut().selected_completion =
                                    self.active_tab().selected_completion.saturating_sub(1);
                                self.active_tab_mut().completion_scroll = completion::visible_window_start(
                                    self.active_tab().selected_completion,
                                    self.active_tab().completion_scroll,
                                    COMPLETION_MAX_VISIBLE,
                                );
                                self.invalidate();
                                return true;
                            }
                            Key::Named(NamedKey::Tab) if self.modifiers.shift_key() => {
                                self.active_tab_mut().selected_completion =
                                    self.active_tab().selected_completion.saturating_sub(1);
                                self.active_tab_mut().completion_scroll = completion::visible_window_start(
                                    self.active_tab().selected_completion,
                                    self.active_tab().completion_scroll,
                                    COMPLETION_MAX_VISIBLE,
                                );
                                self.invalidate();
                                return true;
                            }
                            // Tab accepts the suffix and, on a directory, drills
                            // in (popup stays open and refilters to the dir's
                            // contents); on a file it accepts and closes.
                            Key::Named(NamedKey::Tab) => {
                                let keep = self.active_tab()
                                    .completions
                                    .get(self.active_tab().selected_completion)
                                    .map(|s| s.is_dir)
                                    .unwrap_or(false);
                                self.accept_selected_completion(keep, false);
                                return true;
                            }
                            // Shift+Enter: accept the highlighted completion AND
                            // run the command in one keystroke (writes the
                            // accept-suffix + a carriage return, then closes the
                            // popup). Must precede the plain Enter arm.
                            Key::Named(NamedKey::Enter) if self.modifiers.shift_key() => {
                                self.accept_selected_completion(false, true);
                                return true;
                            }
                            // Enter accepts + closes + suppresses reopen. It
                            // intentionally does NOT submit the command — a
                            // second Enter (popup now empty, so not intercepted)
                            // submits normally.
                            Key::Named(NamedKey::Enter) => {
                                self.accept_selected_completion(false, false);
                                return true;
                            }
                            Key::Named(NamedKey::Escape) => {
                                // Dismiss without accepting: clear the list and
                                // set the dismissed flag so the popup stays
                                // closed even when the shell re-reports input,
                                // until the user types more.
                                self.active_tab_mut().completions.clear();
                                self.active_tab_mut().completions_input = None;
                                self.active_tab_mut().completion_dismissed = true;
                                self.invalidate();
                                return true;
                            }
                            _ => {}
                        }
                    }
                    // macOS Option-as-Meta: with Option held, winit reports the
                    // layout-composed char (e.g. Option+A → "å"). For terminal
                    // meta-bindings we want the base key, so `Option+A` sends
                    // `ESC a` rather than `ESC 0xC3 0xA5`. `key_without_modifiers`
                    // also resolves dead keys (Option+E → "e" instead of Dead('´')).
                    let alt_stripped = self
                        .modifiers
                        .alt_key()
                        .then(|| event.key_without_modifiers());
                    let logical_key = alt_stripped.as_ref().unwrap_or(&event.logical_key);
                    let text = if alt_stripped.is_some() {
                        None
                    } else {
                        event.text.as_deref()
                    };
                    let bytes = input::encode_key(
                        logical_key,
                        text,
                        self.modifiers,
                        self.active_tab().terminal.app_cursor_keys(),
                    );
                    if let Some(bytes) = bytes {
                        // A keystroke we're sending to the PTY snaps the view
                        // back to the live grid; passive modifiers (Cmd+C etc.)
                        // returned None and don't touch the scroll state.
                        self.active_tab_mut().terminal.scroll_to_bottom();
                        // A keystroke cancels any in-flight scroll slide —
                        // snap straight to the settled frame.
                        self.finish_alt_scroll();
                        self.finish_primary_scroll();
                        self.active_tab_mut().scroll_y = 0.0;
                        // Drop any in-flight trackpad momentum so the snap
                        // sticks — otherwise the tail of the flick keeps
                        // scrolling the view away from the bottom.
                        self.active_tab_mut().scroll_suppressed = true;
                        self.reset_blink();
                        self.clear_selection();
                        // A genuine keystroke re-enables the popup after a
                        // finish/dismiss. The auto-inserted accept suffix goes
                        // through `write_pty` directly (not this path), so
                        // accepting never clears the flag — only real input does.
                        self.active_tab_mut().completion_dismissed = false;
                        self.write_pty(&bytes);
                        self.invalidate();
                        return true;
                    }
                }
            }
            _ => (),
        }
        false
    }
}
