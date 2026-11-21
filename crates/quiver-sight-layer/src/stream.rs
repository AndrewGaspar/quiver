//! Capture for remote play (design/REMOTE.md §2.1): the game's presented frames go to
//! `quiver-stream-host` already converted to the encoder's input, in GPU memory the host
//! imports into CUDA, at present time. The wire protocol is `quiver_stream_proto::capture`.
//!
//! **Inert unless a stream is active.** Everything here is off unless the game's environment
//! names the host's socket ([`ENV_SOCKET`], which only `quiver-stream-host` sets). Without it
//! no device or swapchain is changed, no thread starts, and a present pays one relaxed atomic
//! load. With it:
//!
//! - **Device creation** adds `VK_KHR_external_memory_fd` and `VK_KHR_external_semaphore_fd`
//!   and turns timeline semaphores on, when the instance and device are Vulkan 1.2 and the app
//!   hasn't turned timeline semaphores off itself. If that device can't be made, it is made
//!   as the app asked, without capture.
//! - **Swapchain creation** adds `SAMPLED` usage (when the surface allows it) and makes the
//!   conversion pipeline, one image view and one binary semaphore per image.
//! - **A thread** connects to the socket, says hello, and reads what the host wants and which
//!   slots it gives back. A lost connection is retried every second.
//!
//! **Per present on the HUD's target swapchain while the host is connected:** pick a slot the
//! host has released and the GPU is done with (none: the present isn't captured, the game
//! never waits); one compute dispatch (`shaders/convert.comp`) reads the presented image
//! (after the HUD and the Sight panel, so they're in the stream) and writes the slot as NV12,
//! P010, YUV444 or YUV444P16; the submit waits on the app's present semaphores (or the
//! compositor's), signals the image's binary semaphore, which the present then waits on, and
//! the exported timeline semaphore; then a non-blocking `send` of the frame's message. A
//! message the socket can't take now frees the slot again. The ring (three slots in one
//! exportable allocation) is made, and its fds sent, the first time and whenever the
//! connection, the host's wishes or the swapchain change: a one-time cost of a few
//! milliseconds at the start of a stream, never per present.
//!
//! **Colour.** The swapchain's colour space says what its values mean: SDR (`SRGB_NONLINEAR`;
//! an `_SRGB` format is read through its own view, which decodes, and encoded again), HDR10
//! (`HDR10_ST2084`: PQ, BT.2020) or scRGB (`EXTENDED_SRGB_LINEAR`: linear BT.709, 1.0 = 80
//! nits). When the host allows HDR and the game presents HDR10 or scRGB, the ring is 10-bit
//! PQ BT.2020 with the game's `vkSetHdrMetadataEXT` metadata on each frame; otherwise it is
//! BT.709 SDR, an HDR game tone-mapped by clipping at 203-nit SDR white. Limited or full range
//! as the host asks.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ash::vk;
use quiver_stream_proto::capture::{self, MAX_MSG, PixelFormat, SLOTS, ToHost, ToLayer, Want};

use crate::ffi::find_in_chain;
use crate::state::{DeviceData, InstanceData};
use crate::{PANICKED, log};

pub use quiver_stream_proto::capture::ENV_SOCKET;

const CONVERT: &[u8] = include_bytes!("../shaders/convert.comp.spv");

/// The socket's path, when a stream names one.
fn socket_path() -> Option<&'static CStr> {
    static P: OnceLock<Option<CString>> = OnceLock::new();
    P.get_or_init(|| {
        let v = std::env::var_os(ENV_SOCKET).filter(|v| !v.is_empty())?;
        // gamescope is a Vulkan program too, and its children's environment is its own: the
        // stream's display is never captured.
        if std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("gamescope"))).unwrap_or(false) {
            return None;
        }
        CString::new(std::os::unix::ffi::OsStrExt::as_bytes(v.as_os_str())).ok()
    })
    .as_deref()
}

/// A stream names a socket: devices and swapchains are set up for capture.
pub fn configured() -> bool {
    socket_path().is_some()
}

/// The host is connected and wants frames.
#[inline]
pub fn live() -> bool {
    LINK.live.load(Ordering::Relaxed) && !PANICKED.load(Ordering::Relaxed)
}

// --- the connection ------------------------------------------------------------------------

/// What the connection thread and the present path share.
struct Link {
    live: AtomicBool,
    /// The socket (-1: none), and its device and inode: every send checks the number is still
    /// that socket ([`ours`]).
    fd: AtomicI32,
    dev: AtomicU64,
    ino: AtomicU64,
    /// Bumped on every connection (the host then has no ring).
    epoch: AtomicU32,
    /// The host's [`Want`], packed.
    want: AtomicU32,
    /// The ring whose slots `free` describes.
    generation: AtomicU32,
    /// Slots the host doesn't hold.
    free: [AtomicBool; SLOTS],
}

static LINK: Link = Link {
    live: AtomicBool::new(false),
    fd: AtomicI32::new(-1),
    dev: AtomicU64::new(0),
    ino: AtomicU64::new(0),
    epoch: AtomicU32::new(0),
    want: AtomicU32::new(0),
    generation: AtomicU32::new(0),
    free: [const { AtomicBool::new(true) }; SLOTS],
};

static GENERATION: AtomicU32 = AtomicU32::new(1);
/// Captured and dropped presents, for the log.
static CAPTURED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Of those, presents that found every slot held by the host, or a free one the GPU wasn't
/// done with.
static HELD: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicU64 = AtomicU64::new(0);

fn pack(w: Want) -> u32 {
    w.chroma444 as u32 | (w.ten_bit as u32) << 1 | (w.hdr as u32) << 2 | (w.full_range as u32) << 3
}

fn unpack(v: u32) -> Want {
    Want { chroma444: v & 1 != 0, ten_bit: v & 2 != 0, hdr: v & 4 != 0, full_range: v & 8 != 0 }
}

/// Starts the connection thread, once, when a stream names a socket.
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    let Some(path) = socket_path() else { return };
    STARTED.get_or_init(|| {
        let path = path.to_owned();
        if let Err(e) = std::thread::Builder::new().name("quiver-stream".into()).stack_size(128 << 10).spawn(move || connection(path)) {
            log!("stream capture: no connection thread: {e}");
        }
    });
}

/// The socket's identity (device, inode): code elsewhere in the game may close a number twice,
/// and the number is then someone else's.
fn identity(fd: i32) -> Option<(u64, u64)> {
    // SAFETY: a stat of an fd number into our own struct.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        (libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK).then_some((st.st_dev as u64, st.st_ino as u64))
    }
}

/// `fd` is still the socket we made.
fn ours(fd: i32) -> bool {
    fd >= 0 && identity(fd).is_some_and(|(dev, ino)| dev == LINK.dev.load(Ordering::Relaxed) && ino == LINK.ino.load(Ordering::Relaxed))
}

/// What fd `fd` is now, for the log.
fn what_is(fd: i32) -> String {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).map_or_else(|_| "closed".to_owned(), |p| p.display().to_string())
}

fn connect(path: &CStr) -> Option<i32> {
    // SAFETY: plain socket calls on our own fd and a sockaddr_un we fill.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.to_bytes();
        if bytes.len() >= addr.sun_path.len() {
            libc::close(fd);
            return None;
        }
        for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
            *d = *s as c_char;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len) != 0 {
            libc::close(fd);
            return None;
        }
        // Out of the low numbers the game's own code opens and closes all the time: a stray
        // double close there can't hit ours.
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // `rlim_t` is 32 bits in the i686 layer.
        #[allow(clippy::unnecessary_cast)]
        let base = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 { (lim.rlim_cur as u64).saturating_sub(64).clamp(16, 1000) as i32 } else { 16 };
        let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, base);
        if high >= 0 {
            libc::close(fd);
            Some(high)
        } else {
            Some(fd)
        }
    }
}

/// Why a connection ended.
enum End {
    /// The host closed it, or it failed.
    Closed,
    /// The fd number stopped being our socket.
    Stolen,
}

/// The host's messages until the connection ends. Polls, so the thread notices within a
/// quarter second when the number is no longer ours.
fn serve(fd: i32) -> End {
    let mut buf = [0u8; MAX_MSG];
    loop {
        if !ours(fd) {
            return End::Stolen;
        }
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd of ours.
        if unsafe { libc::poll(&mut p, 1, 250) } <= 0 || p.revents == 0 {
            continue;
        }
        if !ours(fd) {
            return End::Stolen;
        }
        // SAFETY: our socket and buffer.
        let got = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if got == 0 {
            return End::Closed;
        }
        if got < 0 {
            let e = std::io::Error::last_os_error();
            if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) {
                continue;
            }
            log!("stream capture: the connection failed: {e}");
            return End::Closed;
        }
        match ToLayer::decode(&buf[..got as usize]) {
            Some(ToLayer::Want(w)) => {
                LINK.want.store(pack(w), Ordering::Relaxed);
                LINK.epoch.fetch_add(1, Ordering::Release);
                LINK.live.store(true, Ordering::Release);
                log!("stream capture: the host wants {w:?}");
            }
            Some(ToLayer::Release { generation, slot }) => {
                if generation == LINK.generation.load(Ordering::Acquire) {
                    LINK.free[slot as usize].store(true, Ordering::Release);
                }
            }
            None => log!("stream capture: a message we don't understand ({got} bytes)"),
        }
    }
}

/// The connection thread: connect, hello, the host's messages until the connection ends, and
/// again a second later.
fn connection(path: CString) {
    let mut logged = false;
    // Ended connections, shut down but not closed until no present can still be sending on
    // their number.
    let mut ended: Vec<i32> = Vec::new();
    loop {
        let Some(fd) = connect(&path) else {
            if !logged {
                log!("stream capture: can't connect to {path:?} yet");
                logged = true;
            }
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let Some((dev, ino)) = identity(fd) else {
            // SAFETY: just made.
            unsafe { libc::close(fd) };
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        LINK.dev.store(dev, Ordering::Relaxed);
        LINK.ino.store(ino, Ordering::Relaxed);
        LINK.fd.store(fd, Ordering::Release);
        for old in ended.drain(..) {
            // SAFETY: an fd of ours (it was shut down a second ago and nothing sends on it).
            unsafe { libc::close(old) };
        }
        log!("stream capture: connected to {path:?} (fd {fd})");
        let mut buf = [0u8; MAX_MSG];
        let n = ToHost::Hello { pid: std::process::id() }.encode(&mut buf);
        // SAFETY: our socket and buffer.
        let end = if unsafe { libc::send(fd, buf.as_ptr().cast(), n, libc::MSG_NOSIGNAL) } == n as isize { serve(fd) } else { End::Closed };
        LINK.live.store(false, Ordering::Relaxed);
        LINK.fd.store(-1, Ordering::Release);
        match end {
            End::Closed => {
                // SAFETY: still ours: shut down now, closed at the next connection.
                unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
                ended.push(fd);
            }
            // Never touched again: the number is someone else's.
            End::Stolen => log!("stream capture: other code in this process closed our socket (fd {fd}, now {})", what_is(fd)),
        }
        log!(
            "stream capture: disconnected ({} presents captured, {} dropped so far: {} with every slot held, {} with the GPU behind)",
            CAPTURED.load(Ordering::Relaxed),
            DROPPED.load(Ordering::Relaxed),
            HELD.load(Ordering::Relaxed),
            BUSY.load(Ordering::Relaxed)
        );
        logged = false;
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Sends `msg` without waiting, with `fds` attached when there are any; never on a number that
/// isn't our socket any more.
fn send(msg: &ToHost, fds: &[i32]) -> bool {
    let fd = LINK.fd.load(Ordering::Acquire);
    if !ours(fd) {
        return false;
    }
    let mut buf = [0u8; MAX_MSG];
    let n = msg.encode(&mut buf);
    // SAFETY: our socket, buffer and control buffer (aligned, big enough for two fds).
    unsafe {
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: n };
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        let mut control = [0u64; 8];
        if !fds.is_empty() {
            let len = std::mem::size_of_val(fds) as u32;
            hdr.msg_control = control.as_mut_ptr().cast();
            hdr.msg_controllen = libc::CMSG_SPACE(len) as _;
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(len) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<i32>(), fds.len());
        }
        libc::sendmsg(fd, &hdr, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) == n as isize
    }
}

// --- the device ------------------------------------------------------------------------------

/// The functions capture needs, from a device made with its extensions.
pub struct DeviceSupport {
    get_memory_fd: vk::PFN_vkGetMemoryFdKHR,
    get_semaphore_fd: vk::PFN_vkGetSemaphoreFdKHR,
    counter_value: vk::PFN_vkGetSemaphoreCounterValue,
    wait_semaphores: vk::PFN_vkWaitSemaphores,
}

impl DeviceSupport {
    /// # Safety
    /// `load` gives the next layer's device functions.
    pub unsafe fn load(load: impl Fn(&CStr) -> *const c_void) -> Option<DeviceSupport> {
        let (m, s, c, w) = (load(c"vkGetMemoryFdKHR"), load(c"vkGetSemaphoreFdKHR"), load(c"vkGetSemaphoreCounterValue"), load(c"vkWaitSemaphores"));
        if m.is_null() || s.is_null() || c.is_null() || w.is_null() {
            return None;
        }
        // SAFETY: non-null pointers to these functions.
        unsafe {
            Some(DeviceSupport {
                get_memory_fd: std::mem::transmute::<*const c_void, vk::PFN_vkGetMemoryFdKHR>(m),
                get_semaphore_fd: std::mem::transmute::<*const c_void, vk::PFN_vkGetSemaphoreFdKHR>(s),
                counter_value: std::mem::transmute::<*const c_void, vk::PFN_vkGetSemaphoreCounterValue>(c),
                wait_semaphores: std::mem::transmute::<*const c_void, vk::PFN_vkWaitSemaphores>(w),
            })
        }
    }
}

const NEED: [&CStr; 2] = [c"VK_KHR_external_memory_fd", c"VK_KHR_external_semaphore_fd"];

/// The app's device create info with capture's extensions and timeline semaphores added.
pub struct DevicePatch {
    info: vk::DeviceCreateInfo<'static>,
    _exts: Vec<*const c_char>,
    _timeline: Option<Box<vk::PhysicalDeviceTimelineSemaphoreFeatures<'static>>>,
}

impl DevicePatch {
    pub fn info(&self) -> *const vk::DeviceCreateInfo<'static> {
        &self.info
    }

    /// `None`: this device can't capture (too old, missing extensions, timeline semaphores
    /// turned off by the app); it is made as the app asked.
    ///
    /// # Safety
    /// `ci` is the app's valid create info for `physical`, which `inst` owns.
    pub unsafe fn new(inst: &InstanceData, physical: vk::PhysicalDevice, ci: *const vk::DeviceCreateInfo<'_>) -> Option<DevicePatch> {
        // SAFETY: as documented.
        unsafe {
            let props = inst.fns.get_physical_device_properties(physical);
            if inst.api_version < vk::API_VERSION_1_2 || props.api_version < vk::API_VERSION_1_2 {
                log!("stream capture: needs Vulkan 1.2 (instance {:#x}, device {:#x})", inst.api_version, props.api_version);
                return None;
            }
            let have = inst.fns.enumerate_device_extension_properties(physical).ok()?;
            if let Some(n) = NEED.iter().find(|n| !have.iter().any(|e| e.extension_name_as_c_str().ok() == Some(**n))) {
                log!("stream capture: the device has no {n:?}");
                return None;
            }
            let ci = &*ci;
            let mut exts: Vec<*const c_char> = if ci.pp_enabled_extension_names.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(ci.pp_enabled_extension_names, ci.enabled_extension_count as usize).to_vec()
            };
            for n in NEED {
                if !exts.iter().any(|&e| crate::ffi::c_str(e) == Some(n)) {
                    exts.push(n.as_ptr());
                }
            }
            let v12 = find_in_chain(ci.p_next, vk::StructureType::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, |_| true) as *const vk::PhysicalDeviceVulkan12Features<'_>;
            let tl = find_in_chain(ci.p_next, vk::StructureType::PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES, |_| true) as *const vk::PhysicalDeviceTimelineSemaphoreFeatures<'_>;
            let mut info = std::mem::transmute::<vk::DeviceCreateInfo<'_>, vk::DeviceCreateInfo<'static>>(*ci);
            let mut timeline = None;
            if !v12.is_null() || !tl.is_null() {
                let on = (!v12.is_null() && (*v12).timeline_semaphore != 0) || (!tl.is_null() && (*tl).timeline_semaphore != 0);
                if !on {
                    log!("stream capture: the app turned timeline semaphores off");
                    return None;
                }
            } else {
                // Ours goes first in the chain; the app's structs follow unchanged.
                let mut t = Box::new(vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true));
                t.p_next = info.p_next as *mut c_void;
                info.p_next = (&*t as *const vk::PhysicalDeviceTimelineSemaphoreFeatures<'_>).cast();
                timeline = Some(t);
            }
            info.enabled_extension_count = exts.len() as u32;
            info.pp_enabled_extension_names = exts.as_ptr();
            Some(DevicePatch { info, _exts: exts, _timeline: timeline })
        }
    }
}

/// The swapchain can be captured: a stream is configured, the device has capture, and the
/// image can be sampled. Its images then get `SAMPLED` usage.
pub fn wants_swapchain(d: &DeviceData, ci: &vk::SwapchainCreateInfoKHR<'_>, features: vk::FormatFeatureFlags, usage: Option<vk::ImageUsageFlags>) -> bool {
    configured()
        && d.stream.is_some()
        && crate::formats::drawable(ci).is_ok()
        && ci.image_extent.width >= 16
        && ci.image_extent.height >= 16
        && features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        && usage.is_some_and(|u| u.contains(vk::ImageUsageFlags::SAMPLED))
}

// --- the per-swapchain capture ---------------------------------------------------------------

/// How the shader reads the swapchain's values (`convert.comp`'s `IN_*`).
fn input_kind(format: vk::Format, cs: vk::ColorSpaceKHR) -> u32 {
    match cs {
        vk::ColorSpaceKHR::HDR10_ST2084_EXT => 2,
        vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT => 3,
        _ if is_srgb(format) => 1,
        _ => 0,
    }
}

fn is_srgb(f: vk::Format) -> bool {
    matches!(f, vk::Format::B8G8R8A8_SRGB | vk::Format::R8G8B8A8_SRGB | vk::Format::A8B8G8R8_SRGB_PACK32)
}

/// What a ring holds for a swapchain and what the host wants: (format, HDR out, the shader's
/// mode and flags).
pub fn choose(want: Want, format: vk::Format, cs: vk::ColorSpaceKHR) -> (PixelFormat, bool, u32, u32) {
    let kind = input_kind(format, cs);
    let hdr = want.hdr && kind >= 2;
    let fmt = PixelFormat::of(want.chroma444, hdr || want.ten_bit);
    let mode = match fmt {
        PixelFormat::Nv12 => 0,
        PixelFormat::P010 => 1,
        PixelFormat::Yuv444 => 2,
        PixelFormat::Yuv444P16 => 3,
    };
    (fmt, hdr, mode, kind | (hdr as u32) << 2 | (want.full_range as u32) << 3)
}

/// Pixels each invocation writes across and down.
fn block(mode: u32) -> (u32, u32) {
    match mode {
        0 => (4, 2),
        1 => (2, 2),
        2 => (4, 1),
        _ => (2, 1),
    }
}

/// `convert.comp`'s push constants.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Push {
    size: [u32; 2],
    in_size: [u32; 2],
    pitch: u32,
    base: u32,
    mode: u32,
    flags: u32,
}

/// Three slots in one exported allocation.
struct Ring {
    generation: u32,
    epoch: u32,
    want: u32,
    format: PixelFormat,
    hdr: bool,
    mode: u32,
    flags: u32,
    width: u32,
    height: u32,
    pitch: u32,
    slot_bytes: u64,
    alloc_bytes: u64,
    dedicated: bool,
    full_range: bool,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    /// The last value a submit writing it signals.
    last: u64,
    /// When it was replaced.
    retired: Option<Instant>,
}

pub struct Capture {
    handle: vk::SwapchainKHR,
    format: vk::Format,
    color_space: vk::ColorSpaceKHR,
    extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    images: Vec<vk::Image>,
    /// Signalled by our submit for image `i`, waited on by its present (as the compositor's).
    semaphores: Vec<vk::Semaphore>,
    timeline: vk::Semaphore,
    value: u64,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: Option<(u32, vk::CommandPool)>,
    cmds: [vk::CommandBuffer; SLOTS],
    /// The value of each command buffer's last submit.
    cmd_value: [u64; SLOTS],
    ring: Option<Ring>,
    retired: Vec<Ring>,
    next: usize,
    /// A ring announcement the socket couldn't take: not again before this.
    retry_at: Option<Instant>,
    seq: u64,
    /// The swapchain's captures failed for good.
    pub failed: bool,
}

// SAFETY: only used under its swapchain's mutex.
unsafe impl Send for Capture {}

impl Capture {
    /// # Safety
    /// `images` are the swapchain's (with `SAMPLED` usage); `d` is its live device.
    pub unsafe fn new(d: &DeviceData, handle: vk::SwapchainKHR, images: &[vk::Image], format: vk::Format, color_space: vk::ColorSpaceKHR, extent: vk::Extent2D) -> Result<Capture, vk::Result> {
        let mut c = Capture {
            handle,
            format,
            color_space,
            extent,
            views: Vec::new(),
            images: images.to_vec(),
            semaphores: Vec::new(),
            timeline: vk::Semaphore::null(),
            value: 0,
            sampler: vk::Sampler::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            pool: None,
            cmds: [vk::CommandBuffer::null(); SLOTS],
            cmd_value: [0; SLOTS],
            ring: None,
            retired: Vec::new(),
            next: 0,
            retry_at: None,
            seq: 0,
            failed: false,
        };
        // SAFETY: as documented; on error what was made is destroyed (nothing in flight).
        match unsafe { c.build(d) } {
            Ok(()) => Ok(c),
            Err(e) => {
                unsafe { c.destroy_now(d) };
                Err(e)
            }
        }
    }

    unsafe fn build(&mut self, d: &DeviceData) -> Result<(), vk::Result> {
        let dev = &d.dev;
        // SAFETY: valid create infos; handles kept for destroy.
        unsafe {
            for &img in &self.images {
                self.views.push(dev.create_image_view(
                    &vk::ImageViewCreateInfo::default().image(img).view_type(vk::ImageViewType::TYPE_2D).format(self.format).subresource_range(crate::compositor::color_range()),
                    None,
                )?);
                self.semaphores.push(dev.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?);
            }
            let mut ty = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(0);
            let mut export = vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
            self.timeline = dev.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ty).push_next(&mut export), None)?;
            self.sampler = dev.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?;
            let samplers = [self.sampler];
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
                    .immutable_samplers(&samplers),
                vk::DescriptorSetLayoutBinding::default().binding(1).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(1).stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            self.set_layout = dev.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None)?;
            let layouts = [self.set_layout];
            let ranges = [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::COMPUTE, offset: 0, size: std::mem::size_of::<Push>() as u32 }];
            self.layout = dev.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts).push_constant_ranges(&ranges), None)?;
            let words: Vec<u32> = CONVERT.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            let module = dev.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)?;
            let stage = vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(module).name(c"main");
            let made = dev.create_compute_pipelines(vk::PipelineCache::null(), &[vk::ComputePipelineCreateInfo::default().stage(stage).layout(self.layout)], None);
            dev.destroy_shader_module(module, None);
            self.pipeline = made.map_err(|(_, e)| e)?[0];
        }
        Ok(())
    }

    fn counter(&self, d: &DeviceData) -> Option<u64> {
        let s = d.stream.as_ref()?;
        let mut v = 0;
        // SAFETY: our timeline semaphore.
        (unsafe { (s.counter_value)(d.handle, self.timeline, &mut v) } == vk::Result::SUCCESS).then_some(v)
    }

    /// The command pool for `family` (made on the first capture); `None` while the old
    /// family's work is in flight.
    unsafe fn ensure_pool(&mut self, d: &DeviceData, family: u32, done: u64) -> Result<Option<()>, vk::Result> {
        let dev = &d.dev;
        if let Some((f, pool)) = self.pool {
            if f == family {
                return Ok(Some(()));
            }
            if self.cmd_value.iter().any(|&v| v > done) {
                return Ok(None);
            }
            // SAFETY: nothing of it in flight.
            unsafe { dev.destroy_command_pool(pool, None) };
            self.pool = None;
        }
        // SAFETY: valid create info; command buffers get the loader's dispatch pointer.
        unsafe {
            let pool = dev.create_command_pool(&vk::CommandPoolCreateInfo::default().flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER).queue_family_index(family), None)?;
            let cmds = match dev.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(SLOTS as u32)) {
                Ok(c) => c,
                Err(e) => {
                    dev.destroy_command_pool(pool, None);
                    return Err(e);
                }
            };
            for (slot, cmd) in self.cmds.iter_mut().zip(cmds) {
                if let Some(set) = d.set_loader_data
                    && set(d.handle, vk::Handle::as_raw(cmd) as usize as *mut _) != vk::Result::SUCCESS
                {
                    dev.destroy_command_pool(pool, None);
                    return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
                }
                *slot = cmd;
            }
            self.pool = Some((family, pool));
        }
        Ok(Some(()))
    }

    /// A ring for what the host wants now, announced to it. `Err`: none could be made.
    unsafe fn make_ring(&mut self, d: &DeviceData, want: u32, epoch: u32) -> Result<Ring, vk::Result> {
        let dev = &d.dev;
        let (format, hdr, mode, flags) = choose(unpack(want), self.format, self.color_space);
        let (width, height) = (self.extent.width & !1, self.extent.height & !1);
        let pitch = (width * format.sample_bytes()).next_multiple_of(256);
        let slot_bytes = (pitch as u64 * format.rows(height) as u64).next_multiple_of(1 << 16);
        let size = slot_bytes * SLOTS as u64;
        if size > d.props.limits.max_storage_buffer_range as u64 {
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let mut ring = Ring {
            generation,
            epoch,
            want,
            format,
            hdr,
            mode,
            flags,
            width,
            height,
            pitch,
            slot_bytes,
            alloc_bytes: 0,
            dedicated: false,
            full_range: unpack(want).full_range,
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            last: 0,
            retired: None,
        };
        // SAFETY: valid create infos; on error everything made is destroyed below.
        let r = unsafe { self.fill_ring(d, &mut ring, size) };
        if let Err(e) = r {
            unsafe { destroy_ring(dev, &ring) };
            return Err(e);
        }
        Ok(ring)
    }

    unsafe fn fill_ring(&self, d: &DeviceData, ring: &mut Ring, size: u64) -> Result<(), vk::Result> {
        let dev = &d.dev;
        let support = d.stream.as_ref().ok_or(vk::Result::ERROR_FEATURE_NOT_PRESENT)?;
        // SAFETY: as make_ring.
        unsafe {
            let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            ring.buffer = dev.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::STORAGE_BUFFER).sharing_mode(vk::SharingMode::EXCLUSIVE).push_next(&mut ext), None)?;
            let mut dedicated_req = vk::MemoryDedicatedRequirements::default();
            let mut req = vk::MemoryRequirements2::default().push_next(&mut dedicated_req);
            dev.get_buffer_memory_requirements2(&vk::BufferMemoryRequirementsInfo2::default().buffer(ring.buffer), &mut req);
            let req = req.memory_requirements;
            ring.dedicated = dedicated_req.prefers_dedicated_allocation != 0 || dedicated_req.requires_dedicated_allocation != 0;
            let ty = d.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL, vk::MemoryPropertyFlags::empty()).ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(ring.buffer);
            let mut info = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty).push_next(&mut export);
            if ring.dedicated {
                info = info.push_next(&mut dedicated);
            }
            ring.memory = dev.allocate_memory(&info, None)?;
            ring.alloc_bytes = req.size;
            dev.bind_buffer_memory(ring.buffer, ring.memory, 0)?;
            let n = self.views.len() as u32;
            let sizes = [
                vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: n },
                vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_BUFFER, descriptor_count: n },
            ];
            ring.pool = dev.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(n).pool_sizes(&sizes), None)?;
            let layouts = vec![self.set_layout; n as usize];
            ring.sets = dev.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(ring.pool).set_layouts(&layouts))?;
            for (set, &view) in ring.sets.iter().zip(&self.views) {
                let img = [vk::DescriptorImageInfo { sampler: vk::Sampler::null(), image_view: view, image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL }];
                let buf = [vk::DescriptorBufferInfo { buffer: ring.buffer, offset: 0, range: vk::WHOLE_SIZE }];
                let writes = [
                    vk::WriteDescriptorSet::default().dst_set(*set).dst_binding(0).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&img),
                    vk::WriteDescriptorSet::default().dst_set(*set).dst_binding(1).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(&buf),
                ];
                dev.update_descriptor_sets(&writes, &[]);
            }
            // The fds: the host gets its own copies; ours are closed once sent.
            let mut mem_fd = -1;
            let r = (support.get_memory_fd)(d.handle, &vk::MemoryGetFdInfoKHR::default().memory(ring.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD), &mut mem_fd);
            if r != vk::Result::SUCCESS {
                return Err(r);
            }
            let mut sem_fd = -1;
            let r = (support.get_semaphore_fd)(d.handle, &vk::SemaphoreGetFdInfoKHR::default().semaphore(self.timeline).handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD), &mut sem_fd);
            if r != vk::Result::SUCCESS {
                libc::close(mem_fd);
                return Err(r);
            }
            let msg = ToHost::Ring(capture::Ring {
                generation: ring.generation,
                slots: SLOTS as u8,
                format: ring.format,
                width: ring.width,
                height: ring.height,
                pitch: ring.pitch,
                slot_bytes: ring.slot_bytes,
                alloc_bytes: ring.alloc_bytes,
                dedicated: ring.dedicated,
                hdr: ring.hdr,
                full_range: ring.full_range,
            });
            let sent = send(&msg, &[mem_fd, sem_fd]);
            libc::close(mem_fd);
            libc::close(sem_fd);
            if !sent {
                return Err(vk::Result::NOT_READY);
            }
        }
        log!(
            "stream capture: ring {} {:?} {}x{} pitch {} ({} bytes, {}dedicated), {} from {:?} {:?}",
            ring.generation,
            ring.format,
            ring.width,
            ring.height,
            ring.pitch,
            ring.alloc_bytes,
            if ring.dedicated { "" } else { "not " },
            if ring.hdr { "HDR10" } else { "SDR" },
            self.format,
            self.color_space
        );
        Ok(())
    }

    /// Frees replaced rings the GPU is done with, a second after they went (the host may
    /// still be reading the last frame; its import holds the memory anyway).
    unsafe fn reap(&mut self, dev: &ash::Device, done: u64) {
        let (keep, gone): (Vec<Ring>, Vec<Ring>) = std::mem::take(&mut self.retired).into_iter().partition(|r| r.last > done || r.retired.is_some_and(|t| t.elapsed() < Duration::from_secs(1)));
        self.retired = keep;
        for r in gone {
            // SAFETY: no submit of ours uses it any more.
            unsafe { destroy_ring(dev, &r) };
        }
    }

    /// One present's capture. `Ok(Some(s))`: submitted; the present waits on `s` alone.
    /// `Ok(None)`: not captured this time (no slot, the host not ready). `Err`: captures
    /// on this swapchain are off.
    ///
    /// # Safety
    /// Inside the app's `vkQueuePresentKHR` on `queue` (of `family`), `image` presented, `waits`
    /// what the present would wait on.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn frame(&mut self, d: &DeviceData, queue: vk::Queue, family: u32, image: u32, waits: &[vk::Semaphore], present_id: Option<u64>, presented: u64) -> Result<Option<vk::Semaphore>, vk::Result> {
        let dev = &d.dev;
        let image = image as usize;
        if image >= self.views.len() || waits.len() > crate::compositor::MAX_WAITS {
            return Ok(None);
        }
        if !d.families.get(family as usize).is_some_and(|f| f.queue_flags.contains(vk::QueueFlags::COMPUTE)) {
            return Ok(None);
        }
        let Some(done) = self.counter(d) else { return Err(vk::Result::ERROR_DEVICE_LOST) };
        // SAFETY: our objects; the queue is externally synchronized by the app's present.
        unsafe {
            if self.ensure_pool(d, family, done)?.is_none() {
                return Ok(None);
            }
            self.reap(dev, done);
            let (want, epoch) = (LINK.want.load(Ordering::Relaxed), LINK.epoch.load(Ordering::Acquire));
            if self.ring.as_ref().is_none_or(|r| r.want != want || r.epoch != epoch) {
                if self.retry_at.is_some_and(|t| Instant::now() < t) {
                    return Ok(None);
                }
                if let Some(mut old) = self.ring.take() {
                    old.retired = Some(Instant::now());
                    self.retired.push(old);
                }
                match self.make_ring(d, want, epoch) {
                    Ok(r) => {
                        for f in &LINK.free {
                            f.store(true, Ordering::Relaxed);
                        }
                        LINK.generation.store(r.generation, Ordering::Release);
                        self.ring = Some(r);
                    }
                    // The socket can't take the announcement now: try at a later present.
                    Err(vk::Result::NOT_READY) => {
                        self.retry_at = Some(Instant::now() + Duration::from_millis(100));
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                }
            }
            let ring = self.ring.as_ref().unwrap();
            let Some(slot) = (0..SLOTS).map(|k| (self.next + k) % SLOTS).find(|&i| LINK.free[i].load(Ordering::Acquire) && self.cmd_value[i] <= done) else {
                DROPPED.fetch_add(1, Ordering::Relaxed);
                let held = LINK.free.iter().all(|f| !f.load(Ordering::Relaxed));
                (if held { &HELD } else { &BUSY }).fetch_add(1, Ordering::Relaxed);
                return Ok(None);
            };
            let cmd = self.cmds[slot];
            dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            let offset = slot as u64 * ring.slot_bytes;
            let range = ring.pitch as u64 * ring.format.rows(ring.height) as u64;
            let read = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.images[image])
                .subresource_range(crate::compositor::color_range());
            // The slot comes back from the host (external), and goes to it after the write.
            let acquire = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .dst_queue_family_index(family)
                .buffer(ring.buffer)
                .offset(offset)
                .size(range);
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::COMPUTE_SHADER, vk::DependencyFlags::empty(), &[], &[acquire], &[read]);
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            dev.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::COMPUTE, self.layout, 0, &[ring.sets[image]], &[]);
            let push = Push {
                size: [ring.width, ring.height],
                in_size: [self.extent.width, self.extent.height],
                pitch: ring.pitch,
                base: (offset / 4) as u32,
                mode: ring.mode,
                flags: ring.flags,
            };
            // SAFETY: repr(C) u32s.
            let bytes = std::slice::from_raw_parts((&push as *const Push).cast::<u8>(), std::mem::size_of::<Push>());
            dev.cmd_push_constants(cmd, self.layout, vk::ShaderStageFlags::COMPUTE, 0, bytes);
            let (bx, by) = block(ring.mode);
            dev.cmd_dispatch(cmd, ring.width.div_ceil(bx).div_ceil(8), ring.height.div_ceil(by).div_ceil(8), 1);
            let back = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.images[image])
                .subresource_range(crate::compositor::color_range());
            let release = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::empty())
                .src_queue_family_index(family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .buffer(ring.buffer)
                .offset(offset)
                .size(range);
            dev.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::COMPUTE_SHADER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), &[], &[release], &[back]);
            dev.end_command_buffer(cmd)?;

            let value = self.value + 1;
            let stages = [vk::PipelineStageFlags::COMPUTE_SHADER; crate::compositor::MAX_WAITS];
            let cmds = [cmd];
            let signal = [self.semaphores[image], self.timeline];
            let values = [0, value];
            let mut timeline = vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&values);
            let mut latency = vk::LatencySubmissionPresentIdNV::default().present_id(present_id.unwrap_or(0));
            let mut submit = vk::SubmitInfo::default().wait_semaphores(waits).wait_dst_stage_mask(&stages[..waits.len()]).command_buffers(&cmds).signal_semaphores(&signal).push_next(&mut timeline);
            if d.low_latency2 && present_id.is_some() {
                submit = submit.push_next(&mut latency);
            }
            dev.queue_submit(queue, &[submit], vk::Fence::null())?;
            self.value = value;
            self.cmd_value[slot] = value;
            self.seq += 1;
            self.next = (slot + 1) % SLOTS;
            let ring = self.ring.as_mut().unwrap();
            ring.last = value;
            LINK.free[slot].store(false, Ordering::Relaxed);
            let hdr = if ring.hdr { metadata(self.handle) } else { None };
            let msg = ToHost::Frame(capture::Frame { generation: ring.generation, slot: slot as u8, value, seq: self.seq, presented, hdr });
            if send(&msg, &[]) {
                CAPTURED.fetch_add(1, Ordering::Relaxed);
            } else {
                // The host never hears of it: the slot is ours again (reused once the GPU is
                // done with it).
                LINK.free[slot].store(true, Ordering::Relaxed);
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Some(signal[0]))
        }
    }

    /// Waits (up to a second) for our submits, then destroys everything. The image
    /// semaphores a present may still wait on are retired with the device's.
    ///
    /// # Safety
    /// The swapchain is going away.
    pub unsafe fn destroy(mut self, d: &DeviceData) {
        if let Some(s) = &d.stream {
            let values = [self.value];
            let sems = [self.timeline];
            // SAFETY: our semaphore.
            let _ = unsafe { (s.wait_semaphores)(d.handle, &vk::SemaphoreWaitInfo::default().semaphores(&sems).values(&values), 1_000_000_000) };
        }
        // SAFETY: done waiting.
        unsafe { self.destroy_now(d) };
    }

    unsafe fn destroy_now(&mut self, d: &DeviceData) {
        let dev = &d.dev;
        // SAFETY: our objects, idle.
        unsafe {
            for r in self.ring.take().into_iter().chain(self.retired.drain(..)) {
                destroy_ring(dev, &r);
            }
            if let Some((_, pool)) = self.pool.take() {
                dev.destroy_command_pool(pool, None);
            }
            dev.destroy_pipeline(self.pipeline, None);
            dev.destroy_pipeline_layout(self.layout, None);
            dev.destroy_descriptor_set_layout(self.set_layout, None);
            dev.destroy_sampler(self.sampler, None);
            dev.destroy_semaphore(self.timeline, None);
            for v in self.views.drain(..) {
                dev.destroy_image_view(v, None);
            }
        }
        d.retire_semaphores(self.semaphores.drain(..));
    }
}

unsafe fn destroy_ring(dev: &ash::Device, r: &Ring) {
    // SAFETY: not in use (the caller checked); null handles are ignored.
    unsafe {
        dev.destroy_descriptor_pool(r.pool, None);
        dev.destroy_buffer(r.buffer, None);
        dev.free_memory(r.memory, None);
    }
}

/// The game's last HDR metadata on `swapchain`, in the stream's units (never waiting for the
/// lock).
fn metadata(swapchain: vk::SwapchainKHR) -> Option<quiver_stream_proto::HdrMetadata> {
    let g = crate::hdr::METADATA.try_lock().ok()?;
    let m = g.as_ref().filter(|m| m.swapchain == vk::Handle::as_raw(swapchain))?;
    let p = &m.primaries;
    Some(capture::hdr_metadata([[p[0], p[1]], [p[2], p[3]], [p[4], p[5]], [p[6], p[7]]], m.max_luminance, m.min_luminance, m.max_cll, m.max_fall))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices() {
        let sdr = Want::default();
        let hdr = Want { hdr: true, ..Default::default() };
        let srgb = vk::ColorSpaceKHR::SRGB_NONLINEAR;
        let pq = vk::ColorSpaceKHR::HDR10_ST2084_EXT;
        let scrgb = vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT;
        assert_eq!(choose(sdr, vk::Format::B8G8R8A8_UNORM, srgb), (PixelFormat::Nv12, false, 0, 0));
        assert_eq!(choose(sdr, vk::Format::B8G8R8A8_SRGB, srgb), (PixelFormat::Nv12, false, 0, 1));
        // HDR allowed but an SDR game: SDR.
        assert_eq!(choose(hdr, vk::Format::B8G8R8A8_UNORM, srgb), (PixelFormat::Nv12, false, 0, 0));
        assert_eq!(choose(hdr, vk::Format::A2B10G10R10_UNORM_PACK32, pq), (PixelFormat::P010, true, 1, 2 | 4));
        assert_eq!(choose(hdr, vk::Format::R16G16B16A16_SFLOAT, scrgb), (PixelFormat::P010, true, 1, 3 | 4));
        // An HDR game, HDR not allowed: tone-mapped to 8-bit SDR.
        assert_eq!(choose(sdr, vk::Format::A2B10G10R10_UNORM_PACK32, pq), (PixelFormat::Nv12, false, 0, 2));
        let w = Want { chroma444: true, ten_bit: false, hdr: true, full_range: true };
        assert_eq!(choose(w, vk::Format::A2B10G10R10_UNORM_PACK32, pq), (PixelFormat::Yuv444P16, true, 3, 2 | 4 | 8));
        assert_eq!(unpack(pack(w)), w);
        assert_eq!(block(0), (4, 2));
        assert_eq!(std::mem::size_of::<Push>(), 32);
    }

    #[test]
    fn inert_without_a_socket() {
        // The test process has no socket named: nothing is configured or live.
        if std::env::var_os(ENV_SOCKET).is_none() {
            assert!(!configured());
            assert!(!live());
        }
    }
}
