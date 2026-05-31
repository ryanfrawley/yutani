//! Native macOS right-click context menu for the terminal grid.
//!
//! Builds and synchronously pops up an AppKit `NSMenu` offering the usual
//! editing actions over the current selection — Copy, Paste, Select All — plus
//! Open Link / Copy Link when the click landed on a hyperlink, and a Clear that
//! wipes the screen and scrollback. The menu runs its own modal tracking loop
//! (`popUpMenuPositioningItem:atLocation:inView:`) and returns the chosen
//! command; `WindowState` applies it (see `state_input` / `state_pointer`).
//!
//! Off macOS this is a no-op stub returning `None`, so the right-click handler
//! compiles everywhere and simply does nothing.

/// One entry the user can pick from the context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContextMenuCommand {
    Copy,
    Paste,
    SelectAll,
    OpenLink,
    CopyLink,
    Clear,
}

/// Click context the menu is built from: whether there's a live selection to
/// copy, and whether the click landed on a hyperlink (which adds the link
/// actions). Paste, Select All, and Clear are always offered.
pub(crate) struct ContextMenuItems {
    pub has_selection: bool,
    pub has_link: bool,
}

/// Pop up the context menu at the current pointer and block until the user
/// picks an item or dismisses it. Returns the chosen command, or `None` on
/// dismissal.
#[cfg(target_os = "macos")]
pub(crate) fn show(items: ContextMenuItems) -> Option<ContextMenuCommand> {
    imp::show(items)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn show(_items: ContextMenuItems) -> Option<ContextMenuCommand> {
    None
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::Mutex;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{class, define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSPoint, NSString};

    use super::{ContextMenuCommand, ContextMenuItems};

    // Each menu item carries one of these as its `tag`; the action reads the
    // sender's tag back to learn which item fired. Tags must be non-zero so a
    // separator (tag 0) is never mistaken for a real pick.
    const TAG_COPY: isize = 1;
    const TAG_PASTE: isize = 2;
    const TAG_SELECT_ALL: isize = 3;
    const TAG_OPEN_LINK: isize = 4;
    const TAG_COPY_LINK: isize = 5;
    const TAG_CLEAR: isize = 6;

    /// Single-slot mailbox from the AppKit menu action (main thread) back to
    /// `show` (also main thread). `popUpMenuPositioningItem:` runs a nested
    /// modal loop that dispatches the action synchronously before it returns,
    /// so we just stash the picked tag here and read it once the loop unwinds.
    /// The `Mutex` is only for the `'static` safety a shared static demands.
    static PICKED: Mutex<Option<isize>> = Mutex::new(None);

    define_class! {
        // A bare target object whose sole job is to receive the menu action
        // and record which item sent it. Items with a `nil` target route
        // through the responder chain and end up disabled under
        // `autoenablesItems`; giving them a concrete target that responds to
        // the action keeps them live and the pick unambiguous.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "YutaniMenuTarget"]
        struct MenuTarget;

        impl MenuTarget {
            #[unsafe(method(yutaniMenuPick:))]
            fn pick(&self, sender: *mut AnyObject) {
                if sender.is_null() {
                    return;
                }
                let tag: isize = unsafe { msg_send![sender, tag] };
                if let Ok(mut g) = PICKED.lock() {
                    *g = Some(tag);
                }
            }
        }
    }

    pub fn show(items: ContextMenuItems) -> Option<ContextMenuCommand> {
        let mtm = MainThreadMarker::new()?;
        unsafe {
            // Drop any stale pick from a menu that was dismissed without a
            // selection — without this a prior pick could leak into this run.
            if let Ok(mut g) = PICKED.lock() {
                *g = None;
            }

            let target: Retained<MenuTarget> = msg_send![mtm.alloc::<MenuTarget>(), init];
            let menu: *mut AnyObject = msg_send![class!(NSMenu), alloc];
            let menu: *mut AnyObject = msg_send![menu, init];
            // We set each item's enabled state ourselves; AppKit's auto-enable
            // (which validates against the responder chain) would otherwise
            // grey out items whose target is our plain helper object.
            let _: () = msg_send![menu, setAutoenablesItems: false];

            let action = sel!(yutaniMenuPick:);
            let add = |title: &str, tag: isize, enabled: bool| {
                let t = NSString::from_str(title);
                let empty = NSString::from_str("");
                let item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
                let item: *mut AnyObject = msg_send![
                    item,
                    initWithTitle: &*t,
                    action: action,
                    keyEquivalent: &*empty,
                ];
                let _: () = msg_send![item, setTag: tag];
                let _: () = msg_send![item, setTarget: &*target];
                let _: () = msg_send![item, setEnabled: enabled];
                let _: () = msg_send![menu, addItem: item];
            };
            let separator = || {
                let sep: *mut AnyObject = msg_send![class!(NSMenuItem), separatorItem];
                let _: () = msg_send![menu, addItem: sep];
            };

            // Link actions lead when the click was on a hyperlink, mirroring
            // the order Safari/Finder use (Open above Copy).
            if items.has_link {
                add("Open Link", TAG_OPEN_LINK, true);
                add("Copy Link", TAG_COPY_LINK, true);
                separator();
            }
            // Copy is dimmed with nothing selected; Paste/Select All are always
            // actionable (Paste no-ops on an empty clipboard).
            add("Copy", TAG_COPY, items.has_selection);
            add("Paste", TAG_PASTE, true);
            add("Select All", TAG_SELECT_ALL, true);
            separator();
            add("Clear", TAG_CLEAR, true);

            // Position at the live pointer in screen coordinates (view = nil).
            // `mouseLocation` reflects the cursor now, which still sits where
            // the right-click landed — robust even though winit delivers the
            // press a frame late, after `NSApp.currentEvent` has moved on.
            let loc: NSPoint = msg_send![class!(NSEvent), mouseLocation];
            let nil_item: *mut AnyObject = std::ptr::null_mut();
            let nil_view: *mut AnyObject = std::ptr::null_mut();
            let _: bool = msg_send![
                menu,
                popUpMenuPositioningItem: nil_item,
                atLocation: loc,
                inView: nil_view,
            ];

            let tag = PICKED.lock().ok().and_then(|mut g| g.take())?;
            Some(match tag {
                TAG_COPY => ContextMenuCommand::Copy,
                TAG_PASTE => ContextMenuCommand::Paste,
                TAG_SELECT_ALL => ContextMenuCommand::SelectAll,
                TAG_OPEN_LINK => ContextMenuCommand::OpenLink,
                TAG_COPY_LINK => ContextMenuCommand::CopyLink,
                TAG_CLEAR => ContextMenuCommand::Clear,
                _ => return None,
            })
        }
    }
}
