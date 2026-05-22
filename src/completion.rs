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
}

/// Maximum candidates returned, to bound directory-scan cost and popup size.
pub const MAX_SUGGESTIONS: usize = 50;

/// Compute filesystem path completions for the token under the cursor.
///
/// `buffer` is the full edit line; `cursor` is a *character* (code-point)
/// offset into it (the units `Terminal::current_input()` reports). `cwd` is
/// the shell's working directory used to resolve relative tokens; when `None`,
/// only absolute and `~` tokens can resolve (relative tokens yield no results).
///
/// Returns up to [`MAX_SUGGESTIONS`] entries, directories first then files,
/// each sorted lexicographically. Returns empty when there is no token, the
/// target directory can't be read, or nothing matches.
pub fn complete_path(buffer: &str, cursor: usize, cwd: Option<&Path>) -> Vec<Suggestion> {
    let (_token_start, token) = token_under_cursor(buffer, cursor);

    let Some((scan_dir, file_prefix, dir_portion)) = split_path(token, cwd) else {
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
        });
    }

    // Directories first, then lexicographic by replacement text.
    suggestions.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.text.cmp(&b.text)));
    suggestions.truncate(MAX_SUGGESTIONS);
    suggestions
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

/// Split a token into the directory to scan, the filename prefix to match, and
/// the directory portion of the token (the part up to and including the last
/// `/`, or empty) used to rebuild [`Suggestion::text`] so the replacement
/// preserves what the user already typed.
///
/// Returns `None` when the token can't resolve to a scannable directory: a
/// `~`-token with no `HOME`, or a relative token with no `cwd`.
fn split_path<'a>(token: &'a str, cwd: Option<&Path>) -> Option<(PathBuf, &'a str, &'a str)> {
    // The directory portion is the token up to and including its last `/`.
    let (dir_portion, prefix) = match token.rfind('/') {
        Some(i) => (&token[..=i], &token[i + 1..]),
        None => ("", token),
    };

    if token.starts_with('~') {
        // Expand a leading `~` / `~/...` against HOME. Without HOME, a
        // `~`-token can't resolve.
        let home = std::env::var_os("HOME")?;
        let home = PathBuf::from(home);
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

        let s = complete_path("cat ap", 6, Some(tmp.path.as_path()));
        assert_eq!(texts(&s), vec!["apple", "apricot"]);
    }

    #[test]
    fn subdirectory_token_preserves_dir_portion() {
        let tmp = TempDir::new();
        tmp.mkdir("sub");
        tmp.touch("sub/main.rs");
        tmp.touch("sub/mod.rs");

        let s = complete_path("vim sub/m", 9, Some(tmp.path.as_path()));
        assert_eq!(texts(&s), vec!["sub/main.rs", "sub/mod.rs"]);
    }

    #[test]
    fn dirs_first_with_trailing_slash() {
        let tmp = TempDir::new();
        tmp.touch("aaa_file");
        tmp.mkdir("zeta_dir");

        // Both match the empty prefix; the directory sorts first despite the
        // later name, and carries a trailing slash.
        let s = complete_path("ls ", 3, Some(tmp.path.as_path()));
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

        let s = complete_path("ls sub/", 7, Some(tmp.path.as_path()));
        assert_eq!(texts(&s), vec!["sub/one", "sub/two"]);
    }

    #[test]
    fn dotfiles_excluded_unless_dot_prefix() {
        let tmp = TempDir::new();
        tmp.touch(".hidden");
        tmp.touch("visible");

        // Empty prefix: hidden entry excluded.
        let s = complete_path("ls ", 3, Some(tmp.path.as_path()));
        assert_eq!(texts(&s), vec!["visible"]);

        // Dot prefix: hidden entry included.
        let s = complete_path("ls .h", 5, Some(tmp.path.as_path()));
        assert_eq!(texts(&s), vec![".hidden"]);
    }

    #[test]
    fn tilde_expansion() {
        let tmp = TempDir::new();
        tmp.touch("somefile");

        // Save/restore HOME around the expansion so other tests are unaffected.
        let saved = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp.path);
        let s = complete_path("cat ~/somef", 11, None);
        match saved {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(texts(&s), vec!["~/somefile"]);
    }

    #[test]
    fn bare_tilde_does_not_panic() {
        // Regression: typing `ls ~` used to panic with
        // "byte index 1 is out of bounds of ``" because the bare `~` lands in
        // `prefix` (not `dir_portion`), leaving `dir_portion` empty and the
        // old `&dir_portion[1..]` slice out of bounds. A bare `~` token must
        // simply return (no panic). With HOME a normal dir, no home entry
        // starts with the literal `~`, so the result is empty.
        let tmp = TempDir::new();
        let s = complete_path("~", 1, Some(tmp.path.as_path()));
        assert!(s.is_empty());
    }

    #[test]
    fn bare_tilde_with_command_does_not_panic() {
        // Same crash repro, with the `~` as the token under the cursor in a
        // real command line (`ls ~`, cursor after the `~`).
        let tmp = TempDir::new();
        let s = complete_path("ls ~", 4, Some(tmp.path.as_path()));
        assert!(s.is_empty());
    }

    #[test]
    fn tilde_slash_still_lists_home() {
        // Positive check that `~/` expansion still works after the fix.
        let tmp = TempDir::new();
        tmp.touch("homefile");

        // Save/restore HOME around the expansion so other tests are unaffected.
        let saved = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp.path);
        // cwd None is fine: `~` resolves via HOME, not cwd.
        let s = complete_path("~/", 2, None);
        match saved {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(texts(&s), vec!["~/homefile"]);
    }

    #[test]
    fn no_cwd_relative_token_is_empty() {
        let s = complete_path("cat ap", 6, None);
        assert!(s.is_empty());
    }

    #[test]
    fn nonexistent_directory_is_empty() {
        let tmp = TempDir::new();
        // Scan a subdirectory that does not exist; must not panic.
        let s = complete_path("cat does_not_exist/x", 20, Some(tmp.path.as_path()));
        assert!(s.is_empty());
    }

    #[test]
    fn max_suggestions_cap() {
        let tmp = TempDir::new();
        for i in 0..(MAX_SUGGESTIONS + 10) {
            tmp.touch(&format!("file_{i:04}"));
        }
        let s = complete_path("ls file_", 8, Some(tmp.path.as_path()));
        assert_eq!(s.len(), MAX_SUGGESTIONS);
    }
}
