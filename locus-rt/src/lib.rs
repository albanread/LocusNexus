//! The Locus managed-heap runtime — the **`gc` effect's handler**.
//!
//! Generated code performs `gc` by `call`ing the `extern "C"` shims below; they
//! drive `locus-gc`'s handle collector. This crate is linked into a program two
//! ways, both exercising the *same* collector:
//!   - as an `rlib` into `locusc`, so the JIT can resolve the shim addresses via
//!     [`runtime_symbols`];
//!   - as a `staticlib` (`locus_rt.lib`) into an allocating program's `.exe`.
//!
//! `locus_alloc` (raw `malloc`, leaking) remains only for the paths not yet
//! migrated to the heap (closures); it disappears once they move over too.

use locus_gc::{Frame, Handle, Heap};
use std::alloc::{alloc, Layout};
use std::cell::RefCell;

thread_local! {
    /// The program's managed heap — one per thread (Locus is single-threaded for
    /// now). Reserves a generous slab of address space; pages commit lazily, so
    /// the reservation is nearly free until the program actually allocates.
    static HEAP: RefCell<Heap> = RefCell::new(Heap::new(256 * 1024 * 1024));
}

/// Allocate an object with `n_pointers` traced pointer fields followed by
/// `n_scalars` opaque scalar fields. Returns its **handle** (a stable, validated
/// table index) as `i64` — never a raw address, so the collector may relocate it
/// freely. This is `perform gc`.
#[no_mangle]
pub extern "C" fn locus_gc_alloc(n_pointers: i64, n_scalars: i64) -> i64 {
    HEAP.with(|h| {
        h.borrow_mut()
            .alloc(n_pointers as u32, n_scalars as u32)
            .to_bits()
    })
}

/// Store handle `target` into pointer field `field` of object `obj` (a traced
/// reference — keeps the target alive and is rewritten when it moves).
#[no_mangle]
pub extern "C" fn locus_gc_set_ptr(obj: i64, field: i64, target: i64) {
    HEAP.with(|h| {
        h.borrow_mut().set_ptr(
            Handle::from_bits(obj),
            field as u32,
            Handle::from_bits(target),
        )
    });
}

/// Store scalar `value` into scalar field `field` of object `obj`.
#[no_mangle]
pub extern "C" fn locus_gc_set_scalar(obj: i64, field: i64, value: i64) {
    HEAP.with(|h| {
        h.borrow_mut()
            .set_scalar(Handle::from_bits(obj), field as u32, value)
    });
}

/// Read pointer field `field` of `obj` as a fresh handle in the current scope.
#[no_mangle]
pub extern "C" fn locus_gc_get_ptr(obj: i64, field: i64) -> i64 {
    HEAP.with(|h| {
        h.borrow_mut()
            .get_ptr(Handle::from_bits(obj), field as u32)
            .to_bits()
    })
}

/// Read scalar field `field` of `obj`.
#[no_mangle]
pub extern "C" fn locus_gc_get_scalar(obj: i64, field: i64) -> i64 {
    HEAP.with(|h| h.borrow().get_scalar(Handle::from_bits(obj), field as u32))
}

/// Store a **raw repr-poly word** into pointer-region cell `field` of `obj`,
/// verbatim (no handle resolution). The store for a `Type::Var` (word) cell —
/// the field is traced (`classify` runs) but already holds the exact word: a
/// tag-room scalar (`value<<2`, `00`) or a traced object address (`addr|10`).
/// See [`locus_gc::Heap::set_word`].
#[no_mangle]
pub extern "C" fn locus_gc_set_word(obj: i64, field: i64, word: i64) {
    HEAP.with(|h| {
        h.borrow_mut()
            .set_word(Handle::from_bits(obj), field as u32, word)
    });
}

/// Read pointer-region cell `field` of `obj` as a **raw repr-poly word**,
/// verbatim (no interning). The inverse of [`locus_gc_set_word`]; the reader
/// decides whether to untag (`>>2`) or pass it through.
/// See [`locus_gc::Heap::get_word`].
#[no_mangle]
pub extern "C" fn locus_gc_get_word(obj: i64, field: i64) -> i64 {
    HEAP.with(|h| h.borrow().get_word(Handle::from_bits(obj), field as u32))
}

/// **ToPtr** — resolve managed handle `h` to its traced object word (`addr|10`)
/// for storage into a `Var` cell. See [`locus_gc::Heap::to_ptr`].
#[no_mangle]
pub extern "C" fn locus_gc_to_ptr(h: i64) -> i64 {
    HEAP.with(|heap| heap.borrow().to_ptr(Handle::from_bits(h)))
}

/// **FromPtr** — intern a raw `addr|10` word (read from a `Var` cell) into a
/// fresh handle. See [`locus_gc::Heap::from_ptr`].
#[no_mangle]
pub extern "C" fn locus_gc_from_ptr(word: i64) -> i64 {
    HEAP.with(|heap| heap.borrow_mut().from_ptr(word).to_bits())
}

/// Legacy capture shim: classify `value` by tag at runtime. New typed closures
/// use `locus_gc_set_ptr` / `locus_gc_set_scalar` so full-width scalar captures
/// are lossless.
#[no_mangle]
pub extern "C" fn locus_gc_set_capture(obj: i64, cell: i64, value: i64) {
    HEAP.with(|h| {
        h.borrow_mut()
            .set_capture(Handle::from_bits(obj), cell as u32, value)
    });
}

/// Legacy capture shim matching `locus_gc_set_capture`.
#[no_mangle]
pub extern "C" fn locus_gc_get_capture(obj: i64, cell: i64) -> i64 {
    HEAP.with(|h| {
        h.borrow_mut()
            .get_capture(Handle::from_bits(obj), cell as u32)
    })
}

/// The length of an array - its logical element count, stored in scalar slot 0.
#[no_mangle]
pub extern "C" fn locus_gc_len(arr: i64) -> i64 {
    HEAP.with(|h| h.borrow().get_scalar(Handle::from_bits(arr), 0))
}

/// Borrow the scalar-field base of an object for compiler-generated no-GC
/// regions. For arrays, scalar slot 0 is the logical length and scalar slot 1
/// begins the packed scalar payload.
#[no_mangle]
pub extern "C" fn locus_gc_scalar_fields_ptr(obj: i64) -> *mut u64 {
    HEAP.with(|h| h.borrow().scalar_fields_ptr(Handle::from_bits(obj)))
}

fn check_array_index(heap: &Heap, arr: Handle, i: i64) {
    let n = heap.get_scalar(arr, 0);
    assert!(
        i >= 0 && i < n,
        "array index {i} out of bounds (length {n})"
    );
}

fn check_stride(stride: i64) -> usize {
    assert!(
        matches!(stride, 1 | 2 | 4 | 8),
        "unsupported scalar array stride {stride}"
    );
    stride as usize
}

/// Bounds-checked read of a scalar array element with byte stride 1/2/4/8.
#[no_mangle]
pub extern "C" fn locus_gc_array_get_scalar_bytes(arr: i64, i: i64, stride: i64) -> i64 {
    HEAP.with(|h| {
        let heap = h.borrow();
        let a = Handle::from_bits(arr);
        check_array_index(&heap, a, i);
        let stride = check_stride(stride);
        let byte = i as usize * stride;
        let cell = 1 + byte / 8;
        let shift = (byte % 8) * 8;
        let raw = heap.get_scalar(a, cell as u32) as u64;
        let mask = if stride == 8 {
            u64::MAX
        } else {
            (1u64 << (stride * 8)) - 1
        };
        ((raw >> shift) & mask) as i64
    })
}

/// Bounds-checked read of a **reference** array element (returns a fresh handle).
#[no_mangle]
pub extern "C" fn locus_gc_array_get_ptr(arr: i64, i: i64) -> i64 {
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let a = Handle::from_bits(arr);
        check_array_index(&heap, a, i);
        heap.get_ptr(a, i as u32).to_bits()
    })
}

/// Bounds-checked read of a legacy full-cell scalar array element.
#[no_mangle]
pub extern "C" fn locus_gc_array_get_scalar(arr: i64, i: i64) -> i64 {
    locus_gc_array_get_scalar_bytes(arr, i, 8)
}

/// Bounds-checked write of a scalar array element with byte stride 1/2/4/8.
#[no_mangle]
pub extern "C" fn locus_gc_array_set_scalar_bytes(arr: i64, i: i64, stride: i64, v: i64) {
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let a = Handle::from_bits(arr);
        check_array_index(&heap, a, i);
        let stride = check_stride(stride);
        let byte = i as usize * stride;
        let cell = 1 + byte / 8;
        let shift = (byte % 8) * 8;
        let bits = stride * 8;
        let mask = if stride == 8 {
            u64::MAX
        } else {
            ((1u64 << bits) - 1) << shift
        };
        let old = heap.get_scalar(a, cell as u32) as u64;
        let payload = if stride == 8 {
            v as u64
        } else {
            ((v as u64) << shift) & mask
        };
        heap.set_scalar(a, cell as u32, ((old & !mask) | payload) as i64);
    });
}

/// Bounds-checked write of one **word** of a *multi-cell* scalar array element.
/// A vector element (`Quad[Float32]` = 2 cells, `Oct[Float32]` = 4) occupies
/// `cells` whole contiguous scalar cells; element `i` word `w` is scalar cell
/// `1 + i*cells + w` (scalar slot 0 is the length). Bounds-checks the *element*
/// index `i` against the logical length, then copies the word verbatim. The
/// SIMD multi-cell store path (`lower.rs`) unrolls one call per word.
#[no_mangle]
pub extern "C" fn locus_gc_array_set_scalar_cell(arr: i64, i: i64, cells: i64, w: i64, v: i64) {
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let a = Handle::from_bits(arr);
        check_array_index(&heap, a, i);
        let cell = 1 + i as usize * cells as usize + w as usize;
        heap.set_scalar(a, cell as u32, v);
    });
}

/// Bounds-checked read of one **word** of a multi-cell scalar array element —
/// the inverse of [`locus_gc_array_set_scalar_cell`].
#[no_mangle]
pub extern "C" fn locus_gc_array_get_scalar_cell(arr: i64, i: i64, cells: i64, w: i64) -> i64 {
    HEAP.with(|h| {
        let heap = h.borrow();
        let a = Handle::from_bits(arr);
        check_array_index(&heap, a, i);
        let cell = 1 + i as usize * cells as usize + w as usize;
        heap.get_scalar(a, cell as u32)
    })
}

/// Bounds-checked write of a legacy full-cell scalar array element.
#[no_mangle]
pub extern "C" fn locus_gc_array_set_scalar(arr: i64, i: i64, v: i64) {
    locus_gc_array_set_scalar_bytes(arr, i, 8, v);
}

/// Bounds-checked write of a **reference** array element.
#[no_mangle]
pub extern "C" fn locus_gc_array_set_ptr(arr: i64, i: i64, target: i64) {
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let a = Handle::from_bits(arr);
        check_array_index(&heap, a, i);
        heap.set_ptr(a, i as u32, Handle::from_bits(target));
    });
}

/// Enter a handle scope (function or loop-body entry). Returns the frame marker.
#[no_mangle]
pub extern "C" fn locus_gc_enter() -> i64 {
    HEAP.with(|h| h.borrow_mut().enter().raw() as i64)
}

/// Leave a scope, popping all its handles.
#[no_mangle]
pub extern "C" fn locus_gc_leave(frame: i64) {
    HEAP.with(|h| h.borrow_mut().leave(Frame::from_raw(frame as usize)));
}

/// Leave a scope, returning `result` to the caller's scope. The result is
/// **self-describing**: if it carries the handle magic it escapes (its object
/// stays rooted in the parent); otherwise it's an ordinary scalar and rides back
/// unchanged. So codegen needn't track statically whether a function returns a
/// handle or a number.
#[no_mangle]
pub extern "C" fn locus_gc_leave_with(frame: i64, result: i64) -> i64 {
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let r = Handle::from_bits(result);
        let f = Frame::from_raw(frame as usize);
        if heap.is_live_handle(r) {
            heap.leave_with(f, r).to_bits()
        } else {
            heap.leave(f);
            result
        }
    })
}

/// Allocate `bytes` of 8-aligned memory (raw, leaking). Still used by closures
/// (not yet migrated to the managed heap).
#[no_mangle]
pub extern "C" fn locus_alloc(bytes: i64) -> *mut u8 {
    let n = (bytes.max(1) as usize).next_multiple_of(8);
    match Layout::from_size_align(n, 8) {
        Ok(layout) => unsafe { alloc(layout) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// Write a scalar `Float` value whose bits ride through the uniform `i64` value
/// model. This fixed-signature helper lets codegen avoid C varargs.
#[no_mangle]
pub extern "C" fn locus_write_float(bits: i64) {
    println!("{}", f64::from_bits(bits as u64));
}

/// Fixed-signature FP helpers used to exercise the extern ABI from JIT tests.
/// They are ordinary C ABI symbols: FP args/returns must travel through native
/// FP registers, not through the uniform Locus `i64` cell representation.
#[no_mangle]
pub extern "C" fn locus_fp64_add(a: f64, b: f64) -> f64 {
    a + b
}

#[no_mangle]
pub extern "C" fn locus_fp64_add_i64(a: f64, b: i64) -> f64 {
    a + b as f64
}

#[no_mangle]
pub extern "C" fn locus_fp32_id(x: f32) -> f32 {
    x
}

/// `(symbol, address)` pairs to register with ORC as absolute symbols — the
/// bridge from JIT'd `call @locus_*` to the Rust functions above.
pub fn runtime_symbols() -> Vec<(&'static str, u64)> {
    macro_rules! sym {
        ($name:literal, $f:path) => {
            ($name, $f as *const () as usize as u64)
        };
    }
    vec![
        sym!("locus_alloc", locus_alloc),
        sym!("locus_gc_alloc", locus_gc_alloc),
        sym!("locus_gc_set_ptr", locus_gc_set_ptr),
        sym!("locus_gc_set_scalar", locus_gc_set_scalar),
        sym!("locus_gc_get_ptr", locus_gc_get_ptr),
        sym!("locus_gc_get_scalar", locus_gc_get_scalar),
        sym!("locus_gc_set_word", locus_gc_set_word),
        sym!("locus_gc_get_word", locus_gc_get_word),
        sym!("locus_gc_to_ptr", locus_gc_to_ptr),
        sym!("locus_gc_from_ptr", locus_gc_from_ptr),
        sym!("locus_gc_set_capture", locus_gc_set_capture),
        sym!("locus_gc_get_capture", locus_gc_get_capture),
        sym!("locus_gc_len", locus_gc_len),
        sym!("locus_gc_scalar_fields_ptr", locus_gc_scalar_fields_ptr),
        sym!("locus_gc_array_get_scalar", locus_gc_array_get_scalar),
        sym!(
            "locus_gc_array_get_scalar_bytes",
            locus_gc_array_get_scalar_bytes
        ),
        sym!("locus_gc_array_get_ptr", locus_gc_array_get_ptr),
        sym!("locus_gc_array_set_scalar", locus_gc_array_set_scalar),
        sym!(
            "locus_gc_array_set_scalar_bytes",
            locus_gc_array_set_scalar_bytes
        ),
        sym!(
            "locus_gc_array_get_scalar_cell",
            locus_gc_array_get_scalar_cell
        ),
        sym!(
            "locus_gc_array_set_scalar_cell",
            locus_gc_array_set_scalar_cell
        ),
        sym!("locus_gc_array_set_ptr", locus_gc_array_set_ptr),
        sym!("locus_gc_enter", locus_gc_enter),
        sym!("locus_gc_leave", locus_gc_leave),
        sym!("locus_gc_leave_with", locus_gc_leave_with),
        sym!("locus_write_float", locus_write_float),
        sym!("locus_fp64_add", locus_fp64_add),
        sym!("locus_fp64_add_i64", locus_fp64_add_i64),
        sym!("locus_fp32_id", locus_fp32_id),
    ]
}
