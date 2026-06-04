//! The GC correctness oracle for Locus's handle heap.
//!
//! The sibling collectors (NCL, OpenDylan) taught one lesson above all: a GC
//! can pass hundreds of tests and still be wrong, because the bug is in *root
//! tracking*, not the collector. So these tests assert the properties a moving,
//! handle-indirected collector must never violate:
//!
//!   1. Objects reachable from a live handle survive a collection, with their
//!      scalar payload bit-for-bit intact.
//!   2. Objects reachable only *through another object's pointer field* survive
//!      too — even when their own handle was freed. (Tracing, not handle-counting.)
//!   3. Objects reachable from no live handle are reclaimed (not copied).
//!   4. Handles stay valid across the move: the same `Handle` resolves to the
//!      relocated object.
//!   5. Allocating past capacity triggers a collection and keeps going — the
//!      "reliable emulation of infinite memory" — without corrupting the live set.

use locus_gc::Heap;

/// A heap big enough to exercise multiple pages but small enough that the stress
/// loop actually forces collections.
fn heap() -> Heap {
    Heap::new(8 * 64 * 1024)
}

#[test]
fn live_object_survives_with_payload_intact() {
    let mut h = heap();
    // A single object holding one scalar.
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 12345);

    let result = h.collect();
    assert_eq!(
        result.evac.objects_copied, 1,
        "the one live object was copied"
    );

    // Same handle, relocated object, identical payload.
    assert_eq!(h.get_scalar(a, 0), 12345, "scalar survived the move");
}

#[test]
fn reachable_through_pointer_field_survives_even_if_handle_freed() {
    let mut h = heap();
    // a: { ptr -> b, scalar 7 } ; b: { scalar 42 }
    let a = h.alloc(1, 1);
    let b = h.alloc(0, 1);
    h.set_scalar(a, 0, 7);
    h.set_scalar(b, 0, 42);
    h.set_ptr(a, 0, b);

    // Drop b's direct handle. b is now reachable ONLY through a's pointer field.
    h.free(b);

    let result = h.collect();
    // Both a and b must be copied: b is alive because a points at it.
    assert_eq!(result.evac.objects_copied, 2, "tracing kept b alive via a");

    // a intact...
    assert_eq!(h.get_scalar(a, 0), 7);
    // ...and following a's (rewritten) pointer reaches the relocated b.
    let b2 = h.get_ptr(a, 0);
    assert_eq!(
        h.get_scalar(b2, 0),
        42,
        "b survived and was reachable through a"
    );
}

#[test]
fn unreachable_object_is_reclaimed() {
    let mut h = heap();
    let keep = h.alloc(0, 1);
    h.set_scalar(keep, 0, 1);

    // Garbage: allocated, handle immediately freed, nothing points to it.
    let garbage = h.alloc(0, 1);
    h.set_scalar(garbage, 0, 999);
    h.free(garbage);

    let result = h.collect();
    assert_eq!(
        result.evac.objects_copied, 1,
        "only the kept object was copied; garbage reclaimed"
    );
    assert_eq!(h.get_scalar(keep, 0), 1, "kept object intact");
}

#[test]
fn diamond_dag_not_duplicated() {
    let mut h = heap();
    // leaf shared by left and right, both under root:
    //        root
    //       /    \
    //    left    right
    //       \    /
    //        leaf (scalar 7)
    let leaf = h.alloc(0, 1);
    h.set_scalar(leaf, 0, 7);
    let left = h.alloc(1, 0);
    let right = h.alloc(1, 0);
    let root = h.alloc(2, 0);
    h.set_ptr(left, 0, leaf);
    h.set_ptr(right, 0, leaf);
    h.set_ptr(root, 0, left);
    h.set_ptr(root, 1, right);

    // Only `root` stays handle-rooted.
    h.free(leaf);
    h.free(left);
    h.free(right);

    let result = h.collect();
    // 4 distinct objects, leaf copied ONCE despite two referents.
    assert_eq!(result.evac.objects_copied, 4, "diamond leaf not duplicated");

    // Both paths reach the same relocated leaf with the same payload.
    let l = h.get_ptr(root, 0);
    let r = h.get_ptr(root, 1);
    let leaf_l = h.get_ptr(l, 0);
    let leaf_r = h.get_ptr(r, 0);
    assert_eq!(h.get_scalar(leaf_l, 0), 7);
    assert_eq!(h.get_scalar(leaf_r, 0), 7);
}

#[test]
fn handles_stable_across_repeated_collections() {
    let mut h = heap();
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 555);
    // Collect many times; the handle must keep resolving and the value persist.
    for _ in 0..20 {
        h.collect();
        assert_eq!(h.get_scalar(a, 0), 555, "handle stable across collections");
    }
}

#[test]
fn linked_list_survives_intact() {
    let mut h = heap();
    // Build 0 -> 1 -> 2 -> ... -> 199, each node { ptr -> next, scalar i }.
    let n = 200;
    let mut head = h.alloc(0, 1); // tail sentinel with no next
    h.set_scalar(head, 0, (n - 1) as i64);
    for i in (0..n - 1).rev() {
        let node = h.alloc(1, 1);
        h.set_scalar(node, 0, i as i64);
        h.set_ptr(node, 0, head);
        h.free(head); // only the new head stays directly rooted
        head = node;
    }

    let result = h.collect();
    assert_eq!(result.evac.objects_copied, n, "every list node survived");

    // Walk the relocated list and check the sequence.
    let mut cur = head;
    for i in 0..n {
        assert_eq!(h.get_scalar(cur, 0), i as i64, "node {i} payload");
        if i + 1 < n {
            cur = h.get_ptr(cur, 0);
        }
    }
}

#[test]
fn infinite_memory_emulation_under_churn() {
    // The stress oracle: allocate MANY TIMES the heap's capacity while keeping a
    // small bounded working set live through handles. The collector must reclaim
    // the churn, keep committed memory flat, and never corrupt the set.
    //
    // The heap reserves 512 KiB (8 × 64 KiB pages). Each iteration allocates a
    // 3-cell garbage object (24 B), so ITERS=100_000 churns ~2.4 MiB through a
    // 0.5 MiB heap — it must be reclaimed and reused ~5× over. If the collector
    // leaked, the alloc-retry path would exhaust the reservation and panic; if it
    // grew, committed_pages would climb with ITERS. Neither may happen.
    let mut h = heap();

    const WORKING: usize = 64;
    const ITERS: usize = 100_000;

    // Bounded working set: a ring of live objects, each tagged with its identity.
    let mut ring: Vec<locus_gc::Handle> = Vec::with_capacity(WORKING);
    for i in 0..WORKING {
        let o = h.alloc(0, 1);
        h.set_scalar(o, 0, i as i64);
        ring.push(o);
    }
    let baseline_pages = h.committed_pages();

    let mut peak_pages = 0;
    let mut cells_allocated: u64 = 0;
    for step in 0..ITERS {
        // Allocate a short-lived garbage object every iteration (1 hdr + 2 scalars).
        let g = h.alloc(0, 2);
        cells_allocated += 3;
        h.set_scalar(g, 0, step as i64);
        h.set_scalar(g, 1, -(step as i64));
        h.free(g);

        // Periodically replace one working-set slot (old object becomes garbage).
        if step % 8 == 0 {
            let slot = step % WORKING;
            h.free(ring[slot]);
            let fresh = h.alloc(0, 1);
            cells_allocated += 2;
            // Re-tag with the canonical identity so the invariant below holds.
            h.set_scalar(fresh, 0, slot as i64);
            ring[slot] = fresh;
        }

        if step % 256 == 0 {
            h.collect();
            // Invariant: every working-set object still holds its identity.
            for (i, &o) in ring.iter().enumerate() {
                assert_eq!(
                    h.get_scalar(o, 0),
                    i as i64,
                    "working-set corruption at step {step}"
                );
            }
            peak_pages = peak_pages.max(h.committed_pages());
        }
    }

    // We churned several heaps' worth of allocation...
    let heap_cells = 8 * (64 * 1024 / 8); // 8 pages × 8192 cells
    assert!(
        cells_allocated > 4 * heap_cells as u64,
        "stress too weak: only {cells_allocated} cells through a {heap_cells}-cell heap"
    );
    // ...yet committed memory never grew beyond a small constant over baseline.
    // (Reclamation is real: live set is 64 objects, so this is independent of ITERS.)
    assert!(
        peak_pages <= baseline_pages + 2,
        "memory not bounded under churn: peaked at {peak_pages} pages vs baseline {baseline_pages}"
    );

    // Final check: the whole working set is intact after all the churn.
    for (i, &o) in ring.iter().enumerate() {
        assert_eq!(h.get_scalar(o, 0), i as i64);
    }
}
