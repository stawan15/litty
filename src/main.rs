mod boxdraw;
mod font;
mod grid;
mod present;
mod render;
mod update;

use grid::Grid;
use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty};
use present::Presenter;
use render::{PaneView, Rect, Renderer, TabHit};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use vte::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, UserAttentionType, Window, WindowId};

nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);

/// Debug aid: with `LITTY_LOG=file`, every PTY read, key/paste sent and resize is appended
/// with a timestamp, so display bugs can be replayed exactly.
fn debug_log(tag: &str, bytes: &[u8]) {
    static LOG: OnceLock<Option<(Mutex<File>, Instant)>> = OnceLock::new();
    let log = LOG.get_or_init(|| {
        let path = std::env::var_os("LITTY_LOG")?;
        Some((Mutex::new(File::create(path).ok()?), Instant::now()))
    });
    if let Some((file, t0)) = log {
        let _ = writeln!(file.lock().unwrap(), "{:>9.3} {tag} {:?}", t0.elapsed().as_secs_f32(), String::from_utf8_lossy(bytes));
    }
}

struct Term {
    grid: Grid,
    parser: Parser,
}

enum Ev {
    /// New output arrived.
    Wake,
    /// The shell of the pane with this id exited.
    Exit(usize),
    /// Last tab closed.
    Quit,
    /// Progress of the self-updater.
    Update(update::Event),
}

struct Pane {
    id: usize,
    term: Arc<Mutex<Term>>,
    master: File,
    pending: Arc<AtomicBool>,
    pid: i32,
    /// Set by the reader thread once the child has been reaped (so its pid must not be signalled).
    exited: Arc<AtomicBool>,
}

/// Split layout of a tab: leaves are pane ids.
enum Node {
    Leaf(usize),
    Split {
        /// True: panes side by side; false: stacked.
        vertical: bool,
        /// Share of the space given to `a`.
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

impl Node {
    fn leaves(&self, out: &mut Vec<usize>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    /// Replace leaf `target` with a split holding it and `new_id`.
    fn split(&mut self, target: usize, vertical: bool, new_id: usize) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split { vertical, ratio: 0.5, a: Box::new(Node::Leaf(target)), b: Box::new(Node::Leaf(new_id)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, vertical, new_id) || b.split(target, vertical, new_id),
        }
    }

    /// The tree without leaf `target` (its sibling takes the parent's place); None if it was the only leaf.
    fn remove(self, target: usize) -> Option<Node> {
        match self {
            Node::Leaf(id) => (id != target).then_some(Node::Leaf(id)),
            Node::Split { vertical, ratio, a, b } => match (a.remove(target), b.remove(target)) {
                (Some(a), Some(b)) => Some(Node::Split { vertical, ratio, a: Box::new(a), b: Box::new(b) }),
                (a, b) => a.or(b),
            },
        }
    }

    /// Branch choices (false = a, true = b) leading to leaf `target`.
    fn path_to(&self, target: usize) -> Option<Vec<bool>> {
        match self {
            Node::Leaf(id) => (*id == target).then(Vec::new),
            Node::Split { a, b, .. } => {
                if let Some(mut p) = a.path_to(target) {
                    p.insert(0, false);
                    Some(p)
                } else {
                    b.path_to(target).map(|mut p| {
                        p.insert(0, true);
                        p
                    })
                }
            }
        }
    }

    fn node_at_mut(&mut self, path: &[bool]) -> Option<&mut Node> {
        match (self, path.split_first()) {
            (node, None) => Some(node),
            (Node::Split { a, b, .. }, Some((&right, rest))) => (if right { b } else { a }).node_at_mut(rest),
            _ => None,
        }
    }

    fn equalize(&mut self) {
        if let Node::Split { ratio, a, b, .. } = self {
            *ratio = 0.5;
            a.equalize();
            b.equalize();
        }
    }
}

/// A gap between two panes that can be dragged.
struct Divider {
    rect: Rect,
    vertical: bool,
    path: Vec<bool>,
    /// The area of the split this divider belongs to (ratio = position within it).
    container: Rect,
}

struct Tab {
    panes: Vec<Pane>,
    root: Node,
    /// Id of the focused pane.
    active: usize,
    zoom: bool,
    layout: Vec<(usize, Rect)>,
    dividers: Vec<Divider>,
}

/// Compute pane rectangles for `node` inside `rect`. Pane sizes snap to whole cells.
fn layout(node: &Node, rect: Rect, cell: (usize, usize), gap: usize, out: &mut Vec<(usize, Rect)>, dividers: &mut Vec<Divider>, path: &mut Vec<bool>) {
    let Node::Split { vertical, ratio, a, b } = node else {
        if let Node::Leaf(id) = node {
            out.push((*id, rect));
        }
        return;
    };
    let (len, unit) = if *vertical { (rect.w, cell.0) } else { (rect.h, cell.1) };
    let avail = len.saturating_sub(gap);
    let min = (4 * unit).min(avail / 2);
    let first = (((avail as f32 * ratio) as usize / unit) * unit).clamp(min, avail.saturating_sub(min).max(min));
    let (ra, rd, rb) = if *vertical {
        (Rect { w: first, ..rect }, Rect { x: rect.x + first, w: gap, ..rect }, Rect { x: rect.x + first + gap, w: avail - first, ..rect })
    } else {
        (Rect { h: first, ..rect }, Rect { y: rect.y + first, h: gap, ..rect }, Rect { y: rect.y + first + gap, h: avail - first, ..rect })
    };
    dividers.push(Divider { rect: rd, vertical: *vertical, path: path.clone(), container: rect });
    path.push(false);
    layout(a, ra, cell, gap, out, dividers, path);
    path.pop();
    path.push(true);
    layout(b, rb, cell, gap, out, dividers, path);
    path.pop();
}

struct App {
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    proxy: EventLoopProxy<Ev>,
    /// Command for the first pane (`-e cmd`); later panes always run a login shell.
    initial_command: Vec<String>,
    mods: ModifiersState,
    window: Option<Arc<Window>>,
    presenter: Option<Presenter>,
    renderer: Option<Renderer>,
    font_pt: f32,
    scale: f32,
    cursor: (f64, f64),
    /// Mouse button code currently held while an application tracks the mouse.
    held: Option<u8>,
    last_cell: (usize, usize),
    selecting: bool,
    anchor: (u64, usize),
    /// Previous left click: when, pane, cell; and how many clicks in a row (for word/line select).
    last_click: Option<(Instant, usize, usize, usize)>,
    click_count: u8,
    wheel_acc: f32,
    ime_pos: (usize, usize),
    next_frame: Instant,
    /// Wake-up needed to draw a frame that was postponed by the frame cap.
    frame_deadline: Option<Instant>,
    blink_on: bool,
    next_blink: Instant,
    focused: bool,
    rail_drag: bool,
    /// Divider being dragged (index into the active tab's dividers).
    divider_drag: Option<usize>,
    icon: CursorIcon,
    /// Find bar query while the bar is open.
    find: Option<String>,
    update: update::State,
    /// The update prompt (Cmd+Shift+U) is showing instead of the short notice.
    update_open: bool,
    /// The package-manager command for updating, when litty may not replace itself.
    update_managed: Option<String>,
}

const DEFAULT_PT: f32 = 14.0;
const PAD_PT: f32 = 10.0;
/// Minimum time between frames. Drawing holds the grid lock, so capping it keeps the parser
/// thread fed during floods like `cat bigfile`.
const FRAME: Duration = Duration::from_millis(15);
const BLINK: Duration = Duration::from_millis(530);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Makes zsh report prompt and command boundaries (OSC 133) so the terminal can draw command
/// blocks. The temporary ZDOTDIR only holds a .zshenv that restores the user's own ZDOTDIR and
/// sources their .zshenv before adding the hooks.
const ZSH_INTEGRATION: &str = r#"# litty shell integration (OSC 133 prompt marks).
if [[ -n "$LT_ORIG_ZDOTDIR" ]]; then ZDOTDIR="$LT_ORIG_ZDOTDIR"; else unset ZDOTDIR; fi
unset LT_ORIG_ZDOTDIR
[[ -f "${ZDOTDIR:-$HOME}/.zshenv" ]] && source "${ZDOTDIR:-$HOME}/.zshenv"
if [[ -o interactive ]]; then
  __lt_precmd() { printf '\e]133;D;%s\a\e]133;A\a\e]7;file://%s%s\a' "$?" "$HOST" "${PWD// /%20}"; }
  __lt_preexec() { printf '\e]133;C\a'; }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __lt_precmd
  add-zsh-hook preexec __lt_preexec
fi
"#;

fn install_zsh_integration() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let dir = cache.join("litty/zsh");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(dir.join(".zshenv"), ZSH_INTEGRATION).ok()?;
    Some(dir)
}

/// Look `name` up in PATH (the child is started with execve, which does no lookup itself).
fn resolve_program(name: &str) -> Option<CString> {
    let path = if name.contains('/') {
        PathBuf::from(name)
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())?
    };
    CString::new(path.to_str()?).ok()
}

/// Start `command` (`litty -e cmd args...`), or a login shell if empty, on a new PTY, in
/// `cwd` if given. Returns the PTY master and the child's pid.
///
/// Everything the child needs is built before forking: other threads exist by now, so the child
/// may only call async-signal-safe functions (chdir, execve, _exit) and the environment is
/// passed explicitly instead of mutating our own.
fn spawn_shell(size: &Winsize, command: &[String], cwd: Option<&str>) -> Option<(File, i32)> {
    let login = command.is_empty();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let (path, argv): (CString, Vec<CString>) = if login {
        // Login shell, like other terminals: argv[0] is "-name".
        let name = format!("-{}", shell.rsplit('/').next().unwrap_or("sh"));
        (CString::new(shell.clone()).ok()?, vec![CString::new(name).ok()?])
    } else {
        (resolve_program(&command[0])?, command.iter().map(|a| CString::new(a.as_str()).ok()).collect::<Option<_>>()?)
    };

    let mut env: Vec<(String, String)> =
        std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect();
    let mut set = |k: &str, v: String| {
        env.retain(|(ek, _)| ek != k);
        env.push((k.to_string(), v));
    };
    set("TERM", "xterm-256color".into());
    set("COLORTERM", "truecolor".into());
    if std::env::var_os("LANG").is_none() {
        set("LANG", "en_US.UTF-8".into());
    }
    if login && shell.ends_with("/zsh") {
        if let Some(dir) = install_zsh_integration() {
            set("LT_ORIG_ZDOTDIR", std::env::var("ZDOTDIR").unwrap_or_default());
            set("ZDOTDIR", dir.to_string_lossy().into_owned());
        }
    }
    let envp: Vec<CString> = env.iter().filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok()).collect();
    let cwd = cwd.and_then(|c| CString::new(c).ok());

    // SAFETY: the child only chdirs and execs (or exits), using data prepared above.
    match unsafe { forkpty(Some(size), None).ok()? } {
        ForkptyResult::Parent { master, child } => Some((unsafe { File::from_raw_fd(master.into_raw_fd()) }, child.as_raw())),
        ForkptyResult::Child => unsafe {
            if let Some(dir) = &cwd {
                libc::chdir(dir.as_ptr());
            }
            let _ = nix::unistd::execve(&path, &argv, &envp);
            libc::_exit(127)
        },
    }
}

/// Reader thread: parses PTY output straight into the grid (the lock gives backpressure), then
/// reaps the child and reports the exit.
fn spawn_reader(id: usize, mut pty: File, term: Arc<Mutex<Term>>, pending: Arc<AtomicBool>, exited: Arc<AtomicBool>, pid: i32, proxy: EventLoopProxy<Ev>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = match pty.read(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            debug_log("out", &buf[..n]);
            let reply = {
                let mut t = term.lock().unwrap();
                let Term { grid, parser } = &mut *t;
                parser.advance(grid, &buf[..n]);
                std::mem::take(&mut grid.reply)
            };
            if !reply.is_empty() {
                let _ = pty.write_all(&reply);
            }
            if !pending.swap(true, Ordering::SeqCst) && proxy.send_event(Ev::Wake).is_err() {
                return;
            }
        }
        // SAFETY: reaps our own child; the flag stops anyone signalling a recycled pid afterwards.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        exited.store(true, Ordering::SeqCst);
        let _ = proxy.send_event(Ev::Exit(id));
    });
}

/// pbcopy/pbpaste convert text with the locale's encoding, and an app launched from Finder or the
/// Dock has no locale: without this, pasted and copied Thai text turns into `?` and mojibake.
fn clipboard_command(cmd: &str, args: &[&str]) -> Command {
    let mut c = Command::new(cmd);
    c.args(args);
    if ["LANG", "LC_ALL", "LC_CTYPE"].iter().all(|k| std::env::var_os(k).is_none()) {
        c.env("LANG", "en_US.UTF-8");
    }
    c
}

fn clipboard() -> Option<Vec<u8>> {
    [("pbpaste", &[][..]), ("wl-paste", &["-n"]), ("xclip", &["-o", "-selection", "clipboard"])]
        .iter()
        .find_map(|(cmd, args)| clipboard_command(cmd, args).output().ok().filter(|o| o.status.success()))
        .map(|o| o.stdout)
}

fn clipboard_set(text: &[u8]) {
    for (cmd, args) in [("pbcopy", &[][..]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard", "-i"])] {
        let child = clipboard_command(cmd, args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        if let Ok(mut c) = child {
            if let Some(mut stdin) = c.stdin.take() {
                let _ = stdin.write_all(text);
            }
            let _ = c.wait();
            return;
        }
    }
}

/// xterm mouse report for the given button `code` (0/1/2 buttons, 3 none, 64/65 wheel, +32 motion).
fn mouse_report(g: &Grid, code: u8, mods: ModifiersState, col: usize, row: usize, release: bool) -> Vec<u8> {
    let m = mods.alt_key() as u8 * 8 + mods.control_key() as u8 * 16;
    if g.mouse_sgr {
        format!("\x1b[<{};{};{}{}", code + m, col + 1, row + 1, if release { 'm' } else { 'M' }).into_bytes()
    } else if col >= 223 || row >= 223 {
        Vec::new()
    } else {
        let c = if release { 3 + m } else { code + m };
        vec![0x1b, b'[', b'M', 32 + c, 33 + col as u8, 33 + row as u8]
    }
}

fn key_bytes(e: &KeyEvent, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let m = 1 + mods.shift_key() as u8 + 2 * mods.alt_key() as u8 + 4 * mods.control_key() as u8;
    let alt_only = mods.alt_key() && !mods.shift_key() && !mods.control_key();
    let csi1 = |c: char, ss3: bool| {
        match (m > 1, ss3) {
            (true, _) => format!("\x1b[1;{m}{c}"),
            (false, true) => format!("\x1bO{c}"),
            (false, false) => format!("\x1b[{c}"),
        }
        .into_bytes()
    };
    let tilde = |n: u8| if m > 1 { format!("\x1b[{n};{m}~") } else { format!("\x1b[{n}~") }.into_bytes();

    let mut out = match &e.logical_key {
        Key::Named(n) => match n {
            // Keys that carry their own modifier encoding never get the Alt/ESC prefix.
            NamedKey::ArrowUp => return Some(csi1('A', app_cursor)),
            NamedKey::ArrowDown => return Some(csi1('B', app_cursor)),
            NamedKey::ArrowRight if alt_only => return Some(b"\x1bf".to_vec()),
            NamedKey::ArrowLeft if alt_only => return Some(b"\x1bb".to_vec()),
            NamedKey::ArrowRight => return Some(csi1('C', app_cursor)),
            NamedKey::ArrowLeft => return Some(csi1('D', app_cursor)),
            NamedKey::Home => return Some(csi1('H', app_cursor)),
            NamedKey::End => return Some(csi1('F', app_cursor)),
            NamedKey::F1 => return Some(csi1('P', true)),
            NamedKey::F2 => return Some(csi1('Q', true)),
            NamedKey::F3 => return Some(csi1('R', true)),
            NamedKey::F4 => return Some(csi1('S', true)),
            NamedKey::F5 => return Some(tilde(15)),
            NamedKey::F6 => return Some(tilde(17)),
            NamedKey::F7 => return Some(tilde(18)),
            NamedKey::F8 => return Some(tilde(19)),
            NamedKey::F9 => return Some(tilde(20)),
            NamedKey::F10 => return Some(tilde(21)),
            NamedKey::F11 => return Some(tilde(23)),
            NamedKey::F12 => return Some(tilde(24)),
            NamedKey::PageUp => return Some(tilde(5)),
            NamedKey::PageDown => return Some(tilde(6)),
            NamedKey::Delete => return Some(tilde(3)),
            NamedKey::Insert => return Some(tilde(2)),
            NamedKey::Enter => b"\r".to_vec(),
            NamedKey::Backspace => vec![0x7f],
            NamedKey::Tab if mods.shift_key() => b"\x1b[Z".to_vec(),
            NamedKey::Tab => b"\t".to_vec(),
            NamedKey::Escape => vec![0x1b],
            NamedKey::Space if mods.control_key() => vec![0],
            _ => e.text.as_ref()?.as_bytes().to_vec(),
        },
        Key::Character(_) => {
            // With Option held, macOS composes special characters; use the bare key as Meta.
            let key = if mods.alt_key() { e.key_without_modifiers() } else { e.logical_key.clone() };
            let Key::Character(s) = key else { return None };
            let c = s.chars().next()?;
            if mods.control_key() && s.chars().count() == 1 && c.is_ascii() {
                match c.to_ascii_lowercase() {
                    'a'..='z' => vec![c.to_ascii_lowercase() as u8 & 0x1f],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    _ => return None,
                }
            } else if mods.alt_key() {
                s.as_bytes().to_vec()
            } else {
                e.text.as_ref()?.as_bytes().to_vec()
            }
        }
        _ => return None,
    };
    if mods.alt_key() {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Where the window size and zoom are remembered between runs.
fn state_file() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache.join("litty/window"))
}

/// (logical width, logical height, font size in points) saved by the previous run.
fn load_state() -> Option<(f64, f64, f32)> {
    let text = std::fs::read_to_string(state_file()?).ok()?;
    let mut it = text.split_whitespace();
    let (w, h, pt): (f64, f64, f32) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    ((200.0..10000.0).contains(&w) && (100.0..10000.0).contains(&h) && (6.0..=48.0).contains(&pt)).then_some((w, h, pt))
}

impl App {
    fn redraw_soon(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn pane(&self) -> &Pane {
        let t = self.tab();
        t.panes.iter().find(|p| p.id == t.active).expect("active pane exists")
    }

    fn term(&self) -> &Arc<Mutex<Term>> {
        &self.pane().term
    }

    fn active_rect(&self) -> Rect {
        let t = self.tab();
        t.layout.iter().find(|(id, _)| *id == t.active).map(|(_, r)| *r).unwrap_or_default()
    }

    /// The pane under pixel (x, y) in the active tab.
    fn pane_at(&self, x: f64, y: f64) -> Option<(usize, Rect)> {
        self.tab().layout.iter().find(|(_, r)| r.contains(x, y)).copied()
    }

    fn resize(&mut self, w: u32, h: u32) {
        let (Some(p), Some(r)) = (&mut self.presenter, &mut self.renderer) else { return };
        if w == 0 || h == 0 {
            return;
        }
        p.resize(w as usize, h as usize);
        r.resize(w as usize, h as usize);
        self.relayout();
    }

    /// Recompute every tab's pane rectangles and resize their grids and PTYs to match. Called
    /// when the window, font, tab count or split layout changes.
    fn relayout(&mut self) {
        let Some(r) = &mut self.renderer else { return };
        r.bar_h = if self.tabs.len() > 1 { r.tab_bar_height() } else { 0 };
        let area = r.area();
        let cell = (r.fonts.cell_w, r.fonts.cell_h);
        let gap = 6 * r.unit();
        for tab in &mut self.tabs {
            tab.layout.clear();
            tab.dividers.clear();
            if tab.zoom {
                tab.layout.push((tab.active, area));
            } else {
                layout(&tab.root, area, cell, gap, &mut tab.layout, &mut tab.dividers, &mut Vec::new());
            }
            for (id, rect) in &tab.layout {
                let Some(pane) = tab.panes.iter().find(|p| p.id == *id) else { continue };
                let (cols, rows) = r.grid_size(*rect);
                debug_log("size", format!("pane {id}: {cols}x{rows}").as_bytes());
                pane.term.lock().unwrap().grid.resize(cols, rows);
                let ws = Winsize { ws_row: rows as u16, ws_col: cols as u16, ws_xpixel: rect.w as u16, ws_ypixel: rect.h as u16 };
                let _ = unsafe { tiocswinsz(pane.master.as_raw_fd(), &ws) };
            }
            for pane in &tab.panes {
                pane.term.lock().unwrap().grid.dirty.fill(true);
            }
        }
        r.clear();
        self.redraw_soon();
    }

    /// Rebuild fonts for the current point size / display scale, then re-layout.
    fn rebuild(&mut self) {
        self.renderer = Some(Renderer::new(self.font_pt * self.scale, (PAD_PT * self.scale) as usize));
        if let (Some(size), Some(r), Some(p)) = (self.window.as_ref().map(|w| w.inner_size()), &mut self.renderer, &mut self.presenter) {
            p.resize(size.width as usize, size.height as usize);
            r.resize(size.width as usize, size.height as usize);
        }
        self.relayout();
    }

    fn redraw(&mut self) {
        // Titles for the tab bar, taken before locking any pane (never hold two locks).
        let titles: Vec<String> = if self.tabs.len() > 1 {
            self.tabs
                .iter()
                .map(|t| {
                    let title = t.panes.iter().find(|p| p.id == t.active).map(|p| p.term.lock().unwrap().grid.tab_title.clone()).unwrap_or_default();
                    if title.is_empty() { "shell".to_string() } else { title }
                })
                .collect()
        } else {
            Vec::new()
        };
        let notice = self.update_notice();
        let mut attention = false;
        let mut clip = None;
        for pane in self.tabs.iter().flat_map(|t| &t.panes) {
            pane.pending.store(false, Ordering::SeqCst);
            let mut t = pane.term.lock().unwrap();
            attention |= std::mem::take(&mut t.grid.attention);
            clip = clip.or(t.grid.clip.take());
        }
        let (Some(win), Some(presenter), Some(r)) = (&self.window, &mut self.presenter, &mut self.renderer) else { return };
        let tab = &self.tabs[self.active];
        let mut title = None;
        let mut cursor = (0, 0);
        for (id, rect) in &tab.layout {
            let Some(pane) = tab.panes.iter().find(|p| p.id == *id) else { continue };
            let focused = *id == tab.active;
            let mut t = pane.term.lock().unwrap();
            let cursor_on = self.blink_on || !t.grid.cursor_blink;
            let view = PaneView { rect: *rect, focused, cursor_on, find: if focused { self.find.as_deref() } else { None }, notice: if focused { notice.as_deref() } else { None } };
            r.draw_pane(&mut t.grid, &view);
            if focused {
                title = t.grid.title.take();
                cursor = r.cursor_px(&t.grid, *rect);
            }
        }
        let dividers: Vec<Rect> = tab.dividers.iter().map(|d| d.rect).collect();
        let single = (tab.layout.len() == 1).then(|| tab.panes.iter().find(|p| p.id == tab.active)).flatten();
        match single {
            Some(pane) => r.draw_chrome(&dividers, Some(&pane.term.lock().unwrap().grid), &titles, self.active),
            None => r.draw_chrome(&dividers, None, &titles, self.active),
        }
        let damage = r.take_damage();
        presenter.present(&r.fb, damage);
        if let Some(title) = title {
            win.set_title(&title);
        }
        if attention && !self.focused {
            win.request_user_attention(Some(UserAttentionType::Informational));
        }
        if let Some(clip) = clip {
            clipboard_set(&clip);
        }
        if cursor != self.ime_pos {
            self.ime_pos = cursor;
            let (pos, size) = (PhysicalPosition::new(cursor.0 as i32, cursor.1 as i32), PhysicalSize::new(r.fonts.cell_w as u32, r.fonts.cell_h as u32));
            win.set_ime_cursor_area(pos, size);
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        debug_log("in ", bytes);
        let id = self.tab().active;
        let tab = &mut self.tabs[self.active];
        if let Some(p) = tab.panes.iter_mut().find(|p| p.id == id) {
            let _ = p.master.write_all(bytes);
        }
    }

    // ---- tabs and panes ----

    fn spawn_pane(&mut self, command: &[String], cwd: Option<&str>) -> Option<Pane> {
        let (cols, rows) = self.renderer.as_ref().map_or((80, 24), |r| r.grid_size(r.area()));
        let size = Winsize { ws_row: rows as u16, ws_col: cols as u16, ws_xpixel: 0, ws_ypixel: 0 };
        let Some((master, pid)) = spawn_shell(&size, command, cwd) else {
            eprintln!("litty: failed to start a shell");
            return None;
        };
        let term = Arc::new(Mutex::new(Term { grid: Grid::new(cols, rows), parser: Parser::new() }));
        let (pending, exited) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let id = self.next_id;
        self.next_id += 1;
        spawn_reader(id, master.try_clone().ok()?, term.clone(), pending.clone(), exited.clone(), pid, self.proxy.clone());
        Some(Pane { id, term, master, pending, pid, exited })
    }

    /// Open a tab running `command` (or a login shell) and switch to it.
    fn new_tab(&mut self, command: &[String], cwd: Option<String>) {
        let Some(pane) = self.spawn_pane(command, cwd.as_deref()) else { return };
        let id = pane.id;
        self.tabs.push(Tab { panes: vec![pane], root: Node::Leaf(id), active: id, zoom: false, layout: Vec::new(), dividers: Vec::new() });
        self.relayout();
        self.activate(self.tabs.len() - 1);
    }

    /// Working directory of the focused pane (as reported by the shell via OSC 7).
    fn current_dir(&self) -> Option<String> {
        self.term().lock().unwrap().grid.cwd.clone()
    }

    fn open_tab(&mut self) {
        let cwd = self.current_dir();
        self.new_tab(&[], cwd);
    }

    /// Split the focused pane: `vertical` puts the new pane to its right, otherwise below.
    fn split(&mut self, vertical: bool) {
        let cwd = self.current_dir();
        let Some(pane) = self.spawn_pane(&[], cwd.as_deref()) else { return };
        let new_id = pane.id;
        let tab = &mut self.tabs[self.active];
        tab.zoom = false;
        if !tab.root.split(tab.active, vertical, new_id) {
            return;
        }
        tab.panes.push(pane);
        self.relayout();
        self.focus_pane(new_id);
    }

    /// Reset per-pane interaction state (selection, mouse, find) when focus moves.
    fn reset_interaction(&mut self) {
        self.find = None;
        (self.selecting, self.held, self.wheel_acc, self.rail_drag, self.divider_drag) = (false, None, 0.0, false, None);
        self.click_count = 0;
    }

    fn focus_pane(&mut self, id: usize) {
        if self.tab().active == id {
            return;
        }
        self.reset_interaction();
        // The old focused pane is dimmed and the new one lit up: both need repainting.
        for p in &self.tab().panes {
            p.term.lock().unwrap().grid.dirty.fill(true);
        }
        let tab = &mut self.tabs[self.active];
        tab.active = id;
        let title = self.term().lock().unwrap().grid.win_title.clone();
        if let Some(w) = &self.window {
            w.set_title(if title.is_empty() { "litty" } else { &title });
        }
        self.redraw_soon();
    }

    fn activate(&mut self, i: usize) {
        self.active = i;
        self.reset_interaction();
        for p in &self.tabs[i].panes {
            let mut t = p.term.lock().unwrap();
            t.grid.dirty.fill(true);
            t.grid.matches.clear();
        }
        if let Some(r) = &mut self.renderer {
            r.clear();
        }
        let title = self.term().lock().unwrap().grid.win_title.clone();
        if let Some(w) = &self.window {
            w.set_title(if title.is_empty() { "litty" } else { &title });
        }
        self.redraw_soon();
    }

    /// Close pane `id` of tab `t`; closes the tab with its last pane. `hangup` also signals the
    /// shell (not needed when it already exited).
    fn close_pane(&mut self, t: usize, id: usize, hangup: bool) {
        let Some(tab) = self.tabs.get_mut(t) else { return };
        let Some(pos) = tab.panes.iter().position(|p| p.id == id) else { return };
        let pane = tab.panes.remove(pos);
        if hangup && !pane.exited.load(Ordering::SeqCst) {
            // SAFETY: the child has not been reaped, so the pid is still ours.
            unsafe { libc::kill(pane.pid, libc::SIGHUP) };
        }
        let mut order = Vec::new();
        tab.root.leaves(&mut order);
        let at = order.iter().position(|&l| l == id).unwrap_or(0);
        let root = std::mem::replace(&mut tab.root, Node::Leaf(0));
        match root.remove(id) {
            Some(root) => {
                tab.root = root;
                tab.zoom = false;
                if tab.active == id {
                    let mut left = Vec::new();
                    tab.root.leaves(&mut left);
                    // Prefer the pane that was next to the closed one.
                    tab.active = left[at.saturating_sub(1).min(left.len() - 1)];
                }
                self.relayout();
                if t == self.active {
                    let active = self.active;
                    self.activate(active);
                }
            }
            None => self.close_tab_at(t),
        }
    }

    fn close_tab_at(&mut self, i: usize) {
        let tab = self.tabs.remove(i);
        for pane in tab.panes {
            if !pane.exited.load(Ordering::SeqCst) {
                // SAFETY: the child has not been reaped, so the pid is still ours.
                unsafe { libc::kill(pane.pid, libc::SIGHUP) };
            }
        }
        if self.tabs.is_empty() {
            let _ = self.proxy.send_event(Ev::Quit);
            return;
        }
        let active = if i < self.active { self.active - 1 } else { self.active.min(self.tabs.len() - 1) };
        self.relayout();
        self.activate(active);
    }

    /// Cmd+W: close the focused pane, or the tab if it is the last one.
    fn close_active(&mut self) {
        let (t, id) = (self.active, self.tab().active);
        self.close_pane(t, id, true);
    }

    fn cycle_tab(&mut self, dir: isize) {
        let n = self.tabs.len() as isize;
        if n > 1 {
            self.activate((self.active as isize + dir).rem_euclid(n) as usize);
        }
    }

    /// Cmd+1..8 select that tab, Cmd+9 the last one.
    fn goto_tab(&mut self, n: usize) {
        let i = if n >= 9 { self.tabs.len() - 1 } else { n - 1 };
        if i < self.tabs.len() {
            self.activate(i);
        }
    }

    fn cycle_pane(&mut self, dir: isize) {
        let mut order = Vec::new();
        self.tab().root.leaves(&mut order);
        if order.len() > 1 {
            let at = order.iter().position(|&l| l == self.tab().active).unwrap_or(0) as isize;
            self.focus_pane(order[(at + dir).rem_euclid(order.len() as isize) as usize]);
        }
    }

    /// Focus the nearest pane in direction (dx, dy).
    fn focus_dir(&mut self, dx: isize, dy: isize) {
        let center = |r: Rect| ((r.x + r.w / 2) as isize, (r.y + r.h / 2) as isize);
        let (ax, ay) = center(self.active_rect());
        let active = self.tab().active;
        let best = self
            .tab()
            .layout
            .iter()
            .filter(|(id, _)| *id != active)
            .filter_map(|(id, r)| {
                let (x, y) = center(*r);
                let along = dx * (x - ax) + dy * (y - ay);
                let across = if dx != 0 { (y - ay).abs() } else { (x - ax).abs() };
                (along > 0).then_some((along + 2 * across, *id))
            })
            .min();
        if let Some((_, id)) = best {
            self.focus_pane(id);
        }
    }

    fn toggle_zoom(&mut self) {
        let tab = &mut self.tabs[self.active];
        if tab.panes.len() > 1 {
            tab.zoom = !tab.zoom;
            self.relayout();
        }
    }

    /// Move the divider nearest above the focused pane in direction (dx, dy) by 5%.
    fn resize_split(&mut self, dx: isize, dy: isize) {
        let tab = &mut self.tabs[self.active];
        let Some(path) = tab.root.path_to(tab.active) else { return };
        let want_vertical = dx != 0;
        for level in (0..path.len()).rev() {
            if let Some(Node::Split { vertical, ratio, .. }) = tab.root.node_at_mut(&path[..level]) {
                if *vertical == want_vertical {
                    *ratio = (*ratio + 0.05 * (dx + dy) as f32).clamp(0.1, 0.9);
                    tab.zoom = false;
                    self.relayout();
                    return;
                }
            }
        }
    }

    fn equalize_splits(&mut self) {
        self.tabs[self.active].root.equalize();
        self.relayout();
    }

    /// Cmd+K: clear screen and scrollback, then have the shell redraw its prompt.
    fn clear_screen(&mut self) {
        let in_alt = {
            let mut t = self.term().lock().unwrap();
            t.grid.clear_all();
            t.grid.in_alt
        };
        if !in_alt {
            self.send(b"\x0c");
        }
        self.redraw_soon();
    }

    /// Cmd+N: a new independent window (a new process).
    fn new_window(&self) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        }
    }

    fn save_state(&self) {
        let (Some(path), Some(win)) = (state_file(), &self.window) else { return };
        let size = win.inner_size().to_logical::<f64>(win.scale_factor());
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{} {} {}\n", size.width.round(), size.height.round(), self.font_pt));
    }

    /// Send user input: snaps the view back to the live screen and clears any selection.
    fn send_input(&mut self, bytes: &[u8]) {
        let changed = {
            let mut t = self.term().lock().unwrap();
            let g = &mut t.grid;
            let changed = g.scroll > 0 || g.sel.is_some();
            if changed {
                g.scroll = 0;
                g.sel = None;
                g.dirty.fill(true);
            }
            changed
        };
        // Keep the cursor visible while typing.
        (self.blink_on, self.next_blink) = (true, Instant::now() + BLINK);
        self.send(bytes);
        if changed {
            self.redraw_soon();
        }
    }

    fn paste(&mut self) {
        let Some(text) = clipboard() else { return };
        let bracketed = self.term().lock().unwrap().grid.bracketed_paste;
        let mut out = Vec::with_capacity(text.len() + 12);
        if bracketed {
            out.extend(b"\x1b[200~");
            out.extend(&text);
            out.extend(b"\x1b[201~");
        } else {
            out.extend(text.iter().map(|&b| if b == b'\n' { b'\r' } else { b }));
        }
        self.send_input(&out);
    }

    fn copy(&mut self) {
        let text = self.term().lock().unwrap().grid.selection_text();
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            clipboard_set(text.as_bytes());
        }
    }

    /// Cmd (macOS) or Ctrl+Shift shortcuts. Returns whether the key was consumed.
    fn shortcut(&mut self, key: &str) -> bool {
        let shift = self.mods.shift_key();
        match key {
            "v" => self.paste(),
            "c" => self.copy(),
            "k" => self.clear_screen(),
            "n" => self.new_window(),
            "u" if shift => {
                match self.update {
                    update::State::None => {}
                    update::State::Failed(_) => self.update = update::State::None,
                    _ => self.update_open = !self.update_open,
                }
                self.notice_changed();
            }
            "f" => {
                self.find = Some(String::new());
                self.find_changed();
            }
            "t" => self.open_tab(),
            "w" => self.close_active(),
            // Splits: Cmd+D right, Cmd+Shift+D down (Ctrl+Shift+O / E on Linux).
            "d" if self.mods.super_key() => self.split(!shift),
            "o" => self.split(true),
            "e" => self.split(false),
            "{" | "[" if shift => self.cycle_tab(-1),
            "}" | "]" if shift => self.cycle_tab(1),
            "[" => self.cycle_pane(-1),
            "]" => self.cycle_pane(1),
            "=" | "+" if self.mods.control_key() && self.mods.super_key() => self.equalize_splits(),
            "=" | "+" => {
                self.font_pt = (self.font_pt + 1.0).min(48.0);
                self.rebuild();
            }
            "-" | "_" => {
                self.font_pt = (self.font_pt - 1.0).max(6.0);
                self.rebuild();
            }
            "0" | ")" => {
                self.font_pt = DEFAULT_PT;
                self.rebuild();
            }
            d if self.mods.super_key() && d.len() == 1 && ("1"..="9").contains(&d) => self.goto_tab(d.parse().unwrap()),
            _ => return false,
        }
        true
    }

    /// Text of the update notice: a short pill, or the prompt once opened.
    fn update_notice(&self) -> Option<String> {
        let key = if cfg!(target_os = "macos") { "Cmd+Shift+U" } else { "Ctrl+Shift+U" };
        Some(match &self.update {
            update::State::None => return None,
            update::State::Available(v) if !self.update_open => format!("↑ litty {v}  {key}"),
            update::State::Available(v) => match &self.update_managed {
                Some(cmd) => format!("litty {v} available · {cmd} · Enter: copy · Esc: skip"),
                None => format!("litty {v} available · Enter: update on quit · Esc: skip"),
            },
            update::State::Downloading(v) => format!("downloading litty {v}…"),
            update::State::Staged(v, _) => format!("litty {v} ready · installs when you quit"),
            update::State::Failed(msg) => format!("update failed: {msg} · {key} to dismiss"),
        })
    }

    /// Repaint the active tab so the notice appears, changes or disappears.
    fn notice_changed(&mut self) {
        for p in &self.tab().panes {
            p.term.lock().unwrap().grid.dirty.fill(true);
        }
        self.redraw_soon();
    }

    /// Keys while the update prompt is open. Enter and Esc are consumed; any other key closes it.
    fn update_key(&mut self, e: &KeyEvent) -> bool {
        let update::State::Available(version) = self.update.clone() else {
            if matches!(e.logical_key, Key::Named(NamedKey::Escape)) {
                self.update_open = false;
                self.notice_changed();
                return true;
            }
            return false;
        };
        match &e.logical_key {
            Key::Named(NamedKey::Escape) => {
                update::skip(&version);
                self.update = update::State::None;
            }
            Key::Named(NamedKey::Enter) => {
                if let Some(cmd) = &self.update_managed {
                    clipboard_set(cmd.as_bytes());
                } else {
                    self.update = update::State::Downloading(version.clone());
                    let proxy = self.proxy.clone();
                    std::thread::spawn(move || {
                        let ev = match update::stage(&version) {
                            Ok(path) => update::Event::Staged(version, path),
                            Err(msg) => update::Event::Failed(msg),
                        };
                        let _ = proxy.send_event(Ev::Update(ev));
                    });
                }
            }
            _ => {
                self.update_open = false;
                self.notice_changed();
                return false;
            }
        }
        self.update_open = false;
        self.notice_changed();
        true
    }

    /// Find bar keys. Returns whether the key was consumed (everything is, while the bar is open).
    fn find_key(&mut self, e: &KeyEvent) -> bool {
        let mods = self.mods;
        match &e.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.find = None;
                let mut t = self.term().lock().unwrap();
                t.grid.matches.clear();
                t.grid.dirty.fill(true);
                drop(t);
                self.redraw_soon();
            }
            Key::Named(NamedKey::Enter) => {
                let mut t = self.term().lock().unwrap();
                let n = t.grid.matches.len();
                if n > 0 {
                    let cur = t.grid.cur_match;
                    t.grid.cur_match = if mods.shift_key() { (cur + n - 1) % n } else { (cur + 1) % n };
                    let id = t.grid.matches[t.grid.cur_match].0;
                    t.grid.scroll_to(id);
                    t.grid.dirty.fill(true);
                }
                drop(t);
                self.redraw_soon();
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(q) = &mut self.find {
                    q.pop();
                }
                self.find_changed();
            }
            _ => {
                let text = e.text.as_deref().filter(|t| !mods.control_key() && !t.chars().any(char::is_control));
                if let (Some(text), Some(q)) = (text, &mut self.find) {
                    q.push_str(text);
                    self.find_changed();
                }
            }
        }
        true
    }

    /// Re-run the search for the current query and jump to the best match.
    fn find_changed(&mut self) {
        let Some(q) = self.find.clone() else { return };
        let mut t = self.term().lock().unwrap();
        t.grid.find_all(&q);
        if let Some(&(id, ..)) = t.grid.matches.get(t.grid.cur_match) {
            t.grid.scroll_to(id);
        }
        t.grid.dirty.fill(true);
        drop(t);
        self.redraw_soon();
    }

    /// Scroll so the previous (`dir < 0`) or next prompt is at the top of the view.
    fn jump_prompt(&mut self, dir: i32) {
        let mut t = self.term().lock().unwrap();
        let g = &mut t.grid;
        let top = g.abs_row(0);
        let target = if dir < 0 {
            g.marks.iter().rev().find(|m| m.start < top).map(|m| m.start)
        } else {
            g.marks.iter().find(|m| m.start > top).map(|m| m.start)
        };
        match target {
            Some(id) => g.scroll_top_to(id),
            None if dir > 0 => g.scroll = 0,
            None => {}
        }
        drop(t);
        self.redraw_soon();
    }

    fn open_url(url: &str) {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }

    fn link_modifier(&self) -> bool {
        self.mods.super_key() || (!cfg!(target_os = "macos") && self.mods.control_key())
    }

    fn scroll_by(&mut self, delta: isize) {
        self.term().lock().unwrap().grid.scroll_view(delta);
        self.redraw_soon();
    }

    fn on_key(&mut self, e: &KeyEvent) {
        let mods = self.mods;
        let cmd = mods.super_key() || (mods.control_key() && mods.shift_key());
        if mods.control_key() && matches!(e.logical_key, Key::Named(NamedKey::Tab)) {
            return self.cycle_tab(if mods.shift_key() { -1 } else { 1 });
        }
        if let (false, true, Key::Character(s)) = (cfg!(target_os = "macos"), mods.alt_key(), &e.logical_key) {
            if let Some(d) = s.chars().next().and_then(|c| c.to_digit(10)).filter(|&d| d > 0) {
                return self.goto_tab(d as usize);
            }
        }
        if let (true, Key::Character(s)) = (cmd, &e.logical_key) {
            if self.shortcut(&s.to_lowercase()) {
                return;
            }
        }
        if self.update_open && self.update_key(e) {
            return;
        }
        if self.find.is_some() && self.find_key(e) {
            return;
        }
        if mods.super_key() {
            let arrow = match &e.logical_key {
                Key::Named(NamedKey::ArrowLeft) => Some((-1, 0)),
                Key::Named(NamedKey::ArrowRight) => Some((1, 0)),
                Key::Named(NamedKey::ArrowUp) => Some((0, -1)),
                Key::Named(NamedKey::ArrowDown) => Some((0, 1)),
                _ => None,
            };
            match (&e.logical_key, arrow) {
                // Cmd+Option+arrows move focus between panes, Cmd+Ctrl+arrows resize the split.
                (_, Some((dx, dy))) if mods.alt_key() => self.focus_dir(dx, dy),
                (_, Some((dx, dy))) if mods.control_key() => self.resize_split(dx, dy),
                (Key::Named(NamedKey::Enter), _) if mods.shift_key() => self.toggle_zoom(),
                // Cmd+Up/Down jump between prompts; Cmd+Left/Right/Backspace edit the line like
                // other Mac terminals.
                (Key::Named(NamedKey::ArrowUp), _) => self.jump_prompt(-1),
                (Key::Named(NamedKey::ArrowDown), _) => self.jump_prompt(1),
                (Key::Named(NamedKey::ArrowLeft), _) => self.send_input(b"\x01"),
                (Key::Named(NamedKey::ArrowRight), _) => self.send_input(b"\x05"),
                (Key::Named(NamedKey::Backspace), _) => self.send_input(b"\x15"),
                _ => {}
            }
            return;
        }
        if mods.shift_key() && !mods.control_key() && !mods.alt_key() {
            let rows = self.term().lock().unwrap().grid.rows as isize;
            match &e.logical_key {
                Key::Named(NamedKey::PageUp) => return self.scroll_by(rows - 1),
                Key::Named(NamedKey::PageDown) => return self.scroll_by(1 - rows),
                Key::Named(NamedKey::Home) => return self.scroll_by(isize::MAX / 2),
                Key::Named(NamedKey::End) => return self.scroll_by(isize::MIN / 2),
                _ => {}
            }
        }
        let app_cursor = self.term().lock().unwrap().grid.app_cursor;
        if let Some(bytes) = key_bytes(e, mods, app_cursor) {
            self.send_input(&bytes);
        }
    }

    /// (col, row) under the mouse within `rect`, clamped to the grid.
    fn cell_at(&self, g: &Grid, rect: Rect) -> (usize, usize) {
        let Some(r) = &self.renderer else { return (0, 0) };
        let x = (self.cursor.0 - rect.x as f64).max(0.0) as usize / r.fonts.cell_w;
        let y = (self.cursor.1 - rect.y as f64).max(0.0) as usize / r.fonts.cell_h;
        (x.min(g.cols - 1), y.min(g.rows - 1))
    }

    /// The tab bar item under the mouse, if the mouse is over the bar.
    fn tab_bar_hit(&self) -> Option<TabHit> {
        let r = self.renderer.as_ref()?;
        (r.bar_h > 0 && self.cursor.1 < r.bar_h as f64).then(|| r.tab_hit(self.cursor.0.max(0.0) as usize, self.tabs.len()))
    }

    fn divider_at(&self, x: f64, y: f64) -> Option<usize> {
        self.tab().dividers.iter().position(|d| d.rect.contains(x, y))
    }

    fn set_icon(&mut self, icon: CursorIcon) {
        if icon != self.icon {
            self.icon = icon;
            if let Some(w) = &self.window {
                w.set_cursor(icon);
            }
        }
    }

    fn on_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        let code = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        let pressed = state == ElementState::Pressed;
        if let Some(hit) = self.tab_bar_hit() {
            if pressed {
                match (code, hit) {
                    (0, TabHit::Tab(i)) => self.activate(i),
                    (0 | 1, TabHit::Close(i)) | (1, TabHit::Tab(i)) => self.close_tab_at(i),
                    (0, TabHit::New) => self.open_tab(),
                    _ => {}
                }
            }
            return;
        }
        if code == 0 {
            if !pressed {
                (self.rail_drag, self.divider_drag) = (false, None);
            } else if let Some(i) = self.divider_at(self.cursor.0, self.cursor.1) {
                self.divider_drag = Some(i);
                return;
            }
        }
        // Clicking a pane focuses it (the click still applies to that pane).
        if pressed {
            if let Some((id, _)) = self.pane_at(self.cursor.0, self.cursor.1) {
                self.focus_pane(id);
            }
        }
        let term = self.term().clone();
        let mut t = term.lock().unwrap();
        let rect = self.active_rect();
        let Some(r) = &self.renderer else { return };
        let (col, row) = self.cell_at(&t.grid, rect);

        // The right rail scrolls through history (shown when the tab has a single pane).
        if code == 0 && pressed && self.tab().layout.len() == 1 && self.cursor.0 >= (r.w - r.pad) as f64 && t.grid.history_len() > 0 {
            self.rail_drag = true;
            t.grid.scroll = r.scroll_for_rail_y(&t.grid, self.cursor.1);
            drop(t);
            self.redraw_soon();
            return;
        }

        if pressed && code == 0 && self.link_modifier() {
            if let Some(url) = t.grid.url_at(row, col) {
                drop(t);
                Self::open_url(&url);
                return;
            }
        }
        if t.grid.mouse != 0 && !self.mods.shift_key() {
            let report = mouse_report(&t.grid, code, self.mods, col, row, !pressed);
            drop(t);
            self.held = pressed.then_some(code);
            self.send(&report);
            return;
        }
        if code != 0 {
            return;
        }
        let g = &mut t.grid;
        if pressed {
            // Double click selects a word, triple click a line.
            let now = Instant::now();
            let pane = self.tab().active;
            let repeat = self.last_click.is_some_and(|(at, p, c, r)| now.duration_since(at) < DOUBLE_CLICK && (p, c, r) == (pane, col, row));
            self.click_count = if repeat { self.click_count % 3 + 1 } else { 1 };
            self.last_click = Some((now, pane, col, row));
            let id = g.abs_row(row);
            match self.click_count {
                2 => {
                    g.select_word(id, col);
                    self.selecting = false;
                }
                3 => {
                    g.select_line(id);
                    self.selecting = false;
                }
                _ => (g.sel, self.anchor, self.selecting) = (None, (id, col), true),
            }
            g.dirty.fill(true);
        } else {
            self.selecting = false;
        }
        drop(t);
        self.redraw_soon();
    }

    fn on_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor = (x, y);
        if self.tabs.is_empty() {
            return;
        }
        if let (Some(hit), false) = (self.tab_bar_hit(), self.selecting) {
            return self.set_icon(if hit != TabHit::None { CursorIcon::Pointer } else { CursorIcon::Default });
        }
        // Dragging a divider resizes the split.
        if let Some(i) = self.divider_drag {
            let (path, container, vertical) = {
                let d = &self.tab().dividers[i];
                (d.path.clone(), d.container, d.vertical)
            };
            let gap = self.tab().dividers[i].rect.w.max(self.tab().dividers[i].rect.h);
            let (pos, start, len) = if vertical { (x, container.x, container.w) } else { (y, container.y, container.h) };
            let avail = len.saturating_sub(gap).max(1) as f64;
            let ratio = (((pos - start as f64) - gap as f64 / 2.0) / avail).clamp(0.1, 0.9) as f32;
            let tab = &mut self.tabs[self.active];
            if let Some(Node::Split { ratio: r, .. }) = tab.root.node_at_mut(&path) {
                *r = ratio;
            }
            self.relayout();
            return;
        }
        if self.rail_drag {
            let term = self.term().clone();
            let mut t = term.lock().unwrap();
            if let Some(r) = &self.renderer {
                t.grid.scroll = r.scroll_for_rail_y(&t.grid, y);
            }
            drop(t);
            self.redraw_soon();
            return;
        }
        if let Some(i) = self.divider_at(x, y) {
            return self.set_icon(if self.tab().dividers[i].vertical { CursorIcon::ColResize } else { CursorIcon::RowResize });
        }
        let term = self.term().clone();
        let mut t = term.lock().unwrap();
        let rect = self.active_rect();
        let (col, row) = self.cell_at(&t.grid, rect);
        // Pointer cursor over links (with the modifier held) and over the rail.
        let in_rail = self.tab().layout.len() == 1 && self.renderer.as_ref().is_some_and(|r| x >= (r.w - r.pad) as f64);
        let over_link = self.link_modifier() && t.grid.url_at(row, col).is_some();
        self.set_icon(if in_rail || over_link { CursorIcon::Pointer } else { CursorIcon::Default });
        if self.selecting {
            let head = (t.grid.abs_row(row), col);
            let sel = Some((self.anchor, head));
            if t.grid.sel != sel && (head != self.anchor || t.grid.sel.is_some()) {
                t.grid.sel = sel;
                t.grid.dirty.fill(true);
                drop(t);
                self.redraw_soon();
            }
            return;
        }
        if t.grid.mouse >= 2 && (col, row) != self.last_cell {
            self.last_cell = (col, row);
            let button = match self.held {
                Some(b) => Some(b),
                None if t.grid.mouse == 3 => Some(3),
                None => None,
            };
            if let Some(b) = button {
                let report = mouse_report(&t.grid, b + 32, self.mods, col, row, false);
                drop(t);
                self.send(&report);
            }
        }
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let ch = self.renderer.as_ref().map_or(16, |r| r.fonts.cell_h) as f32;
        self.wheel_acc += match delta {
            MouseScrollDelta::LineDelta(_, y) => y * 3.0,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / ch,
        };
        let n = self.wheel_acc.trunc();
        self.wheel_acc -= n;
        let n = n as i32;
        if n == 0 {
            return;
        }
        // The wheel acts on the pane under the mouse, whether or not it has focus.
        let Some((id, rect)) = self.pane_at(self.cursor.0, self.cursor.1) else { return };
        let Some(term) = self.tab().panes.iter().find(|p| p.id == id).map(|p| p.term.clone()) else { return };
        let mut t = term.lock().unwrap();
        let g = &mut t.grid;
        let seq = if g.mouse != 0 && !self.mods.shift_key() {
            let (col, row) = self.cell_at(g, rect);
            mouse_report(g, if n > 0 { 64 } else { 65 }, self.mods, col, row, false)
        } else if g.in_alt {
            // No mouse tracking on the alternate screen: wheel scrolls via arrow keys, like less/man.
            let (a, b) = if g.app_cursor { (b"\x1bOA", b"\x1bOB") } else { (b"\x1b[A", b"\x1b[B") };
            if n > 0 { a.to_vec() } else { b.to_vec() }
        } else {
            g.scroll_view(n as isize);
            drop(t);
            self.redraw_soon();
            return;
        };
        drop(t);
        if id != self.tab().active {
            self.focus_pane(id);
        }
        for _ in 0..n.abs() {
            self.send(&seq);
        }
    }
}

impl ApplicationHandler<Ev> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let (w, h) = match load_state() {
            Some((w, h, pt)) => {
                self.font_pt = pt;
                (w, h)
            }
            None => (900.0, 560.0),
        };
        let attrs = Window::default_attributes().with_title("litty").with_inner_size(LogicalSize::new(w, h));
        let window = Arc::new(el.create_window(attrs).unwrap());
        window.set_ime_allowed(true);
        self.presenter = Some(Presenter::new(&window));
        self.scale = window.scale_factor() as f32;
        self.window = Some(window);
        self.rebuild();
        let command = self.initial_command.clone();
        self.new_tab(&command, None);
        if command.is_empty() && update::enabled() {
            let proxy = self.proxy.clone();
            std::thread::spawn(move || update::check(|e| drop(proxy.send_event(Ev::Update(e)))));
        }
    }

    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            self.frame_deadline = None;
            self.redraw_soon();
        }
    }

    /// Sleep until the next thing that needs doing: a postponed frame or a cursor blink.
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        let mut deadline = self.frame_deadline;
        let blinking = self.focused && !self.tabs.is_empty() && self.term().lock().unwrap().grid.cursor_blink;
        if blinking {
            if now >= self.next_blink {
                (self.blink_on, self.next_blink) = (!self.blink_on, now + BLINK);
                self.redraw_soon();
            }
            deadline = Some(deadline.map_or(self.next_blink, |d| d.min(self.next_blink)));
        } else if !self.blink_on {
            self.blink_on = true;
            self.redraw_soon();
        }
        el.set_control_flow(deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: Ev) {
        match ev {
            Ev::Wake => self.redraw_soon(),
            Ev::Exit(id) => {
                // Ignore panes we already closed ourselves.
                if let Some(t) = self.tabs.iter().position(|t| t.panes.iter().any(|p| p.id == id)) {
                    self.close_pane(t, id, false);
                }
            }
            Ev::Quit => el.exit(),
            Ev::Update(e) => {
                self.update = match e {
                    update::Event::Found(v) => {
                        self.update_managed = update::managed_by();
                        update::State::Available(v)
                    }
                    update::Event::Staged(v, path) => update::State::Staged(v, path),
                    update::Event::Failed(msg) => update::State::Failed(msg),
                };
                self.notice_changed();
            }
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.save_state();
        if let update::State::Staged(_, path) = &self.update {
            if let Err(e) = update::apply(path) {
                eprintln!("litty: update failed: {e}");
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => self.resize(s.width, s.height),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                self.rebuild();
            }
            WindowEvent::RedrawRequested if !self.tabs.is_empty() => {
                if Instant::now() < self.next_frame {
                    self.frame_deadline = Some(self.next_frame);
                } else {
                    self.frame_deadline = None;
                    // Paced from the start of the frame, so drawing time doesn't stretch the interval.
                    self.next_frame = Instant::now() + FRAME;
                    self.redraw();
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Focused(f) => {
                self.focused = f;
                if !self.tabs.is_empty() {
                    // Redraw the cursor as solid or hollow.
                    for p in &self.tab().panes {
                        p.term.lock().unwrap().grid.dirty.fill(true);
                    }
                    self.redraw_soon();
                }
            }
            _ if self.tabs.is_empty() => {}
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => self.on_key(&event),
            WindowEvent::Ime(Ime::Commit(text)) => self.send_input(text.as_bytes()),
            WindowEvent::MouseInput { state, button, .. } => self.on_mouse_button(state, button),
            WindowEvent::CursorMoved { position, .. } => self.on_cursor_moved(position.x, position.y),
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(delta),
            _ => {}
        }
    }
}

fn main() {
    // Launched from Finder or the Dock the working directory is "/": start in the home directory.
    if std::env::current_dir().is_ok_and(|d| d == std::path::Path::new("/")) {
        if let Some(home) = std::env::var_os("HOME") {
            let _ = std::env::set_current_dir(home);
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let initial_command = match args.first().map(String::as_str) {
        Some("-e") => args[1..].to_vec(),
        _ => Vec::new(),
    };

    let event_loop = EventLoop::<Ev>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        tabs: Vec::new(),
        active: 0,
        next_id: 0,
        proxy: event_loop.create_proxy(),
        initial_command,
        mods: ModifiersState::empty(),
        window: None,
        presenter: None,
        renderer: None,
        font_pt: DEFAULT_PT,
        scale: 1.0,
        cursor: (0.0, 0.0),
        held: None,
        last_cell: (usize::MAX, usize::MAX),
        selecting: false,
        anchor: (0, 0),
        last_click: None,
        click_count: 0,
        wheel_acc: 0.0,
        ime_pos: (usize::MAX, usize::MAX),
        next_frame: Instant::now(),
        frame_deadline: None,
        blink_on: true,
        next_blink: Instant::now(),
        focused: true,
        rail_drag: false,
        divider_drag: None,
        icon: CursorIcon::Default,
        find: None,
        update: update::State::None,
        update_open: false,
        update_managed: None,
    };
    event_loop.run_app(&mut app).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(command: &[&str], cwd: Option<&str>) -> String {
        let size = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
        let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();
        let (mut master, pid) = spawn_shell(&size, &command, cwd).expect("spawn");
        let mut out = String::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        out
    }

    #[test]
    fn spawns_in_cwd_with_terminal_environment() {
        // Run from a multi-threaded test harness on purpose: tabs are opened after threads exist.
        let out = run(&["sh", "-c", "pwd -P; echo $TERM $COLORTERM"], Some("/tmp"));
        assert!(out.contains("tmp"), "{out:?}");
        assert!(out.contains("xterm-256color truecolor"), "{out:?}");
    }

    #[test]
    fn missing_program_fails_cleanly() {
        let size = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
        assert!(spawn_shell(&size, &["definitely-not-a-program-xyz".to_string()], None).is_none());
    }

    fn ids(n: &Node) -> Vec<usize> {
        let mut v = Vec::new();
        n.leaves(&mut v);
        v
    }

    #[test]
    fn split_remove_and_paths() {
        let mut root = Node::Leaf(0);
        assert!(root.split(0, true, 1));
        assert!(root.split(1, false, 2));
        assert_eq!(ids(&root), vec![0, 1, 2]);
        assert_eq!(root.path_to(2), Some(vec![true, true]));
        assert_eq!(root.path_to(0), Some(vec![false]));
        assert!(!root.split(9, true, 3));
        // Removing a pane lets its sibling take the parent's place.
        let root = root.remove(1).unwrap();
        assert_eq!(ids(&root), vec![0, 2]);
        assert!(matches!(&root, Node::Split { vertical: true, .. }));
        assert!(root.remove(0).unwrap().remove(2).is_none());
    }

    #[test]
    fn layout_tiles_the_area_without_overlap() {
        let mut root = Node::Leaf(0);
        root.split(0, true, 1);
        root.split(1, false, 2);
        let area = Rect { x: 10, y: 40, w: 1000, h: 600 };
        let (mut panes, mut dividers) = (Vec::new(), Vec::new());
        layout(&root, area, (8, 16), 6, &mut panes, &mut dividers, &mut Vec::new());
        assert_eq!(panes.len(), 3);
        assert_eq!(dividers.len(), 2);
        // Pane sizes are whole cells, everything lies inside the area, nothing overlaps.
        let all: Vec<Rect> = panes.iter().map(|p| p.1).chain(dividers.iter().map(|d| d.rect)).collect();
        for (i, a) in all.iter().enumerate() {
            assert!(a.x >= area.x && a.y >= area.y && a.x + a.w <= area.x + area.w && a.y + a.h <= area.y + area.h, "{a:?}");
            for b in &all[i + 1..] {
                let apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
        assert_eq!(panes[0].1.w % 8, 0);
        // Dragging a divider (ratio) moves the split.
        if let Node::Split { ratio, .. } = &mut root {
            *ratio = 0.25;
        }
        let (mut moved, mut d2) = (Vec::new(), Vec::new());
        layout(&root, area, (8, 16), 6, &mut moved, &mut d2, &mut Vec::new());
        assert!(moved[0].1.w < panes[0].1.w);
    }

    #[test]
    fn tiny_areas_do_not_panic() {
        let mut root = Node::Leaf(0);
        for i in 1..6 {
            root.split(i - 1, i % 2 == 0, i);
        }
        let (mut p, mut d) = (Vec::new(), Vec::new());
        layout(&root, Rect { x: 0, y: 0, w: 20, h: 10 }, (8, 16), 6, &mut p, &mut d, &mut Vec::new());
        assert_eq!(p.len(), 6);
    }
}
