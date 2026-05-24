//! `State` methods for the shell-input completion popup: recomputing
//! suggestions, opening the menu, and accepting a selection.

use crate::*;

impl State {
    /// Refresh the cached completion-popup suggestions, but only when the
    /// shell's reported input (`OSC 2122`) actually changed since last time —
    /// `completion::complete_path` does a `read_dir`, so it must never run per
    /// frame. The popup is gated on a non-empty path token so an empty prompt
    /// doesn't dump the whole cwd. Returns true if the cache changed (so the
    /// caller can request a redraw).
    pub(crate) fn recompute_completions(&mut self) -> bool {
        let cur = self.terminal.current_input().cloned();
        let key = cur.as_ref().map(|c| (c.buffer.clone(), c.cursor));
        if key == self.completions_input {
            return false;
        }
        self.completions_input = key;
        // A dismissed popup (Enter/Esc) must not reopen when the shell re-emits
        // OSC 2122 after an accepted suffix — keep it empty until a real
        // keystroke clears the flag. The `autocomplete` config gate works the
        // same way: when the feature is off, never run `completion::complete`,
        // just keep the cache empty (read live so config reload toggles it).
        self.completions = if self.completion_dismissed || !self.config.autocomplete {
            Vec::new()
        } else {
            match &cur {
                // `complete` returns empty for empty/whitespace input on its own
                // (no history match, empty token skipped), so no separate gate.
                Some(c) => {
                    let cwd = self.terminal.cwd().map(std::path::Path::new);
                    let path = std::env::var_os("PATH");
                    let home = std::env::var_os("HOME");
                    completion::complete(
                        &c.buffer,
                        c.cursor,
                        cwd,
                        path.as_deref(),
                        home.as_deref().map(std::path::Path::new),
                        &self.command_history,
                        false,
                    )
                }
                None => Vec::new(),
            }
        };
        // The cache was replaced: restart selection at the top and reset the
        // scroll window so navigation state never points past a shorter list.
        self.selected_completion = 0;
        self.completion_scroll = 0;
        true
    }

    /// Manually (re)open the completion popup for the current input — the
    /// Ctrl+Space command. Unlike the automatic path, this bypasses the
    /// `has_path_token` gate, so triggering on an empty token lists the whole
    /// working directory ("show me what's here"). Clears the dismissed flag so a
    /// popup closed with Esc/Enter comes back, and pins `completions_input` to the
    /// current input so the next (unchanged-input) recompute doesn't immediately
    /// wipe the freshly-summoned list.
    pub(crate) fn trigger_completion(&mut self) {
        // Honor the `autocomplete` config gate: no manual summon when the
        // feature is off. Read live so a config reload toggles it.
        if !self.config.autocomplete {
            return;
        }
        self.completion_dismissed = false;
        // Clone the buffer/cursor out of the immutable `current_input()` borrow so
        // it ends before we take `cwd()` and then mutably write `self.*` fields.
        if let Some((buffer, cursor)) = self
            .terminal
            .current_input()
            .map(|c| (c.buffer.clone(), c.cursor))
        {
            let cwd = self.terminal.cwd().map(std::path::Path::new);
            let path = std::env::var_os("PATH");
            let home = std::env::var_os("HOME");
            self.completions = completion::complete(
                &buffer,
                cursor,
                cwd,
                path.as_deref(),
                home.as_deref().map(std::path::Path::new),
                &self.command_history,
                true,
            );
            self.completions_input = Some((buffer, cursor));
            self.selected_completion = 0;
            self.completion_scroll = 0;
        }
        self.invalidate();
    }

    /// Accept the highlighted suggestion: write the bytes that extend the typed
    /// token into the chosen path, straight to the PTY (the shell's line editor
    /// inserts them at the cursor). Computes the suffix against the LIVE buffer
    /// (not the cached one) and bails if they no longer agree, so a stale cache
    /// can't inject wrong bytes.
    ///
    /// `keep_open` distinguishes drilling into a directory (Tab on a dir) from
    /// finishing (Enter, or Tab on a file): when set, the popup is left alone so
    /// the shell's round-trip (echoed suffix → new `$BUFFER` → OSC 2122) can
    /// refilter the list to the subdirectory's contents; otherwise the popup is
    /// closed and kept closed via `completion_dismissed`.
    ///
    /// `submit` runs the command in the same keystroke (Shift+Enter): the
    /// accept-suffix (if any) is written followed by a carriage return, so the
    /// line executes even when the suggestion didn't extend it (the buffer
    /// submits as-typed). `submit` implies not-keep-open and always closes the
    /// popup (the command is running, so the popup must go).
    pub(crate) fn accept_selected_completion(&mut self, keep_open: bool, submit: bool) {
        let Some(sug) = self.completions.get(self.selected_completion) else {
            return;
        };
        // Clone the suggestion so the byte payload below can hold the immutable
        // `self.terminal` borrow without also borrowing `self.completions`.
        let sug = sug.clone();
        // Build the byte payload (cloning the suffix) while holding the
        // immutable `self.terminal` borrow, then drop it before the &self
        // `write_pty` call below.
        let bytes: Option<Vec<u8>> = self.terminal.current_input().map(|c| {
            let mut bytes = match completion::accept_suffix(&c.buffer, c.cursor, &sug) {
                Some(suffix) => suffix.as_bytes().to_vec(),
                // A stale / non-extending suggestion: no suffix to insert, but
                // we may still submit the line as-typed below.
                None => Vec::new(),
            };
            if submit {
                bytes.push(b'\r');
            }
            bytes
        });
        if let Some(bytes) = bytes {
            if !bytes.is_empty() {
                self.write_pty(&bytes);
            }
        }
        if submit {
            // Submitting: the command is running, so the popup must close and
            // stay closed until the user types again.
            self.completions.clear();
            self.completions_input = None;
            self.completion_dismissed = true;
        } else if keep_open {
            // Drilling into a directory: force a fresh recompute on the next
            // OSC 2122 report, but leave the popup open and undismissed so the
            // round-trip refilters to the subdirectory's contents.
            self.completions_input = None;
        } else {
            // Finishing: close and keep closed until the user types.
            self.completions.clear();
            self.completions_input = None;
            self.completion_dismissed = true;
        }
        self.selected_completion = 0;
        self.completion_scroll = 0;
        self.invalidate();
    }
}
