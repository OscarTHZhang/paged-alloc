use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;

/// The backing-memory strategy for a [`PagePool`](crate::PagePool).
///
/// A `PageSource` is responsible for producing and releasing fixed-size
/// byte pages. The pool calls `allocate` on its cold path when the
/// free list is empty, and `release` on pool shutdown for every buffer
/// still outstanding. `prefault` is invoked by
/// [`PagePool::prewarm`](crate::PagePool::prewarm) to commit physical
/// backing before user-visible operations start.
///
/// The current crate ships one implementation — [`HeapSource`] — which
/// allocates via the global allocator. Future sources (`MmapSource`,
/// `HugePageSource`, `VirtualAllocSource` on Windows) can be added
/// without changing the public [`PagePool`](crate::PagePool) API.
///
/// # Safety
///
/// Implementors must uphold:
///
/// 1. `allocate` returns a pointer to a live, writable region of at
///    least `page_size()` bytes, aligned to at least
///    `align_of::<*mut u8>()`. The contents must be zero-initialized.
/// 2. A pointer returned by `allocate` is valid until it is passed to
///    `release` on the same source instance.
/// 3. `release` is only called with pointers produced by `allocate` on
///    the same source, and each pointer is released at most once.
/// 4. `prefault` may be called any number of times on any live pointer;
///    it must not free or invalidate the pointer.
/// 5. The source is `Send + Sync` and its methods are safe to call from
///    any thread.
pub unsafe trait PageSource: Send + Sync {
    /// The size in bytes of each page this source produces.
    fn page_size(&self) -> usize;

    /// Allocate one zero-initialized page.
    ///
    /// Panics or aborts on allocation failure via
    /// [`std::alloc::handle_alloc_error`] (or equivalent).
    fn allocate(&self) -> NonNull<u8>;

    /// Release a page previously returned by [`allocate`](Self::allocate).
    ///
    /// # Safety
    /// - `ptr` must have been returned by a prior call to `allocate` on
    ///   this same source and must not have been released already.
    /// - After this call, `ptr` must not be accessed by any thread.
    unsafe fn release(&self, ptr: NonNull<u8>);

    /// Touch every operating-system page within the allocation so that
    /// physical backing is committed before user code runs.
    ///
    /// For heap-allocator-backed sources this is a no-op: the global
    /// allocator has already committed the memory and zeroed it. For
    /// lazy-backed sources (anonymous `mmap`, huge pages) an
    /// implementation should write one byte per OS page to trigger the
    /// kernel's zero-fill fault path synchronously.
    ///
    /// # Safety
    /// `ptr` must be a live pointer returned by `allocate` on this source.
    unsafe fn prefault(&self, _ptr: NonNull<u8>) {}
}

/// Heap-backed [`PageSource`] using [`std::alloc::alloc_zeroed`].
///
/// This is the default source for [`PagePool::new`](crate::PagePool::new).
/// Pages are allocated from the process-global allocator and returned
/// to it on pool shutdown.
pub struct HeapSource {
    page_size: usize,
    layout: Layout,
}

impl HeapSource {
    /// Create a heap source producing `page_size`-byte pages.
    ///
    /// # Panics
    /// Panics if `page_size` is 0 or not representable as a [`Layout`].
    pub fn new(page_size: usize) -> Self {
        assert!(
            page_size >= std::mem::size_of::<*mut u8>(),
            "page_size must be at least {} bytes",
            std::mem::size_of::<*mut u8>()
        );
        // Align to at least pointer alignment so the intrusive
        // free-list next pointer stays well-aligned.
        let align = std::mem::align_of::<*mut u8>().max(8);
        let layout = Layout::from_size_align(page_size, align).expect("valid layout");
        HeapSource { page_size, layout }
    }
}

// SAFETY:
// - `alloc_zeroed` returns a live, zero-initialized `page_size`-byte
//   region, aligned to `layout.align()` ≥ 8.
// - The allocator is process-global and inherently `Sync`.
// - `dealloc` is only called from `release`, which the trait contract
//   guarantees is paired 1:1 with `allocate`.
// - `prefault` is the default no-op, which is correct for
//   `alloc_zeroed` since it already touches every page.
unsafe impl PageSource for HeapSource {
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn allocate(&self) -> NonNull<u8> {
        // SAFETY: layout has non-zero size (checked in `new`).
        let ptr = unsafe { alloc_zeroed(self.layout) };
        NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(self.layout))
    }

    unsafe fn release(&self, ptr: NonNull<u8>) {
        // SAFETY: caller guarantees `ptr` came from `allocate` on this
        // source with `self.layout`, so passing the same layout to
        // `dealloc` is sound.
        unsafe { dealloc(ptr.as_ptr(), self.layout) }
    }
}
