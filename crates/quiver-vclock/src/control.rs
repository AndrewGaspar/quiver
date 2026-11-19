//! Pause and step (design/LAB.md §8.2): the control page `vclock.ctl` that the hook DLL maps in
//! the game's process and the lab maps on the host (`win/vclock.h`'s `vc_ctl`, field for field).
//!
//! The controller writes a command (`cmd`, `cmd_arg`, `cmd_timeout_ms`), then `cmd_seq + 1`.
//! The DLL applies each command once, at the next counted Present, from its status thread
//! within 50 ms, or within a millisecond while held, publishes its state, then sets `ack_seq`.
//! The state block is a seqlock (`update` is odd while the DLL writes it).
//!
//! It is a plain file in the run folder, so nothing here can hang the game: when no controller
//! writes, the DLL never holds; when the game dies, the page just stops changing (the lab checks
//! the run, not the page, to tell).

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use serde::Serialize;

/// The page's file in the run folder.
pub const CTL_FILE: &str = "vclock.ctl";
pub const SIZE: usize = 4096;
pub const MAGIC: u64 = 0x314c_5443_5651_5551;
pub const VERSION: u64 = 1;

/// Commands (`VC_CMD_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cmd {
    Pause = 1,
    Step = 2,
    Resume = 3,
}

impl Cmd {
    fn from_u64(v: u64) -> Option<Cmd> {
        match v {
            1 => Some(Cmd::Pause),
            2 => Some(Cmd::Step),
            3 => Some(Cmd::Resume),
            _ => None,
        }
    }
}

/// `vc_ctl` (win/vclock.h), field for field. Atomics have the layout of the plain integers.
#[repr(C)]
pub struct Page {
    pub magic: AtomicU64,
    pub version: AtomicU64,
    pub pid: AtomicU64,
    pub cmd_seq: AtomicU64,
    pub cmd: AtomicU64,
    pub cmd_arg: AtomicU64,
    pub cmd_timeout_ms: AtomicU64,
    pub ack_seq: AtomicU64,
    pub update: AtomicU64,
    pub state: AtomicU64,
    pub engaged: AtomicU64,
    pub frames: AtomicU64,
    pub vticks: AtomicI64,
    pub hold_at: AtomicU64,
    pub ticks_per_frame: AtomicU64,
    pub qpf: AtomicU64,
    pub pauses: AtomicU64,
    pub steps: AtomicU64,
    pub stepped: AtomicU64,
    pub holds: AtomicU64,
    pub held_us: AtomicU64,
    pub held_reads: AtomicU64,
    pub leaked: AtomicI64,
    pub auto_resumes: AtomicU64,
    pub ack_frames: AtomicU64,
    pub ack_hold_at: AtomicU64,
    pub ack_cmd: AtomicU64,
    pub creep_frames: AtomicU64,
    pub escapes: AtomicU64,
    pub overruns: AtomicU64,
}

const _: () = assert!(std::mem::size_of::<Page>() == 30 * 8 && std::mem::size_of::<Page>() <= SIZE);

/// The clock's state as the page has it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    /// `running`, `holding` (a pause or step asked for; the frame in flight hasn't presented
    /// yet) or `held` (the game is blocked at its Present: time doesn't move).
    pub state: String,
    pub engaged: bool,
    /// Presents counted since engagement (the frame number).
    pub frames: u64,
    /// Virtual seconds since engagement (`frames / rate` exactly, unless a valve fired).
    pub virtual_s: f64,
    pub vticks: i64,
    /// The present count a hold stops at (0: running free).
    pub hold_at: u64,
    pub ticks_per_frame: u64,
    pub qpf: u64,
    /// Holds started (a pause, or a step from running).
    pub pauses: u64,
    /// Frames asked for by steps.
    pub steps: u64,
    /// Presents while a hold was on (the steps let through).
    pub stepped: u64,
    /// Times the game blocked at the gate.
    pub holds: u64,
    pub held_s: f64,
    /// Clock reads while held: they return the frozen value and count for no creep.
    pub held_reads: u64,
    /// Virtual ticks time moved while held (0 by design; anything else slipped through).
    pub leaked: i64,
    /// Holds ended by their own timeout.
    pub auto_resumes: u64,
    pub ack_seq: u64,
    pub ack_cmd: Option<Cmd>,
    pub ack_frames: u64,
    pub ack_hold_at: u64,
    pub pid: u64,
    pub creep_frames: u64,
    pub escapes: u64,
    pub overruns: u64,
}

impl Snapshot {
    pub fn held(&self) -> bool {
        self.state == "held"
    }
    pub fn running(&self) -> bool {
        self.state == "running"
    }
    /// The virtual rate the page implies.
    pub fn rate(&self) -> f64 {
        if self.ticks_per_frame > 0 { self.qpf as f64 / self.ticks_per_frame as f64 } else { 0.0 }
    }
}

/// The page, mapped.
pub struct Control {
    file: File,
    ptr: *mut Page,
}

// SAFETY: the page is only touched through atomics.
unsafe impl Send for Control {}
unsafe impl Sync for Control {}

impl Drop for Control {
    fn drop(&mut self) {
        // SAFETY: our mapping of SIZE bytes.
        unsafe { libc::munmap(self.ptr.cast(), SIZE) };
    }
}

impl Control {
    fn map(file: File) -> std::io::Result<Control> {
        // SAFETY: a shared mapping of SIZE bytes of a file at least that long.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Control { file, ptr: p.cast() })
    }

    /// The run folder's page, as the DLL made it. `Err` with why not.
    pub fn open(dir: &Path) -> Result<Control, String> {
        let path = dir.join(CTL_FILE);
        let file = OpenOptions::new().read(true).write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < SIZE as u64 {
            return Err(format!("{} is {len} bytes, not a control page", path.display()));
        }
        let c = Control::map(file).map_err(|e| format!("map {}: {e}", path.display()))?;
        let magic = c.page().magic.load(Ordering::Acquire);
        if magic != MAGIC {
            return Err(format!("{} isn't a control page (magic {magic:#x})", path.display()));
        }
        Ok(c)
    }

    /// A new zeroed page (the tests stand in for the DLL with it).
    pub fn create(path: &Path) -> std::io::Result<Control> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(SIZE as u64)?;
        Control::map(file)
    }

    pub fn page(&self) -> &Page {
        // SAFETY: mapped for our lifetime; every field is an atomic.
        unsafe { &*self.ptr }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn page_ptr(&self) -> *mut Page {
        self.ptr
    }

    /// A consistent copy of the state block (the seqlock).
    pub fn snapshot(&self) -> Snapshot {
        let p = self.page();
        let mut s = Snapshot::default();
        for _ in 0..10_000 {
            let u0 = p.update.load(Ordering::Acquire);
            if u0 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            s = self.read_fields();
            std::sync::atomic::fence(Ordering::Acquire);
            if p.update.load(Ordering::Acquire) == u0 {
                return s;
            }
        }
        s
    }

    fn read_fields(&self) -> Snapshot {
        let p = self.page();
        let l = |a: &AtomicU64| a.load(Ordering::Acquire);
        let tpf = l(&p.ticks_per_frame);
        let qpf = l(&p.qpf);
        let vticks = p.vticks.load(Ordering::Acquire);
        Snapshot {
            state: match l(&p.state) {
                1 => "holding",
                2 => "held",
                _ => "running",
            }
            .into(),
            engaged: l(&p.engaged) != 0,
            frames: l(&p.frames),
            virtual_s: if qpf > 0 { vticks as f64 / qpf as f64 } else { 0.0 },
            vticks,
            hold_at: l(&p.hold_at),
            ticks_per_frame: tpf,
            qpf,
            pauses: l(&p.pauses),
            steps: l(&p.steps),
            stepped: l(&p.stepped),
            holds: l(&p.holds),
            held_s: l(&p.held_us) as f64 / 1e6,
            held_reads: l(&p.held_reads),
            leaked: p.leaked.load(Ordering::Acquire),
            auto_resumes: l(&p.auto_resumes),
            ack_seq: l(&p.ack_seq),
            ack_cmd: Cmd::from_u64(l(&p.ack_cmd)),
            ack_frames: l(&p.ack_frames),
            ack_hold_at: l(&p.ack_hold_at),
            pid: l(&p.pid),
            creep_frames: l(&p.creep_frames),
            escapes: l(&p.escapes),
            overruns: l(&p.overruns),
        }
    }

    /// Writes a command; returns its sequence number (see [`Control::acked`]). Commands from two
    /// controllers at once are serialised by a lock on the file; one that the DLL hasn't
    /// applied yet is replaced by the next, so wait for each.
    pub fn send(&self, cmd: Cmd, arg: u64, timeout_ms: u64) -> u64 {
        let fd = self.file.as_raw_fd();
        // SAFETY: flock on our own descriptor.
        unsafe { libc::flock(fd, libc::LOCK_EX) };
        let p = self.page();
        let seq = p.cmd_seq.load(Ordering::Acquire).max(p.ack_seq.load(Ordering::Acquire)) + 1;
        p.cmd.store(cmd as u64, Ordering::Release);
        p.cmd_arg.store(arg, Ordering::Release);
        p.cmd_timeout_ms.store(timeout_ms, Ordering::Release);
        p.cmd_seq.store(seq, Ordering::Release);
        // SAFETY: as above.
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        seq
    }

    /// The DLL applied command `seq` (or a later one).
    pub fn acked(&self, seq: u64) -> bool {
        self.page().ack_seq.load(Ordering::Acquire) >= seq
    }
}
