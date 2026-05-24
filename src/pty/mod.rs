extern crate libc;
use nix::libc::*;
use std::ffi::CString;

/// Handle to a running child shell attached to a PTY master. Fork has already
/// returned in the parent; the child has exec'd the shell.
pub struct Pty {
    pub child: i32,
    pub master: i32,
}

/// What the forked PTY child should become. Normally the user's login shell;
/// on first run it's the onboarding console program, which itself execs the
/// shell in-place once setup completes (so the same PTY transitions seamlessly
/// from setup to shell).
pub enum ChildProgram {
    Shell,
    /// Re-exec this binary at `exe` with `--onboard`. The path is resolved in
    /// the parent (before fork) to avoid doing the lookup post-fork.
    Onboard { exe: std::path::PathBuf },
}

/// Set an environment variable from raw bytes in the current (post-fork,
/// single-threaded) process. Mirrors the `setenv` use for `TERM`.
unsafe fn set_env(key: &str, val: &std::ffi::OsStr) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(k) = CString::new(key) else { return };
    let Ok(v) = CString::new(val.as_bytes()) else { return };
    setenv(k.as_ptr(), v.as_ptr(), 1);
}

/// Replace the current process with the user's shell, started as a *login*
/// shell (argv[0] = `-<basename>`) so the profile scripts that set PATH (brew
/// shellenv, etc.) run. Never returns; on exec failure it exits 127. Callable
/// from the forked child here and from the onboarding program once it finishes.
///
/// When `zdotdir` is `Some` and the shell is zsh, redirect `$ZDOTDIR` at it so
/// Yutani's shell integration loads automatically (see
/// [`crate::shell_integration`]). The user's original `$ZDOTDIR` (or `$HOME`)
/// is preserved in `YUTANI_USER_ZDOTDIR` for the wrapper dotfiles to chain to.
pub fn exec_login_shell(zdotdir: Option<&std::path::Path>) -> ! {
    let shell_path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    if let Some(dir) = zdotdir {
        if crate::shell_integration::is_zsh(&shell_path) {
            // Capture where the user's real dotfiles live before we move
            // ZDOTDIR: their own ZDOTDIR if set, else $HOME.
            let user_zdotdir = std::env::var_os("ZDOTDIR")
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var_os("HOME"))
                .unwrap_or_default();
            unsafe {
                set_env("YUTANI_USER_ZDOTDIR", &user_zdotdir);
                set_env("YUTANI_ZDOTDIR", dir.as_os_str());
                set_env("ZDOTDIR", dir.as_os_str());
            }
        }
    }
    let shell_c = CString::new(shell_path.clone()).unwrap();
    let basename = std::path::Path::new(&shell_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sh");
    let argv0 = CString::new(format!("-{basename}")).unwrap();
    // execvp replaces the process image; the Ok branch is unreachable.
    match nix::unistd::execvp(shell_c.as_c_str(), &[argv0.as_c_str()]) {
        Ok(_) => std::process::exit(0),
        Err(_) => std::process::exit(127),
    }
}

/// Forks and execs `program` on the slave side of `fdm`. Returns in the parent
/// once `fork` has completed, without blocking on child output. Drive I/O by
/// calling `run` on the returned handle (typically from a worker thread).
///
/// `zdotdir` is the Yutani shell-integration directory (see
/// [`crate::shell_integration::prepare_zdotdir`]); it's forwarded to the shell
/// exec so zsh auto-loads the integration. Only used for the `Shell` program —
/// the onboarding child resolves its own when it execs the shell.
pub fn fork_pty(
    fdm: i32,
    program: ChildProgram,
    zdotdir: Option<std::path::PathBuf>,
) -> Result<Pty, String> {
    let fds: i32;

    unsafe {
        if grantpt(fdm) != 0 {
            return Err("Error on grantpt()".to_string());
        }
        if unlockpt(fdm) != 0 {
            return Err("Error on unlockpt()".to_string());
        }

        fds = open(ptsname(fdm), O_RDWR);
        if fds < 0 {
            return Err("Error opening slave pty".to_string());
        }

        match nix::unistd::fork() {
            Ok(nix::unistd::ForkResult::Child) => {
                close(fdm);

                let mut slave_settings = std::mem::MaybeUninit::<termios>::uninit();
                tcgetattr(fds, slave_settings.as_mut_ptr());
                let mut slave_settings = slave_settings.assume_init();
                slave_settings.c_lflag &= !(ECHO | ICANON);
                tcsetattr(fds, TCSANOW, &mut slave_settings);

                dup2(fds, 0);
                dup2(fds, 1);
                dup2(fds, 2);

                setsid();
                ioctl(0, TIOCSCTTY.into(), 1);

                // Advertise our termcap so apps (and zle) pick the right key
                // sequences. When the app is launched from Finder, $TERM is
                // unset and the shell falls back to "dumb" — which leaves
                // Backspace (0x7f) unmapped and reads as a printable glyph.
                let term = CString::new("TERM").unwrap();
                let term_val = CString::new("xterm-256color").unwrap();
                setenv(term.as_ptr(), term_val.as_ptr(), 1);

                match program {
                    // Start the shell as a *login* shell so /etc/zprofile and
                    // ~/.zprofile run. When launched from Finder we inherit only
                    // the launchd PATH, so brew shellenv — typically sourced from
                    // ~/.zprofile — is what puts /opt/homebrew/bin on PATH.
                    ChildProgram::Shell => exec_login_shell(zdotdir.as_deref()),
                    // First run: become the onboarding console program. It execs
                    // the login shell itself once done. If the re-exec fails for
                    // any reason, fall back to the shell so a broken onboarding
                    // never bricks the terminal.
                    ChildProgram::Onboard { exe } => {
                        let exe_c = CString::new(exe.as_os_str().as_encoded_bytes())
                            .unwrap_or_else(|_| CString::new("yutani").unwrap());
                        let flag = CString::new("--onboard").unwrap();
                        let _ = nix::unistd::execv(
                            exe_c.as_c_str(),
                            &[exe_c.as_c_str(), flag.as_c_str()],
                        );
                        exec_login_shell(zdotdir.as_deref());
                    }
                }
            }
            Ok(nix::unistd::ForkResult::Parent { child }) => {
                close(fds);
                Ok(Pty {
                    child: child.as_raw(),
                    master: fdm,
                })
            }
            Err(e) => Err(format!("Error in fork(): {e}")),
        }
    }
}

impl Pty {
    /// Blocks the calling thread reading from the PTY master, forwarding each
    /// UTF-8 chunk to `on_output`. Returns the child's exit code when it
    /// exits (EOF or EIO), after reaping it with `waitpid`. A normal exit
    /// yields its status; a fatal signal yields `128 + signal` (shell
    /// convention); an indeterminate state yields `-1`.
    pub fn run<F: Fn(&str)>(&self, on_output: F) -> i32 {
        let mut input: [u8; 1500] = [0; 1500];
        let mut pending: Vec<u8> = Vec::with_capacity(4);

        // Optional raw-byte dump for debugging app behavior (e.g. tmux pane
        // borders). Set `TERMINAL_PTY_LOG=/path/to/file` before launching.
        let mut log = std::env::var_os("TERMINAL_PTY_LOG").and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });

        unsafe {
            loop {
                let n = read(self.master, input.as_mut_ptr() as *mut c_void, input.len());
                if n <= 0 {
                    break;
                }
                if let Some(f) = log.as_mut() {
                    use std::io::Write;
                    let _ = f.write_all(&input[..n as usize]);
                    let _ = f.flush();
                }
                pending.extend_from_slice(&input[..n as usize]);
                emit_utf8(&mut pending, &on_output);
            }

            let mut status: c_int = 0;
            libc::waitpid(self.child, &mut status, 0);
            exit_code(status)
        }
    }
}

/// Translate a `waitpid` status word into a single exit code. Normal exit
/// uses the status; a fatal signal maps to `128 + signal` as shells do;
/// anything else (shouldn't happen for a reaped child) is `-1`.
fn exit_code(status: c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    }
}

// Drain the longest valid-UTF8 prefix of `pending` into `on_output`. Incomplete
// trailing bytes stay in `pending` for the next read; invalid bytes are dropped.
fn emit_utf8<F: Fn(&str)>(pending: &mut Vec<u8>, on_output: &F) {
    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                if !s.is_empty() {
                    on_output(s);
                }
                pending.clear();
                return;
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if valid_up_to > 0 {
                    let s = unsafe { std::str::from_utf8_unchecked(&pending[..valid_up_to]) };
                    on_output(s);
                }
                match e.error_len() {
                    None => {
                        // Trailing incomplete sequence — keep it for next call.
                        pending.drain(..valid_up_to);
                        return;
                    }
                    Some(err_len) => {
                        // Invalid bytes — drop them and keep scanning.
                        pending.drain(..valid_up_to + err_len);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::emit_utf8;
    use super::exit_code;
    use std::cell::RefCell;

    fn collector() -> (impl Fn(&str), std::rc::Rc<RefCell<String>>) {
        let out = std::rc::Rc::new(RefCell::new(String::new()));
        let out_clone = out.clone();
        let cb = move |s: &str| out_clone.borrow_mut().push_str(s);
        (cb, out)
    }

    #[test]
    fn ascii_passes_through() {
        let (cb, out) = collector();
        let mut pending = b"hello".to_vec();
        emit_utf8(&mut pending, &cb);
        assert_eq!(*out.borrow(), "hello");
        assert!(pending.is_empty());
    }

    #[test]
    fn split_multibyte_waits_for_rest() {
        // "é" is 0xC3 0xA9 — feed the first byte alone.
        let (cb, out) = collector();
        let mut pending = vec![0xC3];
        emit_utf8(&mut pending, &cb);
        assert_eq!(*out.borrow(), "");
        assert_eq!(pending, vec![0xC3]);

        pending.push(0xA9);
        emit_utf8(&mut pending, &cb);
        assert_eq!(*out.borrow(), "é");
        assert!(pending.is_empty());
    }

    #[test]
    fn valid_prefix_with_incomplete_tail() {
        let (cb, out) = collector();
        // "ab" + first byte of "é"
        let mut pending = vec![b'a', b'b', 0xC3];
        emit_utf8(&mut pending, &cb);
        assert_eq!(*out.borrow(), "ab");
        assert_eq!(pending, vec![0xC3]);
    }

    #[test]
    fn invalid_byte_is_dropped() {
        let (cb, out) = collector();
        // "a" + stray continuation byte + "b"
        let mut pending = vec![b'a', 0x80, b'b'];
        emit_utf8(&mut pending, &cb);
        assert_eq!(*out.borrow(), "ab");
        assert!(pending.is_empty());
    }

    // `waitpid` status words: a normal exit with code N has the code in the
    // high byte (N << 8); a fatal signal S sets the low 7 bits to S.
    #[test]
    fn exit_code_normal_zero() {
        assert_eq!(exit_code(0), 0);
    }

    #[test]
    fn exit_code_normal_nonzero() {
        // exit 1 → status word 0x100, exit 42 → 42 << 8.
        assert_eq!(exit_code(1 << 8), 1);
        assert_eq!(exit_code(42 << 8), 42);
    }

    #[test]
    fn exit_code_signal_maps_to_128_plus_signal() {
        // Killed by signal 9 (SIGKILL) → 137; signal 15 (SIGTERM) → 143.
        assert_eq!(exit_code(9), 137);
        assert_eq!(exit_code(15), 143);
    }
}
