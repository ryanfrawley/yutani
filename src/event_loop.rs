//! The winit event loop: window + GPU bring-up, font worker, tab/window
//! lifecycle, and the per-event dispatch into `WindowState`. Driven by `main()`.
//!
//! winit 0.30 replaced the closure-based `EventLoop::run` with the
//! `ApplicationHandler` trait: the app's long-lived state lives in [`App`] and
//! each event class is delivered to a trait method (`resumed`, `window_event`,
//! `user_event`, `about_to_wait`) carrying an `&ActiveEventLoop` in place of the
//! old `elwt`. Window/GPU/font bring-up — which the old code ran *before*
//! `run()` — now happens in `resumed`, the first point a window can be created.

use crate::*;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

/// Long-lived state for the process. Built empty by [`run`] and populated on the
/// first [`resumed`](App::resumed); the per-event trait methods then route
/// against it. The window registry (`windows` + `tab_to_window`) is owned here
/// rather than against a single `state` binding so one process can host many
/// windows (Cmd-N) and many tabs.
struct App {
    /// Dispatches PTY reader output back onto the loop. Cloned into each tab.
    proxy: EventLoopProxy<app_window::CustomEvent>,
    /// Once-per-process shared GPU/font resources, cloned per window for Cmd-N.
    /// `None` until `resumed` brings them up.
    shared: Option<Rc<AppShared>>,
    /// Materialized zsh shell-integration ZDOTDIR (None when opted out / not
    /// zsh), inherited by every forked shell.
    zdotdir: Option<std::path::PathBuf>,
    /// The window registry. Events resolve to a `WindowState` by `WindowId`;
    /// `tab_to_window` maps a `TabId` (carried on every PtyInput/PtyExit) back
    /// to its owning window.
    windows: std::collections::HashMap<WindowId, WindowState>,
    tab_to_window: std::collections::HashMap<app_window::TabId, WindowId>,
    /// The currently key window, so the tab bar's `+` button (which raises a
    /// global flag with no window context) opens a tab in the right group.
    focused_window: Option<WindowId>,
    /// Startup-timing toggle + origin, printed only when YUTANI_STARTUP_TIMING
    /// is set so the perf work stays reproducible without spamming launches.
    timing: bool,
    t_start: std::time::Instant,
    /// Latches once the first frame is presented, for the FIRST FRAME timing.
    first_frame_done: bool,
    /// Guards the one-time bring-up in `resumed` (called again on some
    /// platforms after suspend/resume; on desktop it fires once at startup).
    inited: bool,
}

/// Build the event loop and run the app. Sync (was `async` pre-0.30): GPU
/// bring-up moved into `App::resumed`, where it blocks on the same future.
pub(crate) fn run() {
    env_logger::init();
    let t_start = std::time::Instant::now();
    let timing = std::env::var_os("YUTANI_STARTUP_TIMING").is_some();

    // Drop the built-in color schemes into ~/.config/yutani/schemes/ before the
    // PTY child forks for onboarding, so a fresh install has themes to offer in
    // setup and the palette picker. Only writes ones not already present, so
    // user edits are never clobbered. Best-effort; runs only in the parent
    // (the `--onboard` child branches out of `main` before reaching here).
    bundled_schemes::seed();

    let event_loop = EventLoop::<app_window::CustomEvent>::with_user_event()
        .build()
        .unwrap();
    let proxy = event_loop.create_proxy();

    let mut app = App {
        proxy,
        shared: None,
        zdotdir: None,
        windows: std::collections::HashMap::new(),
        tab_to_window: std::collections::HashMap::new(),
        focused_window: None,
        timing,
        t_start,
        first_frame_done: false,
        inited: false,
    };
    let _ = event_loop.run_app(&mut app);
}

impl ApplicationHandler<app_window::CustomEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Desktop fires `resumed` once at startup; guard so a later resume
        // (e.g. after suspend) doesn't rebuild the window/GPU stack.
        if self.inited {
            return;
        }
        self.inited = true;

        let t_start = self.t_start;
        let timing = self.timing;
        let lap = |label: &str| {
            if timing {
                eprintln!("[startup] {:>7.1}ms  {label}", t_start.elapsed().as_secs_f64() * 1000.0);
            }
        };

        // On first run (or after a "Run first-time setup…" re-arm), the PTY
        // child becomes the onboarding console program instead of the shell; it
        // execs the shell in-place when done. Resolve our own path here, in the
        // parent, so the post-fork child doesn't have to.
        let child_program = if needs_onboarding() {
            match std::env::current_exe() {
                Ok(exe) => pty::ChildProgram::Onboard { exe },
                // Can't find ourselves to re-exec — skip onboarding rather than
                // fail to launch. It'll retry next time the marker is still unset.
                Err(_) => pty::ChildProgram::Shell,
            }
        } else {
            pty::ChildProgram::Shell
        };

        // Materialize the zsh shell-integration ZDOTDIR (no-op when opted out or
        // $SHELL isn't zsh) so the forked shell auto-loads it — no manual
        // `source …` in the user's rc required. The PTY itself is forked by
        // `create_tab` once the window/font exist and the grid size is known.
        let zdotdir = shell_integration::prepare_zdotdir();

        // Seed the window title from our own working directory — the shell the
        // PTY just forked inherits it (no chdir in the child), so this matches
        // what the first OSC 7 will report, avoiding a bare "Yutani" flash before
        // the first prompt.
        let initial_cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned));
        let initial_title = effective_title(None, initial_cwd.as_deref());

        let transparent = false; // needed because of a shadow bug
        // The first window takes the OS default position; subsequent (Cmd-N)
        // windows cascade off their spawner in `spawn_window_in_process`.
        let attrs = Window::default_attributes()
            .with_title(&initial_title)
            .with_titlebar_transparent(true)
            // Give the first window a known tab-group id (and
            // thus Preferred tabbing mode) so Cmd-T tabs reliably join it.
            .with_tabbing_identifier(&next_tab_group_id())
            .with_transparent(transparent)
            .with_has_shadow(!transparent)
            .with_fullsize_content_view(true)
            .with_decorations(true)
            .with_blur(transparent);
        let window = event_loop.create_window(attrs).unwrap();
        lap("after window build");
        // Teach the window class to answer `newWindowForTab:` so the
        // native tab bar shows its `+` button and routes clicks to us.
        install_new_tab_action(&window);

        let config = Config::load();
        lap("after config load");

        // Load all font data on a worker thread while the GPU is brought up on
        // this (main) thread. The two are independent until WindowState::new needs both,
        // so overlapping them hides whichever finishes first. Font work must hand
        // back owned bytes (FreeType faces aren't Send); GPU/surface creation must
        // stay on the main thread (Cocoa isn't thread-safe), hence this split.
        let config_for_fonts = config.clone();
        let font_handle = std::thread::spawn(move || load_font_data(&config_for_fonts));

        // GPU bring-up is async; `resumed` is sync, so block on it here exactly
        // as the old pre-`run()` path did with `.await`.
        let (gpu, surface, surface_raw) = pollster::block_on(gpu::Gpu::new(&window));
        lap("after Gpu::new (concurrent with font load)");

        let fd = font_handle.join().expect("font loader thread panicked");
        lap("after font data loaded (joined)");

        // Install the color scheme before constructing WindowState so style.rs and the
        // renderer see the right palette on their first read. Missing file is a
        // soft failure: warn and keep defaults so a typo in the config name
        // doesn't take the terminal down. With `auto_theme` on, pick the slot for
        // the OS's current appearance up front so we open in the right scheme.
        // Touches the window, so it stays on the main thread (after the join).
        let initial_dark = window.theme() == Some(winit::window::Theme::Dark);
        install_color_scheme(config.active_scheme(initial_dark));
        // Match the NSAppearance to the palette so the title-bar text the OS
        // draws over our transparent chrome reads against the actual bg —
        // otherwise dark schemes render black "Yutani" text on a dark fill.
        window.set_theme(Some(theme_for_bg(palette::get().background)));
        set_native_window_bg(&window, palette::get().background);

        let pt_size = config.font_size;
        let dpi = (window.scale_factor() * 96.0) as u32;

        // Turn the loaded bytes into FreeType faces + the rustybuzz shaper. This is
        // the not-Send tail that has to run here. Regular comes from the primary
        // lookup (face 0); the styled cuts carry their own TTC face index.
        let mut shaper = shaper::Shaper::new();
        shaper.set_variant(font::FaceVariant::Regular, &fd.primary_data, 0);
        let mut font = font::Font::new(fd.primary_data);
        font.set_char_size(pt_size, dpi);

        for (variant, data, face_index) in fd.styled {
            shaper.set_variant(variant, &data, face_index as u32);
            font.set_variant(variant, data, face_index, pt_size, dpi);
        }

        // Pre-shape every candidate ligature sequence for each installed variant.
        for variant in font::FaceVariant::ALL {
            shaper.precompute(variant);
        }

        // Attach the fallback faces. A styled fallback only attaches when its
        // primary cut actually built (same guard as before — otherwise the chain
        // is dead weight and Atlas::lookup tumbles to Regular anyway).
        for (_label, _family, variant, data, face_index) in fd.fallbacks {
            if variant != font::FaceVariant::Regular
                && font.variants[variant as usize].face.is_none()
            {
                continue;
            }
            font.add_fallback(variant, data, face_index, pt_size, dpi);
        }

        lap("after font faces + shaper built");

        // Build the once-per-process shared resources (device/queue/font/shaper +
        // pipelines), then the initial tab + window against them. `shared` is kept
        // (cloned per window) so Cmd-N can spawn further windows in-process.
        let shared = Rc::new(AppShared::new(gpu, surface.config.format, font, shaper));
        lap("after AppShared::new");

        // Size the initial tab's grid to this window's viewport so the vertex
        // buffers create_window allocates match the terminal dimensions.
        let (cols, rows) = {
            let (cell_w, line_h) = shared.with_font(|f| {
                let m = f.metrics();
                (f.cell_width(), ((m.ascender - m.descender) >> 6) as usize)
            });
            let vp = WindowState::get_viewport_size(
                surface.config.width as f32,
                surface.config.height as f32,
                cell_w,
                line_h,
                0.0, // first window opens standalone — no tab bar
            );
            (vp.char_width, vp.char_height)
        };
        let (tab_id, initial_tab) = create_tab(
            &self.proxy,
            child_program,
            zdotdir.clone(),
            None, // first window inherits our process cwd, as before
            cols,
            rows,
            config.images_memory_cap_mb * 1024 * 1024,
        );
        lap("after create_tab (fork)");
        let mut state =
            WindowState::create_window(shared.clone(), window, surface, surface_raw, config.clone(), dpi, initial_tab);
        lap("after create_window (GPU/atlas/pipelines)");
        state.notify_pty_size(state.active_tab().terminal.cols, state.active_tab().terminal.rows);
        // Size the chrome band to the real native title bar now that the window
        // exists; the field was seeded with the renderer's reserve in WindowState::new.
        state.refresh_chrome_band();
        state.window.set_cursor(winit::window::CursorIcon::Text);
        state.sync_theme_colors();
        state.tabs[state.active]
            .terminal
            .set_keep_placements_in_scrollback(state.config.images_in_scrollback);
        state.sync_terminal_cell_size();
        // Developer smoke-test hook: drop a placement at the top-left on
        // startup if `YUTANI_TEST_IMAGE` is set. Goes through the same path
        // as the Cmd-Shift-I keybind so both are exercised together.
        if let Ok(path) = std::env::var("YUTANI_TEST_IMAGE") {
            state.load_image_at_cell(&path, 0, 0, "YUTANI_TEST_IMAGE");
        }
        state.invalidate();

        let initial_window_id = state.window.id();
        self.windows.insert(initial_window_id, state);
        self.tab_to_window.insert(tab_id, initial_window_id);
        self.focused_window = Some(initial_window_id);
        self.zdotdir = zdotdir;
        self.shared = Some(shared);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, n: app_window::CustomEvent) {
        match n {
            app_window::CustomEvent::PtyInput(ev_tab) => {
                // Resolve the tab's window. A just-closed tab can still
                // deliver one last event — treat an unknown id as a no-op.
                let Some(state) = self
                    .tab_to_window
                    .get(&ev_tab)
                    .and_then(|wid| self.windows.get_mut(wid))
                else {
                    return;
                };
                // Drain a bounded slice of this tab's buffered output. The
                // reader coalesces a burst of reads into one wake, so one
                // `PtyInput` can stand for tens of MB; capping the feed per
                // turn (and self-waking for the rest, below) keeps the loop
                // free to repaint and service clicks/keystrokes mid-flood
                // instead of blocking for the whole burst. (Each window owns
                // exactly one tab, so `active_tab` is `ev_tab`'s tab.)
                let (z, more) = state.active_tab().pty_outbox.drain_up_to(PTY_FEED_CAP);
                if z.is_empty() {
                    // A duplicate/stale wake whose bytes a prior drain already
                    // took — nothing to do.
                    return;
                }
                let bytes = z.len();
                let t0 = std::time::Instant::now();
                state.feed_terminal(&z);
                let reply = state.active_tab_mut().terminal.take_response();
                if !reply.is_empty() {
                    state.write_pty(&reply);
                }
                // Recompute the window title when either the program-set
                // title (OSC 0/2) or the shell's cwd (OSC 7) changed. A
                // manual title wins; otherwise we show the cwd ($HOME
                // collapsed to `~`). Both `take_*` calls must run to clear
                // their dirty flags even when the title doesn't change.
                let title_changed = state.active_tab_mut().terminal.take_title_update().map(|t| {
                    state.manual_title = t;
                });
                let cwd_changed = state.active_tab_mut().terminal.take_cwd_update();
                if title_changed.is_some() || cwd_changed.is_some() {
                    state.window.set_title(&effective_title(
                        state.manual_title.as_deref(),
                        state.active_tab().terminal.cwd(),
                    ));
                }
                // The shell may have reported its history file (OSC 2124):
                // read + parse it and merge past commands into the in-memory
                // history (most-recent-first), behind any already-captured
                // session commands so those stay at the front.
                if let Some(path) = state.active_tab_mut().terminal.take_histfile_update() {
                    if let Ok(contents) = std::fs::read_to_string(&path) {
                        let parsed = completion::parse_zsh_history(&contents);
                        for cmd in parsed.iter().rev() {
                            if !state.active_tab().command_history.iter().any(|c| c == cmd) {
                                state.active_tab_mut().command_history.push(cmd.clone());
                            }
                        }
                        state.active_tab_mut().command_history.truncate(COMMAND_HISTORY_CAP);
                    }
                }
                // A command just submitted at the prompt (OSC 133 C): fold
                // it into the front of the history (deduped).
                if let Some(cmd) = state.active_tab_mut().terminal.take_submitted_command() {
                    dedup_prepend(&mut state.active_tab_mut().command_history, cmd);
                }
                // First-run onboarding (running as the PTY child) may have
                // emitted OSC 2125 live-preview requests in this chunk —
                // apply each to the running renderer.
                for req in state.active_tab_mut().terminal.take_preview_requests() {
                    state.apply_preview(req);
                }
                // The chunk may have carried an OSC 2122 input report;
                // refresh the completion popup's cached suggestions (only
                // recomputes — and only touches disk — when the input
                // actually changed). `invalidate()` below redraws.
                state.recompute_completions();
                state.perf.note_pty(bytes, t0.elapsed());
                state.invalidate();
                // New / removed cells may have changed which URL (if any)
                // sits under the pointer.
                state.update_hover_url();
                // More was buffered than we fed this turn: post a fresh wake so
                // the loop renders this slice and handles input before coming
                // back for the rest. (`state`'s borrow ends above, freeing
                // `self.proxy`.)
                if more {
                    let _ = self.proxy.send_event(app_window::CustomEvent::PtyInput(ev_tab));
                }
            }
            app_window::CustomEvent::PtyExit(ev_tab, code) => {
                let Some(&wid) = self.tab_to_window.get(&ev_tab) else { return };
                let close = match self.windows.get(&wid).map(|s| s.config.shell_exit_mode) {
                    Some(ShellExitMode::Always) => true,
                    Some(ShellExitMode::Never) => false,
                    Some(ShellExitMode::OnSuccess) => code == 0,
                    None => return,
                };
                if close {
                    // Drop this window and free every tab it owned from the
                    // resolver. Quit once the last window is gone. (With one
                    // window this is the old `exit()`; the registry
                    // generalizes it.)
                    if let Some(state) = self.windows.remove(&wid) {
                        for t in &state.tabs {
                            close_tab_pty(t);
                            self.tab_to_window.remove(&t.tab_id);
                        }
                    }
                    if self.windows.is_empty() {
                        event_loop.exit();
                    }
                } else if let Some(state) = self.windows.get_mut(&wid) {
                    // Keep the window so the user can read the final
                    // output / a crash's exit code, then dismiss it
                    // themselves. The PTY master is closed, so typed
                    // input now goes nowhere — selection and scrollback
                    // still work. Render a dim status line via the normal
                    // ANSI path.
                    state.feed_terminal(&format!(
                        "\r\n\x1b[2m[Process completed — exit {code}]\x1b[0m\r\n"
                    ));
                    state.invalidate();
                    state.window.request_redraw();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // A new-window request (cwd, origin) raised by Cmd-N / palette
        // during `input()`, and whether this window asked to close —
        // both acted on after the `state` borrow is released, since
        // they mutate the window registry.
        // (cwd, origin, config, tabbing_id, is_tab, titlebar_px)
        let mut spawn_req: Option<(
            Option<String>,
            Option<(f64, f64)>,
            Config,
            String,
            bool,
            f64,
        )> = None;
        let mut close_this = false;
        if let Some(state) = self.windows.get_mut(&window_id) {
            let consumed = state.input(&event, event_loop);
            // The palette's Set/Clear title actions set the title
            // through the same terminal path OSC 0/2 uses, but a
            // keystroke isn't followed by PtyInput, so poll the title
            // update here too. Mirrors the PtyInput arm above.
            if let Some(t) = state.active_tab_mut().terminal.take_title_update() {
                state.manual_title = t;
                state.window.set_title(&effective_title(
                    state.manual_title.as_deref(),
                    state.active_tab().terminal.cwd(),
                ));
            }
            if !consumed {
                match event {
                    WindowEvent::ThemeChanged(new_theme) => {
                        state.theme = new_theme;
                        // Following the system appearance? Swap to the
                        // scheme slot for the new mode. Otherwise just
                        // keep the OSC color reports in sync as before —
                        // the active scheme doesn't track the OS.
                        if state.config.auto_theme {
                            state.apply_active_scheme();
                        } else {
                            state.sync_theme_colors();
                            state.invalidate();
                        }
                    }
                    WindowEvent::CloseRequested => {
                        close_this = true;
                    }
                    WindowEvent::Focused(focused) => {
                        state.focused = focused;
                        // Snap the cursor to its solid phase on either
                        // transition: gaining focus shouldn't catch the
                        // cursor mid-blink-off, and losing focus parks it
                        // steady (blinking is now disabled). Repaint so
                        // the cursor style change shows immediately.
                        state.reset_blink();
                        if focused {
                            // Track the key window so the native tab
                            // bar's `+` button (no window context) opens
                            // a tab in the right group.
                            self.focused_window = Some(window_id);
                            // A focused window is on screen by
                            // definition. macOS coalesces/delays the
                            // matching `Occluded(false)`, so clear the
                            // flag now — otherwise the redraw requested
                            // below hits the `RedrawRequested if
                            // state.occluded` guard and gets dropped,
                            // leaving a just-selected tab blank until
                            // occlusion finally catches up (up to ~1s).
                            state.occluded = false;
                        }
                        // Reflow on *either* transition: the tab bar's
                        // presence (and thus the usable height) changes
                        // as tabs come and go, and the window that
                        // spawned a new tab sees only a focus *loss* —
                        // without this its prompt stays stranded behind
                        // the freshly-shown bar.
                        state.reflow_for_tab_bar();
                        // The native autocomplete popup is an independent
                        // floating panel; hide it on focus loss (a render may
                        // not follow to do it) so it doesn't sit over other apps.
                        if !focused {
                            state.update_completion_popup();
                        }
                        state.invalidate();
                    }
                    WindowEvent::Occluded(occluded) => {
                        // A fully hidden window neither animates nor
                        // redraws (see the AboutToWait tick). When it
                        // comes back into view, repaint once to catch up
                        // on anything that changed while it was dark —
                        // refreshing the chrome band too, since a tab
                        // selection can change the bar's height.
                        state.occluded = occluded;
                        if !occluded {
                            state.reflow_for_tab_bar();
                            state.invalidate();
                        }
                    }
                    WindowEvent::Resized(size) => {
                        state.resize(size);
                        state.window.request_redraw();
                    }
                    WindowEvent::ScaleFactorChanged {
                        scale_factor: _scale_factor,
                        ..
                    } => {
                        state.refresh_chrome_band();
                        state.window.request_redraw();
                    }
                    // A background native tab still receives PTY output,
                    // which invalidates it — but nothing is on screen, so
                    // skip the GPU work. Reveal (Occluded false) repaints.
                    WindowEvent::RedrawRequested if state.hidden() => {
                        // Background native tab — nothing is on screen,
                        // so skip the GPU work. The reveal (Occluded
                        // false, or regaining focus) requests a fresh
                        // frame. `hidden()` ignores a stale `occluded`
                        // while focused so a rapid tab switch back here
                        // still paints.
                    }
                    WindowEvent::RedrawRequested => {
                        // Two cheap gates before the ~7ms vertex rebuild, so a
                        // frame we shouldn't render yet costs the main thread
                        // nothing (leaving the run loop free for AppKit's native
                        // tab bar). `about_to_wait` re-arms the redraw when due.
                        //   1. Frame pacing: don't render faster than
                        //      MIN_FRAME_INTERVAL — the present thread vsync-paces
                        //      the display anyway, and rendering flat-out starves
                        //      AppKit during a flood of output.
                        //   2. Backpressure: if every present target is still in
                        //      flight, the frame would just be dropped.
                        let paced = state.last_render_at.elapsed() >= MIN_FRAME_INTERVAL;
                        if !paced || !state.present_target_available() {
                            state.render_pending = true;
                            return;
                        }
                        state.render_pending = false;
                        state.last_render_at = std::time::Instant::now();
                        state.update();
                        state.prepare_frame();
                        let t0 = std::time::Instant::now();
                        let result = state.render(clear_color(state.theme));
                        let render_dur = t0.elapsed();
                        // Sync the native autocomplete popup to the cursor
                        // anchor captured during the render just completed.
                        state.update_completion_popup();
                        if !self.first_frame_done {
                            self.first_frame_done = true;
                            if self.timing {
                                eprintln!("[startup] {:>7.1}ms  FIRST FRAME presented", self.t_start.elapsed().as_secs_f64() * 1000.0);
                            }
                        }
                        match result {
                            Ok((surface_wait, fast)) => {
                                state.perf.note_render(render_dur, surface_wait, fast);
                            }
                            Err(wgpu::SurfaceError::Lost) => state.resize(state.surface.size),
                            Err(wgpu::SurfaceError::OutOfMemory) => event_loop.exit(),
                            Err(e) => eprintln!("{:?}", e),
                        }
                    }
                    _ => (),
                }
            }
            // Drain a new-window request raised during `input()`. Clone
            // the spawning window's *live* config so the new window
            // inherits whatever the user currently has — palette toggles
            // (autocomplete, theme, zoom) mutate the per-window config and
            // never the startup snapshot, so passing that snapshot here
            // would resurrect stale settings in every new window.
            if std::mem::take(&mut state.pending_new_window) {
                // Fresh group id → standalone window.
                spawn_req = Some((
                    state.active_tab().terminal.cwd().map(str::to_owned),
                    state.window_origin(),
                    state.config.clone(),
                    next_tab_group_id(),
                    false,
                    state.titlebar_only_px,
                ));
            } else if std::mem::take(&mut state.pending_new_tab) {
                // Reuse this window's group id so the
                // new window joins it as a native tab. AppKit positions
                // tabs itself, so pass no cascade origin.
                use winit::platform::macos::WindowExtMacOS;
                spawn_req = Some((
                    state.active_tab().terminal.cwd().map(str::to_owned),
                    None,
                    state.config.clone(),
                    state.window.tabbing_identifier(),
                    true,
                    state.titlebar_only_px,
                ));
            }
            // Drain a Cmd-W close request. The confirmation (if the
            // shell had a running command) already ran in `input()`,
            // so reaching here means "close now" — fall into the same
            // teardown the OS close button uses below.
            if std::mem::take(&mut state.pending_close) {
                close_this = true;
            }
        }
        if let Some((cwd, origin, cfg, tabbing_id, is_tab, titlebar_px)) = spawn_req {
            spawn_window_in_process(
                event_loop,
                self.shared.as_ref().unwrap(),
                &self.proxy,
                &mut self.windows,
                &mut self.tab_to_window,
                &cfg,
                &self.zdotdir,
                cwd,
                origin,
                &tabbing_id,
                // Only a tab is born into an existing (visible) bar and
                // needs the spawner's bar-free baseline; a standalone
                // window measures its own once it comes up bar-free.
                is_tab.then_some(titlebar_px),
            );
        }
        if close_this {
            // Tear down this window: kill + reap each tab's shell and
            // free its TabId, then drop the window. Quit when the last
            // window is gone.
            if let Some(state) = self.windows.remove(&window_id) {
                for t in &state.tabs {
                    close_tab_pty(t);
                    self.tab_to_window.remove(&t.tab_id);
                }
            }
            if self.windows.is_empty() {
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drain a `+`-button click (no window context, so
        // it targets the key window's group — same as Cmd-T there).
        if NEW_TAB_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let req = self.focused_window.and_then(|wid| self.windows.get(&wid)).map(|s| {
                use winit::platform::macos::WindowExtMacOS;
                (
                    s.active_tab().terminal.cwd().map(str::to_owned),
                    s.config.clone(),
                    s.window.tabbing_identifier(),
                    s.titlebar_only_px,
                )
            });
            if let Some((cwd, cfg, tabbing_id, titlebar_px)) = req {
                spawn_window_in_process(
                    event_loop,
                    self.shared.as_ref().unwrap(),
                    &self.proxy,
                    &mut self.windows,
                    &mut self.tab_to_window,
                    &cfg,
                    &self.zdotdir,
                    cwd,
                    None,
                    &tabbing_id,
                    // The `+` button always opens a tab in the key
                    // window's group, so it inherits that window's bar.
                    Some(titlebar_px),
                );
            }
        }
        // Drain a native command-palette signal (accept / dismiss). Like the
        // `+` button it carries no window context, so it targets the focused
        // window — the palette is always a child of (and key over) that window.
        #[cfg(target_os = "macos")]
        if let Some(sig) = glass_palette::take_palette_signal() {
            if let Some(state) = self.focused_window.and_then(|w| self.windows.get_mut(&w)) {
                match sig {
                    glass_palette::PaletteSignal::Accept(s) => state.palette_accept(s),
                    glass_palette::PaletteSignal::Dismiss => state.palette_dismiss(),
                    glass_palette::PaletteSignal::Close => state.close_glass_palette(),
                }
            }
        }

        // Drain a native find-bar signal (query / next / prev / close), like the
        // palette, routed to the focused window.
        #[cfg(target_os = "macos")]
        if let Some(sig) = glass_find::take_find_signal() {
            if let Some(state) = self.focused_window.and_then(|w| self.windows.get_mut(&w)) {
                match sig {
                    glass_find::FindSignal::Query(q) => state.find_query(q),
                    glass_find::FindSignal::Next => state.find_step(true),
                    glass_find::FindSignal::Prev => state.find_step(false),
                    glass_find::FindSignal::Close => state.find_close(),
                }
            }
        }

        // Each window animates independently; collect the earliest
        // wake-up across all of them and arm the loop for that.
        let mut next_wake: Option<std::time::Instant> = None;
        for state in self.windows.values_mut() {
            // A fully occluded window can't be seen, so don't spend the
            // shared thread animating or redrawing it — and don't let it
            // pull the loop's wake-up earlier. PTY output still feeds its
            // terminal (it just defers the repaint until Occluded(false)
            // invalidates it). perf still flushes so its burst closes.
            if state.hidden() {
                state.perf.maybe_flush();
                continue;
            }
            // Re-arm a deferred redraw once it's due: the pacing interval has
            // elapsed *and* a present target is free. Requesting the redraw only
            // when both hold means the loop sleeps (idle, AppKit-available) until
            // then instead of spinning on a frame it can't draw.
            if state.render_pending
                && state.last_render_at.elapsed() >= MIN_FRAME_INTERVAL
                && state.present_target_available()
            {
                state.render_pending = false;
                state.window.request_redraw();
            }
            if state.maybe_blink_tick() {
                state.invalidate();
            }
            // Edge-fade and cursor-position eases: keep ticking frames
            // as long as anything is still chasing its target.
            let animating = state.is_top_fade_animating()
                || state.is_cursor_animating()
                || state.is_alt_scroll_animating()
                || state.is_primary_scroll_animating();
            if animating {
                // The cursor quad and the alt-screen slide's per-row
                // offsets live in the cell geometry, so they need a full
                // rebuild. The global scroll slide (primary) and the
                // edge fades only move the camera / fade uniforms, which
                // `refresh_scroll_uniforms` handles on the cheap
                // scroll-only path — so prefer that when no
                // geometry-bound animation is in flight.
                if state.is_cursor_animating() || state.is_alt_scroll_animating() {
                    state.invalidate();
                } else {
                    state.invalidate_scroll();
                }
            }
            state.perf.maybe_flush();
            let next_anim = if animating {
                Some(std::time::Instant::now() + ANIM_FRAME)
            } else {
                None
            };
            // Image-animation deadline (Kitty `a=a` playback). The
            // store returns `None` if no animated image is
            // currently advancing; otherwise it returns the
            // earliest moment a frame swap is due.
            let next_image_anim = state
                .active_tab()
                .image_store
                .next_frame_deadline(std::time::Instant::now());
            if next_image_anim.is_some() {
                state.invalidate();
            }
            // A deferred frame needs a wake to actually land: at the pacing
            // deadline if we're still inside MIN_FRAME_INTERVAL, else a short
            // poll while we wait for the present pool to free a target (the
            // present thread doesn't wake the loop itself).
            let next_deferred = if state.render_pending {
                let since = state.last_render_at.elapsed();
                Some(match MIN_FRAME_INTERVAL.checked_sub(since) {
                    Some(remaining) if !remaining.is_zero() => {
                        std::time::Instant::now() + remaining
                    }
                    _ => std::time::Instant::now() + std::time::Duration::from_millis(2),
                })
            } else {
                None
            };
            let this = [
                state.next_blink_wake(),
                next_anim,
                next_image_anim,
                next_deferred,
                state.perf.next_wake(),
            ]
            .into_iter()
            .flatten()
            .min();
            next_wake = match (next_wake, this) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        match next_wake {
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}
