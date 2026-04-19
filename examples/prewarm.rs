//! Prewarm: commit N pages up front at startup so the first N
//! user allocations skip the cold path entirely. Mirrors the
//! ScyllaDB-style "do the expensive setup before admitting
//! traffic" pattern.
//!
//!     cargo run --release --example prewarm

use std::time::Instant;

use paged_alloc::{ChunkPool, Tenant};

const N: usize = 256;
const CHUNK_SIZE: usize = 4 * 1024;

fn main() {
    println!(
        "Allocating {} × {} B chunks, timing the first-N window.\n",
        N, CHUNK_SIZE
    );

    // Cold start: no prewarm. Every allocation hits the source
    // until the free list warms up.
    let t = time_first_n_allocations(|| {
        let pool = ChunkPool::new();
        (pool, Tenant::new("cold"))
    });
    println!(
        "  cold (no prewarm):  {:>8} ns / chunk  ({:?} total)",
        t.per_op, t.total
    );

    // Warm start: prewarm at construction. The first N allocs hit
    // the local free list immediately — no `alloc_zeroed`, no
    // first-touch faults.
    let t = time_first_n_allocations(|| {
        let pool = ChunkPool::with_capacity(16 * 1024, N);
        (pool, Tenant::new("warm"))
    });
    println!(
        "  prewarmed:          {:>8} ns / chunk  ({:?} total)",
        t.per_op, t.total
    );
}

struct Timing {
    per_op: u64,
    total: std::time::Duration,
}

fn time_first_n_allocations<F>(setup: F) -> Timing
where
    F: FnOnce() -> (ChunkPool, Tenant),
{
    let (mut pool, tenant) = setup();
    let start = Instant::now();
    let mut sink = Vec::with_capacity(N);
    for _ in 0..N {
        sink.push(pool.allocate(&tenant, CHUNK_SIZE).seal());
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&sink);
    Timing {
        per_op: (elapsed.as_nanos() as u64) / N as u64,
        total: elapsed,
    }
}
