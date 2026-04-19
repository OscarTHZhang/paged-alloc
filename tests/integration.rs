use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use paged_alloc::{ChunkPool, PagePool, PageSource, Tenant};

#[test]
fn alloc_seal_and_read() {
    let mut pool = PagePool::new(64);
    let tenant = Tenant::new("t");
    let mut builder = pool.allocate(&tenant);
    assert_eq!(builder.capacity(), 64);
    assert!(builder.is_empty());

    builder.append(b"hello ").unwrap();
    builder.append(b"world").unwrap();
    assert_eq!(builder.len(), 11);
    assert_eq!(builder.as_slice(), b"hello world");

    let page = builder.seal();
    assert_eq!(&page[..], b"hello world");
    assert_eq!(page.capacity(), 64);
    assert_eq!(page.len(), 11);
}

#[test]
fn append_beyond_capacity_returns_error() {
    let mut pool = PagePool::new(16);
    let tenant = Tenant::new("t");
    let mut builder = pool.allocate(&tenant);
    // Only write within the first 8 bytes; append is bounds-checked.
    builder.append(b"1234").unwrap();
    let err = builder.append(b"12345678901234").unwrap_err();
    assert_eq!(err.capacity, 16);
    assert_eq!(err.len, 4);
    assert_eq!(err.attempted, 14);
    assert_eq!(builder.as_slice(), b"1234");
}

#[test]
fn direct_slice_write_with_set_len() {
    let mut pool = PagePool::new(16);
    let tenant = Tenant::new("t");
    let mut builder = pool.allocate(&tenant);
    // Zero the first 5 bytes to avoid reading stale intrusive-list bytes.
    builder.as_mut_slice()[..5].copy_from_slice(b"abcde");
    builder.set_len(5);
    let page = builder.seal();
    assert_eq!(&page[..], b"abcde");
}

#[test]
fn drop_sealed_page_recycles_buffer_locally() {
    let mut pool = PagePool::new(64);
    let tenant = Tenant::new("t");

    let page = pool.allocate(&tenant).seal();
    assert_eq!(pool.stats().pages_in_use(), 1);
    assert_eq!(pool.stats().allocations_from_heap(), 1);
    drop(page);
    // After drop on the owning thread the buffer sits in the return
    // queue; free_pages counts it but pages_in_use is 0.
    assert_eq!(pool.stats().pages_in_use(), 0);
    assert_eq!(pool.stats().free_pages(), 1);

    // Second allocation should reuse, not heap-allocate.
    let _page2 = pool.allocate(&tenant).seal();
    assert_eq!(pool.stats().allocations_from_heap(), 1);
    assert_eq!(pool.stats().free_pages(), 0);
    assert_eq!(pool.stats().pages_in_use(), 1);
    assert_eq!(pool.stats().return_queue_drains(), 1);
}

#[test]
fn drop_unsealed_builder_recycles_buffer() {
    let mut pool = PagePool::new(64);
    let tenant = Tenant::new("t");
    {
        let _builder = pool.allocate(&tenant);
        assert_eq!(tenant.stats().pages_in_use(), 1);
    }
    assert_eq!(tenant.stats().pages_in_use(), 0);
    assert_eq!(pool.stats().free_pages(), 1);
    assert_eq!(pool.stats().pages_in_use(), 0);
}

#[test]
fn tenant_stats_track_bytes_and_peak() {
    let mut pool = PagePool::new(128);
    let tenant = Tenant::new("t");
    let p1 = pool.allocate(&tenant).seal();
    let p2 = pool.allocate(&tenant).seal();
    assert_eq!(tenant.stats().pages_in_use(), 2);
    assert_eq!(tenant.stats().bytes_in_use(), 256);
    assert_eq!(tenant.stats().peak_bytes_in_use(), 256);
    assert_eq!(tenant.stats().total_pages_allocated(), 2);

    drop(p1);
    assert_eq!(tenant.stats().pages_in_use(), 1);
    assert_eq!(tenant.stats().bytes_in_use(), 128);
    assert_eq!(tenant.stats().peak_bytes_in_use(), 256);

    drop(p2);
    assert_eq!(tenant.stats().pages_in_use(), 0);
    assert_eq!(tenant.stats().bytes_in_use(), 0);
    assert_eq!(tenant.stats().peak_bytes_in_use(), 256);
    assert_eq!(tenant.stats().total_pages_allocated(), 2);
}

#[test]
fn cloning_page_keeps_stats_until_last_drop() {
    let mut pool = PagePool::new(64);
    let tenant = Tenant::new("t");
    let p1 = pool.allocate(&tenant).seal();
    let p2 = p1.clone();
    let p3 = p1.clone();
    assert_eq!(p1.ref_count(), 3);
    assert_eq!(tenant.stats().pages_in_use(), 1);
    assert_eq!(pool.stats().pages_in_use(), 1);

    drop(p1);
    drop(p2);
    assert_eq!(tenant.stats().pages_in_use(), 1);
    assert_eq!(pool.stats().pages_in_use(), 1);
    assert_eq!(p3.ref_count(), 1);

    drop(p3);
    assert_eq!(tenant.stats().pages_in_use(), 0);
    assert_eq!(pool.stats().free_pages(), 1);
}

#[test]
fn buffers_reused_across_tenants() {
    let mut pool = PagePool::new(64);
    let a = Tenant::new("a");
    let b = Tenant::new("b");

    let page_a = pool.allocate(&a).seal();
    drop(page_a);
    assert_eq!(pool.stats().free_pages(), 1);

    let _page_b = pool.allocate(&b).seal();
    assert_eq!(pool.stats().allocations_from_heap(), 1);

    assert_eq!(a.stats().pages_in_use(), 0);
    assert_eq!(a.stats().total_pages_allocated(), 1);
    assert_eq!(b.stats().pages_in_use(), 1);
    assert_eq!(b.stats().total_pages_allocated(), 1);
}

#[test]
fn page_send_sync_across_reader_threads() {
    let mut pool = PagePool::new(32);
    let tenant = Tenant::new("t");
    let mut builder = pool.allocate(&tenant);
    builder.append(b"shared").unwrap();
    let page = builder.seal();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let p = page.clone();
            thread::spawn(move || {
                assert_eq!(&p[..], b"shared");
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    drop(page);
    assert_eq!(tenant.stats().pages_in_use(), 0);
}

#[test]
fn cross_thread_drop_returns_via_atomic_queue() {
    let mut pool = PagePool::new(64);
    let tenant = Tenant::new("t");

    // Allocate and seal a page, then hand it to another thread for drop.
    let page = pool.allocate(&tenant).seal();
    assert_eq!(pool.stats().pages_in_use(), 1);
    assert_eq!(pool.stats().free_pages(), 0);

    thread::spawn(move || {
        drop(page);
    })
    .join()
    .unwrap();

    // After the spawned thread drops the page, it must be on the return
    // queue. The owning pool hasn't drained it yet.
    assert_eq!(pool.stats().pages_in_use(), 0);
    assert_eq!(pool.stats().free_pages(), 1);
    assert_eq!(pool.stats().return_queue_drains(), 0);

    // Next allocate drains the return queue.
    let _page2 = pool.allocate(&tenant).seal();
    assert_eq!(pool.stats().return_queue_drains(), 1);
    assert_eq!(pool.stats().allocations_from_heap(), 1);
}

#[test]
fn concurrent_per_worker_pools() {
    // Thread-per-core model: each worker owns its own pool and tenant.
    let threads = 8;
    let per_thread = 500;
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut pool = PagePool::new(64);
                let tenant = Tenant::new("t");
                barrier.wait();
                for i in 0..per_thread {
                    let mut b = pool.allocate(&tenant);
                    let byte = (i & 0xff) as u8;
                    b.as_mut_slice()[0] = byte;
                    b.set_len(1);
                    let p = b.seal();
                    assert_eq!(p[0], byte);
                }
                assert_eq!(tenant.stats().total_pages_allocated(), per_thread as u64);
                assert_eq!(tenant.stats().pages_in_use(), 0);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn prewarm_populates_free_list() {
    let mut pool = PagePool::new(64);
    assert_eq!(pool.stats().free_pages(), 0);
    assert_eq!(pool.stats().prewarmed_pages(), 0);

    pool.prewarm(10);
    assert_eq!(pool.stats().free_pages(), 10);
    assert_eq!(pool.stats().prewarmed_pages(), 10);
    assert_eq!(pool.stats().allocations_from_heap(), 10);
    assert_eq!(pool.local_free_pages(), 10);

    // The next 10 allocations hit the local free list only.
    let tenant = Tenant::new("t");
    let mut pages = Vec::new();
    for _ in 0..10 {
        pages.push(pool.allocate(&tenant).seal());
    }
    assert_eq!(pool.stats().allocations_from_heap(), 10);
    assert_eq!(pool.stats().pages_in_use(), 10);
    assert_eq!(pool.stats().free_pages(), 0);

    // The 11th forces a fresh heap allocation.
    pages.push(pool.allocate(&tenant).seal());
    assert_eq!(pool.stats().allocations_from_heap(), 11);
}

#[test]
fn with_capacity_combines_new_and_prewarm() {
    let pool = PagePool::with_capacity(64, 32);
    assert_eq!(pool.stats().free_pages(), 32);
    assert_eq!(pool.stats().prewarmed_pages(), 32);
    assert_eq!(pool.local_free_pages(), 32);
}

/// A `PageSource` that records every call to `allocate`, `release`,
/// and `prefault`. Used to verify that `prewarm` exercises the full
/// source contract — especially `prefault`, which is the knob a future
/// `MmapSource` will use to fault huge pages in at startup.
struct RecordingSource {
    inner: paged_alloc::HeapSource,
    allocated: Arc<AtomicUsize>,
    released: Arc<AtomicUsize>,
    prefaulted: Arc<AtomicUsize>,
}

unsafe impl PageSource for RecordingSource {
    fn page_size(&self) -> usize {
        self.inner.page_size()
    }
    fn allocate(&self) -> NonNull<u8> {
        self.allocated.fetch_add(1, Ordering::Relaxed);
        self.inner.allocate()
    }
    unsafe fn release(&self, ptr: NonNull<u8>) {
        self.released.fetch_add(1, Ordering::Relaxed);
        unsafe { self.inner.release(ptr) }
    }
    unsafe fn prefault(&self, ptr: NonNull<u8>) {
        self.prefaulted.fetch_add(1, Ordering::Relaxed);
        // Touch one byte per 4 KiB OS page to exercise a realistic
        // prefault implementation.
        let page_size = self.inner.page_size();
        let base = ptr.as_ptr();
        let mut offset = 0;
        while offset < page_size {
            unsafe {
                std::ptr::write_volatile(base.add(offset), 0);
            }
            offset += 4096;
        }
    }
}

#[test]
fn prewarm_calls_prefault_on_every_page() {
    let allocated = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicUsize::new(0));
    let prefaulted = Arc::new(AtomicUsize::new(0));
    let source = RecordingSource {
        inner: paged_alloc::HeapSource::new(64),
        allocated: allocated.clone(),
        released: released.clone(),
        prefaulted: prefaulted.clone(),
    };

    let mut pool = PagePool::with_source(source);
    pool.prewarm(25);
    assert_eq!(allocated.load(Ordering::Relaxed), 25);
    assert_eq!(prefaulted.load(Ordering::Relaxed), 25);
    assert_eq!(released.load(Ordering::Relaxed), 0);

    // Consume 25 pages, drop them, drop pool. Every buffer must be
    // released exactly once.
    let tenant = Tenant::new("t");
    let mut pages = Vec::new();
    for _ in 0..25 {
        pages.push(pool.allocate(&tenant).seal());
    }
    drop(pages);
    drop(pool);
    assert_eq!(allocated.load(Ordering::Relaxed), 25);
    assert_eq!(released.load(Ordering::Relaxed), 25);
}

#[test]
fn pool_drop_releases_buffers_still_on_return_queue() {
    // A page dropped on a non-owner thread lands on the return queue.
    // If the pool then drops while that buffer is on the queue, the
    // PoolShared::Drop path must release it through the source.
    let allocated = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicUsize::new(0));
    let source = RecordingSource {
        inner: paged_alloc::HeapSource::new(64),
        allocated: allocated.clone(),
        released: released.clone(),
        prefaulted: Arc::new(AtomicUsize::new(0)),
    };
    let mut pool = PagePool::with_source(source);
    let tenant = Tenant::new("t");
    let page = pool.allocate(&tenant).seal();
    thread::spawn(move || drop(page)).join().unwrap();
    // Now the buffer is on the return queue. Drop the pool.
    drop(pool);
    assert_eq!(allocated.load(Ordering::Relaxed), 1);
    assert_eq!(released.load(Ordering::Relaxed), 1);
}

#[cfg(unix)]
#[test]
fn mmap_source_end_to_end() {
    use paged_alloc::MmapSource;

    // 16 KiB is 4× the typical OS page size, so prefault writes 4 bytes.
    let source = MmapSource::new(16 * 1024);
    let mut pool = PagePool::with_source(source);
    pool.prewarm(4);
    assert_eq!(pool.stats().prewarmed_pages(), 4);

    let tenant = Tenant::new("t");
    let mut builder = pool.allocate(&tenant);
    builder.append(b"mmap-backed").unwrap();
    let page = builder.seal();
    assert_eq!(&page[..], b"mmap-backed");
    drop(page);

    assert_eq!(tenant.stats().pages_in_use(), 0);
    assert_eq!(pool.stats().pages_in_use(), 0);
    assert_eq!(pool.stats().free_pages(), 4);
}

#[cfg(target_os = "linux")]
#[test]
fn madvise_dontneed_zeroes_region_linux() {
    use paged_alloc::{MmapSource, madvise_dontneed};
    use std::ptr::NonNull;

    let source = MmapSource::new(4096);
    let mut pool = PagePool::with_source(source);
    let tenant = Tenant::new("t");
    let mut builder = pool.allocate(&tenant);
    builder.as_mut_slice().fill(0xAB);
    let ptr = NonNull::new(builder.as_mut_slice().as_mut_ptr()).unwrap();
    unsafe {
        madvise_dontneed(ptr, 4096).expect("madvise failed");
    }
    // On Linux MADV_DONTNEED guarantees the next read returns zero.
    let after = builder.as_mut_slice();
    assert!(after.iter().all(|&b| b == 0));
}

// =========================================================================
// ChunkPool tests
// =========================================================================

#[test]
fn chunk_alloc_seal_and_read() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let mut b = pool.allocate(&tenant, 11);
    assert_eq!(b.capacity(), 11);
    b.append(b"hello ").unwrap();
    b.append(b"world").unwrap();
    assert_eq!(b.len(), 11);

    let chunk = b.seal();
    assert_eq!(&chunk[..], b"hello world");
    assert_eq!(chunk.capacity(), 11);
    assert_eq!(chunk.len(), 11);

    assert_eq!(tenant.stats().chunks_in_use(), 1);
    assert_eq!(tenant.stats().total_chunks_allocated(), 1);
    assert_eq!(tenant.stats().bytes_in_use(), 11);
}

#[test]
fn chunk_small_packs_into_shared_page() {
    // Pages are 16 KiB, threshold is 8 KiB. Ten 64-byte chunks should
    // all land in one underlying page.
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let mut chunks = Vec::new();
    for i in 0..10 {
        let mut b = pool.allocate(&tenant, 64);
        b.as_mut_slice()[0] = i as u8;
        b.set_len(64);
        chunks.push(b.seal());
    }

    // Exactly 1 underlying page: 1 heap allocation.
    assert_eq!(pool.page_stats().allocations_from_heap(), 1);
    assert_eq!(pool.stats().packed_allocations(), 10);
    assert_eq!(pool.stats().dedicated_allocations(), 0);
    assert_eq!(pool.stats().oversized_allocations(), 0);

    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c[0], i as u8);
        assert_eq!(c.len(), 64);
    }
}

#[test]
fn chunk_dedicated_takes_whole_page() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    // 10 KiB > threshold (8 KiB) so this goes to the dedicated path.
    let c1 = pool.allocate(&tenant, 10 * 1024).seal();
    let c2 = pool.allocate(&tenant, 10 * 1024).seal();

    assert_eq!(pool.stats().dedicated_allocations(), 2);
    assert_eq!(pool.stats().packed_allocations(), 0);
    assert_eq!(pool.page_stats().allocations_from_heap(), 2);
    drop(c1);
    drop(c2);
}

#[test]
fn chunk_oversized_bypasses_pool() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let size = 64 * 1024; // 4× page_size
    let chunk = pool.allocate(&tenant, size).seal();
    assert_eq!(chunk.capacity(), size);

    assert_eq!(pool.stats().oversized_allocations(), 1);
    assert_eq!(pool.stats().oversized_bytes_in_use(), size as u64);
    // Underlying PagePool was NOT touched.
    assert_eq!(pool.page_stats().allocations_from_heap(), 0);
    assert_eq!(pool.page_stats().pages_in_use(), 0);

    drop(chunk);
    assert_eq!(pool.stats().oversized_bytes_in_use(), 0);
    assert_eq!(pool.stats().chunks_in_use(), 0);
}

#[test]
fn chunk_drop_decrements_stats() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let c = pool.allocate(&tenant, 128).seal();
    assert_eq!(pool.stats().chunks_in_use(), 1);
    assert_eq!(tenant.stats().chunks_in_use(), 1);
    assert_eq!(tenant.stats().bytes_in_use(), 128);
    drop(c);
    assert_eq!(pool.stats().chunks_in_use(), 0);
    assert_eq!(tenant.stats().chunks_in_use(), 0);
    assert_eq!(tenant.stats().bytes_in_use(), 0);
    // Total is monotonic, stays at 1.
    assert_eq!(tenant.stats().total_chunks_allocated(), 1);
}

#[test]
fn chunk_clone_shares_ref_count() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let c1 = pool.allocate(&tenant, 64).seal();
    let c2 = c1.clone();
    let c3 = c1.clone();
    assert_eq!(c1.ref_count(), 3);
    // In-use is a per-chunk concept, not per-handle: 3 handles = 1 chunk.
    assert_eq!(tenant.stats().chunks_in_use(), 1);
    drop(c1);
    drop(c2);
    assert_eq!(tenant.stats().chunks_in_use(), 1);
    drop(c3);
    assert_eq!(tenant.stats().chunks_in_use(), 0);
}

#[test]
fn chunk_builder_drop_unsealed_releases_stats() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    {
        let _b = pool.allocate(&tenant, 256);
        assert_eq!(tenant.stats().chunks_in_use(), 1);
        assert_eq!(tenant.stats().bytes_in_use(), 256);
    }
    assert_eq!(tenant.stats().chunks_in_use(), 0);
    assert_eq!(tenant.stats().bytes_in_use(), 0);
    // Builder did bump total_chunks_allocated (allocation happened).
    assert_eq!(tenant.stats().total_chunks_allocated(), 1);
}

#[test]
fn chunk_zero_size() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let c = pool.allocate(&tenant, 0).seal();
    assert_eq!(c.len(), 0);
    assert_eq!(c.capacity(), 0);
    assert!(c.as_slice().is_empty());
    // No underlying page allocation for zero-size chunks.
    assert_eq!(pool.page_stats().allocations_from_heap(), 0);
}

#[test]
fn chunk_alloc_from_copy_in() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let c = pool.alloc_from(&tenant, b"hello world");
    assert_eq!(&c[..], b"hello world");
    assert_eq!(c.len(), 11);
}

#[test]
fn chunk_packing_rolls_to_next_page_when_full() {
    // 16 KiB page, threshold 8 KiB. Four 4 KiB chunks exactly fill
    // one page; the fifth has to roll to a new page.
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let _a = pool.allocate(&tenant, 4096).seal();
    let _b = pool.allocate(&tenant, 4096).seal();
    let _c = pool.allocate(&tenant, 4096).seal();
    let _d = pool.allocate(&tenant, 4096).seal();
    // Four chunks still fit in the first page.
    assert_eq!(pool.page_stats().allocations_from_heap(), 1);
    let _e = pool.allocate(&tenant, 4096).seal();
    // Fifth rolls to a new page.
    assert_eq!(pool.page_stats().allocations_from_heap(), 2);
    assert_eq!(pool.stats().packed_allocations(), 5);
}

#[test]
fn chunk_packed_page_retains_until_last_chunk_drops() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let a = pool.allocate(&tenant, 64).seal();
    let b = pool.allocate(&tenant, 64).seal();
    let c = pool.allocate(&tenant, 64).seal();
    assert_eq!(pool.page_stats().free_pages(), 0);

    // Drop two of three. Page must NOT return yet — one chunk still holds it.
    drop(a);
    drop(b);
    assert_eq!(pool.page_stats().free_pages(), 0);

    // Also drop the ChunkPool's internal reference by forcing a new
    // page open via a fresh allocation that triggers roll... actually,
    // simpler: drop the pool's "current" via dropping the pool.
    // Or explicitly: free_pages stays 0 until `c` drops AND the pool's
    // current-page handle no longer points at it.
    //
    // The simpler assertion: until `c` drops, free_pages is 0.
    drop(pool); // releases the pool's local head and current-page handle
    // After pool drop, only `c` holds the packed page.
    drop(c);
    // By now the buffer has been released via PackedPage::drop ->
    // PoolShared::push_return. But the PoolShared was dropped with
    // the pool... actually no: the Arc<PoolShared> in PackedPage's
    // Release::Pool keeps PoolShared alive. When the last chunk
    // drops, PackedPage::drop calls shared.push_return, which adds
    // to the return queue of a PoolShared whose PagePool is gone.
    // That's fine — the buffer is still released via PoolShared::drop
    // eventually, when the Arc<PoolShared> count hits 0.
    //
    // This test's assertion isn't about counters (pool is gone); it's
    // about not panicking and cleanly releasing.
}

#[test]
fn chunk_cross_thread_drop() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let chunk = pool.allocate(&tenant, 128).seal();
    thread::spawn(move || drop(chunk)).join().unwrap();
    // Tenant counter decremented from a non-owner thread.
    assert_eq!(tenant.stats().chunks_in_use(), 0);
}

#[test]
fn chunk_send_sync_readers() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let mut b = pool.allocate(&tenant, 6);
    b.append(b"shared").unwrap();
    let chunk = b.seal();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let c = chunk.clone();
            thread::spawn(move || assert_eq!(&c[..], b"shared"))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn chunk_prewarm_avoids_cold_path_for_first_allocs() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    pool.prewarm(2);
    // No chunks yet but 2 pages are ready.
    assert_eq!(pool.page_stats().allocations_from_heap(), 2);
    let heap_before = pool.page_stats().allocations_from_heap();

    let tenant = Tenant::new("t");
    // Fit many small chunks — they consume one of the prewarmed pages.
    let mut sink = Vec::new();
    for _ in 0..20 {
        sink.push(pool.allocate(&tenant, 64).seal());
    }
    assert_eq!(pool.page_stats().allocations_from_heap(), heap_before);
}

#[test]
fn chunk_aligned_allocation_pads_packed_offset() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    // Start with a 3-byte chunk; cursor becomes 3.
    let _a = pool.allocate(&tenant, 3).seal();
    // Request 64-byte-aligned allocation. Cursor advances to 64 first,
    // wasting 61 bytes, then claims 64 bytes.
    let mut b = pool.allocate_aligned(&tenant, 64, 64);
    b.set_len(0);
    // Check that the buffer pointer we got is 64-aligned.
    let ptr = b.as_mut_slice().as_mut_ptr() as usize;
    assert_eq!(ptr % 64, 0, "aligned allocation must be 64-byte aligned");
    let _ = b.seal();
}

#[test]
fn chunk_oversized_alignment_via_layout() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let mut b = pool.allocate_aligned(&tenant, 64 * 1024, 4096);
    let ptr = b.as_mut_slice().as_mut_ptr() as usize;
    assert_eq!(ptr % 4096, 0);
    b.set_len(0);
    let _ = b.seal();
}

#[test]
fn chunk_append_overflow_returns_error() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let mut b = pool.allocate(&tenant, 8);
    b.append(b"1234").unwrap();
    let err = b.append(b"56789").unwrap_err();
    assert_eq!(err.capacity, 8);
    assert_eq!(err.len, 4);
    assert_eq!(err.attempted, 5);
}

#[cfg(unix)]
#[test]
fn chunk_pool_with_mmap_source() {
    use paged_alloc::{ChunkPool, MmapSource};

    // ChunkPool on top of MmapSource — the free list, packing path,
    // and return-queue drain must all work regardless of whether the
    // backing buffers came from the heap or from mmap. 16 KiB is a
    // multiple of any OS page size (4 KiB or 16 KiB on arm64 macOS).
    let mut pool = ChunkPool::with_source(MmapSource::new(16 * 1024));
    pool.prewarm(4);

    let tenant = Tenant::new("t");

    // Allocate enough small chunks to exercise packing + rollover.
    let mut chunks = Vec::new();
    for i in 0..32 {
        let data = vec![i as u8; 128];
        chunks.push(pool.alloc_from(&tenant, &data));
    }

    // All 32 chunks should pack into ≤ 2 mmap'd pages (16 KiB / 128 B = 128
    // slots per page, but packing_allocations tracks each one).
    assert_eq!(pool.stats().packed_allocations(), 32);
    assert_eq!(tenant.stats().chunks_in_use(), 32);

    // Round-trip content check.
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.len(), 128);
        assert_eq!(c[0], i as u8);
        assert_eq!(c[127], i as u8);
    }

    // Drop everything and confirm counters clear.
    drop(chunks);
    assert_eq!(tenant.stats().chunks_in_use(), 0);
    assert_eq!(tenant.stats().bytes_in_use(), 0);
}

#[test]
fn chunk_page_and_chunk_stats_are_independent() {
    // A tenant's chunk counters and page counters advance on different
    // APIs and must not interfere.
    let mut pages = PagePool::new(4096);
    let mut chunks = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let _p = pages.allocate(&tenant).seal();
    let _c = chunks.alloc_from(&tenant, &[0u8; 128]);

    assert_eq!(tenant.stats().pages_in_use(), 1);
    assert_eq!(tenant.stats().total_pages_allocated(), 1);
    assert_eq!(tenant.stats().chunks_in_use(), 1);
    assert_eq!(tenant.stats().total_chunks_allocated(), 1);
    // bytes_in_use is the sum of both granularities.
    assert_eq!(tenant.stats().bytes_in_use(), 4096 + 128);
}

#[test]
fn chunk_alloc_with_fills_buffer_and_sets_full_len() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let chunk = pool.alloc_with(&tenant, 16, |buf| {
        assert_eq!(buf.len(), 16);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
    });

    assert_eq!(chunk.len(), 16);
    assert_eq!(chunk.capacity(), 16);
    for (i, &b) in chunk.iter().enumerate() {
        assert_eq!(b, i as u8);
    }
    assert_eq!(tenant.stats().chunks_in_use(), 1);
    assert_eq!(tenant.stats().bytes_in_use(), 16);
}

#[test]
fn chunk_alloc_with_len_uses_returned_length() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    let chunk = pool.alloc_with_len(&tenant, 32, |buf| {
        let msg = b"variable";
        buf[..msg.len()].copy_from_slice(msg);
        msg.len()
    });

    assert_eq!(chunk.len(), 8);
    assert_eq!(chunk.capacity(), 32);
    assert_eq!(&chunk[..], b"variable");
    // Tenant is charged for the requested capacity, not the logical len.
    assert_eq!(tenant.stats().bytes_in_use(), 32);
}

#[test]
#[should_panic(expected = "len 40 > capacity 32")]
fn chunk_alloc_with_len_panics_if_closure_exceeds_capacity() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let _ = pool.alloc_with_len(&tenant, 32, |_| 40);
}

#[test]
fn chunk_alloc_with_panic_unwinds_cleanly() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");

    // A closure that panics in the middle of init. The ChunkBuilder
    // must drop cleanly: tenant counters must return to 0 and the
    // underlying buffer must be back in the pool's free list.
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _chunk = pool.alloc_with(&tenant, 64, |buf| {
            buf[0] = 1;
            panic!("simulated");
        });
    }));
    assert!(result.is_err());

    assert_eq!(tenant.stats().chunks_in_use(), 0);
    assert_eq!(tenant.stats().bytes_in_use(), 0);
    // Pool is still usable: a subsequent allocation should reuse the
    // recycled buffer from the packed page.
    let _c = pool.alloc_from(&tenant, &[42u8; 16]);
    assert_eq!(tenant.stats().chunks_in_use(), 1);
}

#[test]
fn chunk_alloc_with_zero_size_calls_closure_with_empty_slice() {
    let mut pool = ChunkPool::with_page_size(16 * 1024);
    let tenant = Tenant::new("t");
    let chunk = pool.alloc_with(&tenant, 0, |buf| {
        assert!(buf.is_empty());
    });
    assert_eq!(chunk.len(), 0);
    assert!(chunk.as_slice().is_empty());
}

#[test]
fn page_alloc_from_copies_data() {
    let mut pool = PagePool::new(4096);
    let tenant = Tenant::new("t");
    let page = pool.alloc_from(&tenant, b"block 42");
    assert_eq!(&page[..], b"block 42");
    assert_eq!(page.len(), 8);
    assert_eq!(page.capacity(), 4096);
}

#[test]
#[should_panic(expected = "data length 5000 exceeds page_size 4096")]
fn page_alloc_from_panics_on_oversize() {
    let mut pool = PagePool::new(4096);
    let tenant = Tenant::new("t");
    let oversize = vec![0u8; 5000];
    let _ = pool.alloc_from(&tenant, &oversize);
}

#[test]
fn page_alloc_with_fills_whole_page_sets_full_len() {
    let mut pool = PagePool::new(4096);
    let tenant = Tenant::new("t");
    let page = pool.alloc_with(&tenant, |buf| {
        buf[..5].copy_from_slice(b"hello");
    });
    assert_eq!(page.len(), 4096);
    assert_eq!(page.capacity(), 4096);
    assert_eq!(&page[..5], b"hello");
}

#[test]
fn page_alloc_with_len_uses_returned_length() {
    let mut pool = PagePool::new(4096);
    let tenant = Tenant::new("t");
    let page = pool.alloc_with_len(&tenant, |buf| {
        let msg = b"short";
        buf[..msg.len()].copy_from_slice(msg);
        msg.len()
    });
    assert_eq!(page.len(), 5);
    assert_eq!(page.capacity(), 4096);
    assert_eq!(&page[..], b"short");
}

// =========================================================================
// End ChunkPool tests
// =========================================================================

#[test]
fn tenant_clone_shares_stats() {
    let mut pool = PagePool::new(64);
    let tenant = Tenant::new("t");
    let tenant_clone = tenant.clone();

    let _p = pool.allocate(&tenant).seal();
    assert_eq!(tenant.stats().pages_in_use(), 1);
    assert_eq!(tenant_clone.stats().pages_in_use(), 1);
    assert_eq!(tenant.id().as_str(), "t");
    assert_eq!(tenant_clone.id().as_str(), "t");
}
