//! Oracle for the conservative-scan root model — the GC-oblivious path codegen
//! actually uses. Instead of the program freeing handles, a snapshot of the
//! machine stack decides liveness: a handle is live iff some stack word equals
//! its index. These tests stand in a `Vec<u64>` for that stack snapshot.
//!
//! The properties that must hold (this is where the sibling collectors bled —
//! "missing stack roots"):
//!   1. A handle present on the stack keeps its object alive across the move.
//!   2. A handle absent from the stack is retired, and its object reclaimed —
//!      UNLESS still reachable through a live object's pointer field, in which
//!      case the object survives by tracing and only the dead handle is recycled.
//!   3. A false positive (a scalar that collides with a handle index) merely
//!      retains; it never corrupts.

use locus_gc::{Handle, Heap};

fn heap() -> Heap {
    Heap::new(8 * 64 * 1024)
}

/// Helper: a "stack" holding the given handles' full encoded bits as raw words
/// (what a register/stack slot actually contains).
fn stack(handles: &[Handle]) -> Vec<u64> {
    handles.iter().map(|h| h.to_bits() as u64).collect()
}

#[test]
fn handle_on_stack_survives() {
    let mut h = heap();
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 77);

    // `a` is "in a register / stack slot": present in the scan.
    let result = h.collect_conservative(&stack(&[a]));
    assert_eq!(result.evac.objects_copied, 1);
    assert_eq!(
        h.get_scalar(a, 0),
        77,
        "stacked handle's object survived intact"
    );
}

#[test]
fn handle_off_stack_is_reclaimed() {
    let mut h = heap();
    let keep = h.alloc(0, 1);
    h.set_scalar(keep, 0, 1);

    // Allocate an object but DON'T put its handle on the stack — it's dead.
    let dead = h.alloc(0, 1);
    h.set_scalar(dead, 0, 2);
    let _ = dead; // never referenced from the scanned stack

    let result = h.collect_conservative(&stack(&[keep]));
    assert_eq!(
        result.evac.objects_copied, 1,
        "only the stacked object survived"
    );
    assert_eq!(h.get_scalar(keep, 0), 1);
}

#[test]
fn object_reachable_through_field_survives_without_its_handle_on_stack() {
    let mut h = heap();
    // a { ptr -> b, scalar 7 }, b { scalar 42 }. Only `a` is on the stack.
    let a = h.alloc(1, 1);
    let b = h.alloc(0, 1);
    h.set_scalar(a, 0, 7);
    h.set_scalar(b, 0, 42);
    h.set_ptr(a, 0, b);

    // b's handle is NOT on the stack — but a points at b, so b must survive.
    let result = h.collect_conservative(&stack(&[a]));
    assert_eq!(
        result.evac.objects_copied, 2,
        "tracing kept b alive through a"
    );

    assert_eq!(h.get_scalar(a, 0), 7);
    // Reach b via a's rewritten pointer field; its payload is intact.
    let b2 = h.get_ptr(a, 0);
    assert_eq!(h.get_scalar(b2, 0), 42);
}

#[test]
fn retired_handle_slots_are_recycled() {
    let mut h = heap();
    let keep = h.alloc(0, 1);
    h.set_scalar(keep, 0, 100);

    // Make a pile of dead handles (not on the stack).
    for i in 0..10 {
        let g = h.alloc(0, 1);
        h.set_scalar(g, 0, i);
    }
    let before = h.live_handles();
    assert_eq!(before, 11, "keep + 10 garbage all currently live handles");

    h.collect_conservative(&stack(&[keep]));

    // The 10 dead handles were retired; only `keep` remains.
    assert_eq!(h.live_handles(), 1, "dead handle slots recycled");
    assert_eq!(h.get_scalar(keep, 0), 100);

    // Recycled slots are reused by subsequent allocation (table doesn't grow).
    let r = h.alloc(0, 1);
    h.set_scalar(r, 0, 7);
    assert!(
        r.index() < 11,
        "fresh handle reused a retired slot, index {}",
        r.index()
    );
}

#[test]
fn magic_makes_the_scan_precise() {
    // The 16-bit magic upgrades the conservative scan from merely *sound* to
    // *precise*: a word must carry the handle pattern to be a root. Bare ints —
    // even one numerically equal to a live index — are no longer false positives.
    let mut h = heap();
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 999);

    // A "stack" of plain integers, including a's bare index and some big values.
    // None carry the magic, so none is a root → `a` is correctly reclaimed.
    let result = h.collect_conservative(&[a.index() as u64, 123456, u64::MAX, 0]);
    assert_eq!(
        result.evac.objects_copied, 0,
        "non-handle words are not roots"
    );

    // By contrast, a real handle's bits ARE recognized and keep its object.
    let b = h.alloc(0, 1);
    h.set_scalar(b, 0, 42);
    let r2 = h.collect_conservative(&[b.to_bits() as u64]);
    assert_eq!(r2.evac.objects_copied, 1, "a well-formed handle is a root");
    assert_eq!(h.get_scalar(b, 0), 42, "and its payload is intact");
}

#[test]
fn churn_with_moving_stack_window_stays_correct() {
    // Simulate a program whose live set (the "stack window") slides over time:
    // each step a new object enters the window and the oldest leaves. Collect
    // conservatively from the window. Survivors must keep their identity; the
    // departed must be reclaimed; memory must stay bounded.
    let mut h = heap();
    const WINDOW: usize = 32;
    const STEPS: usize = 5_000;

    let mut window: std::collections::VecDeque<Handle> = std::collections::VecDeque::new();
    for step in 0..STEPS {
        let o = h.alloc(0, 1);
        h.set_scalar(o, 0, step as i64);
        window.push_back(o);
        if window.len() > WINDOW {
            window.pop_front(); // oldest leaves the live set (no longer on "stack")
        }

        if step % 64 == 0 {
            let snap: Vec<u64> = window.iter().map(|hh| hh.to_bits() as u64).collect();
            h.collect_conservative(&snap);
            // Every object still in the window holds the step it was created at.
            for (k, &hh) in window.iter().enumerate() {
                let created = step.saturating_sub(window.len() - 1 - k);
                assert_eq!(
                    h.get_scalar(hh, 0),
                    created as i64,
                    "window object corrupted at step {step}"
                );
            }
        }
    }
    // Bounded: live set is WINDOW objects regardless of STEPS.
    assert!(
        h.committed_pages() <= 6,
        "memory unbounded: {} pages",
        h.committed_pages()
    );
}
