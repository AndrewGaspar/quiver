//! Capture in the Sight layer, inside the game (design/REMOTE.md §2.1; the layer's side is
//! `quiver-sight-layer`'s `stream` module, the wire protocol `quiver_stream_proto::capture`).
//!
//! The host listens on a Unix socket ([`LayerServer`]) and names it to the program
//! (`QUIVER_STREAM_CAPTURE_SOCKET`). A game with the layer connects, and at each present the
//! layer converts the swapchain image straight into the encoder's input format (NV12, P010,
//! YUV444 or YUV444P16; HDR10 when the game presents HDR10 or scRGB and the stream allows it)
//! in a ring of three slots of GPU memory it exported, signals an exported timeline
//! semaphore, and sends the slot and value. Here:
//!
//! - **a thread** accepts the layer's connections (only this user's, checked with
//!   `SO_PEERCRED`), says what the stream wants, and imports each ring's memory and semaphore
//!   into CUDA (`cuImportExternalMemory`, `cuImportExternalSemaphore`) once;
//! - **the video thread** takes the newest frame (older ones go straight back to the layer),
//!   waits on the slot's value on the GPU (`cuWaitExternalSemaphoresAsync`), copies the slot
//!   into the encoder's input (one device-to-device copy of the YUV the encoder reads), gives
//!   the slot back, and encodes. The frame's capture time is the game's present.
//!
//! The copy is what's left between the game's frame and the encoder: registering the
//! imported slots as NVENC input resources would remove it, but the encoder owns its inputs
//! ([`crate::nvenc::Encoder::surface`]). Measured below, it costs tens of microseconds.
//!
//! **Falling back to PipeWire.** Until the layer delivers frames (a program without the
//! layer, a game still loading), and again a second after its frames stop, frames come from
//! gamescope's screencast ([`PwCapture`]). The screencast stays connected for the whole stream
//! and is paused while the layer's frames flow: headless gamescope 3.16 sometimes crashed (in
//! the NVIDIA driver, half a second later) when its consumer disconnected mid-stream, in five
//! of seven tries. Each switch makes a new encoder session ([`Next::Changed`]), so the client
//! sees a new [`Control::Configure`](quiver_stream_proto::Control::Configure) and a keyframe.
//!
//! **The cursor** isn't in the game's swapchain, and needs nothing here: the input thread
//! already sends its shape from the stream's X display ([`crate::input::x11`]'s XFIXES
//! watch) as [`Control::Cursor`](quiver_stream_proto::Control::Cursor), and the client draws
//! it, whichever capture the frames come from.
//!
//! **One game at a time.** Several processes may connect (a launcher, the game); the frames
//! used are those of the one that sent the last frame, until it has sent none for half a
//! second.

use std::ffi::{c_int, c_uint, c_void};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use libloading::Library;
use quiver_stream_proto::capture::{self, MAX_MSG, PixelFormat, ToHost, ToLayer, Want};
use quiver_stream_proto::{Chroma, ColorInfo, HdrMetadata, Micros, now_micros};

use crate::capture::{PwCapture, Want as PwWant};
use crate::cuda::{CUdeviceptr, CUresult, CUstream, Cuda};
use crate::nvenc::{InputFormat, Surface};
use crate::stats::Samples;
use crate::video::{Next, Source};

/// The target's environment opts out of layer capture with this set to `pipewire`.
pub const ENV_CAPTURE: &str = "QUIVER_STREAM_CAPTURE";

/// Whether a command target with this environment gets the layer's capture (it falls back to
/// PipeWire by itself when no layer connects).
pub fn wanted(env: &[(String, String)]) -> bool {
    !env.iter().any(|(k, v)| k == ENV_CAPTURE && v == "pipewire")
}

// --- CUDA's external resources ---------------------------------------------------------------

type CUexternalMemory = *mut c_void;
type CUexternalSemaphore = *mut c_void;

const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: c_int = 1;
const CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD: c_int = 9;
const CUDA_EXTERNAL_MEMORY_DEDICATED: c_uint = 1;
const CU_STREAM_NON_BLOCKING: c_uint = 1;
const CUDA_ERROR_NOT_READY: CUresult = 600;
const CU_MEMORYTYPE_DEVICE: c_uint = 2;

#[repr(C)]
struct MemoryHandleDesc {
    ty: c_int,
    /// The handle union (an fd, or two pointers), 8-aligned: the fd is its first 4 bytes.
    handle: [u64; 2],
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

#[repr(C)]
struct BufferDesc {
    offset: u64,
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

#[repr(C)]
struct SemaphoreHandleDesc {
    ty: c_int,
    handle: [u64; 2],
    flags: c_uint,
    reserved: [c_uint; 16],
}

#[repr(C)]
struct WaitParams {
    value: u64,
    nv_sci_sync: u64,
    keyed_mutex_key: u64,
    keyed_mutex_timeout_ms: c_uint,
    /// The keyed-mutex struct's own padding (it holds a u64).
    _pad: c_uint,
    reserved: [c_uint; 10],
    flags: c_uint,
    reserved2: [c_uint; 16],
}

#[repr(C)]
struct Memcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: *const c_void,
    src_device: CUdeviceptr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: *mut c_void,
    dst_device: CUdeviceptr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

/// The driver calls for imported memory and semaphores, and an asynchronous copy.
struct Ext {
    import_memory: unsafe extern "C" fn(*mut CUexternalMemory, *const MemoryHandleDesc) -> CUresult,
    mapped_buffer: unsafe extern "C" fn(*mut CUdeviceptr, CUexternalMemory, *const BufferDesc) -> CUresult,
    destroy_memory: unsafe extern "C" fn(CUexternalMemory) -> CUresult,
    import_semaphore: unsafe extern "C" fn(*mut CUexternalSemaphore, *const SemaphoreHandleDesc) -> CUresult,
    wait_semaphores: unsafe extern "C" fn(*const CUexternalSemaphore, *const WaitParams, c_uint, CUstream) -> CUresult,
    destroy_semaphore: unsafe extern "C" fn(CUexternalSemaphore) -> CUresult,
    stream_create: unsafe extern "C" fn(*mut CUstream, c_uint) -> CUresult,
    stream_query: unsafe extern "C" fn(CUstream) -> CUresult,
    stream_destroy: unsafe extern "C" fn(CUstream) -> CUresult,
    memcpy_2d_async: unsafe extern "C" fn(*const Memcpy2D, CUstream) -> CUresult,
    mem_free: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
    _lib: Library,
}

// SAFETY: plain function pointers into the driver.
unsafe impl Send for Ext {}
unsafe impl Sync for Ext {}

impl Ext {
    fn open() -> Result<Arc<Ext>> {
        // SAFETY: the system's CUDA driver (already loaded by `Cuda::open`).
        let lib = unsafe { Library::new("libcuda.so.1") }.context("open libcuda.so.1")?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the symbol's type is the driver API's, as declared in cuda.h.
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.with_context(|| format!("libcuda has no {}", $name))?
            };
        }
        Ok(Arc::new(Ext {
            import_memory: sym!("cuImportExternalMemory"),
            mapped_buffer: sym!("cuExternalMemoryGetMappedBuffer"),
            destroy_memory: sym!("cuDestroyExternalMemory"),
            import_semaphore: sym!("cuImportExternalSemaphore"),
            wait_semaphores: sym!("cuWaitExternalSemaphoresAsync"),
            destroy_semaphore: sym!("cuDestroyExternalSemaphore"),
            stream_create: sym!("cuStreamCreate"),
            stream_query: sym!("cuStreamQuery"),
            stream_destroy: sym!("cuStreamDestroy_v2"),
            memcpy_2d_async: sym!("cuMemcpy2DAsync_v2"),
            mem_free: sym!("cuMemFree_v2"),
            _lib: lib,
        }))
    }
}

fn check(r: CUresult, what: &str) -> Result<()> {
    if r != 0 {
        bail!("{what}: CUDA error {r}");
    }
    Ok(())
}

/// A ring imported into CUDA: freed (with the context pushed) when the last frame of it is.
struct Imported {
    cuda: Arc<Cuda>,
    ext: Arc<Ext>,
    ring: capture::Ring,
    memory: CUexternalMemory,
    ptr: CUdeviceptr,
    semaphore: CUexternalSemaphore,
}

// SAFETY: driver handles, used with the context pushed.
unsafe impl Send for Imported {}
unsafe impl Sync for Imported {}

impl Imported {
    /// Takes ownership of both fds (the driver keeps them on success; they're closed on
    /// failure). The context must be current.
    fn new(cuda: &Arc<Cuda>, ext: &Arc<Ext>, ring: capture::Ring, memory_fd: OwnedFd, semaphore_fd: OwnedFd) -> Result<Imported> {
        let desc = MemoryHandleDesc {
            ty: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            handle: [memory_fd.as_raw_fd() as u32 as u64, 0],
            size: ring.alloc_bytes,
            flags: if ring.dedicated { CUDA_EXTERNAL_MEMORY_DEDICATED } else { 0 },
            reserved: [0; 16],
        };
        let mut memory = std::ptr::null_mut();
        // SAFETY: a valid descriptor; on success the driver owns the fd.
        check(unsafe { (ext.import_memory)(&mut memory, &desc) }, "cuImportExternalMemory")?;
        // The driver owns the fd now.
        let _ = memory_fd.into_raw_fd();
        let mut imp = Imported { cuda: cuda.clone(), ext: ext.clone(), ring, memory, ptr: 0, semaphore: std::ptr::null_mut() };
        let buf = BufferDesc { offset: 0, size: ring.alloc_bytes, flags: 0, reserved: [0; 16] };
        // SAFETY: the memory just imported; the whole allocation.
        check(unsafe { (ext.mapped_buffer)(&mut imp.ptr, memory, &buf) }, "cuExternalMemoryGetMappedBuffer")?;
        let desc = SemaphoreHandleDesc { ty: CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD, handle: [semaphore_fd.as_raw_fd() as u32 as u64, 0], flags: 0, reserved: [0; 16] };
        // SAFETY: as above.
        check(unsafe { (ext.import_semaphore)(&mut imp.semaphore, &desc) }, "cuImportExternalSemaphore")?;
        let _ = semaphore_fd.into_raw_fd();
        Ok(imp)
    }

    fn slot(&self, slot: u8) -> CUdeviceptr {
        self.ptr + slot as u64 * self.ring.slot_bytes
    }
}

impl Drop for Imported {
    fn drop(&mut self) {
        let Ok(_g) = self.cuda.push() else { return };
        // SAFETY: ours; no wait on the semaphore is pending (frames hold an Arc while theirs
        // is).
        unsafe {
            if self.ptr != 0 {
                (self.ext.mem_free)(self.ptr);
            }
            if !self.semaphore.is_null() {
                (self.ext.destroy_semaphore)(self.semaphore);
            }
            if !self.memory.is_null() {
                (self.ext.destroy_memory)(self.memory);
            }
        }
    }
}

// --- the socket ------------------------------------------------------------------------------

/// The listening socket, removed when it goes.
pub struct LayerServer {
    fd: OwnedFd,
    path: PathBuf,
}

impl LayerServer {
    /// Listen in the stream's folder, or in `$XDG_RUNTIME_DIR` when that path is too long
    /// for a socket. Only this user can connect (the socket is 0600, and peers are checked).
    pub fn bind(dir: &Path) -> Result<LayerServer> {
        static N: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let mut path = dir.join("capture.sock");
        if path.as_os_str().len() >= 100 {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            path = runtime.join(format!("quiver-stream-capture-{}-{}.sock", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        }
        let _ = std::fs::remove_file(&path);
        // SAFETY: plain socket calls on our own fd and a sockaddr_un we fill.
        let fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                bail!("socket: {}", std::io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
            if bytes.len() >= addr.sun_path.len() {
                bail!("{} is too long for a socket", path.display());
            }
            for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
                *d = *s as libc::c_char;
            }
            if libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t) != 0 {
                bail!("bind {}: {}", path.display(), std::io::Error::last_os_error());
            }
            if libc::listen(fd.as_raw_fd(), 8) != 0 {
                bail!("listen: {}", std::io::Error::last_os_error());
            }
            fd
        };
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(LayerServer { fd, path })
    }

    /// The program's environment entry naming the socket.
    pub fn env(&self) -> (String, String) {
        (capture::ENV_SOCKET.into(), self.path.display().to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LayerServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One connected layer.
struct Peer {
    id: u64,
    fd: OwnedFd,
}

impl Peer {
    fn send(&self, m: &ToLayer) -> bool {
        let mut buf = [0u8; MAX_MSG];
        let n = m.encode(&mut buf);
        // SAFETY: our socket and buffer.
        unsafe { libc::send(self.fd.as_raw_fd(), buf.as_ptr().cast(), n, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) == n as isize }
    }
}

/// A frame the layer captured, for the video thread. Dropping it gives the slot back.
struct Arrived {
    peer: Arc<Peer>,
    ring: Arc<Imported>,
    frame: capture::Frame,
    received: Micros,
}

impl Drop for Arrived {
    fn drop(&mut self) {
        self.peer.send(&ToLayer::Release { generation: self.frame.generation, slot: self.frame.slot });
    }
}

struct Shared {
    want: Mutex<Want>,
    peers: Mutex<Vec<Arc<Peer>>>,
    stop: AtomicBool,
}

/// What the stream asks of the layer, from its colour setup.
fn want_of(c: &ColorInfo) -> Want {
    Want { chroma444: c.chroma == Chroma::Yuv444, ten_bit: c.bit_depth > 8, hdr: c.hdr.is_some(), full_range: c.full_range }
}

/// Reads one message and the fds that came with it.
fn receive(fd: RawFd) -> std::io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut buf = [0u8; MAX_MSG];
    let mut control = [0u64; 8];
    // SAFETY: our socket, buffers of the given sizes.
    unsafe {
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = std::mem::size_of_val(&control) as _;
        let n = libc::recvmsg(fd, &mut hdr, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::WouldBlock { Ok(Some((Vec::new(), Vec::new()))) } else { Err(e) };
        }
        if n == 0 {
            return Ok(None);
        }
        let mut fds = Vec::new();
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let p = libc::CMSG_DATA(c).cast::<i32>();
                for i in 0..len / 4 {
                    fds.push(OwnedFd::from_raw_fd(p.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
        Ok(Some((buf[..n as usize].to_vec(), fds)))
    }
}

/// The socket thread (module docs).
fn serve(server: LayerServer, shared: Arc<Shared>, tx: std_mpsc::Sender<Arrived>, cuda: Arc<Cuda>, ext: Arc<Ext>) {
    struct Conn {
        peer: Arc<Peer>,
        ring: Option<Arc<Imported>>,
    }
    let Ok(_g) = cuda.push() else { return };
    let mut conns: Vec<Conn> = Vec::new();
    let mut next_id = 1;
    let uid = unsafe { libc::getuid() };
    while !shared.stop.load(Ordering::Relaxed) {
        let mut fds: Vec<libc::pollfd> = std::iter::once(server.fd.as_raw_fd()).chain(conns.iter().map(|c| c.peer.fd.as_raw_fd())).map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        // SAFETY: our pollfd array.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) } <= 0 {
            continue;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            // SAFETY: accepting on our listener.
            let fd = unsafe { libc::accept4(server.fd.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
            if fd >= 0 {
                // SAFETY: just accepted.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                // SAFETY: our socket and out struct.
                let ok = unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) } == 0;
                if ok && cred.uid == uid {
                    let peer = Arc::new(Peer { id: next_id, fd });
                    next_id += 1;
                    let want = *shared.want.lock().unwrap();
                    peer.send(&ToLayer::Want(want));
                    tracing::info!("layer capture: pid {} connected (peer {}), asking for {want:?}", cred.pid, peer.id);
                    shared.peers.lock().unwrap().push(peer.clone());
                    conns.push(Conn { peer, ring: None });
                } else {
                    tracing::warn!("layer capture: refused a connection from uid {} pid {}", cred.uid, cred.pid);
                }
            }
        }
        let mut gone = Vec::new();
        for (i, c) in conns.iter_mut().enumerate() {
            // A connection accepted just now is read on the next pass.
            if fds.get(i + 1).is_none_or(|p| p.revents == 0) {
                continue;
            }
            loop {
                let (msg, mut passed) = match receive(c.peer.fd.as_raw_fd()) {
                    Ok(Some((m, _))) if m.is_empty() => break,
                    Ok(Some(x)) => x,
                    Ok(None) => {
                        tracing::debug!("layer capture: peer {} closed its end (fd {})", c.peer.id, c.peer.fd.as_raw_fd());
                        gone.push(c.peer.id);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("layer capture: peer {} (fd {}): {e}", c.peer.id, c.peer.fd.as_raw_fd());
                        gone.push(c.peer.id);
                        break;
                    }
                };
                match ToHost::decode(&msg) {
                    Some(ToHost::Hello { pid }) => tracing::debug!("layer capture: hello from pid {pid}"),
                    Some(ToHost::Ring(r)) if passed.len() == 2 => {
                        let sem = passed.pop().unwrap();
                        let mem = passed.pop().unwrap();
                        match Imported::new(&cuda, &ext, r, mem, sem) {
                            Ok(imp) => {
                                tracing::info!("layer capture: ring {} {:?} {}x{}{} from peer {}", r.generation, r.format, r.width, r.height, if r.hdr { " HDR10" } else { "" }, c.peer.id);
                                c.ring = Some(Arc::new(imp));
                            }
                            Err(e) => {
                                tracing::warn!("layer capture: can't import ring {}: {e:#}", r.generation);
                                c.ring = None;
                            }
                        }
                    }
                    Some(ToHost::Frame(f)) => match &c.ring {
                        Some(ring) if ring.ring.generation == f.generation => {
                            let a = Arrived { peer: c.peer.clone(), ring: ring.clone(), frame: f, received: now_micros() };
                            // A closed channel drops (and so releases) it.
                            let _ = tx.send(a);
                        }
                        _ => {
                            c.peer.send(&ToLayer::Release { generation: f.generation, slot: f.slot });
                        }
                    },
                    other => tracing::debug!("layer capture: ignored {other:?} ({} fds)", passed.len()),
                }
            }
        }
        if !gone.is_empty() {
            conns.retain(|c| !gone.contains(&c.peer.id));
            shared.peers.lock().unwrap().retain(|p| !gone.contains(&p.id));
            tracing::info!("layer capture: peer(s) {gone:?} disconnected");
        }
    }
}

// --- the source ------------------------------------------------------------------------------

/// What layer capture measured, for the report.
#[derive(Debug, Default, Clone)]
pub struct LayerTimes {
    /// The game's present → the frame's message here.
    pub message_us: Samples,
    /// The message → the slot converted (GPU wait) and copied into the encoder's input.
    pub ready_us: Samples,
    /// Frames encoded from the layer, and from PipeWire.
    pub layer_frames: u64,
    pub pipewire_frames: u64,
    /// Layer frames skipped for a newer one.
    pub skipped: u64,
    /// Switches between the two.
    pub switches: u64,
}

/// The newest stream's numbers when its capture ended (tests and benchmarks).
pub static LAST: Mutex<Option<LayerTimes>> = Mutex::new(None);

/// The capture source: the layer's frames, or gamescope's screencast until they come.
pub struct LayerCapture {
    ext: Arc<Ext>,
    shared: Arc<Shared>,
    rx: std_mpsc::Receiver<Arrived>,
    thread: Option<std::thread::JoinHandle<()>>,
    stream: CUstream,
    /// gamescope's screencast, for the whole stream: paused while the layer's frames flow.
    pw: PwCapture,
    pw_on: bool,
    /// The picture the encoder was last told about, and its colour (None: the stream's own,
    /// for PipeWire's frames).
    current: Option<(u32, u32, InputFormat)>,
    color: Option<ColorInfo>,
    held: Option<Arrived>,
    /// The peer whose frames are used, and when its last one came.
    active: Option<(u64, Instant)>,
    pub times: Arc<Mutex<LayerTimes>>,
}

// SAFETY: the stream handle is only used on the video thread, with the context pushed.
unsafe impl Send for LayerCapture {}

/// How long after the layer's last frame PipeWire takes over again.
const LAYER_GONE: Duration = Duration::from_secs(1);
/// How long a frame's GPU work may take before the frame is given up.
const GPU_TIMEOUT: Duration = Duration::from_millis(250);
/// Between looks at whether it's done.
const POLL: Duration = Duration::from_micros(100);

fn input_format(f: PixelFormat) -> InputFormat {
    match f {
        PixelFormat::Nv12 => InputFormat::Nv12,
        PixelFormat::P010 => InputFormat::P010,
        PixelFormat::Yuv444 => InputFormat::Yuv444,
        PixelFormat::Yuv444P16 => InputFormat::Yuv444P16,
    }
}

/// HDR10 metadata for a game that set none: BT.2020 primaries, D65, 1000 / 0.005 nits, no
/// content light levels.
pub fn default_hdr() -> HdrMetadata {
    HdrMetadata { primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white_point: [15635, 16450], max_luminance: 10_000_000, min_luminance: 50, max_cll: 0, max_fall: 0 }
}

/// The encoder's colour setup for a frame of `ring`.
pub fn color_of(ring: &capture::Ring, hdr: Option<&HdrMetadata>) -> ColorInfo {
    ColorInfo {
        bit_depth: if ring.format.ten_bit() { 10 } else { 8 },
        chroma: if ring.format.chroma444() { Chroma::Yuv444 } else { Chroma::Yuv420 },
        full_range: ring.full_range,
        hdr: ring.hdr.then(|| hdr.cloned().unwrap_or_else(default_hdr)),
    }
}

impl LayerCapture {
    /// Serve `server` and start on PipeWire's `node` (waiting up to `timeout` for its first
    /// frame, as [`PwCapture::start`]). `color` is what the stream asked for.
    pub fn start(cuda: Arc<Cuda>, server: LayerServer, node: u32, color: &ColorInfo, timeout: Duration) -> Result<LayerCapture> {
        let ext = Ext::open()?;
        let shared = Arc::new(Shared { want: Mutex::new(want_of(color)), peers: Mutex::new(Vec::new()), stop: AtomicBool::new(false) });
        let mut stream = std::ptr::null_mut();
        {
            let _g = cuda.push()?;
            // SAFETY: an out pointer for a new stream.
            check(unsafe { (ext.stream_create)(&mut stream, CU_STREAM_NON_BLOCKING) }, "cuStreamCreate")?;
        }
        let (tx, rx) = std_mpsc::channel();
        let (sh, cu, ex) = (shared.clone(), cuda.clone(), ext.clone());
        tracing::info!("layer capture: listening on {}", server.path().display());
        let thread = std::thread::Builder::new().name("layer-capture".into()).spawn(move || serve(server, sh, tx, cu, ex))?;
        let pw = PwCapture::start(cuda, node, PwWant::Bgrx, timeout)?;
        Ok(LayerCapture { ext, shared, rx, thread: Some(thread), stream, pw, pw_on: true, current: None, color: None, held: None, active: None, times: Arc::new(Mutex::new(LayerTimes::default())) })
    }

    /// The newest frame waiting, the older ones given back; waiting up to `wait` for one.
    fn newest(&mut self, wait: Duration) -> Option<Arrived> {
        let mut got = if wait.is_zero() { self.rx.try_recv().ok() } else { self.rx.recv_timeout(wait).ok() };
        while let Ok(n) = self.rx.try_recv() {
            if got.is_some() {
                self.times.lock().unwrap().skipped += 1;
            }
            got = Some(n);
        }
        got
    }

    fn layer_active(&self) -> bool {
        self.active.is_some_and(|(_, at)| at.elapsed() < LAYER_GONE)
    }

    /// Waits for the slot's conversion and copies it into `surface`. The context is current.
    fn copy(&mut self, a: &Arrived, surface: &mut Surface) -> Result<()> {
        let r = &a.ring.ring;
        let params = WaitParams { value: a.frame.value, nv_sci_sync: 0, keyed_mutex_key: 0, keyed_mutex_timeout_ms: 0, _pad: 0, reserved: [0; 10], flags: 0, reserved2: [0; 16] };
        let cp = Memcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_host: std::ptr::null(),
            src_device: a.ring.slot(a.frame.slot),
            src_array: std::ptr::null_mut(),
            src_pitch: r.pitch as usize,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: std::ptr::null_mut(),
            dst_device: surface.row(0),
            dst_array: std::ptr::null_mut(),
            dst_pitch: surface.pitch(),
            width_in_bytes: (r.width * r.format.sample_bytes()) as usize,
            height: r.format.rows(r.height) as usize,
        };
        // SAFETY: the imported semaphore and slot (alive while `a` is), the surface of this
        // size and format; our stream.
        unsafe {
            check((self.ext.wait_semaphores)(&a.ring.semaphore, &params, 1, self.stream), "cuWaitExternalSemaphoresAsync")?;
            check((self.ext.memcpy_2d_async)(&cp, self.stream), "cuMemcpy2DAsync")?;
        }
        let start = Instant::now();
        loop {
            // SAFETY: our stream.
            match unsafe { (self.ext.stream_query)(self.stream) } {
                0 => return Ok(()),
                CUDA_ERROR_NOT_READY => {}
                e => bail!("cuStreamQuery: CUDA error {e}"),
            }
            let waited = start.elapsed();
            if waited > GPU_TIMEOUT {
                // The wait may never end (the game died mid-frame): leave the stream to it.
                let mut s = std::ptr::null_mut();
                // SAFETY: a new stream replaces the stuck one, which is released when (if) its
                // work ends.
                unsafe {
                    check((self.ext.stream_create)(&mut s, CU_STREAM_NON_BLOCKING), "cuStreamCreate")?;
                    (self.ext.stream_destroy)(self.stream);
                }
                self.stream = s;
                bail!("the layer's frame wasn't ready after {GPU_TIMEOUT:?}");
            }
            // The GPU work is a fraction of a millisecond: spin briefly, then sleep.
            if waited < Duration::from_millis(2) {
                std::thread::yield_now();
            } else {
                std::thread::sleep(POLL);
            }
        }
    }
}

impl Drop for LayerCapture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.held = None;
        while self.rx.try_recv().is_ok() {}
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let t = self.times.lock().unwrap().clone();
        tracing::info!(
            "layer capture: {} frames from the layer, {} from PipeWire, {} skipped, {} switches; present → message {}, message → ready {}",
            t.layer_frames,
            t.pipewire_frames,
            t.skipped,
            t.switches,
            t.message_us.summary(),
            t.ready_us.summary()
        );
        *LAST.lock().unwrap() = Some(t);
        // SAFETY: ours; whatever is queued on it finishes on its own.
        unsafe { (self.ext.stream_destroy)(self.stream) };
    }
}

impl Source for LayerCapture {
    fn format(&self) -> (u32, u32, InputFormat) {
        self.current.unwrap_or_else(|| self.pw.format())
    }

    fn color(&self) -> Option<ColorInfo> {
        self.color.clone()
    }

    fn recolor(&mut self, color: &ColorInfo) {
        let want = want_of(color);
        *self.shared.want.lock().unwrap() = want;
        for p in self.shared.peers.lock().unwrap().iter() {
            p.send(&ToLayer::Want(want));
        }
    }

    fn next(&mut self, cuda: &Arc<Cuda>, surface: &mut Surface, seq: u64, stop: &AtomicBool) -> Result<Next> {
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(Next::Stopped);
            }
            let wait = if self.layer_active() { Duration::from_millis(50) } else { Duration::ZERO };
            let got = match self.held.take() {
                Some(a) => Some(a),
                None => self.newest(wait),
            };
            if let Some(a) = got {
                // Another process's frames, while the one in use still sends: back at once.
                if self.active.is_some_and(|(id, at)| id != a.peer.id && at.elapsed() < Duration::from_millis(500)) {
                    continue;
                }
                self.active = Some((a.peer.id, Instant::now()));
                if self.pw_on {
                    self.pw.set_active(false);
                    self.pw_on = false;
                    tracing::info!("layer capture: frames from the layer; PipeWire paused");
                    self.times.lock().unwrap().switches += 1;
                }
                let fmt = (a.ring.ring.width, a.ring.ring.height, input_format(a.ring.ring.format));
                let color = color_of(&a.ring.ring, a.frame.hdr.as_ref());
                if self.current != Some(fmt) || (surface.width, surface.height, surface.format) != fmt || self.color.as_ref() != Some(&color) {
                    self.current = Some(fmt);
                    self.color = Some(color);
                    self.held = Some(a);
                    return Ok(Next::Changed { width: fmt.0, height: fmt.1, format: fmt.2 });
                }
                match self.copy(&a, surface) {
                    Ok(()) => {
                        let ready = now_micros();
                        let mut t = self.times.lock().unwrap();
                        t.message_us.push(a.received.saturating_sub(a.frame.presented) as u32);
                        t.ready_us.push(ready.saturating_sub(a.received) as u32);
                        t.layer_frames += 1;
                        return Ok(Next::Frame { captured: a.frame.presented });
                    }
                    Err(e) => {
                        tracing::warn!("layer capture: {e:#}");
                        continue;
                    }
                }
            }
            if self.layer_active() {
                // The layer's last frame was under a second ago and nothing new came: a still
                // picture (a paused game presents nothing).
                return Ok(Next::Idle);
            }
            // PipeWire, until the layer's frames come (again).
            if !self.pw_on {
                self.active = None;
                self.pw.set_active(true);
                self.pw_on = true;
                tracing::info!("layer capture: no frames from the layer for {LAYER_GONE:?}; back to PipeWire");
                self.times.lock().unwrap().switches += 1;
            }
            match self.pw.next(cuda, surface, seq, stop)? {
                Next::Changed { width, height, format } => {
                    self.current = Some((width, height, format));
                    self.color = None;
                    return Ok(Next::Changed { width, height, format });
                }
                Next::Frame { captured } => {
                    self.times.lock().unwrap().pipewire_frames += 1;
                    return Ok(Next::Frame { captured });
                }
                Next::Idle => return Ok(Next::Idle),
                Next::Stopped => return Ok(Next::Stopped),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_cuda_h() {
        // sizeof and offsetof from CUDA 13's cuda.h.
        assert_eq!(std::mem::size_of::<MemoryHandleDesc>(), 104);
        assert_eq!(std::mem::offset_of!(MemoryHandleDesc, handle), 8);
        assert_eq!(std::mem::offset_of!(MemoryHandleDesc, size), 24);
        assert_eq!(std::mem::offset_of!(SemaphoreHandleDesc, handle), 8);
        assert_eq!(std::mem::size_of::<BufferDesc>(), 88);
        assert_eq!(std::mem::size_of::<SemaphoreHandleDesc>(), 96);
        assert_eq!(std::mem::size_of::<WaitParams>(), 144);
        assert_eq!(std::mem::offset_of!(WaitParams, flags), 72);
    }

    #[test]
    fn colour_and_wants() {
        let ring = capture::Ring { generation: 1, slots: 3, format: PixelFormat::P010, width: 2560, height: 1440, pitch: 5120, slot_bytes: 5120 * 2160, alloc_bytes: 5120 * 2160 * 3, dedicated: true, hdr: true, full_range: false };
        let c = color_of(&ring, None);
        assert_eq!((c.bit_depth, c.chroma, c.full_range), (10, Chroma::Yuv420, false));
        assert_eq!(c.hdr, Some(default_hdr()));
        let sdr = capture::Ring { format: PixelFormat::Yuv444, hdr: false, full_range: true, ..ring };
        let c = color_of(&sdr, Some(&default_hdr()));
        assert_eq!((c.bit_depth, c.chroma, c.full_range, c.hdr), (8, Chroma::Yuv444, true, None));
        let mut want = ColorInfo::sdr();
        assert_eq!(want_of(&want), Want::default());
        want.hdr = Some(default_hdr());
        want.bit_depth = 10;
        assert_eq!(want_of(&want), Want { hdr: true, ten_bit: true, ..Default::default() });
        assert!(wanted(&[]));
        assert!(!wanted(&[(ENV_CAPTURE.into(), "pipewire".into())]));
    }

    #[test]
    fn the_socket_is_private_and_goes_away() {
        let t = tempfile::tempdir().unwrap();
        let s = LayerServer::bind(t.path()).unwrap();
        let path = s.path().to_owned();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(s.env().0, capture::ENV_SOCKET);
        drop(s);
        assert!(!path.exists());
        // A folder too deep for a socket path: the runtime dir instead.
        let deep = t.path().join("x".repeat(120));
        let s = LayerServer::bind(&deep).unwrap();
        assert!(s.path().as_os_str().len() < 100, "{}", s.path().display());
    }
}
