# paged-alloc

A paged memory allocator for thread-per-core database engines.

Designed to back data structures where allocation needs to be instrumented
and multi-tenant-aware: memtables for log records, pinned file-block
caches, scratch buffers for serialization, B-tree node pools, and
similar. Chunks are variable-size and tenant-attributed, buffers are
reused from a recycled free list, and sealed values are `Send + Sync`
so they can be shared with reader threads across cores.

```rust
use paged_alloc::{ChunkPool, Tenant};

let mut pool = ChunkPool::new();
let tenant = Tenant::new("file-cache");

let mut b = pool.allocate(&tenant, 256);
b.append(header)?;
b.append(payload)?;
let chunk = b.seal();           // Send + Sync + Clone, derefs to &[u8]

assert_eq!(tenant.stats().bytes_in_use(), 256);
```

## Highlights

- **Shared-nothing thread-per-core.** Pools are `Send + !Sync`;
  allocation takes `&mut self`. No mutex, no atomics on the hot path
  within a worker.
- **Cross-thread-safe sealed values.** A sealed `Chunk` (or `Page`)
  is `Send + Sync + Clone`. When the last clone drops — on any thread
  — the buffer returns to the owning pool through a lock-free MPSC
  return queue.
- **Variable-size and fixed-size APIs.** `ChunkPool` hides the page
  boundary and lets you ask for exact byte counts (small allocations
  pack together, oversized bypass the pool). `PagePool` is available
  when you specifically need fixed-size or page-aligned buffers
  (O_DIRECT, block caches).
- **Pluggable backing memory.** `PageSource` trait; ships with
  `HeapSource` (default, `std::alloc`), `MmapSource` (POSIX mmap,
  `cfg(unix)`), and `HugePageSource` (Linux `MAP_HUGETLB`,
  `cfg(target_os = "linux")`).
- **Startup prewarm.** `pool.prewarm(N)` commits N pages + calls
  `PageSource::prefault` on each, moving cold-path cost out of the
  request window.
- **Multi-tenant accounting.** `Tenant` is a cheap handle carrying
  atomic counters a metrics thread can scrape directly from any
  thread (bytes/pages/chunks in use, peak, totals).

## What it does not do

- Does not replace the global allocator — this crate is a layer
  *on top of* `std::alloc` (and optionally `mmap`).
- Does not enforce quotas — reports per-tenant usage, leaves
  admission control to the caller.
- Does not implement a map/tree data structure — callers use the
  standard library's collections (`HashMap<FileId, Chunk>`,
  `BTreeMap<Lsn, Chunk>`, etc.) and paged-alloc backs the values.
- No nightly Rust features, no `unsafe` in public API.

## Performance

On an Apple M1 Mac mini, with the default `HeapSource` plus mimalloc
as the global allocator:

| Bench | Result |
|---|---|
| `ChunkPool::allocate(…, 64)` + seal + drop, warm | 39 ns / op |
| `PagePool::allocate` + seal + drop, warm, 4 KiB | 38 ns / op |
| Concurrent 4-thread scaling | 3.48× (87% per-thread efficiency) |
| Concurrent 8-thread scaling | 4.84× (60%, limited by 4P+4E silicon) |
| Cross-thread drop (1 dropper) | 49 ns |
| Prewarm speedup (1024 × 4 KiB) | 2.78× |

Full tables, methodology, and a reproducible harness in
`docs/design.md` §10 and `scripts/bench.sh`.

### Recommended global allocator

The hot path does one `Arc::new(ChunkInner)` (or `Arc::new(PageInner)`)
per `seal`, served from the process-global allocator. Scaling therefore
depends on that allocator having **per-thread caches**. The numbers
above assume [mimalloc](https://crates.io/crates/mimalloc) or
[jemalloc](https://crates.io/crates/tikv-jemallocator). With the
default system allocator on Linux/macOS (central-arena
`malloc`), 8-thread aggregate throughput flattens to roughly
single-thread throughput — this is not a limitation of paged-alloc,
but of the allocator serving its small internal allocations.

Typical production setup:

```toml
# in your application's Cargo.toml
[dependencies]
mimalloc = { version = "0.1", default-features = false }
```

```rust
// in your main.rs
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Every serious Rust database engine (TiKV, Databend, Materialize, etc.)
ships with jemalloc or mimalloc for the same reason. `paged-alloc`'s
bench harness already opts in to mimalloc, which is why the reported
numbers match what a real deployment sees.

## Getting started

```bash
# Run the simplest example.
cargo run --release --example hello

# Explore realistic workloads.
cargo run --release --example file_cache
cargo run --release --example append_log

# Reproduce the measurements.
scripts/bench.sh              # ~5 minutes
scripts/bench.sh --quick      # ~90 seconds
scripts/bench.sh --skip-run   # just reparse existing data
```

Seven examples in `examples/`:

| File | Shows |
|---|---|
| `hello.rs` | Simplest `ChunkPool` usage |
| `pages.rs` | Simplest `PagePool` usage with buffer reuse |
| `tenants.rs` | Multi-tenant accounting + cross-thread metrics reads |
| `prewarm.rs` | Cold vs prewarmed startup timing |
| `share_across_threads.rs` | Fan-out with `Chunk::clone` |
| `file_cache.rs` | Realistic `HashMap<FileId, Chunk>` with concurrent readers |
| `append_log.rs` | Realistic append-only log: writer + reader threads |

## Design

The full design note is [`docs/design.md`](docs/design.md). Highlights:

- Free-list design (owner-local intrusive list + MPSC return queue)
- Three-path chunk dispatch (packed / dedicated / oversized)
- Safety argument for `PackedPage`'s shared-buffer invariant
- Concurrency story including the designs tried and rejected
- Measured scaling with hardware-aware ceiling analysis
- Extension points that were left buildable on top

## Layout

```
src/
├── lib.rs        Crate docs and re-exports
├── tenant.rs     Tenant, TenantStats (atomic counters)
├── source.rs     PageSource trait + HeapSource
├── mmap.rs       cfg(unix) MmapSource + cfg(linux) HugePageSource
├── pool.rs       PagePool + PoolShared (fixed-size pages)
├── page.rs       Page, PageBuilder
└── chunk.rs      ChunkPool, Chunk, ChunkBuilder (variable-size)

tests/integration.rs    36 tests covering every public API path
benches/alloc.rs        12 criterion bench groups
scripts/bench.sh        Bench runner + formatted report
examples/               7 runnable examples
docs/design.md          Full design note (v0.3)
```

## License

MIT. See [LICENSE](LICENSE).
