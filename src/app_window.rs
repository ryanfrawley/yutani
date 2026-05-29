use std::sync::{Arc, Mutex};

/// Process-unique identifier for a tab (one PTY + `Terminal`). Minted per
/// reader thread so `CustomEvent`s can be routed to the right tab regardless
/// of which window currently owns it (tabs may move between windows later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

#[derive(Debug, Clone)]
pub enum CustomEvent {
    /// Wake signal: the tagged tab's [`PtyOutbox`] has output ready to drain.
    /// Carries no data — the bytes live in the shared outbox, so a flood of
    /// reads collapses into one bulk drain on the loop instead of one event
    /// (and one `String` allocation) per `read()`.
    PtyInput(TabId),
    /// The child shell exited (EOF/EIO on the PTY master, child reaped),
    /// carrying the tab it belonged to and its exit code. How the window
    /// reacts is governed by the `shell_exit_mode` config setting.
    PtyExit(TabId, i32),
    /// SPIKE: a native NSScrollView's offset changed (fired from an NSView
    /// bounds-change notification). Wakes the loop so `about_to_wait` mirrors the
    /// new offset into the terminal — no busy-polling.
    ScrollSync,
}

/// Per-tab PTY output buffer shared between the reader thread (producer) and
/// the event loop (consumer). The reader appends decoded UTF-8 text; the loop
/// drains it in bulk. A single `PtyInput` wake is posted per empty→non-empty
/// transition (see [`push`](PtyOutbox::push)), so a burst of reads coalesces
/// to ~one event per drain cycle rather than one per read — which is what kept
/// the main thread pinned for seconds under heavy output before.
#[derive(Clone)]
pub struct PtyOutbox(Arc<Mutex<Outbox>>);

struct Outbox {
    text: String,
    /// True while a `PtyInput` wake is in flight (buffer non-empty and the loop
    /// hasn't drained it yet). Gates duplicate wakes so the proxy queue can't
    /// fill with one event per read.
    notified: bool,
}

impl PtyOutbox {
    pub fn new() -> Self {
        PtyOutbox(Arc::new(Mutex::new(Outbox {
            text: String::new(),
            notified: false,
        })))
    }

    /// Append decoded output. Returns `true` iff the caller should post a
    /// `PtyInput` wake — i.e. no wake is already pending. Subsequent pushes
    /// before the loop drains return `false`, collapsing the burst.
    pub fn push(&self, s: &str) -> bool {
        let mut g = self.0.lock().unwrap();
        g.text.push_str(s);
        let wake = !g.notified;
        g.notified = true;
        wake
    }

    /// Drain up to `cap` bytes (rounded down to a UTF-8 char boundary).
    /// Returns the chunk and whether more remains buffered. When the buffer
    /// empties, the pending-wake latch clears so the next `push` re-wakes; when
    /// a remainder is left the latch stays set and the caller must self-wake to
    /// continue (capping the work per loop turn keeps the UI responsive).
    pub fn drain_up_to(&self, cap: usize) -> (String, bool) {
        let mut g = self.0.lock().unwrap();
        if g.text.len() <= cap {
            g.notified = false;
            (std::mem::take(&mut g.text), false)
        } else {
            let mut bound = cap;
            while bound > 0 && !g.text.is_char_boundary(bound) {
                bound -= 1;
            }
            let remainder = g.text.split_off(bound);
            let chunk = std::mem::replace(&mut g.text, remainder);
            (chunk, true)
        }
    }
}

impl Default for PtyOutbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::PtyOutbox;

    #[test]
    fn first_push_wakes_then_coalesces_until_drained() {
        let ob = PtyOutbox::new();
        assert!(ob.push("a"), "first push (empty→pending) must wake");
        assert!(!ob.push("b"), "second push must coalesce — no extra wake");
        assert!(!ob.push("c"), "still coalescing while a wake is in flight");
        // One drain takes everything the burst accumulated.
        let (chunk, more) = ob.drain_up_to(1024);
        assert_eq!(chunk, "abc");
        assert!(!more);
        // Latch cleared: the next push wakes again.
        assert!(ob.push("d"), "push after a full drain must wake again");
    }

    #[test]
    fn drain_under_cap_takes_all_and_reports_no_remainder() {
        let ob = PtyOutbox::new();
        ob.push("hello");
        let (chunk, more) = ob.drain_up_to(1024);
        assert_eq!(chunk, "hello");
        assert!(!more);
        // Buffer is empty now.
        let (chunk, more) = ob.drain_up_to(1024);
        assert_eq!(chunk, "");
        assert!(!more);
    }

    #[test]
    fn drain_caps_and_leaves_remainder() {
        let ob = PtyOutbox::new();
        ob.push("abcdefgh");
        let (chunk, more) = ob.drain_up_to(3);
        assert_eq!(chunk, "abc");
        assert!(more, "remainder must be reported");
        let (chunk, more) = ob.drain_up_to(3);
        assert_eq!(chunk, "def");
        assert!(more);
        let (chunk, more) = ob.drain_up_to(3);
        assert_eq!(chunk, "gh");
        assert!(!more, "final slice clears the remainder flag");
    }

    #[test]
    fn cap_rounds_down_to_char_boundary() {
        let ob = PtyOutbox::new();
        // "é" is 2 bytes (0xC3 0xA9); a cap of 2 lands mid-"ñ" below.
        ob.push("añb"); // 'a'=1, 'ñ'=2, 'b'=1 → 4 bytes
        // cap=2 would split inside 'ñ' (byte 2 is not a boundary after 'a'+first
        // byte of 'ñ'); it must round down to 1 ('a' only).
        let (chunk, more) = ob.drain_up_to(2);
        assert_eq!(chunk, "a");
        assert!(more);
        let (chunk, _) = ob.drain_up_to(8);
        assert_eq!(chunk, "ñb");
    }

    #[test]
    fn pending_latch_survives_a_capped_drain() {
        // While a remainder is left the latch stays set, so a concurrent push
        // does NOT request a second wake (the caller self-wakes instead).
        let ob = PtyOutbox::new();
        ob.push("abcdef"); // wakes
        let (_chunk, more) = ob.drain_up_to(3);
        assert!(more);
        assert!(
            !ob.push("ghi"),
            "push during an unfinished drain must not add a wake"
        );
        // The accumulated remainder + new bytes drain together next turn.
        let (chunk, more) = ob.drain_up_to(64);
        assert_eq!(chunk, "defghi");
        assert!(!more);
    }

    #[test]
    fn empty_push_still_arms_the_latch() {
        // A push of "" is the empty→pending transition: it must wake (otherwise
        // any buffered bytes pushed before the first non-empty content would
        // never get a drain scheduled), and it must arm the latch so the next
        // push coalesces.
        let ob = PtyOutbox::new();
        assert!(ob.push(""), "empty push is still an empty→pending transition");
        assert!(!ob.push("x"), "latch is armed — the follow-up coalesces");
        let (chunk, more) = ob.drain_up_to(1024);
        assert_eq!(chunk, "x");
        assert!(!more);
    }

    #[test]
    fn drain_on_fresh_buffer_is_empty_and_unlatched() {
        // Draining a never-pushed outbox returns nothing, reports no remainder,
        // and leaves the latch clear so the very first real push will wake.
        let ob = PtyOutbox::new();
        let (chunk, more) = ob.drain_up_to(1024);
        assert_eq!(chunk, "");
        assert!(!more);
        assert!(ob.push("a"), "first push after an empty drain must wake");
    }

    #[test]
    fn cap_equal_to_buffer_len_takes_all_no_remainder() {
        // Boundary of the `text.len() <= cap` branch: cap == len must take the
        // whole buffer and report no remainder (off-by-one would leave a byte
        // and spin a needless self-wake).
        let ob = PtyOutbox::new();
        ob.push("abcde"); // 5 bytes
        let (chunk, more) = ob.drain_up_to(5);
        assert_eq!(chunk, "abcde");
        assert!(!more, "cap == len is the take-all branch");
        // Latch cleared, so a new push wakes.
        assert!(ob.push("z"));
    }

    #[test]
    fn cap_one_below_buffer_len_leaves_a_byte() {
        // The opposite side of that boundary: cap == len-1 falls into the
        // capped branch and leaves exactly one byte behind.
        let ob = PtyOutbox::new();
        ob.push("abcde");
        let (chunk, more) = ob.drain_up_to(4);
        assert_eq!(chunk, "abcd");
        assert!(more);
        let (chunk, more) = ob.drain_up_to(4);
        assert_eq!(chunk, "e");
        assert!(!more);
    }

    #[test]
    fn cap_on_exact_char_boundary_does_not_round_down() {
        // When the cap lands exactly on a UTF-8 char boundary no rounding
        // should occur — the full multi-byte char before it is included.
        let ob = PtyOutbox::new();
        ob.push("ñb"); // 'ñ'=2 bytes, 'b'=1 → boundaries at 0,2,3
        let (chunk, more) = ob.drain_up_to(2); // byte 2 IS a boundary (after 'ñ')
        assert_eq!(chunk, "ñ");
        assert!(more);
        let (chunk, more) = ob.drain_up_to(2);
        assert_eq!(chunk, "b");
        assert!(!more);
    }

    #[test]
    fn cap_zero_on_nonempty_buffer_makes_no_progress_but_keeps_latch() {
        // cap=0 is a degenerate case: the buffer is non-empty so we hit the
        // capped branch, round the bound down to 0, and split off everything —
        // yielding an empty chunk with `more == true`. This documents that a
        // zero cap makes NO progress while keeping the latch armed; the event
        // loop must never call drain with cap=0 or it would self-wake forever.
        let ob = PtyOutbox::new();
        ob.push("abc");
        let (chunk, more) = ob.drain_up_to(0);
        assert_eq!(chunk, "", "zero cap drains nothing");
        assert!(more, "the whole buffer is still pending");
        // Latch is still armed (we took the capped branch, never cleared it).
        assert!(!ob.push("d"), "latch stayed set across a zero-cap drain");
        // A real cap still drains everything that accumulated.
        let (chunk, more) = ob.drain_up_to(1024);
        assert_eq!(chunk, "abcd");
        assert!(!more);
    }

    #[test]
    fn cap_zero_on_empty_buffer_clears_latch() {
        // cap=0 with an empty buffer takes the `len <= cap` (0 <= 0) branch:
        // returns empty, no remainder, and clears the latch.
        let ob = PtyOutbox::new();
        ob.push("x");
        let _ = ob.drain_up_to(1024); // empty the buffer, clear latch
        let (chunk, more) = ob.drain_up_to(0);
        assert_eq!(chunk, "");
        assert!(!more, "empty buffer is the take-all branch even at cap 0");
        assert!(ob.push("y"), "latch clear — next push wakes");
    }

    #[test]
    fn each_full_drain_re_arms_a_fresh_wake() {
        // Across several independent burst→drain cycles, every cycle that fully
        // empties the buffer must re-arm exactly one wake. Guards against the
        // latch sticking `true` and silently dropping later bursts.
        let ob = PtyOutbox::new();
        for round in 0..3 {
            assert!(ob.push("data"), "round {round}: first push must wake");
            assert!(!ob.push("more"), "round {round}: burst coalesces");
            let (chunk, more) = ob.drain_up_to(1024);
            assert_eq!(chunk, "datamore");
            assert!(!more);
        }
    }
}
