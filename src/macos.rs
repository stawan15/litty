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

/// (group, position) of a window among the system's native tabs: windows in one tab group share
/// `group`, and `position` is the tab's place in its bar.
pub fn tab_position(w: &Window) -> (usize, usize) {
    let Some(win) = ns_window(w) else { return (0, 0) };
    let me = Retained::as_ptr(&win) as usize;
    // SAFETY: plain NSWindow / NSArray messages on the main thread.
    unsafe {
        let tabs: Option<Retained<AnyObject>> = msg_send![&*win, tabbedWindows];
        let Some(tabs) = tabs else { return (me, 0) };
        let n: usize = msg_send![&*tabs, count];
        let at = |i: usize| -> usize {
            let o: *mut AnyObject = msg_send![&*tabs, objectAtIndex: i];
            o as usize
        };
        ((0..n).map(at).min().unwrap_or(me), (0..n).position(|i| at(i) == me).unwrap_or(0))
    }
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
    /// The quick-terminal hotkey was pressed.
    Quick,
    /// A right-click menu pick: the action at this index in `config::ACTIONS`.
    Action(usize),
    /// The settings window changed the config file.
    Settings,
}

/// Menu item tags at and above this are right-click menu actions (tag - MENU = `config::ACTIONS` index).
const MENU: isize = 100;

/// Menu items carry their action as a tag: the index in this list.
const ACTIONS: [TrayAction; 4] = [TrayAction::Check, TrayAction::Install, TrayAction::NewWindow, TrayAction::Quit];

type Sink = Box<dyn Fn(TrayAction) + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();

/// Carbon virtual key code for a key name as written in the config ("a", "`", "space", "f5").
pub fn key_code(name: &str) -> Option<u32> {
    const KEYS: &str = "asdfhgzxcv?bqweryt123465=97-80]ou[ip?lj'k;\\,/nm.";
    let code = match name {
        "`" | "grave" => 0x32,
        "space" => 0x31,
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "escape" => 0x35,
        f if f.starts_with('f') && f.len() > 1 => {
            const F: [u32; 12] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F];
            *F.get(f[1..].parse::<usize>().ok()?.checked_sub(1)?)?
        }
        k if k.chars().count() == 1 => KEYS.find(k).filter(|_| k != "?")? as u32,
        _ => return None,
    };
    Some(code)
}

/// A system-wide hotkey (Carbon's RegisterEventHotKey: no Accessibility permission needed) that
/// sends `TrayAction::Quick`. `mods` are the config's SHIFT/ALT/CTRL/SUPER bits. Carbon is loaded
/// only here, when the config asks for a quick terminal.
pub fn register_hotkey(key: u32, mods: u8) -> bool {
    use crate::kitty::{ALT, CTRL, SHIFT, SUPER};
    use nix::libc::{RTLD_LAZY, dlopen, dlsym};
    type Target = *mut c_void;
    type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;
    #[repr(C)]
    struct Spec {
        class: u32,
        kind: u32,
    }
    #[repr(C)]
    struct HotKeyId {
        signature: u32,
        id: u32,
    }
    extern "C" fn pressed(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
        if let Some(sink) = SINK.get() {
            sink(TrayAction::Quick);
        }
        0
    }
    let carbon_mods = [(SUPER, 0x100), (SHIFT, 0x200), (ALT, 0x800), (CTRL, 0x1000)].iter().filter(|(m, _)| mods & m != 0).map(|(_, c)| c).sum();
    // SAFETY: Carbon's documented C API, called once on the main thread with matching signatures.
    unsafe {
        let lib = dlopen(c"/System/Library/Frameworks/Carbon.framework/Carbon".as_ptr(), RTLD_LAZY);
        let (target, install, register) = (
            dlsym(lib, c"GetApplicationEventTarget".as_ptr()),
            dlsym(lib, c"InstallEventHandler".as_ptr()),
            dlsym(lib, c"RegisterEventHotKey".as_ptr()),
        );
        if lib.is_null() || target.is_null() || install.is_null() || register.is_null() {
            return false;
        }
        let target: extern "C" fn() -> Target = std::mem::transmute(target);
        let install: extern "C" fn(Target, Handler, u32, *const Spec, *mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(install);
        let register: extern "C" fn(u32, u32, HotKeyId, Target, u32, *mut *mut c_void) -> i32 = std::mem::transmute(register);
        let app = target();
        let spec = Spec { class: u32::from_be_bytes(*b"keyb"), kind: 5 }; // kEventHotKeyPressed
        let (mut handler, mut hotkey) = (std::ptr::null_mut(), std::ptr::null_mut());
        install(app, pressed, 1, &spec, std::ptr::null_mut(), &mut handler) == 0
            && register(key, carbon_mods, HotKeyId { signature: u32::from_be_bytes(*b"lity"), id: 1 }, app, 0, &mut hotkey) == 0
    }
}

/// Bring litty to the front, or hide it so the previous app gets the keyboard back.
pub fn activate(front: bool) {
    // SAFETY: NSApplication messages on the main thread.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        if front {
            let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
        } else {
            let _: () = msg_send![&*app, hide: std::ptr::null::<AnyObject>()];
        }
    }
}

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
            let action = if tag >= MENU { Some(TrayAction::Action((tag - MENU) as usize)) } else { ACTIONS.get(tag as usize).copied() };
            if let (Some(sink), Some(action)) = (SINK.get(), action) {
                sink(action);
            }
        }

        #[unsafe(method(settings:))]
        fn settings(&self, _item: &AnyObject) {
            show_settings();
        }

        #[unsafe(method(changed:))]
        fn changed(&self, control: &AnyObject) {
            setting_changed(control);
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

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: main thread; the item came from the system status bar.
        unsafe {
            let bar: Retained<AnyObject> = msg_send![class!(NSStatusBar), systemStatusBar];
            let _: () = msg_send![&*bar, removeStatusItem: &*self._item];
        }
    }
}

// Right-click menu, settings window, see-through background and Dock badge.

use objc2_foundation::{NSPoint, NSRect};
use std::cell::RefCell;

/// A native right-click menu at the mouse: (title, action from `config::ACTIONS`, enabled);
/// "-" is a separator. Returns once the menu closes; a pick arrives as `TrayAction::Action`.
pub fn context_menu(w: &Window, items: &[(&str, &str, bool)]) {
    let Some(win) = ns_window(w) else { return };
    // SAFETY: AppKit calls on the main thread; the target outlives the (modal) menu.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let event: Option<Retained<AnyObject>> = msg_send![&*app, currentEvent];
        let view: Option<Retained<AnyObject>> = msg_send![&*win, contentView];
        let (Some(event), Some(view)) = (event, view) else { return };
        let target: Retained<Target> = msg_send![Target::class(), new];
        let menu: Retained<AnyObject> = msg_send![class!(NSMenu), new];
        let _: () = msg_send![&*menu, setAutoenablesItems: false];
        for &(title, action, enabled) in items {
            if title == "-" {
                let sep: Retained<AnyObject> = msg_send![class!(NSMenuItem), separatorItem];
                let _: () = msg_send![&*menu, addItem: &*sep];
                continue;
            }
            let Some(i) = crate::config::ACTIONS.iter().position(|a| *a == action) else { continue };
            let item: Retained<AnyObject> = msg_send![msg_send![class!(NSMenuItem), alloc], initWithTitle: &*NSString::from_str(title), action: objc2::sel!(pick:), keyEquivalent: &*NSString::from_str("")];
            let _: () = msg_send![&*item, setTarget: &*target];
            let _: () = msg_send![&*item, setTag: MENU + i as isize];
            let _: () = msg_send![&*item, setEnabled: enabled];
            let _: () = msg_send![&*menu, addItem: &*item];
        }
        let _: () = msg_send![class!(NSMenu), popUpContextMenu: &*menu, withEvent: &*event, forView: &*view];
    }
}

/// Let the desktop show through the background (opacity < 1), blurred if asked. The blur uses
/// the window server's CGSSetWindowBackgroundBlurRadius, as iTerm2, kitty and Alacritty do.
pub fn set_background(w: &Window, opacity: f32, blur: bool) {
    use nix::libc::{RTLD_DEFAULT, dlsym};
    w.set_transparent(opacity < 1.0);
    let Some(win) = ns_window(w) else { return };
    // SAFETY: main thread; the private functions are looked up and skipped if missing.
    unsafe {
        let (conn, set) = (dlsym(RTLD_DEFAULT, c"CGSDefaultConnectionForThread".as_ptr()), dlsym(RTLD_DEFAULT, c"CGSSetWindowBackgroundBlurRadius".as_ptr()));
        if conn.is_null() || set.is_null() {
            return;
        }
        let conn: extern "C" fn() -> u32 = std::mem::transmute(conn);
        let set: extern "C" fn(u32, u32, u32) -> i32 = std::mem::transmute(set);
        let number: isize = msg_send![&*win, windowNumber];
        set(conn(), number as u32, if blur && opacity < 1.0 { 20 } else { 0 });
    }
}

/// The Dock icon's badge: progress a program reports ("40%"), or none.
pub fn dock_badge(label: Option<&str>) {
    // SAFETY: main thread.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let tile: Retained<AnyObject> = msg_send![&*app, dockTile];
        let text = label.map(NSString::from_str);
        let _: () = msg_send![&*tile, setBadgeLabel: text.as_deref()];
    }
}

/// "Settings…  ⌘," in the app menu (the menu winit makes: About, separator, …).
pub fn add_settings_menu() {
    // SAFETY: main thread; the target is kept for the life of the app.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let main: Option<Retained<AnyObject>> = msg_send![&*app, mainMenu];
        let Some(main) = main else { return };
        let first: Option<Retained<AnyObject>> = msg_send![&*main, itemAtIndex: 0isize];
        let Some(sub) = first.and_then(|f| -> Option<Retained<AnyObject>> { msg_send![&*f, submenu] }) else { return };
        let target: Retained<Target> = msg_send![Target::class(), new];
        let item: Retained<AnyObject> = msg_send![msg_send![class!(NSMenuItem), alloc], initWithTitle: &*NSString::from_str("Settings…"), action: objc2::sel!(settings:), keyEquivalent: &*NSString::from_str(",")];
        let _: () = msg_send![&*item, setTarget: &*target];
        let sep: Retained<AnyObject> = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![&*sub, insertItem: &*item, atIndex: 2isize];
        let _: () = msg_send![&*sub, insertItem: &*sep, atIndex: 3isize];
        std::mem::forget(target);
    }
}

/// Settings window controls, by tag: the config key each one writes.
const SETTINGS: [&str; 11] = ["theme", "font", "font-size", "cursor", "cursor-blink", "background-opacity", "background-blur", "paste-warning", "restore", "tray", ""];
const THEMES: [&str; 3] = ["dark", "light", "auto"];
const CURSORS: [&str; 3] = ["block", "bar", "underline"];
const SIZES: [&str; 11] = ["10", "11", "12", "13", "14", "15", "16", "18", "20", "22", "24"];

thread_local! {
    /// The settings window once made (kept, so reopening it is instant), and its controls' target.
    static SETTINGS_WINDOW: RefCell<Option<(Retained<AnyObject>, Retained<Target>)>> = const { RefCell::new(None) };
}

/// Show the settings window: the common settings as native controls, applied as they change.
pub fn show_settings() {
    // SAFETY: AppKit calls on the main thread with valid receivers; objects are retained while used.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
        if let Some(win) = SETTINGS_WINDOW.with_borrow(|w| w.as_ref().map(|w| w.0.clone())) {
            let _: () = msg_send![&*win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
            return;
        }
        let target: Retained<Target> = msg_send![Target::class(), new];
        let c = crate::config::get();
        let raw = |key: &str| crate::config::value(key).unwrap_or_default();
        let str_ = |s: &str| NSString::from_str(s);
        // Tags are 1 + the index in SETTINGS (0 is every view's default).
        let tagged = |view: Retained<AnyObject>, tag: isize| -> Retained<AnyObject> {
            let _: () = msg_send![&*view, setTag: tag + 1];
            let _: () = msg_send![&*view, setTarget: &*target];
            let _: () = msg_send![&*view, setAction: objc2::sel!(changed:)];
            view
        };
        let label = |text: &str| -> Retained<AnyObject> { msg_send![class!(NSTextField), labelWithString: &*str_(text)] };
        let popup = |tag: isize, titles: &[&str], selected: usize| {
            let p: Retained<AnyObject> = msg_send![msg_send![class!(NSPopUpButton), alloc], initWithFrame: NSRect::ZERO, pullsDown: false];
            for t in titles {
                let _: () = msg_send![&*p, addItemWithTitle: &*str_(t)];
            }
            let _: () = msg_send![&*p, selectItemAtIndex: selected as isize];
            tagged(p, tag)
        };
        let check = |tag: isize, title: &str, on: bool| {
            let b: Retained<AnyObject> = msg_send![class!(NSButton), checkboxWithTitle: &*str_(title), target: &*target, action: objc2::sel!(changed:)];
            let _: () = msg_send![&*b, setState: on as isize];
            tagged(b, tag)
        };
        let theme = THEMES.iter().position(|t| *t == raw("theme")).unwrap_or(c.light as usize);
        let font: Retained<AnyObject> = msg_send![class!(NSTextField), textFieldWithString: &*str_(c.font.as_deref().unwrap_or(""))];
        let _: () = msg_send![&*font, setPlaceholderString: &*str_("Maple Mono (built in)")];
        let width: Retained<AnyObject> = msg_send![&*font, widthAnchor];
        let min: Retained<AnyObject> = msg_send![&*width, constraintGreaterThanOrEqualToConstant: 220.0f64];
        let _: () = msg_send![&*min, setActive: true];
        let font = tagged(font, 1);
        let size = format!("{}", c.font_size.unwrap_or(14.0));
        let slider: Retained<AnyObject> = msg_send![class!(NSSlider), sliderWithValue: (c.opacity * 100.0) as f64, minValue: 30.0f64, maxValue: 100.0f64, target: &*target, action: objc2::sel!(changed:)];
        let _: () = msg_send![&*slider, setContinuous: false];
        let slider = tagged(slider, 5);
        let open: Retained<AnyObject> = msg_send![class!(NSButton), buttonWithTitle: &*str_("Open Config File…"), target: &*target, action: objc2::sel!(changed:)];
        let empty = || -> Retained<AnyObject> { msg_send![class!(NSGridCell), emptyContentView] };
        let rows: Vec<[Retained<AnyObject>; 2]> = vec![
            [label("Theme:"), popup(0, &["Dark", "Light", "Match System"], theme)],
            [label("Font:"), font],
            [label("Size:"), popup(2, &SIZES, SIZES.iter().position(|s| *s == size).unwrap_or(4))],
            [label("Cursor:"), popup(3, &["Block", "Bar", "Underline"], c.cursor as usize)],
            [empty(), check(4, "Blink", c.cursor_blink)],
            [label("Background:"), slider],
            [empty(), check(6, "Blur behind the window", c.blur)],
            [label("Behaviour:"), check(7, "Ask before pasting several lines", c.paste_warning)],
            [empty(), check(8, "Reopen tabs and splits at launch", c.restore)],
            [empty(), check(9, "Show the hamster in the menu bar", c.tray)],
            [empty(), tagged(open, 10)],
        ];
        let array = |views: &[Retained<AnyObject>]| -> Retained<AnyObject> {
            let a: Retained<AnyObject> = msg_send![class!(NSMutableArray), new];
            for v in views {
                let _: () = msg_send![&*a, addObject: &**v];
            }
            a
        };
        let row_arrays: Vec<Retained<AnyObject>> = rows.iter().map(|r| array(r)).collect();
        let grid: Retained<AnyObject> = msg_send![class!(NSGridView), gridViewWithViews: &*array(&row_arrays)];
        let _: () = msg_send![&*grid, setRowSpacing: 10.0f64];
        let _: () = msg_send![&*grid, setColumnSpacing: 12.0f64];
        let first: Retained<AnyObject> = msg_send![&*grid, columnAtIndex: 0isize];
        let _: () = msg_send![&*first, setXPlacement: 3isize]; // NSGridCellPlacementTrailing
        let fit: NSSize = msg_send![&*grid, fittingSize];
        let (w, h) = (fit.width.max(360.0), fit.height);
        let _: () = msg_send![&*grid, setFrame: NSRect::new(NSPoint::new(24.0, 20.0), NSSize::new(w, h))];
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w + 48.0, h + 40.0));
        // Titled | closable; buffered backing.
        let win: Retained<AnyObject> = msg_send![msg_send![class!(NSWindow), alloc], initWithContentRect: rect, styleMask: 3usize, backing: 2usize, defer: false];
        let _: () = msg_send![&*win, setReleasedWhenClosed: false];
        let _: () = msg_send![&*win, setTitle: &*str_("litty Settings")];
        let content: Retained<AnyObject> = msg_send![&*win, contentView];
        let _: () = msg_send![&*content, addSubview: &*grid];
        let _: () = msg_send![&*win, center];
        let _: () = msg_send![&*win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        SETTINGS_WINDOW.with_borrow_mut(|w| *w = Some((win, target)));
    }
}

/// A control in the settings window changed: write its key and tell the app.
fn setting_changed(control: &AnyObject) {
    // SAFETY: the control is one of the settings window's, on the main thread.
    let (tag, value) = unsafe {
        let tag: isize = msg_send![control, tag];
        let tag = tag - 1;
        let value = match tag {
            0 | 2 | 3 => {
                let i: isize = msg_send![control, indexOfSelectedItem];
                let list: &[&str] = match tag {
                    0 => &THEMES,
                    2 => &SIZES,
                    _ => &CURSORS,
                };
                list.get(i as usize).unwrap_or(&list[0]).to_string()
            }
            1 => {
                let s: Retained<NSString> = msg_send![control, stringValue];
                s.to_string().trim().to_string()
            }
            5 => {
                let v: f64 = msg_send![control, doubleValue];
                format!("{:.2}", v / 100.0)
            }
            10 => {
                if let Some(p) = crate::config::file() {
                    let _ = std::process::Command::new("open").arg("-t").arg(p).spawn();
                }
                return;
            }
            _ => {
                let on: isize = msg_send![control, state];
                (on != 0).to_string()
            }
        };
        (tag, value)
    };
    let Some(key) = SETTINGS.get(tag as usize) else { return };
    crate::config::set(key, &value);
    if let Some(sink) = SINK.get() {
        sink(TrayAction::Settings);
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

#[cfg(test)]
mod tests {
    #[test]
    fn hotkey_key_codes_match_apple_kvk() {
        let k = |n| super::key_code(n);
        assert_eq!([k("a"), k("0"), k("]"), k("p"), k("l"), k("\\"), k("."), k("`"), k("space"), k("f1"), k("f12")], [0x00, 0x1D, 0x1E, 0x23, 0x25, 0x2A, 0x2F, 0x32, 0x31, 0x7A, 0x6F].map(Some));
        assert_eq!((k("?"), k("f13"), k("nope")), (None, None, None));
    }
}
