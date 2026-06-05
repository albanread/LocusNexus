//! Trigger-policy regressions for the Locus handle heap.
//!
//! The runtime must not wait until the page heap is totally out of free pages
//! before collecting. If it does, a mostly-dead heap with a few sparse live
//! objects can stall mid-evacuation because the collector has no destination
//! page left to copy those live objects into.

use locus_gc::{Handle, Heap};

#[test]
fn sparse_live_pages_collect_before_evacuation_reserve_is_gone() {
    let mut h = Heap::new(2 * 1024 * 1024);
    let outer = h.enter();
    let mut roots: Vec<Handle> = Vec::new();

    for i in 0..120 {
        let live = h.alloc(0, 1);
        h.set_scalar(live, 0, i);
        roots.push(live);

        let garbage_frame = h.enter();
        for _ in 0..90 {
            let _ = h.alloc(0, 32);
        }
        h.leave(garbage_frame);
    }

    for (i, root) in roots.iter().copied().enumerate() {
        assert_eq!(h.get_scalar(root, 0), i as i64);
    }
    assert!(
        h.stats().collections > 0,
        "allocation should have triggered collection before total heap exhaustion"
    );
    h.leave(outer);
}
