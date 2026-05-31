//! Shell-integration handlers for [`Terminal`].
//!
//! Carved out of the main `terminal.rs` `impl Terminal` block: the OSC
//! handlers that implement yutani's shell integration — OSC 7 (cwd), OSC 133
//! (FinalTerm semantic prompt marks), and the yutani-private OSC 2122/2124/2125
//! (current input line, history-file path, live preview) — together with the
//! prompt/command-region navigation built on the marks they record
//! (`scroll_to_*_prompt`, `command_regions`, `prompt_status_markers`,
//! `last_command_output_span`, …).
//!
//! The `handle_osc` dispatcher, OSC 0/2 window-title handling, OSC 8
//! hyperlinks, the `reply`/`cluster_str` helpers, and the semantic-mark
//! storage all stay in the parent module; the five private OSC handlers the
//! dispatcher calls are `pub(super)`. Shared types and free functions reach
//! here via `use super::*`.

use super::*;

impl Terminal {
    /// `OSC 7 ; file://<host>/<path> ST` — the shell reports its current
    /// working directory. Emitted from the prompt hook (`precmd` in zsh,
    /// `PROMPT_COMMAND` in bash) on every prompt. The host name is advisory
    /// (we ignore it; remote dirs over ssh aren't reachable locally anyway)
    /// and the path is percent-encoded.
    ///
    /// We accept the canonical `file://host/path` form and also a bare
    /// absolute path (`/some/dir`) some minimal integrations emit. Anything
    /// else — relative paths, unknown schemes, empty payloads — is dropped.
    /// An unchanged value doesn't mark the cwd dirty, so re-emitting the
    /// same directory on every prompt is free for the front end.
    pub(super) fn handle_osc_7(&mut self, payload: &str) {
        let path = if let Some(after_scheme) = payload.strip_prefix("file://") {
            // Strip the authority (host) component: everything up to the
            // first '/'. `file:///path` (empty host) and `file://host/path`
            // both leave `after_scheme` pointing at the leading '/'.
            match after_scheme.find('/') {
                Some(slash) => &after_scheme[slash..],
                None => return, // host with no path — nothing usable
            }
        } else if payload.starts_with('/') {
            payload
        } else {
            return;
        };

        let decoded = percent_decode_path(path);
        if decoded.is_empty() {
            return;
        }
        if self.cwd.as_deref() != Some(decoded.as_str()) {
            self.cwd = Some(decoded);
            self.cwd_dirty = true;
        }
    }

    /// Returns the working directory once if it changed since the last call,
    /// clearing the dirty flag. The front end calls this after each `feed`
    /// to retitle the window / seed a new tab without diffing strings itself.
    pub fn take_cwd_update(&mut self) -> Option<String> {
        if self.cwd_dirty {
            self.cwd_dirty = false;
            self.cwd.clone()
        } else {
            None
        }
    }

    /// The shell's reported working directory (OSC 7), if known. Borrowed for
    /// read access (e.g. seeding the path completer); `take_cwd_update` remains
    /// the change-notification path.
    #[allow(dead_code)]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// The shell's current interactive input line, if the shell is reporting
    /// one via `OSC 2122`. `None` when no edit line is active (no shell
    /// integration, a command is running, or the alternate screen is up).
    /// Borrowed because the autocomplete layer reads it every frame.
    // Consumed by a later autocomplete slice; tests exercise it now.
    #[allow(dead_code)]
    pub fn current_input(&self) -> Option<&CurrentInput> {
        self.current_input.as_ref()
    }

    /// `OSC 133 ; <kind> [; params...] ST` — FinalTerm semantic prompt
    /// marks. We record the four standard kinds (`A`/`B`/`C`/`D`) anchored
    /// to the cursor position. Trailing `key=value` params (e.g. `A;aid=7`,
    /// the exit code's siblings on `D`) are tolerated and ignored — only the
    /// leading kind token and `D`'s first numeric field are read.
    ///
    /// Marks only make sense on the primary screen — full-screen apps that
    /// take the alternate screen (vim, less) don't run prompts — so anything
    /// emitted while the alt screen is active is dropped.
    pub(super) fn handle_osc_133(&mut self, payload: &str) {
        if self.use_alternate {
            return;
        }
        let mut fields = payload.split(';');
        let kind = match fields.next() {
            Some("A") => SemanticMarkKind::PromptStart,
            Some("B") => SemanticMarkKind::InputStart,
            Some("C") => {
                // Command submitted: capture the live edit buffer (if any,
                // non-blank) so the front end can fold it into the in-session
                // command history for completion. Then clear it — the edit line
                // is gone, so the OSC 2122 report no longer describes anything.
                if let Some(ci) = &self.current_input {
                    if !ci.buffer.trim().is_empty() {
                        self.last_submitted_command = Some(ci.buffer.clone());
                    }
                }
                self.current_input = None;
                SemanticMarkKind::OutputStart
            }
            Some("D") => {
                // The first field after `D` is the exit code when present and
                // numeric. A `key=value` field (or non-numeric junk) means
                // "no exit code reported".
                let exit = fields
                    .next()
                    .filter(|f| !f.contains('='))
                    .and_then(|f| f.parse::<i32>().ok());
                SemanticMarkKind::CommandEnd { exit }
            }
            _ => return,
        };
        self.semantic_marks.push(SemanticMark {
            anchor: MarkAnchor::Live {
                row: self.cursor.row,
            },
            col: self.cursor.col,
            kind,
        });
    }

    /// `OSC 2122 ; <cursor> ; <base64(buffer)> ST` — the yutani-private
    /// current-input report. The shell's line-editor hook emits this on every
    /// edit/cursor move so the terminal can track the live edit buffer without
    /// reconstructing it from the grid; it is the data foundation autocomplete
    /// builds on. A no-op in other terminals (private-use OSC number).
    ///
    /// `<cursor>` is a decimal *character* (code-point) offset into the buffer
    /// (zsh `$CURSOR`). The buffer is STANDARD base64 of its UTF-8 bytes so it
    /// can contain `;`, control chars, and other OSC-hostile bytes safely; an
    /// empty buffer encodes to the empty string, so `OSC 2122 ; 0 ; ST` is a
    /// valid "empty active line" report and yields `Some` with an empty buffer.
    ///
    /// Like OSC 133, line editing only happens on the primary screen, so a
    /// report received while the alternate screen is active is dropped. Any
    /// malformed payload (missing `;`, non-numeric cursor, invalid base64 or
    /// UTF-8) is ignored, leaving the previous state untouched.
    pub(super) fn handle_osc_2122(&mut self, payload: &str) {
        if self.use_alternate {
            return;
        }
        let Some((cursor_str, b64)) = payload.split_once(';') else {
            return;
        };
        let Ok(cursor) = cursor_str.parse::<usize>() else {
            return;
        };
        use base64::Engine;
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) else {
            return;
        };
        let Ok(buffer) = String::from_utf8(bytes) else {
            return;
        };
        // `$CURSOR` is a character offset; clamp defensively so consumers can
        // index by char without bounds checks (e.g. an off-by-one past EOL).
        let cursor = cursor.min(buffer.chars().count());
        self.current_input = Some(CurrentInput { buffer, cursor });
    }

    /// `OSC 2124 ; <base64(path)> \a` — yutani-private: the shell reports its
    /// history file ($HISTFILE) so the terminal can read past commands for the
    /// autocomplete popup. Stored once; the front end reads + parses the file.
    /// Malformed payloads (invalid base64 / UTF-8) are ignored.
    pub(super) fn handle_osc_2124(&mut self, payload: &str) {
        use base64::Engine;
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload.as_bytes()) else {
            return;
        };
        let Ok(path) = String::from_utf8(bytes) else {
            return;
        };
        if path.is_empty() {
            return;
        }
        if self.histfile.as_deref() != Some(path.as_str()) {
            self.histfile = Some(path);
            self.histfile_dirty = true;
        }
    }

    /// Returns the history-file path once if it changed since the last call,
    /// clearing the dirty flag. The front end calls this after each `feed` to
    /// read + parse the file for history-based completion suggestions.
    pub fn take_histfile_update(&mut self) -> Option<String> {
        if self.histfile_dirty {
            self.histfile_dirty = false;
            self.histfile.clone()
        } else {
            None
        }
    }

    /// `OSC 2125 ; <verb> [; <arg>] \a` — yutani-private live-preview control,
    /// emitted by the first-run onboarding (which runs as the PTY child) so the
    /// user sees their choice on the real window before it's saved. Recognized
    /// verbs:
    ///
    /// - `scheme;<name>` — preview a color scheme by name; empty / `-` /
    ///   `default` reverts to the built-in palette.
    /// - `glow;off|subtle|full` — preview a CRT-glow preset.
    /// - `scanlines;on|off` — toggle the scanline overlay.
    /// - `crt;off|low|high` — preview a combined CRT effect (bloom + scanlines).
    /// - `font;<points>` — preview an absolute font size (rebuilds the grid).
    /// - `reload` — re-read config + scheme from disk (the final commit step,
    ///   after the onboarding has written the chosen settings).
    ///
    /// Unknown verbs / args are dropped, matching the terminal's general
    /// "unknown OSC is a no-op" contract. Parsed requests are queued for the
    /// front end to apply via [`take_preview_requests`](Self::take_preview_requests).
    pub(super) fn handle_osc_2125(&mut self, payload: &str) {
        let (verb, arg) = payload.split_once(';').unwrap_or((payload, ""));
        let req = match verb {
            "scheme" => {
                let name = match arg {
                    "" | "-" | "default" => None,
                    other => Some(other.to_string()),
                };
                PreviewRequest::Scheme(name)
            }
            "glow" => match crate::GlowLevel::from_str(arg) {
                Some(level) => PreviewRequest::Glow(level),
                None => return,
            },
            "scanlines" => match arg {
                "on" => PreviewRequest::Scanlines(true),
                "off" => PreviewRequest::Scanlines(false),
                _ => return,
            },
            "crt" => match crate::CrtLevel::from_str(arg) {
                Some(level) => PreviewRequest::Crt(level),
                None => return,
            },
            "font" => match arg.parse::<f32>() {
                Ok(pt) if pt.is_finite() => PreviewRequest::FontSize(pt),
                _ => return,
            },
            "reload" => PreviewRequest::Reload,
            _ => return,
        };
        self.preview_requests.push(req);
    }

    /// Drain any live-preview requests queued since the last call. The front end
    /// calls this after each `feed` and applies each to the running renderer.
    pub fn take_preview_requests(&mut self) -> Vec<PreviewRequest> {
        std::mem::take(&mut self.preview_requests)
    }

    /// Returns the last command submitted at a prompt (the OSC 2122 buffer that
    /// was live when OSC 133 `C` fired), once, clearing it. The front end folds
    /// it into the in-session command history for completion.
    pub fn take_submitted_command(&mut self) -> Option<String> {
        self.last_submitted_command.take()
    }

    /// Fold the recorded semantic marks into per-command regions, in
    /// emission order. Line fields are absolute line indices (the
    /// [`line_at`](Self::line_at) convention), computed from the current
    /// scrollback length so they track the viewport.
    ///
    /// A `PromptStart` opens a region; `InputStart` / `OutputStart` fill it;
    /// `CommandEnd` closes it. Missing marks are tolerated — an interrupted
    /// command leaves `command_end: None`, and a region needs only its
    /// opening `PromptStart` to be emitted. Consumed by the prompt-navigation
    /// keybinding (`scroll_to_prev_prompt` / `scroll_to_next_prompt`).
    pub fn command_regions(&self) -> Vec<CommandRegion> {
        // Live marks anchor below all scrollback rows; scrollback marks are
        // already absolute scrollback row indices.
        let base = self.scrollback.len() as isize;
        let abs = |m: &SemanticMark| match m.anchor {
            MarkAnchor::Live { row } => base + row as isize,
            MarkAnchor::Scrollback { row } => row,
        };

        let mut regions: Vec<CommandRegion> = Vec::new();
        let mut current: Option<CommandRegion> = None;
        for m in &self.semantic_marks {
            match m.kind {
                SemanticMarkKind::PromptStart => {
                    if let Some(r) = current.take() {
                        regions.push(r);
                    }
                    current = Some(CommandRegion {
                        prompt_start: abs(m),
                        input_start: None,
                        output_start: None,
                        command_end: None,
                        exit_code: None,
                    });
                }
                SemanticMarkKind::InputStart => {
                    if let Some(r) = current.as_mut() {
                        r.input_start = Some(abs(m));
                    }
                }
                SemanticMarkKind::OutputStart => {
                    if let Some(r) = current.as_mut() {
                        r.output_start = Some(abs(m));
                    }
                }
                SemanticMarkKind::CommandEnd { exit } => {
                    if let Some(mut r) = current.take() {
                        r.command_end = Some(abs(m));
                        r.exit_code = exit;
                        regions.push(r);
                    }
                }
            }
        }
        if let Some(r) = current.take() {
            regions.push(r);
        }
        regions
    }

    /// Scroll the viewport so the nearest prompt *above* the current top
    /// lands at the top of the viewport. Returns false (no-op) when there is
    /// no such prompt or on the alt screen. Drives the prompt-navigation
    /// keybinding (jump to previous prompt).
    pub fn scroll_to_prev_prompt(&mut self) -> bool {
        if self.use_alternate {
            return false;
        }
        let top = self.visual_to_abs_line(0);
        let target = self
            .command_regions()
            .into_iter()
            .map(|r| r.prompt_start)
            .filter(|&p| p < top)
            .max();
        match target {
            Some(p) => self.scroll_to_abs_top(p),
            None => false,
        }
    }

    /// Scroll the viewport so the nearest prompt *below* the current top
    /// lands at the top of the viewport. Returns false (no-op) when there is
    /// no such prompt or on the alt screen. Drives the prompt-navigation
    /// keybinding (jump to next prompt).
    pub fn scroll_to_next_prompt(&mut self) -> bool {
        if self.use_alternate {
            return false;
        }
        let top = self.visual_to_abs_line(0);
        let target = self
            .command_regions()
            .into_iter()
            .map(|r| r.prompt_start)
            .filter(|&p| p > top)
            .min();
        match target {
            Some(p) => self.scroll_to_abs_top(p),
            None => false,
        }
    }

    /// Set `view_offset` so absolute line `abs` sits at the top of the
    /// viewport, clamped into the scrollable range (a prompt already on the
    /// live grid clamps to the bottom, `view_offset == 0`). Returns whether
    /// the offset changed.
    fn scroll_to_abs_top(&mut self, abs: isize) -> bool {
        let sb = self.scrollback.len() as isize;
        let new = (sb - abs).clamp(0, sb) as usize;
        if new == self.view_offset {
            return false;
        }
        self.view_offset = new;
        true
    }

    /// Scroll so absolute line `abs` is comfortably visible, placing it about a
    /// third of the way down the viewport for context. No-op on the alt screen
    /// (no scrollback) or when `abs` is already within the visible band.
    /// Returns whether the view actually moved. Drives find-in-scrollback's
    /// "jump to current match".
    pub fn scroll_line_into_view(&mut self, abs: isize) -> bool {
        if self.use_alternate {
            return false;
        }
        let top = self.visual_to_abs_line(0);
        let bottom = top + self.rows as isize - 1;
        if abs >= top && abs <= bottom {
            return false;
        }
        let margin = self.rows as isize / 3;
        self.scroll_to_abs_top(abs - margin)
    }

    /// One `(absolute_line, status)` per command region, anchored at the
    /// region's prompt-start line. The renderer maps each absolute line to a
    /// visible row (via [`visual_to_abs_line`](Self::visual_to_abs_line)) and
    /// draws a status-colored gutter bar. Empty when no marks are present.
    pub fn prompt_status_markers(&self) -> Vec<(isize, PromptStatus)> {
        self.command_regions()
            .into_iter()
            .map(|r| {
                let status = match r.exit_code {
                    Some(0) => PromptStatus::Success,
                    Some(_) => PromptStatus::Failure,
                    None => PromptStatus::Pending,
                };
                (r.prompt_start, status)
            })
            .collect()
    }

    /// Absolute line span `[start, end]` (inclusive) of the most recent
    /// *completed* command's output: from its `OutputStart` line up to the
    /// line just above its `CommandEnd` (the `D` mark sits on the next
    /// prompt's line). Returns `None` when no completed command produced any
    /// output. Drives the select-last-command-output keybinding.
    pub fn last_command_output_span(&self) -> Option<(isize, isize)> {
        self.command_regions()
            .into_iter()
            .rev()
            .find_map(|r| match (r.output_start, r.command_end) {
                (Some(out), Some(end)) if end - 1 >= out => Some((out, end - 1)),
                _ => None,
            })
    }
}
