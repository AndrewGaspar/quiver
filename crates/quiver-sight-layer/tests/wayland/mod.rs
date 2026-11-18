//! A fake Wayland compositor (libwayland-server, on its own thread) and a fake game
//! (libwayland-client, with a reader thread that loops on `wl_display_dispatch` the way
//! Wine's `waylanddrv_unix_read_events` does), connected by a socketpair in this process.
//! For tests/keys_wayland.rs: the layer's keyboard goes on the game's display, as in a game.
//!
//! The compositor does what Hyprland's seat does for keys: `enter`/`key`/`leave` go to every
//! `wl_keyboard` of the focused client (on every `wl_seat` it bound), and a keyboard made
//! while the client has focus gets an `enter` at once. Both libraries are `dlopen`ed (the
//! layer has no link-time dependency and neither does this test).

#![allow(dead_code)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const KEYMAP_NAME: &CStr = c"quiver-test-keymap";

unsafe fn open(name: &CStr) -> Option<*mut c_void> {
    let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    (!h.is_null()).then_some(h)
}

unsafe fn sym<T: Copy>(h: *mut c_void, name: &CStr) -> T {
    let p = unsafe { libc::dlsym(h, name.as_ptr()) };
    assert!(!p.is_null(), "{name:?} missing");
    assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
    unsafe { std::mem::transmute_copy(&p) }
}

#[repr(C)]
struct WlArray {
    size: usize,
    alloc: usize,
    data: *mut c_void,
}

// ---- Server --------------------------------------------------------------------------------

type Dispatcher = unsafe extern "C" fn(*const c_void, *mut c_void, u32, *const c_void, *mut u64) -> c_int;
type Bind = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32);
type ResourceDestroy = unsafe extern "C" fn(*mut c_void);

struct Slib {
    display_create: unsafe extern "C" fn() -> *mut c_void,
    display_destroy: unsafe extern "C" fn(*mut c_void),
    get_event_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    loop_dispatch: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    flush_clients: unsafe extern "C" fn(*mut c_void),
    client_create: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void,
    client_destroy: unsafe extern "C" fn(*mut c_void),
    global_create: unsafe extern "C" fn(*mut c_void, *const c_void, c_int, *mut c_void, Bind) -> *mut c_void,
    resource_create: unsafe extern "C" fn(*mut c_void, *const c_void, c_int, u32) -> *mut c_void,
    set_dispatcher: unsafe extern "C" fn(*mut c_void, Dispatcher, *const c_void, *mut c_void, Option<ResourceDestroy>),
    post_event: unsafe extern "C" fn(*mut c_void, u32, ...),
    resource_destroy: unsafe extern "C" fn(*mut c_void),
    get_version: unsafe extern "C" fn(*mut c_void) -> c_int,
    get_user_data: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    compositor_iface: *const c_void,
    surface_iface: *const c_void,
    region_iface: *const c_void,
    seat_iface: *const c_void,
    keyboard_iface: *const c_void,
    pointer_iface: *const c_void,
}

unsafe impl Send for Slib {}
unsafe impl Sync for Slib {}

fn slib() -> Option<&'static Slib> {
    static L: std::sync::OnceLock<Option<Slib>> = std::sync::OnceLock::new();
    L.get_or_init(|| unsafe {
        let h = open(c"libwayland-server.so.0")?;
        Some(Slib {
            display_create: sym(h, c"wl_display_create"),
            display_destroy: sym(h, c"wl_display_destroy"),
            get_event_loop: sym(h, c"wl_display_get_event_loop"),
            loop_dispatch: sym(h, c"wl_event_loop_dispatch"),
            flush_clients: sym(h, c"wl_display_flush_clients"),
            client_create: sym(h, c"wl_client_create"),
            client_destroy: sym(h, c"wl_client_destroy"),
            global_create: sym(h, c"wl_global_create"),
            resource_create: sym(h, c"wl_resource_create"),
            set_dispatcher: sym(h, c"wl_resource_set_dispatcher"),
            post_event: sym(h, c"wl_resource_post_event"),
            resource_destroy: sym(h, c"wl_resource_destroy"),
            get_version: sym(h, c"wl_resource_get_version"),
            get_user_data: sym(h, c"wl_resource_get_user_data"),
            compositor_iface: sym(h, c"wl_compositor_interface"),
            surface_iface: sym(h, c"wl_surface_interface"),
            region_iface: sym(h, c"wl_region_interface"),
            seat_iface: sym(h, c"wl_seat_interface"),
            keyboard_iface: sym(h, c"wl_keyboard_interface"),
            pointer_iface: sym(h, c"wl_pointer_interface"),
        })
    })
    .as_ref()
}

/// What the compositor saw, for assertions.
#[derive(Default)]
pub struct Stats {
    pub seats_bound: AtomicU32,
    pub seats_live: AtomicU32,
    pub keyboards_made: AtomicU32,
    pub keyboards_live: AtomicU32,
    pub keyboards_released: AtomicU32,
    pub seats_released: AtomicU32,
}

pub enum Cmd {
    /// Focus the client's surface with these evdev keys held.
    Focus(Vec<u32>),
    Leave,
    Key(u32, bool),
    /// Drop the client (its connection breaks).
    Kill,
    /// Reply once everything before it was sent and flushed.
    Sync(mpsc::Sender<()>),
    Stop,
}

pub struct ServerOpts {
    pub seat: bool,
    pub keyboard_cap: bool,
}

struct Srv {
    lib: &'static Slib,
    display: *mut c_void,
    client: *mut c_void,
    surface: *mut c_void,
    keyboards: Vec<*mut c_void>,
    focused: bool,
    held: Vec<u32>,
    serial: u32,
    caps: u32,
    stats: Arc<Stats>,
}

impl Srv {
    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    unsafe fn send_keymap(&self, kb: *mut c_void) {
        unsafe {
            let fd = libc::memfd_create(KEYMAP_NAME.as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            let text = b"xkb_keymap { };\0";
            libc::write(fd, text.as_ptr().cast(), text.len());
            // Format 1 (xkb_v1); libwayland dups the fd for the message, so ours closes now.
            (self.lib.post_event)(kb, 0, 1u32, fd, text.len() as u32);
            libc::close(fd);
        }
    }

    unsafe fn enter(&mut self, kb: *mut c_void) {
        if self.surface.is_null() {
            return;
        }
        let serial = self.next_serial();
        let mut held = self.held.clone();
        let mut arr = WlArray { size: held.len() * 4, alloc: held.len() * 4, data: held.as_mut_ptr().cast() };
        unsafe {
            (self.lib.post_event)(kb, 1, serial, self.surface, &mut arr as *mut WlArray);
            (self.lib.post_event)(kb, 4, serial, 0u32, 0u32, 0u32, 0u32);
        }
    }

    unsafe fn cmd(&mut self, c: Cmd) -> bool {
        unsafe {
            match c {
                Cmd::Focus(held) => {
                    self.held = held;
                    self.focused = true;
                    for kb in self.keyboards.clone() {
                        self.enter(kb);
                    }
                }
                Cmd::Leave => {
                    self.focused = false;
                    let serial = self.next_serial();
                    for &kb in &self.keyboards {
                        (self.lib.post_event)(kb, 2, serial, self.surface);
                    }
                }
                Cmd::Key(code, down) => {
                    if down {
                        self.held.push(code);
                    } else {
                        self.held.retain(|&k| k != code);
                    }
                    if self.focused {
                        let serial = self.next_serial();
                        for &kb in &self.keyboards {
                            (self.lib.post_event)(kb, 3, serial, 1000u32, code, down as u32);
                        }
                    }
                }
                Cmd::Kill => {
                    let client = std::mem::replace(&mut self.client, std::ptr::null_mut());
                    self.keyboards.clear();
                    self.surface = std::ptr::null_mut();
                    self.focused = false;
                    if !client.is_null() {
                        (self.lib.client_destroy)(client);
                    }
                }
                Cmd::Sync(tx) => {
                    (self.lib.flush_clients)(self.display);
                    let _ = tx.send(());
                }
                Cmd::Stop => return false,
            }
        }
        true
    }
}

unsafe fn srv<'a>(data: *mut c_void) -> &'a mut Srv {
    unsafe { &mut *(data as *mut Srv) }
}

unsafe extern "C" fn bind_compositor(client: *mut c_void, data: *mut c_void, version: u32, id: u32) {
    unsafe {
        let s = srv(data);
        let r = (s.lib.resource_create)(client, s.lib.compositor_iface, version as c_int, id);
        (s.lib.set_dispatcher)(r, compositor_req, std::ptr::null(), data, None);
    }
}

unsafe extern "C" fn compositor_req(_: *const c_void, res: *mut c_void, opcode: u32, _: *const c_void, args: *mut u64) -> c_int {
    unsafe {
        let s = srv((s_lib().get_user_data)(res));
        let id = *args as u32;
        let client = client_of(res);
        let iface = if opcode == 0 { s.lib.surface_iface } else { s.lib.region_iface };
        let r = (s.lib.resource_create)(client, iface, (s.lib.get_version)(res), id);
        (s.lib.set_dispatcher)(r, destroy_req, std::ptr::null(), (s as *mut Srv).cast(), Some(surface_gone));
        if opcode == 0 {
            s.surface = r;
        }
    }
    0
}

unsafe extern "C" fn surface_gone(res: *mut c_void) {
    unsafe {
        let s = srv((s_lib().get_user_data)(res));
        if s.surface == res {
            s.surface = std::ptr::null_mut();
        }
    }
}

/// wl_surface / wl_region: opcode 0 is `destroy`; everything else is ignored. (Also the
/// inert wl_pointer, which nothing here makes.)
unsafe extern "C" fn destroy_req(_: *const c_void, res: *mut c_void, opcode: u32, _: *const c_void, _: *mut u64) -> c_int {
    if opcode == 0 {
        unsafe { (s_lib().resource_destroy)(res) };
    }
    0
}

unsafe extern "C" fn bind_seat(client: *mut c_void, data: *mut c_void, version: u32, id: u32) {
    unsafe {
        let s = srv(data);
        let r = (s.lib.resource_create)(client, s.lib.seat_iface, version as c_int, id);
        (s.lib.set_dispatcher)(r, seat_req, std::ptr::null(), data, Some(seat_gone));
        s.stats.seats_bound.fetch_add(1, Ordering::Relaxed);
        s.stats.seats_live.fetch_add(1, Ordering::Relaxed);
        (s.lib.post_event)(r, 0, s.caps);
        if version >= 2 {
            (s.lib.post_event)(r, 1, c"seat0".as_ptr());
        }
    }
}

unsafe extern "C" fn seat_gone(res: *mut c_void) {
    unsafe { srv((s_lib().get_user_data)(res)).stats.seats_live.fetch_sub(1, Ordering::Relaxed) };
}

unsafe extern "C" fn seat_req(_: *const c_void, res: *mut c_void, opcode: u32, _: *const c_void, args: *mut u64) -> c_int {
    unsafe {
        let s = srv((s_lib().get_user_data)(res));
        let version = (s.lib.get_version)(res);
        match opcode {
            1 => {
                let kb = (s.lib.resource_create)(client_of(res), s.lib.keyboard_iface, version, *args as u32);
                (s.lib.set_dispatcher)(kb, keyboard_req, std::ptr::null(), (s as *mut Srv).cast(), Some(keyboard_gone));
                s.keyboards.push(kb);
                s.stats.keyboards_made.fetch_add(1, Ordering::Relaxed);
                s.stats.keyboards_live.fetch_add(1, Ordering::Relaxed);
                s.send_keymap(kb);
                if version >= 4 {
                    (s.lib.post_event)(kb, 5, 25i32, 600i32);
                }
                if s.focused {
                    s.enter(kb);
                }
            }
            3 => {
                s.stats.seats_released.fetch_add(1, Ordering::Relaxed);
                (s.lib.resource_destroy)(res);
            }
            _ => {
                // get_pointer / get_touch: an inert object.
                let r = (s.lib.resource_create)(client_of(res), s.lib.pointer_iface, version, *args as u32);
                (s.lib.set_dispatcher)(r, destroy_req, std::ptr::null(), (s as *mut Srv).cast(), None);
            }
        }
    }
    0
}

unsafe extern "C" fn keyboard_req(_: *const c_void, res: *mut c_void, opcode: u32, _: *const c_void, _: *mut u64) -> c_int {
    unsafe {
        if opcode == 0 {
            srv((s_lib().get_user_data)(res)).stats.keyboards_released.fetch_add(1, Ordering::Relaxed);
            (s_lib().resource_destroy)(res);
        }
    }
    0
}

unsafe extern "C" fn keyboard_gone(res: *mut c_void) {
    unsafe {
        let s = srv((s_lib().get_user_data)(res));
        s.keyboards.retain(|&k| k != res);
        s.stats.keyboards_live.fetch_sub(1, Ordering::Relaxed);
    }
}

fn s_lib() -> &'static Slib {
    slib().expect("loaded")
}

unsafe fn client_of(res: *mut c_void) -> *mut c_void {
    type GetClient = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    static F: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let f = *F.get_or_init(|| unsafe {
        let h = libc::dlopen(c"libwayland-server.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
        libc::dlsym(h, c"wl_resource_get_client".as_ptr()) as usize
    });
    unsafe { std::mem::transmute::<usize, GetClient>(f)(res) }
}

pub struct Server {
    tx: mpsc::Sender<Cmd>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub stats: Arc<Stats>,
}

impl Server {
    /// Starts the compositor with one client on `fd` (the server end of a socketpair).
    pub fn start(fd: c_int, opts: ServerOpts) -> Option<Server> {
        let lib = slib()?;
        let stats = Arc::new(Stats::default());
        let (tx, rx) = mpsc::channel::<Cmd>();
        let st = stats.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || unsafe {
            let display = (lib.display_create)();
            let lp = (lib.get_event_loop)(display);
            let s = Box::into_raw(Box::new(Srv {
                lib,
                display,
                client: std::ptr::null_mut(),
                surface: std::ptr::null_mut(),
                keyboards: Vec::new(),
                focused: false,
                held: Vec::new(),
                serial: 0,
                caps: if opts.keyboard_cap { 2 | 1 } else { 1 },
                stats: st,
            }));
            (lib.global_create)(display, lib.compositor_iface, 4, s.cast(), bind_compositor);
            if opts.seat {
                (lib.global_create)(display, lib.seat_iface, 7, s.cast(), bind_seat);
            }
            (*s).client = (lib.client_create)(display, fd);
            let _ = ready_tx.send(());
            'run: loop {
                (lib.loop_dispatch)(lp, 2);
                (lib.flush_clients)(display);
                while let Ok(c) = rx.try_recv() {
                    if !(*s).cmd(c) {
                        break 'run;
                    }
                }
                (lib.flush_clients)(display);
            }
            (*s).cmd(Cmd::Kill);
            (lib.display_destroy)(display);
            drop(Box::from_raw(s));
        });
        ready_rx.recv().ok()?;
        Some(Server { tx, thread: Some(thread), stats })
    }

    pub fn send(&self, c: Cmd) {
        self.tx.send(c).unwrap();
    }

    /// Everything sent so far is on the wire.
    pub fn sync(&self) {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Sync(tx));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    pub fn keys(&self, evs: &[(u32, bool)]) {
        for &(c, d) in evs {
            self.send(Cmd::Key(c, d));
        }
        self.sync();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- The fake game -------------------------------------------------------------------------

struct Clib {
    connect_to_fd: unsafe extern "C" fn(c_int) -> *mut c_void,
    disconnect: unsafe extern "C" fn(*mut c_void),
    roundtrip: unsafe extern "C" fn(*mut c_void) -> c_int,
    dispatch: unsafe extern "C" fn(*mut c_void) -> c_int,
    add_listener: unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> c_int,
    marshal_constructor: unsafe extern "C" fn(*mut c_void, u32, *const c_void, ...) -> *mut c_void,
    marshal_constructor_versioned: unsafe extern "C" fn(*mut c_void, u32, *const c_void, u32, ...) -> *mut c_void,
    registry_iface: *const c_void,
    compositor_iface: *const c_void,
    surface_iface: *const c_void,
    seat_iface: *const c_void,
    keyboard_iface: *const c_void,
}

unsafe impl Send for Clib {}
unsafe impl Sync for Clib {}

fn clib() -> Option<&'static Clib> {
    static L: std::sync::OnceLock<Option<Clib>> = std::sync::OnceLock::new();
    L.get_or_init(|| unsafe {
        let h = open(c"libwayland-client.so.0")?;
        Some(Clib {
            connect_to_fd: sym(h, c"wl_display_connect_to_fd"),
            disconnect: sym(h, c"wl_display_disconnect"),
            roundtrip: sym(h, c"wl_display_roundtrip"),
            dispatch: sym(h, c"wl_display_dispatch"),
            add_listener: sym(h, c"wl_proxy_add_listener"),
            marshal_constructor: sym(h, c"wl_proxy_marshal_constructor"),
            marshal_constructor_versioned: sym(h, c"wl_proxy_marshal_constructor_versioned"),
            registry_iface: sym(h, c"wl_registry_interface"),
            compositor_iface: sym(h, c"wl_compositor_interface"),
            surface_iface: sym(h, c"wl_surface_interface"),
            seat_iface: sym(h, c"wl_seat_interface"),
            keyboard_iface: sym(h, c"wl_keyboard_interface"),
        })
    })
    .as_ref()
}

/// What the game's own objects saw.
#[derive(Default)]
pub struct GameSeen {
    pub keys: AtomicU32,
    pub enters: AtomicU32,
    pub keymaps: AtomicU32,
    /// Set if one of the game's listeners ran on a thread other than its reader.
    pub wrong_thread: AtomicBool,
    reader: Mutex<Option<std::thread::ThreadId>>,
}

struct GameState {
    lib: &'static Clib,
    compositor: *mut c_void,
    seat: *mut c_void,
    keyboard: *mut c_void,
    seen: Arc<GameSeen>,
}

#[repr(C)]
struct RegistryListener {
    global: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const c_char, u32),
    global_remove: unsafe extern "C" fn(*mut c_void, *mut c_void, u32),
}
#[repr(C)]
struct SeatListener {
    capabilities: unsafe extern "C" fn(*mut c_void, *mut c_void, u32),
    name: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char),
}
#[repr(C)]
struct KeyboardListener {
    keymap: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, i32, u32),
    enter: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *mut c_void),
    leave: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void),
    key: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32, u32, u32),
    modifiers: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32, u32, u32, u32),
    repeat_info: unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32),
}

unsafe fn game<'a>(d: *mut c_void) -> &'a mut GameState {
    unsafe { &mut *(d as *mut GameState) }
}

fn check_thread(seen: &GameSeen) {
    if let Some(r) = *seen.reader.lock().unwrap()
        && r != std::thread::current().id()
    {
        seen.wrong_thread.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn g_global(d: *mut c_void, reg: *mut c_void, name: u32, iface: *const c_char, version: u32) {
    unsafe {
        let g = game(d);
        let iface = CStr::from_ptr(iface);
        let l = g.lib;
        if iface == c"wl_compositor" {
            let v = version.min(4);
            g.compositor = (l.marshal_constructor_versioned)(reg, 0, l.compositor_iface, v, name, c"wl_compositor".as_ptr(), v, std::ptr::null::<c_void>());
        } else if iface == c"wl_seat" {
            let v = version.min(7);
            g.seat = (l.marshal_constructor_versioned)(reg, 0, l.seat_iface, v, name, c"wl_seat".as_ptr(), v, std::ptr::null::<c_void>());
            (l.add_listener)(g.seat, (&G_SEAT as *const SeatListener).cast(), d);
        }
    }
}
unsafe extern "C" fn g_global_remove(_: *mut c_void, _: *mut c_void, _: u32) {}
unsafe extern "C" fn g_caps(d: *mut c_void, seat: *mut c_void, caps: u32) {
    unsafe {
        let g = game(d);
        if caps & 2 != 0 && g.keyboard.is_null() {
            g.keyboard = (g.lib.marshal_constructor)(seat, 1, g.lib.keyboard_iface, std::ptr::null::<c_void>());
            (g.lib.add_listener)(g.keyboard, (&G_KEYBOARD as *const KeyboardListener).cast(), d);
        }
    }
}
unsafe extern "C" fn g_seat_name(_: *mut c_void, _: *mut c_void, _: *const c_char) {}
unsafe extern "C" fn g_keymap(d: *mut c_void, _: *mut c_void, _: u32, fd: i32, _: u32) {
    unsafe {
        let g = game(d);
        check_thread(&g.seen);
        g.seen.keymaps.fetch_add(1, Ordering::Relaxed);
        libc::close(fd);
    }
}
unsafe extern "C" fn g_enter(d: *mut c_void, _: *mut c_void, _: u32, _: *mut c_void, _: *mut c_void) {
    unsafe {
        let g = game(d);
        check_thread(&g.seen);
        g.seen.enters.fetch_add(1, Ordering::Relaxed);
    }
}
unsafe extern "C" fn g_leave(_: *mut c_void, _: *mut c_void, _: u32, _: *mut c_void) {}
unsafe extern "C" fn g_key(d: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32) {
    unsafe {
        let g = game(d);
        check_thread(&g.seen);
        g.seen.keys.fetch_add(1, Ordering::Relaxed);
    }
}
unsafe extern "C" fn g_mods(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32, _: u32) {}
unsafe extern "C" fn g_repeat(_: *mut c_void, _: *mut c_void, _: i32, _: i32) {}

static G_REGISTRY: RegistryListener = RegistryListener { global: g_global, global_remove: g_global_remove };
static G_SEAT: SeatListener = SeatListener { capabilities: g_caps, name: g_seat_name };
static G_KEYBOARD: KeyboardListener = KeyboardListener { keymap: g_keymap, enter: g_enter, leave: g_leave, key: g_key, modifiers: g_mods, repeat_info: g_repeat };

/// The fake game: its display, a surface, its own seat and keyboard on the default queue,
/// and a reader thread.
pub struct Game {
    lib: &'static Clib,
    pub display: *mut c_void,
    /// The game's `wl_surface` (for a `VkWaylandSurfaceCreateInfoKHR`).
    pub surface: *mut c_void,
    state: *mut GameState,
    reader: Option<std::thread::JoinHandle<()>>,
    pub seen: Arc<GameSeen>,
}

struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}

impl Game {
    pub fn connect(fd: c_int) -> Option<Game> {
        let lib = clib()?;
        unsafe {
            let display = (lib.connect_to_fd)(fd);
            assert!(!display.is_null());
            let seen = Arc::new(GameSeen::default());
            let state = Box::into_raw(Box::new(GameState {
                lib,
                compositor: std::ptr::null_mut(),
                seat: std::ptr::null_mut(),
                keyboard: std::ptr::null_mut(),
                seen: seen.clone(),
            }));
            let registry = (lib.marshal_constructor)(display, 1, lib.registry_iface, std::ptr::null::<c_void>());
            (lib.add_listener)(registry, (&G_REGISTRY as *const RegistryListener).cast(), state.cast());
            // The game's own setup, on its own (default) queue, before its reader starts.
            assert!((lib.roundtrip)(display) >= 0);
            assert!((lib.roundtrip)(display) >= 0);
            assert!(!(*state).compositor.is_null());
            let surface = (lib.marshal_constructor)((*state).compositor, 0, lib.surface_iface, std::ptr::null::<c_void>());
            assert!((lib.roundtrip)(display) >= 0);
            let d = SendPtr(display);
            let s2 = seen.clone();
            let reader = std::thread::spawn(move || {
                let d = d;
                *s2.reader.lock().unwrap() = Some(std::thread::current().id());
                // As Wine's read thread: blocks in poll, reads, dispatches its own queue.
                while (lib.dispatch)(d.0) >= 0 {}
            });
            Some(Game { lib, display, surface, state, reader: Some(reader), seen })
        }
    }

    /// Waits for the reader thread to exit (after the server dropped the client), then
    /// disconnects. The layer must have let go of the display first.
    pub fn disconnect(mut self) {
        if let Some(r) = self.reader.take() {
            r.join().unwrap();
        }
        unsafe {
            (self.lib.disconnect)(self.display);
            drop(Box::from_raw(self.state));
        }
    }
}

pub fn socketpair() -> (c_int, c_int) {
    let mut fds = [0; 2];
    let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
    assert_eq!(r, 0);
    (fds[0], fds[1])
}

/// Waits until `f` holds (polling), up to `secs`.
pub fn wait_for(secs: f64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

/// Open fds in this process that are the test keymap's memfd.
pub fn keymap_fds_open() -> usize {
    let want = format!("/memfd:{} (deleted)", KEYMAP_NAME.to_str().unwrap());
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.flatten().filter(|e| std::fs::read_link(e.path()).is_ok_and(|p| p.to_string_lossy() == want)).count())
        .unwrap_or(0)
}
