use std::fmt;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::pool::PoolShared;
use crate::tenant::TenantStats;

/// Returned when an append would overflow a page's capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFull {
    pub capacity: usize,
    pub len: usize,
    pub attempted: usize,
}

impl fmt::Display for PageFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "page full: attempted to append {} bytes to page with {}/{} used",
            self.attempted, self.len, self.capacity
        )
    }
}

impl std::error::Error for PageFull {}

/// An exclusive, writable handle to a freshly allocated page.
///
/// Buffers taken from the pool's free list may contain arbitrary bytes
/// from previous use — callers must write before reading. Consuming the
/// builder via [`seal`](Self::seal) produces an immutable [`Page`].
/// Dropping an unsealed builder returns the buffer to the pool via the
/// same cross-thread-safe return path that sealed pages use.
#[must_use = "a PageBuilder represents a live allocation; call seal() \
              to publish it or drop it explicitly to release"]
pub struct PageBuilder {
    buf: Option<NonNull<u8>>,
    len: usize,
    shared: Arc<PoolShared>,
    tenant: Arc<TenantStats>,
}

// SAFETY: `NonNull<u8>` is a unique owning pointer to a pool-managed
// buffer; transferring it to another thread is sound so long as no
// other thread holds a reference to the same buffer. The type system
// prevents that via exclusive ownership of `PageBuilder`.
unsafe impl Send for PageBuilder {}

impl PageBuilder {
    pub(crate) fn new(
        buf: NonNull<u8>,
        shared: Arc<PoolShared>,
        tenant: Arc<TenantStats>,
    ) -> Self {
        PageBuilder {
            buf: Some(buf),
            len: 0,
            shared,
            tenant,
        }
    }

    pub fn capacity(&self) -> usize {
        self.shared.page_size()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn remaining(&self) -> usize {
        self.capacity() - self.len
    }

    pub fn as_slice(&self) -> &[u8] {
        let buf = self.buf.expect("live builder");
        // SAFETY: `buf` is a live, unique `page_size`-byte region.
        unsafe { std::slice::from_raw_parts(buf.as_ptr(), self.len) }
    }

    /// Full backing buffer as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let buf = self.buf.expect("live builder");
        let cap = self.capacity();
        // SAFETY: `buf` is a live, unique region of exactly `cap` bytes.
        unsafe { std::slice::from_raw_parts_mut(buf.as_ptr(), cap) }
    }

    /// Sets the logical content length.
    ///
    /// # Panics
    /// Panics if `len` exceeds [`capacity`](Self::capacity).
    pub fn set_len(&mut self, len: usize) {
        let cap = self.capacity();
        assert!(len <= cap, "len {} > capacity {}", len, cap);
        self.len = len;
    }

    /// Appends `data` to the end of the logical content.
    pub fn append(&mut self, data: &[u8]) -> Result<(), PageFull> {
        let cap = self.capacity();
        if self.len + data.len() > cap {
            return Err(PageFull {
                capacity: cap,
                len: self.len,
                attempted: data.len(),
            });
        }
        let buf = self.buf.expect("live builder");
        // SAFETY: `buf[self.len .. self.len + data.len()]` lies inside
        // the exclusive `cap`-byte region we own.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                buf.as_ptr().add(self.len),
                data.len(),
            );
        }
        self.len += data.len();
        Ok(())
    }

    /// Seals the builder into an immutable, shareable [`Page`].
    pub fn seal(mut self) -> Page {
        let buf = self.buf.take().expect("live builder");
        let inner = PageInner {
            buf: Some(buf),
            capacity: self.shared.page_size(),
            len: self.len,
            shared: self.shared.clone(),
            tenant: self.tenant.clone(),
        };
        Page {
            inner: Arc::new(inner),
        }
    }
}

impl Drop for PageBuilder {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            let capacity = self.shared.page_size() as u64;
            self.tenant.record_release(capacity);
            // SAFETY: `buf` was produced by this pool's source, has
            // never been released, and is now uniquely held by us.
            unsafe {
                self.shared.push_return(buf);
            }
        }
    }
}

impl fmt::Debug for PageBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageBuilder")
            .field("capacity", &self.capacity())
            .field("len", &self.len)
            .finish()
    }
}

/// An immutable, reference-counted handle to a sealed page.
///
/// Cloning is cheap (an `Arc` bump). Pages are `Send + Sync`, so they
/// can be shared freely with reader threads across cores. When the
/// last clone drops — on *any* thread — the buffer returns to the
/// originating pool via a lock-free MPSC return queue, and the
/// tenant's in-use counters are decremented atomically.
#[must_use = "a Page holds allocated memory; drop it explicitly to \
              release, or bind it to use its contents"]
pub struct Page {
    inner: Arc<PageInner>,
}

impl Page {
    pub fn len(&self) -> usize {
        self.inner.len
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    pub fn as_slice(&self) -> &[u8] {
        let buf = self.inner.buf.expect("live page");
        // SAFETY: `buf` is a live region of at least `capacity` bytes
        // and nothing mutates it while the `Arc<PageInner>` is alive.
        unsafe { std::slice::from_raw_parts(buf.as_ptr(), self.inner.len) }
    }

    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }
}

impl Clone for Page {
    fn clone(&self) -> Self {
        Page {
            inner: self.inner.clone(),
        }
    }
}

impl Deref for Page {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Page {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("ref_count", &self.ref_count())
            .finish()
    }
}

struct PageInner {
    buf: Option<NonNull<u8>>,
    capacity: usize,
    len: usize,
    shared: Arc<PoolShared>,
    tenant: Arc<TenantStats>,
}

// SAFETY: the inner `NonNull<u8>` is an exclusive owning pointer to a
// pool buffer. The buffer is only read from, never mutated, once the
// `Arc<PageInner>` exists. Concurrent readers observe the same
// immutable bytes, which is sound for `Sync`. Transferring the
// ref-counted handle across threads is sound for `Send`.
unsafe impl Send for PageInner {}
unsafe impl Sync for PageInner {}

impl Drop for PageInner {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            self.tenant.record_release(self.capacity as u64);
            // SAFETY: `buf` came from this pool's source, is still
            // live, and is now uniquely held by us on the way to the
            // return queue.
            unsafe {
                self.shared.push_return(buf);
            }
        }
    }
}
