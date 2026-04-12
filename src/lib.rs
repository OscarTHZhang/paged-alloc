//! Paged allocator for thread-per-core database engines.
//!
//! A [`PagePool`] is a single-writer, fixed-page-size allocator intended
//! to be owned by one worker thread (it is `Send` but `!Sync`). Allocation
//! takes `&mut self`, so the hot path has no mutex and no atomics for
//! the free list. Callers in a thread-per-core setup create one
//! `PagePool` per worker.
//!
//! Pages are write-once: callers receive a [`PageBuilder`] they can fill
//! in place, then [`seal`](PageBuilder::seal) it into an immutable [`Page`]
//! that is `Send + Sync` and cheaply cloneable for multi-reader sharing.
//! When the last clone drops — on any thread — the buffer returns to the
//! owning pool via a lock-free MPSC return queue. The owner drains that
//! queue lazily on its next allocate.
//!
//! Allocations are attributed to a [`Tenant`], which exposes atomic
//! [`TenantStats`] counters that a metrics thread can scrape directly.
//! For a global view across multiple per-worker pools, the caller
//! aggregates per-worker [`Tenant`]s themselves — the library does not
//! maintain a global registry.
//!
//! # Example
//!
//! ```
//! use paged_alloc::{PagePool, Tenant};
//!
//! let mut pool = PagePool::new(4096);
//! let tenant = Tenant::new("tenant-a");
//!
//! let mut builder = pool.allocate(&tenant);
//! builder.append(b"hello ").unwrap();
//! builder.append(b"world").unwrap();
//! let page = builder.seal();
//!
//! assert_eq!(&page[..], b"hello world");
//! assert_eq!(tenant.stats().pages_in_use(), 1);
//! drop(page);
//! assert_eq!(tenant.stats().pages_in_use(), 0);
//! ```

mod page;
mod pool;
mod source;
mod tenant;

#[cfg(unix)]
mod mmap;

pub use page::{Page, PageBuilder, PageFull};
pub use pool::{PagePool, PoolShared, PoolStats};
pub use source::{HeapSource, PageSource};
pub use tenant::{Tenant, TenantId, TenantStats};

#[cfg(unix)]
pub use mmap::{MmapSource, madvise_dontneed};

#[cfg(target_os = "linux")]
pub use mmap::HugePageSource;
