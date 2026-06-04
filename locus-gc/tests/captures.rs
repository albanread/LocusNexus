//! Closure capture layout tests. The legacy self-describing capture shims remain
//! covered here, but new typed closures use the ordinary object layout: pointer
//! captures in traced fields, scalar captures in raw scalar fields.

use locus_gc::{Handle, Heap};

fn heap() -> Heap {
    Heap::new(8 * 64 * 1024)
}

#[test]
fn scalar_captures_round_trip_through_the_fixnum_encoding() {
    let mut h = heap();
    let c = h.alloc(3, 0); // a 3-cell "closure"
    for (cell, v) in [(0u32, 7i64), (1, -42), (2, 1 << 40)] {
        h.set_capture(c, cell, v);
    }
    assert_eq!(h.get_capture(c, 0), 7);
    assert_eq!(h.get_capture(c, 1), -42, "sign preserved");
    assert_eq!(h.get_capture(c, 2), 1 << 40, "wide value preserved");
}

#[test]
fn typed_scalar_capture_cells_are_lossless_across_gc() {
    let mut h = heap();
    let c = h.alloc(0, 6);
    let bits = [
        0x0000_0000_0000_0000_u64,
        0x3ff0_0000_0000_0000,
        0x7ff8_0000_0000_0001,
        0x8000_0000_0000_0000,
        0xffff_ffff_ffff_ffff,
        0xabcd_0000_0000_0000,
    ];

    for (i, b) in bits.iter().enumerate() {
        h.set_scalar(c, i as u32, *b as i64);
    }

    h.collect();

    for (i, b) in bits.iter().enumerate() {
        assert_eq!(h.get_scalar(c, i as u32) as u64, *b);
    }
}

#[test]
fn typed_mixed_closure_layout_survives_collection() {
    let mut h = heap();

    let target = h.alloc(0, 1);
    h.set_scalar(target, 0, 12345);

    // Typed closure layout: pointer captures first, scalar 0 is fn-ptr, scalar
    // captures follow. Scalars deliberately use bit patterns the old fixnum
    // capture path cannot preserve.
    let c = h.alloc(1, 2);
    h.set_ptr(c, 0, target);
    h.set_scalar(c, 0, 0x8000_0000_0000_0000_u64 as i64);
    h.set_scalar(c, 1, 0x7ff8_0000_0000_0001_u64 as i64);

    h.free(target);

    let result = h.collect();
    assert_eq!(
        result.evac.objects_copied, 2,
        "closure and captured target survive"
    );

    let target2 = h.get_ptr(c, 0);
    assert_eq!(h.get_scalar(target2, 0), 12345);
    assert_eq!(h.get_scalar(c, 0) as u64, 0x8000_0000_0000_0000);
    assert_eq!(h.get_scalar(c, 1) as u64, 0x7ff8_0000_0000_0001);
}

#[test]
fn typed_scalar_slot_containing_handle_bits_is_not_traced() {
    let mut h = heap();

    let target = h.alloc(0, 1);
    h.set_scalar(target, 0, 99);
    let raw_handle_bits = target.to_bits();

    let c = h.alloc(0, 1);
    h.set_scalar(c, 0, raw_handle_bits);
    h.free(target);

    let result = h.collect();
    assert_eq!(
        result.evac.objects_copied, 1,
        "raw handle bits in a scalar slot must not retain the target"
    );
    assert_eq!(h.get_scalar(c, 0), raw_handle_bits);
}

#[test]
fn typed_self_capture_uses_pointer_slot() {
    let mut h = heap();
    let c = h.alloc(1, 1);
    h.set_ptr(c, 0, c);
    h.set_scalar(c, 0, 0x4321);

    let result = h.collect();
    assert_eq!(
        result.evac.objects_copied, 1,
        "self-referential typed closure survives"
    );
    let self_again = h.get_ptr(c, 0);
    assert!(h.is_live_handle(self_again));
    assert_eq!(h.get_scalar(c, 0), 0x4321);
}

#[test]
fn a_code_address_round_trips_as_a_capture() {
    // The fn-ptr is stored like any scalar capture; function addresses are
    // 16-aligned and fit the 62-bit fixnum, so they survive the shift exactly.
    let mut h = heap();
    let c = h.alloc(1, 0);
    let fn_ptr = 0x7FF6_1234_5670_i64; // aligned-ish, < 2^61
    h.set_capture(c, 0, fn_ptr);
    assert_eq!(h.get_capture(c, 0), fn_ptr);
}

#[test]
fn handle_capture_round_trips() {
    let mut h = heap();
    let target = h.alloc(0, 1);
    h.set_scalar(target, 0, 999);

    let c = h.alloc(1, 0);
    h.set_capture(c, 0, target.to_bits());

    // Comes back as a (fresh) handle to the same object.
    let got = Handle::from_bits(h.get_capture(c, 0));
    assert!(h.is_live_handle(got));
    assert_eq!(h.get_scalar(got, 0), 999);
}

#[test]
fn captured_handle_survives_collection_after_its_own_handle_is_freed() {
    // THE hazard, fixed: a closure captures a handle; the original handle is
    // dropped; the object is now reachable ONLY through the closure's captured
    // cell. A collection must keep it alive (the cell is traced) and relocate it.
    let mut h = heap();

    let target = h.alloc(0, 1);
    h.set_scalar(target, 0, 12345);

    let closure = h.alloc(2, 0); // [fn_ptr-ish scalar][captured handle]
    h.set_capture(closure, 0, 0x4321); // stand-in fn-ptr (scalar)
    h.set_capture(closure, 1, target.to_bits());

    // Drop the only direct handle to `target`. It now lives solely via `closure`.
    h.free(target);

    let result = h.collect();
    // closure + target both survive: tracing reached target through the cell.
    assert_eq!(
        result.evac.objects_copied, 2,
        "captured object kept alive through the closure"
    );

    // Read it back through the (relocated) capture.
    assert_eq!(h.get_capture(closure, 0), 0x4321, "scalar capture intact");
    let t2 = Handle::from_bits(h.get_capture(closure, 1));
    assert_eq!(
        h.get_scalar(t2, 0),
        12345,
        "captured object intact and reachable"
    );
}

#[test]
fn a_self_capturing_closure_is_fine() {
    // `let rec` self-capture: a closure cell pointing at its own object. Tracing
    // handles the cycle; the closure survives a collection.
    let mut h = heap();
    let c = h.alloc(1, 0);
    h.set_capture(c, 0, c.to_bits()); // capture self
    let result = h.collect();
    assert_eq!(
        result.evac.objects_copied, 1,
        "self-referential closure, one object"
    );
    // Following the self-capture gets back to the same object.
    let self_again = Handle::from_bits(h.get_capture(c, 0));
    assert!(h.is_live_handle(self_again));
}
