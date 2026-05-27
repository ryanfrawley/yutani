//! First-run onboarding: a thin console program that runs *as the PTY child*
//! the first time Yutani launches (see the `--onboard` branch in `main` and the
//! `ChildProgram::Onboard` fork in `pty`). It is rendered natively by the very
//! terminal it's configuring, asks a handful of single-keypress questions, and
//! drives a **live preview** of the user's color-scheme / glow choices by
//! emitting the yutani-private `OSC 2125` control sequence back to the host.
//!
//! When the user finishes, it writes a starter `~/.config/yutani/config.toml`,
//! stamps the onboarding marker, asks the host to reload from disk, and then
//! execs the user's shell *in-place* — so the same PTY flows straight from
//! setup into a normal shell with no second window or respawn. Quitting early
//! (`q` / Ctrl-C) writes nothing and just launches the shell, so onboarding
//! reappears next time.
//!
//! The forked child left the slave PTY in raw mode (no echo, no canonical line
//! editing), which is exactly what we want: we read one byte at a time and
//! render the UI ourselves, so the flow is keypress-driven with no Enter
//! needed to change a selection.

use std::io::{Read, Write};

use crate::CrtLevel;

/// Clear screen + home cursor. Raw mode means we own the screen for the
/// duration of setup; we repaint the whole question block on each keypress.
const CLEAR: &str = "\x1b[2J\x1b[H";

const BANNER: &str = "\x1b[1m  Welcome to Yutani\x1b[0m\r\n  \x1b[2mLet's set up a few things. Your choices preview live.\x1b[0m\r\n";

const FOOTER: &str = "\r\n  \x1b[2m[↑/↓] move · [1-9] jump · [Enter] confirm · [q] skip setup\x1b[0m\r\n";

/// A decoded keypress from the raw-mode PTY. Higher-level than a byte so the
/// question loop doesn't have to know about escape-sequence shapes.
enum Key {
    Up,
    Down,
    /// A 1-based option number (`1`–`9`).
    Digit(usize),
    Confirm,
    /// EOF / Ctrl-C / `q` — skip the rest of setup.
    Quit,
    /// Anything we don't act on; the loop just repaints and waits.
    Other,
}

/// Run the onboarding flow. Never returns: it execs the user's shell in-place
/// whether the user completes setup or skips it.
pub fn run() -> ! {
    let font_size = match ask_font_size() {
        Some(f) => f,
        None => skip(),
    };
    let scheme = match ask_scheme() {
        Some(s) => s,
        None => skip(),
    };
    let crt = match ask_crt() {
        Some(c) => c,
        None => skip(),
    };
    let autocomplete = match ask_autocomplete() {
        Some(b) => b,
        None => skip(),
    };
    // Offer Touch ID for sudo only when it can actually be turned on (macOS,
    // module present, sudo includes sudo_local). Skipped silently otherwise so
    // setup stays the same length everywhere.
    if matches!(crate::touchid::status(), crate::touchid::Status::Disabled) {
        match ask_touch_id() {
            // The native authorization dialog (which itself supports Touch ID)
            // is the user's feedback; a failure only lands in the log, since
            // `finish` repaints over this screen immediately.
            Some(true) => {
                if let crate::touchid::Outcome::Failed(e) = crate::touchid::enable() {
                    eprintln!("onboarding: enabling Touch ID for sudo failed: {e}");
                }
            }
            Some(false) => {}
            None => skip(),
        }
    }
    finish(scheme, crt, font_size, autocomplete)
}

/// Question 2: color scheme. Offers the built-in default plus any schemes the
/// user already has under `~/.config/yutani/schemes/`. Returns the chosen
/// scheme name (`None` = built-in default), or `None`-via-skip handled by caller.
fn ask_scheme() -> Option<Option<String>> {
    let schemes = discover_schemes();
    // labels[0] / payloads[0] is always the built-in default.
    let mut labels = vec!["Built-in default".to_string()];
    let mut payloads = vec!["scheme;-".to_string()];
    for name in &schemes {
        labels.push(name.clone());
        payloads.push(format!("scheme;{name}"));
    }
    let pick = ask(
        "  Color scheme",
        &labels,
        0,
        |i| osc(&payloads[i]),
    )?;
    Some(if pick == 0 {
        None
    } else {
        Some(schemes[pick - 1].clone())
    })
}

/// Question 3: CRT effect. One dial for the retro-monitor look — bloom and
/// scanlines move together. Defaults to Off.
fn ask_crt() -> Option<CrtLevel> {
    let labels: Vec<String> = ["Off", "Low", "High"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let payloads = ["crt;off", "crt;low", "crt;high"];
    let levels = [CrtLevel::Off, CrtLevel::Low, CrtLevel::High];
    let pick = ask("  CRT effect (bloom + scanlines)", &labels, 0, |i| osc(payloads[i]))?;
    Some(levels[pick])
}

/// Question 1: font size. Previews live — the cell grid reflows under the
/// onboarding screen, which repaints to fit on the next keypress.
fn ask_font_size() -> Option<f32> {
    let labels: Vec<String> = [
        "9 — small",
        "10 — default (recommended)",
        "12 — large",
        "14 — extra large",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let sizes = [9.0_f32, 10.0, 12.0, 14.0];
    let pick = ask("  Font size", &labels, 1, |i| osc(&format!("font;{}", sizes[i])))?;
    Some(sizes[pick])
}

/// Question 4: autocomplete popup. No live preview — the filesystem/history
/// suggestions only appear while typing at a shell prompt, which doesn't exist
/// during setup, so this just records the choice. Defaults to On.
fn ask_autocomplete() -> Option<bool> {
    let labels: Vec<String> = ["On (recommended)", "Off"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let pick = ask("  Autocomplete (as-you-type suggestions)", &labels, 0, |_| {})?;
    Some(pick == 0)
}

/// Optional final step: offer Touch ID for `sudo`. No live preview — picking
/// "Yes" triggers a native authorization dialog (handled by the caller), not a
/// terminal change. Defaults to On. Returns the choice, or `None` on skip.
fn ask_touch_id() -> Option<bool> {
    let labels: Vec<String> = ["Yes, enable it (recommended)", "Not now"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let pick = ask(
        "  Use Touch ID for sudo?  \x1b[2m(authenticate sudo with your fingerprint)\x1b[0m",
        &labels,
        0,
        |_| {},
    )?;
    Some(pick == 0)
}

/// Render a single-choice question and drive it from keypresses. The current
/// selection is highlighted; arrows / `j` / `k` move it and a digit jumps to an
/// option, each firing `preview` so the host updates live; Enter confirms.
/// Returns the chosen index, or `None` if the user pressed `q` / Ctrl-C / hit
/// EOF (skip setup).
fn ask(question: &str, labels: &[String], default_ix: usize, mut preview: impl FnMut(usize)) -> Option<usize> {
    let mut cur = default_ix.min(labels.len().saturating_sub(1));
    preview(cur);
    loop {
        let mut s = String::new();
        s.push_str(CLEAR);
        s.push_str(BANNER);
        s.push_str("\r\n\x1b[1m");
        s.push_str(question);
        s.push_str("\x1b[0m\r\n\r\n");
        for (i, label) in labels.iter().enumerate() {
            if i == cur {
                // Reverse-video the active row so the live preview is anchored
                // to something visible on screen.
                s.push_str(&format!("    \x1b[7m {}  {} \x1b[0m\r\n", i + 1, label));
            } else {
                s.push_str(&format!("     {}  {}\r\n", i + 1, label));
            }
        }
        s.push_str(FOOTER);
        print!("{s}");
        let _ = std::io::stdout().flush();

        match read_key() {
            Key::Quit => return None,
            Key::Confirm => return Some(cur),
            // Arrows / j / k move the highlight and fire the live preview,
            // wrapping at the ends so a long list is quick to cycle.
            Key::Up => {
                cur = if cur == 0 { labels.len() - 1 } else { cur - 1 };
                preview(cur);
            }
            Key::Down => {
                cur = (cur + 1) % labels.len();
                preview(cur);
            }
            // A digit jumps straight to that option.
            Key::Digit(n) => {
                if n <= labels.len() {
                    cur = n - 1;
                    preview(cur);
                }
            }
            Key::Other => {}
        }
    }
}

/// Commit the chosen settings: write config, stamp the marker, ask the host to
/// reload from disk (so the live look matches exactly what was saved), then exec
/// the shell. Never returns.
fn finish(scheme: Option<String>, crt: CrtLevel, font_size: f32, autocomplete: bool) -> ! {
    let mut cfg = crate::Config::defaults();
    cfg.color_scheme = scheme;
    cfg.font_size = font_size;
    cfg.apply_crt_level(crt);
    cfg.autocomplete = autocomplete;
    cfg.save();
    crate::mark_onboarded();
    // Reload from disk so the running window reflects exactly the persisted
    // config (and not just the transient preview state).
    osc("reload");
    print!("{CLEAR}{BANNER}\r\n  \x1b[1mAll set — launching your shell…\x1b[0m\r\n\r\n");
    let _ = std::io::stdout().flush();
    crate::pty::exec_login_shell(crate::shell_integration::prepare_zdotdir().as_deref());
}

/// Skip path: revert any live preview to the on-disk config and start the shell
/// without writing the marker, so onboarding runs again next launch. Never
/// returns.
fn skip() -> ! {
    osc("reload");
    print!(
        "{CLEAR}{BANNER}\r\n  \x1b[1mSetup skipped.\x1b[0m \x1b[2mRun \"Run first-time setup…\" from the command palette (Cmd-Shift-P) to try again.\x1b[0m\r\n\r\n"
    );
    let _ = std::io::stdout().flush();
    crate::pty::exec_login_shell(crate::shell_integration::prepare_zdotdir().as_deref());
}

/// Discovered scheme names under `~/.config/yutani/schemes/`, sorted, capped at
/// 8 so every option stays a single-digit keypress.
fn discover_schemes() -> Vec<String> {
    let Some(dir) = crate::config_dir().map(|d| d.join("schemes")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                p.file_stem().and_then(|s| s.to_str()).map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    out.sort();
    out.truncate(8);
    out
}

/// Emit a yutani-private `OSC 2125` live-preview control sequence to the host
/// terminal. `payload` is the part after the `2125;` (e.g. `scheme;Solarized`,
/// `glow;subtle`, `reload`). BEL-terminated to match the rest of our OSC output.
fn osc(payload: &str) {
    print!("\x1b]2125;{payload}\x07");
    let _ = std::io::stdout().flush();
}

/// Read a single byte from stdin (the raw-mode PTY slave). `None` on EOF or
/// error, treated by callers as "skip setup".
fn read_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    match std::io::stdin().read(&mut b) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(b[0]),
    }
}

/// Decode one keypress, resolving arrow-key escape sequences. Arrow keys arrive
/// as CSI (`ESC [ A`/`B`) or, defensively, SS3 (`ESC O A`/`B`) — onboarding
/// never enables DECCKM, so the terminal sends the CSI form, but we accept both.
/// `j`/`k` mirror down/up for vim muscle memory.
fn read_key() -> Key {
    match read_byte() {
        None | Some(0x03) | Some(b'q') | Some(b'Q') => Key::Quit,
        Some(b'\r') | Some(b'\n') => Key::Confirm,
        Some(b'k') => Key::Up,
        Some(b'j') => Key::Down,
        Some(c) if c.is_ascii_digit() && c != b'0' => Key::Digit((c - b'0') as usize),
        // Escape introducer: read the CSI/SS3 body and map the final byte.
        Some(0x1b) => match read_byte() {
            Some(b'[') | Some(b'O') => match read_byte() {
                Some(b'A') => Key::Up,
                Some(b'B') => Key::Down,
                _ => Key::Other,
            },
            _ => Key::Other,
        },
        _ => Key::Other,
    }
}
