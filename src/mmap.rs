//! POSIX `mmap`-backed [`PageSource`] implementations.
//!
//! This module is compiled on `cfg(unix)` targets only. [`MmapSource`]
//! works on Linux, macOS, and other POSIX systems; [`HugePageSource`]
//! is Linux-only because it relies on `MAP_HUGETLB`, which is not
//! provided on macOS or the BSDs.
//!
//! # Why mmap
//!
//! Anonymous `mmap` gives three things the global heap allocator does
//! not:
//!
//! 1. **`madvise(MADV_DONTNEED)` for cache eviction** — the ability to
//!    release physical backing while keeping the virtual mapping. This
//!    is essentially mandatory for a file-cache use case.
//! 2. **Guaranteed page-size alignment**, useful for `O_DIRECT` I/O,
//!    DMA, and manual SIMD.
//! 3. **Isolation from the global allocator's fragmentation profile**
//!    for large (≥ 64 KiB) buffers.
//!
//! See `docs/design.md` §8.3 and the mmap analysis note for the full
//! trade-offs.

use std::io;
use std::ptr::{NonNull, null_mut};

use crate::source::PageSource;

/// Anonymous `mmap`-backed [`PageSource`].
///
/// Every [`allocate`](PageSource::allocate) call issues one
/// `mmap(MAP_ANONYMOUS | MAP_PRIVATE)` with the configured page size.
/// Every [`release`](PageSource::release) call issues the matching
/// `munmap`. Pages are returned zero-filled lazily on first touch
/// (the kernel maps them to the shared zero page until written).
///
/// This source is best used **behind a prewarmed pool** so the
/// syscall cost is paid at startup rather than on user-visible
/// allocations — `mmap` and `munmap` are ~1 µs each and far too
/// expensive per request.
///
/// # Example
///
/// ```no_run
/// use paged_alloc::{MmapSource, PagePool};
///
/// // 16 KiB pages backed by mmap, prewarmed with 1024 pages (16 MiB).
/// let mut pool = PagePool::with_source(MmapSource::new(16 * 1024));
/// pool.prewarm(1024);
/// ```
pub struct MmapSource {
    page_size: usize,
}

impl MmapSource {
    /// Create a new mmap-backed source with the given page size.
    ///
    /// # Panics
    /// Panics if `page_size` is 0, smaller than the pointer size, or
    /// not a multiple of the host OS page size (typically 4 KiB).
    pub fn new(page_size: usize) -> Self {
        assert!(
            page_size >= std::mem::size_of::<*mut u8>(),
            "page_size must be at least {} bytes",
            std::mem::size_of::<*mut u8>()
        );
        let os_page = os_page_size();
        assert!(
            page_size.is_multiple_of(os_page),
            "page_size ({}) must be a multiple of the OS page size ({})",
            page_size,
            os_page
        );
        MmapSource { page_size }
    }
}

// SAFETY:
// - `libc::mmap` with `MAP_ANONYMOUS | MAP_PRIVATE` returns a live
//   region of exactly `page_size` bytes, page-aligned, zero-filled
//   (lazily via the kernel's shared zero page).
// - `libc::munmap` is the unique release for such a region.
// - Both are thread-safe system calls.
unsafe impl PageSource for MmapSource {
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn allocate(&self) -> NonNull<u8> {
        // SAFETY: passing valid libc constants and our page size.
        let ptr = unsafe {
            libc::mmap(
                null_mut(),
                self.page_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            panic!(
                "mmap failed for {} bytes: {}",
                self.page_size,
                io::Error::last_os_error()
            );
        }
        // SAFETY: mmap guarantees non-null on success.
        unsafe { NonNull::new_unchecked(ptr as *mut u8) }
    }

    unsafe fn release(&self, ptr: NonNull<u8>) {
        // SAFETY: caller guarantees `ptr` came from `allocate` on this
        // source with `self.page_size`.
        let rc = unsafe { libc::munmap(ptr.as_ptr() as *mut _, self.page_size) };
        debug_assert_eq!(rc, 0, "munmap failed: {}", io::Error::last_os_error());
    }

    unsafe fn prefault(&self, ptr: NonNull<u8>) {
        // Touch one byte per OS page to commit physical backing. The
        // volatile write prevents the compiler from eliding the stores.
        let os_page = os_page_size();
        let base = ptr.as_ptr();
        let mut offset = 0;
        while offset < self.page_size {
            // SAFETY: `base + offset` is within the live mapping.
            unsafe {
                std::ptr::write_volatile(base.add(offset), 0);
            }
            offset += os_page;
        }
    }
}

/// Hint the kernel that `ptr`'s pages are not currently needed so it
/// can reclaim physical backing. The virtual mapping stays valid; the
/// next access to the region faults in zero-filled pages.
///
/// This is exposed as a free function rather than a method so a
/// higher-level cache layer can call it on evicted pages without
/// borrowing the pool. It is a thin wrapper around
/// `madvise(ptr, len, MADV_DONTNEED)` on Linux and
/// `MADV_FREE_REUSABLE` on macOS (the closest available equivalent).
///
/// # Safety
/// `ptr` must point to a live mmap-backed region of at least `len`
/// bytes, and `len` must be a multiple of the OS page size. After
/// this call, any reader observing the region sees zeros.
pub unsafe fn madvise_dontneed(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let advice = libc::MADV_DONTNEED;
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE_REUSABLE;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let advice = libc::MADV_DONTNEED;

    // SAFETY: caller guarantees the region is valid mmap-backed memory.
    let rc = unsafe { libc::madvise(ptr.as_ptr() as *mut _, len, advice) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn os_page_size() -> usize {
    // SAFETY: sysconf is thread-safe and takes no pointers.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size <= 0 { 4096 } else { size as usize }
}

/// Explicit huge-page-backed [`PageSource`] using
/// `mmap(MAP_HUGETLB)`. **Linux only.**
///
/// Requires kernel-reserved huge pages configured via
/// `vm.nr_hugepages` (for 2 MiB pages) or equivalent for 1 GiB pages.
/// If no huge pages are reserved, `mmap` fails with `ENOMEM` and
/// [`PagePool::allocate`](crate::PagePool::allocate) or
/// [`PagePool::prewarm`](crate::PagePool::prewarm) will panic.
///
/// Use this only when the expected working set exceeds the L2 TLB's
/// 4 KiB-page reach (≈ 6 MiB on Skylake, ≈ 16 MiB on Zen 3+) and
/// random-access latency dominates throughput. Pair with
/// [`PagePool::prewarm`](crate::PagePool::prewarm) at startup so the
/// ~1 ms per-huge-page first-touch fault cost is paid before any
/// user-visible operations run.
///
/// **Do not use Transparent Huge Pages (THP)** for this — THP is an
/// unrelated kernel feature controlled globally and is a known source
/// of tail-latency outliers. `MAP_HUGETLB` gives you predictable
/// huge-page behavior instead.
#[cfg(target_os = "linux")]
pub struct HugePageSource {
    page_size: usize,
    /// `log2(huge page size)`, encoded in the top bits of `mmap`'s
    /// flags. `0` means "use the default huge page size" (usually 2
    /// MiB).
    log2_huge_size: i32,
}

#[cfg(target_os = "linux")]
impl HugePageSource {
    /// The default 2 MiB huge-page size on x86-64 and aarch64 Linux.
    pub const SIZE_2MIB: usize = 2 * 1024 * 1024;
    /// 1 GiB huge pages (requires kernel reservation at boot).
    pub const SIZE_1GIB: usize = 1024 * 1024 * 1024;

    /// Create a new huge-page-backed source.
    ///
    /// `page_size` must be equal to the chosen huge-page size (you
    /// cannot carve 2 MiB huge pages into 4 KiB chunks at this layer).
    /// Typical values are [`SIZE_2MIB`](Self::SIZE_2MIB) or
    /// [`SIZE_1GIB`](Self::SIZE_1GIB).
    ///
    /// # Panics
    /// Panics if `page_size` is not a recognized huge-page size.
    pub fn new(page_size: usize) -> Self {
        let log2_huge_size = match page_size {
            Self::SIZE_2MIB => 21, // 2^21 = 2 MiB
            Self::SIZE_1GIB => 30, // 2^30 = 1 GiB
            _ => panic!(
                "page_size {} is not a supported huge-page size; use SIZE_2MIB or SIZE_1GIB",
                page_size
            ),
        };
        HugePageSource {
            page_size,
            log2_huge_size,
        }
    }
}

#[cfg(target_os = "linux")]
// SAFETY: same as MmapSource — mmap with MAP_HUGETLB returns a live
// huge-page-backed region; munmap releases it.
unsafe impl PageSource for HugePageSource {
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn allocate(&self) -> NonNull<u8> {
        // Encode the page size selector in the top bits of `flags`,
        // per `man mmap` / `<linux/mman.h>` (MAP_HUGE_SHIFT = 26).
        const MAP_HUGE_SHIFT: i32 = 26;
        let flags = libc::MAP_ANONYMOUS
            | libc::MAP_PRIVATE
            | libc::MAP_HUGETLB
            | (self.log2_huge_size << MAP_HUGE_SHIFT);
        // SAFETY: passing valid libc constants and our page size.
        let ptr = unsafe {
            libc::mmap(
                null_mut(),
                self.page_size,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            panic!(
                "mmap(MAP_HUGETLB) failed for {} bytes: {} — \
                 check vm.nr_hugepages and the huge-page pool",
                self.page_size,
                io::Error::last_os_error()
            );
        }
        // SAFETY: mmap guarantees non-null on success.
        unsafe { NonNull::new_unchecked(ptr as *mut u8) }
    }

    unsafe fn release(&self, ptr: NonNull<u8>) {
        // SAFETY: caller guarantees the pointer came from `allocate`.
        let rc = unsafe { libc::munmap(ptr.as_ptr() as *mut _, self.page_size) };
        debug_assert_eq!(rc, 0);
    }

    unsafe fn prefault(&self, ptr: NonNull<u8>) {
        // Huge pages fault one whole huge page at a time, so one
        // write per huge page is sufficient.
        // SAFETY: `ptr` points to a live huge page of `page_size` bytes.
        unsafe {
            std::ptr::write_volatile(ptr.as_ptr(), 0);
        }
    }
}
