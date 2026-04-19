//! The smallest possible paged-alloc program: allocate bytes,
//! read them back, observe the tenant counters, done.
//!
//!     cargo run --release --example hello

use paged_alloc::{ChunkPool, Tenant};

fn main() {
    // A pool for variable-size allocations, backed internally by
    // 16 KiB pages. `Tenant` attributes allocations for accounting.
    let mut pool = ChunkPool::new();
    let tenant = Tenant::new("greetings");

    // `allocate(tenant, size)` returns a ChunkBuilder of exactly
    // `size` bytes. Write into it however you like, then seal.
    let mut builder = pool.allocate(&tenant, 11);
    builder.append(b"hello world").unwrap();
    let chunk = builder.seal();

    // `Chunk` derefs to `&[u8]`.
    println!("chunk contents: {}", std::str::from_utf8(&chunk).unwrap());
    println!("chunks in use:  {}", tenant.stats().chunks_in_use());
    println!("bytes in use:   {}", tenant.stats().bytes_in_use());

    // When the last clone drops, the buffer returns to the pool
    // and tenant counters decrement.
    drop(chunk);
    println!();
    println!("after drop:");
    println!("chunks in use:  {}", tenant.stats().chunks_in_use());
    println!("bytes in use:   {}", tenant.stats().bytes_in_use());
}
