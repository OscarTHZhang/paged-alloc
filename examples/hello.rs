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

    // The one-call form: copy a slice into a fresh chunk.
    let chunk = pool.alloc_from(&tenant, b"hello world");

    // `Chunk` is `Send + Sync + Clone` and derefs to `&[u8]`.
    println!("chunk contents: {}", std::str::from_utf8(&chunk).unwrap());
    println!("chunks in use:  {}", tenant.stats().chunks_in_use());
    println!("bytes in use:   {}", tenant.stats().bytes_in_use());

    // You can also build a chunk by filling a mutable buffer
    // in a closure. Useful when the content is computed, not copied:
    let computed = pool.alloc_with(&tenant, 16, |buf| {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i * 7) as u8;
        }
    });
    println!("computed[0..4]: {:?}", &computed[..4]);

    // Or the full builder for multi-step writes (error handling,
    // conditional bailout, etc.):
    let mut b = pool.allocate(&tenant, 32);
    b.append(b"[").unwrap();
    b.append(b"header").unwrap();
    b.append(b"]").unwrap();
    let framed = b.seal();
    println!("framed:         {}", std::str::from_utf8(&framed).unwrap());

    // Nothing requires an explicit drop — when `chunk`, `computed`,
    // and `framed` go out of scope at the end of `main`, Rust's
    // RAII returns their buffers to the pool and decrements the
    // tenant counters automatically.
}
