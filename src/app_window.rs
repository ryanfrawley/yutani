use std::collections::VecDeque;
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
    /// Decoded UTF-8 bytes awaiting drain. A `VecDeque` (not a `String`) so a
    /// capped drain consumes the front in O(cap): the head pointer advances and
    /// the freed space is recycled for later pushes, with no copy of the
    /// unconsumed tail. A `String` here forced `split_off` to memmove the whole
    /// remaining backlog on every drain — O(n²) under sustained backpressure,
    /// which profiling showed as ~a third of the main thread on a flood.
    ///
    /// Invariant: only ever extended from `&str`, and capped drains cut on a
    /// UTF-8 char boundary, so the contents are always valid UTF-8.
    bytes: VecDeque<u8>,
    /// True while a `PtyInput` wake is in flight (buffer non-empty and the loop
    /// hasn't drained it yet). Gates duplicate wakes so the proxy queue can't
    /// fill with one event per read.
    notified: bool,
}

impl PtyOutbox {
    pub fn new() -> Self {
        PtyOutbox(Arc::new(Mutex::new(Outbox {
            bytes: VecDeque::new(),
            notified: false,
        })))
    }

    /// Append decoded output. Returns `true` iff the caller should post a
    /// `PtyInput` wake — i.e. no wake is already pending. Subsequent pushes
    /// before the loop drains return `false`, collapsing the burst.
    pub fn push(&self, s: &str) -> bool {
        let mut g = self.0.lock().unwrap();
        g.bytes.extend(s.as_bytes());
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
        let len = g.bytes.len();
        let (bound, more) = if len <= cap {
            (len, false)
        } else {
            // Round `cap` down to a char boundary: back off while the byte at
            // `bound` is a UTF-8 continuation byte (0b10xx_xxxx). The buffer is
            // valid UTF-8 and `bound > 0`, so this lands on a real boundary.
            let mut bound = cap;
            while bound > 0 && g.bytes[bound] & 0xC0 == 0x80 {
                bound -= 1;
            }
            (bound, true)
        };
        if !more {
            g.notified = false;
        }
        // Front drain: O(bound). The remaining tail is not relocated.
        let chunk: Vec<u8> = g.bytes.drain(..bound).collect();
        debug_assert!(std::str::from_utf8(&chunk).is_ok());
        // SAFETY: `bytes` is only ever extended from `&str` and cut on a char
        // boundary, so `chunk` is always valid UTF-8 (asserted in debug builds).
        let s = unsafe { String::from_utf8_unchecked(chunk) };
        (s, more)
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

    #[test]
    fn large_buffer_drained_at_small_cap_reassembles_byte_for_byte() {
        // The core of the VecDeque rewrite: a big backlog drained in many small
        // front-cuts must come back out in order, byte-for-byte identical, with
        // every turn but the last reporting a remainder. (Under the old String
        // backend this was the O(n²) memmove-the-tail path.)
        let ob = PtyOutbox::new();
        // ~256 KiB of deterministic non-ASCII-free content; pushed in several
        // chunks to mimic a flood arriving across reads.
        let mut input = String::new();
        for i in 0..25_000u32 {
            input.push_str(&format!("line{i:05}\n"));
        }
        assert!(input.len() > 200_000, "want a backlog well past any cap");
        // Push in pieces (only the first should request a wake).
        let mid = {
            // split on a char boundary near the middle
            let mut m = input.len() / 2;
            while !input.is_char_boundary(m) {
                m += 1;
            }
            m
        };
        assert!(ob.push(&input[..mid]), "first push wakes");
        assert!(!ob.push(&input[mid..]), "second push coalesces");

        // Drain repeatedly at a small cap until empty, reassembling as we go.
        let cap = 97; // small, prime-ish, not a divisor of the content length
        let mut out = String::with_capacity(input.len());
        let mut turns = 0;
        loop {
            let (chunk, more) = ob.drain_up_to(cap);
            assert!(
                chunk.len() <= cap,
                "a capped chunk never exceeds the cap (turn {turns})"
            );
            out.push_str(&chunk);
            turns += 1;
            if !more {
                break;
            }
            assert!(turns < 100_000, "drain must terminate");
        }
        assert_eq!(out, input, "reassembled stream must equal the input exactly");
        // Many turns actually happened — we exercised the multi-chunk path.
        assert!(
            turns >= input.len() / cap,
            "expected at least {} drain turns, took {turns}",
            input.len() / cap
        );
        // Buffer is empty and the latch was cleared by the final full drain.
        assert!(ob.push("z"), "push after the final full drain wakes again");
    }

    #[test]
    fn interleaved_push_and_drain_preserves_fifo_with_no_loss() {
        // Producer/consumer interleave simulated single-threaded: push a slice,
        // drain a capped chunk, push more, drain again… Everything must emerge
        // in FIFO order with nothing lost or duplicated, and the final drain
        // (after the producer stops) must report `more == false`.
        let ob = PtyOutbox::new();
        let mut expected = String::new();
        let mut out = String::new();
        let cap = 5;

        let mut woke_first = false;
        for round in 0..200u32 {
            let piece = format!("[{round}]"); // variable-length, 3–7 bytes
            expected.push_str(&piece);
            let wake = ob.push(&piece);
            if !woke_first {
                assert!(wake, "the very first push into an empty buffer wakes");
                woke_first = true;
            }
            // Drain one capped chunk each round — the consumer lags the producer,
            // so a backlog builds and persists (more stays true throughout).
            let (chunk, _more) = ob.drain_up_to(cap);
            assert!(chunk.len() <= cap);
            out.push_str(&chunk);
        }
        // Producer stopped; flush the remaining backlog to completion.
        loop {
            let (chunk, more) = ob.drain_up_to(cap);
            out.push_str(&chunk);
            if !more {
                break;
            }
        }
        assert_eq!(out, expected, "FIFO order preserved, no loss or duplication");
        // Final drain emptied the buffer, so the latch re-arms.
        assert!(ob.push("!"), "latch clear after the buffer fully drained");
    }

    #[test]
    fn multibyte_content_capped_mid_char_never_yields_invalid_utf8() {
        // Caps repeatedly land mid-character on a mix of 2-, 3- and 4-byte
        // scalars. Each chunk must be independently valid UTF-8 (rounded down to
        // a boundary) and the concatenation must equal the original string. A
        // bad boundary cut would corrupt the stream or panic.
        let ob = PtyOutbox::new();
        let mut input = String::new();
        // emoji (4 bytes), CJK (3 bytes), accented latin (2 bytes), ascii (1).
        for _ in 0..400 {
            input.push_str("😀漢ña");
        }
        ob.push(&input);

        // Sweep caps that are deliberately not aligned to the 4+3+2+1 = 10-byte
        // group, so cuts land inside every kind of multibyte scalar.
        let caps = [1usize, 2, 3, 4, 5, 7, 11, 13];
        let mut out = String::new();
        let mut i = 0;
        loop {
            let cap = caps[i % caps.len()];
            i += 1;
            let (chunk, more) = ob.drain_up_to(cap);
            // `chunk` is a String, so if drain ever produced invalid UTF-8 it
            // would have panicked building it; assert validity explicitly too.
            assert!(
                std::str::from_utf8(chunk.as_bytes()).is_ok(),
                "every chunk is valid UTF-8"
            );
            assert!(chunk.len() <= cap, "chunk respects the cap");
            out.push_str(&chunk);
            if !more {
                break;
            }
            assert!(i < 1_000_000, "drain must terminate");
        }
        assert_eq!(out, input, "multibyte stream reassembles exactly");
    }

    #[test]
    fn wake_latch_re_arms_only_after_a_full_multi_drain_sequence() {
        // Across a long capped-drain sequence the pending-wake latch must stay
        // armed for every intermediate (remainder) turn and clear exactly once,
        // on the turn that finally empties the buffer.
        let ob = PtyOutbox::new();
        ob.push(&"x".repeat(50)); // single push, wakes once
        let cap = 7;
        let mut emptied = false;
        for turn in 0..100 {
            let (_chunk, more) = ob.drain_up_to(cap);
            if more {
                // Backlog remains: latch must still be armed, so a push here
                // does NOT request another wake (the loop self-wakes).
                assert!(
                    !ob.push(""),
                    "turn {turn}: latch stays armed while a remainder is pending"
                );
            } else {
                // This turn emptied the buffer (note: the empty push above only
                // armed the latch, it added no bytes). The latch is now clear,
                // so the next push must wake.
                assert!(
                    ob.push("more"),
                    "turn {turn}: full drain clears the latch — next push wakes"
                );
                emptied = true;
                break;
            }
        }
        assert!(emptied, "the sequence must reach a full-drain turn");
        // After re-arming, the freshly pushed bytes drain and re-clear normally.
        let (chunk, more) = ob.drain_up_to(1024);
        assert_eq!(chunk, "more");
        assert!(!more);
    }
}
