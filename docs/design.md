# paged-alloc — Design Note

**Status:** v0.3, living document
**Audience:** contributors and users integrating this crate into a thread-per-core database engine
**Reproducing the numbers:** `scripts/bench.sh` runs the full criterion suite and prints the same tables shown in §10.

---

## 1. Motivation

Database engines need cheap, instrumented byte buffers for data structures
that aren't well served by the global allocator directly: memtables for log
records, pinned-file caches, block caches, write buffers, etc. The common
traits of these buffers are:

- **Fixed or near-fixed size** (e.g. 4 KiB, 16 KiB, 64 KiB pages).
- **Write-once, read-many** semantics — filled once, sealed, shared across
  readers, then freed when no reader remains.
- **Multi-tenant** — allocations must be attributed so the operator can
  observe (and, in a separate layer, enforce) per-tenant usage.
- **Hot path** — dozens of millions of allocations per second per core in
  a busy ingestion workload.

This crate provides a small, self-contained allocator for exactly that
workload. It does not try to replace the global allocator, does not
provide a `GlobalAlloc` impl, and does not ship with an `unsafe` footgun
beyond the minimum needed for an intrusive free list.

## 2. Goals and non-goals

### Goals

- Single-writer, fixed-page-size allocation with a recycled free list.
- `Send + Sync` sealed pages that can be shared with reader threads.
- Per-tenant accounting visible to off-core metrics threads, with no
  cross-tenant interference.
- Linear-to-sublinear scaling in a thread-per-core deployment, paired
  with a scalable global allocator.
- No mutex, no lock, no channel on the hot path within a worker.
- Small, auditable `unsafe` surface (intrusive free list only).
- Stable Rust; no nightly features.

### Non-goals

- Replacing the global allocator or implementing `GlobalAlloc`.
- Multi-page or variable-size allocations.
- Quota **enforcement** (the library reports usage; enforcement belongs
  one layer up).
- NUMA pinning as a built-in (the `PageSource` trait is the extension
  point if it becomes needed).
- Typed reinterpret helpers (`zerocopy`, `bytemuck`) — callers layer
  these over the raw `&[u8]` API themselves.
- A global tenant registry. Aggregation across workers is the caller's
  job; keeping this out of the library avoids coupling to any particular
  threading or metrics model.

### What's shipped as of v0.3

- `HeapSource` — default, backed by `std::alloc::alloc_zeroed`.
- `MmapSource` — POSIX anonymous mmap (`cfg(unix)`).
- `HugePageSource` — Linux `MAP_HUGETLB` (`cfg(target_os = "linux")`).
- `PagePool::prewarm` — commit N pages into the free list at startup,
  invoking `PageSource::prefault` on each so lazy-backed sources
  (future huge-pages) fault synchronously outside the request path.
- **`ChunkPool` / `Chunk` / `ChunkBuilder`** (new in v0.3) — variable-
  size byte allocation layered on top of `PagePool`. The caller
  specifies the exact byte size they want; the library hides the
  underlying page structure, packing small allocations together and
  routing oversized ones direct to the heap.

## 3. Workload assumptions

- **Thread-per-core architecture.** Each OS thread (or scheduler core)
  owns its own `PagePool`. The library does not attempt to make a single
  pool safely usable from multiple allocator threads — that class of
  design (shared sync pool) is the wrong fit for linear scaling at this
  layer, as detailed in §8.
- **Multiple tenants per worker.** A single worker routinely allocates on
  behalf of many tenants, so `Tenant` is cheap to instantiate and hold.
- **Cross-core page sharing is the norm.** Sealed pages are handed to
  reader tasks on other cores (e.g. scan threads reading a memtable,
  async tasks reading cached blocks). The last `Arc` drop can land on
  any thread.
- **Metrics reads are rare relative to allocations.** A metrics thread
  polls counters on the order of per-second; workers allocate on the
  order of tens of millions per second.

## 4. Public API at a glance

```rust
use paged_alloc::{PagePool, Tenant};

let mut pool = PagePool::new(4096);       // single-writer; !Sync, Send
let tenant = Tenant::new("tenant-a");     // cheap, Clone, Send+Sync

// Optional: commit some pages up front so the first N requests don't
// touch the source's cold path.
pool.prewarm(1024);

let mut builder = pool.allocate(&tenant); // requires &mut pool
builder.append(b"hello ")?;
builder.append(b"world")?;
let page = builder.seal();                // -> Page: Send+Sync

assert_eq!(&page[..], b"hello world");
assert_eq!(tenant.stats().pages_in_use(), 1);

// Clone freely across threads.
std::thread::spawn({ let p = page.clone(); move || { let _ = &p[..]; } })
    .join().unwrap();
drop(page);
assert_eq!(tenant.stats().pages_in_use(), 0);
```

### Plugging in a different backing source

```rust
use paged_alloc::{PagePool, MmapSource};

// mmap-backed pool: 16 KiB pages, 1024 prewarmed.
let mut pool = PagePool::with_source(MmapSource::new(16 * 1024));
pool.prewarm(1024);

// On Linux, explicit 2 MiB huge pages (requires vm.nr_hugepages).
#[cfg(target_os = "linux")]
let mut huge = PagePool::with_source(
    paged_alloc::HugePageSource::new(paged_alloc::HugePageSource::SIZE_2MIB)
);
```

### Type inventory

| Type | Role | Thread-safety |
|---|---|---|
| `PagePool` | Worker-owned fixed-size-page allocator. Holds the local intrusive free-list head and an `Arc<PoolShared>`. | `Send + !Sync`. `allocate` takes `&mut self`. |
| `PoolShared` | Cross-thread-visible portion of a page pool. Owns the backing `Box<dyn PageSource>`, the atomic MPSC `return_head`, and atomic `PoolStats`. | `Send + Sync`. Held by every live `Page`. |
| `PoolStats` | Atomic `u64` counters: `allocations_from_heap`, `pages_in_use`, `free_pages`, `return_queue_drains`, `prewarmed_pages`. | Safe to read from any thread. |
| `PageBuilder` | Exclusive writable handle to a fresh page. Exposes `append`, `as_mut_slice`, `set_len`, `remaining`. Stores a raw `NonNull<u8>` buffer. | `Send + !Sync`. |
| `Page` | Immutable, reference-counted sealed page handle. Derefs to `&[u8]`. | `Send + Sync`. Drops from any thread. |
| `ChunkPool` *(new)* | Worker-owned variable-size allocator built on top of a `PagePool`. `allocate(tenant, size)` dispatches to packed / dedicated / oversized path. | `Send + !Sync`. `allocate` takes `&mut self`. |
| `ChunkBuilder` *(new)* | Exclusive writable handle to a freshly allocated chunk. Same shape as `PageBuilder` but capacity is the caller-requested byte size. | `Send + !Sync`. |
| `Chunk` *(new)* | Immutable, reference-counted chunk handle. Derefs to `&[u8]`. May share an underlying page with other chunks (packed path), own a full page (dedicated), or own a heap-allocated buffer (oversized). | `Send + Sync`. Drops from any thread. |
| `ChunkPoolStats` *(new)* | Atomic counters: `total_chunks`, `chunks_in_use`, per-path allocation counts, `oversized_bytes_in_use`. | Safe to read from any thread. |
| `PageSource` (trait) | Pluggable backing-memory strategy: `page_size`, `allocate`, `release`, `prefault`. | `Send + Sync`. |
| `HeapSource` | Default source: `alloc_zeroed` / `dealloc`. | `Send + Sync`. |
| `MmapSource` | `mmap(MAP_ANONYMOUS \| MAP_PRIVATE)` / `munmap`. `cfg(unix)`. | `Send + Sync`. |
| `HugePageSource` | `mmap(MAP_ANONYMOUS \| MAP_PRIVATE \| MAP_HUGETLB)`. `cfg(target_os = "linux")`. | `Send + Sync`. |
| `Tenant` | Cheap, cloneable handle carrying a `TenantId` and `Arc<TenantStats>`. | `Send + Sync`. |
| `TenantStats` | Atomic counters: `bytes_in_use`, `pages_in_use`, `total_pages_allocated`, `peak_bytes_in_use`, `chunks_in_use`, `total_chunks_allocated`. | Safe for any reader; writes are single-writer in the common case. Relaxed atomics. |
| `PageFull` | Error returned by `PageBuilder::append` when a write would overflow the page. | — |
| `ChunkFull` *(new)* | Error returned by `ChunkBuilder::append` when a write would overflow the chunk. | — |

## 5. Lifecycle of a page

```text
allocate(&mut pool, &tenant)
──────────────────────────────────────────────────────────
1. pool.pop_local()                              (plain pointer read)
   if null → pool.drain_return_queue()           (one AtomicPtr::swap)
     if null → vec![0u8; page_size]              (cold: heap allocation)
2. pool_stats.pages_in_use += 1                  (atomic, owner-local)
3. tenant.stats.record_allocate(page_size)       (4 atomics, owner-local)
4. return PageBuilder { buf, len=0,
                        Arc<PoolShared>, Arc<TenantStats> }

builder.append(..) / as_mut_slice / set_len
builder.seal() -> Page
──────────────────────────────────────────────────────────
5. Arc::new(PageInner { buf, shared, tenant })   (one heap allocation)
6. return Page { inner: Arc<PageInner> }

drop(Page)                                       (may run on any thread)
──────────────────────────────────────────────────────────
7. If this is the last clone:
   a. tenant.stats.record_release(page_size)     (atomic; uncontended
                                                   when drop is local)
   b. shared.push_return(buf):
        loop {
          head = return_head.load(Relaxed)
          buf[0..8] = head                       (intrusive next ptr)
          if return_head.CAS(head, buf, Release, Relaxed) break
        }
      pool_stats.pages_in_use -= 1
      pool_stats.free_pages   += 1
```

## 5.1 Pluggable backing via `PageSource`

The pool's cold path doesn't care where the bytes come from; it only
cares that they're live, writable, and `page_size` big. That's the
entire contract of the `PageSource` trait:

```rust
pub unsafe trait PageSource: Send + Sync {
    fn page_size(&self) -> usize;
    fn allocate(&self) -> NonNull<u8>;
    unsafe fn release(&self, ptr: NonNull<u8>);
    unsafe fn prefault(&self, _ptr: NonNull<u8>) {}   // default: no-op
}
```

`PoolShared` owns a `Box<dyn PageSource>`, so one level of vtable
indirection is added to the cold path and nothing at all to the hot
path (the hot path pops from the intrusive free list, which doesn't
touch the source). A measured comparison is in §10: heap-backed and
mmap-backed pools produce **identical** steady-state numbers (38.4 vs
38.3 ns/op at 16 KiB pages).

Three implementations ship today:

- **`HeapSource`** (default): `std::alloc::alloc_zeroed` with a
  pre-computed `Layout`. `prefault` is a no-op because the global
  allocator has already committed the page and zeroed it.
- **`MmapSource`** (`cfg(unix)`): one `mmap(MAP_ANONYMOUS | MAP_PRIVATE)`
  per allocation, one `munmap` per release. Buys you
  `madvise(MADV_DONTNEED)` for cache eviction and guaranteed
  OS-page alignment; costs you ~2 µs per cold-path syscall, which is
  why mmap must be paired with `prewarm` (see §5.2) or a warm free
  list to be viable.
- **`HugePageSource`** (`cfg(target_os = "linux")`): `mmap` with
  `MAP_HUGETLB` and the `MAP_HUGE_SHIFT`-encoded size selector. Two
  constants are exposed: `SIZE_2MIB` (default huge page on x86-64 and
  aarch64 Linux) and `SIZE_1GIB` (requires kernel reservation at
  boot). Expected to matter only for multi-GB working sets where TLB
  pressure at 4 KiB granularity dominates random-access latency.

Anything else — Windows `VirtualAlloc`, NUMA-pinned `mbind`,
`hugetlbfs` files, GPU pinned memory — is an additional
`PageSource` implementation with no change to `PagePool` or `Page`.

A separate `madvise_dontneed(ptr, len)` free function is exposed on
`cfg(unix)` so a higher-level cache layer can advise the kernel to
release physical backing on eviction without borrowing the pool.

## 5.2 Prewarm

ScyllaDB-style startup discipline: do the expensive, unpredictable
work (memset, first-touch page faults, mmap syscalls) during process
initialization, before admitting user traffic. Paged-alloc exposes
this via:

```rust
pub fn prewarm(&mut self, num_pages: usize);
pub fn with_capacity(page_size: usize, num_pages: usize) -> Self;
```

`prewarm` allocates `num_pages` buffers from the source, calls
`source.prefault` on each, and pushes them onto the owner-local free
list. Subsequent allocations up to `num_pages` land on the hot path
(local pop), zero syscalls, zero faults. A `PoolStats::prewarmed_pages`
counter records the total committed so metrics can surface it.

Measured on this machine, prewarming 1024 × 4 KiB pages reduces the
startup-to-ready time from **149 µs → 61 µs** (2.45× speedup), saving
~86 ns per page that would otherwise be paid on the user-visible
request path. For a future mmap or huge-page pool the savings are
much larger because `prefault` pays page-fault cost upfront instead
of during the first request that touches each page.

## 5.3 Variable-size allocation: `ChunkPool`

`PagePool` gives callers fixed-size pages. That's the right
abstraction for file-block caches, `O_DIRECT` buffers, and anything
that wants page-aligned allocations at a specific size. It's the
wrong abstraction for workloads where records don't fit the page
geometry: log records that are ~64–512 bytes each, file cache values
that exceed a page, memtable entries with heterogeneous sizes.

`ChunkPool` sits on top of `PagePool` and presents a size-based API.
Callers say "give me a chunk of N bytes" and get a [`ChunkBuilder`]
with exactly N bytes of capacity; seal it into a [`Chunk`] that
derefs to a contiguous `&[u8]` of length N. The caller never sees
the underlying page boundary.

### Three size classes

Each allocation takes one of three paths based on its size relative
to the configured `page_size`:

| Size | Path | Layout |
|---|---|---|
| `size == 0` | Zero | No backing buffer at all. Dangling-but-aligned handle returned. |
| `0 < size ≤ page_size / 2` | **Packed** | Shares an open page with other small allocations via a bump cursor. Many chunks per page. |
| `page_size / 2 < size ≤ page_size` | **Dedicated** | Takes a full fresh page from the underlying `PagePool`. One chunk per page; no packing. |
| `size > page_size` | **Oversized** | Bypasses the `PagePool` entirely; `std::alloc::alloc_zeroed(Layout)` with the exact size and alignment. |

The split between packed and dedicated at `page_size / 2` is the
standard bump-vs-slab cutoff: above half a page, you can fit at most
one chunk per page anyway, so packing buys nothing and costs you
fragmentation bookkeeping. Below half a page, packing density is
meaningful (16 KiB pages can hold 256 × 64-byte chunks).

The oversized path does not go through `PageSource`. That trait has
a fixed `page_size()` and isn't useful for one-off allocations of
arbitrary size; the global allocator is. Oversized buffers are
tracked per-pool in `ChunkPoolStats::oversized_bytes_in_use` so
observability is preserved.

### `PackedPage` and the shared-buffer invariant

The non-trivial piece is the packed path. A single underlying
`page_size`-byte buffer can back multiple live `Chunk` handles
referencing disjoint ranges of its bytes, while the pool is
simultaneously writing new allocations to the tail of the same
buffer. This would be unsound without a careful invariant.

The invariant:

> The currently-open page maintains a monotonic `write_offset`
> cursor. Every allocation claims `[align_up(cursor, align),
> cursor+size+padding)` and advances the cursor past that range.
> The range is frozen at allocation time: no subsequent writer
> touches it. `Chunk` handles are issued only after their range is
> frozen.

From this invariant:

- Multiple `Chunk` handles from the same page reference **disjoint**
  byte ranges.
- A `ChunkBuilder` writing to its range cannot overlap with any
  `Chunk`'s range on the same page, because its range is strictly
  after every previously-issued range.
- Concurrent reads through a `Chunk` (on one thread) and writes
  through a subsequent `ChunkBuilder` (on the pool-owning thread)
  touch disjoint bytes of the same buffer — no data race even
  without per-byte synchronization.

This makes the manual `unsafe impl Send for PackedPage` and `unsafe
impl Sync for PackedPage` sound: the `PackedPage`'s `NonNull<u8>` is
an owning pointer, but the type never mutably dereferences it
through a `&self` method, and all user-facing reads/writes go
through `Chunk::as_slice` / `ChunkBuilder::as_mut_slice` which carry
a `(offset, length)` that the invariant guarantees is disjoint from
every other access.

A `PackedPage` is ref-counted via `Arc<PackedPage>`. The
`ChunkPool` holds one `Arc` for its currently-open page; each
`Chunk` holds one too. When the `ChunkPool` opens a new page, it
drops its handle to the old one; when the last `Chunk` from that
old page drops, `Arc::drop` fires `PackedPage::drop`, which routes
the buffer back to its origin via the `Release` enum (pool's MPSC
return queue for pool-backed, `std::alloc::dealloc` for oversized).

### Stats: two independent levels

`ChunkPool` exposes two stats surfaces:

- `chunk_pool.stats() -> &ChunkPoolStats` — chunk-level: total
  chunks allocated (lifetime), chunks in use (current), count by
  path (packed/dedicated/oversized), oversized bytes in use.
- `chunk_pool.page_stats() -> &PoolStats` — page-level: heap
  allocations, pages in use, return-queue drains, etc. Delegates to
  the underlying `PagePool`.

`TenantStats` gets two new fields — `chunks_in_use` and
`total_chunks_allocated` — that advance on `ChunkPool` allocations
and are independent of the page-level counters. A tenant that uses
both a `PagePool` and a `ChunkPool` sees:

- `tenant.bytes_in_use()` = sum of bytes from both allocators.
- `tenant.pages_in_use()` / `total_pages_allocated()` = page
  allocations only.
- `tenant.chunks_in_use()` / `total_chunks_allocated()` = chunk
  allocations only.

No double-counting: packed pages allocated by `ChunkPool` are not
attributed to any tenant at the page level — they're shared
infrastructure. Only the chunks that carve into them are attributed
to tenants.

### Alignment

`allocate(tenant, size)` uses pointer alignment (`align_of::<*mut
u8>()`) by default. `allocate_aligned(tenant, size, align)` lets
callers request higher alignment (e.g. 64-byte SIMD alignment,
4 KiB for `O_DIRECT` staging buffers).

On the packed path, aligning up may waste a few bytes of padding
before the allocated range. This is accepted — the alternative
(refusing to pack misaligned requests, or using a separate slab per
alignment) would be complexity out of proportion to the tiny space
cost.

On the dedicated path, the whole page is returned at its natural
alignment (whatever the `PageSource` provides).

On the oversized path, the `Layout` passed to `alloc_zeroed`
specifies `max(align, pointer_align)`, so the allocator guarantees
the requested alignment.

### When to use which

| Workload | Use |
|---|---|
| `O_DIRECT` file-block cache, fixed 4 KiB or 16 KiB records | `PagePool` |
| File-content cache with heterogeneous file sizes | `ChunkPool` |
| Memtable appending log records of 10s–1000s of bytes | `ChunkPool` |
| In-memory slab of fixed-size B-tree nodes | `PagePool` if node size == page size; otherwise `ChunkPool` |
| Scratch buffers for serialization / decompression | `ChunkPool` |

`ChunkPool` is the more general tool. `PagePool` remains available
for the cases where the caller specifically wants a fixed page
boundary or page alignment.

## 6. Free list design

The free list is the core data structure and the only place the library
carries `unsafe`. It has three layers.

### 6.1 Owner-local list (hot path, no atomics)

A plain `*mut u8` stored inside `PagePool`. Access to this pointer is
gated by `&mut self` on `PagePool`, so the borrow checker enforces that
only the owning worker thread can touch it, and only one call can touch
it at a time. No `UnsafeCell`, no atomics, no locks.

The list is **intrusive**: the first `size_of::<*mut u8>()` bytes of each
free buffer store the next pointer. This costs no extra memory per node
and no per-node allocation. It does mean that callers must write before
reading — the first 8 bytes of a freshly popped buffer are "uninitialized"
in the sense that they may hold the prior next pointer. This is
documented on `PageBuilder::as_mut_slice`.

### 6.2 Cross-thread return queue (`PoolShared::return_head`)

An `AtomicPtr<u8>` intrusive **MPSC Treiber stack**:

- **Multi-producer**: any thread that holds an `Arc<PoolShared>` can push
  via `compare_exchange_weak(head, buf)`. This is how `PageInner::drop`
  returns a buffer when the last clone is on a non-owning thread.
- **Single-consumer**: only the pool owner drains, through `&mut
  PagePool::drain_return_queue`. The owner performs a single
  `swap(return_head, null)` and installs the resulting linked list as the
  local free list. Subsequent allocations pop from the local list
  without touching any atomics, until the list is empty and another
  drain is required.

Because pop is single-consumer, the classic Treiber-stack ABA hazard
does not apply: the owner is the only thread that removes nodes, so no
popped-and-re-pushed node can sneak back underneath the owner's read.
This is why we can use a CAS-free `swap` for pops instead of the more
expensive per-pop CAS.

### 6.3 Cold path (`allocate_fresh`)

When both the local list and the return queue are empty, the pool
falls back to the backing `PageSource`:

```rust
self.shared.source.allocate()
```

For `HeapSource` this is one `alloc_zeroed` call (~100–700 ns
depending on page size). For `MmapSource` it's one `mmap` syscall
(~2 µs regardless of size). Either way the pool increments
`PoolStats::allocations_from_heap`. In steady state this path is
never hit; only during warmup and in workloads whose working set
exceeds the free list.

### 6.4 What the pool does **not** do

- **No stealing** across workers. If core A has a thousand free pages
  and core B needs one, core B heap-allocates. This is deliberate: steal
  logic introduces shared state, which reintroduces cache-line
  ping-pong on the hot path.
- **No shard-of-shards.** Sharding the free list was tried and rejected
  (§8.2).
- **No size classes.** One `PagePool` serves one page size. If the caller
  needs multiple sizes, they construct multiple pools.

## 7. Tenancy model

A `Tenant` is a cheap handle:

```rust
pub struct Tenant {
    id: TenantId,                 // Arc<str>
    stats: Arc<TenantStats>,      // shared with readers
}
```

Each `(worker, tenant-id)` pair gets one `Tenant` instance. The library
intentionally does **not** maintain a global registry keyed by
`TenantId`; workers that serve the same logical tenant simply construct
their own `Tenant::new("t")` and the caller aggregates as needed.

### 7.1 Counters

`TenantStats` holds four `AtomicU64` values with relaxed ordering:

- `bytes_in_use` — sum of page sizes currently held.
- `pages_in_use` — number of pages currently held.
- `total_pages_allocated` — monotonic counter of lifetime allocations.
- `peak_bytes_in_use` — high-water mark of `bytes_in_use`, updated via a
  small compare-exchange loop.

Relaxed ordering is sound because these counters are advisory
statistics, not synchronization primitives. A reader may transiently
observe a state where `bytes_in_use` lags `pages_in_use × page_size`
during a concurrent allocation or drop; this is acceptable for a metrics
surface.

### 7.2 Single-writer vs cross-thread drop

The common case is single-writer: the allocation path touches tenant
counters from the owning worker thread, so counter writes are
uncontended (~5 ns per atomic on modern x86/ARM).

The uncommon case is a cross-thread `Page` drop — the last clone lands
on a reader thread, and `PageInner::drop` executes there. The drop path
decrements `bytes_in_use` and `pages_in_use` from a non-owner thread, so
these atomics become multi-writer for that operation. The cost grows to
~20–40 ns per atomic under real contention, which is acceptable given
the low frequency.

### 7.3 Cross-worker aggregation

"What is tenant-A using in total across all workers?" is answered by the
caller walking its per-worker bookkeeping and summing. A typical pattern
is:

```rust
struct Worker {
    pool: PagePool,
    tenants: HashMap<TenantId, Tenant>,
}

fn tenant_total_bytes(workers: &[Worker], id: &TenantId) -> u64 {
    workers.iter()
        .filter_map(|w| w.tenants.get(id))
        .map(|t| t.stats().bytes_in_use())
        .sum()
}
```

This keeps the library a leaf dependency with no global state of its
own.

### 7.4 Enforcement is out of scope

`paged-alloc` reports usage; it does not refuse allocations when a
tenant exceeds a threshold. Callers that want quotas wrap `allocate`
and check `tenant.stats().bytes_in_use()` against their own limits
before calling through.

## 8. Concurrency story

### 8.1 The design, in one paragraph

Each worker owns one `PagePool`, which is `Send + !Sync`. Allocation
takes `&mut self`, so the type system enforces that only the owning
worker can hit the allocation path. Sealed `Page`s are `Send + Sync`
because they hold an `Arc<PoolShared>` and an `Arc<TenantStats>`, both of
which are trivially thread-safe. Cross-thread drops route the buffer
back to the owning pool through the lock-free MPSC return queue
described in §6.2. Nothing is shared between workers except the one
`Arc<PoolShared>` each worker's pages happen to point at — but since
each pool has its own, there is no cross-worker cache-line ping-pong on
that either.

### 8.2 Designs that were tried and rejected

The shape of the hot path was not obvious at the start. These are the
alternatives we ran and measured:

1. **Sync pool with `Mutex<Vec<Box<[u8]>>>`** (v0, original).
   Single-thread fine; negative scaling under contention because every
   core serialized on one mutex.

2. **Sync pool with a sharded free list** (`N` shards, threads pinned to
   a shard via a thread-local seed, work-stealing on empty shard).
   Removed the mutex but did *not* fix scaling. The real bottleneck was
   cache-line ping-pong on `PoolStats` atomics, `TenantStats` atomics,
   and `Arc::clone` on `Arc<PagePool>` / `Arc<TenantStats>` — ~15–20
   atomic RMWs per op, all on shared lines. Total per-op cost grew
   roughly as `(atomics × contended_threads)`.

3. **Shared pool, lock-free MPSC free list (single-writer allocate, any-
   thread recycle)**. Simpler than the sharded design but still bounded
   by the same shared-counter contention as (2).

4. **Thread-local caches over a shared pool (tcmalloc/mimalloc shape).**
   Would scale, but ~10× more code and the thread-local-lifetime /
   thread-exit glue is the #1 source of `unsafe` bugs in production
   allocators. Disproportionate complexity for this workload.

5. **Shared-nothing per-worker pool** (the chosen design). Every
   per-thread contention source from (2) and (3) disappears because each
   worker has its own pool, its own stats, and its own `Arc`s.

### 8.3 Remaining shared resource: the global allocator

The hot path still makes one heap allocation per `seal`: the
`Arc::new(PageInner)` that wraps the buffer into a shareable handle.
Buffers themselves are reused (the pool's free list); only the small
`ArcInner<PageInner>` is fresh each time.

This per-op heap allocation is the only cross-core shared resource the
library touches, and the per-thread-cache design of the global allocator
now determines the scaling ceiling:

| Global allocator | Observed scaling shape |
|---|---|
| Per-thread caches (mimalloc, jemalloc, tcmalloc) | Near-linear at low thread counts, sublinear at high counts due to hardware factors (heterogeneous cores, memory bandwidth). |
| Central-arena (libmalloc, default glibc malloc under churn) | Flat throughput — one worker saturates the arena. |

**Decision:** document the allocator recommendation, do not work around
it in the library. An "inline ref-counted header in the buffer itself"
variant was considered (hand-rolled atomic ref-count, `ManuallyDrop`
`NonNull` headers, ~150 LOC of `unsafe`) but rejected: the fix costs the
user a single line `#[global_allocator]` in `main.rs`, which every
serious database engine already pays (ScyllaDB, TiKV, CockroachDB,
RocksDB all ship with jemalloc or equivalent).

### 8.4 Hardware ceiling: heterogeneous cores

On Apple silicon with a mix of performance and efficiency cores (M1,
M2, M3, M4 base chips all have a 4P + 4E or similar layout), the
scaling at N threads is bounded by the chip topology:

$$\text{aggregate} \approx N_P + N_E \times f_E$$

where $f_E$ is the ratio of E-core to P-core throughput on this
workload (atomic-heavy integer loop; ≈ 0.45–0.55 on M1 Icestorm
E-cores). On this document's reference M1 Mac mini (4P + 4E), that
gives a theoretical ceiling of $\approx 4 + 4 \times 0.5 = 6.0\times$
at 8 threads. The measured number is **5.11×** (see §10), right in
the envelope once you include scheduler thrashing.

**This is not a software limitation.** The 1→4 scaling trajectory
(100% → 97% → 94% → 90% per-thread efficiency) on the same machine
confirms the design is architecturally near-linear. Projected onto
homogeneous server silicon (x86-64 AMD/Intel server, Apple M*-Max /
M*-Ultra with 8+ P-cores, Graviton, Ampere Altra), the same library
binary should deliver $8 \times 0.9 \approx 7.2\times$ scaling at 8
threads.

If you need to observe that, run `scripts/bench.sh` on a cloud VM
with 8+ homogeneous cores. Nothing in the library's design changes
between the two runs — only the hardware substrate does.

## 9. Memory safety surface

The library's `unsafe` code is concentrated in `src/pool.rs`:

- `PoolShared::push_return` — writes the intrusive `next` pointer into
  the first 8 bytes of a just-released buffer, then CAS-pushes the raw
  pointer onto `return_head`.
- `PagePool::pop_local` — reads the next pointer out of the first 8
  bytes of the local head, reconstructs a `Box<[u8]>` from the raw
  pointer via `slice::from_raw_parts_mut` and `Box::from_raw`.
- `PagePool::drain_return_queue` — atomic swap of `return_head`, installs
  the resulting linked list as the new local head.
- `PagePool::Drop` — walks the intrusive list and frees every remaining
  `Box<[u8]>`.
- `unsafe impl Send for PagePool` — sound because the raw-pointer head
  is owner-local and `PagePool` is `!Sync`; transferring ownership to a
  new thread transfers the only handle to that pointer.

Every `unsafe` block has a safety comment explaining the invariants it
relies on. The invariants are:

- Any `*mut u8` on the local or shared free list points to a live,
  `page_size`-sized allocation this pool previously produced via
  `allocate_fresh` (or received from another instance of the same pool
  via `push_return`).
- The first 8 bytes of such a buffer are never read as data while the
  buffer is on a free list — they hold the `next` pointer.
- The `!Sync` marker plus `&mut self` on `allocate` ensures `local_head`
  is never touched concurrently by two threads.

No `UnsafeCell` is used. Callers outside the crate cannot write unsafe
code to interact with the allocator.

## 10. Measured cost (Apple M1 Mac mini, 4P + 4E, release, mimalloc global)

Reproducible via `scripts/bench.sh`. `steady_state` measures a tight
`allocate(&mut) → seal → drop` loop on a warm free list, so the
per-op cost is independent of page size.

### Single-threaded steady state

| Page size | Time / op | Throughput |
|---|---|---|
| 4 KiB | 38.1 ns | 100 GiB/s |
| 16 KiB | 41.6 ns | 367 GiB/s |
| 64 KiB | 41.5 ns | 1.44 TiB/s |

Heap baseline (`vec![0u8; N].into_boxed_slice()`, no pool) for contrast:
70 ns @ 4 KiB, 218 ns @ 16 KiB, 1.5 µs @ 64 KiB.

### Concurrent scaling — one `PagePool` per worker

| Threads | ns / op | Aggregate | Scaling | Per-thread efficiency |
|---|---|---|---|---|
| 1 | 38.2 ns | 26.2 Melem/s | **1.00×** | 100% |
| 2 | 19.7 ns | 50.8 Melem/s | **1.94×** | 97% |
| 3 | 13.6 ns | 73.6 Melem/s | **2.81×** | 94% |
| 4 | 10.6 ns | 94.1 Melem/s | **3.60×** | 90% |
| 8 | 7.5 ns | 133.6 Melem/s | 5.11× | 64% |

1→4 threads is near-linear (90% per-thread efficiency at 4 threads
shows the design is architecturally sound). 8-thread scaling drops
to 5.11× because the chip's 4 E-cores run this workload at ~50% of
P-core throughput — a hardware ceiling, not a software one. See §8.4
for the full projection onto homogeneous hardware.

### Heap vs mmap — cold path

Single allocation on a fresh pool (source-syscall / memset bound):

| Page size | Heap | Mmap | Mmap / Heap |
|---|---|---|---|
| 16 KiB | 292 ns | 2.18 µs | **7.5× slower** |
| 64 KiB | 719 ns | 2.14 µs | 3.0× slower |

Mmap cost is flat (~2.1 µs, syscall-bound); heap cost grows with
size (memset-bound). This is why `MmapSource` must be paired with
`prewarm` or a warm free list — a 2 µs syscall per request is 50×
too slow for a hot allocator.

### Source abstraction cost — warm free list

| Source | Time / op |
|---|---|
| `HeapSource` (16 KiB) | 38.7 ns |
| `MmapSource` (16 KiB) | 38.4 ns |

Identical. The `Box<dyn PageSource>` adds **zero per-op cost** because
the hot path never touches the source — it only pops from the local
intrusive free list. This is the key correctness result for the
pluggable-source refactor.

### Prewarm impact on startup

Time to serve N sealed pages from a cold-started worker:

| N pages | Cold start | Prewarmed | Speedup | Savings per page |
|---|---|---|---|---|
| 64 | 8.18 µs | 4.08 µs | **2.00×** | 64 ns |
| 256 | 35.3 µs | 16.0 µs | **2.21×** | 75 ns |
| 1024 | 149 µs | 61.0 µs | **2.45×** | 86 ns |

Prewarming moves the source's cold-path cost out of the request path
into process startup.

### Chunk allocation

Warm steady-state, 16 KiB underlying pages, threshold 8 KiB:

| Path | Size | Time / op |
|---|---|---|
| Packed | 64 B | 39 ns |
| Packed | 256 B | 39 ns |
| Packed | 1 KiB | 40 ns |
| Packed | 4 KiB | 44 ns |
| Dedicated | 10 KiB | 56 ns |
| Dedicated | 12 KiB | 56 ns |
| Oversized | 64 KiB | 692 ns |
| Oversized | 256 KiB | 2.5 µs |

Packed path cost is essentially the same as raw `PagePool` steady
state (~38 ns) — the extra `Arc<ChunkInner>` allocation per chunk is
the only real delta, and mimalloc serves 80-byte allocations from
its thread-local cache. Dedicated adds ~18 ns over `Page` for the
extra `Arc<PackedPage>` and chunk-level stats updates. Oversized is
dominated by `alloc_zeroed` memset time (proportional to size).

### Chunk abstraction overhead

Side-by-side at 16 KiB:

| Layer | Time |
|---|---|
| `PagePool::allocate → seal → drop` | 38 ns |
| `ChunkPool::allocate(size=page_size)` (dedicated path) | 57 ns |

The +19 ns (+50%) overhead is the price of variable-size
abstraction and chunk-level accounting. Users who specifically need
fixed-page semantics should prefer `PagePool` directly; users with
variable-size records should use `ChunkPool` and accept this cost.

### Other

- **`cross_thread_drop`** (page allocated on owner, dropped on N sibling threads): 72 ns (1 dropper) → 172 ns (8 droppers). Atomic CAS on `return_head` contends as the dropper count grows; single-dropper cost is just one CAS.
- **`page_clone`** (Arc bump): 19 ns for one clone, ~9 ns amortized per clone in bulk.
- **`append_fill_page`** (16 KiB page filled via `append` in 1 KiB chunks): 325 ns / page, 47 GiB/s — memcpy-bound.

## 11. Code map

```
src/
├── lib.rs       Crate docs, module wiring, public re-exports.
├── tenant.rs    TenantId, TenantStats (atomic counters — page-level
│                and chunk-level, single-writer peak update via
│                load-then-store), Tenant.
├── source.rs    PageSource trait + HeapSource (alloc_zeroed / dealloc).
├── mmap.rs      cfg(unix) MmapSource; cfg(target_os="linux")
│                HugePageSource; madvise_dontneed free function.
├── pool.rs      PagePool (local intrusive head, allocate(&mut self),
│                allocate_raw_page, prewarm, with_capacity, with_source),
│                PoolShared (Box<dyn PageSource>, AtomicPtr return_head,
│                atomic PoolStats, push_return / drain_return_queue).
├── page.rs      PageFull error, PageBuilder (write phase, NonNull<u8>
│                buffer), Page (Arc<PageInner>, Send+Sync, derefs to
│                &[u8]), PageInner::drop → push_return.
└── chunk.rs     ChunkPool (packed/dedicated/oversized dispatch on top
                 of PagePool::allocate_raw_page + std::alloc for
                 oversized), ChunkBuilder, Chunk, ChunkInner,
                 ChunkPoolStats, internal PackedPage with Release enum.

tests/
└── integration.rs  35 tests including, for PagePool:
                    - cross_thread_drop_returns_via_atomic_queue
                    - concurrent_per_worker_pools
                    - tenant_stats_track_bytes_and_peak
                    - page_send_sync_across_reader_threads
                    - prewarm_populates_free_list
                    - prewarm_calls_prefault_on_every_page
                    - pool_drop_releases_buffers_still_on_return_queue
                    - mmap_source_end_to_end
                    - madvise_dontneed_zeroes_region_linux (Linux-only)
                    and for ChunkPool:
                    - chunk_small_packs_into_shared_page
                    - chunk_dedicated_takes_whole_page
                    - chunk_oversized_bypasses_pool
                    - chunk_packing_rolls_to_next_page_when_full
                    - chunk_packed_page_retains_until_last_chunk_drops
                    - chunk_cross_thread_drop
                    - chunk_send_sync_readers
                    - chunk_prewarm_avoids_cold_path_for_first_allocs
                    - chunk_aligned_allocation_pads_packed_offset
                    - chunk_page_and_chunk_stats_are_independent

benches/
└── alloc.rs     12 criterion benches:
                 - steady_state_alloc_seal_drop
                 - heap_baseline_box_slice
                 - cold_alloc_seal_drop
                 - append_fill_page
                 - page_clone
                 - concurrent_per_worker (1/2/3/4/8 threads)
                 - cross_thread_drop     (1/2/4/8 droppers)
                 - startup_ready         (cold vs prewarmed, 64/256/1024)
                 - source_cold_path      (heap vs mmap, 16 KiB/64 KiB)
                 - source_steady_state   (heap vs mmap, warm free list)
                 - chunk_alloc           (packed/dedicated/oversized)
                 - chunk_vs_page_overhead (abstraction cost at 16 KiB)
                 Uses mimalloc in the harness to surface pool-level
                 scaling independent of libmalloc.

scripts/
└── bench.sh     Runs `cargo bench` and pretty-prints a report with
                 scaling factors, per-thread efficiency, heap-vs-mmap
                 comparison, and prewarm impact. `--skip-run` re-reads
                 existing target/criterion data. `--quick` shortens
                 measurement time. System topology (P/E core split on
                 Apple silicon) is detected and printed.

docs/
└── design.md    This document.
```

## 12. Extension points

Each of these was scoped out and can be layered on without reshaping
the core. Several were speculative in v0.1 and have since been
implemented:

**Already shipped in v0.2:**

- ✅ **`PageSource` trait** — pluggable backing memory. `PoolShared`
  owns a `Box<dyn PageSource>`; `HeapSource`, `MmapSource`,
  `HugePageSource` all implement it. Adding new sources (Windows
  `VirtualAlloc`, NUMA `mbind`, GPU pinned memory) is additive — no
  change to `PagePool` or `Page`.
- ✅ **`prewarm` API** — ScyllaDB-style startup: commit N pages and
  fault them in before serving traffic.
- ✅ **`madvise_dontneed` helper** — exposed on `cfg(unix)` so a
  higher-level cache layer can release physical backing on eviction.

**Already shipped in v0.3:**

- ✅ **Variable-size allocation** via `ChunkPool` / `Chunk`. Packed
  small-record storage, dedicated full-page, oversized
  heap-direct. Page size no longer leaks into the user-facing API
  for the common case.

**Still out of scope but buildable on top:**

- **Quota enforcement.** A thin wrapper type over `PagePool` that
  checks `tenant.stats().bytes_in_use()` against a limit before
  calling `allocate`, returning an error on overage.
- **Typed reinterpret.** Callers layer `zerocopy` or `bytemuck` on
  `Page::as_slice()` / `PageBuilder::as_mut_slice()` as needed.
- **Multi-page / segmented buffers.** Build a `PageChain` type at a
  higher layer that owns `Vec<Page>`. The allocator itself keeps its
  single-page contract.
- **Global tenant registry.** ~50 lines over `Tenant`: a
  `RwLock<HashMap<TenantId, Vec<Weak<TenantStats>>>>` touched only at
  worker creation / teardown and at metrics scrape. Add it to the
  library if caller bookkeeping becomes tedious.
- **Lock-free sharded free list / thread-local caches.** If a single
  pool must be shared by multiple allocator threads after all, the
  internals could be swapped to a tcmalloc-style design without
  changing the public API. Today's design deliberately rejects this
  in favor of thread-per-core.
- **Windows `VirtualAlloc` backend.** A `WindowsSource` implementing
  `PageSource` — not a design change, just new code behind
  `cfg(target_os = "windows")`.

## 13. Open questions

- **Should the cold-path buffer allocation be zero-initialized?** Today
  it is (`alloc_zeroed` in `HeapSource`, lazy zero-fill in `MmapSource`),
  which costs a memset at creation time for the heap backend. Recycled
  buffers are **not** zeroed — the "write before read" contract on
  `PageBuilder::as_mut_slice` and `ChunkBuilder::as_mut_slice` allows
  stale bytes. Moving the cold path to `Box::new_uninit_slice` for
  `HeapSource` would save the first-touch memset at the cost of some
  unsafe transmute. Unclear whether real workloads see this — the
  prewarm path already amortizes this cost to startup.
- **Is a `Tenant::snapshot()` helper worth adding?** Today callers
  sum fields individually; a one-call snapshot would avoid a torn read
  across multiple atomics. Low priority; most metrics systems tolerate
  brief inconsistency between related counters.

(An earlier open question about `PagePool::Drop` memory fencing was
resolved during the v0.2 work: `PoolShared::Drop` now correctly drains
the return queue with an `Acquire` swap, and this handles the
"pool dropped while `Page` clones still live" case via the
`Arc<PoolShared>` outliving any live page. See §6 for the full
ownership and lifetime story.)

## 14. Revision history

- **v0.1 (initial)**
  - Original: sync pool, mutex free list. Rejected for scaling.
  - Sharded free list + thread pinning. Rejected for scaling
    (stats/Arc contention).
  - Single-writer pool + lock-free MPSC free list. Pairs with a
    scalable global allocator for near-linear scaling.

- **v0.3**
  - Added `ChunkPool`, `ChunkBuilder`, `Chunk`, `ChunkPoolStats`,
    `ChunkFull`. Variable-size byte allocator layered on `PagePool`
    with three-path dispatch (packed / dedicated / oversized). Page
    size is now hidden from the user-facing API for the common
    memtable / cache use cases.
  - Internal `PackedPage` type with `Release { Pool, Heap }` enum to
    route buffer release either back through the `PagePool`'s MPSC
    return queue or directly to `std::alloc::dealloc` for oversized
    allocations.
  - Extended `TenantStats` with `chunks_in_use` and
    `total_chunks_allocated`; `record_chunk_allocate` /
    `record_chunk_release` methods. Page-level and chunk-level
    counters advance independently.
  - Added `pub(crate) PagePool::allocate_raw_page` so `ChunkPool`
    can grab raw buffers without the `PageBuilder` wrapping or
    per-page tenant accounting.
  - 17 new integration tests for packed sharing, dedicated, oversized,
    packing-roll, cross-thread drop, alignment, prewarm interaction,
    stats independence.
  - 2 new criterion bench groups (`chunk_alloc`,
    `chunk_vs_page_overhead`) and a new section in
    `scripts/bench.sh` that prints them.
  - Measured: packed path ~40 ns (same as raw `PagePool`),
    dedicated path ~57 ns (+19 ns for Arc<ChunkInner> +
    Arc<PackedPage>), oversized ~700 ns for 64 KiB (memset-bound).

- **v0.2**
  - Extracted the backing-memory strategy behind a `PageSource` trait.
    `PoolShared` now owns a `Box<dyn PageSource>`; the trait is
    future-proof for mmap, huge pages, Windows `VirtualAlloc`, NUMA
    pinning, GPU pinned memory, etc. Measured cost of the abstraction:
    **zero** per-op (hot path never touches the source).
  - Internal buffer representation moved from `Box<[u8]>` to
    `NonNull<u8>`. Buffers are now released through the source's
    `release` method, enabling `MmapSource` to `munmap` correctly.
  - `MmapSource` implementation (POSIX, `cfg(unix)`). Exposes
    `madvise_dontneed` as a free function for cache-eviction code.
  - `HugePageSource` implementation (Linux, `MAP_HUGETLB` with
    `SIZE_2MIB` / `SIZE_1GIB` constants). Compile-checked on
    non-Linux, not runtime-tested on this document's reference
    machine.
  - `PagePool::prewarm(N)` and `PagePool::with_capacity(page_size, N)`.
    Commits N pages and calls `PageSource::prefault` on each.
  - Single-writer peak-update optimization in `TenantStats`: the CAS
    loop in `record_allocate` is replaced with a plain load-then-store
    now that allocation is known to be single-writer.
  - `scripts/bench.sh`: parses `target/criterion/**/estimates.json`
    and prints a pretty report with scaling factors, per-thread
    efficiency, heap-vs-mmap comparison, and prewarm impact.
  - Concurrent scaling extended to include a 3-thread data point so
    the near-linear 1→4 trajectory is visible independent of the
    heterogeneous-core-induced dropoff at 8 threads.
  - Measured steady-state improvement from ~41 ns → 38 ns per op.
    Added `[profile.release] lto="thin" codegen-units=1` to make
    bench numbers match the production build shape.
