//! Realistic append-only log workload using `ChunkPool`.
//!
//! One writer thread owns the `ChunkPool` and continuously appends
//! records of varying sizes into an indexed log
//! (`Arc<RwLock<BTreeMap<Lsn, Chunk>>>`), advertising the current
//! high-water LSN via an `AtomicU64`. Multiple reader threads do
//! random point-lookups against the published LSN range, clone
//! chunks, read bytes, and drop clones — all on the reader
//! thread. This exercises the same cross-thread drop path a
//! database memtable with concurrent readers would produce.
//!
//! Writer and readers run concurrently for a fixed duration.
//! The program then reports their throughputs independently so
//! you can see whether reader activity is crowding out the
//! writer (or vice versa).
//!
//! Run with:
//!
//! ```text
//! cargo run --release --example append_log
//! ```

use std::collections::BTreeMap;
use std::hint::{black_box, spin_loop};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use paged_alloc::{Chunk, ChunkPool, Tenant};

type Lsn = u64;

fn main() {
    // ----- Workload knobs -----------------------------------------------
    let duration_secs: u64 = 3;
    let num_readers: usize = 2;
    let avg_record_size: usize = 256;
    let record_size_jitter: usize = 192; // total range: 256–448 B
    let page_size: usize = 16 * 1024;
    let prewarm_pages: usize = 2048;

    // ----- Shared state --------------------------------------------------
    let log: Arc<RwLock<BTreeMap<Lsn, Chunk>>> =
        Arc::new(RwLock::new(BTreeMap::new()));
    let high_water = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let tenant = Tenant::new("log");

    // ----- Writer thread -------------------------------------------------
    let writer = {
        let log = log.clone();
        let high_water = high_water.clone();
        let running = running.clone();
        let tenant = tenant.clone();
        thread::spawn(move || {
            let mut pool = ChunkPool::with_capacity(page_size, prewarm_pages);
            let mut lsn = 0u64;
            let mut rng = 0xC6BC279692B5C323u64;
            let max_size = avg_record_size + record_size_jitter;
            let mut payload = vec![0u8; max_size];
            let writer_start = Instant::now();

            while running.load(Ordering::Relaxed) {
                rng = lcg(rng);
                let size = avg_record_size
                    - record_size_jitter
                    + (rng as usize % (2 * record_size_jitter + 1));
                // Fill the payload with an lsn-dependent pattern.
                for (i, b) in payload[..size].iter_mut().enumerate() {
                    *b = ((i as u64) ^ lsn) as u8;
                }
                // alloc_from copies payload[..size] into a fresh chunk.
                let chunk = pool.alloc_from(&tenant, &payload[..size]);
                // Publish into the log. A write lock serializes with
                // concurrent readers' BTreeMap::get calls.
                log.write().unwrap().insert(lsn, chunk);
                high_water.store(lsn, Ordering::Relaxed);
                lsn += 1;
            }

            let writer_elapsed = writer_start.elapsed();
            WriterResult {
                records_written: lsn,
                elapsed: writer_elapsed,
                bytes_in_use: tenant.stats().bytes_in_use(),
                pool_heap_allocs: pool.page_stats().allocations_from_heap(),
                pool_pages_in_use: pool.page_stats().pages_in_use(),
                pool_return_drains: pool.page_stats().return_queue_drains(),
                chunk_packed: pool.stats().packed_allocations(),
                chunk_dedicated: pool.stats().dedicated_allocations(),
                chunk_oversized: pool.stats().oversized_allocations(),
                // Move the pool out so we can keep reading its stats from
                // `main` before the program exits. Pool keeps Arc<PoolShared>
                // alive for any chunks still in the log.
                _pool: pool,
            }
        })
    };

    // ----- Reader threads ------------------------------------------------
    let reader_handles: Vec<_> = (0..num_readers)
        .map(|reader_id| {
            let log = log.clone();
            let high_water = high_water.clone();
            let running = running.clone();
            thread::spawn(move || {
                let mut reads = 0u64;
                let mut bytes = 0u64;
                let mut checksum: u64 = 0;
                let mut rng = (reader_id as u64)
                    .wrapping_mul(0x9E3779B97F4A7C15)
                    .wrapping_add(0xFEEDBEEFDEADC0DE);
                let reader_start = Instant::now();

                while running.load(Ordering::Relaxed) {
                    let hw = high_water.load(Ordering::Relaxed);
                    if hw == 0 {
                        spin_loop();
                        continue;
                    }
                    rng = lcg(rng);
                    // Target an LSN uniformly over [0, hw).
                    let target = rng % hw;
                    let chunk: Option<Chunk> =
                        log.read().unwrap().get(&target).cloned();
                    if let Some(chunk) = chunk {
                        // Read bytes. Integer add prevents elimination.
                        let mut s: u64 = 0;
                        for &b in chunk.iter() {
                            s = s.wrapping_add(b as u64);
                        }
                        checksum = checksum.wrapping_add(s);
                        reads += 1;
                        bytes += chunk.len() as u64;
                        // Cross-thread drop of `chunk` here.
                    }
                }
                ReaderResult {
                    reads,
                    bytes,
                    elapsed: reader_start.elapsed(),
                    checksum,
                }
            })
        })
        .collect();

    // Let both writer and readers run for the configured duration.
    thread::sleep(Duration::from_secs(duration_secs));
    running.store(false, Ordering::Relaxed);

    let wr = writer.join().unwrap();
    let readers: Vec<_> = reader_handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Consume checksum so the reader loops weren't dead code.
    let combined: u64 = readers.iter().fold(0u64, |a, r| a.wrapping_add(r.checksum));
    black_box(combined);

    // ----- Report --------------------------------------------------------
    let writer_secs = wr.elapsed.as_secs_f64();
    println!("─── writer ────────────────────────────────────────────────");
    println!(
        "  Wall clock: {:?}",
        wr.elapsed
    );
    println!(
        "  Records:    {} ({:.2} M/s)",
        wr.records_written,
        wr.records_written as f64 / 1e6 / writer_secs,
    );
    let avg_size = wr.bytes_in_use.checked_div(wr.records_written).unwrap_or(0);
    println!(
        "  Avg size:   {} B;  total published: {:.2} MiB",
        avg_size,
        wr.bytes_in_use as f64 / (1024.0 * 1024.0),
    );
    println!(
        "  Pool:       heap_allocs={}, pages_in_use={}, return_drains={}",
        wr.pool_heap_allocs, wr.pool_pages_in_use, wr.pool_return_drains,
    );
    println!(
        "  Paths:      packed={}, dedicated={}, oversized={}",
        wr.chunk_packed, wr.chunk_dedicated, wr.chunk_oversized,
    );

    let mut total_reads = 0u64;
    let mut total_bytes = 0u64;
    let mut max_elapsed = Duration::ZERO;
    for r in &readers {
        total_reads += r.reads;
        total_bytes += r.bytes;
        if r.elapsed > max_elapsed {
            max_elapsed = r.elapsed;
        }
    }
    let reader_secs = max_elapsed.as_secs_f64();
    println!();
    println!("─── readers ({}) ──────────────────────────────────────────", num_readers);
    println!("  Wall clock:    {:?}", max_elapsed);
    println!(
        "  Aggregate:     {} reads ({:.2} Mreads/s, {:.2} GiB/s)",
        total_reads,
        total_reads as f64 / 1e6 / reader_secs,
        total_bytes as f64 / 1_073_741_824.0 / reader_secs,
    );
    println!(
        "  Per reader:    {:.2} Mreads/s",
        (total_reads as f64 / num_readers as f64) / 1e6 / reader_secs,
    );
    println!(
        "  Per read:      {:.1} ns",
        max_elapsed.as_nanos() as f64 / total_reads.max(1) as f64,
    );

    println!();
    println!("─── tenant ────────────────────────────────────────────────");
    println!(
        "  chunks_in_use:         {}",
        tenant.stats().chunks_in_use()
    );
    println!(
        "  bytes_in_use:          {:.2} MiB",
        tenant.stats().bytes_in_use() as f64 / (1024.0 * 1024.0)
    );
    println!(
        "  total_chunks_allocated: {}",
        tenant.stats().total_chunks_allocated()
    );
    println!(
        "  peak_bytes_in_use:     {:.2} MiB",
        tenant.stats().peak_bytes_in_use() as f64 / (1024.0 * 1024.0)
    );

    // Drop the log to observe counters return to 0.
    drop(log);
    println!();
    println!("─── after dropping the log ────────────────────────────────");
    println!(
        "  chunks_in_use: {}, bytes_in_use: {}",
        tenant.stats().chunks_in_use(),
        tenant.stats().bytes_in_use(),
    );
}

/// Stats snapshot from the writer thread.
struct WriterResult {
    records_written: u64,
    elapsed: Duration,
    bytes_in_use: u64,
    pool_heap_allocs: u64,
    pool_pages_in_use: u64,
    pool_return_drains: u64,
    chunk_packed: u64,
    chunk_dedicated: u64,
    chunk_oversized: u64,
    // Kept alive for the lifetime of main so pool stats above stay
    // valid even while chunks remain in the log.
    _pool: ChunkPool,
}

struct ReaderResult {
    reads: u64,
    bytes: u64,
    elapsed: Duration,
    checksum: u64,
}

/// Knuth LCG — good enough for access-pattern randomness without
/// pulling in a `rand` dependency.
fn lcg(s: u64) -> u64 {
    s.wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}
