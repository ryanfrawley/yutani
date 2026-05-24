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
    /// Every strip to underline. A heuristic match or a contiguous OSC 8 link
    /// is one segment (or a few, when it wraps across rows); an OSC 8 link
    /// whose `id=` is shared by non-contiguous spans contributes a segment per
    /// visible run, so all siblings underline together. Ordered by line then
    /// column for stable equality (so hover repaint de-dup works).
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

/// Locate an http/https URL within a row of cells that covers `col`. The
/// run is bounded by surrounding whitespace; trailing sentence punctuation
/// (`.,;:!?)]}>'"`) is stripped so a URL at the end of a sentence still
/// opens cleanly.
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
        for prefix in ["https://", "http://"] {
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

/// Locate the URL under `(abs_line, col)`, joining wrap-continued rows so a
/// link that spilled past the right edge still resolves as a single span.
/// Falls back to a same-row search when no wrap continuation is in play.
/// Inclusive column runs of cells whose hyperlink id equals `id`, in one row.
pub(crate) fn hyperlink_runs(
    cells: &[style::Cell],
    id: std::num::NonZeroU32,
) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        if cells[i].hyperlink == Some(id) {
            let start = i;
            while i + 1 < cells.len() && cells[i + 1].hyperlink == Some(id) {
                i += 1;
            }
            runs.push((start, i));
        }
        i += 1;
    }
    runs
}

/// Locate an OSC 8 explicit hyperlink under `(abs_line, col)`. The link id is
/// taken from the cell; every visible cell sharing that id is part of the same
/// logical link (the OSC 8 `id=` contract), so we collect a [`HoverSegment`]
/// for every run of it across the visible rows — including non-contiguous
/// siblings, which then underline together. We scan only what's on screen
/// because that's all the overlay can draw; siblings scrolled off don't need a
/// strip. Takes precedence over the heuristic: the extent and target are
/// exactly what the app declared.
pub(crate) fn find_osc8_link_at(
    terminal: &terminal::Terminal,
    abs_line: isize,
    col: usize,
) -> Option<HoverUrl> {
    let row = terminal.line_at(abs_line)?;
    if col >= row.len() {
        return None;
    }
    let id = row[col].hyperlink?;
    let uri = terminal.hyperlink_uri(id)?.to_string();

    let mut segments = Vec::new();
    for v in 0..terminal.rows as isize {
        let line = terminal.visual_to_abs_line(v);
        let Some(cells) = terminal.line_at(line) else {
            continue;
        };
        for (start_col, end_col) in hyperlink_runs(cells, id) {
            segments.push(HoverSegment {
                abs_line: line,
                start_col,
                end_col,
            });
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(HoverUrl {
        segments,
        url: uri,
    })
}

/// Scheme allowlist for opening a clicked link. The heuristic only ever
/// produces http/https, but OSC 8 lets an app declare an arbitrary target, so
/// we refuse anything outside a small safe set (no `javascript:`, `data:`,
/// `vbscript:`, etc.) before handing it to the OS opener.
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
