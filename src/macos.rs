//! Native macOS window tabs: each litty tab is a real NSWindow that the system groups into one
//! tabbed window, so the tab bar, drag-to-detach and Mission Control behave like any other app.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

fn ns_window(w: &Window) -> Option<Retained<AnyObject>> {
    let RawWindowHandle::AppKit(h) = w.window_handle().ok()?.as_raw() else { return None };
    // SAFETY: the handle holds a valid NSView, and windows are only touched on the main thread.
    let view: &AnyObject = unsafe { h.ns_view.cast().as_ref() };
    unsafe { msg_send![view, window] }
}

/// Add `new` as a tab of the window `existing` belongs to, and show it.
pub fn add_tab(existing: &Window, new: &Window) {
    let (Some(a), Some(b)) = (ns_window(existing), ns_window(new)) else { return };
    // SAFETY: plain NSWindow messages; 1 is NSWindowTabbingModePreferred / NSWindowAbove.
    unsafe {
        let _: () = msg_send![&*a, setTabbingMode: 1isize];
        let _: () = msg_send![&*b, setTabbingMode: 1isize];
        let _: () = msg_send![&*a, addTabbedWindow: &*b, ordered: 1isize];
        let _: () = msg_send![&*b, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
    }
}
