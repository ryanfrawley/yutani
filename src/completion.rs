//! Filesystem path completion.
//!
//! A pure, synchronous function library: given the current edit buffer, the
//! cursor (a character offset), and the shell's cwd, it returns path
//! completions for the token under the cursor. Nothing renders or wires input
//! here — a later UI slice consumes [`complete_path`].
//!
//! Scanning is done inline with `read_dir`; a later slice can move it
//! off-thread if directory size becomes a latency concern.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A single path-completion candidate produced by [`complete_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The full replacement text for the token under the cursor — i.e. the
    /// token's existing directory portion plus the matched entry name (e.g.
    /// token `src/ma` against a match `main.rs` yields `src/main.rs`).
    /// Directories carry a trailing `/`. This is what a later slice writes
    /// back to the shell, so it is the *whole* token, not just the entry name.
    pub text: String,
    /// True if the entry is a directory. The UI sorts these first and the
    /// trailing `/` in `text` reflects it.
    pub is_dir: bool,
    /// True when `text` is a full command line (a history match) that replaces
    /// the whole typed line, vs. a token completion that replaces only the
    /// token under the cursor. Drives how `accept_suffix` computes what to type.
    pub whole_line: bool,
}

/// Maximum candidates returned, to bound directory-scan cost and popup size.
pub const MAX_SUGGESTIONS: usize = 50;

/// Compute filesystem path completions for the token under the cursor.
///
/// `buffer` is the full edit line; `cursor` is a *character* (code-point)
/// offset into it (the units `Terminal::current_input()` reports). `cwd` is
/// the shell's working directory used to resolve relative tokens; when `None`,
/// only absolute and `~` tokens can resolve (relative tokens yield no results).
/// `home` is the directory a leading `~` expands to (the value of `$HOME`);
/// when `None`, `~`-tokens yield no results. It is passed in rather than read
/// from the process environment so tests can supply an explicit temp home
/// without mutating the shared `HOME` env var.
///
/// Returns up to [`MAX_SUGGESTIONS`] entries, directories first then files,
/// each sorted lexicographically. Returns empty when there is no token, the
/// target directory can't be read, or nothing matches.
pub fn complete_path(
    buffer: &str,
    cursor: usize,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Vec<Suggestion> {
    let (_token_start, token) = token_under_cursor(buffer, cursor);

    let Some((scan_dir, file_prefix, dir_portion)) = split_path(token, cwd, home) else {
        return Vec::new();
    };

    let Ok(entries) = std::fs::read_dir(&scan_dir) else {
        return Vec::new();
    };

    // Hidden entries (leading `.`) are only offered when the user has typed a
    // leading `.` themselves — matching shell glob behavior.
    let want_hidden = file_prefix.starts_with('.');

    let mut suggestions: Vec<Suggestion> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            // Skip non-UTF-8 names; the replacement text is a Rust `String`.
            continue;
        };

        // Case-sensitive prefix match (shell default; case-insensitive
        // matching is a possible future option).
        if !name.starts_with(file_prefix) {
            continue;
        }
        if name.starts_with('.') && !want_hidden {
            continue;
        }

        // Determine directory-ness without following the entry (no symlink
        // traversal); treat an errored `file_type` as a non-directory.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let slash = if is_dir { "/" } else { "" };
        suggestions.push(Suggestion {
            text: format!("{dir_portion}{name}{slash}"),
            is_dir,
            whole_line: false,
        });
    }

    // Directories first, then lexicographic by replacement text.
    suggestions.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.text.cmp(&b.text)));
    suggestions.truncate(MAX_SUGGESTIONS);
    suggestions
}

/// True when there is a non-empty token (a run of non-whitespace ending at the
/// cursor) under the cursor. The completion UI gates the popup on this so an
/// empty token — cursor at the start of the line or right after whitespace —
/// does NOT dump the entire cwd. A trailing-slash token like `sub/` is
/// non-empty and so still shows (it lists the directory's contents).
pub(crate) fn has_path_token(buffer: &str, cursor: usize) -> bool {
    !token_under_cursor(buffer, cursor).1.is_empty()
}

/// True when `token_start` is in command position: everything before it on the
/// line is whitespace (so the token is the first word of the line).
//
// TODO: pipe/semicolon segments — v1 only treats the first word of the whole
// line as a command; words after `|`, `;`, `&&` are not yet recognized as
// command positions.
fn is_command_position(buffer: &str, token_start: usize) -> bool {
    buffer[..token_start].chars().all(|c| c == ' ' || c == '\t')
}

/// Complete executable names on `$PATH` for `prefix`. Scans each `:`-separated
/// directory in `path`, collects regular-file (symlinks followed) entries whose
/// name starts with `prefix` and that have an execute bit set, dedups by name
/// keeping the first occurrence in PATH order (matching shell command lookup),
/// sorts the survivors lexicographically, and caps at MAX_SUGGESTIONS. Each
/// suggestion's `text` is the command name plus a trailing space (so the user
/// can type arguments next), `is_dir: false`. Returns empty when `path` is None
/// or nothing matches.
//
// This scans PATH synchronously on each input change. The caller caches the
// result (it is not recomputed per frame), but a future slice could index/cache
// the PATH contents to avoid re-reading every directory on each keystroke.
//
// `path` here is the *terminal process's* `$PATH`, not the shell's live PATH
// (a known limitation: the shell's PATH after `export PATH=...` would need an
// OSC report to observe). Likewise this only sees executables on disk — no
// shell builtins, functions, or aliases.
pub fn complete_command(prefix: &str, path: Option<&std::ffi::OsStr>) -> Vec<Suggestion> {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = path else {
        return Vec::new();
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut suggestions: Vec<Suggestion> = Vec::new();

    // `split_paths` handles `:` separation (and empty entries) correctly.
    for dir in std::env::split_paths(path) {
        // Skip dirs we can't read (nonexistent, no permission, etc.).
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };

            // Case-sensitive prefix match (matches the path completer;
            // case-insensitive matching is a possible future option).
            if !name.starts_with(prefix) {
                continue;
            }

            // Follow symlinks: a symlink to an executable is itself runnable.
            // A dangling symlink errors here and is skipped.
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            // Must be a regular file (exclude directories, which carry the
            // exec bit but aren't commands) with an execute bit set.
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                continue;
            }

            // Dedup by name in PATH order: first occurrence wins (matching the
            // shell's command lookup).
            if seen.insert(name.to_string()) {
                suggestions.push(Suggestion {
                    text: format!("{name} "),
                    is_dir: false,
                    whole_line: false,
                });
            }
        }
    }

    suggestions.sort_by(|a, b| a.text.cmp(&b.text));
    suggestions.truncate(MAX_SUGGESTIONS);
    suggestions
}

/// Parse a zsh history file's contents into commands, oldest-first. Handles the
/// extended-history line form `: <ts>:<elapsed>;<command>` (strip through the
/// first `;`) and plain lines. Joins `\`-continued lines (zsh's multi-line
/// entry encoding) with a newline. Skips blank entries. v1: best-effort; exotic
/// metafied bytes are passed through.
pub fn parse_zsh_history(contents: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;

    for raw in contents.lines() {
        // A line that ends in an unescaped backslash continues onto the next
        // physical line (zsh encodes embedded newlines this way). We detect a
        // trailing `\` and, if present, strip it and keep accumulating.
        let continues = raw.ends_with('\\');
        let line = if continues {
            &raw[..raw.len() - 1]
        } else {
            raw
        };

        // For a fresh entry (no continuation pending), strip the extended-
        // history metadata prefix `: <ts>:<elapsed>;` if present.
        let segment = if pending.is_none() {
            strip_ext_history_prefix(line)
        } else {
            line
        };

        match &mut pending {
            Some(acc) => {
                acc.push('\n');
                acc.push_str(segment);
            }
            None => pending = Some(segment.to_string()),
        }

        if !continues {
            if let Some(cmd) = pending.take() {
                if !cmd.is_empty() {
                    out.push(cmd);
                }
            }
        }
    }

    // A trailing continued entry with no final newline still counts.
    if let Some(cmd) = pending.take() {
        if !cmd.is_empty() {
            out.push(cmd);
        }
    }

    out
}

/// Strip zsh's extended-history metadata prefix from a line: `: <ts>:<elapsed>;`
/// leaving just the command. A line that doesn't start with `: ` (a plain
/// history entry) is returned unchanged.
fn strip_ext_history_prefix(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix(": ") {
        // `<ts>:<elapsed>;<command>` — the command begins after the first `;`.
        if let Some(idx) = rest.find(';') {
            return &rest[idx + 1..];
        }
    }
    line
}

/// History suggestions for `prefix`: entries from `history` (assumed already
/// most-recent-first and deduped) that START WITH the non-empty `prefix`,
/// excluding any entry equal to `prefix` (nothing to add), preserving order,
/// capped at MAX_SUGGESTIONS. Each is a whole-line suggestion (`whole_line:
/// true, is_dir: false`). Empty `prefix` → empty (never suggest all history).
pub fn history_matches(history: &[String], prefix: &str) -> Vec<Suggestion> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Suggestion> = Vec::new();
    for entry in history {
        if entry == prefix {
            continue;
        }
        if entry.starts_with(prefix) {
            out.push(Suggestion {
                text: entry.clone(),
                is_dir: false,
                whole_line: true,
            });
            if out.len() >= MAX_SUGGESTIONS {
                break;
            }
        }
    }
    out
}

/// Map a character (code-point) `cursor` offset into `buffer` to a byte offset,
/// clamping past-end cursors to the buffer length.
fn cursor_byte_offset(buffer: &str, cursor: usize) -> usize {
    buffer
        .char_indices()
        .nth(cursor)
        .map(|(b, _)| b)
        .unwrap_or(buffer.len())
}

/// Top-level completion: whole-line history matches (shown first) merged with
/// token completion — command (`$PATH`) completion when the cursor is on the
/// command word, otherwise filesystem path completion.
///
/// `history` is most-recent-first, deduped command lines (shell history file +
/// this session). `manual` is true for an explicit trigger (Ctrl+Space): it
/// lets the token branch run even on an empty token (listing the cwd); the
/// automatic path passes false so an empty token never auto-dumps the cwd.
///
/// The merged list is deduped by `text` (history wins over a duplicate token
/// suggestion) and truncated to [`MAX_SUGGESTIONS`].
///
/// `home` is the `$HOME` directory used to expand a leading `~`; it is passed
/// in (rather than read from the process env here) so tests can supply an
/// explicit temp home without mutating the shared `HOME` env var.
pub fn complete(
    buffer: &str,
    cursor: usize,
    cwd: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
    history: &[String],
    manual: bool,
) -> Vec<Suggestion> {
    let cursor_byte = cursor_byte_offset(buffer, cursor);
    let prefix = &buffer[..cursor_byte];

    // History matches first.
    let mut out = history_matches(history, prefix);

    // Token completion (command or path), gated so an empty token doesn't
    // auto-dump the cwd unless this is a manual trigger.
    let (token_start, token) = token_under_cursor(buffer, cursor);
    if !token.is_empty() || manual {
        let looks_like_command = !token.is_empty()
            && !token.contains('/')
            && !token.starts_with('~')
            && !token.starts_with('.')
            && is_command_position(buffer, token_start);
        let token_suggestions = if looks_like_command {
            complete_command(token, path)
        } else {
            complete_path(buffer, cursor, cwd, home)
        };
        out.extend(token_suggestions);
    }

    // Dedup by text, keeping the first occurrence (history precedes token
    // suggestions, so a history match wins over a duplicate token suggestion).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.text.clone()));
    out.truncate(MAX_SUGGESTIONS);
    out
}

/// Computed on-screen rectangle for the completion popup, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupLayout {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// True when the popup was flipped to sit *above* the cursor row because it
    /// would have overflowed the bottom of the screen below the cursor.
    pub flipped_above: bool,
}

/// Place an `n_items`-row popup anchored to the cell below the cursor.
///
/// `anchor_x` is the popup's left edge (the left of the cursor column);
/// `anchor_below_y` is the y of the top of the line *below* the cursor row
/// (where the popup normally hangs); `cursor_top_y` is the top of the cursor's
/// own row, used as the popup's bottom edge when it flips above.
///
/// The box height is `n_items * item_h`. When the popup would overflow the
/// bottom of the screen (minus `pad`), it flips to sit above the cursor row.
/// `x` is clamped so the right edge stays `pad` inside `screen_w` and never
/// goes left of `pad`; `y` is clamped to never go above `pad`.
#[allow(clippy::too_many_arguments)]
pub fn popup_layout(
    anchor_x: f32,
    anchor_below_y: f32,
    cursor_top_y: f32,
    n_items: usize,
    item_h: f32,
    box_w: f32,
    screen_w: f32,
    screen_h: f32,
    pad: f32,
) -> PopupLayout {
    let h = n_items as f32 * item_h;

    // Vertical: prefer below the cursor; flip above if it would overflow the
    // bottom margin and there's more room above than the overflow forces.
    let overflows_below = anchor_below_y + h > screen_h - pad;
    let (mut y, flipped_above) = if overflows_below {
        // Sit above: bottom edge at the cursor row's top, growing upward.
        (cursor_top_y - h, true)
    } else {
        (anchor_below_y, false)
    };
    // Clamp to the top margin (covers a popup too tall to fit either way).
    if y < pad {
        y = pad;
    }

    // Horizontal: keep the right edge on screen, but never push left of `pad`.
    let mut x = anchor_x;
    if x + box_w > screen_w - pad {
        x = screen_w - pad - box_w;
    }
    if x < pad {
        x = pad;
    }

    PopupLayout {
        x,
        y,
        w: box_w,
        h,
        flipped_above,
    }
}

/// Extract the token under the cursor: the run of non-whitespace characters
/// ending at the cursor. We scan left from the cursor to the previous
/// whitespace (space or tab) or the start of the buffer. Everything to the
/// right of the cursor is ignored (v1 assumes the cursor sits at the token's
/// end — the common completion case). Returns the byte offset of the token's
/// start and the token substring. An empty token (cursor at buffer start or
/// right after whitespace) is valid and means "list the directory".
fn token_under_cursor(buffer: &str, cursor: usize) -> (usize, &str) {
    // `cursor` is a character offset; map it to a byte offset. Clamp past-end
    // cursors to the buffer end so callers can't index out of bounds.
    let cursor_byte = buffer
        .char_indices()
        .nth(cursor)
        .map(|(b, _)| b)
        .unwrap_or(buffer.len());

    // Walk left over the bytes before the cursor, stopping at the last space
    // or tab. `char_indices` keeps the math correct across multi-byte chars.
    let mut token_start = cursor_byte;
    for (b, ch) in buffer[..cursor_byte].char_indices().rev() {
        if ch == ' ' || ch == '\t' {
            break;
        }
        token_start = b;
    }

    (token_start, &buffer[token_start..cursor_byte])
}

/// Bytes to append to accept `sug` for the current line: the part of `sug.text`
/// beyond what's already typed. For a whole-line (history) suggestion the typed
/// part is the buffer up to the cursor; for a token suggestion it's the token
/// under the cursor. Returns `None` when `sug.text` doesn't extend the typed
/// part — a guard against writing garbage if the cached suggestion is stale
/// relative to the live buffer. Assumes the cursor sits at the end of the token
/// (v1; mid-token accept is a later refinement).
pub fn accept_suffix<'a>(buffer: &str, cursor: usize, sug: &'a Suggestion) -> Option<&'a str> {
    let typed = if sug.whole_line {
        let cursor_byte = cursor_byte_offset(buffer, cursor);
        &buffer[..cursor_byte]
    } else {
        token_under_cursor(buffer, cursor).1
    };
    sug.text.strip_prefix(typed)
}

/// New scroll-window start so row `selected` stays within a `max_visible`-row
/// viewport that currently starts at `current_start`. Scrolls just enough to
/// bring `selected` into view (no centering).
pub fn visible_window_start(selected: usize, current_start: usize, max_visible: usize) -> usize {
    if selected < current_start {
        selected
    } else if max_visible > 0 && selected >= current_start + max_visible {
        selected + 1 - max_visible
    } else {
        current_start
    }
}

/// Split a token into the directory to scan, the filename prefix to match, and
/// the directory portion of the token (the part up to and including the last
/// `/`, or empty) used to rebuild [`Suggestion::text`] so the replacement
/// preserves what the user already typed.
///
/// Returns `None` when the token can't resolve to a scannable directory: a
/// `~`-token with no `HOME`, or a relative token with no `cwd`.
fn split_path<'a>(
    token: &'a str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<(PathBuf, &'a str, &'a str)> {
    // The directory portion is the token up to and including its last `/`.
    let (dir_portion, prefix) = match token.rfind('/') {
        Some(i) => (&token[..=i], &token[i + 1..]),
        None => ("", token),
    };

    if token.starts_with('~') {
        // Expand a leading `~` / `~/...` against the supplied home directory.
        // Without a home, a `~`-token can't resolve.
        let home = home?.to_path_buf();
        // The directory portion after the leading `~`, e.g. `~/src/` -> `src/`.
        // For a bare `~` (or `~foo` with no slash) the `~` lands in `prefix`,
        // not `dir_portion`, so `dir_portion` is empty — strip safely rather
        // than slicing `[1..]` (which would panic on an empty string). With no
        // slash yet there's nothing to scan beyond HOME, so `rest` becomes ""
        // and we scan HOME with the (non-matching) prefix: no panic, no spurious
        // results. `~/`, `~/src/`, etc. are unaffected.
        let rest = dir_portion.strip_prefix('~').unwrap_or(dir_portion);
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        let scan_dir = if rest.is_empty() {
            home
        } else {
            home.join(rest)
        };
        Some((scan_dir, prefix, dir_portion))
    } else if token.starts_with('/') {
        // Absolute token: scan its parent directory.
        let scan_dir = if dir_portion.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(dir_portion)
        };
        Some((scan_dir, prefix, dir_portion))
    } else {
        // Relative token: resolve the directory portion against the cwd. With
        // no cwd we can't resolve relative paths.
        let cwd = cwd?;
        let scan_dir = if dir_portion.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(dir_portion)
        };
        Some((scan_dir, prefix, dir_portion))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// RAII temp directory: created on `new`, removed on drop (even if a test
    /// panics). Uses pid + an atomic counter for a unique-enough name without
    /// pulling in the `tempfile` crate.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let mut p = std::env::temp_dir();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            p.push(format!("yutani_k9_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&p).unwrap();
            TempDir { path: p }
        }

        fn touch(&self, name: &str) {
            std::fs::write(self.path.join(name), b"").unwrap();
        }

        fn mkdir(&self, name: &str) {
            std::fs::create_dir_all(self.path.join(name)).unwrap();
        }

        /// Create a file with the given octal mode (used to make executables).
        fn touch_mode(&self, name: &str, mode: u32) {
            use std::os::unix::fs::PermissionsExt;
            let p = self.path.join(name);
            std::fs::write(&p, b"").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn texts(s: &[Suggestion]) -> Vec<String> {
        s.iter().map(|x| x.text.clone()).collect()
    }

    #[test]
    fn token_picks_run_ending_at_cursor() {
        // Cursor at end of `ap` in `cat ap`.
        let (start, tok) = token_under_cursor("cat ap", 6);
        assert_eq!(tok, "ap");
        assert_eq!(start, 4);
    }

    #[test]
    fn token_after_space_is_empty() {
        let (start, tok) = token_under_cursor("ls ", 3);
        assert_eq!(tok, "");
        assert_eq!(start, 3);
    }

    #[test]
    fn token_at_buffer_start_is_empty() {
        let (start, tok) = token_under_cursor("ls", 0);
        assert_eq!(tok, "");
        assert_eq!(start, 0);
    }

    #[test]
    fn token_handles_multibyte_chars_before_token() {
        // An emoji and an accented char precede the token; byte math must hold.
        // Buffer chars: 'é'(1) ' '(2) '🚀'(3) ' '(4) 's'(5) 'r'(6) 'c'(7)
        let buffer = "é 🚀 src";
        let cursor = 7; // after 'c'
        let (_start, tok) = token_under_cursor(buffer, cursor);
        assert_eq!(tok, "src");
    }

    #[test]
    fn prefix_match_in_cwd() {
        let tmp = TempDir::new();
        tmp.touch("apple");
        tmp.touch("apricot");
        tmp.touch("banana");

        let s = complete_path("cat ap", 6, Some(tmp.path.as_path()), None);
        assert_eq!(texts(&s), vec!["apple", "apricot"]);
    }

    #[test]
    fn subdirectory_token_preserves_dir_portion() {
        let tmp = TempDir::new();
        tmp.mkdir("sub");
        tmp.touch("sub/main.rs");
        tmp.touch("sub/mod.rs");

        let s = complete_path("vim sub/m", 9, Some(tmp.path.as_path()), None);
        assert_eq!(texts(&s), vec!["sub/main.rs", "sub/mod.rs"]);
    }

    #[test]
    fn dirs_first_with_trailing_slash() {
        let tmp = TempDir::new();
        tmp.touch("aaa_file");
        tmp.mkdir("zeta_dir");

        // Both match the empty prefix; the directory sorts first despite the
        // later name, and carries a trailing slash.
        let s = complete_path("ls ", 3, Some(tmp.path.as_path()), None);
        assert_eq!(texts(&s), vec!["zeta_dir/", "aaa_file"]);
        assert!(s[0].is_dir);
        assert!(!s[1].is_dir);
    }

    #[test]
    fn trailing_slash_token_lists_dir() {
        let tmp = TempDir::new();
        tmp.mkdir("sub");
        tmp.touch("sub/one");
        tmp.touch("sub/two");

        let s = complete_path("ls sub/", 7, Some(tmp.path.as_path()), None);
        assert_eq!(texts(&s), vec!["sub/one", "sub/two"]);
    }

    #[test]
    fn dotfiles_excluded_unless_dot_prefix() {
        let tmp = TempDir::new();
        tmp.touch(".hidden");
        tmp.touch("visible");

        // Empty prefix: hidden entry excluded.
        let s = complete_path("ls ", 3, Some(tmp.path.as_path()), None);
        assert_eq!(texts(&s), vec!["visible"]);

        // Dot prefix: hidden entry included.
        let s = complete_path("ls .h", 5, Some(tmp.path.as_path()), None);
        assert_eq!(texts(&s), vec![".hidden"]);
    }

    #[test]
    fn tilde_expansion() {
        let tmp = TempDir::new();
        tmp.touch("somefile");

        // Pass an explicit home so `~` expands to the temp dir without touching
        // the process-global `HOME` (which would race with parallel tests).
        let s = complete_path("cat ~/somef", 11, None, Some(tmp.path.as_path()));

        assert_eq!(texts(&s), vec!["~/somefile"]);
    }

    #[test]
    fn bare_tilde_does_not_panic() {
        // Regression: typing `ls ~` used to panic with
        // "byte index 1 is out of bounds of ``" because the bare `~` lands in
        // `prefix` (not `dir_portion`), leaving `dir_portion` empty and the
        // old `&dir_portion[1..]` slice out of bounds. A bare `~` token must
        // simply return (no panic). With home a normal dir, no home entry
        // starts with the literal `~`, so the result is empty.
        let tmp = TempDir::new();
        let s = complete_path("~", 1, Some(tmp.path.as_path()), Some(tmp.path.as_path()));
        assert!(s.is_empty());
    }

    #[test]
    fn bare_tilde_with_command_does_not_panic() {
        // Same crash repro, with the `~` as the token under the cursor in a
        // real command line (`ls ~`, cursor after the `~`).
        let tmp = TempDir::new();
        let s = complete_path(
            "ls ~",
            4,
            Some(tmp.path.as_path()),
            Some(tmp.path.as_path()),
        );
        assert!(s.is_empty());
    }

    #[test]
    fn tilde_slash_still_lists_home() {
        // Positive check that `~/` expansion still works after the fix.
        let tmp = TempDir::new();
        tmp.touch("homefile");

        // Pass an explicit home; `~` resolves via that, not cwd, so cwd None is
        // fine. No process-global `HOME` mutation, so no parallel-test race.
        let s = complete_path("~/", 2, None, Some(tmp.path.as_path()));

        assert_eq!(texts(&s), vec!["~/homefile"]);
    }

    #[test]
    fn no_cwd_relative_token_is_empty() {
        let s = complete_path("cat ap", 6, None, None);
        assert!(s.is_empty());
    }

    #[test]
    fn nonexistent_directory_is_empty() {
        let tmp = TempDir::new();
        // Scan a subdirectory that does not exist; must not panic.
        let s = complete_path("cat does_not_exist/x", 20, Some(tmp.path.as_path()), None);
        assert!(s.is_empty());
    }

    #[test]
    fn has_path_token_gating() {
        // Empty / whitespace-trailing tokens must not trigger the popup.
        assert!(!has_path_token("", 0));
        assert!(!has_path_token("ls ", 3));
        assert!(!has_path_token("ls", 0));
        // Non-empty tokens (including a trailing-slash dir token) do.
        assert!(has_path_token("cat ap", 6));
        assert!(has_path_token("ls sub/", 7));
        assert!(has_path_token("vim sub/m", 9));
    }

    #[test]
    fn popup_below_normal_placement() {
        // Plenty of room below: no flip, sits at the anchor.
        let l = popup_layout(100.0, 220.0, 200.0, 5, 20.0, 300.0, 1000.0, 800.0, 8.0);
        assert_eq!(l.x, 100.0);
        assert_eq!(l.y, 220.0);
        assert_eq!(l.w, 300.0);
        assert_eq!(l.h, 100.0);
        assert!(!l.flipped_above);
    }

    #[test]
    fn popup_flips_above_on_bottom_overflow() {
        // anchor_below_y + h (760 + 100) overflows screen_h - pad (792), so it
        // flips to sit above: bottom at cursor_top_y (740), top at 740 - 100.
        let l = popup_layout(100.0, 760.0, 740.0, 5, 20.0, 300.0, 1000.0, 800.0, 8.0);
        assert!(l.flipped_above);
        assert_eq!(l.y, 640.0);
    }

    #[test]
    fn popup_clamps_right_edge() {
        // anchor_x + box_w (900 + 300) overflows; x clamps so right edge sits
        // pad inside the screen: 1000 - 8 - 300 = 692.
        let l = popup_layout(900.0, 220.0, 200.0, 3, 20.0, 300.0, 1000.0, 800.0, 8.0);
        assert_eq!(l.x, 692.0);
    }

    #[test]
    fn popup_clamps_to_left_and_top() {
        // A negative anchor_x clamps to the left pad; a huge popup that can't
        // fit above clamps its top to the top pad.
        let l = popup_layout(-50.0, 760.0, 100.0, 20, 20.0, 300.0, 1000.0, 800.0, 8.0);
        assert_eq!(l.x, 8.0);
        assert_eq!(l.y, 8.0);
    }

    #[test]
    fn max_suggestions_cap() {
        let tmp = TempDir::new();
        for i in 0..(MAX_SUGGESTIONS + 10) {
            tmp.touch(&format!("file_{i:04}"));
        }
        let s = complete_path("ls file_", 8, Some(tmp.path.as_path()), None);
        assert_eq!(s.len(), MAX_SUGGESTIONS);
    }

    /// Build a token (non-whole-line) suggestion for accept_suffix tests.
    fn tok(text: &str) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            is_dir: text.ends_with('/'),
            whole_line: false,
        }
    }

    /// Build a whole-line (history) suggestion.
    fn line(text: &str) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            is_dir: false,
            whole_line: true,
        }
    }

    #[test]
    fn accept_suffix_extends_partial_token() {
        // Token `sr` (cursor at end), suggestion `src/`: suffix is `c/`.
        assert_eq!(accept_suffix("sr", 2, &tok("src/")), Some("c/"));
    }

    #[test]
    fn accept_suffix_completed_dir_token() {
        // Token `src/` fully matched; suffix is the entry name.
        assert_eq!(
            accept_suffix("src/", 4, &tok("src/main.rs")),
            Some("main.rs")
        );
    }

    #[test]
    fn accept_suffix_mismatch_returns_none() {
        // Suggestion doesn't extend the typed token: no bytes to write.
        assert_eq!(accept_suffix("x", 1, &tok("src/")), None);
    }

    #[test]
    fn accept_suffix_multibyte_round_trip() {
        // A multibyte token followed by an entry name; suffix math must hold.
        // Token is `café` (cursor after the accented char run).
        let buffer = "ls café";
        let cursor = 7; // chars: l s ' ' c a f é -> 7 code points
        assert_eq!(
            accept_suffix(buffer, cursor, &tok("café_dir/")),
            Some("_dir/")
        );
    }

    #[test]
    fn accept_suffix_whole_line_uses_full_buffer() {
        // A whole-line history suggestion: typed part is the buffer up to the
        // cursor (the whole line `git pu`), not the token under it.
        let sug = line("git push origin main");
        assert_eq!(accept_suffix("git pu", 6, &sug), Some("sh origin main"));
    }

    #[test]
    fn accept_suffix_whole_line_mismatch_none() {
        // History line doesn't extend what's typed.
        let sug = line("git push");
        assert_eq!(accept_suffix("ls fo", 5, &sug), None);
    }

    #[test]
    fn visible_window_already_visible_unchanged() {
        // selected in [start, start+max): no scroll.
        assert_eq!(visible_window_start(3, 2, 10), 2);
        assert_eq!(visible_window_start(2, 2, 10), 2);
    }

    #[test]
    fn visible_window_scrolls_up_to_selected() {
        // selected above the window: start moves to selected.
        assert_eq!(visible_window_start(1, 5, 10), 1);
    }

    #[test]
    fn visible_window_scrolls_down_to_last_row() {
        // selected at/below start+max: scroll so selected is the last visible row.
        // start=0, max=10, selected=10 -> start = 10 + 1 - 10 = 1.
        assert_eq!(visible_window_start(10, 0, 10), 1);
        assert_eq!(visible_window_start(12, 0, 10), 3);
    }

    #[test]
    fn visible_window_zero_max_is_noop() {
        assert_eq!(visible_window_start(5, 2, 0), 2);
    }

    // ---- command completion (slice K15) ----

    /// Build an `OsString` PATH from temp dirs for `complete_command` tests.
    fn path_of(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs.iter().map(|d| d.as_os_str())).unwrap()
    }

    #[test]
    fn complete_command_matches_executable_excludes_others() {
        let tmp = TempDir::new();
        // `gizmo` is executable and matches; `ginormous` matches the prefix but
        // is not executable (mode 0o644).
        tmp.touch_mode("gizmo", 0o755);
        tmp.touch_mode("ginormous", 0o644);
        // A subdirectory matching the prefix is excluded even with the exec bit.
        tmp.mkdir("gitdir");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                tmp.path.join("gitdir"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let p = path_of(&[tmp.path.as_path()]);
        let s = complete_command("gi", Some(p.as_os_str()));
        assert_eq!(texts(&s), vec!["gizmo "]);
        assert!(!s[0].is_dir);
    }

    #[test]
    fn complete_command_dedups_by_path_order() {
        let dir1 = TempDir::new();
        let dir2 = TempDir::new();
        dir1.touch_mode("dup", 0o755);
        dir2.touch_mode("dup", 0o755);

        // dir1 first: its `dup` wins, exactly one result.
        let p = path_of(&[dir1.path.as_path(), dir2.path.as_path()]);
        let s = complete_command("dup", Some(p.as_os_str()));
        assert_eq!(texts(&s), vec!["dup "]);

        // Reverse order: still exactly one (independence of which copy is kept).
        let p = path_of(&[dir2.path.as_path(), dir1.path.as_path()]);
        let s = complete_command("dup", Some(p.as_os_str()));
        assert_eq!(texts(&s), vec!["dup "]);
    }

    #[test]
    fn complete_command_prefix_filters() {
        let tmp = TempDir::new();
        tmp.touch_mode("alpha", 0o755);
        tmp.touch_mode("alps", 0o755);
        tmp.touch_mode("beta", 0o755);

        let p = path_of(&[tmp.path.as_path()]);
        let s = complete_command("al", Some(p.as_os_str()));
        assert_eq!(texts(&s), vec!["alpha ", "alps "]);
    }

    #[test]
    fn complete_command_nonexistent_dir_skipped_and_none_empty() {
        let tmp = TempDir::new();
        tmp.touch_mode("run", 0o755);
        let missing = tmp.path.join("does_not_exist");

        // A nonexistent dir in PATH is skipped without panic.
        let p = path_of(&[missing.as_path(), tmp.path.as_path()]);
        let s = complete_command("ru", Some(p.as_os_str()));
        assert_eq!(texts(&s), vec!["run "]);

        // path: None -> empty.
        assert!(complete_command("ru", None).is_empty());
    }

    #[test]
    fn complete_command_caps_at_max() {
        let tmp = TempDir::new();
        for i in 0..(MAX_SUGGESTIONS + 10) {
            tmp.touch_mode(&format!("cmd_{i:04}"), 0o755);
        }
        let p = path_of(&[tmp.path.as_path()]);
        let s = complete_command("cmd_", Some(p.as_os_str()));
        assert_eq!(s.len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn is_command_position_cases() {
        // `"gi"`: token at start -> true.
        let (start, _) = token_under_cursor("gi", 2);
        assert!(is_command_position("gi", start));
        // `"  gi"`: leading spaces, token_start at the 'g' -> true.
        let (start, tok) = token_under_cursor("  gi", 4);
        assert_eq!(tok, "gi");
        assert!(is_command_position("  gi", start));
        // `"ls foo"`: token_start at `foo` -> false (a command precedes it).
        let (start, tok) = token_under_cursor("ls foo", 6);
        assert_eq!(tok, "foo");
        assert!(!is_command_position("ls foo", start));
    }

    #[test]
    fn complete_dispatches_command_for_bare_first_token() {
        let bindir = TempDir::new();
        bindir.touch_mode("yutanitest", 0o755);
        let p = path_of(&[bindir.path.as_path()]);

        // Bare command-position token -> command completion (trailing space).
        let s = complete("yutani", 6, None, Some(p.as_os_str()), None, &[], false);
        assert_eq!(texts(&s), vec!["yutanitest "]);
    }

    #[test]
    fn complete_dispatches_path_for_argument() {
        let cwd = TempDir::new();
        cwd.mkdir("src");
        // `ls sr` — `sr` is an argument; path completion against cwd finds src/.
        let s = complete("ls sr", 5, Some(cwd.path.as_path()), None, None, &[], false);
        assert_eq!(texts(&s), vec!["src/"]);
    }

    #[test]
    fn complete_path_like_first_word_goes_to_path() {
        let cwd = TempDir::new();
        cwd.touch("script.sh");
        cwd.mkdir("bin");
        cwd.touch("bin/x");

        // `./sc` in first position -> path completion (not command).
        let s = complete("./sc", 4, Some(cwd.path.as_path()), None, None, &[], false);
        assert_eq!(texts(&s), vec!["./script.sh"]);

        // `~/x` in first position -> path completion via the supplied home (not
        // command). The explicit `home` arg avoids mutating the process-global
        // `HOME`, which would race with parallel tests.
        let s = complete("~/b", 3, None, None, Some(cwd.path.as_path()), &[], false);
        assert_eq!(texts(&s), vec!["~/bin/"]);
    }

    // ---- history completion (slice K16) ----

    #[test]
    fn parse_zsh_history_extended_and_plain() {
        let contents = ": 1700000000:0;git status\nls -la\n: 1700000005:2;cargo build\n";
        let cmds = parse_zsh_history(contents);
        assert_eq!(cmds, vec!["git status", "ls -la", "cargo build"]);
    }

    #[test]
    fn parse_zsh_history_joins_continued_lines() {
        // A `\`-continued multi-line entry is joined with a newline.
        let contents = ": 1700000000:0;echo one\\\ntwo\nls\n";
        let cmds = parse_zsh_history(contents);
        assert_eq!(cmds, vec!["echo one\ntwo", "ls"]);
    }

    #[test]
    fn parse_zsh_history_skips_blank_entries() {
        let contents = "ls\n\n: 1700000000:0;\ncargo test\n";
        let cmds = parse_zsh_history(contents);
        // The empty plain line and the empty extended command are skipped.
        assert_eq!(cmds, vec!["ls", "cargo test"]);
    }

    #[test]
    fn history_matches_filters_by_prefix() {
        let h = vec![
            "git push".to_string(),
            "git pull".to_string(),
            "ls -la".to_string(),
        ];
        let s = history_matches(&h, "git p");
        assert_eq!(texts(&s), vec!["git push", "git pull"]);
        assert!(s.iter().all(|x| x.whole_line && !x.is_dir));
    }

    #[test]
    fn history_matches_excludes_exact_equal() {
        let h = vec!["git push".to_string(), "git pull".to_string()];
        // `git push` equals the prefix exactly: nothing to add, excluded.
        let s = history_matches(&h, "git push");
        assert!(s.is_empty());
    }

    #[test]
    fn history_matches_preserves_given_order() {
        // Order is preserved as given (most-recent-first per the contract).
        let h = vec![
            "cargo test".to_string(),
            "cargo build".to_string(),
            "cargo check".to_string(),
        ];
        let s = history_matches(&h, "cargo ");
        assert_eq!(texts(&s), vec!["cargo test", "cargo build", "cargo check"]);
    }

    #[test]
    fn history_matches_empty_prefix_is_empty() {
        let h = vec!["ls".to_string()];
        assert!(history_matches(&h, "").is_empty());
    }

    #[test]
    fn history_matches_caps_at_max() {
        let h: Vec<String> = (0..(MAX_SUGGESTIONS + 10))
            .map(|i| format!("run cmd_{i:04}"))
            .collect();
        let s = history_matches(&h, "run ");
        assert_eq!(s.len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn complete_merges_history_ahead_of_tokens() {
        let bindir = TempDir::new();
        bindir.touch_mode("gitfoo", 0o755);
        let p = path_of(&[bindir.path.as_path()]);
        let h = vec!["git status".to_string()];

        // `git` (cursor at end): history line `git status` (whole-line) comes
        // first, then the command-completion token `gitfoo `.
        let s = complete("git", 3, None, Some(p.as_os_str()), None, &h, false);
        assert_eq!(texts(&s), vec!["git status", "gitfoo "]);
        assert!(s[0].whole_line);
        assert!(!s[1].whole_line);
    }

    #[test]
    fn complete_empty_token_no_history_is_empty() {
        // Empty token, non-manual, no history: nothing (no cwd dump).
        let cwd = TempDir::new();
        cwd.touch("file");
        let s = complete("ls ", 3, Some(cwd.path.as_path()), None, None, &[], false);
        assert!(s.is_empty());
    }

    #[test]
    fn complete_manual_empty_token_lists_cwd() {
        // Manual trigger on an empty token still lists the cwd.
        let cwd = TempDir::new();
        cwd.touch("file");
        let s = complete("ls ", 3, Some(cwd.path.as_path()), None, None, &[], true);
        assert_eq!(texts(&s), vec!["file"]);
    }

    #[test]
    fn complete_dedups_history_over_token() {
        // A history line equal to a token suggestion's text appears once
        // (history wins, keeping whole_line).
        let bindir = TempDir::new();
        bindir.touch_mode("deploy", 0o755);
        let p = path_of(&[bindir.path.as_path()]);
        // History entry text exactly matches the command suggestion `deploy `.
        let h = vec!["deploy ".to_string()];
        let s = complete("deploy", 6, None, Some(p.as_os_str()), None, &h, false);
        assert_eq!(texts(&s), vec!["deploy "]);
        assert!(s[0].whole_line);
    }
}
