//! The winit event loop: window + GPU bring-up, font worker, and the
//! per-event dispatch into `State`. Driven by `main()`.

use crate::*;

/// Everything `run()` needs to build the font stack, loaded as owned bytes /
/// strings so it can be produced on a worker thread (a FreeType `Face` is not
/// `Send`, but the raw font data is). The main thread turns this into FreeType
/// faces + the rustybuzz shaper after the GPU has been brought up concurrently.
pub(crate) async fn run() {
    env_logger::init();
    // Startup phase timing, printed only when YUTANI_STARTUP_TIMING is set so
    // the perf work stays reproducible without spamming every launch.
    let t_start = std::time::Instant::now();
    let timing = std::env::var_os("YUTANI_STARTUP_TIMING").is_some();
    let lap = |label: &str| {
        if timing {
            eprintln!("[startup] {:>7.1}ms  {label}", t_start.elapsed().as_secs_f64() * 1000.0);
        }
    };
    let event_loop = EventLoopBuilder::<app_window::CustomEvent>::with_user_event()
        .build()
        .unwrap();
    let event_loop_proxy = event_loop.create_proxy();

    // create the pty before forking so we have the handle available
    let fdm: i32;
    unsafe {
        fdm = posix_openpt(O_RDWR);
        println!("fdm: {fdm}");
        if fdm < 0 {
            panic!("Error on posix_openpt()");
        }
    }

    // On first run (or after a "Run first-time setup…" re-arm), the PTY child
    // becomes the onboarding console program instead of the shell; it execs the
    // shell in-place when done. Resolve our own path here, in the parent, so the
    // post-fork child doesn't have to.
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
    // `source …` in the user's rc required.
    let zdotdir = shell_integration::prepare_zdotdir();

    // Fork before the window is created so we hold the master fd across setup.
    let pty = pty::fork_pty(fdm, child_program, zdotdir).expect("failed to fork pty");
    lap("after fork_pty");
    std::thread::spawn(move || {
        let code = pty.run(|data| {
            let _ = event_loop_proxy.send_event(app_window::CustomEvent::PtyInput(data.to_owned()));
        });
        // `run` returns once the shell has exited and been reaped. Wake the
        // event loop so it can react instead of leaving a frozen window —
        // queued after every PtyInput, so any final shell output lands first.
        let _ = event_loop_proxy.send_event(app_window::CustomEvent::PtyExit(code));
    });

    // Seed the window title from our own working directory — the shell the
    // PTY just forked inherits it (no chdir in the child), so this matches
    // what the first OSC 7 will report, avoiding a bare "Yutani" flash before
    // the first prompt.
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned));
    let initial_title = effective_title(None, initial_cwd.as_deref());

    let transparent = false; // needed because of a shadow bug
    let mut window_builder = WindowBuilder::new()
        .with_title(&initial_title)
        .with_titlebar_transparent(true)
        .with_transparent(transparent)
        .with_has_shadow(!transparent)
        .with_fullsize_content_view(true)
        .with_decorations(true)
        .with_blur(transparent);
    // When spawned via Cmd-N the parent forwards its position; cascade off it
    // so the new window steps down-and-right instead of stacking exactly atop.
    if let Some(pos) = cascade_position() {
        window_builder = window_builder.with_position(pos);
    }
    let window = window_builder.build(&event_loop).unwrap();
    lap("after window build");

    // event_loop.set_control_flow(ControlFlow::Poll);

    let config = Config::load();
    lap("after config load");

    // Load all font data on a worker thread while the GPU is brought up on
    // this (main) thread. The two are independent until State::new needs both,
    // so overlapping them hides whichever finishes first. Font work must hand
    // back owned bytes (FreeType faces aren't Send); GPU/surface creation must
    // stay on the main thread (Cocoa isn't thread-safe), hence this split.
    let config_for_fonts = config.clone();
    let font_handle = std::thread::spawn(move || load_font_data(&config_for_fonts));

    let gpu = gpu::GpuContext::new(&window).await;
    lap("after GpuContext::new (concurrent with font load)");

    let fd = font_handle.join().expect("font loader thread panicked");
    lap("after font data loaded (joined)");

    // Install the color scheme before constructing State so style.rs and the
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
        if font.set_variant(variant, data, face_index, pt_size, dpi) {
            println!("primary {:?}: {} (face index {})", variant, fd.primary_name, face_index);
        }
    }

    // Pre-shape every candidate ligature sequence for each installed variant.
    for variant in font::FaceVariant::ALL {
        shaper.precompute(variant);
    }

    // Attach the fallback faces. A styled fallback only attaches when its
    // primary cut actually built (same guard as before — otherwise the chain
    // is dead weight and Atlas::lookup tumbles to Regular anyway).
    for (label, family, variant, data, face_index) in fd.fallbacks {
        if variant != font::FaceVariant::Regular
            && font.variants[variant as usize].face.is_none()
        {
            continue;
        }
        if font.add_fallback(variant, data, face_index, pt_size, dpi) {
            println!(
                "fallback {} {:?}: {} (face index {})",
                label, variant, family, face_index,
            );
        }
    }

    lap("after font faces + shaper built");
    let mut state = State::new(fdm, window, gpu, font, shaper, config, dpi).await;
    lap("after State::new (GPU/atlas/pipelines)");
    state.notify_pty_size(state.terminal.cols, state.terminal.rows);
    // Size the chrome band to the real native title bar now that the window
    // exists; the field was seeded with the renderer's reserve in State::new.
    state.refresh_chrome_band();
    state.window.set_cursor_icon(winit::window::CursorIcon::Text);
    state.sync_theme_colors();
    state
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

    let mut theme = state.window.theme().unwrap_or(winit::window::Theme::Light);

    // Program-set window title (OSC 0/2). When `Some`, it wins over the
    // cwd-derived title; cleared back to `None` by an empty OSC 0/2 payload,
    // at which point we fall back to the cwd.
    let mut manual_title: Option<String> = None;
    let mut first_frame_done = false;

    let _ = event_loop.run(move |event, elwt| {
        match event {
            Event::UserEvent(n) => match n {
                app_window::CustomEvent::PtyInput(z) => {
                    let bytes = z.len();
                    let t0 = std::time::Instant::now();
                    state.feed_terminal(&z);
                    let reply = state.terminal.take_response();
                    if !reply.is_empty() {
                        state.write_pty(&reply);
                    }
                    // Recompute the window title when either the program-set
                    // title (OSC 0/2) or the shell's cwd (OSC 7) changed. A
                    // manual title wins; otherwise we show the cwd ($HOME
                    // collapsed to `~`). Both `take_*` calls must run to clear
                    // their dirty flags even when the title doesn't change.
                    let title_changed = state.terminal.take_title_update().map(|t| {
                        manual_title = t;
                    });
                    let cwd_changed = state.terminal.take_cwd_update();
                    if title_changed.is_some() || cwd_changed.is_some() {
                        state.window.set_title(&effective_title(
                            manual_title.as_deref(),
                            state.terminal.cwd(),
                        ));
                    }
                    // The shell may have reported its history file (OSC 2124):
                    // read + parse it and merge past commands into the in-memory
                    // history (most-recent-first), behind any already-captured
                    // session commands so those stay at the front.
                    if let Some(path) = state.terminal.take_histfile_update() {
                        if let Ok(contents) = std::fs::read_to_string(&path) {
                            let parsed = completion::parse_zsh_history(&contents);
                            for cmd in parsed.iter().rev() {
                                if !state.command_history.iter().any(|c| c == cmd) {
                                    state.command_history.push(cmd.clone());
                                }
                            }
                            state.command_history.truncate(COMMAND_HISTORY_CAP);
                        }
                    }
                    // A command just submitted at the prompt (OSC 133 C): fold
                    // it into the front of the history (deduped).
                    if let Some(cmd) = state.terminal.take_submitted_command() {
                        dedup_prepend(&mut state.command_history, cmd);
                    }
                    // First-run onboarding (running as the PTY child) may have
                    // emitted OSC 2125 live-preview requests in this chunk —
                    // apply each to the running renderer.
                    for req in state.terminal.take_preview_requests() {
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
                }
                app_window::CustomEvent::PtyExit(code) => {
                    let close = match state.config.shell_exit_mode {
                        ShellExitMode::Always => true,
                        ShellExitMode::Never => false,
                        ShellExitMode::OnSuccess => code == 0,
                    };
                    if close {
                        elwt.exit();
                    } else {
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
            },
            Event::WindowEvent { window_id, event } if window_id == state.window.id() => {
                let consumed = state.input(&event, elwt);
                // The palette's Set/Clear title actions set the title through
                // the same terminal path OSC 0/2 uses, but a keystroke isn't
                // followed by PtyInput, so poll the title update here too.
                // Mirrors the OSC-driven poll in the PtyInput arm above.
                if let Some(t) = state.terminal.take_title_update() {
                    manual_title = t;
                    state.window.set_title(&effective_title(
                        manual_title.as_deref(),
                        state.terminal.cwd(),
                    ));
                }
                if !consumed {
                    match event {
                        WindowEvent::ThemeChanged(new_theme) => {
                            theme = new_theme;
                            // Following the system appearance? Swap to the
                            // scheme slot for the new mode. Otherwise just keep
                            // the OSC color reports in sync as before — the
                            // active scheme doesn't track the OS.
                            if state.config.auto_theme {
                                state.apply_active_scheme();
                            } else {
                                state.sync_theme_colors();
                                state.invalidate();
                            }
                        }
                        WindowEvent::CloseRequested => {
                            elwt.exit();
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
                        WindowEvent::RedrawRequested => {
                            state.update();
                            state.prepare_frame();
                            let t0 = std::time::Instant::now();
                            let result = state.render(clear_color(theme));
                            let render_dur = t0.elapsed();
                            if !first_frame_done {
                                first_frame_done = true;
                                if timing {
                                    eprintln!("[startup] {:>7.1}ms  FIRST FRAME presented", t_start.elapsed().as_secs_f64() * 1000.0);
                                }
                            }
                            match result {
                                Ok((surface_wait, fast)) => {
                                    state.perf.note_render(render_dur, surface_wait, fast);
                                }
                                Err(wgpu::SurfaceError::Lost) => state.resize(state.gpu.size),
                                Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                                Err(e) => eprintln!("{:?}", e),
                            }
                        }
                        _ => (),
                    }
                }
            }
            Event::AboutToWait => {
                if state.maybe_blink_tick() {
                    state.invalidate();
                }
                // Edge-fade and cursor-position eases: keep ticking frames
                // as long as either is still chasing its target.
                let animating = state.is_top_fade_animating()
                    || state.is_cursor_animating()
                    || state.is_alt_scroll_animating();
                if animating {
                    state.invalidate();
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
                    .image_store
                    .next_frame_deadline(std::time::Instant::now());
                if next_image_anim.is_some() {
                    state.invalidate();
                }
                let next_wake = [
                    state.next_blink_wake(),
                    next_anim,
                    next_image_anim,
                    state.perf.next_wake(),
                ]
                .into_iter()
                .flatten()
                .min();
                match next_wake {
                    Some(t) => elwt.set_control_flow(
                        winit::event_loop::ControlFlow::WaitUntil(t),
                    ),
                    None => elwt.set_control_flow(winit::event_loop::ControlFlow::Wait),
                }
            }
            _ => (),
        }
    });
}
