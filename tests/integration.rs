use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use paged_alloc::{PagePool, PageSource, Tenant};

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
