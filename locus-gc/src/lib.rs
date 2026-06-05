//! Locus's garbage-collected heap — handles over [NewGC](newgc_core).
//!
//! # The shape of a Locus heap object
//!
//! Locus boxes tuples, records, closures, and continuations. Unlike a Lisp
//! cons or `TinyLayout`'s all-pointer payload, a Locus object mixes **pointer**
//! fields (references to other heap objects) and **scalar** fields (raw `i64`s —
//! the uniform value model's integers and booleans). A record `{ next = obj, n = 42 }`
//! is one pointer and one scalar.
//!
//! NewGC traces a *contiguous* pointer range per object (`ObjectLayout`), so the
//! Locus compiler lays every object out as:
//!
//! ```text
//!   cell 0          : header  (encodes n_pointers and total payload length)
//!   cell 1 ..= P    : pointer fields   — tagged header-pointers, NewGC traces these
//!   cell P+1 ..= N  : scalar  fields   — raw i64, NewGC copies but never reads
//! ```
//!
//! Pointer fields come first so the traced range is `[1, 1+P)`. The scalar cells
//! sit outside that range: evacuation copies their bits verbatim and never calls
//! [`classify`](LocusLayout::classify) on them, so a scalar `i64` whose bit
//! pattern happens to look like a tagged pointer is harmless — the collector
//! simply never inspects it.
//!
//! # Tag discipline
//!
//! Identical to NewGC's proven `TinyLayout` (2-bit tag, validated by NewGC's
//! DAG/cycle/evacuation suite): a heap word is `immediate` (`00`), a
//! `header-pointer` (`10`), or a `forwarding marker` (`11`). Locus has no
//! cons-shaped objects, so tag `01` is never produced. `classify` is only ever
//! invoked on the tagged pointer cells `[1, 1+P)` and on a target's first cell
//! during the forward check — never on a live header or a scalar — so the
//! header's raw `(n_pointers, length)` encoding can never be misread as a word.

use newgc_core::page_heap::space::PageHeap;
use newgc_core::page_heap::CollectResult;
use newgc_core::word::Word;
use newgc_core::{Generation, HeapLayout, ObjectLayout, PointerKind, WordKind};

/// Number of tag bits in a Locus heap word.
pub const TAG_BITS: u32 = 2;
/// Mask selecting the tag bits.
pub const TAG_MASK: u64 = 0b11;
/// Mask selecting the payload (address / value), i.e. everything but the tag.
pub const PAYLOAD_MASK: u64 = !TAG_MASK;

/// Tag for an immediate cell (`00`). Used for the fill word; also the bit
/// pattern of a scalar field that, were it ever classified, reads as inert.
pub const TAG_IMMEDIATE: u64 = 0b00;
/// Tag for a pointer to a header-bearing heap object (`10`).
pub const TAG_HEADER: u64 = 0b10;
/// Tag for a forwarding marker left in a moved object's first cell (`11`).
pub const TAG_FORWARD: u64 = 0b11;

/// Build a tagged pointer to a header-bearing Locus object. GC heap pointers are
/// 8-byte aligned, so the low 2 bits are always clear and free for the tag.
#[inline(always)]
pub fn header_ptr(addr: *const u8) -> u64 {
    debug_assert!(
        (addr as u64) & TAG_MASK == 0,
        "heap pointer must be 4+-byte aligned"
    );
    (addr as u64) | TAG_HEADER
}

/// Build a forwarding marker pointing at the object's new location.
#[inline(always)]
pub fn forward(new_addr: *const u8) -> u64 {
    debug_assert!((new_addr as u64) & TAG_MASK == 0);
    (new_addr as u64) | TAG_FORWARD
}

/// Build the header cell for a Locus object with `n_pointers` pointer fields
/// followed by `n_scalars` scalar fields.
///
/// Encoding: `n_pointers` in the high 32 bits, total payload length
/// (`n_pointers + n_scalars`) in the low 32 bits. This single cell tells the
/// collector both the object's size (to copy it) and its traced range (to follow
/// and rewrite its outgoing pointers).
#[inline(always)]
pub const fn header(n_pointers: u32, n_scalars: u32) -> u64 {
    let total_payload = n_pointers as u64 + n_scalars as u64;
    ((n_pointers as u64) << 32) | (total_payload & 0xFFFF_FFFF)
}

/// Decode the pointer-field count from a header cell.
#[inline(always)]
pub const fn header_n_pointers(h: u64) -> usize {
    (h >> 32) as usize
}

/// Decode the total payload length (pointer + scalar cells) from a header cell.
#[inline(always)]
pub const fn header_payload_len(h: u64) -> usize {
    (h & 0xFFFF_FFFF) as usize
}

/// The Locus heap layout binding. Zero-sized marker type.
#[derive(Copy, Clone, Debug, Default)]
pub struct LocusLayout;

impl HeapLayout for LocusLayout {
    /// Fill freed/uninitialised cells with immediate zero — decodes as
    /// [`WordKind::Immediate`], never a stale pointer the scanner could follow.
    const FILL_WORD: u64 = 0;

    #[inline(always)]
    fn classify(raw: u64) -> WordKind {
        let addr = (raw & PAYLOAD_MASK) as *const u8;
        match raw & TAG_MASK {
            TAG_IMMEDIATE => WordKind::Immediate,
            TAG_HEADER => WordKind::PointerHeader(addr),
            TAG_FORWARD => WordKind::Forwarded(addr),
            // `01` is the cons tag in the shared scheme; Locus never produces a
            // cons-shaped object, so this arm is unreachable in practice. Treat
            // it as inert rather than panicking inside the collector.
            _ => WordKind::Immediate,
        }
    }

    #[inline(always)]
    fn make_forward(new_addr: *const u8) -> u64 {
        forward(new_addr)
    }

    #[inline(always)]
    fn make_pointer(addr: *const u8, _kind: PointerKind) -> u64 {
        // Locus objects are uniformly header-bearing; the cons kind never occurs.
        header_ptr(addr)
    }

    #[inline(always)]
    fn rewrite_pointer_addr(old_raw: u64, new_addr: *const u8) -> u64 {
        let tag = old_raw & TAG_MASK;
        (new_addr as u64) | tag
    }

    #[inline(always)]
    unsafe fn header_layout(header_cell: *const u64) -> ObjectLayout {
        let h = unsafe { *header_cell };
        let n_pointers = header_n_pointers(h);
        let total_payload = header_payload_len(h);
        debug_assert!(
            n_pointers <= total_payload,
            "more pointers than payload cells"
        );
        // [header][P pointer cells][(len-P) scalar cells]. NewGC traces [1, 1+P);
        // the scalar cells are copied but never classified.
        ObjectLayout {
            total_cells: 1 + total_payload,
            pointer_cells_start: 1,
            pointer_cells_end: 1 + n_pointers,
        }
    }
}

// ===========================================================================
// The handle heap.
// ===========================================================================

/// A **validated, generational** reference to a heap object — the only thing a
/// Locus program holds for one, never a raw pointer.
///
/// A handle is a 64-bit value packed as
///
/// ```text
///   bit 63                                  bit 0
///   ┌────────────┬─────────┬──────────────────┬──────────────────┐
///   │  MAGIC 16  │ TYPE  8 │  GENERATION  22  │     INDEX  18    │
///   └────────────┴─────────┴──────────────────┴──────────────────┘
/// ```
///
/// * **MAGIC** (`0xABCD`) makes a handle self-identifying. It is **not a
///   pointer** — `0xABCD` in bits 48-63 is a non-canonical address no real
///   pointer can take — and **not a plausible int** — an ordinary value hitting
///   that exact 16-bit pattern is a 1-in-65536 fluke, and a *meaningful* program
///   value doing so is vanishingly rare. So `is_well_formed` is a reliable
///   "is this a handle?" test on any `i64`, wherever it's found (a register, a
///   stack slot, a closure env).
/// * **GENERATION** detects staleness. Every time a table slot is reused for a
///   new object its generation bumps, so a handle left over from a *previous*
///   occupant fails validation — a use-after-free (e.g. a handle captured in a
///   closure whose object was later collected) becomes a clean error, not silent
///   corruption. A *moving* collection keeps the slot and generation (only the
///   table entry's address changes), so relocation never invalidates a handle.
/// * **TYPE** is reserved (0 today) for future handle kinds — tuple / record /
///   closure / array — so they can't be confused for one another.
/// * **INDEX** is the table slot. 18 bits ⇒ 256K *concurrent* handles; with
///   recycling, live handles stay tiny.
///
/// The table of object pointers it indexes is the collector's precise root set,
/// so the compiler emitting handle-based code stays oblivious to the GC.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Handle(u64);

impl Handle {
    const MAGIC: u64 = 0xABCD;
    const INDEX_BITS: u64 = 18;
    const GEN_BITS: u64 = 22;
    const TYPE_BITS: u64 = 8;
    const INDEX_MASK: u64 = (1 << Self::INDEX_BITS) - 1;
    const GEN_MASK: u64 = (1 << Self::GEN_BITS) - 1;
    const TYPE_MASK: u64 = (1 << Self::TYPE_BITS) - 1;
    const GEN_SHIFT: u64 = Self::INDEX_BITS;
    const TYPE_SHIFT: u64 = Self::INDEX_BITS + Self::GEN_BITS;
    const MAGIC_SHIFT: u64 = 48;

    /// Largest table index a handle can name (the 18-bit field's max).
    pub const MAX_INDEX: u32 = Self::INDEX_MASK as u32;

    /// Pack a slot index, its generation, and a kind tag into a handle.
    #[inline(always)]
    fn encode(index: u32, generation: u32, kind: u8) -> Handle {
        debug_assert!(
            index as u64 <= Self::INDEX_MASK,
            "handle index {index} exceeds the 18-bit field (256K concurrent handles)"
        );
        Handle(
            (Self::MAGIC << Self::MAGIC_SHIFT)
                | ((kind as u64 & Self::TYPE_MASK) << Self::TYPE_SHIFT)
                | ((generation as u64 & Self::GEN_MASK) << Self::GEN_SHIFT)
                | (index as u64 & Self::INDEX_MASK),
        )
    }

    /// The table slot this handle names.
    #[inline(always)]
    pub fn index(self) -> u32 {
        (self.0 & Self::INDEX_MASK) as u32
    }

    /// The generation stamped into this handle.
    #[inline(always)]
    pub fn generation(self) -> u32 {
        ((self.0 >> Self::GEN_SHIFT) & Self::GEN_MASK) as u32
    }

    /// The kind tag (reserved; 0 for all handles today).
    #[inline(always)]
    pub fn kind(self) -> u8 {
        ((self.0 >> Self::TYPE_SHIFT) & Self::TYPE_MASK) as u8
    }

    /// Does this value carry the handle magic? A cheap, reliable first-line check
    /// that rejects pointers and ordinary integers before any table access.
    #[inline(always)]
    pub fn is_well_formed(self) -> bool {
        (self.0 >> Self::MAGIC_SHIFT) == Self::MAGIC
    }

    /// The value-world bits — what generated code holds and passes around as i64.
    #[inline(always)]
    pub fn to_bits(self) -> i64 {
        self.0 as i64
    }

    /// Reinterpret an i64 as a handle (no validation; pair with
    /// [`is_well_formed`](Handle::is_well_formed) / the heap's `resolve`).
    #[inline(always)]
    pub fn from_bits(v: i64) -> Handle {
        Handle(v as u64)
    }
}

/// A saved position of the handle stack, returned by [`Heap::enter`] and
/// consumed by [`Heap::leave`]. Restoring it pops every handle allocated since.
#[derive(Copy, Clone, Debug)]
pub struct Frame(usize);

impl Frame {
    /// The raw stack position. Exposed so a C ABI (the codegen runtime shims)
    /// can carry a frame as a plain integer across the FFI boundary.
    #[inline(always)]
    pub fn raw(self) -> usize {
        self.0
    }

    /// Rebuild a frame from a raw stack position (inverse of [`raw`](Frame::raw)).
    #[inline(always)]
    pub fn from_raw(n: usize) -> Frame {
        Frame(n)
    }
}

/// Locus's own GC statistics — the **handle / root** view that complements
/// NewGC's physical [`GcStats`](newgc_core::GcStats) (bytes, pages, generations).
///
/// Where NewGC answers *"how much memory,"* these answer *"how the program uses
/// it"*: how heap-object lifetimes collapse onto the scoped handle stack, how
/// much apparent allocation is real churn versus allocation-free slot reuse, and
/// how little survives each collection. The headline numbers are
/// [`slot_reuse_ratio`](Stats::slot_reuse_ratio) (loops walking chains for free)
/// and [`handle_stack_peak`](Stats#structfield.handle_stack_peak) (the live-root
/// high-water mark — small even for huge object graphs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    // -- Allocation (lifetime totals, program-driven) ----------------------
    /// Objects allocated over the heap's life (every [`Heap::alloc`]).
    pub objects_allocated: u64,
    /// Cells allocated, including object headers.
    pub cells_allocated: u64,
    /// Pointer cells allocated — the traced part of the heap.
    pub pointer_cells_allocated: u64,
    /// Scalar cells allocated — the opaque part the collector copies but never reads.
    pub scalar_cells_allocated: u64,
    /// [`Heap::alloc`] calls that found the young generation full and had to run
    /// a collection then retry (the "infinite memory" backstop firing).
    pub alloc_triggered_collections: u64,

    // -- Handles & scopes (the stack-discipline view) ----------------------
    /// New table slots taken ([`Heap::alloc`] + [`Heap::get_ptr`]) — the true
    /// handle allocations.
    pub handles_interned: u64,
    /// Allocation-free slot updates ([`Heap::set`] + [`Heap::step_ptr`]) — the
    /// loop-variable reuse that keeps a chain walk at O(1) handles. High relative
    /// to `handles_interned` means the program is *walking*, not *retaining*.
    pub slot_reuses: u64,
    /// Scopes entered ([`Heap::enter`]).
    pub frames_entered: u64,
    /// Scopes left ([`Heap::leave`] + [`Heap::leave_with`]).
    pub frames_left: u64,
    /// Results escaped into a parent scope ([`Heap::leave_with`]).
    pub escapes: u64,
    /// Deepest the handle stack ever got — the live-root high-water mark.
    pub handle_stack_peak: usize,

    // -- Collections (the survival view) -----------------------------------
    /// Collections run, under any root model.
    pub collections: u64,
    /// Objects that survived a collection, summed over every cycle.
    pub objects_copied: u64,
    /// Cells copied (survivor volume).
    pub cells_copied: u64,
    /// `from`-generation pages reclaimed to Free across all cycles.
    pub pages_freed: u64,
}

impl Stats {
    /// Bytes allocated over the heap's life (cells × 8).
    pub fn bytes_allocated(&self) -> u64 {
        self.cells_allocated * 8
    }

    /// Fraction of allocated objects that survived a collection, averaged over
    /// the run. Near 0 = healthy generational churn (most objects die young);
    /// near 1 = long-lived data, or under-collecting. `None` before any alloc.
    pub fn survival_rate(&self) -> Option<f64> {
        (self.objects_allocated != 0)
            .then(|| self.objects_copied as f64 / self.objects_allocated as f64)
    }

    /// Fraction of handle updates that *reused* a slot instead of allocating one.
    /// High = loops walk chains without growing the root set (the scoped win).
    /// `None` before any handle activity.
    pub fn slot_reuse_ratio(&self) -> Option<f64> {
        let total = self.handles_interned + self.slot_reuses;
        (total != 0).then(|| self.slot_reuses as f64 / total as f64)
    }

    /// Net open scopes (`entered − left`). Zero at a quiescent point; a non-zero
    /// value at program end means a scope leaked.
    pub fn open_frames(&self) -> i64 {
        self.frames_entered as i64 - self.frames_left as i64
    }

    /// Single-line `key=value` diagnostic, mirroring NewGC's `GcStats::render`.
    pub fn render(&self) -> String {
        let pct = |o: Option<f64>| {
            o.map(|v| format!("{:.1}%", v * 100.0))
                .unwrap_or_else(|| "n/a".into())
        };
        format!(
            "objects={} cells={} bytes={} ptr_cells={} scalar_cells={} \
             handles_interned={} slot_reuses={} reuse={} peak_depth={} \
             frames={}/{} open={} escapes={} \
             collections={} alloc_forced={} survived_objs={} survived_cells={} pages_freed={} survival={}",
            self.objects_allocated, self.cells_allocated, self.bytes_allocated(),
            self.pointer_cells_allocated, self.scalar_cells_allocated,
            self.handles_interned, self.slot_reuses, pct(self.slot_reuse_ratio()),
            self.handle_stack_peak, self.frames_entered, self.frames_left,
            self.open_frames(), self.escapes,
            self.collections, self.alloc_triggered_collections,
            self.objects_copied, self.cells_copied, self.pages_freed,
            pct(self.survival_rate()),
        )
    }
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pct = |o: Option<f64>| {
            o.map(|v| format!("{:.1}%", v * 100.0))
                .unwrap_or_else(|| "n/a".into())
        };
        writeln!(f, "Locus GC stats (handle/root view):")?;
        writeln!(
            f,
            "  allocated : {} objects, {} cells ({} B) — {} ptr / {} scalar",
            self.objects_allocated,
            self.cells_allocated,
            self.bytes_allocated(),
            self.pointer_cells_allocated,
            self.scalar_cells_allocated,
        )?;
        writeln!(
            f,
            "  handles   : {} interned, {} slot-reuses ({} reuse), peak depth {}",
            self.handles_interned,
            self.slot_reuses,
            pct(self.slot_reuse_ratio()),
            self.handle_stack_peak,
        )?;
        writeln!(
            f,
            "  scopes    : {} entered, {} left ({} open), {} escapes",
            self.frames_entered,
            self.frames_left,
            self.open_frames(),
            self.escapes,
        )?;
        write!(
            f,
            "  collected : {} cycles ({} alloc-forced), {} objects / {} cells survived, {} pages freed, survival {}",
            self.collections, self.alloc_triggered_collections,
            self.objects_copied, self.cells_copied, self.pages_freed, pct(self.survival_rate()),
        )
    }
}

/// A free table slot holds this immediate sentinel. `classify` reads it as
/// [`WordKind::Immediate`], so the collector skips it — a freed handle is never
/// a root, and the object it used to name dies unless something else retains it.
const TOMBSTONE: u64 = 0;

/// Locus's garbage-collected heap: a [`PageHeap<LocusLayout>`] plus a handle
/// table. The table doubles as the precise root set handed to the collector.
pub struct Heap {
    inner: PageHeap<LocusLayout>,
    /// Handle table: index → tagged object pointer (or [`TOMBSTONE`]). The whole
    /// vector is the GC root set.
    table: Vec<Word>,
    /// Generation of the current occupant of each slot, parallel to `table` but
    /// **persistent** — it survives a scope `leave` (table truncation) so a slot
    /// re-pushed later can bump its generation and invalidate stale handles.
    gen: Vec<u32>,
    /// Recycled handle indices, so the table doesn't grow without bound.
    free: Vec<u32>,
    /// Locus's own handle/root-view statistics.
    stats: Stats,
}

impl Heap {
    /// Create a heap reserving `reserved_bytes` of address space. Pages are
    /// committed lazily as the heap grows, so the reservation can be generous.
    pub fn new(reserved_bytes: usize) -> Self {
        let mut inner = PageHeap::with_reservation(reserved_bytes);
        // Preserve evacuation headroom. NewGC's default 8 MiB trigger budget is
        // appropriate for the normal runtime reservation, but small heaps used
        // by regression tests need a proportionally smaller trigger.
        let trigger_budget = (reserved_bytes / 8).clamp(64 * 1024, 8 * 1024 * 1024);
        inner.set_gc_budget_min_bytes(trigger_budget);
        Heap {
            inner,
            table: Vec::new(),
            gen: Vec::new(),
            free: Vec::new(),
            stats: Stats::default(),
        }
    }

    /// Update the handle-stack high-water mark after a push.
    #[inline]
    fn note_peak(&mut self) {
        if self.table.len() > self.stats.handle_stack_peak {
            self.stats.handle_stack_peak = self.table.len();
        }
    }

    /// Validate `h` and return its live slot index, or `None` if the handle is
    /// malformed (bad magic), names a slot past the live table, points at a freed
    /// (tombstoned) slot, or is **stale** (its generation no longer matches the
    /// slot's current occupant — a use-after-free).
    #[inline]
    fn try_resolve(&self, h: Handle) -> Option<usize> {
        if !h.is_well_formed() {
            return None;
        }
        let i = h.index() as usize;
        if i >= self.table.len() || self.table[i].raw() == TOMBSTONE {
            return None;
        }
        (self.gen[i] == h.generation()).then_some(i)
    }

    /// Validate `h` and return its live slot index, panicking with a precise
    /// diagnosis on failure — so a use-after-free surfaces as an immediate, clear
    /// error rather than silent corruption.
    #[inline]
    fn resolve(&self, h: Handle) -> usize {
        assert!(
            h.is_well_formed(),
            "not a handle (bad magic): {:#018x}",
            h.0
        );
        let i = h.index() as usize;
        assert!(
            i < self.table.len(),
            "handle index {i} past live table (len {})",
            self.table.len()
        );
        assert!(
            self.table[i].raw() != TOMBSTONE,
            "handle to a freed slot {i}"
        );
        assert!(
            self.gen[i] == h.generation(),
            "stale handle: slot {i} is now generation {}, handle is {} (use-after-free)",
            self.gen[i],
            h.generation()
        );
        i
    }

    /// Occupy slot `i` with `w`, assigning the next generation. A slot reused
    /// after a prior occupant bumps its generation (invalidating stale handles to
    /// that occupant); a brand-new slot starts at generation 0.
    #[inline]
    fn occupy(&mut self, i: usize, w: Word) -> Handle {
        let g = if i < self.gen.len() {
            self.gen[i] = self.gen[i].wrapping_add(1) & (Handle::GEN_MASK as u32);
            self.gen[i]
        } else {
            debug_assert!(i == self.gen.len());
            self.gen.push(0);
            0
        };
        self.table[i] = w;
        Handle::encode(i as u32, g, 0)
    }

    /// Register a freshly-built object Word in the handle table, reusing a freed
    /// slot when one is available.
    fn intern(&mut self, w: Word) -> Handle {
        self.stats.handles_interned += 1;
        if let Some(i) = self.free.pop() {
            self.occupy(i as usize, w)
        } else {
            let i = self.table.len();
            self.table.push(w);
            self.note_peak();
            self.occupy(i, w)
        }
    }

    /// Raw mutable pointer to an object's header cell (validates the handle).
    #[inline(always)]
    fn obj_ptr(&self, h: Handle) -> *mut u64 {
        let i = self.resolve(h);
        (self.table[i].raw() & PAYLOAD_MASK) as *mut u64
    }

    /// Allocate an object with `n_pointers` pointer fields followed by
    /// `n_scalars` scalar fields, all initialised to immediate zero. Returns a
    /// handle. Runs the page heap's trigger-aware collection policy before the
    /// evacuation reserve is gone; if allocation still fails, collects and
    /// retries once — the "reliable emulation of infinite memory."
    pub fn alloc(&mut self, n_pointers: u32, n_scalars: u32) -> Handle {
        if self.inner.should_collect() {
            self.collect_auto();
        }
        let total = 1 + n_pointers as usize + n_scalars as usize;
        let p = match self.inner.try_alloc_boxed_in(Generation::G0, total) {
            Some(p) => p,
            None => {
                self.stats.alloc_triggered_collections += 1;
                self.collect_auto();
                self.inner
                    .try_alloc_boxed_in(Generation::G0, total)
                    .expect("heap exhausted even after collection")
            }
        };
        unsafe {
            *p.as_ptr() = header(n_pointers, n_scalars);
            for i in 1..total {
                *p.as_ptr().add(i) = LocusLayout::FILL_WORD;
            }
        }
        self.stats.objects_allocated += 1;
        self.stats.cells_allocated += total as u64;
        self.stats.pointer_cells_allocated += n_pointers as u64;
        self.stats.scalar_cells_allocated += n_scalars as u64;
        let w = Word::from_raw(header_ptr(p.as_ptr() as *const u8));
        self.intern(w)
    }

    /// Number of pointer fields in `h`'s object (decoded from its header).
    #[inline]
    pub fn n_pointers(&self, h: Handle) -> usize {
        header_n_pointers(unsafe { *self.obj_ptr(h) })
    }

    /// Total payload length (pointer + scalar cells) of `h`'s object.
    #[inline]
    pub fn payload_len(&self, h: Handle) -> usize {
        header_payload_len(unsafe { *self.obj_ptr(h) })
    }

    /// Store `target` into pointer field `field` of `obj`. The field holds a
    /// real (traced) pointer to the target object, so the collector keeps the
    /// target alive through `obj` and rewrites the field when the target moves.
    pub fn set_ptr(&mut self, obj: Handle, field: u32, target: Handle) {
        debug_assert!(
            (field as usize) < self.n_pointers(obj),
            "pointer field out of range"
        );
        let target_word = self.table[self.resolve(target)].raw();
        unsafe { *self.obj_ptr(obj).add(1 + field as usize) = target_word };
    }

    /// Read pointer field `field` of `obj` into a **fresh** handle in the current
    /// scope. Use this when the loaded reference must live on as a *new* binding
    /// (e.g. `let child = node.left`). For stepping a single binding along a
    /// chain (the common loop case — `node = node.next`), prefer
    /// [`step_ptr`](Heap::step_ptr), which reuses one slot instead of allocating
    /// a handle per hop.
    ///
    /// Interning the same object twice yields two handles to one object — sound,
    /// because both are simply roots the collector forwards identically.
    pub fn get_ptr(&mut self, obj: Handle, field: u32) -> Handle {
        debug_assert!(
            (field as usize) < self.n_pointers(obj),
            "pointer field out of range"
        );
        let w = unsafe { *self.obj_ptr(obj).add(1 + field as usize) };
        self.intern(Word::from_raw(w))
    }

    /// Store a **raw repr-poly word** into *pointer-region* cell `field` of `obj`,
    /// **verbatim** — no handle resolution (`docs/repr-poly-impl.md` D4). This is
    /// the store for a `Type::Var` (word) cell: the field is in the traced range
    /// (so `classify` runs on it during evacuation), but it already holds a
    /// repr-poly word — either a real interior pointer (`addr|10`, the collector
    /// follows it) or a tag-room scalar (`value<<2`, low bits `00`, skipped).
    ///
    /// Distinct from [`set_ptr`](Heap::set_ptr) (which resolves its argument as a
    /// program handle through the table — it would *fault* on a tagged scalar that
    /// is not a live handle) and from [`set_scalar`](Heap::set_scalar) (which
    /// writes the *untraced* scalar region — wrong region for a value that may be a
    /// pointer). The caller has already produced the exact word to store (a tag
    /// shift `v<<2`, or a handle's traced object address); this just lays it down.
    pub fn set_word(&mut self, obj: Handle, field: u32, word: i64) {
        debug_assert!(
            (field as usize) < self.n_pointers(obj),
            "word field out of pointer range"
        );
        // A well-formed word-cell value is exactly a tag-room scalar (`v<<2`, low
        // bits `00`) or a real interior pointer (`addr|10`). It is **never** a
        // program *handle* (a `0xABCD…` table index): those must be resolved to
        // `addr|10` first (the matrix's `ToPtr` / `set_ptr` case). That resolution
        // is not wired for this slice (list_len/list_reverse over `Int` never store
        // a concrete handle into a `Var` cell — they store tagged scalars and
        // passthrough words), so a handle reaching here is a mis-routed coercion.
        // This cheap magic-bit check (no table access — *not* a trace-path gate)
        // catches that in debug builds rather than letting index bits be `classify`d.
        // TODO(repr-poly ToPtr): when a concrete handle may flow into a `Var` cell
        // (e.g. `Cons((1,2), Nil)`), resolve it here / at a `ToPtr` coercion.
        debug_assert!(
            !Handle::from_bits(word).is_well_formed(),
            "set_word got a program handle (0xABCD index); a concrete handle into a \
             Var cell needs ToPtr/set_ptr resolution (not wired for the list_len slice)"
        );
        unsafe { *self.obj_ptr(obj).add(1 + field as usize) = word as u64 };
    }

    /// Read *pointer-region* cell `field` of `obj` as a **raw repr-poly word**,
    /// **verbatim** — the inverse of [`set_word`](Heap::set_word). No interning:
    /// the word comes back exactly as stored (a tagged scalar `v<<2`, or a traced
    /// object address `addr|10`), because the reader (the load matrix) decides how
    /// to interpret it (untag, or pass through). Contrast [`get_ptr`](Heap::get_ptr),
    /// which interns the cell into a fresh handle and so would mis-handle a tagged
    /// scalar sitting in the same traced range.
    ///
    /// Soundness of reading *after* a possible collection: a word cell is in the
    /// traced range, so if it held an interior pointer the evacuator rewrote it to
    /// the object's new address; a tagged scalar (`00`) was left untouched. Either
    /// way the verbatim word remains valid.
    pub fn get_word(&self, obj: Handle, field: u32) -> i64 {
        debug_assert!(
            (field as usize) < self.n_pointers(obj),
            "word field out of pointer range"
        );
        unsafe { *self.obj_ptr(obj).add(1 + field as usize) as i64 }
    }

    /// **ToPtr** — resolve a managed `handle` to its traced object word
    /// (`addr|10`), the exact value [`set_ptr`](Heap::set_ptr) stores. This lets a
    /// concrete handle be laid into a `Var` (word) cell as a *real interior
    /// pointer* the collector follows and rewrites on evacuation (the matrix's
    /// `ToPtr` coercion). The result is `classify`d `10` (not the program-handle
    /// magic that `set_word` rejects). Inverse of [`from_ptr`](Heap::from_ptr).
    pub fn to_ptr(&self, handle: Handle) -> i64 {
        self.table[self.resolve(handle)].raw() as i64
    }

    /// **FromPtr** — intern a raw `addr|10` object `word` (read verbatim from a
    /// `Var` cell via [`get_word`](Heap::get_word)) into a **fresh** handle, the
    /// interning tail of [`get_ptr`](Heap::get_ptr). The `FromPtr` coercion:
    /// recover a usable handle from a word cell that holds a managed reference.
    pub fn from_ptr(&mut self, word: i64) -> Handle {
        self.intern(Word::from_raw(word as u64))
    }

    /// Store a scalar `i64` into scalar field `field` of `obj` (the scalar cells
    /// follow the pointer cells). The collector copies but never inspects it.
    pub fn set_scalar(&mut self, obj: Handle, field: u32, value: i64) {
        let base = 1 + self.n_pointers(obj);
        debug_assert!(
            (field as usize) < self.payload_len(obj) - self.n_pointers(obj),
            "scalar field out of range"
        );
        unsafe { *self.obj_ptr(obj).add(base + field as usize) = value as u64 };
    }

    /// Read scalar field `field` of `obj`.
    pub fn get_scalar(&self, obj: Handle, field: u32) -> i64 {
        let base = 1 + self.n_pointers(obj);
        unsafe { *self.obj_ptr(obj).add(base + field as usize) as i64 }
    }

    /// Raw pointer to scalar field 0 of `obj`.
    ///
    /// This is for compiler-generated no-GC regions only: the pointer becomes
    /// stale if a collection can run before the generated code is done with it.
    pub fn scalar_fields_ptr(&self, obj: Handle) -> *mut u64 {
        let base = 1 + self.n_pointers(obj);
        unsafe { self.obj_ptr(obj).add(base) }
    }

    // -- Legacy closure captures: self-describing cells -----------------------
    //
    // New compiler output uses the normal typed object layout: pointer captures
    // are stored with `set_ptr`, scalar captures with `set_scalar`, and scalar
    // bits are preserved losslessly. These shims remain for older tests and
    // transitional code only. A legacy closure is an all-pointer-cells object
    // (`alloc(1 + n_captures, 0)`), so the collector classifies every cell. We
    // don't tell it which captures are references — the *value* does, via the
    // handle magic:
    //   * a captured handle is stored as the traced object address (tag-10), so
    //     the collector keeps the captured object alive AND rewrites the cell
    //     when it moves — exactly the capture hazard, fixed.
    //   * a captured scalar (or a code address like the fn-ptr) is stored as a
    //     tag-00 fixnum (`v << 2`) the collector reads as immediate and ignores.
    // The tag in each cell then tells the reader which it is.

    /// Legacy: store `value` into closure cell `cell` of `obj`, classifying it
    /// by tag. This is lossy for full-width scalar bit patterns.
    pub fn set_capture(&mut self, obj: Handle, cell: u32, value: i64) {
        let probe = Handle::from_bits(value);
        let word = if probe.is_well_formed() {
            if let Some(i) = self.try_resolve(probe) {
                // A live handle → store the traced object address (tag-10).
                self.table[i].raw()
            } else {
                // Well-formed magic but not live — treat as a (rare) scalar.
                ((value as u64) << 2) & !TAG_MASK
            }
        } else {
            // A scalar / code address → tag-00 fixnum the collector ignores.
            ((value as u64) << 2) & !TAG_MASK
        };
        let base = self.obj_ptr(obj);
        unsafe { *base.add(1 + cell as usize) = word };
    }

    /// Legacy: read closure cell `cell` of `obj`. A traced-pointer cell comes
    /// back as a fresh handle (the captured object, wherever the collector moved
    /// it); a fixnum cell comes back as the original scalar when it fit the old
    /// 62-bit encoding.
    pub fn get_capture(&mut self, obj: Handle, cell: u32) -> i64 {
        let word = unsafe { *self.obj_ptr(obj).add(1 + cell as usize) };
        if word & TAG_MASK == TAG_HEADER {
            // A handle capture: intern the (possibly relocated) address.
            self.intern(Word::from_raw(word)).to_bits()
        } else {
            // A scalar capture: undo the fixnum shift (sign-preserving).
            (word as i64) >> 2
        }
    }

    /// Drop a handle: tombstone its table slot and recycle the index. The object
    /// it named is reclaimed at the next collection unless another handle or a
    /// live object's pointer field still reaches it.
    ///
    /// This is the *explicit* lifetime model. For codegen, prefer the scoped
    /// handle stack ([`enter`](Heap::enter)/[`leave`](Heap::leave)), which frees
    /// a whole scope's handles in O(1) and never leaves holes.
    pub fn free(&mut self, h: Handle) {
        let i = self.resolve(h);
        self.table[i] = Word::from_raw(TOMBSTONE);
        self.free.push(i as u32);
    }

    // -- Scoped handle stack -------------------------------------------------
    //
    // The allocation discipline codegen actually uses. The crucial property
    // that makes it cheap: under design Z, a heap object's fields hold *raw
    // traced pointers*, never handles. So a handle never lives inside a heap
    // object — it only ever names a live *local or temporary*. Handle lifetime
    // therefore coincides with lexical scope, and the handle table can be run as
    // a plain STACK: a function (or loop body) `enter`s a frame, allocates its
    // handles, and `leave`s — popping them all in O(1). No free list, no holes,
    // no conservative scan: every slot below the stack top is, by construction,
    // a live root, so [`collect`](Heap::collect) over the whole table IS the
    // precise collection. Walking an N-node list costs O(1) live handles, not N.

    /// Enter a handle scope: snapshot the current top of the handle stack.
    /// Pair with [`leave`](Heap::leave) (or [`leave_with`](Heap::leave_with)).
    #[inline]
    pub fn enter(&mut self) -> Frame {
        debug_assert!(
            self.free.is_empty(),
            "scoped stack and free-list models must not be mixed"
        );
        self.stats.frames_entered += 1;
        Frame(self.table.len())
    }

    /// Leave a scope: pop every handle allocated since `frame` was entered. O(1)
    /// bulk reclamation. The objects those handles named are reclaimed at the
    /// next collection unless something still rooted reaches them.
    #[inline]
    pub fn leave(&mut self, frame: Frame) {
        debug_assert!(frame.0 <= self.table.len(), "frames must nest (LIFO)");
        self.stats.frames_left += 1;
        self.table.truncate(frame.0);
    }

    /// Leave a scope but let one `result` handle *escape* into the parent scope —
    /// the function-return idiom (like V8's `EscapableHandleScope`).
    ///
    /// Two cases, matching how a function produces its return value:
    ///
    /// * **A value the function computed** (`result` lives *in* this scope, e.g.
    ///   a freshly allocated object or `node.left`). It's copied down to the
    ///   frame boundary and returned renumbered into the caller's scope; every
    ///   other handle in the scope is popped. The result lands at the caller's
    ///   next free slot, exactly like a stack machine pushing a return value.
    ///
    /// * **A handle the function was passed through** (`result` lives *below*
    ///   this scope — e.g. `fn id(x) = x`, or returning one of its arguments).
    ///   It's already rooted in the caller, so we just pop our own handles and
    ///   hand back the original handle unchanged — no copy.
    #[inline]
    pub fn leave_with(&mut self, frame: Frame, result: Handle) -> Handle {
        self.stats.frames_left += 1;
        self.stats.escapes += 1;
        let ri = result.index() as usize;
        if ri < frame.0 {
            // Pass-through: result already belongs to the caller's scope, so its
            // handle stays valid after we pop ours.
            self.table.truncate(frame.0);
            result
        } else {
            // Escape: move the in-scope result's object down to the frame
            // boundary and hand back a fresh handle for it in the caller's scope.
            debug_assert!(ri < self.table.len(), "result out of range");
            let w = self.table[ri];
            self.table.truncate(frame.0);
            self.table.push(w);
            self.note_peak();
            self.occupy(frame.0, w)
        }
    }

    /// Point handle slot `dst` at whatever `src` names — reusing `dst`'s slot, no
    /// allocation. The loop-variable assignment `x = y`.
    #[inline]
    pub fn set(&mut self, dst: Handle, src: Handle) {
        self.stats.slot_reuses += 1;
        let s = self.resolve(src);
        let d = self.resolve(dst);
        self.table[d] = self.table[s];
    }

    /// Step handle slot `dst` to pointer field `field` of `obj` — reusing `dst`'s
    /// slot, no allocation. This is `node = node.next`: one slot walks an entire
    /// chain, so traversal is O(1) in handles regardless of length. (`dst` and
    /// `obj` may be the same handle.)
    #[inline]
    pub fn step_ptr(&mut self, dst: Handle, obj: Handle, field: u32) {
        debug_assert!(
            (field as usize) < self.n_pointers(obj),
            "pointer field out of range"
        );
        self.stats.slot_reuses += 1;
        let w = unsafe { *self.obj_ptr(obj).add(1 + field as usize) };
        let d = self.resolve(dst);
        self.table[d] = Word::from_raw(w);
    }

    /// Current depth of the handle stack (number of live handles). For tests and
    /// diagnostics: under the scoped model this is the precise live-root count.
    #[inline]
    pub fn handle_stack_depth(&self) -> usize {
        self.table.len()
    }

    /// Run a minor collection treating *every live table entry* as a root.
    ///
    /// This is the "explicit-liveness" model: a handle stays a root until the
    /// program [`free`](Heap::free)s it. Useful for testing the moving collector
    /// and for runtimes that track handle lifetimes precisely. Codegen instead
    /// uses [`collect_conservative`](Heap::collect_conservative), where the stack
    /// itself decides liveness.
    pub fn collect(&mut self) -> CollectResult {
        // Disjoint borrow: the collector mutates `inner`; the root closure
        // mutates `table`. Destructuring lets both borrows coexist.
        let result = {
            let Heap { inner, table, .. } = self;
            inner.collect_minor(|evac| {
                for w in table.iter_mut() {
                    evac.visit(w);
                }
            })
        };
        self.record_collection(&result);
        result
    }

    /// Run NewGC's trigger-aware policy with the live handle table as the
    /// precise root set. This keeps a destination-page reserve for evacuation
    /// instead of waiting until G0 has consumed the entire reservation.
    pub fn collect_auto(&mut self) -> CollectResult {
        let result = {
            let Heap { inner, table, .. } = self;
            inner.collect_auto(|evac| {
                for w in table.iter_mut() {
                    evac.visit(w);
                }
            })
        };
        self.record_collection(&result);
        result
    }

    /// Fold a finished collection's evac results (and any cascade) into the stats.
    fn record_collection(&mut self, r: &CollectResult) {
        self.stats.collections += 1;
        self.stats.objects_copied += r.evac.objects_copied as u64;
        self.stats.cells_copied += r.evac.cells_copied as u64;
        self.stats.pages_freed += r.evac.pages_freed as u64;
        if let Some(c) = &r.cascade {
            self.stats.objects_copied += c.objects_copied as u64;
            self.stats.cells_copied += c.cells_copied as u64;
            self.stats.pages_freed += c.pages_freed as u64;
        }
    }

    /// Run a minor collection using a **conservative scan of `stack`** as the
    /// root source — the GC-oblivious model the Locus compiler targets.
    ///
    /// `stack` is a snapshot of the machine stack (and spilled registers) at a
    /// safe point. Each word is decoded as a handle and kept as a root only if it
    /// is **well-formed and live** ([`try_resolve`](Heap::try_resolve)). Thanks to
    /// the 16-bit magic, this is now *precise*, not merely sound: an ordinary int
    /// or a pointer can't carry the magic, so it isn't mistaken for a root. (Even
    /// a fluke collision would only *retain* an object for one cycle — never
    /// corrupt one — because a handle is a stable index, never a raw address the
    /// collector rewrites.)
    ///
    /// Handles the scan does not find are unreachable from the program, so their
    /// slots are recycled; the objects they named are reclaimed unless still
    /// reached through a surviving object's pointer field.
    pub fn collect_conservative(&mut self, stack: &[u64]) -> CollectResult {
        // 1. Which live slots does the stack reference? (Decode + validate.)
        let mut live = std::collections::HashSet::new();
        for &word in stack {
            if let Some(i) = self.try_resolve(Handle::from_bits(word as i64)) {
                live.insert(i as u32);
            }
        }
        let n = self.table.len();

        // 2. Collect, visiting only the stack-found handles as roots.
        let result = {
            let inner = &mut self.inner;
            let table = &mut self.table;
            inner.collect_minor(|evac| {
                for &h in &live {
                    evac.visit(&mut table[h as usize]);
                }
            })
        };

        // 3. Retire handles the stack no longer holds. Their slots return to the
        //    free list; their objects were reclaimed in step 2 unless tracing
        //    from a live root kept them alive (then the object lives on, reached
        //    through a pointer field, and only the dead handle is recycled).
        for i in 0..n as u32 {
            if !live.contains(&i) && self.table[i as usize].raw() != TOMBSTONE {
                self.table[i as usize] = Word::from_raw(TOMBSTONE);
                self.free.push(i);
            }
        }

        self.record_collection(&result);
        result
    }

    /// Is `h` a well-formed handle naming a currently-live object? The runtime
    /// uses this to tell a returned handle from a returned scalar (the latter
    /// lacks the magic), and it's the precise "is this a live reference?" test.
    #[inline]
    pub fn is_live_handle(&self, h: Handle) -> bool {
        self.try_resolve(h).is_some()
    }

    /// Number of live (non-tombstoned) handles. For tests/diagnostics.
    pub fn live_handles(&self) -> usize {
        self.table.len() - self.free.len()
    }

    /// Locus's own handle/root-view statistics (see [`Stats`]). Cheap copy.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// NewGC's physical statistics (reserved/committed bytes, generation
    /// occupancy, trigger policy). The byte/page view that pairs with
    /// [`stats`](Heap::stats)' handle/root view.
    pub fn gc_stats(&self) -> newgc_core::GcStats {
        self.inner.stats()
    }

    /// Reset the Locus stats counters to zero (e.g. to measure a phase in
    /// isolation). Does not touch the heap or NewGC's own counters.
    pub fn reset_stats(&mut self) {
        self.stats = Stats::default();
    }

    /// Total pages committed across all generations. For memory-bound assertions.
    pub fn committed_pages(&self) -> usize {
        use Generation::*;
        [G0, G1, Tenured]
            .iter()
            .map(|&g| self.inner.count_pages_in_gen(g))
            .sum()
    }

    /// Pages committed in a single generation. For diagnostics.
    pub fn pages_in(&self, g: Generation) -> usize {
        self.inner.count_pages_in_gen(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_zero_is_immediate() {
        assert!(matches!(LocusLayout::classify(0), WordKind::Immediate));
        assert!(matches!(
            LocusLayout::classify(LocusLayout::FILL_WORD),
            WordKind::Immediate
        ));
    }

    #[test]
    fn classify_header_pointer() {
        let dummy: u64 = 0;
        let p = &dummy as *const u64 as *const u8;
        let raw = header_ptr(p);
        match LocusLayout::classify(raw) {
            WordKind::PointerHeader(addr) => assert_eq!(addr, p),
            other => panic!("expected PointerHeader, got {other:?}"),
        }
    }

    #[test]
    fn classify_forwarding_marker() {
        let dummy: u64 = 0;
        let p = (&dummy as *const u64 as *const u8).wrapping_offset(8);
        let raw = LocusLayout::make_forward(p);
        match LocusLayout::classify(raw) {
            WordKind::Forwarded(addr) => assert_eq!(addr, p),
            other => panic!("expected Forwarded, got {other:?}"),
        }
    }

    #[test]
    fn make_pointer_is_header_kind() {
        let dummy: u64 = 0;
        let p = &dummy as *const u64 as *const u8;
        // Both kinds collapse to a header pointer (Locus has no cons).
        for kind in [PointerKind::Cons, PointerKind::Header] {
            let raw = LocusLayout::make_pointer(p, kind);
            match LocusLayout::classify(raw) {
                WordKind::PointerHeader(addr) => assert_eq!(addr, p),
                other => panic!("expected PointerHeader, got {other:?}"),
            }
        }
    }

    #[test]
    fn rewrite_preserves_tag() {
        let old_addr: u64 = 0x1000;
        let new_addr: u64 = 0x2000;
        for &tag in &[TAG_HEADER, TAG_FORWARD] {
            let old_raw = old_addr | tag;
            let new_raw = LocusLayout::rewrite_pointer_addr(old_raw, new_addr as *const u8);
            assert_eq!(new_raw & TAG_MASK, tag, "tag preserved");
            assert_eq!(new_raw & PAYLOAD_MASK, new_addr, "address rewritten");
        }
    }

    #[test]
    fn header_encodes_pointer_and_scalar_counts() {
        // A record { next: ptr, n: scalar }: 1 pointer, 1 scalar.
        let h = header(1, 1);
        assert_eq!(header_n_pointers(h), 1);
        assert_eq!(header_payload_len(h), 2);
    }

    #[test]
    fn header_layout_mixed_object() {
        // 2 pointers, 3 scalars: total payload 5, traced range [1, 3).
        let h = header(2, 3);
        let cell = &h as *const u64;
        let layout = unsafe { LocusLayout::header_layout(cell) };
        assert_eq!(layout.total_cells, 6, "header + 5 payload");
        assert_eq!(layout.pointer_cells_start, 1);
        assert_eq!(layout.pointer_cells_end, 3, "cells 1,2 are pointers");
        assert_eq!(layout.pointer_cell_count(), 2);
    }

    #[test]
    fn header_layout_all_pointers() {
        // A 3-tuple of heap objects: 3 pointers, 0 scalars.
        let h = header(3, 0);
        let cell = &h as *const u64;
        let layout = unsafe { LocusLayout::header_layout(cell) };
        assert_eq!(layout.total_cells, 4);
        assert_eq!(layout.pointer_cells_end, 4, "all payload cells traced");
    }

    #[test]
    fn header_layout_all_scalars() {
        // A pair of plain ints (i64, i64): 0 pointers, 2 scalars.
        let h = header(0, 2);
        let cell = &h as *const u64;
        let layout = unsafe { LocusLayout::header_layout(cell) };
        assert_eq!(layout.total_cells, 3);
        assert_eq!(layout.pointer_cells_start, 1);
        assert_eq!(layout.pointer_cells_end, 1, "empty traced range");
        assert_eq!(layout.pointer_cell_count(), 0);
    }

    #[test]
    fn scalar_bit_pattern_never_misread() {
        // A scalar i64 whose low bits collide with the header tag is fine: the
        // collector never classifies scalar cells. We just assert the layout
        // keeps such a value OUTSIDE the traced range.
        let h = header(1, 1); // 1 ptr, 1 scalar
        let cell = &h as *const u64;
        let layout = unsafe { LocusLayout::header_layout(cell) };
        // The scalar lives at cell index 2, which is >= pointer_cells_end (2).
        assert!(2 >= layout.pointer_cells_end, "scalar cell is untraced");
    }

    #[test]
    fn scalar_array_payload_start_is_16_byte_aligned() {
        let mut h = Heap::new(8 * 64 * 1024);
        let _odd_sized_object = h.alloc(0, 2);
        let arr = h.alloc(0, 3); // length slot + at least two scalar data cells.
        let data = unsafe { h.obj_ptr(arr).add(2) } as usize;
        assert_eq!(data % 16, 0, "scalar array payload must be 16-byte aligned");
    }

    #[test]
    fn scalar_array_payload_cells_are_not_traced() {
        let mut h = Heap::new(8 * 64 * 1024);
        let arr = h.alloc(0, 2);
        h.set_scalar(arr, 0, 1);
        let target = h.alloc(0, 1);
        let target_word = h.table[h.resolve(target)].raw();
        h.set_scalar(arr, 1, target_word as i64);
        h.free(target);

        h.reset_stats();
        h.collect();

        assert_eq!(
            h.stats().objects_copied,
            1,
            "only the array should survive; scalar payload bytes are not roots"
        );
    }
}
