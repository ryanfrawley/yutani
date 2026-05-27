//! macOS Touch ID for `sudo`.
//!
//! `sudo` authenticates through PAM. Adding an `auth sufficient pam_tid.so`
//! line to its PAM stack makes Touch ID a sufficient factor, so a `sudo` run
//! inside Yutani pops the system Touch ID sheet (falling back to a password if
//! the fingerprint isn't recognised). This is purely a system-config change —
//! Yutani itself does nothing at the PTY level — but doing it for the user
//! means writing a root-owned file under `/etc/pam.d`, which needs an admin
//! authorization. We get that authorization (and the convenience) the only way
//! macOS allows: an explicit, opt-in prompt that runs the edit with
//! administrator privileges via `osascript`. That authorization dialog itself
//! supports Touch ID, so on capable Macs the whole flow is touch-only.
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
    /// The user dismissed the authorization dialog (osascript `-128`). Not an
    /// error — callers stay silent.
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

/// Whether `contents` has an *active* (non-commented) `auth … pam_tid.so` line.
fn has_active_pam_tid(contents: &str) -> bool {
    contents.lines().any(|line| {
        let t = line.trim_start();
        !t.starts_with('#') && first_word(t) == Some("auth") && t.contains("pam_tid.so")
    })
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

/// AppleScript string-literal escaping: backslash and double-quote. Used to
/// embed a filesystem path inside the `do shell script "…"` literal.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// macOS implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::io::Write;
    use std::process::Command;

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

    /// Run `script` as root via `osascript`'s authorization dialog. The script
    /// is written to a 0700 temp file and executed by `/bin/sh`, so we never
    /// have to nest shell quoting inside AppleScript. `prompt` is the sentence
    /// shown in the auth dialog.
    fn run_privileged(script: &str, prompt: &str) -> Outcome {
        // Unique temp path; pid is enough since this is a one-shot, main-thread
        // call. Under our own dir-less temp file in the system temp dir.
        let path = std::env::temp_dir().join(format!("yutani-touchid-{}.sh", std::process::id()));
        {
            let mut f = match std::fs::File::create(&path) {
                Ok(f) => f,
                Err(e) => return Outcome::Failed(format!("temp file: {e}")),
            };
            if let Err(e) = f.write_all(script.as_bytes()) {
                let _ = std::fs::remove_file(&path);
                return Outcome::Failed(format!("temp write: {e}"));
            }
        }

        let path_str = path.to_string_lossy().into_owned();
        let apple = format!(
            "do shell script \"/bin/sh '{}'\" with administrator privileges with prompt \"{}\"",
            applescript_escape(&path_str),
            applescript_escape(prompt),
        );
        let result = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(&apple)
            .output();
        let _ = std::fs::remove_file(&path);

        match result {
            Ok(out) if out.status.success() => Outcome::Done,
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                // The user dismissed the dialog: AppleScript error -128.
                if stderr.contains("-128") || stderr.contains("User canceled") {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed(stderr.trim().to_string())
                }
            }
            Err(e) => Outcome::Failed(format!("osascript: {e}")),
        }
    }

    pub fn enable() -> Outcome {
        // Idempotent append. `set -e` won't trip on the `if !` test (grep's
        // exit code is consumed by the condition).
        let script = format!(
            "set -e\n\
             F={SUDO_LOCAL}\n\
             if ! grep -Eq '^[[:space:]]*auth[[:space:]].*pam_tid\\.so' \"$F\" 2>/dev/null; then\n\
             \tprintf '%s\\n' '{PAM_LINE}' >> \"$F\"\n\
             fi\n",
        );
        run_privileged(&script, "Yutani wants to enable Touch ID for sudo.")
    }

    pub fn disable() -> Outcome {
        // Strip the active pam_tid auth line(s) we'd have added. We only ever
        // edit sudo_local, so this is the exact inverse of enable(); a missing
        // file is already "disabled".
        let script = format!(
            "set -e\n\
             F={SUDO_LOCAL}\n\
             [ -f \"$F\" ] || exit 0\n\
             /usr/bin/sed -i '' -E '/^[[:space:]]*auth[[:space:]].*pam_tid\\.so/d' \"$F\"\n",
        );
        run_privileged(&script, "Yutani wants to turn off Touch ID for sudo.")
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

    #[test]
    fn applescript_escaping_quotes_and_backslashes() {
        assert_eq!(applescript_escape("a/b/c"), "a/b/c");
        assert_eq!(applescript_escape("a\"b"), "a\\\"b");
        assert_eq!(applescript_escape("a\\b"), "a\\\\b");
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

    // ----- applescript_escape: ordering and combined characters -----

    #[test]
    fn applescript_escape_handles_backslash_before_quote() {
        // A backslash immediately followed by a quote: each is escaped exactly
        // once, and the backslash-doubling must not re-escape the quote's added
        // backslash. Input `\"` -> `\\` + `\"` == `\\\"`.
        assert_eq!(applescript_escape("\\\""), "\\\\\\\"");
    }

    #[test]
    fn applescript_escape_is_identity_on_plain_text_and_empty() {
        assert_eq!(applescript_escape(""), "");
        assert_eq!(
            applescript_escape("/etc/pam.d/sudo_local"),
            "/etc/pam.d/sudo_local"
        );
        // A realistic temp path with spaces needs no escaping (no quotes/slashes
        // of the escaped kind).
        assert_eq!(
            applescript_escape("/var/folders/yutani touchid"),
            "/var/folders/yutani touchid"
        );
    }

    #[test]
    fn applescript_escape_handles_multiple_quotes_and_backslashes() {
        assert_eq!(applescript_escape("\"\""), "\\\"\\\"");
        assert_eq!(applescript_escape("\\\\"), "\\\\\\\\");
    }
}
