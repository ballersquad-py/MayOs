//! A Wayland compositor for Linux programs.
//!
//! Programs connect to `$XDG_RUNTIME_DIR/wayland-0` (`/run/wayland-0`);
//! the kernel serves the socket directly. Requests are handled in the
//! calling program's own `sendmsg`, events are queued for it to read.
//!
//! Supported: wl_compositor, wl_subcompositor, wl_shm (shared-memory
//! buffers, ARGB8888 / XRGB8888), wl_seat (pointer and keyboard with an
//! XKB keymap), wl_output, xdg_wm_base (toplevels and popups), wl_shell,
//! zxdg_decoration_manager_v1 (MayOS draws the frame) and a stub
//! wl_data_device_manager. Each toplevel becomes a MayOS window; its
//! surfaces are composed when the program commits them.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::linux::{Desc, DescRef};
use super::unix::{self, QueueRef, Service, ServiceFactory, Shm};
use crate::input::{Key, KeyEvent};
use crate::sync::{Mutex, Spin};

pub const SOCKET_PATH: &str = "/run/wayland-0";

static KEYMAP: &[u8] = include_bytes!("../../../assets/xkb/us.xkb");

/// Toplevels waiting for the desktop to open a window.
static PENDING: Spin<Vec<Arc<Window>>> = Spin::new(Vec::new());

pub fn take_pending() -> Vec<Arc<Window>> {
    core::mem::take(&mut *PENDING.lock())
}

/// Start serving the socket.
pub fn init() {
    let f: Arc<dyn ServiceFactory> = Arc::new(Factory);
    unix::register_service(SOCKET_PATH, f);
}

struct Factory;

impl ServiceFactory for Factory {
    fn connect(&self, to_client: QueueRef, pid: u64) -> Arc<dyn Service> {
        let c = Arc::new(Client {
            pid,
            to_client,
            st: Mutex::new(State::default()),
            alive: AtomicBool::new(true),
            me: Spin::new(Weak::new()),
        });
        *c.me.lock() = Arc::downgrade(&c);
        CLIENTS.lock().push(Arc::downgrade(&c));
        c
    }
}

static CLIENTS: Spin<Vec<Weak<Client>>> = Spin::new(Vec::new());

/// Called by the desktop about 60 times a second: frame callbacks.
pub fn tick() {
    let clients: Vec<Arc<Client>> = {
        let mut l = CLIENTS.lock();
        l.retain(|w| w.strong_count() > 0);
        l.iter().filter_map(|w| w.upgrade()).collect()
    };
    for c in clients {
        if c.alive.load(Ordering::Relaxed) {
            c.frame_done();
        }
    }
}

// -------------------------------------------------------------------------
// Windows (shared with the desktop)
// -------------------------------------------------------------------------

/// A toplevel as the desktop sees it.
pub struct Window {
    client: Weak<Client>,
    /// wl_surface id of the toplevel.
    surface: u32,
    pub title: Spin<String>,
    /// Composed picture: (width, height, pixels 0xAARRGGBB).
    pub image: Spin<Option<(i32, i32, Arc<Vec<u32>>)>>,
    /// Bumped on every new picture.
    pub version: AtomicU32,
    /// The program destroyed the toplevel or went away.
    pub gone: AtomicBool,
    /// Size the program asked for with min/max hints, if any.
    pub min_size: Spin<(i32, i32)>,
    opened: AtomicBool,
    pub pid: u64,
}

impl Window {
    fn with_client(&self, f: impl FnOnce(&Client, &mut State)) {
        if let Some(c) = self.client.upgrade() {
            let mut st = c.st.lock();
            f(&c, &mut st);
            c.flush(&mut st);
        }
    }

    /// The desktop resized the window (content size).
    pub fn resize(&self, w: i32, h: i32, focused: bool) {
        let s = self.surface;
        self.with_client(|_, st| st.configure_toplevel(s, w, h, focused));
    }

    pub fn close(&self) {
        let s = self.surface;
        self.with_client(|_, st| st.close_toplevel(s));
    }

    pub fn pointer_motion(&self, x: i32, y: i32) {
        let s = self.surface;
        self.with_client(|_, st| st.pointer_motion(s, x, y));
    }

    pub fn pointer_button(&self, button: u8, down: bool) {
        let s = self.surface;
        self.with_client(|_, st| st.pointer_button(s, button, down));
    }

    pub fn pointer_axis(&self, delta: i32) {
        self.with_client(|_, st| st.pointer_axis(delta));
    }

    pub fn pointer_leave(&self) {
        self.with_client(|_, st| st.pointer_leave());
    }

    pub fn key(&self, k: &KeyEvent) {
        let s = self.surface;
        self.with_client(|c, st| st.key(c, s, k));
    }

    pub fn focus(&self, on: bool) {
        let s = self.surface;
        self.with_client(|c, st| st.keyboard_focus(c, s, on));
    }
}

// -------------------------------------------------------------------------
// Client state
// -------------------------------------------------------------------------

pub struct Client {
    pid: u64,
    to_client: QueueRef,
    st: Mutex<State>,
    alive: AtomicBool,
    me: Spin<Weak<Client>>,
}

#[derive(Clone)]
struct Buffer {
    shm: Arc<Shm>,
    off: u64,
    w: i32,
    h: i32,
    stride: i32,
    opaque: bool,
}

#[derive(Default)]
struct Surface {
    /// Some(None) = attach(null), Some(Some(id)) = attach(buffer).
    pending: Option<Option<u32>>,
    frames: Vec<u32>,
    /// Picture of the last committed buffer.
    image: Option<(i32, i32, Arc<Vec<u32>>)>,
    role: Role,
    /// Sub-surfaces and popups drawn on top, in order.
    children: Vec<u32>,
    entered_output: bool,
}

#[derive(Default, Clone)]
enum Role {
    #[default]
    None,
    Toplevel(Arc<Window>),
    /// Position relative to the parent surface.
    Sub { parent: u32, x: i32, y: i32 },
    Popup { parent: u32, x: i32, y: i32, xdg_popup: u32 },
}

enum Obj {
    Display,
    Registry,
    Callback,
    Compositor,
    SubCompositor,
    Subsurface { surface: u32 },
    Shm,
    ShmPool { shm: Arc<Shm> },
    Buffer(Buffer),
    Surface(Surface),
    Region,
    Seat,
    Pointer,
    Keyboard,
    Touch,
    Output,
    WmBase,
    Positioner(Positioner),
    XdgSurface { surface: u32, geometry: Option<(i32, i32, i32, i32)> },
    Toplevel { xdg_surface: u32 },
    Popup { xdg_surface: u32 },
    Shell,
    ShellSurface { surface: u32 },
    DataDeviceManager,
    DataSource,
    DataDevice,
    DecorationManager,
    Decoration,
    Other,
}

#[derive(Default, Clone, Copy)]
struct Positioner {
    size: (i32, i32),
    anchor_rect: (i32, i32, i32, i32),
    anchor: u32,
    gravity: u32,
    offset: (i32, i32),
}

impl Positioner {
    /// Popup position relative to the parent's window geometry.
    fn place(&self) -> (i32, i32) {
        let (ax, ay, aw, ah) = self.anchor_rect;
        // Anchor point on the anchor rectangle (xdg_positioner.anchor).
        let (px, py) = match self.anchor {
            1 => (ax + aw / 2, ay),
            2 => (ax + aw / 2, ay + ah),
            3 => (ax, ay + ah / 2),
            4 => (ax + aw, ay + ah / 2),
            5 => (ax, ay),
            6 => (ax, ay + ah),
            7 => (ax + aw, ay),
            8 => (ax + aw, ay + ah),
            _ => (ax + aw / 2, ay + ah / 2),
        };
        let (w, h) = self.size;
        // Which way the popup grows from that point (gravity).
        let (x, y) = match self.gravity {
            1 => (px - w / 2, py - h),
            2 => (px - w / 2, py),
            3 => (px - w, py - h / 2),
            4 => (px, py - h / 2),
            5 => (px - w, py - h),
            6 => (px - w, py),
            7 => (px, py - h),
            8 => (px, py),
            _ => (px - w / 2, py - h / 2),
        };
        (x + self.offset.0, y + self.offset.1)
    }
}

struct Global {
    name: u32,
    iface: &'static str,
    version: u32,
}

const GLOBALS: &[Global] = &[
    Global { name: 1, iface: "wl_compositor", version: 4 },
    Global { name: 2, iface: "wl_subcompositor", version: 1 },
    Global { name: 3, iface: "wl_shm", version: 1 },
    Global { name: 4, iface: "wl_seat", version: 5 },
    Global { name: 5, iface: "wl_output", version: 3 },
    Global { name: 6, iface: "xdg_wm_base", version: 2 },
    Global { name: 7, iface: "wl_shell", version: 1 },
    Global { name: 8, iface: "wl_data_device_manager", version: 3 },
    Global { name: 9, iface: "zxdg_decoration_manager_v1", version: 1 },
];

#[derive(Default)]
struct State {
    objs: BTreeMap<u32, Obj>,
    versions: BTreeMap<u32, u32>,
    inbuf: Vec<u8>,
    infds: VecDeque<DescRef>,
    out: Vec<u8>,
    outfds: Vec<DescRef>,
    serial: u32,
    /// Frame callbacks of committed surfaces, answered at the next tick.
    frame_ready: Vec<u32>,
    pointers: Vec<u32>,
    keyboards: Vec<u32>,
    /// Surface under the pointer and its origin in toplevel coordinates.
    pointer_on: Option<(u32, i32, i32)>,
    keyboard_on: Option<u32>,
    mods: u32,
    logged: Vec<u32>,
}

// --- wire format ---------------------------------------------------------

struct Args<'a> {
    b: &'a [u8],
    o: usize,
}

impl Args<'_> {
    fn u(&mut self) -> u32 {
        let v = self.b.get(self.o..self.o + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap())).unwrap_or(0);
        self.o += 4;
        v
    }
    fn i(&mut self) -> i32 {
        self.u() as i32
    }
    fn s(&mut self) -> String {
        let len = self.u() as usize;
        let end = (self.o + len).min(self.b.len());
        let raw = &self.b[self.o..end];
        let text = raw.split(|&c| c == 0).next().unwrap_or(&[]);
        let s = String::from_utf8_lossy(text).into_owned();
        self.o += (len + 3) & !3;
        s
    }
    fn arr(&mut self) -> Vec<u8> {
        let len = self.u() as usize;
        let end = (self.o + len).min(self.b.len());
        let v = self.b[self.o..end].to_vec();
        self.o += (len + 3) & !3;
        v
    }
}

enum A<'a> {
    U(u32),
    I(i32),
    F(i32),
    S(&'a str),
    Arr(&'a [u8]),
    Fd(DescRef),
}

fn now_ms() -> u32 {
    crate::time::uptime_ms() as u32
}

impl State {
    fn ev(&mut self, obj: u32, op: u16, args: Vec<A>) {
        let mut body = Vec::new();
        for a in args {
            match a {
                A::U(v) => body.extend_from_slice(&v.to_le_bytes()),
                A::I(v) | A::F(v) => body.extend_from_slice(&v.to_le_bytes()),
                A::S(s) => {
                    let len = s.len() + 1;
                    body.extend_from_slice(&(len as u32).to_le_bytes());
                    body.extend_from_slice(s.as_bytes());
                    body.push(0);
                    while body.len() % 4 != 0 {
                        body.push(0);
                    }
                }
                A::Arr(a) => {
                    body.extend_from_slice(&(a.len() as u32).to_le_bytes());
                    body.extend_from_slice(a);
                    while body.len() % 4 != 0 {
                        body.push(0);
                    }
                }
                A::Fd(d) => self.outfds.push(d),
            }
        }
        let size = (8 + body.len()) as u32;
        self.out.extend_from_slice(&obj.to_le_bytes());
        self.out.extend_from_slice(&((size << 16) | op as u32).to_le_bytes());
        self.out.extend_from_slice(&body);
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    fn version(&self, id: u32) -> u32 {
        self.versions.get(&id).copied().unwrap_or(1)
    }

    fn new_obj(&mut self, id: u32, o: Obj, version: u32) {
        self.objs.insert(id, o);
        self.versions.insert(id, version);
    }

    /// A client object went away: tell the client its id is free.
    fn destroy(&mut self, id: u32) {
        self.objs.remove(&id);
        self.versions.remove(&id);
        self.pointers.retain(|&p| p != id);
        self.keyboards.retain(|&k| k != id);
        self.ev(1, 1, vec![A::U(id)]);
    }

    fn surface(&mut self, id: u32) -> Option<&mut Surface> {
        match self.objs.get_mut(&id) {
            Some(Obj::Surface(s)) => Some(s),
            _ => None,
        }
    }

    // --- requests --------------------------------------------------------

    fn request(&mut self, c: &Client, id: u32, op: u16, a: &mut Args) {
        let kind = match self.objs.get(&id) {
            Some(o) => core::mem::discriminant(o),
            None => return, // destroyed object: ignore
        };
        let parent_ver = self.version(id);
        macro_rules! is {
            ($v:pat) => {
                matches!(self.objs.get(&id), Some($v))
            };
        }
        let _ = kind;
        if is!(Obj::Display) {
            match op {
                0 => {
                    let cb = a.u();
                    self.ev(cb, 0, vec![A::U(self.serial)]);
                    self.ev(1, 1, vec![A::U(cb)]);
                }
                1 => {
                    let reg = a.u();
                    self.new_obj(reg, Obj::Registry, 1);
                    for g in GLOBALS {
                        self.ev(reg, 0, vec![A::U(g.name), A::S(g.iface), A::U(g.version)]);
                    }
                }
                _ => {}
            }
        } else if is!(Obj::Registry) {
            if op == 0 {
                let name = a.u();
                let _iface = a.s();
                let ver = a.u();
                let new = a.u();
                self.bind(c, name, ver, new);
            }
        } else if is!(Obj::Compositor) {
            match op {
                0 => self.new_obj(a.u(), Obj::Surface(Surface::default()), parent_ver),
                1 => self.new_obj(a.u(), Obj::Region, 1),
                _ => {}
            }
        } else if is!(Obj::SubCompositor) {
            match op {
                0 => self.destroy(id),
                1 => {
                    let new = a.u();
                    let surface = a.u();
                    let parent = a.u();
                    self.new_obj(new, Obj::Subsurface { surface }, 1);
                    if let Some(s) = self.surface(surface) {
                        s.role = Role::Sub { parent, x: 0, y: 0 };
                    }
                    if let Some(p) = self.surface(parent) {
                        p.children.push(surface);
                    }
                }
                _ => {}
            }
        } else if let Some(Obj::Subsurface { surface }) = self.objs.get(&id) {
            let surface = *surface;
            match op {
                0 => {
                    self.unlink_surface(surface);
                    self.destroy(id);
                }
                1 => {
                    let (x, y) = (a.i(), a.i());
                    if let Some(s) = self.surface(surface)
                        && let Role::Sub { x: sx, y: sy, .. } = &mut s.role
                    {
                        *sx = x;
                        *sy = y;
                    }
                }
                _ => {}
            }
        } else if is!(Obj::Shm) {
            if op == 0 {
                let new = a.u();
                let fd = self.infds.pop_front();
                let _size = a.i();
                let shm = fd.and_then(|d| match &*d.lock() {
                    Desc::Memfd { shm, .. } => Some(shm.clone()),
                    _ => None,
                });
                match shm {
                    Some(shm) => self.new_obj(new, Obj::ShmPool { shm }, 1),
                    None => {
                        self.new_obj(new, Obj::Other, 1);
                        self.ev(1, 0, vec![A::U(id), A::U(2), A::S("wl_shm pool needs a memfd or /dev/shm file")]);
                    }
                }
            } else {
                self.destroy(id);
            }
        } else if let Some(Obj::ShmPool { shm }) = self.objs.get(&id) {
            let shm = shm.clone();
            match op {
                0 => {
                    let new = a.u();
                    let (off, w, h, stride, fmt) = (a.i(), a.i(), a.i(), a.i(), a.u());
                    let b = Buffer { shm, off: off.max(0) as u64, w: w.clamp(0, 8192), h: h.clamp(0, 8192), stride, opaque: fmt == 1 };
                    self.new_obj(new, Obj::Buffer(b), 1);
                }
                1 => self.destroy(id),
                _ => {}
            }
        } else if is!(Obj::Buffer(_)) {
            if op == 0 {
                self.destroy(id);
            }
        } else if is!(Obj::Surface(_)) {
            self.surface_request(id, op, a);
        } else if is!(Obj::Region) {
            if op == 0 {
                self.destroy(id);
            }
        } else if is!(Obj::Seat) {
            match op {
                0 => {
                    let p = a.u();
                    self.new_obj(p, Obj::Pointer, parent_ver);
                    self.pointers.push(p);
                }
                1 => {
                    let k = a.u();
                    self.new_obj(k, Obj::Keyboard, parent_ver);
                    self.keyboards.push(k);
                    self.send_keymap(k);
                    if let Some(s) = self.keyboard_on {
                        let serial = self.next_serial();
                        self.ev(k, 1, vec![A::U(serial), A::U(s), A::Arr(&[])]);
                        let mods = self.mods;
                        self.ev(k, 4, vec![A::U(serial), A::U(mods & !2), A::U(0), A::U(mods & 2), A::U(0)]);
                    }
                }
                2 => self.new_obj(a.u(), Obj::Touch, parent_ver),
                _ => self.destroy(id),
            }
        } else if is!(Obj::Pointer) || is!(Obj::Keyboard) || is!(Obj::Touch) || is!(Obj::Output) {
            // set_cursor (pointer 0) is ignored: MayOS draws its cursor.
            let release = if is!(Obj::Pointer) { op == 1 } else { op == 0 };
            if release {
                self.destroy(id);
            }
        } else if is!(Obj::WmBase) {
            match op {
                0 => self.destroy(id),
                1 => self.new_obj(a.u(), Obj::Positioner(Positioner::default()), 1),
                2 => {
                    let new = a.u();
                    let surface = a.u();
                    self.new_obj(new, Obj::XdgSurface { surface, geometry: None }, parent_ver);
                }
                _ => {} // pong
            }
        } else if let Some(Obj::Positioner(p)) = self.objs.get_mut(&id) {
            match op {
                0 => {
                    self.destroy(id);
                }
                1 => p.size = (a.i(), a.i()),
                2 => p.anchor_rect = (a.i(), a.i(), a.i(), a.i()),
                3 => p.anchor = a.u(),
                4 => p.gravity = a.u(),
                6 => p.offset = (a.i(), a.i()),
                _ => {}
            }
        } else if let Some(Obj::XdgSurface { surface, .. }) = self.objs.get(&id) {
            let surface = *surface;
            match op {
                0 => self.destroy(id),
                1 => {
                    let new = a.u();
                    self.new_obj(new, Obj::Toplevel { xdg_surface: id }, parent_ver);
                    self.make_toplevel(c, surface, new);
                    self.configure_toplevel(surface, 0, 0, true);
                }
                2 => {
                    let new = a.u();
                    let parent_xdg = a.u();
                    let pos = a.u();
                    self.new_obj(new, Obj::Popup { xdg_surface: id }, parent_ver);
                    let positioner = match self.objs.get(&pos) {
                        Some(Obj::Positioner(p)) => *p,
                        _ => Positioner::default(),
                    };
                    let parent = match self.objs.get(&parent_xdg) {
                        Some(Obj::XdgSurface { surface, .. }) => *surface,
                        _ => 0,
                    };
                    // Positions are relative to the parent's window geometry.
                    let (gx, gy) = self.geometry_of(parent).map(|g| (g.0, g.1)).unwrap_or((0, 0));
                    let (x, y) = positioner.place();
                    if let Some(s) = self.surface(surface) {
                        s.role = Role::Popup { parent, x: x + gx, y: y + gy, xdg_popup: new };
                    }
                    if let Some(p) = self.surface(parent) {
                        p.children.push(surface);
                    }
                    self.ev(new, 0, vec![A::I(x), A::I(y), A::I(positioner.size.0), A::I(positioner.size.1)]);
                    let serial = self.next_serial();
                    self.ev(id, 0, vec![A::U(serial)]);
                }
                3 => {
                    let g = (a.i(), a.i(), a.i(), a.i());
                    if let Some(Obj::XdgSurface { geometry, .. }) = self.objs.get_mut(&id) {
                        *geometry = Some(g);
                    }
                }
                _ => {} // ack_configure
            }
        } else if let Some(Obj::Toplevel { xdg_surface }) = self.objs.get(&id) {
            let xdg = *xdg_surface;
            let surface = match self.objs.get(&xdg) {
                Some(Obj::XdgSurface { surface, .. }) => *surface,
                _ => 0,
            };
            match op {
                0 => {
                    self.end_toplevel(surface);
                    self.destroy(id);
                }
                2 => {
                    let t = a.s();
                    if let Some(w) = self.window_of(surface) {
                        *w.title.lock() = t;
                    }
                }
                8 => {
                    let (w, h) = (a.i(), a.i());
                    if let Some(win) = self.window_of(surface) {
                        *win.min_size.lock() = (w, h);
                    }
                }
                _ => {}
            }
        } else if let Some(Obj::Popup { xdg_surface }) = self.objs.get(&id) {
            let xdg = *xdg_surface;
            if op == 0 {
                if let Some(Obj::XdgSurface { surface, .. }) = self.objs.get(&xdg) {
                    let s = *surface;
                    self.unlink_surface(s);
                }
                self.destroy(id);
            }
        } else if is!(Obj::Shell) {
            if op == 0 {
                let new = a.u();
                let surface = a.u();
                self.new_obj(new, Obj::ShellSurface { surface }, 1);
            }
        } else if let Some(Obj::ShellSurface { surface }) = self.objs.get(&id) {
            let surface = *surface;
            match op {
                3 | 5 | 7 => {
                    if self.window_of(surface).is_none() {
                        self.make_toplevel(c, surface, id);
                    }
                }
                8 => {
                    let t = a.s();
                    if let Some(w) = self.window_of(surface) {
                        *w.title.lock() = t;
                    }
                }
                _ => {}
            }
        } else if is!(Obj::DataDeviceManager) {
            match op {
                0 => self.new_obj(a.u(), Obj::DataSource, 1),
                1 => self.new_obj(a.u(), Obj::DataDevice, parent_ver),
                _ => {}
            }
        } else if is!(Obj::DataSource) {
            if op == 1 {
                self.destroy(id);
            }
        } else if is!(Obj::DataDevice) {
            if op == 2 {
                self.destroy(id);
            }
        } else if is!(Obj::DecorationManager) {
            match op {
                0 => self.destroy(id),
                1 => {
                    let new = a.u();
                    self.new_obj(new, Obj::Decoration, 1);
                    self.ev(new, 0, vec![A::U(2)]); // server side
                }
                _ => {}
            }
        } else if is!(Obj::Decoration) {
            match op {
                0 => self.destroy(id),
                _ => self.ev(id, 0, vec![A::U(2)]),
            }
        } else if matches!(self.objs.get(&id), Some(Obj::Callback | Obj::Other)) {
            // nothing to do
        } else if !self.logged.contains(&(op as u32)) {
            self.logged.push(op as u32);
        }
    }

    fn bind(&mut self, c: &Client, name: u32, ver: u32, new: u32) {
        let Some(g) = GLOBALS.iter().find(|g| g.name == name) else { return };
        let ver = ver.min(g.version).max(1);
        let obj = match g.iface {
            "wl_compositor" => Obj::Compositor,
            "wl_subcompositor" => Obj::SubCompositor,
            "wl_shm" => Obj::Shm,
            "wl_seat" => Obj::Seat,
            "wl_output" => Obj::Output,
            "xdg_wm_base" => Obj::WmBase,
            "wl_shell" => Obj::Shell,
            "wl_data_device_manager" => Obj::DataDeviceManager,
            "zxdg_decoration_manager_v1" => Obj::DecorationManager,
            _ => Obj::Other,
        };
        self.new_obj(new, obj, ver);
        match g.iface {
            "wl_shm" => {
                self.ev(new, 0, vec![A::U(0)]);
                self.ev(new, 0, vec![A::U(1)]);
            }
            "wl_seat" => {
                self.ev(new, 0, vec![A::U(3)]); // pointer | keyboard
                if ver >= 2 {
                    self.ev(new, 1, vec![A::S("seat0")]);
                }
            }
            "wl_output" => {
                let (w, h) = crate::gui::display_mode();
                self.ev(new, 0, vec![A::I(0), A::I(0), A::I(w as i32 * 254 / 960), A::I(h as i32 * 254 / 960), A::I(0), A::S("MayOS"), A::S("Display"), A::I(0)]);
                self.ev(new, 1, vec![A::U(3), A::I(w as i32), A::I(h as i32), A::I(60_000)]);
                if ver >= 2 {
                    self.ev(new, 3, vec![A::I(1)]);
                    self.ev(new, 2, vec![]);
                }
            }
            _ => {}
        }
        let _ = c;
    }

    fn surface_request(&mut self, id: u32, op: u16, a: &mut Args) {
        match op {
            0 => {
                self.end_toplevel(id);
                self.unlink_surface(id);
                self.destroy(id);
            }
            1 => {
                let b = a.u();
                if let Some(s) = self.surface(id) {
                    s.pending = Some(if b == 0 { None } else { Some(b) });
                }
            }
            3 => {
                let cb = a.u();
                self.new_obj(cb, Obj::Callback, 1);
                if let Some(s) = self.surface(id) {
                    s.frames.push(cb);
                }
            }
            6 => self.commit(id),
            _ => {} // damage, regions, transform, scale
        }
    }

    fn commit(&mut self, id: u32) {
        let Some(s) = self.surface(id) else { return };
        let frames = core::mem::take(&mut s.frames);
        let pending = s.pending.take();
        let needs_enter = !s.entered_output;
        self.frame_ready.extend(frames);
        if let Some(att) = pending {
            match att {
                None => {
                    if let Some(s) = self.surface(id) {
                        s.image = None;
                    }
                }
                Some(bid) => {
                    if let Some(Obj::Buffer(b)) = self.objs.get(&bid) {
                        let b = b.clone();
                        let img = copy_buffer(&b);
                        if let Some(s) = self.surface(id) {
                            s.image = img;
                        }
                        // Pixels are copied: the program may reuse the buffer.
                        self.ev(bid, 0, vec![]);
                    }
                }
            }
        }
        if needs_enter {
            let outputs: Vec<u32> = self.objs.iter().filter(|(_, o)| matches!(o, Obj::Output)).map(|(k, _)| *k).collect();
            if let Some(o) = outputs.first() {
                if let Some(s) = self.surface(id) {
                    s.entered_output = true;
                }
                self.ev(id, 0, vec![A::U(*o)]);
            }
        }
        if let Some(root) = self.root_of(id) {
            self.compose(root);
        }
    }

    /// The toplevel surface a surface is drawn into.
    fn root_of(&self, mut id: u32) -> Option<u32> {
        for _ in 0..16 {
            match self.objs.get(&id) {
                Some(Obj::Surface(s)) => match &s.role {
                    Role::Toplevel(_) => return Some(id),
                    Role::Sub { parent, .. } | Role::Popup { parent, .. } => id = *parent,
                    Role::None => return None,
                },
                _ => return None,
            }
        }
        None
    }

    fn window_of(&self, surface: u32) -> Option<Arc<Window>> {
        match self.objs.get(&surface) {
            Some(Obj::Surface(Surface { role: Role::Toplevel(w), .. })) => Some(w.clone()),
            _ => None,
        }
    }

    /// xdg window geometry of a toplevel or popup surface.
    fn geometry_of(&self, surface: u32) -> Option<(i32, i32, i32, i32)> {
        self.objs.values().find_map(|o| match o {
            Obj::XdgSurface { surface: s, geometry } if *s == surface => *geometry,
            _ => None,
        })
    }

    fn make_toplevel(&mut self, c: &Client, surface: u32, _role_obj: u32) {
        let w = Arc::new(Window {
            client: c.me.lock().clone(),
            surface,
            title: Spin::new(String::from("Linux program")),
            image: Spin::new(None),
            version: AtomicU32::new(0),
            gone: AtomicBool::new(false),
            min_size: Spin::new((0, 0)),
            opened: AtomicBool::new(false),
            pid: c.pid,
        });
        if let Some(s) = self.surface(surface) {
            s.role = Role::Toplevel(w);
        }
    }

    fn end_toplevel(&mut self, surface: u32) {
        if let Some(w) = self.window_of(surface) {
            w.gone.store(true, Ordering::Relaxed);
            if let Some(s) = self.surface(surface) {
                s.role = Role::None;
            }
        }
    }

    fn unlink_surface(&mut self, id: u32) {
        let parent = match self.surface(id).map(|s| s.role.clone()) {
            Some(Role::Sub { parent, .. }) | Some(Role::Popup { parent, .. }) => Some(parent),
            _ => None,
        };
        if let Some(p) = parent {
            if let Some(ps) = self.surface(p) {
                ps.children.retain(|&c| c != id);
            }
            if let Some(s) = self.surface(id) {
                s.role = Role::None;
                s.image = None;
            }
            if let Some(root) = self.root_of(p) {
                self.compose(root);
            }
        }
    }

    /// Draw a toplevel and everything on it into its window picture.
    fn compose(&mut self, root: u32) {
        let Some(win) = self.window_of(root) else { return };
        let Some((bw, bh, _)) = self.surface(root).and_then(|s| s.image.clone()) else {
            *win.image.lock() = None;
            return;
        };
        let (gx, gy, gw, gh) = self.geometry_of(root).filter(|g| g.2 > 0 && g.3 > 0).unwrap_or((0, 0, bw, bh));
        let (w, h) = (gw.min(4096), gh.min(4096));
        let mut out = vec![0xff00_0000u32; (w * h) as usize];
        self.draw_tree(root, -gx, -gy, &mut out, w, h, 0);
        *win.image.lock() = Some((w, h, Arc::new(out)));
        win.version.fetch_add(1, Ordering::Relaxed);
        if !win.opened.swap(true, Ordering::Relaxed) {
            PENDING.lock().push(win);
        }
    }

    fn draw_tree(&self, id: u32, x: i32, y: i32, out: &mut [u32], w: i32, h: i32, depth: u32) {
        let Some(Obj::Surface(s)) = self.objs.get(&id) else { return };
        if let Some((iw, ih, px)) = &s.image {
            blend(out, w, h, px, *iw, *ih, x, y);
        }
        if depth > 8 {
            return;
        }
        for &c in &s.children {
            let Some(Obj::Surface(cs)) = self.objs.get(&c) else { continue };
            let (cx, cy) = match cs.role {
                Role::Sub { x: sx, y: sy, .. } => (x + sx, y + sy),
                Role::Popup { x: px, y: py, .. } => {
                    // Popup coordinates are in the parent's geometry space.
                    let (gx, gy) = self.geometry_of(c).map(|g| (g.0, g.1)).unwrap_or((0, 0));
                    (x + px - gx, y + py - gy)
                }
                _ => continue,
            };
            self.draw_tree(c, cx, cy, out, w, h, depth + 1);
        }
    }

    // --- configure / close --------------------------------------------------

    fn configure_toplevel(&mut self, surface: u32, w: i32, h: i32, activated: bool) {
        let xdg = self.objs.iter().find_map(|(k, o)| match o {
            Obj::XdgSurface { surface: s, .. } if *s == surface => Some(*k),
            _ => None,
        });
        let top = xdg.and_then(|x| {
            self.objs.iter().find_map(|(k, o)| match o {
                Obj::Toplevel { xdg_surface } if *xdg_surface == x => Some(*k),
                _ => None,
            })
        });
        if let (Some(x), Some(t)) = (xdg, top) {
            let states: Vec<u8> = if activated { 4u32.to_le_bytes().to_vec() } else { Vec::new() };
            self.ev(t, 0, vec![A::I(w), A::I(h), A::Arr(&states)]);
            let serial = self.next_serial();
            self.ev(x, 0, vec![A::U(serial)]);
            return;
        }
        // wl_shell_surface.configure(edges, w, h)
        let shell = self.objs.iter().find_map(|(k, o)| match o {
            Obj::ShellSurface { surface: s } if *s == surface => Some(*k),
            _ => None,
        });
        if let Some(sh) = shell
            && w > 0
            && h > 0
        {
            self.ev(sh, 1, vec![A::U(0), A::I(w), A::I(h)]);
        }
    }

    fn close_toplevel(&mut self, surface: u32) {
        let xdg = self.objs.iter().find_map(|(k, o)| match o {
            Obj::XdgSurface { surface: s, .. } if *s == surface => Some(*k),
            _ => None,
        });
        let top = xdg.and_then(|x| {
            self.objs.iter().find_map(|(k, o)| match o {
                Obj::Toplevel { xdg_surface } if *xdg_surface == x => Some(*k),
                _ => None,
            })
        });
        if let Some(t) = top {
            self.ev(t, 1, vec![]);
        }
    }

    // --- input ----------------------------------------------------------

    /// Topmost surface at toplevel position (x, y) (window geometry space):
    /// (surface, its origin in the same space).
    fn hit(&self, root: u32, x: i32, y: i32) -> (u32, i32, i32) {
        let (gx, gy) = self.geometry_of(root).map(|g| (g.0, g.1)).unwrap_or((0, 0));
        let mut best = (root, -gx, -gy);
        fn walk(st: &State, id: u32, ox: i32, oy: i32, x: i32, y: i32, best: &mut (u32, i32, i32), depth: u32) {
            let Some(Obj::Surface(s)) = st.objs.get(&id) else { return };
            if let Some((w, h, _)) = &s.image
                && x >= ox
                && y >= oy
                && x < ox + w
                && y < oy + h
            {
                *best = (id, ox, oy);
            }
            if depth > 8 {
                return;
            }
            for &c in &s.children {
                let Some(Obj::Surface(cs)) = st.objs.get(&c) else { continue };
                let (cx, cy) = match cs.role {
                    Role::Sub { x: sx, y: sy, .. } => (ox + sx, oy + sy),
                    Role::Popup { x: px, y: py, .. } => {
                        let (gx, gy) = st.geometry_of(c).map(|g| (g.0, g.1)).unwrap_or((0, 0));
                        (ox + px - gx, oy + py - gy)
                    }
                    _ => continue,
                };
                walk(st, c, cx, cy, x, y, best, depth + 1);
            }
        }
        walk(self, root, -gx, -gy, x, y, &mut best, 0);
        best
    }

    fn pointer_frame(&mut self) {
        let ps: Vec<u32> = self.pointers.clone();
        for p in ps {
            if self.version(p) >= 5 {
                self.ev(p, 5, vec![]);
            }
        }
    }

    fn pointer_motion(&mut self, root: u32, x: i32, y: i32) {
        let (s, ox, oy) = self.hit(root, x, y);
        let (sx, sy) = ((x - ox) * 256, (y - oy) * 256);
        let ps: Vec<u32> = self.pointers.clone();
        if self.pointer_on.map(|p| p.0) != Some(s) {
            if let Some((old, _, _)) = self.pointer_on {
                let serial = self.next_serial();
                for &p in &ps {
                    self.ev(p, 1, vec![A::U(serial), A::U(old)]);
                }
            }
            let serial = self.next_serial();
            for &p in &ps {
                self.ev(p, 0, vec![A::U(serial), A::U(s), A::F(sx), A::F(sy)]);
            }
            self.pointer_on = Some((s, ox, oy));
        } else {
            let t = now_ms();
            for &p in &ps {
                self.ev(p, 2, vec![A::U(t), A::F(sx), A::F(sy)]);
            }
        }
        self.pointer_frame();
    }

    fn pointer_leave(&mut self) {
        if let Some((old, _, _)) = self.pointer_on.take() {
            let serial = self.next_serial();
            let ps: Vec<u32> = self.pointers.clone();
            for p in ps {
                self.ev(p, 1, vec![A::U(serial), A::U(old)]);
            }
            self.pointer_frame();
        }
    }

    fn pointer_button(&mut self, root: u32, button: u8, down: bool) {
        // A press outside every popup dismisses them.
        if down {
            let on = self.pointer_on.map(|p| p.0);
            let popups: Vec<(u32, u32)> = self
                .objs
                .iter()
                .filter_map(|(k, o)| match o {
                    Obj::Surface(Surface { role: Role::Popup { xdg_popup, .. }, .. }) => Some((*k, *xdg_popup)),
                    _ => None,
                })
                .collect();
            let inside = popups.iter().any(|(s, _)| Some(*s) == on);
            if !inside {
                for (_, p) in popups.iter().rev() {
                    self.ev(*p, 1, vec![]);
                }
            }
        }
        let _ = root;
        let code = 0x110 + button.min(2) as u32;
        let serial = self.next_serial();
        let t = now_ms();
        let ps: Vec<u32> = self.pointers.clone();
        for p in ps {
            self.ev(p, 3, vec![A::U(serial), A::U(t), A::U(code), A::U(down as u32)]);
        }
        self.pointer_frame();
    }

    fn pointer_axis(&mut self, delta: i32) {
        let t = now_ms();
        let v = -delta.signum() * 10 * 256;
        let ps: Vec<u32> = self.pointers.clone();
        for p in ps {
            self.ev(p, 4, vec![A::U(t), A::U(0), A::F(v)]);
        }
        self.pointer_frame();
    }

    fn send_keymap(&mut self, k: u32) {
        let shm = Shm::new();
        shm.write_at(0, KEYMAP);
        shm.write_at(KEYMAP.len() as u64, &[0]);
        let d: DescRef = Arc::new(Mutex::new(Desc::Memfd { shm, pos: 0 }));
        self.ev(k, 0, vec![A::U(1), A::Fd(d), A::U(KEYMAP.len() as u32 + 1)]);
        if self.version(k) >= 4 {
            self.ev(k, 5, vec![A::I(25), A::I(500)]);
        }
    }

    fn keyboard_focus(&mut self, _c: &Client, surface: u32, on: bool) {
        let ks: Vec<u32> = self.keyboards.clone();
        let serial = self.next_serial();
        if on {
            if self.keyboard_on == Some(surface) {
                return;
            }
            self.keyboard_on = Some(surface);
            for &k in &ks {
                self.ev(k, 1, vec![A::U(serial), A::U(surface), A::Arr(&[])]);
            }
            self.send_mods(0);
            let size = self.window_of(surface).and_then(|w| w.image.lock().as_ref().map(|i| (i.0, i.1)));
            if let Some((w, h)) = size {
                self.configure_toplevel(surface, w, h, true);
            }
        } else if self.keyboard_on == Some(surface) {
            self.keyboard_on = None;
            for &k in &ks {
                self.ev(k, 2, vec![A::U(serial), A::U(surface)]);
            }
        }
    }

    fn send_mods(&mut self, mods: u32) {
        self.mods = mods;
        let serial = self.next_serial();
        let ks: Vec<u32> = self.keyboards.clone();
        for k in ks {
            self.ev(k, 4, vec![A::U(serial), A::U(mods & !2), A::U(0), A::U(mods & 2), A::U(0)]);
        }
    }

    fn key(&mut self, c: &Client, surface: u32, k: &KeyEvent) {
        if self.keyboard_on != Some(surface) {
            self.keyboard_focus(c, surface, true);
        }
        // XKB modifier bits: Shift 1, Lock 2, Control 4, Mod1 (Alt) 8.
        let mut mods = (k.shift as u32) | (k.ctrl as u32) << 2 | (k.alt as u32) << 3 | (self.mods & 2);
        if k.key == Key::CapsLock && k.pressed {
            mods ^= 2;
        }
        if mods != self.mods {
            self.send_mods(mods);
        }
        let Some(code) = super::screen::keycode(k.key) else { return };
        let serial = self.next_serial();
        let t = now_ms();
        let ks: Vec<u32> = self.keyboards.clone();
        for kb in ks {
            self.ev(kb, 3, vec![A::U(serial), A::U(t), A::U(code as u32), A::U(k.pressed as u32)]);
        }
    }
}

/// Copy a buffer's pixels out of shared memory.
fn copy_buffer(b: &Buffer) -> Option<(i32, i32, Arc<Vec<u32>>)> {
    if b.w <= 0 || b.h <= 0 || b.stride < b.w * 4 {
        return None;
    }
    let mut px = vec![0u32; (b.w * b.h) as usize];
    let mut row = vec![0u8; (b.w * 4) as usize];
    for y in 0..b.h {
        if !b.shm.copy_out(b.off + y as u64 * b.stride as u64, &mut row) {
            return None;
        }
        let dst = &mut px[(y * b.w) as usize..((y + 1) * b.w) as usize];
        for (d, s) in dst.iter_mut().zip(row.chunks_exact(4)) {
            let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            *d = if b.opaque { v | 0xff00_0000 } else { v };
        }
    }
    Some((b.w, b.h, Arc::new(px)))
}

/// Draw premultiplied ARGB `src` over `dst` at (x, y).
#[allow(clippy::too_many_arguments)]
fn blend(dst: &mut [u32], dw: i32, dh: i32, src: &[u32], sw: i32, sh: i32, x: i32, y: i32) {
    let (x0, y0) = (x.max(0), y.max(0));
    let (x1, y1) = ((x + sw).min(dw), (y + sh).min(dh));
    for yy in y0..y1 {
        let srow = &src[((yy - y) * sw) as usize..];
        let drow = &mut dst[(yy * dw) as usize..];
        for xx in x0..x1 {
            let s = srow[(xx - x) as usize];
            let a = s >> 24;
            let d = &mut drow[xx as usize];
            if a == 255 {
                *d = s;
            } else if a != 0 {
                let inv = 255 - a;
                let mix = |sh: u32| {
                    let sc = (s >> sh) & 255;
                    let dc = (*d >> sh) & 255;
                    ((sc + dc * inv / 255).min(255)) << sh
                };
                *d = 0xff00_0000 | mix(16) | mix(8) | mix(0);
            }
        }
    }
}

// -------------------------------------------------------------------------
// Service
// -------------------------------------------------------------------------

impl Client {
    fn flush(&self, st: &mut State) {
        if st.out.is_empty() && st.outfds.is_empty() {
            return;
        }
        let bytes = core::mem::take(&mut st.out);
        let fds = core::mem::take(&mut st.outfds);
        self.to_client.lock().push(&bytes, fds);
    }

    fn frame_done(&self) {
        let mut st = self.st.lock();
        if st.frame_ready.is_empty() {
            return;
        }
        let t = now_ms();
        for cb in core::mem::take(&mut st.frame_ready) {
            if st.objs.remove(&cb).is_some() {
                st.ev(cb, 0, vec![A::U(t)]);
                st.ev(1, 1, vec![A::U(cb)]);
            }
        }
        self.flush(&mut st);
    }
}

impl Service for Client {
    fn receive(&self, data: &[u8], fds: Vec<DescRef>) {
        let mut st = self.st.lock();
        if st.objs.is_empty() {
            st.objs.insert(1, Obj::Display);
        }
        st.inbuf.extend_from_slice(data);
        st.infds.extend(fds);
        let mut o = 0;
        let buf = core::mem::take(&mut st.inbuf);
        while o + 8 <= buf.len() {
            let id = u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
            let w = u32::from_le_bytes(buf[o + 4..o + 8].try_into().unwrap());
            let (size, op) = ((w >> 16) as usize, (w & 0xffff) as u16);
            if size < 8 || o + size > buf.len() {
                if size < 8 {
                    o = buf.len(); // garbage: drop it
                }
                break;
            }
            let mut a = Args { b: &buf[o + 8..o + size], o: 0 };
            st.request(self, id, op, &mut a);
            o += size;
        }
        st.inbuf = buf[o..].to_vec();
        self.flush(&mut st);
    }

    fn closed(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let mut st = self.st.lock();
        for o in st.objs.values() {
            if let Obj::Surface(Surface { role: Role::Toplevel(w), .. }) = o {
                w.gone.store(true, Ordering::Relaxed);
            }
        }
        st.objs.clear();
        st.infds.clear();
    }
}
