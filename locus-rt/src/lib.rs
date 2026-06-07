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
use std::collections::VecDeque;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::ptr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    Ask {
        prompt: String,
        response: String,
        used_default: bool,
    },
    Tell {
        text: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentTranscript {
    pub events: Vec<AgentEvent>,
    pub remaining_responses: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentHostEvent {
    Ask { prompt: String },
    Tell { text: String },
}

type LiveAgentCallback = Box<dyn FnMut(AgentHostEvent) -> Option<String>>;

#[derive(Debug)]
struct AgentSession {
    io: AgentIo,
    last_response_utf8: Vec<u8>,
    events: Vec<AgentEvent>,
}

enum AgentIo {
    Queued {
        responses: VecDeque<String>,
        default_response: String,
    },
    Live {
        callback: LiveAgentCallback,
    },
}

impl std::fmt::Debug for AgentIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentIo::Queued { responses, .. } => f
                .debug_struct("Queued")
                .field("remaining_responses", &responses.len())
                .finish(),
            AgentIo::Live { .. } => f.write_str("Live { .. }"),
        }
    }
}

impl AgentSession {
    fn new(responses: Vec<String>, default_response: String) -> Self {
        Self {
            io: AgentIo::Queued {
                responses: responses.into(),
                default_response,
            },
            last_response_utf8: Vec::new(),
            events: Vec::new(),
        }
    }

    fn live(callback: LiveAgentCallback) -> Self {
        Self {
            io: AgentIo::Live { callback },
            last_response_utf8: Vec::new(),
            events: Vec::new(),
        }
    }

    fn empty() -> Self {
        Self::new(Vec::new(), String::new())
    }

    fn into_transcript(self) -> AgentTranscript {
        let remaining_responses = match self.io {
            AgentIo::Queued { responses, .. } => responses.len(),
            AgentIo::Live { .. } => 0,
        };
        AgentTranscript {
            events: self.events,
            remaining_responses,
        }
    }
}

thread_local! {
    /// The program's managed heap — one per thread (Locus is single-threaded for
    /// now). Reserves a generous slab of address space; pages commit lazily, so
    /// the reservation is nearly free until the program actually allocates.
    static HEAP: RefCell<Heap> = RefCell::new(Heap::new(256 * 1024 * 1024));
    static AGENT_SESSION: RefCell<Option<AgentSession>> = RefCell::new(None);
}

/// Run `f` with a scoped text channel for generated Locus programs. Calls to
/// `agentAskText` consume queued responses and record both asks and tells in the
/// returned transcript. Outside a session the agent channel is inert.
pub fn with_agent_text_session<T, F>(
    responses: Vec<String>,
    default_response: String,
    f: F,
) -> (T, AgentTranscript)
where
    F: FnOnce() -> T,
{
    AGENT_SESSION.with(|slot| {
        let previous = slot.replace(Some(AgentSession::new(responses, default_response)));
        let result = catch_unwind(AssertUnwindSafe(f));
        let finished = slot.replace(previous).unwrap_or_else(AgentSession::empty);
        let transcript = finished.into_transcript();
        match result {
            Ok(value) => (value, transcript),
            Err(payload) => resume_unwind(payload),
        }
    })
}

/// Run `f` with a scoped live text channel. `callback` is called for every
/// `agentTellText` and `agentAskText`; on ask it may block until a host reply is
/// available. This is the runtime half of the MCP live-session tools.
pub fn with_agent_live_session<T, F, C>(callback: C, f: F) -> (T, AgentTranscript)
where
    F: FnOnce() -> T,
    C: FnMut(AgentHostEvent) -> Option<String> + 'static,
{
    AGENT_SESSION.with(|slot| {
        let previous = slot.replace(Some(AgentSession::live(Box::new(callback))));
        let result = catch_unwind(AssertUnwindSafe(f));
        let finished = slot.replace(previous).unwrap_or_else(AgentSession::empty);
        let transcript = finished.into_transcript();
        match result {
            Ok(value) => (value, transcript),
            Err(payload) => resume_unwind(payload),
        }
    })
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

/// Materialize a fresh root handle for the same managed object as `h` in the
/// current handle scope. Used for handle-valued loop accumulators: the root slot
/// lives for the whole loop while per-iteration temporaries come and go.
#[no_mangle]
pub extern "C" fn locus_gc_root(h: i64) -> i64 {
    HEAP.with(|heap| {
        let mut heap = heap.borrow_mut();
        let h = Handle::from_bits(h);
        let word = heap.to_ptr(h);
        heap.from_ptr(word).to_bits()
    })
}

/// Retarget an existing root handle at the object named by `value`, reusing the
/// root slot and allocating no handle-table entry. The `value` handle may belong
/// to a loop-body frame that is about to be popped.
#[no_mangle]
pub extern "C" fn locus_gc_root_set(root: i64, value: i64) -> i64 {
    HEAP.with(|heap| {
        let mut heap = heap.borrow_mut();
        let root = Handle::from_bits(root);
        heap.set(root, Handle::from_bits(value));
        root.to_bits()
    })
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

/// Allocate a full-width scalar `Array[Int]` and initialize every element.
#[no_mangle]
pub extern "C" fn locus_array_new_int(len: i64, value: i64) -> i64 {
    assert!(len >= 0, "array length {len} is negative");
    let len = usize::try_from(len).expect("array length does not fit usize");
    let scalar_cells = len.checked_add(1).expect("array length overflow");
    let scalar_cells =
        u32::try_from(scalar_cells).expect("array too large for managed heap object");
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let arr = heap.alloc(0, scalar_cells);
        heap.set_scalar(arr, 0, len as i64);
        for i in 0..len {
            heap.set_scalar(arr, 1 + i as u32, value);
        }
        arr.to_bits()
    })
}

fn materialize_utf16_units(src: &[u16]) -> i64 {
    let units = src.len();
    let data_cells = units.saturating_mul(2).div_ceil(8);
    let data_cells = u32::try_from(data_cells).expect("string payload too large for managed array");
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let arr = heap.alloc(0, 1 + data_cells);
        heap.set_scalar(arr, 0, units as i64);
        if units == 0 {
            return arr.to_bits();
        }
        for (i, unit) in src.iter().copied().enumerate() {
            let byte = i * 2;
            let cell = 1 + byte / 8;
            let shift = (byte % 8) * 8;
            let mask = 0xFFFFu64 << shift;
            let old = heap.get_scalar(arr, cell as u32) as u64;
            let payload = ((unit as u64) << shift) & mask;
            heap.set_scalar(arr, cell as u32, ((old & !mask) | payload) as i64);
        }
        arr.to_bits()
    })
}

fn read_utf16_units(heap: &Heap, s: i64) -> Vec<u16> {
    let arr = Handle::from_bits(s);
    let units = heap.get_scalar(arr, 0).max(0) as usize;
    let mut out = Vec::with_capacity(units);
    for i in 0..units {
        let byte = i * 2;
        let cell = 1 + byte / 8;
        let shift = (byte % 8) * 8;
        let raw = heap.get_scalar(arr, cell as u32) as u64;
        out.push(((raw >> shift) & 0xFFFF) as u16);
    }
    out
}

fn managed_string_to_rust(s: i64) -> String {
    let units = HEAP.with(|h| read_utf16_units(&h.borrow(), s));
    String::from_utf16_lossy(&units)
}

/// Ask the MCP/agent host for a UTF-8 response. The returned pointer remains
/// valid until the next agent call on this thread; `locus_agent_response_len`
/// returns its byte length. The public stdlib immediately copies it into a
/// managed Locus `String`.
#[no_mangle]
pub extern "C" fn locus_agent_ask_utf8(prompt: i64) -> *const u8 {
    let prompt = managed_string_to_rust(prompt);
    AGENT_SESSION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(session) = slot.as_mut() else {
            return ptr::null();
        };
        let (response, used_default) = match &mut session.io {
            AgentIo::Queued {
                responses,
                default_response,
            } => match responses.pop_front() {
                Some(response) => (response, false),
                None => (default_response.clone(), true),
            },
            AgentIo::Live { callback } => (
                callback(AgentHostEvent::Ask {
                    prompt: prompt.clone(),
                })
                .unwrap_or_default(),
                false,
            ),
        };
        session.events.push(AgentEvent::Ask {
            prompt,
            response: response.clone(),
            used_default,
        });
        session.last_response_utf8 = response.into_bytes();
        if session.last_response_utf8.is_empty() {
            ptr::null()
        } else {
            session.last_response_utf8.as_ptr()
        }
    })
}

/// Byte length for the last `locus_agent_ask_utf8` response.
#[no_mangle]
pub extern "C" fn locus_agent_response_len() -> i64 {
    AGENT_SESSION.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|session| session.last_response_utf8.len() as i64)
            .unwrap_or(0)
    })
}

/// Tell the MCP/agent host a line of managed Locus text.
#[no_mangle]
pub extern "C" fn locus_agent_tell_text(text: i64) {
    let text = managed_string_to_rust(text);
    AGENT_SESSION.with(|slot| {
        if let Some(session) = slot.borrow_mut().as_mut() {
            if let AgentIo::Live { callback } = &mut session.io {
                let _ = callback(AgentHostEvent::Tell { text: text.clone() });
            }
            session.events.push(AgentEvent::Tell { text });
        }
    });
}

fn read_string_pair(a: i64, b: i64) -> (Vec<u16>, Vec<u16>) {
    HEAP.with(|h| {
        let heap = h.borrow();
        (read_utf16_units(&heap, a), read_utf16_units(&heap, b))
    })
}

fn contains_at_units(haystack: &[u16], needle: &[u16], at: usize) -> bool {
    if haystack.len() < needle.len() || haystack.len() - needle.len() < at {
        return false;
    }
    haystack[at..at + needle.len()] == *needle
}

/// Materialize a managed Locus `String` from `len` UTF-16 code units at `ptr`.
/// This is the dynamic counterpart to string-literal lowering: strings are
/// scalar arrays with 16-bit stride and scalar slot 0 as the logical length.
#[no_mangle]
pub extern "C" fn locus_string_from_utf16(ptr: *const u16, len: i64) -> i64 {
    if ptr.is_null() || len <= 0 {
        return materialize_utf16_units(&[]);
    }
    let src = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    materialize_utf16_units(src)
}

/// Materialize a managed Locus `String` from `len` UTF-8 bytes at `ptr`.
/// Invalid byte sequences use Rust's replacement-character policy, matching the
/// stdlib's boundary stance of returning a valid Locus string rather than
/// leaking raw bytes into app code.
#[no_mangle]
pub extern "C" fn locus_string_from_utf8(ptr: *const u8, len: i64) -> i64 {
    if ptr.is_null() || len <= 0 {
        return materialize_utf16_units(&[]);
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let text = String::from_utf8_lossy(bytes);
    let units: Vec<u16> = text.encode_utf16().collect();
    materialize_utf16_units(&units)
}

/// Marshal a managed Locus `String` **out** to a fresh host-owned, NUL-terminated
/// UTF-8 C string; returns its pointer (`0` on failure). The inbound path for FFI
/// service plugins (a SQL query, a file path) — centralized here so a plugin's
/// boundary never hand-rolls the UTF-16→UTF-8 conversion (and can't leak it: the
/// service pairs every call with [`locus_cstr_free`]). Interior NULs are stripped
/// (a C string can't hold them). The Locus side stays a sealed `String`.
#[no_mangle]
pub extern "C" fn locus_string_to_cstr(s: i64) -> i64 {
    let units = HEAP.with(|h| read_utf16_units(&h.borrow(), s));
    let text = String::from_utf16_lossy(&units);
    let cleaned: String = text.chars().filter(|&c| c != '\0').collect();
    match std::ffi::CString::new(cleaned) {
        Ok(c) => c.into_raw() as i64,
        Err(_) => 0,
    }
}

/// Free a C string from [`locus_string_to_cstr`]. Idempotent on `0`.
#[no_mangle]
pub extern "C" fn locus_cstr_free(ptr: i64) {
    if ptr == 0 {
        return;
    }
    // SAFETY: `ptr` came from `CString::into_raw`; reclaim and drop it.
    unsafe {
        drop(std::ffi::CString::from_raw(
            ptr as *mut std::os::raw::c_char,
        ));
    }
}

/// Return a fresh empty managed string.
#[no_mangle]
pub extern "C" fn locus_string_empty() -> i64 {
    materialize_utf16_units(&[])
}

/// Return a managed string containing one UTF-16 code unit.
#[no_mangle]
pub extern "C" fn locus_string_unit(unit: i64) -> i64 {
    let unit = unit.clamp(0, 0xFFFF) as u16;
    materialize_utf16_units(&[unit])
}

/// Return a clipped UTF-16 code-unit slice of a managed string.
#[no_mangle]
pub extern "C" fn locus_string_slice(s: i64, start: i64, len: i64) -> i64 {
    let units = HEAP.with(|h| read_utf16_units(&h.borrow(), s));
    let start = start.max(0) as usize;
    let count = len.max(0) as usize;
    let start = start.min(units.len());
    let end = start.saturating_add(count).min(units.len());
    materialize_utf16_units(&units[start..end])
}

/// Return a fresh managed string containing `a` followed by `b`.
#[no_mangle]
pub extern "C" fn locus_string_concat(a: i64, b: i64) -> i64 {
    let (left, right) = HEAP.with(|h| {
        let heap = h.borrow();
        (read_utf16_units(&heap, a), read_utf16_units(&heap, b))
    });
    let mut out = Vec::with_capacity(left.len().saturating_add(right.len()));
    out.extend_from_slice(&left);
    out.extend_from_slice(&right);
    materialize_utf16_units(&out)
}

/// Return `s` repeated `count` times.
#[no_mangle]
pub extern "C" fn locus_string_repeat(s: i64, count: i64) -> i64 {
    let units = HEAP.with(|h| read_utf16_units(&h.borrow(), s));
    let count = count.max(0) as usize;
    let mut out = Vec::with_capacity(units.len().saturating_mul(count));
    for _ in 0..count {
        out.extend_from_slice(&units);
    }
    materialize_utf16_units(&out)
}

/// Return 1 when two managed strings contain the same UTF-16 code units.
#[no_mangle]
pub extern "C" fn locus_string_equals(a: i64, b: i64) -> i64 {
    let (left, right) = read_string_pair(a, b);
    (left == right) as i64
}

/// Lexicographic UTF-16 code-unit comparison: -1, 0, or 1.
#[no_mangle]
pub extern "C" fn locus_string_compare(a: i64, b: i64) -> i64 {
    let (left, right) = read_string_pair(a, b);
    match left.cmp(&right) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Return 1 when `needle` appears in `haystack` exactly at `at`.
#[no_mangle]
pub extern "C" fn locus_string_contains_at(haystack: i64, needle: i64, at: i64) -> i64 {
    if at < 0 {
        return 0;
    }
    let (haystack, needle) = read_string_pair(haystack, needle);
    contains_at_units(&haystack, &needle, at as usize) as i64
}

/// Find the first occurrence of `needle` at or after `start`, or -1.
#[no_mangle]
pub extern "C" fn locus_string_find_from(haystack: i64, needle: i64, start: i64) -> i64 {
    let (haystack, needle) = read_string_pair(haystack, needle);
    let start = start.max(0) as usize;
    if needle.is_empty() {
        return start.min(haystack.len()) as i64;
    }
    if haystack.len() < needle.len() || haystack.len() - needle.len() < start {
        return -1;
    }
    for i in start..=haystack.len() - needle.len() {
        if contains_at_units(&haystack, &needle, i) {
            return i as i64;
        }
    }
    -1
}

/// Find the last occurrence of `needle`, or -1. Empty needle returns len.
#[no_mangle]
pub extern "C" fn locus_string_last_find(haystack: i64, needle: i64) -> i64 {
    let (haystack, needle) = read_string_pair(haystack, needle);
    if needle.is_empty() {
        return haystack.len() as i64;
    }
    if haystack.len() < needle.len() {
        return -1;
    }
    for i in (0..=haystack.len() - needle.len()).rev() {
        if contains_at_units(&haystack, &needle, i) {
            return i as i64;
        }
    }
    -1
}

/// Count non-overlapping occurrences of `needle`. Empty needle counts as 0.
#[no_mangle]
pub extern "C" fn locus_string_count(haystack: i64, needle: i64) -> i64 {
    let (haystack, needle) = read_string_pair(haystack, needle);
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let mut i = 0;
    let mut count = 0;
    while i <= haystack.len() - needle.len() {
        if contains_at_units(&haystack, &needle, i) {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
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

/// Process-global sink for `perform console` lines. `None` → write the line to
/// stdout (the CLI default). A host installs a sink to route console lines into
/// its own surface instead of the OS console — the IDE points it at its console
/// pane, so a JIT'd program's `perform console` lands there. A plain `fn(&str)`
/// (the line is already decoded), so the host never touches the Locus heap.
static CONSOLE_SINK: std::sync::Mutex<Option<fn(&str)>> = std::sync::Mutex::new(None);

/// Install (`Some`) or clear (`None`) the console-line sink. See [`CONSOLE_SINK`].
pub fn set_console_sink(sink: Option<fn(&str)>) {
    if let Ok(mut g) = CONSOLE_SINK.lock() {
        *g = sink;
    }
}

/// The native `console` effect's default handler: write `s` as one line. The
/// prelude declares `console : String -> Unit`, and codegen routes an unhandled
/// `perform console s` here (mirroring `console_float` → [`locus_write_float`]).
/// Decodes the managed Locus `String` (on the heap-owning thread — this is
/// called from JIT'd code, never cross-thread) and routes it to the host sink if
/// one is installed, else stdout. The trailing newline matches "write a *line*".
#[no_mangle]
pub extern "C" fn locus_write_console_line(s: i64) {
    let line = managed_string_to_rust(s);
    let sink = CONSOLE_SINK.lock().ok().and_then(|g| *g);
    match sink {
        Some(f) => f(&line),
        None => println!("{line}"),
    }
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
        sym!("locus_gc_root", locus_gc_root),
        sym!("locus_gc_root_set", locus_gc_root_set),
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
        sym!("locus_array_new_int", locus_array_new_int),
        sym!("locus_string_from_utf16", locus_string_from_utf16),
        sym!("locus_string_from_utf8", locus_string_from_utf8),
        sym!("locus_string_to_cstr", locus_string_to_cstr),
        sym!("locus_cstr_free", locus_cstr_free),
        sym!("locus_string_empty", locus_string_empty),
        sym!("locus_string_unit", locus_string_unit),
        sym!("locus_string_slice", locus_string_slice),
        sym!("locus_string_concat", locus_string_concat),
        sym!("locus_string_repeat", locus_string_repeat),
        sym!("locus_string_equals", locus_string_equals),
        sym!("locus_string_compare", locus_string_compare),
        sym!("locus_string_contains_at", locus_string_contains_at),
        sym!("locus_string_find_from", locus_string_find_from),
        sym!("locus_string_last_find", locus_string_last_find),
        sym!("locus_string_count", locus_string_count),
        sym!("locus_agent_ask_utf8", locus_agent_ask_utf8),
        sym!("locus_agent_response_len", locus_agent_response_len),
        sym!("locus_agent_tell_text", locus_agent_tell_text),
        sym!("locus_gc_enter", locus_gc_enter),
        sym!("locus_gc_leave", locus_gc_leave),
        sym!("locus_gc_leave_with", locus_gc_leave_with),
        sym!("locus_write_float", locus_write_float),
        sym!("locus_write_console_line", locus_write_console_line),
        sym!("locus_fp64_add", locus_fp64_add),
        sym!("locus_fp64_add_i64", locus_fp64_add_i64),
        sym!("locus_fp32_id", locus_fp32_id),
    ]
}
