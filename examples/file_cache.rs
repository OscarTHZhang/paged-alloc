//! Realistic file-cache workload using `ChunkPool`.
//!
//! A single owner thread populates a shared
//! `Arc<RwLock<HashMap<FileId, Chunk>>>` with N files of varying
//! sizes, allocating them from a `ChunkPool`. Multiple reader
//! threads then do random point-lookups, clone the cached chunk,
//! read its bytes, and drop the clone — all on the reader thread,
//! which exercises the cross-thread drop path in the way a
//! production file cache actually does.
//!
//! Run with:
//!
//! ```text
//! cargo run --release --example file_cache
//! ```
//!
//! The program reports sustained read throughput, per-read
//! latency, and the tenant / pool statistics so you can see
//! exactly where memory is sitting. Tweak the constants at the
//! top of `main` to explore different cache sizes, file-size
//! distributions, and reader counts.

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Instant;

use paged_alloc::{Chunk, ChunkPool, Tenant};

type FileId = u64;

fn main() {
    // ----- Workload knobs -----------------------------------------------
    let num_files: u64 = 1_000;
    let avg_file_size: usize = 8 * 1024; // 8 KiB
    let file_size_jitter: usize = 4 * 1024; // ±4 KiB
    let num_readers: usize = 4;
    let reads_per_reader: u64 = 200_000;
    let page_size: usize = 16 * 1024;
    let prewarm_pages: usize = 1024;

    // ----- Populate the cache (owner thread) ----------------------------
    println!("─── populating cache ──────────────────────────────────────");
    let populate_start = Instant::now();
    let mut pool = ChunkPool::with_capacity(page_size, prewarm_pages);
    let tenant = Tenant::new("file-cache");

    let mut cache: HashMap<FileId, Chunk> = HashMap::with_capacity(num_files as usize);
    let mut rng = 0x9E3779B97F4A7C15u64;
    for id in 0..num_files {
        rng = lcg(rng);
        let size = avg_file_size - file_size_jitter + (rng as usize % (2 * file_size_jitter + 1));
        // Allocate + fill + seal in one call. The closure writes a
        // deterministic pattern so reader checksums are meaningful
        // and the optimizer cannot elide the loads.
        let chunk = pool.alloc_with(&tenant, size, |buf| {
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = ((i as u64) ^ id) as u8;
            }
        });
        cache.insert(id, chunk);
    }
    let populate_elapsed = populate_start.elapsed();
    let total_bytes = tenant.stats().bytes_in_use();
    println!("  {} files populated in {:?}", num_files, populate_elapsed);
    println!(
        "  Cache size: {:.2} MiB; chunks in use: {}",
        total_bytes as f64 / (1024.0 * 1024.0),
        tenant.stats().chunks_in_use()
    );
    println!(
        "  Pool: heap allocs={}, pages in use={}, packed allocs={}, dedicated allocs={}",
        pool.page_stats().allocations_from_heap(),
        pool.page_stats().pages_in_use(),
        pool.stats().packed_allocations(),
        pool.stats().dedicated_allocations(),
    );

    // Wrap the cache for concurrent reads.
    let cache = Arc::new(RwLock::new(cache));

    // ----- Concurrent readers -------------------------------------------
    println!();
    println!(
        "─── running {} reader threads × {} reads ──────────────────",
        num_readers, reads_per_reader
    );
    let start = Instant::now();
    let handles: Vec<_> = (0..num_readers)
        .map(|worker_id| {
            let cache = cache.clone();
            thread::spawn(move || {
                let mut reads_hit = 0u64;
                let mut bytes_read = 0u64;
                let mut checksum: u64 = 0;
                let mut rng = (worker_id as u64)
                    .wrapping_mul(0x9E3779B97F4A7C15)
                    .wrapping_add(0xDEADBEEFCAFEBABE);

                for _ in 0..reads_per_reader {
                    rng = lcg(rng);
                    let file_id = rng % num_files;
                    // Take a read lock, clone the Chunk, release the
                    // lock, then do the read. This is the pattern
                    // every production file cache uses.
                    let chunk: Option<Chunk> = cache.read().unwrap().get(&file_id).cloned();
                    if let Some(chunk) = chunk {
                        // Pretend to do something useful with the bytes.
                        let mut s: u64 = 0;
                        for &b in chunk.iter() {
                            s = s.wrapping_add(b as u64);
                        }
                        checksum = checksum.wrapping_add(s);
                        reads_hit += 1;
                        bytes_read += chunk.len() as u64;
                        // `chunk` drops here — on the reader thread.
                        // This is the cross-thread drop path: it
                        // push_return's the buffer (if it was the
                        // last clone) and decrements tenant stats
                        // atomically.
                    }
                }
                (reads_hit, bytes_read, checksum)
            })
        })
        .collect();

    let mut total_reads = 0u64;
    let mut total_bytes = 0u64;
    let mut combined_checksum: u64 = 0;
    for h in handles {
        let (reads, bytes, checksum) = h.join().unwrap();
        total_reads += reads;
        total_bytes += bytes;
        combined_checksum = combined_checksum.wrapping_add(checksum);
    }
    let elapsed = start.elapsed();
    black_box(combined_checksum);

    // ----- Report --------------------------------------------------------
    let secs = elapsed.as_secs_f64();
    println!();
    println!("─── results ────────────────────────────────────────────────");
    println!("  Wall clock: {:?}", elapsed);
    println!(
        "  Aggregate: {:.2} Mreads/s   {:.2} GiB/s",
        total_reads as f64 / 1e6 / secs,
        total_bytes as f64 / 1_073_741_824.0 / secs,
    );
    println!(
        "  Per read:  {:.1} ns        ({:.0} ns/reader-op × {} readers)",
        elapsed.as_nanos() as f64 / total_reads as f64,
        elapsed.as_nanos() as f64 * num_readers as f64 / total_reads as f64,
        num_readers,
    );
    println!(
        "  Bytes copied by readers: {:.2} MiB",
        total_bytes as f64 / (1024.0 * 1024.0),
    );
    println!();

    // ----- Post-run stats -----------------------------------------------
    println!("─── post-run state ────────────────────────────────────────");
    println!(
        "  Tenant: chunks_in_use={}, bytes_in_use={:.2} MiB, total_chunks_allocated={}",
        tenant.stats().chunks_in_use(),
        tenant.stats().bytes_in_use() as f64 / (1024.0 * 1024.0),
        tenant.stats().total_chunks_allocated(),
    );
    println!(
        "  Pool: pages_in_use={}, free_pages={}, return_queue_drains={}",
        pool.page_stats().pages_in_use(),
        pool.page_stats().free_pages(),
        pool.page_stats().return_queue_drains(),
    );

    // Drop the cache; tenant and pool counters should return to zero.
    drop(cache);
    println!();
    println!("─── after dropping the cache ──────────────────────────────");
    println!(
        "  Tenant: chunks_in_use={}, bytes_in_use={}",
        tenant.stats().chunks_in_use(),
        tenant.stats().bytes_in_use(),
    );
}

/// Knuth LCG — good enough for generating uniformly-distributed
/// integers for benchmark workload access patterns, and has no
/// external dependency.
fn lcg(s: u64) -> u64 {
    s.wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}
