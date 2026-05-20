#[derive(Debug, Clone)]
pub enum CustomEvent {
    PtyInput(String),
    /// The child shell exited (EOF/EIO on the PTY master, child reaped),
    /// carrying its exit code. How the window reacts is governed by the
    /// `shell_exit_mode` config setting.
    PtyExit(i32),
}
