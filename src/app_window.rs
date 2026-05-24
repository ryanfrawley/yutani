/// Process-unique identifier for a tab (one PTY + `Terminal`). Minted per
/// reader thread so `CustomEvent`s can be routed to the right tab regardless
/// of which window currently owns it (tabs may move between windows later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

#[derive(Debug, Clone)]
pub enum CustomEvent {
    /// Shell output for the tagged tab.
    PtyInput(TabId, String),
    /// The child shell exited (EOF/EIO on the PTY master, child reaped),
    /// carrying the tab it belonged to and its exit code. How the window
    /// reacts is governed by the `shell_exit_mode` config setting.
    PtyExit(TabId, i32),
}
