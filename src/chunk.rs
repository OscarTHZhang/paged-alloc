//! Variable-size byte allocator layered on top of [`PagePool`].
//!
//! `ChunkPool` hides the pool's fixed page size from callers. A user
//! requests an exact number of bytes and receives a [`ChunkBuilder`]
//! they can fill and seal into a [`Chunk`] — an immutable, cloneable,
//! cross-thread-safe handle that derefs to `&[u8]`. Internally the
//! pool dispatches each allocation into one of three size classes:
//!
//! - **Small** (`size ≤ page_size / 2`): packed alongside other small
//!   allocations inside a shared [`PackedPage`]. Zero heap allocation
//!   beyond the occasional fresh page pull from the underlying
//!   `PagePool`; one `Arc<ChunkInner>` allocation per chunk for the
//!   ref-counted handle.
//! - **Dedicated** (`page_size / 2 < size ≤ page_size`): takes a full
//!   fresh page from the underlying `PagePool`. No packing — the page
//!   is retired when the chunk drops.
//! - **Oversized** (`size > page_size`): bypasses the fixed-size pool
//!   entirely and calls [`std::alloc::alloc_zeroed`] with an exact-size
//!   layout. Released directly with `dealloc` when the chunk drops.
//!
//! Regardless of path, [`Chunk::as_slice`] always returns a single
//! contiguous `&[u8]`. The caller never sees the boundary between
//! classes or between packed pages.
//!
//! # Safety argument (packed path)
//!
//! Multiple [`Chunk`] handles may reference disjoint ranges of the same
//! underlying [`PackedPage`] buffer. The `ChunkPool` maintains a
//! monotonic `write_offset` cursor on the currently-open page; every
//! new allocation claims `[cursor, cursor+size+padding)` and advances
//! the cursor past it. A chunk's range is therefore always disjoint
//! from every other chunk's range on the same page and from any range
//! that may be written later. Reading through one chunk and writing
//! to a later chunk on the same buffer are non-aliasing accesses, so
//! the manual `Send`/`Sync` impls on `PackedPage` are sound.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::fmt;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::pool::{PagePool, PoolShared, PoolStats};
use crate::source::PageSource;
use crate::tenant::{Tenant, TenantStats};

/// Returned when an append would overflow a chunk's capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkFull {
    pub capacity: usize,
    pub len: usize,
    pub attempted: usize,
}

impl fmt::Display for ChunkFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "chunk full: attempted to append {} bytes to chunk with {}/{} used",
            self.attempted, self.len, self.capacity
        )
    }
}

impl std::error::Error for ChunkFull {}

// =========================================================================
// Internal: PackedPage and Release
// =========================================================================

/// How a [`PackedPage`]'s backing memory returns to its origin.
enum Release {
    /// Buffer came from the underlying `PagePool` and is returned via
    /// the MPSC return queue when the last chunk drops.
    Pool(Arc<PoolShared>),
    /// Buffer came directly from the global allocator (oversized path)
    /// and is freed via `std::alloc::dealloc` with this layout.
    Heap(Layout),
}

/// A buffer that may back one or more [`Chunk`] handles.
///
/// Held inside an `Arc` by every chunk that references it; buffer is
/// released to its origin (pool or heap) when the last `Arc` drops.
struct PackedPage {
    buf: NonNull<u8>,
    release: Release,
}

// SAFETY: `buf` is a unique owning pointer to a live region. Reads and
// writes through it are range-disjoint per the module-level safety
// argument; no interior mutability is exposed.
unsafe impl Send for PackedPage {}
unsafe impl Sync for PackedPage {}

impl Drop for PackedPage {
    fn drop(&mut self) {
        match &self.release {
            // SAFETY: `buf` came from this pool's source via
            // `PagePool::allocate_raw_page`; `push_return` is the
            // matching release.
            Release::Pool(shared) => unsafe {
                shared.push_return(self.buf);
            },
            // SAFETY: `buf` came from `alloc_zeroed` with this layout;
            // `dealloc` with the same layout is the matching release.
            Release::Heap(layout) => unsafe {
                dealloc(self.buf.as_ptr(), *layout);
            },
        }
    }
}

/// The page currently open for packed allocations. Owned exclusively
/// by the [`ChunkPool`] — never shared.
struct OpenPage {
    shared: Arc<PackedPage>,
    write_offset: usize,
}

// =========================================================================
// Pool-level chunk stats
// =========================================================================

/// Chunk-level counters at the pool scope.
///
/// Complements [`PoolStats`] (which reports page-level activity).
/// The pool owns this via `Arc`; each [`Chunk`] carries a clone so
/// its `Drop` can decrement `chunks_in_use` without needing a
/// reference back to the pool.
#[derive(Default)]
pub struct ChunkPoolStats {
    total_chunks: AtomicU64,
    chunks_in_use: AtomicU64,
    packed_allocations: AtomicU64,
    dedicated_allocations: AtomicU64,
    oversized_allocations: AtomicU64,
    oversized_bytes_in_use: AtomicU64,
}

impl ChunkPoolStats {
    pub fn total_chunks(&self) -> u64 {
        self.total_chunks.load(Ordering::Relaxed)
    }
    pub fn chunks_in_use(&self) -> u64 {
        self.chunks_in_use.load(Ordering::Relaxed)
    }
    pub fn packed_allocations(&self) -> u64 {
        self.packed_allocations.load(Ordering::Relaxed)
    }
    pub fn dedicated_allocations(&self) -> u64 {
        self.dedicated_allocations.load(Ordering::Relaxed)
    }
    pub fn oversized_allocations(&self) -> u64 {
        self.oversized_allocations.load(Ordering::Relaxed)
    }
    pub fn oversized_bytes_in_use(&self) -> u64 {
        self.oversized_bytes_in_use.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for ChunkPoolStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkPoolStats")
            .field("total_chunks", &self.total_chunks())
            .field("chunks_in_use", &self.chunks_in_use())
            .field("packed_allocations", &self.packed_allocations())
            .field("dedicated_allocations", &self.dedicated_allocations())
            .field("oversized_allocations", &self.oversized_allocations())
            .field("oversized_bytes_in_use", &self.oversized_bytes_in_use())
            .finish()
    }
}

// =========================================================================
// ChunkPool
// =========================================================================

/// A variable-size byte allocator backed by a fixed-size [`PagePool`].
///
/// Like `PagePool`, one `ChunkPool` is owned by one worker thread
/// (`Send + !Sync`); allocation takes `&mut self`. Unlike `PagePool`,
/// callers specify the exact byte size they want — the underlying page
/// structure is hidden.
///
/// # Example
///
/// ```
/// use paged_alloc::{ChunkPool, Tenant};
///
/// let mut pool = ChunkPool::new();
/// let tenant = Tenant::new("t");
/// let mut b = pool.allocate(&tenant, 11);
/// b.append(b"hello ").unwrap();
/// b.append(b"world").unwrap();
/// let chunk = b.seal();
/// assert_eq!(&chunk[..], b"hello world");
/// ```
pub struct ChunkPool {
    pages: PagePool,
    current: Option<OpenPage>,
    threshold: usize,
    stats: Arc<ChunkPoolStats>,
}

impl ChunkPool {
    /// Default underlying page size (16 KiB). Matches the standard used
    /// elsewhere in the crate's benchmarks.
    pub const DEFAULT_PAGE_SIZE: usize = 16 * 1024;

    /// Default alignment applied to allocations when the caller does
    /// not explicitly request one. Equal to pointer alignment.
    pub const DEFAULT_ALIGN: usize = std::mem::align_of::<*mut u8>();

    /// Creates a pool with the default page size
    /// ([`DEFAULT_PAGE_SIZE`](Self::DEFAULT_PAGE_SIZE)) backed by the
    /// heap allocator.
    pub fn new() -> Self {
        Self::with_page_size(Self::DEFAULT_PAGE_SIZE)
    }

    /// Creates a pool with a custom underlying page size.
    ///
    /// # Panics
    /// Panics if `page_size` is too small to hold the intrusive free
    /// list pointer (see [`PagePool::new`]).
    pub fn with_page_size(page_size: usize) -> Self {
        Self::from_pool(PagePool::new(page_size))
    }

    /// Creates a pool and immediately prewarms `num_pages` of the
    /// underlying fixed-size page pool.
    pub fn with_capacity(page_size: usize, num_pages: usize) -> Self {
        Self::from_pool(PagePool::with_capacity(page_size, num_pages))
    }

    /// Creates a pool backed by a custom [`PageSource`].
    pub fn with_source<S: PageSource + 'static>(source: S) -> Self {
        Self::from_pool(PagePool::with_source(source))
    }

    fn from_pool(pages: PagePool) -> Self {
        let threshold = pages.page_size() / 2;
        ChunkPool {
            pages,
            current: None,
            threshold,
            stats: Arc::new(ChunkPoolStats::default()),
        }
    }

    /// Underlying fixed page size.
    pub fn page_size(&self) -> usize {
        self.pages.page_size()
    }

    /// Split threshold between the packed and dedicated paths
    /// (= `page_size / 2`).
    pub fn pack_threshold(&self) -> usize {
        self.threshold
    }

    /// Chunk-level statistics for this pool.
    pub fn stats(&self) -> &ChunkPoolStats {
        &self.stats
    }

    /// Page-level statistics from the underlying [`PagePool`].
    pub fn page_stats(&self) -> &PoolStats {
        self.pages.stats()
    }

    /// Shared handle to the underlying pool (for metrics scrapers).
    pub fn shared(&self) -> &Arc<PoolShared> {
        self.pages.shared()
    }

    /// Prewarms the underlying `PagePool`'s free list.
    ///
    /// See [`PagePool::prewarm`] for details. This does not
    /// pre-allocate any chunks; it just populates the fixed-size page
    /// cache so subsequent chunk allocations avoid the source's cold
    /// path.
    pub fn prewarm(&mut self, num_pages: usize) {
        self.pages.prewarm(num_pages);
    }

    /// Allocates a chunk of exactly `size` bytes, with
    /// [`DEFAULT_ALIGN`](Self::DEFAULT_ALIGN) alignment.
    pub fn allocate(&mut self, tenant: &Tenant, size: usize) -> ChunkBuilder {
        self.allocate_aligned(tenant, size, Self::DEFAULT_ALIGN)
    }

    /// Allocates a chunk of exactly `size` bytes, aligned to `align`
    /// (must be a power of two).
    ///
    /// On the packed path, requesting higher-than-default alignment
    /// may waste a few bytes in the current open page to reach an
    /// aligned offset.
    pub fn allocate_aligned(
        &mut self,
        tenant: &Tenant,
        size: usize,
        align: usize,
    ) -> ChunkBuilder {
        assert!(
            align.is_power_of_two(),
            "align must be a power of two, got {align}"
        );
        if size == 0 {
            return self.allocate_zero(tenant);
        }
        let page_size = self.pages.page_size();
        if size > page_size {
            self.allocate_oversized(tenant, size, align)
        } else if size > self.threshold {
            self.allocate_dedicated(tenant, size)
        } else {
            self.allocate_packed(tenant, size, align)
        }
    }

    /// Convenience: allocate, copy bytes in, and seal in one call.
    pub fn alloc_from(&mut self, tenant: &Tenant, data: &[u8]) -> Chunk {
        let mut b = self.allocate(tenant, data.len());
        if !data.is_empty() {
            b.as_mut_slice().copy_from_slice(data);
            b.set_len(data.len());
        }
        b.seal()
    }

    /// Allocates `size` bytes, runs `init` on the writable buffer,
    /// seals, and returns the resulting [`Chunk`]. The chunk's
    /// logical length is always `size`.
    ///
    /// This is a one-call shorthand for the common pattern:
    /// `allocate → as_mut_slice / fill → set_len(size) → seal`.
    ///
    /// # Panic safety
    /// If `init` panics, the underlying `ChunkBuilder` is dropped,
    /// which releases the tenant's accounting for the attempted
    /// allocation. Subsequent allocations continue to work.
    ///
    /// # Example
    ///
    /// ```
    /// use paged_alloc::{ChunkPool, Tenant};
    ///
    /// let mut pool = ChunkPool::new();
    /// let tenant = Tenant::new("t");
    /// let chunk = pool.alloc_with(&tenant, 4, |buf| {
    ///     buf[0] = 0xDE;
    ///     buf[1] = 0xAD;
    ///     buf[2] = 0xBE;
    ///     buf[3] = 0xEF;
    /// });
    /// assert_eq!(&chunk[..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    /// ```
    pub fn alloc_with<F>(&mut self, tenant: &Tenant, size: usize, init: F) -> Chunk
    where
        F: FnOnce(&mut [u8]),
    {
        let mut b = self.allocate(tenant, size);
        init(b.as_mut_slice());
        b.set_len(size);
        b.seal()
    }

    /// Allocates `capacity` bytes, runs `init` on the writable
    /// buffer, and seals the result with the logical length returned
    /// by `init`. Use this when the final length of the record is
    /// determined while filling it (e.g. serialization whose size
    /// depends on content).
    ///
    /// # Panics
    /// Panics if `init` returns a value greater than `capacity`.
    ///
    /// # Panic safety
    /// If `init` panics, the underlying `ChunkBuilder` is dropped
    /// and the tenant's accounting for the attempted allocation is
    /// released.
    ///
    /// # Example
    ///
    /// ```
    /// use paged_alloc::{ChunkPool, Tenant};
    ///
    /// let mut pool = ChunkPool::new();
    /// let tenant = Tenant::new("t");
    /// // Reserve up to 32 bytes; serialize "hi!" and return its length.
    /// let chunk = pool.alloc_with_len(&tenant, 32, |buf| {
    ///     let msg = b"hi!";
    ///     buf[..msg.len()].copy_from_slice(msg);
    ///     msg.len()
    /// });
    /// assert_eq!(&chunk[..], b"hi!");
    /// assert_eq!(chunk.len(), 3);
    /// assert_eq!(chunk.capacity(), 32);
    /// ```
    pub fn alloc_with_len<F>(&mut self, tenant: &Tenant, capacity: usize, init: F) -> Chunk
    where
        F: FnOnce(&mut [u8]) -> usize,
    {
        let mut b = self.allocate(tenant, capacity);
        let len = init(b.as_mut_slice());
        b.set_len(len);
        b.seal()
    }

    // ---------------------------------------------------------------
    // Dispatch paths
    // ---------------------------------------------------------------

    fn allocate_zero(&mut self, tenant: &Tenant) -> ChunkBuilder {
        tenant.stats().record_chunk_allocate(0);
        self.stats.total_chunks.fetch_add(1, Ordering::Relaxed);
        self.stats.chunks_in_use.fetch_add(1, Ordering::Relaxed);
        ChunkBuilder {
            packed: None,
            tenant: Some(tenant.stats_arc()),
            stats: self.stats.clone(),
            oversized_bytes: 0,
            offset: 0,
            capacity: 0,
            len: 0,
        }
    }

    fn allocate_packed(
        &mut self,
        tenant: &Tenant,
        size: usize,
        align: usize,
    ) -> ChunkBuilder {
        let page_size = self.pages.page_size();

        // Find or open a page that has room for our aligned range.
        let (shared, offset) = loop {
            if let Some(open) = self.current.as_ref() {
                let aligned = align_up(open.write_offset, align);
                if aligned.saturating_add(size) <= page_size {
                    let open = self.current.as_mut().unwrap();
                    open.write_offset = aligned + size;
                    break (open.shared.clone(), aligned);
                }
            }
            // Retire the current page (dropping our Arc handle to it —
            // any chunks already issued keep it alive) and open a new
            // one.
            self.current = None;
            self.open_new_page();
        };

        tenant.stats().record_chunk_allocate(size as u64);
        self.stats.total_chunks.fetch_add(1, Ordering::Relaxed);
        self.stats.chunks_in_use.fetch_add(1, Ordering::Relaxed);
        self.stats.packed_allocations.fetch_add(1, Ordering::Relaxed);

        ChunkBuilder {
            packed: Some(shared),
            tenant: Some(tenant.stats_arc()),
            stats: self.stats.clone(),
            oversized_bytes: 0,
            offset: offset as u32,
            capacity: size as u32,
            len: 0,
        }
    }

    fn allocate_dedicated(&mut self, tenant: &Tenant, size: usize) -> ChunkBuilder {
        let (buf, pool_shared) = self.pages.allocate_raw_page();
        let packed = Arc::new(PackedPage {
            buf,
            release: Release::Pool(pool_shared),
        });

        tenant.stats().record_chunk_allocate(size as u64);
        self.stats.total_chunks.fetch_add(1, Ordering::Relaxed);
        self.stats.chunks_in_use.fetch_add(1, Ordering::Relaxed);
        self.stats
            .dedicated_allocations
            .fetch_add(1, Ordering::Relaxed);

        ChunkBuilder {
            packed: Some(packed),
            tenant: Some(tenant.stats_arc()),
            stats: self.stats.clone(),
            oversized_bytes: 0,
            offset: 0,
            capacity: size as u32,
            len: 0,
        }
    }

    fn allocate_oversized(
        &mut self,
        tenant: &Tenant,
        size: usize,
        align: usize,
    ) -> ChunkBuilder {
        let align = align.max(Self::DEFAULT_ALIGN);
        let layout = Layout::from_size_align(size, align)
            .expect("oversized layout from valid size + power-of-two align");
        // SAFETY: layout has non-zero size (size > page_size > 0).
        let raw = unsafe { alloc_zeroed(layout) };
        let buf = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));

        let packed = Arc::new(PackedPage {
            buf,
            release: Release::Heap(layout),
        });

        tenant.stats().record_chunk_allocate(size as u64);
        self.stats.total_chunks.fetch_add(1, Ordering::Relaxed);
        self.stats.chunks_in_use.fetch_add(1, Ordering::Relaxed);
        self.stats
            .oversized_allocations
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .oversized_bytes_in_use
            .fetch_add(size as u64, Ordering::Relaxed);

        ChunkBuilder {
            packed: Some(packed),
            tenant: Some(tenant.stats_arc()),
            stats: self.stats.clone(),
            oversized_bytes: size as u64,
            offset: 0,
            capacity: size as u32,
            len: 0,
        }
    }

    fn open_new_page(&mut self) {
        let (buf, pool_shared) = self.pages.allocate_raw_page();
        let packed = Arc::new(PackedPage {
            buf,
            release: Release::Pool(pool_shared),
        });
        self.current = Some(OpenPage {
            shared: packed,
            write_offset: 0,
        });
    }
}

impl Default for ChunkPool {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ChunkPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkPool")
            .field("page_size", &self.page_size())
            .field("threshold", &self.threshold)
            .field("chunk_stats", &*self.stats)
            .field("page_stats", self.page_stats())
            .finish()
    }
}

fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

// =========================================================================
// ChunkBuilder
// =========================================================================

/// An exclusive, writable handle to a freshly allocated chunk.
///
/// Callers write via [`append`](Self::append), direct slice access
/// from [`as_mut_slice`](Self::as_mut_slice), or arbitrary-offset
/// writes paired with [`set_len`](Self::set_len). Consuming the
/// builder via [`seal`](Self::seal) produces an immutable [`Chunk`].
/// Dropping an unsealed builder releases the tenant's accounting for
/// the requested bytes; the underlying buffer space is not reclaimed
/// (it's been claimed on the packed page's cursor, like any bump
/// allocation).
#[must_use = "a ChunkBuilder represents a live allocation; call \
              seal() to publish it, or drop it explicitly to release \
              the tenant accounting"]
pub struct ChunkBuilder {
    /// `None` only for zero-size builders.
    packed: Option<Arc<PackedPage>>,
    /// `None` after seal has moved it into the Chunk.
    tenant: Option<Arc<TenantStats>>,
    stats: Arc<ChunkPoolStats>,
    /// For oversized allocations we track bytes to decrement on drop
    /// the oversized_bytes_in_use counter. Zero for non-oversized.
    oversized_bytes: u64,
    offset: u32,
    capacity: u32,
    len: u32,
}

// SAFETY: `packed` is an Arc (Send+Sync) and the raw pointer it wraps
// is only accessed via disjoint ranges enforced by the module-level
// invariant. All other fields are Send.
unsafe impl Send for ChunkBuilder {}

impl ChunkBuilder {
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn remaining(&self) -> usize {
        (self.capacity - self.len) as usize
    }

    pub fn as_slice(&self) -> &[u8] {
        let cap = self.capacity();
        let len = self.len();
        match self.packed.as_ref() {
            Some(page) => {
                // SAFETY: range [offset, offset+cap) is this builder's
                // exclusive slice of the underlying buffer; `len ≤ cap`.
                unsafe {
                    let base = page.buf.as_ptr().add(self.offset as usize);
                    std::slice::from_raw_parts(base, len)
                }
            }
            None => {
                debug_assert_eq!(cap, 0);
                &[]
            }
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let cap = self.capacity();
        match self.packed.as_ref() {
            Some(page) => {
                // SAFETY: exclusive builder access to [offset, offset+cap).
                unsafe {
                    let base = page.buf.as_ptr().add(self.offset as usize);
                    std::slice::from_raw_parts_mut(base, cap)
                }
            }
            None => {
                debug_assert_eq!(cap, 0);
                &mut []
            }
        }
    }

    /// Sets the logical content length.
    ///
    /// # Panics
    /// Panics if `len` exceeds [`capacity`](Self::capacity).
    pub fn set_len(&mut self, len: usize) {
        assert!(
            len <= self.capacity(),
            "len {} > capacity {}",
            len,
            self.capacity()
        );
        self.len = len as u32;
    }

    pub fn append(&mut self, data: &[u8]) -> Result<(), ChunkFull> {
        let cap = self.capacity();
        let cur = self.len();
        if cur + data.len() > cap {
            return Err(ChunkFull {
                capacity: cap,
                len: cur,
                attempted: data.len(),
            });
        }
        if !data.is_empty() {
            let buf = self.as_mut_slice();
            buf[cur..cur + data.len()].copy_from_slice(data);
            self.len += data.len() as u32;
        }
        Ok(())
    }

    /// Seals the builder into an immutable, shareable [`Chunk`].
    pub fn seal(mut self) -> Chunk {
        let tenant = self.tenant.take().expect("live builder");
        Chunk {
            inner: Arc::new(ChunkInner {
                packed: self.packed.clone(),
                tenant,
                stats: self.stats.clone(),
                oversized_bytes: self.oversized_bytes,
                offset: self.offset,
                capacity: self.capacity,
                length: self.len,
            }),
        }
    }
}

impl Drop for ChunkBuilder {
    fn drop(&mut self) {
        if let Some(tenant) = self.tenant.take() {
            // Builder was dropped without seal. Release the tenant's
            // accounting for the allocated capacity.
            tenant.record_chunk_release(self.capacity as u64);
            self.stats.chunks_in_use.fetch_sub(1, Ordering::Relaxed);
            if self.oversized_bytes > 0 {
                self.stats
                    .oversized_bytes_in_use
                    .fetch_sub(self.oversized_bytes, Ordering::Relaxed);
            }
        }
    }
}

impl fmt::Debug for ChunkBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkBuilder")
            .field("capacity", &self.capacity())
            .field("len", &self.len)
            .finish()
    }
}

// =========================================================================
// Chunk
// =========================================================================

/// An immutable, reference-counted handle to a sealed chunk.
///
/// Cloning is cheap (an `Arc` bump). `Chunk` is `Send + Sync`, so it
/// can be shared freely with reader threads. When the last clone
/// drops — on any thread — the tenant's in-use counters decrement and
/// the `Arc<PackedPage>` drops; if that was the last reference to the
/// underlying buffer, the buffer returns to the pool (or heap, for
/// oversized).
#[must_use = "a Chunk holds allocated memory; drop it explicitly to \
              release, or bind it to use its contents"]
pub struct Chunk {
    inner: Arc<ChunkInner>,
}

impl Chunk {
    pub fn len(&self) -> usize {
        self.inner.length as usize
    }

    pub fn is_empty(&self) -> bool {
        self.inner.length == 0
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity as usize
    }

    pub fn as_slice(&self) -> &[u8] {
        let len = self.len();
        match self.inner.packed.as_ref() {
            Some(page) => {
                // SAFETY: range [offset, offset+length) is finalized
                // and disjoint from any concurrent writer (invariant).
                unsafe {
                    let base = page.buf.as_ptr().add(self.inner.offset as usize);
                    std::slice::from_raw_parts(base, len)
                }
            }
            None => {
                debug_assert_eq!(len, 0);
                &[]
            }
        }
    }

    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }
}

impl Clone for Chunk {
    fn clone(&self) -> Self {
        Chunk {
            inner: self.inner.clone(),
        }
    }
}

impl Deref for Chunk {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl fmt::Debug for Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chunk")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("ref_count", &self.ref_count())
            .finish()
    }
}

struct ChunkInner {
    packed: Option<Arc<PackedPage>>,
    tenant: Arc<TenantStats>,
    stats: Arc<ChunkPoolStats>,
    oversized_bytes: u64,
    offset: u32,
    capacity: u32,
    length: u32,
}

impl Drop for ChunkInner {
    fn drop(&mut self) {
        self.tenant.record_chunk_release(self.capacity as u64);
        self.stats.chunks_in_use.fetch_sub(1, Ordering::Relaxed);
        if self.oversized_bytes > 0 {
            self.stats
                .oversized_bytes_in_use
                .fetch_sub(self.oversized_bytes, Ordering::Relaxed);
        }
    }
}
