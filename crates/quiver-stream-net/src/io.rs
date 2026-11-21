//! The UDP socket, the Linux way: GSO on send, GRO and `recvmmsg` on receive, a per-packet
//! DSCP mark, and DF set with the kernel's PMTU cache ignored (our own probing decides).
//!
//! The DSCP mark matters on Wi-Fi: Linux and most access points map it to a WMM access
//! category, so video (AF41) and input/audio (EF) skip the best-effort queue that a
//! download on the same network fills.

use std::io;
use std::mem::{self, MaybeUninit};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const UDP_SEGMENT: libc::c_int = 103;
const UDP_GRO: libc::c_int = 104;

/// The DSCP class of a datagram, written into the TOS / traffic class byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Video (AF41).
    Media,
    /// Input, audio, control and acks (EF).
    Interactive,
}

impl Class {
    fn tos(self) -> libc::c_int {
        match self {
            Class::Media => 34 << 2,
            Class::Interactive => 46 << 2,
        }
    }
}

/// Socket state: which offloads work.
#[derive(Debug)]
pub struct Io {
    gso: bool,
    gro: bool,
    v6: bool,
    dscp: bool,
}

fn setsockopt(fd: RawFd, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
    // SAFETY: a valid fd and a pointer to an int of the given size.
    let r = unsafe {
        libc::setsockopt(fd, level, name, &value as *const _ as *const libc::c_void, mem::size_of::<libc::c_int>() as libc::socklen_t)
    };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

impl Io {
    /// Sets the socket up: non-blocking, big buffers, DF, GRO; and finds out whether GSO
    /// works.
    pub fn new(sock: &UdpSocket, dscp: bool) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        let fd = sock.as_raw_fd();
        let v6 = sock.local_addr()?.is_ipv6();
        let _ = setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, 4 << 20);
        let _ = setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, 4 << 20);
        let _ = setsockopt(fd, libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, libc::IP_PMTUDISC_PROBE);
        if v6 {
            let _ = setsockopt(fd, libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER, libc::IPV6_PMTUDISC_PROBE);
        }
        let gro = setsockopt(fd, libc::SOL_UDP, UDP_GRO, 1).is_ok();
        let gso = setsockopt(fd, libc::SOL_UDP, UDP_SEGMENT, 0).is_ok();
        Ok(Self { gso, gro, v6, dscp })
    }

    /// The most segments one send may carry.
    pub fn max_segments(&self) -> usize {
        if self.gso { 64 } else { 1 }
    }

    pub fn gro(&self) -> bool {
        self.gro
    }

    /// Sends `buf` to `dest`. With `segment`, `buf` is several datagrams of that size end to
    /// end (the last may be shorter), sent in one call with GSO.
    pub fn send(&mut self, sock: &UdpSocket, dest: SocketAddr, buf: &[u8], segment: Option<usize>, class: Class) -> io::Result<()> {
        let dest = if self.v6 { SocketAddr::V6(to_v6(dest)) } else { dest };
        let mut addr = SockAddr::from(dest);
        let mut iov = libc::iovec { iov_base: buf.as_ptr() as *mut libc::c_void, iov_len: buf.len() };
        let mut control = [0u64; 8];
        // SAFETY: msghdr is plain data; every pointer set below outlives the call.
        let mut hdr: libc::msghdr = unsafe { mem::zeroed() };
        hdr.msg_name = addr.as_mut_ptr();
        hdr.msg_namelen = addr.len;
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = mem::size_of_val(&control) as _;
        let mut used = 0usize;
        // SAFETY: the control buffer is 64 bytes, aligned for cmsghdr, with room for two
        // int-sized messages; CMSG_NXTHDR stays inside msg_controllen.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&hdr);
            if self.dscp {
                let mapped_v4 = matches!(dest, SocketAddr::V6(a) if a.ip().to_ipv4_mapped().is_some());
                let (level, name) = if !self.v6 || mapped_v4 {
                    (libc::IPPROTO_IP, libc::IP_TOS)
                } else {
                    (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
                };
                used += put_cmsg(&mut cmsg, &hdr, level, name, class.tos());
            }
            if let Some(seg) = segment.filter(|s| *s < buf.len()) {
                (*cmsg).cmsg_level = libc::SOL_UDP;
                (*cmsg).cmsg_type = UDP_SEGMENT;
                (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<u16>() as u32) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut u16, seg as u16);
                used += libc::CMSG_SPACE(mem::size_of::<u16>() as u32) as usize;
            }
        }
        hdr.msg_controllen = used as _;
        if used == 0 {
            hdr.msg_control = std::ptr::null_mut();
        }
        loop {
            // SAFETY: hdr is fully initialised and its buffers live through the call.
            let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, 0) };
            if n >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                // The kernel or driver can't segment: fall back to one datagram a call.
                Some(libc::EIO) | Some(libc::EINVAL) if segment.is_some() && self.gso => {
                    self.gso = false;
                    let seg = segment.expect("checked");
                    for d in buf.chunks(seg) {
                        self.send(sock, dest, d, None, class)?;
                    }
                    return Ok(());
                }
                Some(libc::EINVAL) if self.dscp => {
                    self.dscp = false;
                    return self.send(sock, dest, buf, segment, class);
                }
                _ => return Err(e),
            }
        }
    }
}

/// Writes an int control message at `*cmsg` and advances it. Returns the bytes used.
///
/// # Safety
/// `*cmsg` must point at room for an int-sized control message inside `hdr`'s buffer.
unsafe fn put_cmsg(cmsg: &mut *mut libc::cmsghdr, hdr: &libc::msghdr, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> usize {
    unsafe {
        (**cmsg).cmsg_level = level;
        (**cmsg).cmsg_type = name;
        (**cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<libc::c_int>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(*cmsg) as *mut libc::c_int, value);
        *cmsg = libc::CMSG_NXTHDR(hdr, *cmsg);
        libc::CMSG_SPACE(mem::size_of::<libc::c_int>() as u32) as usize
    }
}

fn to_v6(a: SocketAddr) -> SocketAddrV6 {
    match a {
        SocketAddr::V4(v4) => SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0),
        SocketAddr::V6(v6) => v6,
    }
}

/// Unmaps a v4-mapped address so the two forms compare equal.
pub fn canonical(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => a,
        },
        v4 => v4,
    }
}

struct SockAddr {
    storage: libc::sockaddr_storage,
    len: libc::socklen_t,
}

impl SockAddr {
    fn as_mut_ptr(&mut self) -> *mut libc::c_void {
        &mut self.storage as *mut _ as *mut libc::c_void
    }
}

impl From<SocketAddr> for SockAddr {
    fn from(a: SocketAddr) -> Self {
        // SAFETY: sockaddr_storage is plain data, large and aligned enough for either form.
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let len = match a {
            SocketAddr::V4(v4) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: v4.port().to_be(),
                    sin_addr: libc::in_addr { s_addr: u32::from(*v4.ip()).to_be() },
                    sin_zero: [0; 8],
                };
                // SAFETY: as above.
                unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in, sin) };
                mem::size_of::<libc::sockaddr_in>()
            }
            SocketAddr::V6(v6) => {
                let sin6 = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: libc::in6_addr { s6_addr: v6.ip().octets() },
                    sin6_scope_id: v6.scope_id(),
                };
                // SAFETY: as above.
                unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in6, sin6) };
                mem::size_of::<libc::sockaddr_in6>()
            }
        };
        Self { storage, len: len as libc::socklen_t }
    }
}

fn from_storage(s: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match s.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: the family says this is a sockaddr_in.
            let sin = unsafe { &*(s as *const _ as *const libc::sockaddr_in) };
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)), u16::from_be(sin.sin_port))))
        }
        libc::AF_INET6 => {
            // SAFETY: the family says this is a sockaddr_in6.
            let sin6 = unsafe { &*(s as *const _ as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            Some(canonical(SocketAddr::V6(SocketAddrV6::new(ip, u16::from_be(sin6.sin6_port), sin6.sin6_flowinfo, sin6.sin6_scope_id))))
        }
        _ => None,
    }
}

/// Buffers for one `recvmmsg` call.
pub struct RecvBatch {
    bufs: Vec<Vec<u8>>,
    addrs: Vec<libc::sockaddr_storage>,
    controls: Vec<[u64; 8]>,
    /// `(from, len, segment size)` per message received.
    pub meta: Vec<(SocketAddr, usize, usize)>,
}

impl RecvBatch {
    pub fn new(messages: usize, gro: bool) -> Self {
        let size = if gro { 65_536 } else { 2_048 };
        Self {
            bufs: (0..messages).map(|_| vec![0; size]).collect(),
            // SAFETY: plain data.
            addrs: (0..messages).map(|_| unsafe { mem::zeroed() }).collect(),
            controls: vec![[0; 8]; messages],
            meta: Vec::with_capacity(messages),
        }
    }

    /// Message `i`'s bytes.
    pub fn buf_mut(&mut self, i: usize) -> &mut [u8] {
        let len = self.meta[i].1;
        &mut self.bufs[i][..len]
    }

    /// Receives what is waiting, without blocking. Returns the number of messages; each may
    /// hold several datagrams (GRO) of `meta[i].2` bytes each.
    pub fn recv(&mut self, sock: &UdpSocket) -> io::Result<usize> {
        let n = self.bufs.len();
        let mut iovs: Vec<libc::iovec> =
            self.bufs.iter_mut().map(|b| libc::iovec { iov_base: b.as_mut_ptr() as *mut libc::c_void, iov_len: b.len() }).collect();
        let mut hdrs: Vec<libc::mmsghdr> = (0..n)
            .map(|i| {
                // SAFETY: plain data, filled in below.
                let mut h: libc::mmsghdr = unsafe { mem::zeroed() };
                h.msg_hdr.msg_name = &mut self.addrs[i] as *mut _ as *mut libc::c_void;
                h.msg_hdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                h.msg_hdr.msg_iov = &mut iovs[i];
                h.msg_hdr.msg_iovlen = 1;
                h.msg_hdr.msg_control = self.controls[i].as_mut_ptr() as *mut libc::c_void;
                h.msg_hdr.msg_controllen = mem::size_of::<[u64; 8]>() as _;
                h
            })
            .collect();
        // SAFETY: every header points at buffers owned by self or by iovs, which outlive the
        // call.
        let got = unsafe { libc::recvmmsg(sock.as_raw_fd(), hdrs.as_mut_ptr(), n as libc::c_uint, libc::MSG_DONTWAIT, std::ptr::null_mut()) };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        self.meta.clear();
        for h in hdrs.iter().take(got as usize) {
            let len = h.msg_len as usize;
            let i = self.meta.len();
            let from = from_storage(&self.addrs[i]).unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
            let seg = gro_segment(&h.msg_hdr).unwrap_or(len).max(1);
            self.meta.push((from, len, seg));
        }
        Ok(self.meta.len())
    }
}

fn gro_segment(hdr: &libc::msghdr) -> Option<usize> {
    // SAFETY: walking the control messages the kernel wrote, within msg_controllen.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(hdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_UDP && (*cmsg).cmsg_type == UDP_GRO {
                let v = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const libc::c_int);
                return Some(v as usize);
            }
            cmsg = libc::CMSG_NXTHDR(hdr, cmsg);
        }
    }
    None
}

/// An eventfd, to wake the I/O thread from another thread.
#[derive(Debug)]
pub struct Waker {
    fd: OwnedFd,
}

impl Waker {
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd returns a new fd or -1.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh, owned descriptor.
        Ok(Self { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a valid u64 to our eventfd.
        unsafe { libc::write(self.fd.as_raw_fd(), &one as *const u64 as *const libc::c_void, 8) };
    }

    fn drain(&self) {
        let mut v = MaybeUninit::<u64>::uninit();
        // SAFETY: reading 8 bytes into a u64-sized buffer.
        unsafe { libc::read(self.fd.as_raw_fd(), v.as_mut_ptr() as *mut libc::c_void, 8) };
    }
}

/// Waits until `sock` is readable, `waker` fires, or `timeout_us` passes (None: forever).
pub fn wait(sock: &UdpSocket, waker: &Waker, timeout_us: Option<u64>) {
    let mut fds = [
        libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: waker.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    let ts = timeout_us.map(|us| libc::timespec { tv_sec: (us / 1_000_000) as libc::time_t, tv_nsec: ((us % 1_000_000) * 1000) as libc::c_long });
    let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
    // SAFETY: two valid pollfds and an optional valid timespec.
    unsafe { libc::ppoll(fds.as_mut_ptr(), 2, tsp, std::ptr::null()) };
    if fds[1].revents & libc::POLLIN != 0 {
        waker.drain();
    }
}

/// Asks for 1 µs of timer slack on this thread, so the pacer's sleeps end on time (the
/// default is 50 µs).
pub fn tighten_timer_slack() {
    // SAFETY: PR_SET_TIMERSLACK takes a value in nanoseconds.
    unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1000 as libc::c_ulong, 0, 0, 0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gso_send_arrives_as_separate_datagrams() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut io_a = Io::new(&a, true).unwrap();
        let io_b = Io::new(&b, true).unwrap();
        let payload: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        io_a.send(&a, b.local_addr().unwrap(), &payload, Some(1200), Class::Media).unwrap();
        let mut batch = RecvBatch::new(8, io_b.gro());
        let mut got = Vec::new();
        for _ in 0..100 {
            if let Ok(n) = batch.recv(&b) {
                for i in 0..n {
                    let (from, _, seg) = batch.meta[i];
                    assert_eq!(from, a.local_addr().unwrap());
                    got.extend(batch.buf_mut(i).chunks(seg).map(|c| c.len()));
                }
            }
            if got.iter().sum::<usize>() == 3000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(got, vec![1200, 1200, 600]);
    }

    #[test]
    fn mapped_addresses_compare_equal() {
        let v4: SocketAddr = "10.0.0.1:5".parse().unwrap();
        assert_eq!(canonical(SocketAddr::V6(to_v6(v4))), v4);
    }
}
