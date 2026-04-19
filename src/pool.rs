use std::fmt;
use std::ptr::{self, NonNull, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::page::{Page, PageBuilder};
use crate::source::{HeapSource, PageSource};
use crate::tenant::Tenant;

/// Pool-wide counters. All values are advisory (relaxed atomics).
#[derive(Default)]
pub struct PoolStats {
    allocations_from_heap: AtomicU64,
    pages_in_use: AtomicU64,
    free_pages: AtomicU64,
    /// How many times `allocate` had to drain the cross-thread return
    /// queue because the local free list was empty.
    return_queue_drains: AtomicU64,
    /// How many pages have been committed via `prewarm`.
    prewarmed_pages: AtomicU64,
}

impl PoolStats {
    pub fn allocations_from_heap(&self) -> u64 {
        self.allocations_from_heap.load(Ordering::Relaxed)
    }

    pub fn pages_in_use(&self) -> u64 {
        self.pages_in_use.load(Ordering::Relaxed)
    }

    pub fn free_pages(&self) -> u64 {
        self.free_pages.load(Ordering::Relaxed)
    }

    pub fn return_queue_drains(&self) -> u64 {
        self.return_queue_drains.load(Ordering::Relaxed)
    }

    pub fn prewarmed_pages(&self) -> u64 {
        self.prewarmed_pages.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for PoolStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolStats")
            .field("allocations_from_heap", &self.allocations_from_heap())
            .field("pages_in_use", &self.pages_in_use())
            .field("free_pages", &self.free_pages())
            .field("return_queue_drains", &self.return_queue_drains())
            .field("prewarmed_pages", &self.prewarmed_pages())
            .finish()
    }
}

/// Shared, cross-thread portion of a [`PagePool`].
///
/// Held inside [`Arc<PoolShared>`] by each live [`Page`](crate::Page) so
/// that pages dropped on non-owning threads can return their buffers
/// via an internal `push_return` MPSC path. Also owns the
/// [`PageSource`] so that buffers outstanding on non-owning threads
/// when [`PagePool`] drops are still released by the matching source
/// when the last [`Page`](crate::Page) eventually drops.
pub struct PoolShared {
    page_size: usize,
    source: Box<dyn PageSource>,
    /// MPSC intrusive stack. Any thread may push via CAS
    /// ([`push_return`](Self::push_return)); only the pool owner
    /// drains, via [`PagePool::drain_return_queue`].
    return_head: AtomicPtr<u8>,
    stats: PoolStats,
}

impl PoolShared {
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn stats(&self) -> &PoolStats {
        &self.stats
    }

    /// Push a recycled buffer onto the cross-thread return queue.
    ///
    /// Safe to call from any thread. The first
    /// `size_of::<*mut u8>()` bytes of the buffer are clobbered with
    /// the next pointer.
    ///
    /// # Safety
    /// `ptr` must have been produced by this pool's source and must
    /// not be in use by any other [`Page`](crate::Page) or
    /// [`PageBuilder`](crate::PageBuilder).
    pub(crate) unsafe fn push_return(&self, ptr: NonNull<u8>) {
        let raw = ptr.as_ptr();
        loop {
            let head = self.return_head.load(Ordering::Relaxed);
            // SAFETY: `raw` is a unique, live `[u8; page_size]` region.
            // No other thread can observe it until the CAS below
            // succeeds.
            unsafe {
                ptr::write(raw as *mut *mut u8, head);
            }
            match self.return_head.compare_exchange_weak(
                head,
                raw,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.stats.pages_in_use.fetch_sub(1, Ordering::Relaxed);
                    self.stats.free_pages.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => continue,
            }
        }
    }
}

impl Drop for PoolShared {
    fn drop(&mut self) {
        // By the time PoolShared drops, the owning PagePool has already
        // been dropped AND every Page/PageBuilder has dropped — those
        // are the only strong references. There may still be buffers
        // on the return queue pushed by the last round of cross-thread
        // drops; release them now.
        let mut head = self.return_head.swap(null_mut(), Ordering::Acquire);
        while !head.is_null() {
            // SAFETY: the next pointer is stored in the first 8 bytes
            // of the buffer, written by `push_return` above.
            let next = unsafe { ptr::read(head as *const *mut u8) };
            // SAFETY: `head` came from this pool's source via
            // `push_return`, which means it originated from
            // `self.source.allocate`.
            unsafe { self.source.release(NonNull::new_unchecked(head)) };
            head = next;
        }
    }
}

impl fmt::Debug for PoolShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolShared")
            .field("page_size", &self.page_size)
            .field("stats", &self.stats)
            .finish()
    }
}

/// A fixed-page-size allocator designed for thread-per-core databases.
///
/// Each `PagePool` is owned by one worker thread (it is `Send` but
/// `!Sync`). Allocation takes `&mut self`, so the type system enforces
/// single-writer access to the hot path — no mutex, no atomic, just a
/// local intrusive free-list pop.
///
/// [`Page`](crate::Page)s allocated from this pool are `Send + Sync`
/// via a shared [`PoolShared`] they hold, so callers can freely share
/// sealed pages with reader threads. When a `Page` dropped on a
/// non-owning thread returns its buffer, it CAS-pushes onto the pool's
/// cross-thread return queue. The owner drains that queue lazily when
/// its local free list runs out.
///
/// # Example
///
/// ```
/// use paged_alloc::{PagePool, Tenant};
///
/// let mut pool = PagePool::new(4096);
/// // Commit 256 pages up front to avoid hitting the heap during
/// // request handling.
/// pool.prewarm(256);
///
/// let tenant = Tenant::new("t");
/// let mut builder = pool.allocate(&tenant);
/// builder.append(b"hello").unwrap();
/// let page = builder.seal();
/// assert_eq!(&page[..], b"hello");
/// ```
pub struct PagePool {
    shared: Arc<PoolShared>,
    /// Owner-only intrusive free-list head. Each node's first
    /// `size_of::<*mut u8>()` bytes store the next pointer.
    local_head: *mut u8,
}

// SAFETY: `PagePool` contains a `*mut u8` that points to buffers owned
// by this pool. The pool is the unique owner of that pointer and no
// other thread can observe it without going through `Arc<PoolShared>`.
// Sending the whole `PagePool` to another thread transfers that unique
// ownership.
unsafe impl Send for PagePool {}
// Intentionally not `Sync`: `allocate` requires `&mut self`.

impl PagePool {
    /// Creates a new pool with the given page size in bytes, backed by
    /// the global heap allocator.
    ///
    /// # Panics
    /// Panics if `page_size < size_of::<*mut u8>()`, since the intrusive
    /// free list needs room for the next pointer.
    pub fn new(page_size: usize) -> Self {
        Self::with_source(HeapSource::new(page_size))
    }

    /// Creates a new pool and immediately prewarms `num_pages` pages
    /// into the free list.
    ///
    /// Equivalent to [`PagePool::new`] followed by
    /// [`PagePool::prewarm`].
    pub fn with_capacity(page_size: usize, num_pages: usize) -> Self {
        let mut pool = Self::new(page_size);
        pool.prewarm(num_pages);
        pool
    }

    /// Creates a new pool backed by a custom [`PageSource`].
    ///
    /// Use this to plug in alternative backings (mmap, huge pages,
    /// mocked sources for testing).
    pub fn with_source<S: PageSource + 'static>(source: S) -> Self {
        let page_size = source.page_size();
        assert!(
            page_size >= std::mem::size_of::<*mut u8>(),
            "page_size must be at least {} bytes",
            std::mem::size_of::<*mut u8>()
        );
        PagePool {
            shared: Arc::new(PoolShared {
                page_size,
                source: Box::new(source),
                return_head: AtomicPtr::new(null_mut()),
                stats: PoolStats::default(),
            }),
            local_head: null_mut(),
        }
    }

    pub fn page_size(&self) -> usize {
        self.shared.page_size
    }

    pub fn stats(&self) -> &PoolStats {
        &self.shared.stats
    }

    /// A handle to the shared portion of the pool, useful for metrics
    /// scrapers that want to observe stats from off-core threads.
    pub fn shared(&self) -> &Arc<PoolShared> {
        &self.shared
    }

    /// Count of pages currently sitting on the owner-local free list.
    /// Does not include pages still pending on the cross-thread return
    /// queue. For the combined count see
    /// [`PoolStats::free_pages`].
    pub fn local_free_pages(&self) -> usize {
        let mut count = 0;
        let mut head = self.local_head;
        while !head.is_null() {
            count += 1;
            // SAFETY: head is a valid buffer on our local list; its
            // first 8 bytes hold the next pointer.
            head = unsafe { ptr::read(head as *const *mut u8) };
        }
        count
    }

    /// Commits `num_pages` pages into the free list up front, so the
    /// next `num_pages` allocations do not hit the source's cold path.
    ///
    /// For the default heap-backed source this is equivalent to
    /// allocating and zero-initializing `num_pages * page_size` bytes.
    /// For lazy-backed sources (future `MmapSource`, huge pages) this
    /// also calls [`PageSource::prefault`] on each page so the kernel
    /// commits physical backing synchronously at startup, avoiding
    /// first-touch faults on the latency-sensitive path.
    ///
    /// This is the recommended way to warm a pool before admitting
    /// user traffic. Call it once during startup after configuring
    /// the budget for a worker's memtable or cache.
    ///
    /// # Example
    ///
    /// ```
    /// use paged_alloc::PagePool;
    ///
    /// // Reserve a 4 MiB budget (1024 × 4 KiB pages) before serving
    /// // any user requests.
    /// let mut pool = PagePool::new(4096);
    /// pool.prewarm(1024);
    /// assert_eq!(pool.stats().free_pages(), 1024);
    /// assert_eq!(pool.stats().prewarmed_pages(), 1024);
    /// ```
    pub fn prewarm(&mut self, num_pages: usize) {
        let source = &self.shared.source;
        for _ in 0..num_pages {
            let ptr = source.allocate();
            // SAFETY: `ptr` was just returned by `source.allocate()`,
            // so it is a live buffer owned exclusively by this pool.
            unsafe {
                source.prefault(ptr);
            }
            // Push onto local free list. SAFETY: `ptr` is unique and
            // at least `page_size` ≥ 8 bytes, so writing the next
            // pointer into its first 8 bytes is sound.
            unsafe {
                ptr::write(ptr.as_ptr() as *mut *mut u8, self.local_head);
            }
            self.local_head = ptr.as_ptr();
        }
        let n = num_pages as u64;
        self.shared
            .stats
            .allocations_from_heap
            .fetch_add(n, Ordering::Relaxed);
        self.shared.stats.free_pages.fetch_add(n, Ordering::Relaxed);
        self.shared
            .stats
            .prewarmed_pages
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Allocates a page attributed to `tenant`.
    ///
    /// Hot path: pops from the local intrusive free list (no atomics).
    /// If that list is empty, drains the cross-thread return queue via
    /// one atomic swap. If the return queue is also empty, allocates a
    /// new buffer from the backing [`PageSource`].
    pub fn allocate(&mut self, tenant: &Tenant) -> PageBuilder {
        let (buf, shared) = self.allocate_raw_page();
        tenant.stats().record_allocate(shared.page_size as u64);
        PageBuilder::new(buf, shared, tenant.stats_arc())
    }

    /// Allocates a fresh page, copies `data` into it, seals, and
    /// returns the [`Page`]. The resulting page's logical length is
    /// `data.len()`.
    ///
    /// # Panics
    /// Panics if `data.len() > page_size()` — a page cannot hold
    /// more bytes than its configured size.
    ///
    /// # Example
    ///
    /// ```
    /// use paged_alloc::{PagePool, Tenant};
    ///
    /// let mut pool = PagePool::new(4096);
    /// let tenant = Tenant::new("t");
    /// let page = pool.alloc_from(&tenant, b"hello");
    /// assert_eq!(&page[..], b"hello");
    /// ```
    pub fn alloc_from(&mut self, tenant: &Tenant, data: &[u8]) -> Page {
        assert!(
            data.len() <= self.shared.page_size,
            "data length {} exceeds page_size {}",
            data.len(),
            self.shared.page_size
        );
        let mut b = self.allocate(tenant);
        if !data.is_empty() {
            b.as_mut_slice()[..data.len()].copy_from_slice(data);
            b.set_len(data.len());
        }
        b.seal()
    }

    /// Allocates a fresh page, runs `init` on the full mutable
    /// buffer, seals with `len == page_size()`, and returns the
    /// resulting [`Page`].
    ///
    /// # Panic safety
    /// If `init` panics, the underlying `PageBuilder` drops,
    /// recycling its buffer and releasing the tenant's accounting.
    ///
    /// # Example
    ///
    /// ```
    /// use paged_alloc::{PagePool, Tenant};
    ///
    /// let mut pool = PagePool::new(4096);
    /// let tenant = Tenant::new("t");
    /// let page = pool.alloc_with(&tenant, |buf| {
    ///     buf[..6].copy_from_slice(b"header");
    ///     buf[6..9].copy_from_slice(&[0, 0, 0]);
    /// });
    /// assert_eq!(&page[..6], b"header");
    /// assert_eq!(page.len(), 4096);
    /// ```
    pub fn alloc_with<F>(&mut self, tenant: &Tenant, init: F) -> Page
    where
        F: FnOnce(&mut [u8]),
    {
        let mut b = self.allocate(tenant);
        init(b.as_mut_slice());
        let cap = b.capacity();
        b.set_len(cap);
        b.seal()
    }

    /// Allocates a fresh page, runs `init` on the full mutable
    /// buffer, and seals the result with the logical length returned
    /// by `init`. Use this when the record's size is determined by
    /// its contents.
    ///
    /// # Panics
    /// Panics if `init` returns a value greater than `page_size()`.
    ///
    /// # Panic safety
    /// If `init` panics, the underlying `PageBuilder` drops,
    /// recycling its buffer and releasing the tenant's accounting.
    pub fn alloc_with_len<F>(&mut self, tenant: &Tenant, init: F) -> Page
    where
        F: FnOnce(&mut [u8]) -> usize,
    {
        let mut b = self.allocate(tenant);
        let len = init(b.as_mut_slice());
        b.set_len(len);
        b.seal()
    }

    /// Allocates one raw page without wrapping in [`PageBuilder`] and
    /// without touching tenant accounting. Used by higher-level layers
    /// (e.g. `ChunkPool`) that account for allocations at sub-page
    /// granularity and wrap the buffer in their own lifecycle type.
    ///
    /// The returned `NonNull<u8>` points to a `page_size`-byte region
    /// produced by this pool's [`PageSource`]. The caller becomes
    /// responsible for eventually returning the buffer by calling
    /// [`PoolShared::push_return`] on the accompanying `Arc<PoolShared>`.
    pub(crate) fn allocate_raw_page(&mut self) -> (NonNull<u8>, Arc<PoolShared>) {
        let buf = self
            .pop_local()
            .or_else(|| self.drain_return_queue())
            .unwrap_or_else(|| self.allocate_fresh());
        self.shared.stats.pages_in_use.fetch_add(1, Ordering::Relaxed);
        (buf, self.shared.clone())
    }

    fn pop_local(&mut self) -> Option<NonNull<u8>> {
        if self.local_head.is_null() {
            return None;
        }
        let head = self.local_head;
        // SAFETY: `head` is a buffer this pool previously pushed onto
        // the local list. Its first 8 bytes are a valid next pointer
        // we wrote earlier. No other thread can observe it.
        let next = unsafe { ptr::read(head as *const *mut u8) };
        self.local_head = next;
        self.shared.stats.free_pages.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: non-null by the check above.
        Some(unsafe { NonNull::new_unchecked(head) })
    }

    #[cold]
    fn drain_return_queue(&mut self) -> Option<NonNull<u8>> {
        let head = self.shared.return_head.swap(null_mut(), Ordering::Acquire);
        if head.is_null() {
            return None;
        }
        self.shared
            .stats
            .return_queue_drains
            .fetch_add(1, Ordering::Relaxed);
        self.local_head = head;
        self.pop_local()
    }

    #[cold]
    fn allocate_fresh(&mut self) -> NonNull<u8> {
        self.shared
            .stats
            .allocations_from_heap
            .fetch_add(1, Ordering::Relaxed);
        self.shared.source.allocate()
    }
}

impl Drop for PagePool {
    fn drop(&mut self) {
        // Walk and release the owner-local intrusive list. The return
        // queue is released by `PoolShared::drop` once the last Page
        // clone drops, ensuring we don't release buffers still held by
        // live pages on other threads.
        while !self.local_head.is_null() {
            let head = self.local_head;
            // SAFETY: head is a valid buffer this pool produced.
            let next = unsafe { ptr::read(head as *const *mut u8) };
            self.local_head = next;
            // SAFETY: `head` came from `self.shared.source.allocate`.
            unsafe {
                self.shared
                    .source
                    .release(NonNull::new_unchecked(head));
            }
        }
    }
}

impl fmt::Debug for PagePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PagePool")
            .field("shared", &*self.shared)
            .field("local_free_pages", &self.local_free_pages())
            .finish()
    }
}
