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

/// What the user picked in the menu-bar menu, or a clicked notification.
#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    Check,
    Install,
    NewWindow,
    Quit,
    /// A "command finished" notification was clicked: show the pane with this id.
    Focus(usize),
}

/// Menu items carry their action as a tag: the index in this list.
const ACTIONS: [TrayAction; 4] = [TrayAction::Check, TrayAction::Install, TrayAction::NewWindow, TrayAction::Quit];

type Sink = Box<dyn Fn(TrayAction) + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();

/// Where menu picks and notification clicks go (the app's event loop). Set once at startup.
pub fn set_sink(sink: Sink) {
    let _ = SINK.set(sink);
}

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
    /// A slow command just finished in the background: happy (exit 0) or dizzy.
    Done(bool),
}

const TRAY_FRAME: Duration = Duration::from_millis(125);
const IDLE: &[u8] = include_bytes!("../assets/tray/idle.png");
const IDLE_UPDATE: &[u8] = include_bytes!("../assets/tray/idle-update.png");
const DONE_OK: &[u8] = include_bytes!("../assets/tray/done-ok.png");
const DONE_FAIL: &[u8] = include_bytes!("../assets/tray/done-fail.png");
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
    pub fn new() -> Option<Tray> {
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
            Some(Tray { _item: item, button: button?, menu, target, images: vec![None; 4 + RUN.len() + LOAD.len()], mode: None, frame: 0, next: Instant::now() })
        }
    }

    fn image(&mut self, index: usize) -> Option<Retained<AnyObject>> {
        if self.images[index].is_none() {
            let png = match index {
                0 => IDLE,
                1 => IDLE_UPDATE,
                2 => DONE_OK,
                3 => DONE_FAIL,
                i if i < 4 + RUN.len() => RUN[i - 4],
                i => LOAD[i - 4 - RUN.len()],
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
            TrayMode::Idle | TrayMode::Update | TrayMode::Done(_) => 1,
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
            TrayMode::Done(true) => 2,
            TrayMode::Done(false) => 3,
            TrayMode::Running => 4 + self.frame,
            TrayMode::Loading => 4 + RUN.len() + self.frame,
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
                    let tag = ACTIONS.iter().position(|b| std::mem::discriminant(b) == std::mem::discriminant(&a)).unwrap_or(0);
                    let _: () = msg_send![&*item, setTag: tag as isize];
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

// "Command finished" notifications through UserNotifications. The framework is loaded on first
// use (not linked), so startup doesn't pay for it; it needs an app bundle, so a bare binary
// (cargo run) silently skips notifications.

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "LittyNotificationDelegate"]
    struct NotificationDelegate;

    impl NotificationDelegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &AnyObject, response: &AnyObject, done: &block2::Block<dyn Fn()>) {
            // SAFETY: documented UNNotificationResponse → UNNotification → UNNotificationRequest chain.
            let id: Option<Retained<NSString>> = unsafe {
                let note: Retained<AnyObject> = msg_send![response, notification];
                let request: Retained<AnyObject> = msg_send![&*note, request];
                msg_send![&*request, identifier]
            };
            let pane = id.and_then(|s| s.to_string().strip_prefix("litty-pane-")?.split('-').next()?.parse().ok());
            if let (Some(sink), Some(pane)) = (SINK.get(), pane) {
                sink(TrayAction::Focus(pane));
            }
            done.call(());
        }

        // Also show the banner if litty happens to be the active app.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(&self, _center: &AnyObject, _note: &AnyObject, done: &block2::Block<dyn Fn(usize)>) {
            // UNNotificationPresentationOptionList | Banner
            done.call((8 | 16,));
        }
    }
);

pub struct Notifier {
    center: Retained<AnyObject>,
    _delegate: Retained<NotificationDelegate>,
    sent: u64,
}

impl Notifier {
    pub fn new() -> Option<Notifier> {
        // SAFETY: main thread; the class exists once the framework is loaded, and
        // currentNotificationCenter is only called inside an app bundle (it throws otherwise).
        unsafe {
            let bundle: Retained<AnyObject> = msg_send![class!(NSBundle), mainBundle];
            let id: Option<Retained<NSString>> = msg_send![&*bundle, bundleIdentifier];
            id?;
            let path = c"/System/Library/Frameworks/UserNotifications.framework/UserNotifications";
            if nix::libc::dlopen(path.as_ptr(), nix::libc::RTLD_LAZY).is_null() {
                return None;
            }
            let class = objc2::runtime::AnyClass::get(c"UNUserNotificationCenter")?;
            let center: Retained<AnyObject> = msg_send![class, currentNotificationCenter];
            let delegate: Retained<NotificationDelegate> = msg_send![NotificationDelegate::class(), new];
            let _: () = msg_send![&*center, setDelegate: &*delegate];
            Some(Notifier { center, _delegate: delegate, sent: 0 })
        }
    }

    /// Post a notification that opens the pane `pane` when clicked. The first one asks the user
    /// for permission; after that the system remembers the answer.
    pub fn notify(&mut self, pane: usize, title: &str, body: &str) {
        self.sent += 1;
        let (title, body) = (title.to_string(), body.to_string());
        let id = format!("litty-pane-{pane}-{}", self.sent);
        let post = block2::RcBlock::new(move |granted: objc2::runtime::Bool, _err: *mut AnyObject| {
            if !granted.as_bool() {
                return;
            }
            // SAFETY: UserNotifications is thread-safe; this runs on its private queue.
            unsafe {
                let Some(class) = objc2::runtime::AnyClass::get(c"UNUserNotificationCenter") else { return };
                let center: Retained<AnyObject> = msg_send![class, currentNotificationCenter];
                let Some(content_class) = objc2::runtime::AnyClass::get(c"UNMutableNotificationContent") else { return };
                let content: Retained<AnyObject> = msg_send![content_class, new];
                let _: () = msg_send![&*content, setTitle: &*NSString::from_str(&title)];
                let _: () = msg_send![&*content, setBody: &*NSString::from_str(&body)];
                let Some(request_class) = objc2::runtime::AnyClass::get(c"UNNotificationRequest") else { return };
                let none: *const AnyObject = std::ptr::null();
                let request: Retained<AnyObject> = msg_send![request_class, requestWithIdentifier: &*NSString::from_str(&id), content: &*content, trigger: none];
                let no_handler: Option<&block2::Block<dyn Fn(*mut AnyObject)>> = None;
                let _: () = msg_send![&*center, addNotificationRequest: &*request, withCompletionHandler: no_handler];
            }
        });
        // SAFETY: main thread, valid center; UNAuthorizationOptionAlert (4).
        unsafe {
            let _: () = msg_send![&*self.center, requestAuthorizationWithOptions: 4usize, completionHandler: &*post];
        }
    }
}
