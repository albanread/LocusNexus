//! Handle flow across function boundaries — the two directions a call moves a
//! handle, and the property that makes both sound under a moving collector.
//!
//!   * IN  (argument): a handle is just a table index. The caller's frame stays
//!     active *below* the callee's, so the argument slot is still a live root —
//!     rooted for free, no copy. If a GC fires inside the callee the slot is
//!     rewritten and the callee re-reads the stable index to find the moved
//!     object.
//!   * OUT (return): `leave_with` copies a computed result down into the caller's
//!     scope (escape), or — if the function returns a handle it was *passed* —
//!     hands the original back unchanged (pass-through), since it's already
//!     rooted in the caller.

use locus_gc::{Handle, Heap};

// --- "Functions" that manage their own handle scope -------------------------

/// Allocate and return a 2-scalar pair. The result is computed *in* this scope,
/// so it must escape into the caller's.
fn make_pair(h: &mut Heap, a: i64, b: i64) -> Handle {
    let frame = h.enter();
    let pair = h.alloc(0, 2);
    h.set_scalar(pair, 0, a);
    h.set_scalar(pair, 1, b);
    h.leave_with(frame, pair) // escape
}

/// Read a passed pair and return a scalar — no handle escapes.
fn pair_sum(h: &mut Heap, pair: Handle) -> i64 {
    let frame = h.enter();
    let s = h.get_scalar(pair, 0) + h.get_scalar(pair, 1);
    h.leave(frame);
    s
}

/// Return the argument unchanged: a pass-through escape.
fn id(h: &mut Heap, x: Handle) -> Handle {
    let frame = h.enter();
    h.leave_with(frame, x) // x lives below `frame` → returned as-is
}

/// Churn a pile of short-lived garbage (each in its own popped scope, so it's
/// unrooted at collection), collecting proactively the way a real runtime does,
/// then read the passed `witness`. The collections relocate `witness` while it's
/// only rooted by the *caller's* frame.
fn churn_then_read(h: &mut Heap, witness: Handle, iters: usize) -> i64 {
    let frame = h.enter();
    for i in 0..iters {
        let g = h.enter();
        let _garbage = h.alloc(0, 4);
        h.leave(g); // popped → unreachable at the next collection
                    // Proactive collection (threshold-driven in a real runtime), not waiting
                    // for total exhaustion — that keeps a reserve for the evacuator.
        if i % 200 == 0 {
            h.collect();
        }
    }
    let v = h.get_scalar(witness, 0);
    h.leave(frame);
    v
}

// --- Tests ------------------------------------------------------------------

#[test]
fn return_escapes_a_fresh_handle_into_caller_scope() {
    let mut h = Heap::new(8 * 64 * 1024);
    let base = h.handle_stack_depth();
    let outer = h.enter();

    let pair = make_pair(&mut h, 11, 22);
    // Exactly one handle came back into the caller's scope.
    assert_eq!(
        h.handle_stack_depth(),
        base + 1,
        "one result handle escaped"
    );
    assert_eq!(h.get_scalar(pair, 0), 11);
    assert_eq!(h.get_scalar(pair, 1), 22);

    // It's a live root in the caller, so it survives a collection.
    h.collect();
    assert_eq!(pair_sum(&mut h, pair), 33, "escaped pair intact after GC");

    h.leave(outer);
    assert_eq!(h.handle_stack_depth(), base, "caller scope fully unwound");
}

#[test]
fn return_passes_through_an_argument_without_allocating() {
    let mut h = Heap::new(8 * 64 * 1024);
    let outer = h.enter();
    let x = h.alloc(0, 1);
    h.set_scalar(x, 0, 7);
    let depth = h.handle_stack_depth();

    let y = id(&mut h, x);
    assert_eq!(
        y.index(),
        x.index(),
        "pass-through returns the very same handle"
    );
    assert_eq!(
        h.handle_stack_depth(),
        depth,
        "no handle allocated for a pass-through"
    );
    assert_eq!(h.get_scalar(y, 0), 7);

    h.leave(outer);
}

#[test]
fn passed_handle_survives_gc_inside_callee() {
    let mut h = Heap::new(8 * 64 * 1024);
    let outer = h.enter();

    let witness = h.alloc(0, 1);
    h.set_scalar(witness, 0, 0xBEEF);

    // Hand `witness` to a function that allocates+collects enough to relocate it.
    let got = churn_then_read(&mut h, witness, 20_000);
    assert_eq!(
        got, 0xBEEF,
        "passed handle survived relocation inside the callee"
    );

    // Collections really did happen (the object moved; the index didn't).
    assert!(h.stats().collections > 0, "test didn't actually collect");

    // And `witness` is still usable in the caller afterward.
    assert_eq!(h.get_scalar(witness, 0), 0xBEEF);
    h.leave(outer);
}

#[test]
fn deep_call_chain_keeps_all_arguments_rooted() {
    // A recursive descent that allocates at every level, holding a live pair
    // across each recursive call. Every frame's pair must survive the GCs that
    // deeper frames trigger, and the sums must be exact on the way back up.
    fn descend(h: &mut Heap, depth: i64) -> i64 {
        if depth == 0 {
            return 0;
        }
        let frame = h.enter();
        let pair = make_pair(h, depth, depth * 2); // local, live across the call
                                                   // Recurse: deeper levels allocate and may collect, relocating `pair`.
        let below = descend(h, depth - 1);
        // Read our pair AFTER the recursive call (and its GCs).
        let here = pair_sum(h, pair); // depth + 2*depth = 3*depth
        h.leave(frame);
        here + below
    }

    let mut h = Heap::new(6 * 64 * 1024);
    let n = 400;
    let got = descend(&mut h, n);
    // Sum of 3*d for d in 1..=n.
    let expected: i64 = (1..=n).map(|d| 3 * d).sum();
    assert_eq!(
        got, expected,
        "a frame's pair was corrupted by a deeper frame's GC"
    );
}
