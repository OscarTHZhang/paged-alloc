use std::sync::Arc;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use paged_alloc::{PagePool, Tenant};

// Bench harness only: use mimalloc so we can distinguish pool-level
// scaling from system-allocator scaling. This does not affect the
// library or its users.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const PAGE_SIZES: &[usize] = &[4 * 1024, 16 * 1024, 64 * 1024];

/// Steady-state alloc + seal + drop on a warm local free list.
fn bench_steady_state(c: &mut Criterion) {
    let mut group = c.benchmark_group("steady_state_alloc_seal_drop");
    for &page_size in PAGE_SIZES {
        let mut pool = PagePool::new(page_size);
        let tenant = Tenant::new("bench");
        // Prime the free list with a drop so the first timed iteration is hot.
        drop(pool.allocate(&tenant).seal());

        group.throughput(Throughput::Bytes(page_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(page_size),
            &page_size,
            |b, _| {
                b.iter(|| {
                    let page = pool.allocate(&tenant).seal();
                    black_box(&page);
                });
            },
        );
    }
    group.finish();
}

/// `vec![0u8; N].into_boxed_slice()` baseline — what the pool replaces on
/// the cold path.
fn bench_heap_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("heap_baseline_box_slice");
    for &page_size in PAGE_SIZES {
        group.throughput(Throughput::Bytes(page_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(page_size),
            &page_size,
            |b, &size| {
                b.iter(|| {
                    let buf: Box<[u8]> = vec![0u8; size].into_boxed_slice();
                    black_box(buf);
                });
            },
        );
    }
    group.finish();
}

/// Cold-path: each iteration uses a fresh pool, so the allocation must go
/// to the heap.
fn bench_cold_alloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold_alloc_seal_drop");
    for &page_size in PAGE_SIZES {
        group.throughput(Throughput::Bytes(page_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(page_size),
            &page_size,
            |b, &size| {
                b.iter_batched(
                    || (PagePool::new(size), Tenant::new("bench")),
                    |(mut pool, tenant)| {
                        let page = pool.allocate(&tenant).seal();
                        black_box(&page);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

/// Fill an entire page via `append` in small chunks.
fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("append_fill_page");
    let page_size = 16 * 1024;
    let mut pool = PagePool::new(page_size);
    let tenant = Tenant::new("bench");
    for &chunk in &[16usize, 64, 256, 1024] {
        let payload = vec![0xabu8; chunk];
        group.throughput(Throughput::Bytes(page_size as u64));
        group.bench_with_input(
            BenchmarkId::new("chunk_bytes", chunk),
            &chunk,
            |b, _| {
                b.iter(|| {
                    let mut builder = pool.allocate(&tenant);
                    while builder.remaining() >= payload.len() {
                        builder.append(black_box(&payload)).unwrap();
                    }
                    black_box(builder.seal());
                });
            },
        );
    }
    group.finish();
}

/// Clone a sealed page N times then drop the clones.
fn bench_page_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("page_clone");
    let mut pool = PagePool::new(4096);
    let tenant = Tenant::new("bench");
    for &n in &[1usize, 4, 16, 64] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let page = pool.allocate(&tenant).seal();
            b.iter(|| {
                let clones: Vec<_> = (0..n).map(|_| page.clone()).collect();
                black_box(clones);
            });
        });
    }
    group.finish();
}

/// Aggregate throughput under thread-per-core: each worker owns its own
/// pool and tenant. Ideal linear scaling means N× the single-thread
/// number.
fn bench_concurrent_per_worker(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_per_worker");
    group.measurement_time(Duration::from_secs(5));
    group.throughput(Throughput::Elements(1));
    let page_size = 4096;

    for &threads in &[1usize, 2, 3, 4, 8] {
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let per_thread = (iters as usize).div_ceil(threads).max(1);
                    let barrier = Arc::new(Barrier::new(threads + 1));
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let barrier = barrier.clone();
                            thread::spawn(move || {
                                // Each worker owns its own pool.
                                let mut pool = PagePool::new(page_size);
                                let tenant = Tenant::new("bench");
                                // Prime.
                                drop(pool.allocate(&tenant).seal());
                                barrier.wait();
                                for _ in 0..per_thread {
                                    let page = pool.allocate(&tenant).seal();
                                    black_box(&page);
                                }
                            })
                        })
                        .collect();
                    barrier.wait();
                    let start = Instant::now();
                    for h in handles {
                        h.join().unwrap();
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

/// Heap vs mmap cold-path comparison. Each iteration creates a fresh
/// pool (no prewarm) and does one allocate+seal+drop, so the source's
/// cold path dominates.
#[cfg(unix)]
fn bench_source_cold_path(c: &mut Criterion) {
    use paged_alloc::MmapSource;
    let mut group = c.benchmark_group("source_cold_path");
    // Use page sizes ≥ 16 KiB so the bench runs on both Linux (4 KiB
    // OS pages) and arm64 macOS (16 KiB OS pages).
    for &page_size in &[16 * 1024usize, 64 * 1024] {
        group.throughput(Throughput::Bytes(page_size as u64));

        group.bench_with_input(
            BenchmarkId::new("heap", page_size),
            &page_size,
            |b, &size| {
                b.iter_batched(
                    || (PagePool::new(size), Tenant::new("bench")),
                    |(mut pool, tenant)| {
                        let page = pool.allocate(&tenant).seal();
                        black_box(&page);
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("mmap", page_size),
            &page_size,
            |b, &size| {
                b.iter_batched(
                    || {
                        (
                            PagePool::with_source(MmapSource::new(size)),
                            Tenant::new("bench"),
                        )
                    },
                    |(mut pool, tenant)| {
                        let page = pool.allocate(&tenant).seal();
                        black_box(&page);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

/// Steady-state with mmap vs heap backing on a warm free list. Should
/// be identical in steady state since the free list is doing all the
/// work; this confirms the source abstraction has no per-op cost.
#[cfg(unix)]
fn bench_source_steady_state(c: &mut Criterion) {
    use paged_alloc::MmapSource;
    let mut group = c.benchmark_group("source_steady_state");
    let page_size = 16 * 1024;
    group.throughput(Throughput::Bytes(page_size as u64));

    {
        let mut pool = PagePool::new(page_size);
        let tenant = Tenant::new("bench");
        drop(pool.allocate(&tenant).seal());
        group.bench_function("heap", |b| {
            b.iter(|| {
                let page = pool.allocate(&tenant).seal();
                black_box(&page);
            });
        });
    }
    {
        let mut pool = PagePool::with_source(MmapSource::new(page_size));
        let tenant = Tenant::new("bench");
        drop(pool.allocate(&tenant).seal());
        group.bench_function("mmap", |b| {
            b.iter(|| {
                let page = pool.allocate(&tenant).seal();
                black_box(&page);
            });
        });
    }
    group.finish();
}

/// Measures the cross-thread drop path. Each logical op: the owner thread
/// allocates and seals a page, hands it to a spawned dropper thread, and
/// joins. The page drop happens on the non-owner thread, so its buffer
/// returns via the atomic MPSC queue. This isolates the CAS cost on the
/// cross-thread return path.
fn bench_cross_thread_drop(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread_drop");
    group.measurement_time(Duration::from_secs(5));
    group.throughput(Throughput::Elements(1));
    let page_size = 4096;

    // Fanout: one owner, N dropper threads each doing iters/N drops in
    // parallel. This stresses multi-producer contention on return_head.
    for &droppers in &[1usize, 2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::from_parameter(droppers),
            &droppers,
            |b, &droppers| {
                b.iter_custom(|iters| {
                    let per_dropper = (iters as usize).div_ceil(droppers).max(1);
                    let total_ops = per_dropper * droppers;

                    let mut pool = PagePool::new(page_size);
                    let tenant = Tenant::new("bench");
                    // Pre-allocate `total_ops` sealed pages so the owner
                    // is not in the timed path.
                    let mut pages: Vec<_> = (0..total_ops)
                        .map(|_| pool.allocate(&tenant).seal())
                        .collect();

                    let barrier = Arc::new(Barrier::new(droppers + 1));
                    let mut chunks: Vec<Vec<_>> = (0..droppers).map(|_| Vec::with_capacity(per_dropper)).collect();
                    for (i, page) in pages.drain(..).enumerate() {
                        chunks[i % droppers].push(page);
                    }

                    let handles: Vec<_> = chunks
                        .into_iter()
                        .map(|chunk| {
                            let barrier = barrier.clone();
                            thread::spawn(move || {
                                barrier.wait();
                                for page in chunk {
                                    drop(page);
                                }
                            })
                        })
                        .collect();
                    barrier.wait();
                    let start = Instant::now();
                    for h in handles {
                        h.join().unwrap();
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

/// Startup latency for the first N allocations, with and without a
/// prewarm call. Measures the time to make N pages ready to serve,
/// mimicking a worker's startup-before-serving window.
fn bench_startup_ready(c: &mut Criterion) {
    let mut group = c.benchmark_group("startup_ready");
    let page_size = 4096;
    for &n in &[64usize, 256, 1024] {
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("cold_first_allocs", n), &n, |b, &n| {
            b.iter_batched(
                || (PagePool::new(page_size), Tenant::new("bench")),
                |(mut pool, tenant)| {
                    // Simulate the first N user allocations of a fresh
                    // worker — every one hits the source's cold path.
                    let mut sink = Vec::with_capacity(n);
                    for _ in 0..n {
                        sink.push(pool.allocate(&tenant).seal());
                    }
                    black_box(sink);
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("prewarm_then_allocs", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut pool = PagePool::new(page_size);
                    pool.prewarm(n);
                    (pool, Tenant::new("bench"))
                },
                |(mut pool, tenant)| {
                    // After prewarm the N pages come from the local
                    // list with no source calls.
                    let mut sink = Vec::with_capacity(n);
                    for _ in 0..n {
                        sink.push(pool.allocate(&tenant).seal());
                    }
                    black_box(sink);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

#[cfg(unix)]
criterion_group!(
    benches,
    bench_steady_state,
    bench_heap_baseline,
    bench_cold_alloc,
    bench_append,
    bench_page_clone,
    bench_concurrent_per_worker,
    bench_cross_thread_drop,
    bench_startup_ready,
    bench_source_cold_path,
    bench_source_steady_state,
);
#[cfg(not(unix))]
criterion_group!(
    benches,
    bench_steady_state,
    bench_heap_baseline,
    bench_cold_alloc,
    bench_append,
    bench_page_clone,
    bench_concurrent_per_worker,
    bench_cross_thread_drop,
    bench_startup_ready,
);
criterion_main!(benches);
