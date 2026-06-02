//! URL detection for the terminal grid: heuristic scanning of cell runs and
//! OSC 8 hyperlink resolution, plus the hover model the renderer underlines
//! and the Cmd-click handler opens.

use crate::{style, terminal};

/// Word-character predicate for double-click word selection. Letters and
/// digits, plus the punctuation that's commonly part of identifiers, paths,
/// and URLs in shell output (so e.g. `~/foo/bar.txt` selects as one token).
pub(crate) fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '~' | '+' | ':' | '@' | '%')
}

/// A clickable URL the mouse is currently hovering over. Tracked while the
/// Cmd modifier is held so the renderer can underline the span and the
/// click handler can open it. URLs that wrap at the right edge span
/// multiple rows; the start/end pair is inclusive on both ends.
/// One underline strip on a single (scroll-stable) line; columns inclusive.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HoverSegment {
    pub(crate) abs_line: isize,
    pub(crate) start_col: usize,
    pub(crate) end_col: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HoverUrl {
    /// Every strip to underline. A heuristic match or an OSC 8 link is one
    /// segment, or a few when it wraps across rows — one per row it spans.
    /// Only the run physically connected to the hovered cell is included, so a
    /// link that merely shares an `id=` with a far-off span doesn't drag that
    /// span in. Ordered by line then column for stable equality (so hover
    /// repaint de-dup works).
    pub(crate) segments: Vec<HoverSegment>,
    /// The URL text itself, ready to hand to `open(1)`.
    pub(crate) url: String,
}

#[cfg(test)]
impl HoverUrl {
    /// First/last segment edges — convenient for the single-run (heuristic or
    /// contiguous OSC 8) cases the tests assert on. Segments are ordered by
    /// line then column.
    pub(crate) fn start_abs_line(&self) -> isize {
        self.segments.first().unwrap().abs_line
    }
    pub(crate) fn end_abs_line(&self) -> isize {
        self.segments.last().unwrap().abs_line
    }
    pub(crate) fn start_col(&self) -> usize {
        self.segments.first().unwrap().start_col
    }
    pub(crate) fn end_col(&self) -> usize {
        self.segments.last().unwrap().end_col
    }
}

/// Schemes the heuristic recognizes in bare (non-OSC-8) text, longest-prefix
/// first so `https` wins over `http`. Kept in step with [`is_safe_url`]'s
/// allowlist — every scheme we'd open on click is also one we'll underline as
/// plain text. `mailto:` has no `//` authority, hence the single colon.
const URL_SCHEMES: [&str; 6] = ["https://", "http://", "file://", "ftp://", "ssh://", "mailto:"];

/// Locate a URL within a row of cells that covers `col`. Recognizes the
/// [`URL_SCHEMES`] set (http/https plus file/ftp/ssh/mailto), so a bare
/// `file:///path` or `ssh://host` printed as plain text is clickable, not just
/// OSC 8 links. The run is bounded by surrounding whitespace; trailing sentence
/// punctuation (`.,;:!?)]}>'"`) is stripped so a URL at the end of a sentence
/// still opens cleanly.
pub(crate) fn find_url_in_cells(cells: &[style::Cell], col: usize) -> Option<(usize, usize, String)> {
    let n = cells.len();
    if col >= n || cells[col].ch.is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !cells[start - 1].ch.is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < n && !cells[end + 1].ch.is_whitespace() {
        end += 1;
    }
    let mut hit: Option<(usize, usize)> = None;
    'outer: for s in start..=end {
        for prefix in URL_SCHEMES {
            let plen = prefix.len();
            if s + plen > end + 1 {
                continue;
            }
            if cells[s..s + plen]
                .iter()
                .zip(prefix.chars())
                .all(|(c, p)| c.ch == p)
            {
                hit = Some((s, plen));
                break 'outer;
            }
        }
    }
    let (url_start_col, prefix_len) = hit?;
    let mut url_end_col = end;
    while url_end_col > url_start_col
        && matches!(
            cells[url_end_col].ch,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '\'' | '"'
        )
    {
        url_end_col -= 1;
    }
    // Reject scheme-only matches like "https://" or "https://." — a URL is
    // only useful if there's at least one host char past the separator.
    if url_end_col + 1 <= url_start_col + prefix_len {
        return None;
    }
    if col < url_start_col || col > url_end_col {
        return None;
    }
    let url: String = cells[url_start_col..=url_end_col]
        .iter()
        .map(|c| c.ch)
        .collect();
    Some((url_start_col, url_end_col, url))
}

/// Cap on how far we'll walk in either direction looking for a wrapped URL
/// continuation. URLs that span more than this many rows are exotic enough
/// that the heuristic isn't worth burning scrollback walks on.
pub(crate) const URL_WRAP_MAX_ROWS: usize = 32;

/// Build the wrapped logical line containing `abs_line`: walk back and
/// forward across rows whose adjacent edges are both non-whitespace (the
/// terminal's autowrap left no separator between them) and concatenate the
/// cells. Returns `(start_abs_line, cols, flat_cells)` so callers can map
/// flat indices back to (row, col). Bounded by `URL_WRAP_MAX_ROWS` either
/// side.
pub(crate) fn build_wrapped_line(
    terminal: &terminal::Terminal,
    abs_line: isize,
) -> Option<(isize, usize, Vec<style::Cell>)> {
    let row = terminal.line_at(abs_line)?;
    let cols = row.len();
    if cols == 0 {
        return Some((abs_line, 0, Vec::new()));
    }

    // Walk back as long as the previous row's last col is non-whitespace
    // *and* the current row's first col is non-whitespace — the only
    // signature autowrap leaves on the cell grid (no soft-wrap flag).
    let mut start = abs_line;
    let mut steps = 0;
    while steps < URL_WRAP_MAX_ROWS {
        let prev = match terminal.line_at(start - 1) {
            Some(p) if p.len() == cols => p,
            _ => break,
        };
        let cur = terminal.line_at(start).expect("walked from a valid row");
        if prev.last().map(|c| c.ch.is_whitespace()).unwrap_or(true)
            || cur.first().map(|c| c.ch.is_whitespace()).unwrap_or(true)
        {
            break;
        }
        start -= 1;
        steps += 1;
    }

    let mut end = abs_line;
    let mut steps = 0;
    while steps < URL_WRAP_MAX_ROWS {
        let next = match terminal.line_at(end + 1) {
            Some(n) if n.len() == cols => n,
            _ => break,
        };
        let cur = terminal.line_at(end).expect("walked from a valid row");
        if cur.last().map(|c| c.ch.is_whitespace()).unwrap_or(true)
            || next.first().map(|c| c.ch.is_whitespace()).unwrap_or(true)
        {
            break;
        }
        end += 1;
        steps += 1;
    }

    let mut buf = Vec::with_capacity(((end - start + 1) as usize) * cols);
    for line in start..=end {
        let r = terminal.line_at(line)?;
        buf.extend_from_slice(r);
    }
    Some((start, cols, buf))
}

/// The inclusive column run of cells carrying `id` that covers `col`, or
/// `None` if the cell at `col` doesn't carry it. A single contiguous stretch:
/// it stops at the first neighbouring cell that lacks the id.
pub(crate) fn run_containing(
    cells: &[style::Cell],
    id: std::num::NonZeroU32,
    col: usize,
) -> Option<(usize, usize)> {
    if cells.get(col)?.hyperlink != Some(id) {
        return None;
    }
    let mut start = col;
    while start > 0 && cells[start - 1].hyperlink == Some(id) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cells.len() && cells[end + 1].hyperlink == Some(id) {
        end += 1;
    }
    Some((start, end))
}

/// Locate an OSC 8 explicit hyperlink under `(abs_line, col)` and return only
/// the **physically connected** run of it that the pointer is on — the
/// contiguous cells under the cursor, plus any rows the link autowrapped onto
/// (one row's run reaches the last column and the next resumes at column 0).
/// We deliberately do *not* gather every other span that merely shares the
/// `id=`: an app can reuse one id across many separate occurrences of a link,
/// and lighting up copies several line-breaks away (as the old behaviour did)
/// is surprising. Only the glyphs joined to the one being hovered underline.
///
/// Connectivity is the same grid signature [`build_wrapped_line`] uses for the
/// heuristic — edge-to-edge across a row boundary — so a wrapped link still
/// resolves as one span. The walk is bounded by [`URL_WRAP_MAX_ROWS`] each way.
/// Takes precedence over the heuristic: the extent and target are exactly what
/// the app declared. Segments come back ordered by line for stable equality.
pub(crate) fn find_osc8_link_at(
    terminal: &terminal::Terminal,
    abs_line: isize,
    col: usize,
) -> Option<HoverUrl> {
    let row = terminal.line_at(abs_line)?;
    let cols = row.len();
    if col >= cols {
        return None;
    }
    let id = row[col].hyperlink?;
    let uri = terminal.hyperlink_uri(id)?.to_string();
    let (start_col, end_col) = run_containing(row, id, col)?;

    let mut segments = vec![HoverSegment {
        abs_line,
        start_col,
        end_col,
    }];

    // Walk up while the topmost strip starts at column 0: that's only a wrap
    // continuation if the row above ends on this same link at its last column.
    let mut top_line = abs_line;
    let mut top_start = start_col;
    let mut steps = 0;
    while top_start == 0 && steps < URL_WRAP_MAX_ROWS {
        let Some(prev) = terminal.line_at(top_line - 1) else {
            break;
        };
        if prev.len() != cols {
            break;
        }
        let Some((ps, pe)) = run_containing(prev, id, cols - 1) else {
            break;
        };
        segments.push(HoverSegment {
            abs_line: top_line - 1,
            start_col: ps,
            end_col: pe,
        });
        top_line -= 1;
        top_start = ps;
        steps += 1;
    }

    // Mirror downward: extend while this link reaches the last column and the
    // row below resumes it at column 0.
    let mut bot_line = abs_line;
    let mut bot_end = end_col;
    let mut steps = 0;
    while bot_end == cols - 1 && steps < URL_WRAP_MAX_ROWS {
        let Some(next) = terminal.line_at(bot_line + 1) else {
            break;
        };
        if next.len() != cols {
            break;
        }
        let Some((ns, ne)) = run_containing(next, id, 0) else {
            break;
        };
        segments.push(HoverSegment {
            abs_line: bot_line + 1,
            start_col: ns,
            end_col: ne,
        });
        bot_line += 1;
        bot_end = ne;
        steps += 1;
    }

    // Hovering any row of the link must yield the identical set; the walk
    // visits rows out of order, so sort to the line-then-column order the
    // `HoverUrl` accessors and repaint de-dup rely on.
    segments.sort_by_key(|s| (s.abs_line, s.start_col));
    Some(HoverUrl {
        segments,
        url: uri,
    })
}

/// Scheme allowlist for opening a clicked link. The heuristic produces only
/// these schemes (see [`URL_SCHEMES`]), but OSC 8 lets an app declare an
/// arbitrary target, so we refuse anything outside this small safe set (no
/// `javascript:`, `data:`, `vbscript:`, etc.) before handing it to the OS
/// opener.
pub(crate) fn is_safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    const SAFE: [&str; 6] = ["http://", "https://", "mailto:", "ftp://", "file://", "ssh://"];
    SAFE.iter().any(|p| lower.starts_with(p))
}

pub(crate) fn find_url_at(
    terminal: &terminal::Terminal,
    abs_line: isize,
    col: usize,
) -> Option<HoverUrl> {
    // App-declared OSC 8 links win over the heuristic: exact bounds, and they
    // may carry non-http schemes the heuristic can't express.
    if let Some(hu) = find_osc8_link_at(terminal, abs_line, col) {
        return Some(hu);
    }
    let (start_abs, cols, flat) = build_wrapped_line(terminal, abs_line)?;
    if cols == 0 || col >= cols {
        return None;
    }
    let row_offset = (abs_line - start_abs) as usize;
    let virtual_col = row_offset * cols + col;
    let (s, e, url) = find_url_in_cells(&flat, virtual_col)?;
    // A heuristic match is one contiguous (possibly wrapped) run: first row
    // from `start_col` to the edge, full-width middle rows, last row to
    // `end_col`. Express it as the same per-line segments OSC 8 uses.
    let start_line = start_abs + (s / cols) as isize;
    let start_col = s % cols;
    let end_line = start_abs + (e / cols) as isize;
    let end_col = e % cols;
    let mut segments = Vec::new();
    let mut line = start_line;
    while line <= end_line {
        let from = if line == start_line { start_col } else { 0 };
        let to = if line == end_line { end_col } else { cols - 1 };
        segments.push(HoverSegment {
            abs_line: line,
            start_col: from,
            end_col: to,
        });
        line += 1;
    }
    Some(HoverUrl { segments, url })
}

#[cfg(target_os = "macos")]
pub(crate) fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn open_url(_url: &str) {}
