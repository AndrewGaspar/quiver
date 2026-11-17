//! Shared memory regions: the trait the ring and HUD code works over, raw regions for memory
//! the caller mapped, and (with `std`) file mappings on Linux and under Wine.

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64};

/// Memory shared with other processes.
///
/// # Safety
/// `base()` must be valid for reads (and writes, if `writable()`) of `len()` bytes for as
/// long as the value lives, 64-byte aligned, and only ever accessed through atomics or
/// through this crate's protocols.
pub unsafe trait ShmRegion {
    fn base(&self) -> *mut u8;
    fn len(&self) -> usize;
    fn writable(&self) -> bool;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A region the caller mapped some other way (a Vulkan layer's own mapping, a test).
pub struct RawRegion {
    base: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: the region is only accessed through atomics.
unsafe impl Send for RawRegion {}
unsafe impl Sync for RawRegion {}

impl RawRegion {
    /// # Safety
    /// As [`ShmRegion`]: `base` valid for `len` bytes while the value lives, 64-aligned
    /// (checked), and writable if `writable`.
    pub unsafe fn new(base: *mut u8, len: usize, writable: bool) -> Option<RawRegion> {
        (!base.is_null() && (base as usize) % 64 == 0).then_some(RawRegion { base, len, writable })
    }
}

unsafe impl ShmRegion for RawRegion {
    fn base(&self) -> *mut u8 {
        self.base
    }
    fn len(&self) -> usize {
        self.len
    }
    fn writable(&self) -> bool {
        self.writable
    }
}

unsafe impl<R: ShmRegion> ShmRegion for &R {
    fn base(&self) -> *mut u8 {
        (**self).base()
    }
    fn len(&self) -> usize {
        (**self).len()
    }
    fn writable(&self) -> bool {
        (**self).writable()
    }
}

#[cfg(feature = "std")]
unsafe impl<R: ShmRegion> ShmRegion for std::sync::Arc<R> {
    fn base(&self) -> *mut u8 {
        (**self).base()
    }
    fn len(&self) -> usize {
        (**self).len()
    }
    fn writable(&self) -> bool {
        (**self).writable()
    }
}

/// Typed atomic views at byte offsets. Offsets are crate constants, bounds-checked in debug.
pub(crate) trait Atomics: ShmRegion {
    fn u64_at(&self, off: usize) -> &AtomicU64 {
        debug_assert!(off % 8 == 0 && off + 8 <= self.len());
        // SAFETY: in bounds and aligned (base is 64-aligned, offsets are multiples of 8);
        // the memory is only accessed atomically.
        unsafe { &*self.base().add(off).cast::<AtomicU64>() }
    }
    fn u32_at(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off % 4 == 0 && off + 4 <= self.len());
        // SAFETY: as u64_at.
        unsafe { &*self.base().add(off).cast::<AtomicU32>() }
    }
    fn u16_at(&self, off: usize) -> &AtomicU16 {
        debug_assert!(off % 2 == 0 && off + 2 <= self.len());
        // SAFETY: as u64_at.
        unsafe { &*self.base().add(off).cast::<AtomicU16>() }
    }
}

impl<R: ShmRegion + ?Sized> Atomics for R {}

#[cfg(feature = "std")]
pub use heap::HeapRegion;

#[cfg(feature = "std")]
mod heap {
    use super::ShmRegion;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    /// Zeroed, page-aligned process memory: for tests and in-process use of the formats.
    pub struct HeapRegion {
        base: *mut u8,
        layout: Layout,
    }

    // SAFETY: only accessed through atomics.
    unsafe impl Send for HeapRegion {}
    unsafe impl Sync for HeapRegion {}

    impl HeapRegion {
        pub fn new(len: usize) -> HeapRegion {
            let layout = Layout::from_size_align(len.max(1), 4096).expect("region size");
            // SAFETY: non-zero size.
            let base = unsafe { alloc_zeroed(layout) };
            if base.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            HeapRegion { base, layout }
        }
    }

    impl Drop for HeapRegion {
        fn drop(&mut self) {
            // SAFETY: allocated in new with this layout.
            unsafe { dealloc(self.base, self.layout) }
        }
    }

    unsafe impl ShmRegion for HeapRegion {
        fn base(&self) -> *mut u8 {
            self.base
        }
        fn len(&self) -> usize {
            self.layout.size()
        }
        fn writable(&self) -> bool {
            true
        }
    }
}

#[cfg(all(feature = "std", unix))]
pub use unix::Mapping;

#[cfg(all(feature = "std", unix))]
mod unix {
    use super::ShmRegion;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// A `MAP_SHARED` mapping of a whole file in the session dir.
    pub struct Mapping {
        base: *mut u8,
        len: usize,
        writable: bool,
        file: File,
    }

    // SAFETY: the mapping is only accessed through atomics.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    impl Mapping {
        /// Opens an existing file read-only (`O_RDONLY | O_NOFOLLOW`, `PROT_READ`), as quiverd
        /// opens rings and the layer opens `hud.image`. Refuses anything that isn't a regular
        /// file owned by this user, and files outside `min_len..=max_len`.
        pub fn open_read(path: &Path, min_len: usize, max_len: usize) -> io::Result<Mapping> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)?;
            Self::map(file, false, min_len, max_len)
        }

        /// Opens an existing file read-write, as a writer attaching to a file someone else
        /// created (quiver-sight to `hud.image`).
        pub fn open_write(path: &Path, min_len: usize, max_len: usize) -> io::Result<Mapping> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)?;
            Self::map(file, true, min_len, max_len)
        }

        /// Creates a new file (mode 0600, `O_EXCL`), sizes it to `len` (zero-filled) and maps
        /// it read-write.
        pub fn create(path: &Path, len: usize) -> io::Result<Mapping> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            file.set_len(len as u64)?;
            Self::map(file, true, len, len)
        }

        fn map(file: File, writable: bool, min_len: usize, max_len: usize) -> io::Result<Mapping> {
            let md = file.metadata()?;
            if !md.file_type().is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a regular file"));
            }
            // SAFETY: geteuid has no preconditions.
            if md.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "file owned by another user"));
            }
            let len = usize::try_from(md.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file too large"))?;
            if len < min_len || len > max_len || len == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("file size {len} outside {min_len}..={max_len}")));
            }
            let prot = if writable { libc::PROT_READ | libc::PROT_WRITE } else { libc::PROT_READ };
            // SAFETY: a fresh shared mapping of a file we hold open; checked for failure.
            let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, file.as_raw_fd(), 0) };
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            Ok(Mapping { base: p.cast(), len, writable, file })
        }

        pub fn file(&self) -> &File {
            &self.file
        }

        /// The file's size now. A writer that shrinks a mapped file makes reads past the end
        /// fault (SIGBUS); a reader of untrusted files can check this before each drain to
        /// narrow that window (it can't close it).
        pub fn size_now(&self) -> io::Result<u64> {
            Ok(self.file.metadata()?.len())
        }

        /// Whether `path` still names the mapped file (same device and inode): false after the
        /// writer replaced or removed it.
        pub fn is_file_at(&self, path: &Path) -> bool {
            match (self.file.metadata(), std::fs::symlink_metadata(path)) {
                (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                _ => false,
            }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: mapped in `map` with this length.
            unsafe { libc::munmap(self.base.cast(), self.len) };
        }
    }

    unsafe impl ShmRegion for Mapping {
        fn base(&self) -> *mut u8 {
            self.base
        }
        fn len(&self) -> usize {
            self.len
        }
        fn writable(&self) -> bool {
            self.writable
        }
    }
}

#[cfg(all(feature = "std", windows))]
pub use win::Mapping;

#[cfg(all(feature = "std", windows))]
mod win {
    //! Under Wine, a view of a section made from a real file is a `MAP_SHARED` mmap of that
    //! Unix file (SIGHT-HUD §3.7), so Linux processes mapping the same path see the stores.
    use super::ShmRegion;
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    type Handle = *mut c_void;
    const PAGE_READONLY: u32 = 0x02;
    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_WRITE: u32 = 0x02;
    const FILE_MAP_READ: u32 = 0x04;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(file: Handle, attrs: *mut c_void, protect: u32, max_hi: u32, max_lo: u32, name: *const u16) -> Handle;
        fn MapViewOfFile(mapping: Handle, access: u32, off_hi: u32, off_lo: u32, bytes: usize) -> *mut c_void;
        fn UnmapViewOfFile(base: *const c_void) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    /// A view of a whole file (`CreateFileMappingW` + `MapViewOfFile`).
    pub struct Mapping {
        base: *mut u8,
        len: usize,
        writable: bool,
        _file: File,
    }

    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    impl Mapping {
        pub fn open_read(path: &Path, min_len: usize, max_len: usize) -> io::Result<Mapping> {
            let file = OpenOptions::new().read(true).share_mode(FILE_SHARE_ALL).open(path)?;
            Self::map(file, false, min_len, max_len)
        }

        pub fn open_write(path: &Path, min_len: usize, max_len: usize) -> io::Result<Mapping> {
            let file = OpenOptions::new().read(true).write(true).share_mode(FILE_SHARE_ALL).open(path)?;
            Self::map(file, true, min_len, max_len)
        }

        /// `CREATE_NEW`, sized to `len`; what the bridge does for a mod's `ring_req`.
        pub fn create(path: &Path, len: usize) -> io::Result<Mapping> {
            let file = OpenOptions::new().read(true).write(true).create_new(true).share_mode(FILE_SHARE_ALL).open(path)?;
            file.set_len(len as u64)?;
            Self::map(file, true, len, len)
        }

        fn map(file: File, writable: bool, min_len: usize, max_len: usize) -> io::Result<Mapping> {
            let md = file.metadata()?;
            if !md.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a regular file"));
            }
            let len = md.len() as usize;
            if len < min_len || len > max_len || len == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("file size {len} outside {min_len}..={max_len}")));
            }
            let (protect, access) = if writable { (PAGE_READWRITE, FILE_MAP_WRITE) } else { (PAGE_READONLY, FILE_MAP_READ) };
            // SAFETY: a valid file handle; results are checked.
            unsafe {
                let m = CreateFileMappingW(file.as_raw_handle() as Handle, std::ptr::null_mut(), protect, 0, 0, std::ptr::null());
                if m.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let p = MapViewOfFile(m, access, 0, 0, len);
                let err = io::Error::last_os_error();
                CloseHandle(m); // the view keeps the section alive
                if p.is_null() {
                    return Err(err);
                }
                Ok(Mapping { base: p.cast(), len, writable, _file: file })
            }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: mapped in `map`.
            unsafe { UnmapViewOfFile(self.base as *const c_void) };
        }
    }

    unsafe impl ShmRegion for Mapping {
        fn base(&self) -> *mut u8 {
            self.base
        }
        fn len(&self) -> usize {
            self.len
        }
        fn writable(&self) -> bool {
            self.writable
        }
    }
}
