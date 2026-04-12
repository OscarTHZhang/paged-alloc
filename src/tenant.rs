use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Opaque tenant identifier. Cheap to clone.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TenantId(Arc<str>);

impl TenantId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TenantId {
    fn from(s: &str) -> Self {
        TenantId(Arc::from(s))
    }
}

impl From<String> for TenantId {
    fn from(s: String) -> Self {
        TenantId(Arc::from(s.into_boxed_str()))
    }
}

impl fmt::Debug for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TenantId({:?})", &*self.0)
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Atomic counters describing a tenant's current and historical page usage.
///
/// All counters use [`Ordering::Relaxed`]: they are advisory metrics, not
/// synchronization primitives. A concurrent reader may observe a snapshot
/// where `bytes_in_use` briefly lags `pages_in_use * page_size` during an
/// allocation or drop.
#[derive(Default)]
pub struct TenantStats {
    bytes_in_use: AtomicU64,
    pages_in_use: AtomicU64,
    total_pages_allocated: AtomicU64,
    peak_bytes_in_use: AtomicU64,
}

impl TenantStats {
    pub fn bytes_in_use(&self) -> u64 {
        self.bytes_in_use.load(Ordering::Relaxed)
    }

    pub fn pages_in_use(&self) -> u64 {
        self.pages_in_use.load(Ordering::Relaxed)
    }

    pub fn total_pages_allocated(&self) -> u64 {
        self.total_pages_allocated.load(Ordering::Relaxed)
    }

    pub fn peak_bytes_in_use(&self) -> u64 {
        self.peak_bytes_in_use.load(Ordering::Relaxed)
    }

    pub(crate) fn record_allocate(&self, page_bytes: u64) {
        self.total_pages_allocated.fetch_add(1, Ordering::Relaxed);
        self.pages_in_use.fetch_add(1, Ordering::Relaxed);
        let new_bytes = self
            .bytes_in_use
            .fetch_add(page_bytes, Ordering::Relaxed)
            + page_bytes;
        // `record_allocate` is only called from the pool-owning worker
        // thread (allocation path is single-writer), so a plain
        // load-then-store is sufficient for peak tracking. The old
        // CAS loop was necessary only when multiple threads could race
        // on peak; single-writer lets us save the retry overhead.
        if new_bytes > self.peak_bytes_in_use.load(Ordering::Relaxed) {
            self.peak_bytes_in_use.store(new_bytes, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_release(&self, page_bytes: u64) {
        self.bytes_in_use.fetch_sub(page_bytes, Ordering::Relaxed);
        self.pages_in_use.fetch_sub(1, Ordering::Relaxed);
    }
}

impl fmt::Debug for TenantStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantStats")
            .field("bytes_in_use", &self.bytes_in_use())
            .field("pages_in_use", &self.pages_in_use())
            .field("total_pages_allocated", &self.total_pages_allocated())
            .field("peak_bytes_in_use", &self.peak_bytes_in_use())
            .finish()
    }
}

/// A cheap, cloneable handle identifying the owner of an allocation.
///
/// Cloning a `Tenant` shares the underlying [`TenantStats`], so accounting
/// is unified regardless of how many clones exist or which pools they
/// allocate from.
#[derive(Clone)]
pub struct Tenant {
    id: TenantId,
    stats: Arc<TenantStats>,
}

impl Tenant {
    pub fn new(id: impl Into<TenantId>) -> Self {
        Tenant {
            id: id.into(),
            stats: Arc::new(TenantStats::default()),
        }
    }

    pub fn id(&self) -> &TenantId {
        &self.id
    }

    pub fn stats(&self) -> &TenantStats {
        &self.stats
    }

    pub(crate) fn stats_arc(&self) -> Arc<TenantStats> {
        self.stats.clone()
    }
}

impl fmt::Debug for Tenant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tenant")
            .field("id", &self.id)
            .field("stats", &*self.stats)
            .finish()
    }
}
