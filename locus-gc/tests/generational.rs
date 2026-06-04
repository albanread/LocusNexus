//! Generational, validated handles — the slotmap property that turns a
//! use-after-free into a clean, immediate error instead of silent corruption,
//! and the magic that tells a handle from an ordinary value.

use locus_gc::{Handle, Heap};

fn heap() -> Heap {
    Heap::new(8 * 64 * 1024)
}

#[test]
fn reused_slot_bumps_generation_and_invalidates_the_old_handle() {
    let mut h = heap();
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 111);
    let stale = Handle::from_bits(a.to_bits()); // keep a copy of a's bits

    // Free a, then allocate b — which recycles a's slot with a new generation.
    h.free(a);
    let b = h.alloc(0, 1);
    h.set_scalar(b, 0, 222);

    assert_eq!(a.index(), b.index(), "b reused a's slot");
    assert_ne!(
        a.generation(),
        b.generation(),
        "but with a fresh generation"
    );

    // b is the live occupant; the leftover handle to the old occupant is dead.
    assert_eq!(h.get_scalar(b, 0), 222);
    assert!(h.is_live_handle(b));
    assert!(!h.is_live_handle(stale), "stale handle no longer validates");
}

#[test]
#[should_panic(expected = "stale handle")]
fn using_a_stale_handle_panics_instead_of_corrupting() {
    let mut h = heap();
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 1);
    h.free(a);
    let _b = h.alloc(0, 1); // recycles a's slot, bumps the generation

    // `a` is now a use-after-free. Reading through it is caught, not silent.
    let _ = h.get_scalar(a, 0);
}

#[test]
#[should_panic(expected = "freed slot")]
fn using_a_freed_handle_panics() {
    let mut h = heap();
    let a = h.alloc(0, 1);
    h.free(a);
    // No realloc: the slot is tombstoned. Touching `a` is caught.
    let _ = h.get_scalar(a, 0);
}

#[test]
fn a_moving_collection_keeps_handles_valid() {
    // Relocation preserves a handle: same slot, same generation, new address.
    let mut h = heap();
    let a = h.alloc(0, 1);
    h.set_scalar(a, 0, 7);
    let id = (a.index(), a.generation());

    h.collect(); // evacuates a to the next generation (the object moves)

    assert_eq!(
        (a.index(), a.generation()),
        id,
        "the handle itself is unchanged"
    );
    assert_eq!(
        h.get_scalar(a, 0),
        7,
        "and it still resolves, to the moved object"
    );
}

#[test]
fn ordinary_values_are_not_handles() {
    let h = heap();
    // Ints and pointers lack the 0xABCD magic in bits 48..64.
    for v in [0i64, 1, 42, -1, i64::MAX, i64::MIN, 0x7FFF_0000_0000_0000] {
        assert!(
            !h.is_live_handle(Handle::from_bits(v)),
            "value {v:#018x} must not pass as a handle"
        );
    }
}

#[test]
fn a_handle_carries_the_magic() {
    let mut h = heap();
    let a = h.alloc(0, 1);
    // Bits 48..64 are exactly the magic.
    assert_eq!((a.to_bits() as u64) >> 48, 0xABCD);
    assert!(a.is_well_formed());
    assert!(h.is_live_handle(a));
}
