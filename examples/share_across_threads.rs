//! A sealed `Chunk` is `Send + Sync + Clone`, so you can hand
//! clones to any number of reader threads and drop them from
//! whichever thread holds the last clone. Cloning is a single
//! `Arc` bump (~10 ns); dropping the last clone returns the
//! underlying buffer to the owning pool.
//!
//!     cargo run --release --example share_across_threads

use std::thread;

use paged_alloc::{ChunkPool, Tenant};

fn main() {
    let mut pool = ChunkPool::new();
    let tenant = Tenant::new("shared");

    let chunk = pool.alloc_from(&tenant, b"same bytes, many threads");
    println!("allocated 1 chunk; ref_count={}", chunk.ref_count());

    // Fan out to 4 reader threads. Each gets a cheap clone.
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let c = chunk.clone();
            thread::spawn(move || {
                let s = std::str::from_utf8(&c).unwrap();
                println!("  thread {i}: read {:>2} bytes: {s}", c.len());
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // Four clones went to threads (then dropped on join), so the
    // in-use count is still 1 (our local `chunk`). The single
    // chunk is shared by reference, not duplicated.
    assert_eq!(tenant.stats().chunks_in_use(), 1);

    drop(chunk);
    assert_eq!(tenant.stats().chunks_in_use(), 0);
    println!("\nall threads joined, chunk dropped. tenant counters cleared.");
}
