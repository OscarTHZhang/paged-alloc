//! PagePool: fixed-size, page-aligned buffers. Prefer this over
//! `ChunkPool` when you specifically need a known page size or
//! page alignment — for example, O_DIRECT I/O staging buffers,
//! file-block caches whose block size matches `page_size`, or
//! SIMD loads requiring 4 KiB alignment.
//!
//!     cargo run --release --example pages

use paged_alloc::{PagePool, Tenant};

fn main() {
    // A pool of 4 KiB pages. The page size is caller-chosen.
    let mut pool = PagePool::new(4096);
    let tenant = Tenant::new("block-cache");

    // Allocate gives you a PageBuilder with capacity == page_size.
    let mut builder = pool.allocate(&tenant);
    assert_eq!(builder.capacity(), 4096);

    builder.append(b"block 0\n").unwrap();
    let page = builder.seal();
    println!("first 8 bytes: {:?}", &page[..8]);

    // Dropping a page returns its buffer to the pool's local free
    // list. The very next allocation reuses that buffer without
    // asking the source for a fresh one.
    drop(page);

    let stats_before = pool.stats().allocations_from_heap();
    let _p2 = pool.allocate(&tenant).seal();
    let stats_after = pool.stats().allocations_from_heap();

    println!(
        "heap allocations: {} → {} (reused the recycled buffer)",
        stats_before, stats_after
    );
    assert_eq!(stats_before, stats_after);
}
