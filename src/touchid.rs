//! macOS Touch ID for `sudo`.
//!
//! `sudo` authenticates through PAM. Adding an `auth sufficient pam_tid.so`
//! line to its PAM stack makes Touch ID a sufficient factor, so a `sudo` run
//! inside Yutani pops the system Touch ID sheet (falling back to a password if
//! the fingerprint isn't recognised). This is purely a system-config change —
//! Yutani itself does nothing at the PTY level — but doing it for the user
//! means writing a root-owned file under `/etc/pam.d`, which needs an admin
//! authorization. We get that authorization (and the convenience) through
//! Apple's setuid `/usr/libexec/authopen` helper, which presents the native
//! Authorization Services dialog and, on success, hands back a writable fd to
//! the file. That authorization dialog itself supports Touch ID, so on capable
//! Macs the whole flow is touch-only.
//!
//! We deliberately *don't* use `osascript … with administrator privileges`
//! here: despite running as root, that elevation can't create files under
//! `/etc/pam.d` (it returns "Operation not permitted"), whereas `authopen` —
//! like a real `sudo` — can. `authopen -w` truncates and reads the new file
//! contents from stdin, so `enable`/`disable` compute the *complete* desired
//! file (the existing `sudo_local` is world-readable) and write it back.
//!
//! Since macOS Sonoma the supported, upgrade-safe place for this line is
//! `/etc/pam.d/sudo_local` (OS updates reset `/etc/pam.d/sudo` but leave
//! `sudo_local` intact, and `sudo` already `include`s it as its first auth
//! step). We only ever write to `sudo_local`, and only when `sudo` actually
//! includes it — we never touch `/etc/pam.d/sudo` itself.
//!
//! Two surfaces drive this module: the "Touch ID for sudo…" command palette
//! entry (see `command_palette::PaletteAction::ToggleTouchIdSudo`), which runs
//! [`run_palette_command`] and is reversible; and the first-run onboarding
//! step (see `onboard`), which calls [`enable`] directly.

/// Whether Touch ID is currently a `sudo` auth factor — derived from the
/// world-readable PAM files, so no privilege is needed to check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// `pam_tid.so` is active in the sudo PAM stack.
    Enabled,
    /// Not active, but it can be enabled (the module exists and `sudo`
    /// includes `sudo_local`).
    Disabled,
    /// Can't be offered here: not macOS, the `pam_tid` module is missing, or
    /// `sudo` doesn't `include sudo_local` (an unusual/legacy stack we won't
    /// edit blindly).
    Unsupported,
}

/// Result of a privileged enable/disable attempt.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The PAM file now matches the requested state.
    Done,
    /// The user dismissed the authorization dialog (authopen exits non-zero
    /// with no diagnostic). Not an error — callers stay silent.
    Cancelled,
    /// The privileged step ran but failed; the string is for surfacing/logging.
    Failed(String),
}

const SUDO_LOCAL: &str = "/etc/pam.d/sudo_local";
const SUDO: &str = "/etc/pam.d/sudo";
/// The line we add. Whitespace matches Apple's own PAM formatting.
const PAM_LINE: &str = "auth       sufficient     pam_tid.so";

// ---------------------------------------------------------------------------
// Pure parsers — no IO, so they're unit-testable without touching /etc.
// ---------------------------------------------------------------------------

/// Whether a single line is an *active* (non-commented) `auth … pam_tid.so`
/// directive — the thing that makes Touch ID a sudo factor.
fn is_active_pam_tid_line(line: &str) -> bool {
    let t = line.trim_start();
    !t.starts_with('#') && first_word(t) == Some("auth") && t.contains("pam_tid.so")
}

/// Whether `contents` has an *active* (non-commented) `auth … pam_tid.so` line.
fn has_active_pam_tid(contents: &str) -> bool {
    contents.lines().any(is_active_pam_tid_line)
}

/// The `sudo_local` contents that *enable* Touch ID: `contents` with our
/// [`PAM_LINE`] appended, unless an active `pam_tid` line is already present
/// (then it's returned unchanged, so enabling is idempotent). A trailing
/// newline is ensured before appending. `authopen -w` replaces the whole file,
/// so we preserve any existing local config rather than just appending in place.
fn add_pam_tid(contents: &str) -> String {
    if has_active_pam_tid(contents) {
        return contents.to_string();
    }
    let mut out = contents.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(PAM_LINE);
    out.push('\n');
    out
}

/// The `sudo_local` contents that *disable* Touch ID: `contents` with every
/// active `pam_tid` line removed (commented lines and unrelated config are
/// kept). Line endings are normalised to `\n` since we rewrite the whole file.
fn strip_active_pam_tid(contents: &str) -> String {
    contents
        .lines()
        .filter(|line| !is_active_pam_tid_line(line))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Whether `contents` (the `/etc/pam.d/sudo` file) has an active
/// `auth … include … sudo_local` line — the hook our `sudo_local` edit relies
/// on. Tolerant of `include`/`substack` and extra whitespace.
fn includes_sudo_local(contents: &str) -> bool {
    contents.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with('#') || first_word(t) != Some("auth") {
            return false;
        }
        let mut words = t.split_whitespace();
        // auth <control> sudo_local  — control is usually `include`.
        words.any(|w| w == "sudo_local")
    })
}

fn first_word(s: &str) -> Option<&str> {
    s.split_whitespace().next()
}

// ---------------------------------------------------------------------------
// macOS implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Read a PAM file, treating any read error as "no such line".
    fn read(path: &str) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Whether the `pam_tid` module is installed (it ships with macOS; the
    /// version suffix — `pam_tid.so.2` today — can change, so we match the
    /// stem rather than a fixed name).
    fn pam_tid_present() -> bool {
        std::fs::read_dir("/usr/lib/pam")
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("pam_tid.so"))
                })
            })
            .unwrap_or(false)
    }

    pub fn status() -> Status {
        if has_active_pam_tid(&read(SUDO_LOCAL)) || has_active_pam_tid(&read(SUDO)) {
            return Status::Enabled;
        }
        if pam_tid_present() && includes_sudo_local(&read(SUDO)) {
            Status::Disabled
        } else {
            Status::Unsupported
        }
    }

    /// Write `content` as the complete contents of the root-owned `path` via
    /// Apple's setuid `/usr/libexec/authopen` helper. `-c` creates the file if
    /// needed; `-w` opens it for writing (truncating) and reads the new bytes
    /// from our stdin. The helper presents the native, Touch-ID-capable
    /// Authorization Services dialog; we handle no password ourselves.
    ///
    /// We use `authopen` rather than `osascript … with administrator
    /// privileges` because the latter, despite running as root, gets
    /// "Operation not permitted" creating files under `/etc/pam.d`.
    fn authopen_write(path: &str, content: &str) -> Outcome {
        let mut child = match Command::new("/usr/libexec/authopen")
            .args(["-c", "-w", path])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(format!("authopen: {e}")),
        };

        // authopen writes whatever it reads from stdin, up to EOF; dropping the
        // handle after the write closes the pipe and signals EOF.
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(content.as_bytes()) {
                // Reap the child so we don't leak it, then report.
                let _ = child.wait();
                return Outcome::Failed(format!("authopen stdin: {e}"));
            }
        }

        match child.wait_with_output() {
            Ok(out) if out.status.success() => Outcome::Done,
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let trimmed = stderr.trim();
                // authopen exits non-zero with no diagnostic when the user
                // dismisses the auth dialog (errAuthorizationCanceled); a real
                // failure prints a reason. Treat the silent case as a cancel.
                if trimmed.is_empty() {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed(trimmed.to_string())
                }
            }
            Err(e) => Outcome::Failed(format!("authopen: {e}")),
        }
    }

    pub fn enable() -> Outcome {
        let current = read(SUDO_LOCAL);
        // Already enabled: don't bother the user with an auth prompt.
        if has_active_pam_tid(&current) {
            return Outcome::Done;
        }
        authopen_write(SUDO_LOCAL, &add_pam_tid(&current))
    }

    pub fn disable() -> Outcome {
        let current = read(SUDO_LOCAL);
        // Already off (covers a missing/empty file): nothing to authorize.
        if !has_active_pam_tid(&current) {
            return Outcome::Done;
        }
        authopen_write(SUDO_LOCAL, &strip_active_pam_tid(&current))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;
    pub fn status() -> Status {
        Status::Unsupported
    }
    pub fn enable() -> Outcome {
        Outcome::Failed("Touch ID for sudo is only available on macOS".into())
    }
    pub fn disable() -> Outcome {
        Outcome::Failed("Touch ID for sudo is only available on macOS".into())
    }
}

pub use imp::{disable, enable, status};

// ---------------------------------------------------------------------------
// Native palette flow (macOS) — opt-in, explained, reversible.
// ---------------------------------------------------------------------------

/// Drive the "Touch ID for sudo…" command: show a native alert describing the
/// current state, and on confirmation run the privileged enable/disable. Runs
/// entirely on the main thread (the palette dispatch is on the event loop), so
/// `NSAlert.runModal` is safe here. No-op off macOS (the command is hidden
/// there anyway).
#[cfg(target_os = "macos")]
pub fn run_palette_command() {
    match status() {
        Status::Disabled => {
            let go = alert(
                "Enable Touch ID for sudo?",
                "Yutani will add a line to /etc/pam.d/sudo_local so sudo accepts your \
                 fingerprint. You'll be asked to authorize this change once. It only affects \
                 sudo, takes effect immediately, and can be turned off again from this same \
                 command.",
                &["Enable", "Cancel"],
            );
            if go == 0 {
                report(enable(), "Touch ID is now enabled for sudo.");
            }
        }
        Status::Enabled => {
            let go = alert(
                "Touch ID for sudo is on",
                "sudo currently accepts your fingerprint. You can turn this off — sudo will \
                 go back to asking for your password.",
                &["Turn Off", "Done"],
            );
            if go == 0 {
                report(disable(), "Touch ID for sudo has been turned off.");
            }
        }
        Status::Unsupported => {
            alert(
                "Touch ID for sudo isn't available",
                "This Mac's sudo configuration can't be set up for Touch ID automatically \
                 (the pam_tid module is missing or sudo doesn't include sudo_local).",
                &["OK"],
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn run_palette_command() {}

/// Show the outcome of a privileged step. Success and failure get an alert;
/// a cancellation is silent (the user already dismissed a dialog).
#[cfg(target_os = "macos")]
fn report(outcome: Outcome, success_msg: &str) {
    match outcome {
        Outcome::Done => {
            alert("Done", success_msg, &["OK"]);
        }
        Outcome::Cancelled => {}
        Outcome::Failed(e) => {
            alert(
                "Couldn't update sudo settings",
                &format!("The change didn't go through.\n\n{e}"),
                &["OK"],
            );
        }
    }
}

/// Minimal `NSAlert` wrapper mirroring `confirm_close_running_command` in
/// `main.rs`: add buttons left-to-right (first is default), run modally, and
/// return the 0-based index of the button pressed.
#[cfg(target_os = "macos")]
fn alert(message: &str, informative: &str, buttons: &[&str]) -> usize {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    unsafe {
        let alert: Retained<AnyObject> = msg_send![class!(NSAlert), new];
        let msg = NSString::from_str(message);
        let info = NSString::from_str(informative);
        let _: () = msg_send![&*alert, setMessageText: &*msg];
        let _: () = msg_send![&*alert, setInformativeText: &*info];
        // NSAlertStyleInformational.
        let _: () = msg_send![&*alert, setAlertStyle: 1usize];
        for title in buttons {
            let t = NSString::from_str(title);
            let _: *mut AnyObject = msg_send![&*alert, addButtonWithTitle: &*t];
        }
        let response: isize = msg_send![&*alert, runModal];
        // NSAlertFirstButtonReturn == 1000, second 1001, …
        (response - 1000).max(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_active_pam_tid_line() {
        assert!(has_active_pam_tid("auth       sufficient     pam_tid.so\n"));
        assert!(has_active_pam_tid("# header\nauth   sufficient   pam_tid.so   # touch id\n"));
    }

    #[test]
    fn ignores_commented_or_unrelated_lines() {
        assert!(!has_active_pam_tid("# auth sufficient pam_tid.so\n"));
        assert!(!has_active_pam_tid("   #auth sufficient pam_tid.so\n"));
        assert!(!has_active_pam_tid("auth sufficient pam_opendirectory.so\n"));
        assert!(!has_active_pam_tid(""));
        // "pam_tid.so" appearing on a non-auth line shouldn't count.
        assert!(!has_active_pam_tid("account required pam_tid.so\n"));
    }

    #[test]
    fn detects_sudo_local_include() {
        assert!(includes_sudo_local("auth       include        sudo_local\n"));
        assert!(includes_sudo_local(
            "# sudo\nauth include sudo_local\nauth required pam_opendirectory.so\n"
        ));
    }

    #[test]
    fn missing_or_commented_include_is_not_detected() {
        assert!(!includes_sudo_local("# auth include sudo_local\n"));
        assert!(!includes_sudo_local("auth required pam_opendirectory.so\n"));
        assert!(!includes_sudo_local(""));
        // sudo_local mentioned on a non-auth line doesn't count.
        assert!(!includes_sudo_local("session optional sudo_local\n"));
    }

    // ----- has_active_pam_tid: whitespace, ordering, and the exact line -----

    #[test]
    fn detects_pam_tid_with_tab_separated_fields() {
        // Apple's own files mix tabs and spaces; the parser keys off whitespace
        // splitting, so a tab-delimited line must still be detected.
        assert!(has_active_pam_tid("auth\tsufficient\tpam_tid.so\n"));
        assert!(has_active_pam_tid(
            "auth\t   sufficient \t  pam_tid.so\t\n"
        ));
    }

    #[test]
    fn detects_pam_tid_with_leading_whitespace() {
        // Indented (space- or tab-led) active lines still count — only a leading
        // `#` comments a line out.
        assert!(has_active_pam_tid("    auth sufficient pam_tid.so\n"));
        assert!(has_active_pam_tid("\t auth sufficient pam_tid.so\n"));
    }

    #[test]
    fn detects_the_exact_line_we_write() {
        // The constant we append must be recognised by the detector that decides
        // whether enabling is needed; otherwise enable() wouldn't be idempotent
        // from status()'s point of view.
        assert!(has_active_pam_tid(PAM_LINE));
        assert!(has_active_pam_tid(&format!("{PAM_LINE}\n")));
    }

    #[test]
    fn detects_pam_tid_among_multiple_lines_and_versions() {
        // A realistic multi-line stack with other modules before/after.
        let contents = "# sudo_local: local config\n\
                        auth       sufficient     pam_smartcard.so\n\
                        auth       sufficient     pam_tid.so\n\
                        auth       required        pam_opendirectory.so\n";
        assert!(has_active_pam_tid(contents));
        // The versioned module name (pam_tid.so.2) still contains the stem.
        assert!(has_active_pam_tid("auth sufficient pam_tid.so.2\n"));
    }

    #[test]
    fn one_active_pam_tid_line_among_commented_duplicates_counts() {
        // Several pam_tid lines, only one uncommented — still enabled.
        let contents = "# auth sufficient pam_tid.so\n\
                        #auth sufficient pam_tid.so\n\
                        auth       sufficient     pam_tid.so\n";
        assert!(has_active_pam_tid(contents));
    }

    #[test]
    fn all_commented_pam_tid_lines_are_not_active() {
        // Every pam_tid line is commented (with assorted leading whitespace), so
        // the feature is not active.
        let contents = "# auth sufficient pam_tid.so\n\
                        \t# auth sufficient pam_tid.so\n\
                           #auth sufficient pam_tid.so\n";
        assert!(!has_active_pam_tid(contents));
    }

    #[test]
    fn pam_tid_substring_in_module_name_does_not_false_positive_on_non_auth() {
        // The module string only counts on an `auth` line; a `session`/`account`
        // line mentioning it must not register as active.
        assert!(!has_active_pam_tid("session optional pam_tid.so\n"));
        assert!(!has_active_pam_tid("account required pam_tid.so\n"));
    }

    #[test]
    fn pam_tid_only_in_comment_token_after_auth_still_counts() {
        // Documented quirk: detection is `first word == auth` AND the line
        // contains the module string anywhere — including a trailing comment.
        // This pins that behaviour so a future refactor notices if it changes.
        assert!(has_active_pam_tid(
            "auth required pam_opendirectory.so # not pam_tid.so really\n"
        ));
    }

    // ----- includes_sudo_local: whitespace, ordering, trailing content -----

    #[test]
    fn detects_sudo_local_include_with_tabs_and_trailing_whitespace() {
        assert!(includes_sudo_local("auth\tinclude\tsudo_local\n"));
        assert!(includes_sudo_local("auth   include   sudo_local   \n"));
    }

    #[test]
    fn detects_sudo_local_with_substack_control() {
        // The doc comment promises tolerance of `substack` as well as `include`.
        assert!(includes_sudo_local("auth substack sudo_local\n"));
    }

    #[test]
    fn detects_sudo_local_with_leading_whitespace() {
        assert!(includes_sudo_local("    auth include sudo_local\n"));
        assert!(includes_sudo_local("\tauth\tinclude\tsudo_local\n"));
    }

    #[test]
    fn sudo_local_must_be_a_whole_word_not_a_substring() {
        // A path/word that merely contains "sudo_local" as a substring must not
        // satisfy the whole-word match (`split_whitespace` + `==`).
        assert!(!includes_sudo_local("auth include sudo_local_extra\n"));
        assert!(!includes_sudo_local("auth include my_sudo_local\n"));
    }

    #[test]
    fn detects_sudo_local_among_a_realistic_sudo_stack() {
        let contents = "# sudo: auth account password session\n\
                        auth       include        sudo_local\n\
                        auth       sufficient     pam_smartcard.so\n\
                        auth       required        pam_opendirectory.so\n\
                        account    required        pam_permit.so\n";
        assert!(includes_sudo_local(contents));
    }

    #[test]
    fn sudo_local_on_a_non_auth_line_is_ignored() {
        // Only the `auth` facility's include hooks our edit; session/account
        // includes of sudo_local don't count.
        assert!(!includes_sudo_local("session include sudo_local\n"));
        assert!(!includes_sudo_local("account include sudo_local\n"));
    }

    // ----- add_pam_tid / strip_active_pam_tid: the content transforms -----

    #[test]
    fn add_pam_tid_appends_the_line_to_an_empty_file() {
        // A fresh/missing sudo_local (empty contents) becomes just our line,
        // with no stray leading newline.
        assert_eq!(add_pam_tid(""), format!("{PAM_LINE}\n"));
    }

    #[test]
    fn add_pam_tid_preserves_existing_content_and_fixes_missing_newline() {
        // Apple's template (comments, no active line) is kept verbatim and our
        // line is appended after a newline is ensured.
        let template = "# sudo_local: local config\n# uncomment to enable\n";
        assert_eq!(add_pam_tid(template), format!("{template}{PAM_LINE}\n"));
        // No trailing newline on the input: one is added before appending.
        assert_eq!(
            add_pam_tid("auth include sudo_local"),
            format!("auth include sudo_local\n{PAM_LINE}\n")
        );
    }

    #[test]
    fn add_pam_tid_is_idempotent_when_already_enabled() {
        // Already-active contents are returned byte-for-byte: enabling twice
        // must not duplicate the line (and lets enable() skip the auth prompt).
        let already = format!("# comment\n{PAM_LINE}\n");
        assert_eq!(add_pam_tid(&already), already);
    }

    #[test]
    fn add_pam_tid_output_is_detected_as_active() {
        // Round-trip: whatever add_pam_tid produces must read back as enabled.
        assert!(has_active_pam_tid(&add_pam_tid("")));
        assert!(has_active_pam_tid(&add_pam_tid("# only comments\n")));
    }

    #[test]
    fn strip_active_pam_tid_removes_only_the_active_line() {
        // The active pam_tid line goes; surrounding config and commented-out
        // pam_tid lines stay. Output reads back as disabled.
        let contents = "# sudo_local\n\
                        auth       sufficient     pam_smartcard.so\n\
                        auth       sufficient     pam_tid.so\n\
                        # auth sufficient pam_tid.so\n";
        let out = strip_active_pam_tid(contents);
        assert_eq!(
            out,
            "# sudo_local\n\
             auth       sufficient     pam_smartcard.so\n\
             # auth sufficient pam_tid.so\n"
        );
        assert!(!has_active_pam_tid(&out));
    }

    #[test]
    fn strip_active_pam_tid_removes_every_active_duplicate() {
        // More than one active line (shouldn't happen, but be safe) are all
        // removed, leaving the file fully disabled.
        let contents = format!("{PAM_LINE}\nauth sufficient pam_tid.so.2\nfoo bar\n");
        let out = strip_active_pam_tid(&contents);
        assert_eq!(out, "foo bar\n");
        assert!(!has_active_pam_tid(&out));
    }

    #[test]
    fn strip_active_pam_tid_leaves_an_already_disabled_file_unchanged() {
        // No active line: every (normalised) line survives.
        let contents = "# auth sufficient pam_tid.so\nauth include sudo_local\n";
        assert_eq!(strip_active_pam_tid(contents), contents);
    }

    #[test]
    fn is_active_pam_tid_line_predicate_classifies_single_lines() {
        // The refactored-out predicate, tested directly: an uncommented `auth`
        // line mentioning the module is active; comments, leading-`#`, and
        // non-`auth` facilities are not.
        assert!(is_active_pam_tid_line("auth sufficient pam_tid.so"));
        assert!(is_active_pam_tid_line(
            "\tauth\tsufficient\tpam_tid.so\t# touch id"
        ));
        assert!(!is_active_pam_tid_line("# auth sufficient pam_tid.so"));
        assert!(!is_active_pam_tid_line("   #auth sufficient pam_tid.so"));
        assert!(!is_active_pam_tid_line("account required pam_tid.so"));
        assert!(!is_active_pam_tid_line("auth sufficient pam_opendirectory.so"));
        assert!(!is_active_pam_tid_line(""));
    }

    #[test]
    fn strip_active_pam_tid_normalises_crlf_line_endings() {
        // The doc promises endings are normalised to `\n` (we rewrite the whole
        // file). CRLF input comes back LF-only, with the active line removed.
        let contents = "# sudo_local\r\nauth sufficient pam_tid.so\r\nfoo bar\r\n";
        let out = strip_active_pam_tid(contents);
        assert_eq!(out, "# sudo_local\nfoo bar\n");
        assert!(!has_active_pam_tid(&out));
    }

    #[test]
    fn strip_active_pam_tid_on_empty_input_yields_empty() {
        // A missing/empty sudo_local stays empty (no stray newline introduced).
        assert_eq!(strip_active_pam_tid(""), "");
    }

    #[test]
    fn strip_active_pam_tid_appends_trailing_newline_to_a_no_newline_file() {
        // `lines()` drops the final missing newline; we re-emit one per surviving
        // line, so a file with no trailing newline gains one (whole-file rewrite).
        assert_eq!(strip_active_pam_tid("foo bar"), "foo bar\n");
    }

    #[test]
    fn add_pam_tid_idempotent_for_a_whitespace_variant_active_line() {
        // Idempotency keys off `has_active_pam_tid`, not an exact `PAM_LINE`
        // match: an existing tab-delimited active line is left untouched even
        // though it differs from the constant we'd otherwise append.
        let already = "auth\tsufficient\tpam_tid.so\n";
        assert_eq!(add_pam_tid(already), already);
    }

    #[test]
    fn enable_then_disable_round_trips_to_disabled() {
        // add_pam_tid and strip_active_pam_tid are inverses with respect to the
        // "is it active?" predicate: enabling then disabling leaves the file
        // not-active again.
        let base = "# sudo_local: local config\nauth include sudo_local\n";
        let enabled = add_pam_tid(base);
        assert!(has_active_pam_tid(&enabled));
        let disabled = strip_active_pam_tid(&enabled);
        assert!(!has_active_pam_tid(&disabled));
    }

    #[test]
    fn disable_then_enable_round_trips_to_enabled() {
        // The other inverse direction: stripping a file that's already off and
        // re-adding leaves it active again (and readable as enabled).
        let base = "# sudo_local: local config\nauth include sudo_local\n";
        let disabled = strip_active_pam_tid(base);
        assert!(!has_active_pam_tid(&disabled));
        let enabled = add_pam_tid(&disabled);
        assert!(has_active_pam_tid(&enabled));
    }

    #[test]
    fn strip_then_add_is_a_stable_fixpoint() {
        // Disabling already-disabled and enabling already-enabled are no-ops at
        // the predicate level, so a second pass changes nothing.
        let enabled = add_pam_tid("auth include sudo_local\n");
        assert_eq!(add_pam_tid(&enabled), enabled);
        let disabled = strip_active_pam_tid(&enabled);
        assert_eq!(strip_active_pam_tid(&disabled), disabled);
    }
}
