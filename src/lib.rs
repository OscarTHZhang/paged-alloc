//! Paged memory allocator for thread-per-core database engines.
//!
//! Provides two layered APIs:
//!
//! - [`ChunkPool`] — variable-size, size-based allocation. Callers ask
//!   for N bytes; the pool packs small allocations together inside
//!   recycled fixed-size pages, gives medium allocations a full page,
//!   and routes oversized allocations straight to the global
//!   allocator. The underlying page boundary is hidden. This is the
//!   API most callers want for memtables, log records, file-cache
//!   values, and other variable-size workloads.
//!
//! - [`PagePool`] — fixed-size aligned pages. Callers receive a whole
//!   page of the pool's configured size. Use this when the caller
//!   specifically needs a known page size or page alignment —
//!   `O_DIRECT` I/O staging, file-block caches, SIMD loads.
//!
//! Both pools are **single-writer per worker**: they are `Send + !Sync`
//! and `allocate` takes `&mut self`. A thread-per-core application
//! creates one pool per worker thread. Sealed [`Chunk`] and [`Page`]
//! handles are `Send + Sync + Clone`, so they can be freely shared
//! with reader threads; when the last clone drops — on any thread —
//! the buffer returns to the owning pool via a lock-free MPSC
//! return queue.
//!
//! # Short example
//!
//! The one-call form, for the common "I have bytes, store them" case:
//!
//! ```
//! use paged_alloc::{ChunkPool, Tenant};
//!
//! let mut pool = ChunkPool::new();
//! let tenant = Tenant::new("file-cache");
//!
//! let chunk = pool.alloc_from(&tenant, b"hello world");
//! assert_eq!(&chunk[..], b"hello world");
//! assert_eq!(tenant.stats().bytes_in_use(), 11);
//! ```
//!
//! The closure form, for composing records in place:
//!
//! ```
//! # use paged_alloc::{ChunkPool, Tenant};
//! # let mut pool = ChunkPool::new();
//! # let tenant = Tenant::new("t");
//! let record = pool.alloc_with(&tenant, 16, |buf| {
//!     buf[..6].copy_from_slice(b"header");
//!     buf[6..16].fill(0);
//! });
//! assert_eq!(&record[..6], b"header");
//! ```
//!
//! The builder form, for multi-step writes with error handling or
//! conditional bailout:
//!
//! ```
//! # use paged_alloc::{ChunkPool, Tenant};
//! # let mut pool = ChunkPool::new();
//! # let tenant = Tenant::new("t");
//! let mut b = pool.allocate(&tenant, 32);
//! b.append(b"[").unwrap();
//! b.append(b"payload").unwrap();
//! b.append(b"]").unwrap();
//! let chunk = b.seal();
//! assert_eq!(&chunk[..], b"[payload]");
//! ```
//!
//! None of the forms require an explicit `drop` — when the `Chunk`
//! (or a `Page`) goes out of scope, or the last `Clone` of it drops,
//! Rust's RAII returns the buffer to the pool and decrements tenant
//! counters.
//!
//! # Features
//!
//! - **Multi-tenant accounting.** [`Tenant`] is a cheap handle that
//!   carries [`TenantStats`] — atomic counters (`bytes_in_use`,
//!   `chunks_in_use`, `pages_in_use`, peak values) that any thread
//!   can read safely for metrics scraping.
//! - **Pluggable backing memory.** The [`PageSource`] trait lets you
//!   swap the underlying memory provider. [`HeapSource`] (default) is
//!   backed by [`std::alloc`]; [`MmapSource`] (cfg(unix)) is backed
//!   by `mmap(MAP_ANONYMOUS | MAP_PRIVATE)` and enables cache-eviction
//!   patterns via [`madvise_dontneed`]; `HugePageSource`
//!   (cfg(target_os = "linux")) uses `MAP_HUGETLB` for workloads with
//!   multi-GiB hot working sets.
//! - **Prewarm at startup.** [`ChunkPool::prewarm`] and
//!   [`PagePool::prewarm`] commit N pages up front and invoke
//!   [`PageSource::prefault`] on each, moving cold-path cost out of
//!   the request window.
//! - **No mutex, no atomics on the hot path within a worker.** The
//!   free list lives in an owner-local intrusive stack, and the hot
//!   path never touches shared state. The MPSC return queue is only
//!   touched on cross-thread drop.
//!
//! # What it does not do
//!
//! - Does not replace the global allocator. This crate is a layer on
//!   top of [`std::alloc`] (and optionally `mmap`).
//! - Does not enforce quotas — accounting only. Callers that want
//!   hard per-tenant limits wrap [`ChunkPool::allocate`] with their
//!   own admission check.
//! - Does not ship a map / tree data structure. Use
//!   `std::collections::{HashMap, BTreeMap}` or similar and make
//!   [`Chunk`] (or [`Page`]) the value type.
//!
//! # Example programs
//!
//! See the `examples/` directory. Progression from simplest to most
//! realistic:
//!
//! - `hello`, `pages`, `tenants`, `prewarm`, `share_across_threads` —
//!   focused one-concept examples.
//! - `file_cache` — realistic concurrent file cache:
//!   `Arc<RwLock<HashMap<FileId, Chunk>>>` with multiple reader
//!   threads.
//! - `append_log` — realistic append-only log: writer thread owns the
//!   pool and publishes into `Arc<RwLock<BTreeMap<Lsn, Chunk>>>`;
//!   reader threads do concurrent point-lookups.
//!
//! Run with `cargo run --release --example <name>`.
//!
//! # Design note
//!
//! `docs/design.md` in the repository has the full design: free-list
//! invariants, safety arguments for the shared-buffer packing,
//! concurrency tradeoffs, measured scaling, and extension points.

mod chunk;
mod page;
mod pool;
mod source;
mod tenant;

#[cfg(unix)]
mod mmap;

pub use chunk::{Chunk, ChunkBuilder, ChunkFull, ChunkPool, ChunkPoolStats};
pub use page::{Page, PageBuilder, PageFull};
pub use pool::{PagePool, PoolShared, PoolStats};
pub use source::{HeapSource, PageSource};
pub use tenant::{Tenant, TenantId, TenantStats};

#[cfg(unix)]
pub use mmap::{MmapSource, madvise_dontneed};

#[cfg(target_os = "linux")]
pub use mmap::HugePageSource;
