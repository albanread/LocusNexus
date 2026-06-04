//! Locus's own GC stats — the handle/root view. These assert the counters tell
//! the truth about a scoped chain walk, and that the headline ratios capture the
//! "heap allocation behaving like stack allocation" story.

use locus_gc::Heap;

const NEXT: u32 = 0;
const VAL: u32 = 0;

#[test]
fn stats_capture_a_scoped_chain_walk() {
    let mut h = Heap::new(8 * 64 * 1024);
    let n = 600i64;

    // Build a list holding only `head` live (reused slot), then walk it with one
    // reused cursor. Lots of objects; almost no live handles.
    let head = h.alloc(1, 1);
    h.set_scalar(head, VAL, n - 1);
    h.set_ptr(head, NEXT, head);
    for i in (0..n - 1).rev() {
        let body = h.enter();
        let node = h.alloc(1, 1);
        h.set_scalar(node, VAL, i);
        h.set_ptr(node, NEXT, head);
        h.set(head, node); // reuse the head slot
        h.leave(body);
    }

    let walk = h.enter();
    let cursor = h.alloc(1, 1);
    h.set(cursor, head);
    let mut sum = 0i64;
    for i in 0..n {
        sum += h.get_scalar(cursor, VAL);
        if i + 1 < n {
            h.step_ptr(cursor, cursor, NEXT); // reuse the cursor slot
        }
    }
    h.leave(walk);
    assert_eq!(sum, (0..n).sum::<i64>());

    let s = h.stats();
    eprintln!("{s}");
    eprintln!("render: {}", s.render());

    // We allocated at least every node + head + cursor.
    assert!(
        s.objects_allocated >= n as u64,
        "objects_allocated = {}",
        s.objects_allocated
    );
    // The pointer/scalar split is tracked: each node is 1 ptr + 1 scalar cell.
    assert_eq!(
        s.pointer_cells_allocated, s.objects_allocated,
        "1 ptr cell per node"
    );
    assert_eq!(
        s.scalar_cells_allocated, s.objects_allocated,
        "1 scalar cell per node"
    );
    assert_eq!(
        s.cells_allocated,
        3 * s.objects_allocated,
        "header + ptr + scalar each"
    );

    // Slot reuse dominates: set() during build + step_ptr() during walk, ~2n.
    assert!(
        s.slot_reuses >= (2 * n as u64) - 4,
        "slot_reuses = {}",
        s.slot_reuses
    );
    let reuse = s.slot_reuse_ratio().unwrap();
    assert!(reuse > 0.5, "expected mostly slot reuse, got {reuse:.3}");

    // The whole thing ran at a tiny live-root high-water mark.
    assert!(
        s.handle_stack_peak < 16,
        "peak handle depth = {}",
        s.handle_stack_peak
    );

    // Scopes balanced (every enter was left).
    assert_eq!(s.open_frames(), 0, "all scopes closed");
    assert!(
        s.frames_entered >= n as u64 - 1,
        "a body scope per node built"
    );
}

#[test]
fn stats_show_low_survival_under_churn() {
    // Allocate lots of short-lived garbage (popped before each collection) plus a
    // tiny retained set. Survival rate should be low: most objects die young.
    let mut h = Heap::new(8 * 64 * 1024);

    let keep = h.alloc(0, 1);
    h.set_scalar(keep, 0, 1);

    for _ in 0..5_000 {
        let g = h.enter();
        let _garbage = h.alloc(0, 3);
        h.leave(g);
    }
    h.collect();
    h.collect();

    let s = h.stats();
    eprintln!("{s}");
    assert!(s.collections >= 2);
    // Far more allocated than survived: generational churn is healthy.
    let survival = s.survival_rate().unwrap();
    assert!(
        survival < 0.2,
        "survival rate should be low under churn, got {survival:.3}"
    );
    assert_eq!(h.get_scalar(keep, 0), 1, "retained object intact");
}

#[test]
fn reset_stats_zeroes_the_counters() {
    let mut h = Heap::new(8 * 64 * 1024);
    let _ = h.alloc(0, 1);
    assert!(h.stats().objects_allocated > 0);
    h.reset_stats();
    assert_eq!(h.stats(), Default::default(), "counters cleared");
    // The heap itself is unaffected: we can still allocate.
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 9);
    assert_eq!(h.get_scalar(a, 0), 9);
    assert_eq!(h.stats().objects_allocated, 1, "counting resumed from zero");
}
