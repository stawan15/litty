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

// Menu-bar hamster: sits still while idle (no timer, no CPU), runs in its wheel while a command
// runs, stuffs its cheeks while an update downloads. The menu shows update status and actions.

use objc2::runtime::NSObject;
use objc2::{ClassType, class, define_class};
use objc2_foundation::{NSSize, NSString};
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// What the user picked in the menu-bar menu.
#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    Check,
    Install,
    NewWindow,
    Quit,
}

const ACTIONS: [TrayAction; 4] = [TrayAction::Check, TrayAction::Install, TrayAction::NewWindow, TrayAction::Quit];

type Sink = Box<dyn Fn(TrayAction) + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "LittyTrayTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(pick:))]
        fn pick(&self, item: &AnyObject) {
            let tag: isize = unsafe { msg_send![item, tag] };
            if let (Some(sink), Some(&action)) = (SINK.get(), ACTIONS.get(tag as usize)) {
                sink(action);
            }
        }
    }
);

#[derive(Clone, Copy, PartialEq)]
pub enum TrayMode {
    Idle,
    /// Idle with a dot: an update is available or waiting to install.
    Update,
    Running,
    Loading,
}

const TRAY_FRAME: Duration = Duration::from_millis(125);
const IDLE: &[u8] = include_bytes!("../assets/tray/idle.png");
const IDLE_UPDATE: &[u8] = include_bytes!("../assets/tray/idle-update.png");
const RUN: [&[u8]; 8] = [
    include_bytes!("../assets/tray/run-0.png"),
    include_bytes!("../assets/tray/run-1.png"),
    include_bytes!("../assets/tray/run-2.png"),
    include_bytes!("../assets/tray/run-3.png"),
    include_bytes!("../assets/tray/run-4.png"),
    include_bytes!("../assets/tray/run-5.png"),
    include_bytes!("../assets/tray/run-6.png"),
    include_bytes!("../assets/tray/run-7.png"),
];
const LOAD: [&[u8]; 8] = [
    include_bytes!("../assets/tray/load-0.png"),
    include_bytes!("../assets/tray/load-1.png"),
    include_bytes!("../assets/tray/load-2.png"),
    include_bytes!("../assets/tray/load-3.png"),
    include_bytes!("../assets/tray/load-4.png"),
    include_bytes!("../assets/tray/load-5.png"),
    include_bytes!("../assets/tray/load-6.png"),
    include_bytes!("../assets/tray/load-7.png"),
];

pub struct Tray {
    _item: Retained<AnyObject>,
    button: Retained<AnyObject>,
    menu: Retained<AnyObject>,
    target: Retained<Target>,
    /// Decoded on first use, so an idle hamster never loads the animation frames.
    images: Vec<Option<Retained<AnyObject>>>,
    mode: Option<TrayMode>,
    frame: usize,
    next: Instant,
}

impl Tray {
    pub fn new(sink: Sink) -> Option<Tray> {
        let _ = SINK.set(sink);
        // SAFETY: AppKit calls on the main thread (the event loop's), with valid receivers.
        unsafe {
            let bar: Retained<AnyObject> = msg_send![class!(NSStatusBar), systemStatusBar];
            // NSSquareStatusItemLength
            let item: Retained<AnyObject> = msg_send![&*bar, statusItemWithLength: -2.0f64];
            let button: Option<Retained<AnyObject>> = msg_send![&*item, button];
            let menu: Retained<AnyObject> = msg_send![class!(NSMenu), new];
            let _: () = msg_send![&*menu, setAutoenablesItems: false];
            let _: () = msg_send![&*item, setMenu: &*menu];
            let target: Retained<Target> = msg_send![Target::class(), new];
            Some(Tray { _item: item, button: button?, menu, target, images: vec![None; 2 + RUN.len() + LOAD.len()], mode: None, frame: 0, next: Instant::now() })
        }
    }

    fn image(&mut self, index: usize) -> Option<Retained<AnyObject>> {
        if self.images[index].is_none() {
            let png = match index {
                0 => IDLE,
                1 => IDLE_UPDATE,
                i if i < 2 + RUN.len() => RUN[i - 2],
                i => LOAD[i - 2 - RUN.len()],
            };
            // SAFETY: NSData copies the bytes; NSImage decodes the PNG lazily.
            self.images[index] = unsafe {
                let data: Retained<AnyObject> = msg_send![class!(NSData), dataWithBytes: png.as_ptr().cast::<c_void>(), length: png.len()];
                let image: Option<Retained<AnyObject>> = msg_send![msg_send![class!(NSImage), alloc], initWithData: &*data];
                image.inspect(|i| {
                    // 36 px frames drawn at 18 pt: sharp on Retina.
                    let _: () = msg_send![&**i, setSize: NSSize::new(18.0, 18.0)];
                    let _: () = msg_send![&**i, setAccessibilityDescription: &*NSString::from_str("litty")];
                })
            };
        }
        self.images[index].clone()
    }

    /// Show `mode`, advancing its animation when a frame is due unless `still`. Returns when the
    /// next frame is due (None while still, so a quiet tray never wakes the app). Each frame costs
    /// AppKit a few ms of snapshotting, hence animating only when it is worth looking at.
    pub fn animate(&mut self, mode: TrayMode, still: bool, now: Instant) -> Option<Instant> {
        let frames = match mode {
            TrayMode::Idle | TrayMode::Update => 1,
            _ if still => 1,
            TrayMode::Running => RUN.len(),
            TrayMode::Loading => LOAD.len(),
        };
        if self.mode != Some(mode) {
            (self.mode, self.frame, self.next) = (Some(mode), 0, now);
        } else if frames == 1 || now < self.next {
            return (frames > 1).then_some(self.next);
        } else {
            self.frame = (self.frame + 1) % frames;
        }
        let index = match mode {
            TrayMode::Idle => 0,
            TrayMode::Update => 1,
            TrayMode::Running => 2 + self.frame,
            TrayMode::Loading => 2 + RUN.len() + self.frame,
        };
        if let Some(image) = self.image(index) {
            // SAFETY: main thread, valid button and image.
            let _: () = unsafe { msg_send![&*self.button, setImage: &*image] };
        }
        self.next = now + TRAY_FRAME;
        (frames > 1).then_some(self.next)
    }

    /// Rebuild the menu: a status line, an optional action for it, then New Window and Quit.
    pub fn set_menu(&self, status: &str, action: Option<(&str, TrayAction)>) {
        // SAFETY: main thread; items are retained by the menu.
        unsafe {
            let _: () = msg_send![&*self.menu, removeAllItems];
            let add = |title: &str, action: Option<TrayAction>, key: &str| {
                let sel = action.map(|_| objc2::sel!(pick:));
                let item: Retained<AnyObject> = msg_send![msg_send![class!(NSMenuItem), alloc], initWithTitle: &*NSString::from_str(title), action: sel, keyEquivalent: &*NSString::from_str(key)];
                if let Some(a) = action {
                    let _: () = msg_send![&*item, setTarget: &*self.target];
                    let _: () = msg_send![&*item, setTag: a as isize];
                } else {
                    let _: () = msg_send![&*item, setEnabled: false];
                }
                let _: () = msg_send![&*self.menu, addItem: &*item];
            };
            add(status, None, "");
            if let Some((title, a)) = action {
                add(title, Some(a), "");
            }
            let sep: Retained<AnyObject> = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![&*self.menu, addItem: &*sep];
            add("New Window", Some(TrayAction::NewWindow), "");
            add("Quit litty", Some(TrayAction::Quit), "");
        }
    }
}
