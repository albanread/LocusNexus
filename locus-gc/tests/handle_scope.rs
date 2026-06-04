//! The handle-allocation answer: handles follow lexical scope, so the handle
//! table runs as a STACK with O(1) frame push/pop and bulk free.
//!
//! Because design Z keeps raw (traced) pointers inside heap objects and never
//! stores a handle in an object, a handle only ever names a live local. So a
//! program that builds and walks a 1000-node list needs just a *constant* number
//! of live handles at any instant — even though it touches 1000 objects and
//! triggers many collections. These tests pin that property down, and confirm
//! the scoped handles are precise GC roots (the partially-built list survives a
//! collection that fires mid-construction).

use locus_gc::{Handle, Heap};

const NEXT: u32 = 0; // pointer field 0 = the `next` link
const VAL: u32 = 0; // scalar field 0 = the node's value

/// Build `0 -> 1 -> ... -> n-1` by prepending, holding only a single `head`
/// handle live at a time. Returns the head and the peak handle-stack depth seen.
fn build_list(h: &mut Heap, n: i64) -> (Handle, usize) {
    let mut peak = 0;
    // `head` lives in the outer scope for the whole build.
    let head = h.alloc(1, 1);
    h.set_scalar(head, VAL, n - 1);
    // tail node's `next` points at itself as a benign terminator we never follow.
    h.set_ptr(head, NEXT, head);

    for i in (0..n - 1).rev() {
        let body = h.enter();
        let node = h.alloc(1, 1); // a collection may fire HERE
        h.set_scalar(node, VAL, i);
        h.set_ptr(node, NEXT, head);
        h.set(head, node); // head := node  (reuse the head slot, no new handle)
        peak = peak.max(h.handle_stack_depth());
        h.leave(body); // pop `node`; head (outer) survives
    }
    (head, peak)
}

#[test]
fn build_and_walk_costs_constant_handles() {
    // Small heap so construction forces real collections mid-build.
    let mut h = Heap::new(8 * 64 * 1024);
    let n = 1000;

    let depth_before = h.handle_stack_depth();
    let (head, peak) = build_list(&mut h, n);

    // The whole 1000-node list was built with a constant handle high-water mark.
    assert!(
        peak <= depth_before + 2,
        "handle use not constant: peaked at {peak} (baseline {depth_before}) building {n} nodes"
    );

    // Walk it with ONE reused cursor handle, summing values; verify the sequence.
    let walk = h.enter();
    let cursor = h.alloc(1, 1); // borrow a slot to use as the cursor
    h.set(cursor, head);
    let mut seen = 0i64;
    let mut expected_sum = 0i64;
    for i in 0..n {
        let v = h.get_scalar(cursor, VAL);
        assert_eq!(v, i, "node {i} has wrong value {v}");
        seen += 1;
        expected_sum += v;
        if i + 1 < n {
            h.step_ptr(cursor, cursor, NEXT); // cursor = cursor.next, no alloc
        }
    }
    let walk_peak = h.handle_stack_depth();
    h.leave(walk);

    assert_eq!(seen, n);
    assert_eq!(expected_sum, (0..n).sum::<i64>());
    // Walking 1000 nodes used a constant number of handles, too.
    assert!(
        walk_peak <= depth_before + 3,
        "walk handle use not constant: {walk_peak}"
    );
}

#[test]
fn scoped_handles_are_precise_roots_under_midbuild_gc() {
    // Force collections during the build and confirm NOTHING is lost: the
    // finished list is fully intact, which can only happen if `head` (a scoped
    // handle) correctly rooted the partially-built list at every collection.
    let mut h = Heap::new(6 * 64 * 1024); // tight: many GCs while building
    let n = 800;
    let (head, _) = build_list(&mut h, n);

    // Explicitly collect once more for good measure, then walk.
    h.collect();

    let cursor = h.alloc(1, 1);
    h.set(cursor, head);
    for i in 0..n {
        assert_eq!(h.get_scalar(cursor, VAL), i, "list corrupted at node {i}");
        if i + 1 < n {
            h.step_ptr(cursor, cursor, NEXT);
        }
    }
}

#[test]
fn leave_with_escapes_one_handle() {
    let mut h = Heap::new(8 * 64 * 1024);
    let base = h.handle_stack_depth();

    // A "callee" that allocates several temporaries but returns one result.
    let outer = h.enter();
    let result = {
        let inner = h.enter();
        let _t1 = h.alloc(0, 1);
        let _t2 = h.alloc(0, 1);
        let r = h.alloc(0, 1);
        h.set_scalar(r, 0, 4242);
        // Escape `r` into the outer scope; t1/t2 are popped.
        h.leave_with(inner, r)
    };

    // Only the escaped handle remains above `outer`'s entry.
    assert_eq!(
        h.handle_stack_depth(),
        base + 1,
        "exactly one handle escaped"
    );
    assert_eq!(h.get_scalar(result, 0), 4242, "escaped handle still valid");

    // It survives a collection (it's a live root) ...
    h.collect();
    assert_eq!(h.get_scalar(result, 0), 4242);

    h.leave(outer);
    assert_eq!(h.handle_stack_depth(), base, "outer scope fully unwound");
}
