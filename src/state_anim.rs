//! `State` animation/timing methods: cursor blink phase and the alt-screen
//! smooth-scroll slide.

use crate::*;

impl State {
    /// Effective blink state: DECSCUSR's request is gated by the user's
    /// `cursor_blink` config so opting out disables blinking globally.
    pub(crate) fn cursor_blink_enabled(&self) -> bool {
        self.config.cursor_blink && self.terminal.cursor_blink()
    }

    /// Combined visibility check: DECTCEM (cursor_visible) gates whether the
    /// cursor exists at all; blink only suppresses it on the "off" half-phase
    /// of the cycle when DECSCUSR has selected a blinking variant.
    pub(crate) fn cursor_currently_visible(&self) -> bool {
        self.terminal.cursor_visible() && (!self.cursor_blink_enabled() || self.blink_on)
    }

    /// If a blink half-cycle has elapsed, flip the phase and request a redraw.
    /// Returns true when the cursor visibility actually changed.
    pub(crate) fn maybe_blink_tick(&mut self) -> bool {
        if !self.cursor_blink_enabled() || !self.terminal.cursor_visible() {
            return false;
        }
        if self.last_blink.elapsed() < BLINK_INTERVAL {
            return false;
        }
        self.blink_on = !self.blink_on;
        self.last_blink = std::time::Instant::now();
        true
    }

    /// Next instant the event loop should wake to flip the blink phase, or
    /// `None` if the cursor isn't blinking right now.
    pub(crate) fn next_blink_wake(&self) -> Option<std::time::Instant> {
        if self.cursor_blink_enabled() && self.terminal.cursor_visible() {
            Some(self.last_blink + BLINK_INTERVAL)
        } else {
            None
        }
    }

    /// Snap the cursor to its visible phase and reset the blink timer.
    /// Called on user input so the cursor doesn't wink off mid-keystroke.
    pub(crate) fn reset_blink(&mut self) {
        self.blink_on = true;
        self.last_blink = std::time::Instant::now();
    }

    /// True while the cursor quad is mid-ease, or any ghost glyphs are
    /// still fading. Keeps the event loop ticking until both finish so
    /// the redraw isn't held up waiting for the next PTY/blink event.
    pub(crate) fn is_cursor_animating(&self) -> bool {
        let anim_active = match &self.cursor_anim {
            Some(a) => a.animating(self.config.cursor_anim_secs),
            None => false,
        };
        anim_active || !self.cursor_ghosts.is_empty()
    }

    /// If the just-fed output scrolled the alt screen (an explicit SU/SD or
    /// line-feed), kick off a smooth slide for it. The terminal has stashed the
    /// departing rows; we set the initial `scroll_y` offset and let
    /// `update_alt_scroll` ease it to zero over `ALT_SCROLL_ANIM_SECS`.
    pub(crate) fn maybe_start_alt_scroll(&mut self) {
        if ALT_SCROLL_ANIM_SECS <= 0.0 {
            return;
        }
        let Some(scroll) = self.terminal.take_alt_scroll() else {
            return;
        };
        // The renderer slides the region and clips its bottom edge, so the
        // departing rows must exit at the top (behind the toolbar). That holds
        // only when the region is anchored at row 0 — the common case (apps
        // reserve a *bottom* status line). A region starting mid-screen would
        // bleed past its top edge, so skip the slide there; the scroll has
        // already been applied to the grid, it just snaps instead of animating.
        if scroll.region_top != 0 {
            self.terminal.clear_alt_anim();
            return;
        }
        let metrics = self.font.face().size_metrics().unwrap();
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let total_px = scroll.rows as f32 * line_height;
        self.alt_scroll_anim = Some(AltScrollAnim {
            up: scroll.up,
            rows: scroll.rows,
            region_top: scroll.region_top,
            region_bottom: scroll.region_bottom,
            total_px,
            started: std::time::Instant::now(),
        });
        // Start displaying the pre-scroll frame: shift the (already-scrolled)
        // grid back by the full distance so the departing rows fill the gap.
        self.scroll_y = if scroll.up { total_px } else { -total_px } as f64;
        self.invalidate();
    }

    /// Advance the alt-screen scroll slide for this frame, easing `scroll_y`
    /// toward zero. Finishes (and releases the frozen rows) when the slide
    /// completes or the alt screen is no longer active. No-op when idle.
    pub(crate) fn update_alt_scroll(&mut self) {
        let Some(anim) = self.alt_scroll_anim else {
            return;
        };
        if !self.terminal.on_alt_screen() {
            self.finish_alt_scroll();
            self.scroll_y = 0.0;
            return;
        }
        let t = (anim.started.elapsed().as_secs_f32() / ALT_SCROLL_ANIM_SECS).clamp(0.0, 1.0);
        // Ease-out cubic: quick to start, gentle to settle.
        let eased = 1.0 - (1.0 - t).powi(3);
        let remaining = anim.total_px * (1.0 - eased);
        if t >= 1.0 || remaining <= 0.5 {
            self.finish_alt_scroll();
            self.scroll_y = 0.0;
            return;
        }
        self.scroll_y = if anim.up { remaining } else { -remaining } as f64;
    }

    /// End any alt-screen scroll slide and drop the terminal's frozen rows.
    /// Leaves `scroll_y` untouched — callers that cancel mid-slide (a keystroke
    /// snapping the view) zero it themselves.
    pub(crate) fn finish_alt_scroll(&mut self) {
        if self.alt_scroll_anim.take().is_some() {
            self.terminal.clear_alt_anim();
        }
    }

    /// True while an alt-screen scroll slide is mid-flight — keeps the event
    /// loop ticking frames until it settles.
    pub(crate) fn is_alt_scroll_animating(&self) -> bool {
        self.alt_scroll_anim.is_some()
    }
}
