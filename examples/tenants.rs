//! Multi-tenant accounting. One pool can attribute allocations
//! to any number of independent `Tenant`s, each with its own
//! atomic counters that any thread can read safely.
//!
//!     cargo run --release --example tenants

use paged_alloc::{ChunkPool, Tenant};

fn main() {
    let mut pool = ChunkPool::new();

    let alice = Tenant::new("alice");
    let bob = Tenant::new("bob");

    // Allocations attribute to whichever tenant you pass in.
    let _a1 = pool.alloc_from(&alice, &[0u8; 1024]);
    let _a2 = pool.alloc_from(&alice, &[0u8; 2048]);
    let _b1 = pool.alloc_from(&bob, &[0u8; 512]);

    print_tenant("alice", &alice);
    print_tenant("bob",   &bob);

    // Tenant counters are AtomicU64s — metrics scrapers on other
    // threads can read them without synchronization from the owner.
    let alice_clone = alice.clone();
    std::thread::spawn(move || {
        println!("\nmetrics thread sees alice.bytes_in_use = {}",
                 alice_clone.stats().bytes_in_use());
    })
    .join()
    .unwrap();
}

fn print_tenant(label: &str, t: &Tenant) {
    let s = t.stats();
    println!("{label}: chunks={} bytes={} peak={}",
             s.chunks_in_use(),
             s.bytes_in_use(),
             s.peak_bytes_in_use());
}
