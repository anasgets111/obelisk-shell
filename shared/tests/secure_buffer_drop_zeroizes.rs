//! Proves `SecureBuffer`'s `Drop` backup (ADR-0005) actually zeroizes, without reading memory after
//! it's freed (which would be undefined behavior). Reading a Vec's own spare capacity while it's
//! still owned (see the `explicit_zeroize` test in shared/src/secure_buffer.rs) can't reach this
//! path: `Drop` runs *after* that memory is handed back to the allocator, so the only well-defined
//! place left to observe it is the allocator's `dealloc` call itself, at the exact moment it
//! receives the pointer back.
//!
//! Same technique `zeroize`'s own test suite uses for this (`zeroize-1.9.0/tests/alloc.rs`):
//! a `#[global_allocator]` that inspects bytes right as they're deallocated. Unlike that
//! test, this one keys off the exact pointer being watched for, not merely an allocation
//! size -- the test binary's own harness (argument parsing, etc.) allocates plenty of
//! same-sized buffers of its own, and a size-only filter flags those as false positives.
//!
//! This has to live in its own integration test binary (`tests/*.rs` files each get a
//! separate process) since a global allocator applies to the whole binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use shared::SecureBuffer;

const SECRET: &str = "correct horse battery staple secret text";

/// Address of the one allocation this test cares about, set right before `drop(buf)`.
/// Zero means "not currently watching anything".
static WATCHED_PTR: AtomicUsize = AtomicUsize::new(0);

/// Index of the first non-zero byte seen in the watched allocation, or [`CLEAN`]. `dealloc`
/// records rather than asserts, like `secure_buffer_growth_zeroizes`'s `LEAK_FOUND`: a panic
/// there unwinds out of `GlobalAlloc` from inside drop glue, skipping the handback below and
/// leaking the block instead of failing cleanly.
static FIRST_DIRTY_BYTE: AtomicUsize = AtomicUsize::new(CLEAN);
const CLEAN: usize = usize::MAX;

struct ZeroCheckingAllocator;

// SAFETY: `alloc`/`dealloc` delegate every allocation to `System`, adding only a read of memory
// that is still live. The `GlobalAlloc` contract -- returning correctly aligned blocks for the
// requested layout, and freeing only what it handed out -- is `System`'s, unchanged.
unsafe impl GlobalAlloc for ZeroCheckingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is whatever the caller asked for and is forwarded untouched, which
        // is exactly what `System`'s own `alloc` requires.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // `compare_exchange`, not `swap`: the harness allocates on its own thread, and a `swap`
        // let any unrelated `dealloc` between the `store` and this one consume the watch. The
        // test's "did we observe anything" guard then passed on that same swap, so a run that
        // checked nothing reported success.
        if !ptr.is_null() && WATCHED_PTR.compare_exchange(ptr as usize, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            for i in 0..layout.size() {
                // Safety: `ptr` is valid for `layout.size()` bytes until this call
                // returns it to the allocator -- this read happens before that handback
                // completes.
                if unsafe { core::ptr::read(ptr.add(i)) } != 0 {
                    FIRST_DIRTY_BYTE.store(i, Ordering::SeqCst);
                    break;
                }
            }
        }
        // SAFETY: `ptr`/`layout` are the pair the caller received from `alloc` above and are
        // forwarded unchanged; the reads before this point do not alter either.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: ZeroCheckingAllocator = ZeroCheckingAllocator;

#[test]
fn dropping_a_secure_buffer_zeroizes_before_deallocation() {
    let mut buf = SecureBuffer::new();
    buf.push_str(SECRET);
    assert_eq!(buf.len(), SECRET.len());

    let watched = buf.expose_secret().as_ptr() as usize;
    WATCHED_PTR.store(watched, Ordering::SeqCst);
    drop(buf);

    // If this is still `watched`, `dealloc` never ran on the pointer we asked it to check
    // -- the test would otherwise pass by accident (nothing to zeroize is trivially "zeroed").
    assert_eq!(
        WATCHED_PTR.load(Ordering::SeqCst),
        0,
        "SecureBuffer's backing allocation was never deallocated; this test observed nothing"
    );
    let dirty = FIRST_DIRTY_BYTE.load(Ordering::SeqCst);
    assert_eq!(dirty, CLEAN, "byte {dirty} of a dropped SecureBuffer's allocation was not zeroed");
}
