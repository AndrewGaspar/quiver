//! Keeps a truncated ring from taking quiverd down.
//!
//! Rings are files the game process owns and can `ftruncate` while quiverd has them
//! mapped; a read past the new end then faults with SIGBUS. `Mapping::size_now` before
//! each drain narrows that window but can't close it. So quiverd installs a SIGBUS handler
//! that, for a fault inside a mapping it registered here, maps a page of zeros over the
//! faulting page (`MAP_FIXED | MAP_ANONYMOUS`) and returns: the load is retried and reads
//! zeros, which the ring protocol sees as uncommitted slots. The reader checks
//! [`Guard::faulted`] after each drain and drops the ring as invalid ("truncated").
//!
//! Faults anywhere else restore the previous disposition and return, so the fault repeats
//! and the process dies (with a core dump) exactly as it would have without the handler.
//!
//! Only the thread that reads rings touches registered mappings, and it registers and
//! unregisters them itself, so the handler never races a change to the table it reads.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

const SLOTS: usize = 64;

struct Slot {
    base: AtomicUsize,
    len: AtomicUsize,
    faults: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: Slot = Slot { base: AtomicUsize::new(0), len: AtomicUsize::new(0), faults: AtomicU32::new(0) };
static TABLE: [Slot; SLOTS] = [EMPTY; SLOTS];
static PREVIOUS: OnceLock<libc::sigaction> = OnceLock::new();
static PAGE: AtomicUsize = AtomicUsize::new(4096);

extern "C" fn on_sigbus(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo for SA_SIGINFO handlers.
    let addr = unsafe { (*info).si_addr() } as usize;
    let page = PAGE.load(Ordering::Relaxed);
    for slot in &TABLE {
        let base = slot.base.load(Ordering::Acquire);
        let len = slot.len.load(Ordering::Acquire);
        if base != 0 && addr >= base && addr < base + len {
            let at = addr & !(page - 1);
            // SAFETY: replaces one page inside a mapping this process made and still owns
            // (registered and not yet unmapped) with private zeros; mmap is a plain syscall.
            let p = unsafe {
                libc::mmap(
                    at as *mut libc::c_void,
                    page,
                    libc::PROT_READ,
                    libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if p != libc::MAP_FAILED {
                slot.faults.fetch_add(1, Ordering::AcqRel);
                return;
            }
            break;
        }
    }
    // Not ours (or the remap failed): the previous disposition, then let the fault repeat.
    if let Some(prev) = PREVIOUS.get() {
        // SAFETY: restoring a sigaction we read at install time.
        unsafe { libc::sigaction(libc::SIGBUS, prev, std::ptr::null_mut()) };
    }
}

/// Installs the handler (once per process).
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page > 0 {
            PAGE.store(page as usize, Ordering::Relaxed);
        }
        // SAFETY: a zeroed sigaction is a valid starting point; we fill handler, mask, flags.
        unsafe {
            let mut prev: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGBUS, std::ptr::null(), &mut prev);
            let _ = PREVIOUS.set(prev);
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_sigbus as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut()) != 0 {
                tracing::warn!("installing the SIGBUS guard for perf rings failed: {}", std::io::Error::last_os_error());
            }
        }
    });
}

/// A registered mapping. Unregisters on drop, which must happen before the mapping is
/// unmapped (declare it after the mapping, or drop it first).
pub struct Guard {
    slot: Option<usize>,
}

impl Guard {
    /// Registers `[base, base + len)`. Without a free slot the mapping is unguarded (the
    /// caller decides whether to read it anyway; quiverd doesn't).
    pub fn register(base: *const u8, len: usize) -> Guard {
        install();
        for (i, slot) in TABLE.iter().enumerate() {
            if slot.base.compare_exchange(0, usize::MAX, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                slot.faults.store(0, Ordering::Release);
                slot.len.store(len, Ordering::Release);
                slot.base.store(base as usize, Ordering::Release);
                return Guard { slot: Some(i) };
            }
        }
        Guard { slot: None }
    }

    pub fn is_guarded(&self) -> bool {
        self.slot.is_some()
    }

    /// Faults absorbed since registration: the file was truncated under the mapping.
    pub fn faulted(&self) -> u32 {
        self.slot.map_or(0, |i| TABLE[i].faults.load(Ordering::Acquire))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(i) = self.slot {
            TABLE[i].len.store(0, Ordering::Release);
            TABLE[i].base.store(0, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn truncated_mapping_reads_zeros_instead_of_crashing() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[0xab; 3 * 4096]).unwrap();
        use std::os::unix::io::AsRawFd;
        // SAFETY: a fresh read-only shared mapping of a file we hold.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), 3 * 4096, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
        assert_ne!(p, libc::MAP_FAILED);
        let base = p as *const u8;
        let g = Guard::register(base, 3 * 4096);
        assert!(g.is_guarded());
        // SAFETY: inside the mapping.
        assert_eq!(unsafe { std::ptr::read_volatile(base.add(8192)) }, 0xab);
        f.set_len(4096).unwrap();
        // Past the new end: SIGBUS, absorbed.
        // SAFETY: inside the mapping; the guard replaces the page on the fault.
        let v = unsafe { std::ptr::read_volatile(base.add(8192 + 5)) };
        assert_eq!(v, 0);
        assert_eq!(g.faulted(), 1);
        // Still-backed pages read as before.
        // SAFETY: inside the mapping, within the file.
        assert_eq!(unsafe { std::ptr::read_volatile(base) }, 0xab);
        drop(g);
        // SAFETY: unmapping what we mapped.
        unsafe { libc::munmap(p, 3 * 4096) };
    }
}
