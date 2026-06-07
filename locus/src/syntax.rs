//! The Locus core syntax: **effect rows**, **value types**, and **terms**.
//!
//! A direct transcription of `calculus.md` §1.1 (the effect grade) and
//! §2.1 (the monad fragment). Staging — `Code[T]` / `quote` / `splice` —
//! is a later slice, so there is no `□` here yet.

use std::collections::BTreeSet;
use std::sync::Arc;

/// A **type** unification variable's identity (`polymorphism-impl.md`, the
/// stores). Monotonic and never reused; the unification store
/// ([`crate::unify::UnifStore`]) maps it to an `Unbound{level}`/`Bound(Type)`
/// cell. Present in [`Type::Var`]; **zonked away** before IR/stage (D6), so it
/// never reaches a later phase.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TyVarId(pub u32);

/// A **row** unification variable's identity — the open tail `ρ` of a [`Row`]
/// (D1). Like [`TyVarId`]: monotonic, never reused, resolved through the store,
/// zonked to the closed empty row if still unbound at the end (D6).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RowVarId(pub u32);

impl RowVarId {
    const PARSED_BASE: u32 = 0x8000_0000;

    /// Parser-only placeholder for annotation syntax such as `{gc | r}`.
    /// Sema rewrites these to real store-allocated row vars before unification.
    pub(crate) fn parsed(index: u32) -> RowVarId {
        RowVarId(Self::PARSED_BASE + index)
    }

    pub(crate) fn parsed_index(self) -> Option<u32> {
        (self.0 >= Self::PARSED_BASE).then_some(self.0 - Self::PARSED_BASE)
    }
}

/// An effect label (`calculus.md` §1.1).
///
/// We start with **object** effects only; the object/generative (`O`/`G`)
/// split (§3.1) and the `Insert` label arrive with the staging slice.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Label {
    /// `exn[X]` — may abort with an `X`. (`raise` ≡ `perform Throw[X]`.)
    Exn(String),
    /// A fine-grained world / IO label: `console`, `fs`, `net`, `clock`, …
    World(String),
    /// `gc` — touches the managed heap (allocates).
    Gc,
    /// `st` — **observable mutation** of a first-class `Ref[T]` cell
    /// (`docs/mutability.md` §2; `calculus.md` §1.1). A read (`!r`) or write
    /// (`r := v`) of a `Ref` carries `st`: the cell is a value, potentially
    /// observable, so its mutation is an effect in the row. `ref` (allocation)
    /// carries `{gc}`, not `st`.
    ///
    /// **v1 is a single `st` label**, *not* the calculus's parameterized `st[T]`.
    /// A `Label` lives in a `BTreeSet` (the set-row, D4), which needs `Label: Ord`;
    /// parameterizing by [`Type`] would force `Type: Ord` (a large, invasive change
    /// — `Type` carries unification-var ids and is only `Eq`). The sprint plan and
    /// `mutability.md` O-M3 both endorse a single `st` as the simpler *sound* v1: it
    /// is a safe upper bound (a handler still sees "this computation mutates a Ref"),
    /// just coarser than per-type. Parameterized `st[T]` is a later refinement.
    St,
    /// A user-declared effect operation's label (`effect Foo: op(…)`).
    User(String),

    /// `Insert` — the built-in **generative** effect (let-insertion). `δ`
    /// distributes it *out* of `□` (calculus §3.1); every other label is an
    /// **object** effect that stays inside.
    Insert,
}

impl Label {
    /// Is this a **generative** (`G`) label (§3.1)? Generative effects fire
    /// at generation and distribute out of `□`. Built-in: `Insert`;
    /// everything else is an **object** (`O`) effect.
    pub fn is_generative(&self) -> bool {
        matches!(self, Label::Insert)
    }

    /// Is this a **native** effect — one the runtime supplies a *prelowered*
    /// default handler (a JIT-callable Rust function) for? The `World` IO
    /// surface and `gc`. A residual native effect lowers to that runtime call;
    /// a residual `User` effect, by contrast, is genuinely unhandled.
    pub fn is_native(&self) -> bool {
        matches!(self, Label::World(_) | Label::Gc)
    }
}

/// A runtime **layer** ([`capabilities.md`]): the fixed privilege lattice
/// `boundary < services < app`. A lower [`rank`](Layer::rank) is *more*
/// privileged and grafts *outermost* (closest to the world). The **default is
/// `app`** — the most-confined layer — so forgetting the annotation can never
/// grant privilege.
///
/// - **`boundary`** — the only layer that **mints** raw capabilities (`extern` /
///   `extern asm` / foreign-bind): `winapi`, `crt`, `gc!`, `asm`. Manifest-gated.
/// - **`services`** — *seal* the raw powers and export abstract effects
///   (`console`/`fsro`/`fsrw`/`net` over `winapi`, `alloc` over `gc!`, …).
/// - **`app`** — open user space. Users layer among themselves via per-module
///   imports + `seals`, not via more enum tiers: a sealed label is gone from a
///   module's exports regardless of layer, so user-land structure needs no new
///   privilege levels.
///
/// Minting is `boundary`-only; **sealing is available at every layer** — that
/// split is what lets a user module create a feature and seal it above the
/// services (`sealing-plan.md` S2/S4).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Layer {
    Boundary,
    Services,
    App,
}

impl Layer {
    /// Privilege rank: `boundary` = 0 (most privileged) … `app` = 2 (most
    /// confined). Lower rank grafts further out.
    pub fn rank(self) -> u8 {
        match self {
            Layer::Boundary => 0,
            Layer::Services => 1,
            Layer::App => 2,
        }
    }

    /// Is this the mint-capable boundary layer? Only `boundary` modules may
    /// `extern` / `extern asm` / foreign-bind (the mint-gate, S2).
    pub fn can_mint(self) -> bool {
        matches!(self, Layer::Boundary)
    }

    /// The surface name (`at boundary`, …).
    pub fn name(self) -> &'static str {
        match self {
            Layer::Boundary => "boundary",
            Layer::Services => "services",
            Layer::App => "app",
        }
    }

    /// Parse a layer name written after `at`. `None` for an unknown name.
    pub fn from_name(s: &str) -> Option<Layer> {
        match s {
            "boundary" => Some(Layer::Boundary),
            "services" => Some(Layer::Services),
            "app" => Some(Layer::App),
            _ => None,
        }
    }
}

/// An effect **row** `E` (`calculus.md` §1.1): a monoid under union, with
/// unit the empty row `∅` = **pure**.
///
/// Slice 1 uses simple **set-rows** (dedup, unordered). Scoped rows with
/// significant order are OBLIGATION 1.1.a — deferred, by *simple clarity*.
///
/// **Row polymorphism (D1).** A row carries zero or more open **tails**
/// `ρ` ([`RowVarId`]): an empty tail set is a *closed* row (today's behaviour,
/// byte-for-byte), a non-empty tail set is an *open* row `{labels | ρ...}` that
/// can still absorb further labels through unification. Multiple tails are the
/// smallest step beyond S4's single-tail rows: `union` can now preserve
/// independent callback rows such as `ρ_f ∪ ρ_g` for `compose`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Row {
    labels: BTreeSet<Label>,
    /// The open tails `ρ`, or empty for a closed row. **Closed reproduces today
    /// exactly** (every row the monomorphic checker builds is closed).
    tails: Arc<BTreeSet<RowVarId>>,
}

impl Row {
    /// The empty row `∅` — **pure** (default-pure, §1.1). Closed.
    pub fn pure() -> Row {
        Row {
            labels: BTreeSet::new(),
            tails: Arc::new(BTreeSet::new()),
        }
    }

    /// A singleton row `{ℓ}`. Closed.
    pub fn single(l: Label) -> Row {
        Row {
            labels: BTreeSet::from([l]),
            tails: Arc::new(BTreeSet::new()),
        }
    }

    /// An **open** row `{labels | ρ}` from a label set and a tail variable.
    /// Constructed only by row unification (case D's fresh `ρ`, and the
    /// binding of a tail); the monomorphic checker never calls this.
    pub fn open(labels: BTreeSet<Label>, tail: RowVarId) -> Row {
        Row {
            labels,
            tails: Arc::new(BTreeSet::from([tail])),
        }
    }

    /// A row from a label set with an explicit (possibly-`None`) tail — the
    /// general constructor unification uses when it has computed both halves.
    pub fn with_tail(labels: BTreeSet<Label>, tail: Option<RowVarId>) -> Row {
        Row {
            labels,
            tails: Arc::new(tail.into_iter().collect()),
        }
    }

    /// A row from a label set with an explicit tail set. This is the multi-tail
    /// constructor used by row union and multi-row normalization.
    pub fn with_tails(labels: BTreeSet<Label>, tails: BTreeSet<RowVarId>) -> Row {
        Row {
            labels,
            tails: Arc::new(tails),
        }
    }

    /// Row union `∪` — the monoid operation (`E₁ ∪ E₂`). Composes effects that
    /// *genuinely happen* (App, Let, …). The label sets merge; an open tail on
    /// either side carries through (a closed row stays closed). **For two closed
    /// rows this is byte-for-byte today's union.**
    ///
    /// Note: `union` is *not* `unify_row` — it accumulates effects, it does not
    /// equate two rows the type demands be equal (`polymorphism-impl.md`,
    /// "Discipline: `union` vs `unify_row`"). Multi-tail rows are exactly the
    /// effect-accumulation case: if `f` performs `ρ_f` and its argument performs
    /// `ρ_g`, the application row is `{| ρ_f, ρ_g}` until constraints solve them.
    pub fn union(&self, other: &Row) -> Row {
        Row {
            labels: self.labels.union(&other.labels).cloned().collect(),
            tails: Arc::new(self.tails.union(&other.tails).cloned().collect()),
        }
    }

    /// Is this the pure (empty) row? A `pure` proc *provably* performs
    /// nothing observable. **D1: an open empty row `{ | ρ }` is NOT pure** — it
    /// can still absorb labels — so this is empty labels and no tails.
    pub fn is_pure(&self) -> bool {
        self.labels.is_empty() && self.tails.is_empty()
    }

    /// The labels in this row (the closed part; the tail is separate).
    pub fn labels(&self) -> impl Iterator<Item = &Label> {
        self.labels.iter()
    }

    /// The label set (borrowed) — for unification's set algebra.
    pub(crate) fn label_set(&self) -> &BTreeSet<Label> {
        &self.labels
    }

    /// The open tail set.
    pub(crate) fn tail_set(&self) -> &BTreeSet<RowVarId> {
        &self.tails
    }

    /// Discharge labels: `E \ ls` — what a handler removes from a row when
    /// it handles those operations ("effects shrink", §2.1 (op)). The tail is
    /// **preserved** (an open row stays open after discharging concrete labels).
    pub fn without(&self, ls: &[Label]) -> Row {
        Row {
            labels: self
                .labels
                .iter()
                .filter(|l| !ls.contains(l))
                .cloned()
                .collect(),
            tails: Arc::clone(&self.tails),
        }
    }

    /// Split by kind (§3.1): `(object part, generative part)`. This is the
    /// partition `δ` applies at a `quote` boundary — object effects stay in
    /// the `□`, generative effects come out. The **tail rides with the object
    /// part** (an open row's residual is object effects that stay in the `□`);
    /// the generative part is always closed.
    pub fn partition(&self) -> (Row, Row) {
        let mut obj = BTreeSet::new();
        let mut gen = BTreeSet::new();
        for l in &self.labels {
            if l.is_generative() {
                gen.insert(l.clone());
            } else {
                obj.insert(l.clone());
            }
        }
        (
            Row {
                labels: obj,
                tails: Arc::clone(&self.tails),
            },
            Row {
                labels: gen,
                tails: Arc::new(BTreeSet::new()),
            },
        )
    }
}

/// Storage metadata for one typed value when it appears in a managed object
/// field, closure capture, or future typed array element.
///
/// Today's runtime stores every value in one machine cell: managed values are
/// traced pointer cells; immediates and native words are opaque scalar cells.
/// The descriptor is wider than today's representation so floats, SIMD values,
/// and packed arrays can add byte-level layout without teaching every lowering
/// path a new ad hoc predicate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValueLayout {
    pub pointer_cells: usize,
    pub scalar_cells: usize,
    pub byte_width: usize,
    pub align: usize,
    pub known: bool,
    /// A **word cell** (repr-poly tags, `docs/repr-poly-impl.md` D4): the single
    /// cell is laid in the collector's **traced** range (so it is counted in
    /// `pointer_cells` and `classify` runs on it), but it holds a **raw word** —
    /// either a real interior pointer (`addr|10`, followed) or a tag-room scalar
    /// (`value<<2`, low bits `00`, skipped). It is therefore *stored verbatim*
    /// (a raw 64-bit word store, neither `set_ptr` — which resolves a handle and
    /// faults on a tagged scalar — nor `set_scalar`, which is untraced). This is
    /// the representation of a `Type::Var` field. `false` for every other layout.
    pub word: bool,
}

impl ValueLayout {
    pub const fn pointer_cell() -> ValueLayout {
        ValueLayout {
            pointer_cells: 1,
            scalar_cells: 0,
            byte_width: 8,
            align: 8,
            known: true,
            word: false,
        }
    }

    /// A **word cell** — one traced cell holding a raw repr-poly word (D4). It is
    /// *gc-reachable / classified* (counted in `pointer_cells`) yet **stored
    /// verbatim**, distinguished from [`pointer_cell`](ValueLayout::pointer_cell)
    /// only by the `word` flag the store path keys on. This is the layout of a
    /// `Type::Var` field (`storage_layout(Type::Var)`).
    pub const fn word_cell() -> ValueLayout {
        ValueLayout {
            pointer_cells: 1,
            scalar_cells: 0,
            byte_width: 8,
            align: 8,
            known: true,
            word: true,
        }
    }

    pub const fn scalar_cell() -> ValueLayout {
        ValueLayout {
            pointer_cells: 0,
            scalar_cells: 1,
            byte_width: 8,
            align: 8,
            known: true,
            word: false,
        }
    }

    pub const fn scalar_bytes(byte_width: usize, align: usize) -> ValueLayout {
        ValueLayout {
            pointer_cells: 0,
            scalar_cells: (byte_width + 7) / 8,
            byte_width,
            align,
            known: true,
            word: false,
        }
    }

    pub const fn unknown_scalar_cell() -> ValueLayout {
        ValueLayout {
            pointer_cells: 0,
            scalar_cells: 1,
            byte_width: 8,
            align: 8,
            known: false,
            word: false,
        }
    }

    pub fn total_cells(self) -> usize {
        self.pointer_cells + self.scalar_cells
    }

    pub fn is_scalar_only(self) -> bool {
        self.known && self.pointer_cells == 0
    }

    pub fn is_gc_reachable(self) -> bool {
        self.known && self.pointer_cells > 0
    }

    pub fn is_single_pointer_cell(self) -> bool {
        self.known && self.pointer_cells == 1 && self.scalar_cells == 0
    }

    pub fn is_single_scalar_cell(self) -> bool {
        self.known && self.pointer_cells == 0 && self.scalar_cells == 1
    }

    /// Is this the single **word cell** of a `Type::Var` field (D4)? A traced cell
    /// (so it is `is_single_pointer_cell` *and* `is_gc_reachable` — it lives in
    /// the pointer region and `classify` runs on it) whose contents are a raw
    /// repr-poly word, so the store/load path must use the **verbatim** primitive
    /// (`set_word`/`get_word`) rather than `set_ptr`/`set_scalar`.
    pub fn is_word_cell(self) -> bool {
        self.known && self.word && self.pointer_cells == 1 && self.scalar_cells == 0
    }

    pub fn aggregate(fields: impl IntoIterator<Item = ValueLayout>) -> ValueLayout {
        let mut out = ValueLayout {
            pointer_cells: 0,
            scalar_cells: 0,
            byte_width: 0,
            align: 1,
            known: true,
            // An aggregate is never itself a single word cell; the per-field word
            // flag is what each field carries through (a `Var` field keeps its own
            // `word_cell` layout). The aggregate descriptor only sums region sizes.
            word: false,
        };
        for field in fields {
            out.pointer_cells += field.pointer_cells;
            out.scalar_cells += field.scalar_cells;
            out.byte_width += field.byte_width;
            out.align = out.align.max(field.align);
            out.known &= field.known;
        }
        out
    }
}

/// Value types `A` (`calculus.md` §1–§2). No `Code[T]` yet (staging slice).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Type {
    /// A **type unification variable** ([`TyVarId`]) — an unsolved type the
    /// checker will pin by [`crate::unify::unify`]. Resolved through the store
    /// and **zonked away before IR/stage** (D6): a `Var` must never reach
    /// `ir.rs`/`stage.rs`. The monomorphic checker (S1) never *creates* one — it
    /// exists so unification and the oracle are well-typed and so S2 can layer
    /// schemes on without re-touching the representation.
    Var(TyVarId),
    Int,
    /// `Float` - an IEEE-754 binary64 value. Scalar and untraced: it can live
    /// in one-cell value slots, but arithmetic semantics are distinct and must
    /// never be lowered as integer math.
    Float,
    /// `Float32` - an IEEE-754 binary32 value. Surface type support lands ahead
    /// of full scalar lowering so arrays/SIMD can build on the representation
    /// plan without pretending `Float32` is executable everywhere yet.
    Float32,
    /// `Pair[T]`, `Quad[T]`, `Oct[T]` - fixed-lane SIMD values. The first SIMD
    /// slice permits `T` to be `Float32` or `Float`; vectors are unboxed scalar
    /// values, not GC references.
    Vector(VectorShape, Box<Type>),
    /// `Mask[Pair]`, `Mask[Quad]`, `Mask[Oct]` - a fixed-lane SIMD predicate
    /// value. Masks are local vector values (`<N x i1>` in LLVM), not GC refs.
    Mask(VectorShape),
    Bool,
    Unit,
    /// `String` — immutable text (`"…"`). A native value type whose
    /// representation the runtime owns; the front end only tracks that it *is*
    /// a string (and the effects of producing it).
    Str,
    /// `A -> B ! E` — a function carrying a **latent** row `E`: the effects
    /// performed when it is *applied*. The row rides on the arrow.
    Fun(Box<Type>, Box<Type>, Row),

    /// `□(A ! E_O)` — **code** of a computation `A ! E_O`: when run (at the
    /// next-lower stage) it yields an `A`, performing the **object** effects
    /// `E_O`. Built by `quote`, consumed by `splice`. Object effects stay
    /// *inside* the `□`; generative ones do not (calculus §3).
    Code(Box<Type>, Row),

    /// `(A, B, …)` — a **tuple** (product). A heap struct of element values;
    /// at runtime a pointer in the uniform `i64` model. Destructured by
    /// `let (x, …) = e`. Two or more elements (`()` is `Unit`, `(A)` is `A`).
    Tuple(Vec<Type>),

    /// `{ x: A, y: B, … }` — a **record** (a product with *named* fields). Fields
    /// are kept **sorted by name** (so field order is irrelevant to the type), and
    /// at runtime it is a tuple of the sorted field values; `r.x` is a projection.
    Record(Vec<(String, Type)>),

    /// `Array[T]` — a **homogeneous, variable-length, mutable** sequence: the
    /// first genuinely *dynamic* heap object, and what the collector exists for.
    /// A managed-heap allocation. Pointer arrays store traced pointer slots;
    /// scalar arrays store a contiguous untraced byte payload with a typed
    /// element stride. Built with `[e1, …, en]`, measured with `len`,
    /// read/written (bounds-checked) with `a[i]` / `a[i] <- v`.
    Array(Box<Type>),

    /// A **nominal** reference to a `type`-declared sum (e.g. `List`, `Option`),
    /// **with its type arguments** (D8). `Named("List", [Int])` is `List[Int]`;
    /// `Named("Color", [])` is every existing *monomorphic* sum — empty args
    /// reproduce the pre-S3 representation byte-for-byte (the S3 differential
    /// guarantee). Unlike the structural types above, the name carries no
    /// variants here; those live in the type environment, which is what lets a
    /// sum be **recursive** (`type List[a] = Nil | Cons(a, List[a])`). At runtime
    /// a handle to a tagged GC object (scalar field 0 is the constructor tag, the
    /// rest is its payload). The type argument never affects the runtime layout
    /// (`List[Int]` and `List[Bool]` are byte-identical), so it is purely a
    /// type-checking artifact — zonked through like any other.
    Named(String, Vec<Type>),

    /// Native FFI integer / pointer widths — written **only** in an `extern`
    /// signature, where they pin the boundary representation. Sema erases them
    /// to `Int` for the value world (uniform `i64`) and records the widths as an
    /// [`ExternAbi`]; only the foreign call converts (trunc / sext / zext).
    /// `I32` = `int`/`BOOL`, `U32` = `DWORD`/`UINT`, `Ptr` = `HANDLE`/`LP…`.
    I32,
    U32,
    Ptr,
}

/// Fixed SIMD lane counts exposed in the surface type language.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VectorShape {
    Pair,
    Quad,
    Oct,
}

impl VectorShape {
    pub const fn lanes(self) -> usize {
        match self {
            VectorShape::Pair => 2,
            VectorShape::Quad => 4,
            VectorShape::Oct => 8,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            VectorShape::Pair => "Pair",
            VectorShape::Quad => "Quad",
            VectorShape::Oct => "Oct",
        }
    }

    pub const fn plural_name(self) -> &'static str {
        match self {
            VectorShape::Pair => "Pairs",
            VectorShape::Quad => "Quads",
            VectorShape::Oct => "Octs",
        }
    }

    pub fn from_name(name: &str) -> Option<VectorShape> {
        match name {
            "Pair" => Some(VectorShape::Pair),
            "Quad" => Some(VectorShape::Quad),
            "Oct" => Some(VectorShape::Oct),
            _ => None,
        }
    }

    pub fn from_plural_name(name: &str) -> Option<VectorShape> {
        match name {
            "Pairs" => Some(VectorShape::Pair),
            "Quads" => Some(VectorShape::Quad),
            "Octs" => Some(VectorShape::Oct),
            _ => None,
        }
    }

    pub fn from_splat_name(name: &str) -> Option<VectorShape> {
        match name {
            "splatPair" => Some(VectorShape::Pair),
            "splatQuad" => Some(VectorShape::Quad),
            "splatOct" => Some(VectorShape::Oct),
            _ => None,
        }
    }

    /// `loadPair`/`loadQuad`/`loadOct` — the array vector-load intrinsic names.
    pub fn from_load_name(name: &str) -> Option<VectorShape> {
        match name {
            "loadPair" => Some(VectorShape::Pair),
            "loadQuad" => Some(VectorShape::Quad),
            "loadOct" => Some(VectorShape::Oct),
            _ => None,
        }
    }

    /// `storePair`/`storeQuad`/`storeOct` — the array vector-store intrinsic names.
    pub fn from_store_name(name: &str) -> Option<VectorShape> {
        match name {
            "storePair" => Some(VectorShape::Pair),
            "storeQuad" => Some(VectorShape::Quad),
            "storeOct" => Some(VectorShape::Oct),
            _ => None,
        }
    }

    pub fn lane_index(self, field: &str) -> Option<usize> {
        let idx = match field {
            "x" => 0,
            "y" => 1,
            "z" => 2,
            "w" => 3,
            "lane0" => 0,
            "lane1" => 1,
            "lane2" => 2,
            "lane3" => 3,
            "lane4" => 4,
            "lane5" => 5,
            "lane6" => 6,
            "lane7" => 7,
            _ => return None,
        };
        (idx < self.lanes()).then_some(idx)
    }
}

/// A primitive binary operator on `Int`s. Arithmetic (`+ - * / %` and their
/// explicit wrapping/checked spellings), bitwise (`& | ^ << >>`) and shifts all
/// yield `Int`; comparison (`== != < <= > >=`) yields `Bool`. These are kernel primitives
/// — you cannot define `+` or `&` from nothing — kept deliberately few. Bitwise
/// ops act on the uniform `i64` value; `>>` is arithmetic (sign-preserving),
/// matching signed-int `>>` elsewhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    /// Bare arithmetic is the ratified v1 default: wrapping and pure.
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// Explicit wrapping arithmetic, also pure.
    AddWrap,
    SubWrap,
    MulWrap,
    /// Checked arithmetic; overflow carries `exn[Overflow]`.
    AddChecked,
    SubChecked,
    MulChecked,
    /// `&` — bitwise AND.
    And,
    /// `|` — bitwise OR.
    Or,
    /// `^` — bitwise XOR.
    Xor,
    /// `<<` — left shift.
    Shl,
    /// `>>` — arithmetic right shift (sign-preserving).
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    /// Does this operator produce a `Bool` (rather than an `Int`)?
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }

    /// Does this operator carry `exn[Overflow]`?
    pub fn is_checked_overflow(self) -> bool {
        matches!(
            self,
            BinOp::AddChecked | BinOp::SubChecked | BinOp::MulChecked
        )
    }

    /// The surface symbol.
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::AddWrap => "+%",
            BinOp::SubWrap => "-%",
            BinOp::MulWrap => "*%",
            BinOp::AddChecked => "+?",
            BinOp::SubChecked => "-?",
            BinOp::MulChecked => "*?",
            BinOp::And => "&",
            BinOp::Or => "|",
            BinOp::Xor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
        }
    }
}

/// An explicit numeric conversion. Locus does not implicitly promote between
/// integer and floating types; source code must name the conversion it wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CastOp {
    /// `toFloat x` - signed `Int` to binary64 `Float`.
    ToFloat,
    /// `floor x` - `Float` to `Int`, rounding toward negative infinity.
    Floor,
    /// `round x` - `Float` to `Int`, rounding to nearest.
    Round,
    /// `toFloat32 x` - narrow binary64 `Float` to binary32 `Float32`.
    ToFloat32,
    /// `fromFloat32 x` - widen binary32 `Float32` to binary64 `Float`.
    FromFloat32,
}

impl CastOp {
    pub fn symbol(self) -> &'static str {
        match self {
            CastOp::ToFloat => "toFloat",
            CastOp::Floor => "floor",
            CastOp::Round => "round",
            CastOp::ToFloat32 => "toFloat32",
            CastOp::FromFloat32 => "fromFloat32",
        }
    }
}

/// Explicit floating math operations. These are distinct from implicit
/// fast-math rewrites: source code must name the operation it wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FloatMathOp {
    /// `sqrt x` - square root with the element type preserved.
    Sqrt,
    /// `sum v` - horizontal lane sum of a SIMD vector.
    Sum,
    /// `dot(a, b)` - horizontal sum of elementwise products.
    Dot,
    /// `length v` - Euclidean vector length.
    Length,
    /// `fma(a, b, c)` - fused multiply-add with one final rounding.
    Fma,
}

impl FloatMathOp {
    pub fn symbol(self) -> &'static str {
        match self {
            FloatMathOp::Sqrt => "sqrt",
            FloatMathOp::Sum => "sum",
            FloatMathOp::Dot => "dot",
            FloatMathOp::Length => "length",
            FloatMathOp::Fma => "fma",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaskReduceOp {
    /// `any m` - true when at least one mask lane is true.
    Any,
    /// `all m` - true when every mask lane is true.
    All,
}

impl MaskReduceOp {
    pub fn symbol(self) -> &'static str {
        match self {
            MaskReduceOp::Any => "any",
            MaskReduceOp::All => "all",
        }
    }
}

/// The access width of a raw memory `peek`/`poke`, in bits. A `peek` reads this
/// many bits and **zero-extends** to the uniform `i64`; a `poke` writes the low
/// bits of its `i64` value. (`fill`/`copy` work in bytes and need no width.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemWidth {
    W8,
    W16,
    W32,
    W64,
}

impl MemWidth {
    /// Parse the bit-width suffix of a `peekNN`/`pokeNN` primitive name.
    pub fn from_suffix(s: &str) -> Option<MemWidth> {
        match s {
            "8" => Some(MemWidth::W8),
            "16" => Some(MemWidth::W16),
            "32" => Some(MemWidth::W32),
            "64" => Some(MemWidth::W64),
            _ => None,
        }
    }

    /// The width in bits, e.g. for the surface spelling `peek16`.
    pub fn bits(self) -> u32 {
        match self {
            MemWidth::W8 => 8,
            MemWidth::W16 => 16,
            MemWidth::W32 => 32,
            MemWidth::W64 => 64,
        }
    }

    /// The width in bytes — the element stride a subscript `a[i]` scales by.
    pub fn bytes(self) -> i64 {
        (self.bits() / 8) as i64
    }
}

/// Terms `e` of the effect fragment (`calculus.md` §2.1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Term {
    /// A variable `x`.
    Var(String),
    Int(i64),
    /// A `Float` literal, stored as raw `f64` bits so the AST remains `Eq`.
    Float(u64),
    Bool(bool),
    Unit,
    /// `brk` — a **debug-crash** expression: lowers to a trap (`llvm.trap` → an
    /// illegal-instruction fault) so a deliberate crash can be inserted to
    /// exercise the host crash handler. Diverges, so it types at any expected
    /// type (a fresh variable). Gated: only parses under `--brk-enable`.
    Brk,
    /// A string literal `"…"`.
    Str(String),
    /// `a op b` - a primitive binary op ([`BinOp`]).
    Bin(BinOp, Box<Term>, Box<Term>),
    /// `toFloat x`, `floor x`, ... - explicit numeric conversions.
    Cast(CastOp, Box<Term>),
    /// `sqrt x` - explicit floating square root.
    Sqrt(Box<Term>),
    /// `sum v` - explicit horizontal SIMD lane sum.
    Sum(Box<Term>),
    /// `dot(a, b)` - explicit SIMD dot product.
    Dot(Box<Term>, Box<Term>),
    /// `length v` - explicit SIMD Euclidean length.
    Length(Box<Term>),
    /// `fma(a, b, c)` - explicit fused multiply-add.
    Fma(Box<Term>, Box<Term>, Box<Term>),
    /// `any m` / `all m` - horizontal mask reductions to `Bool`.
    MaskReduce(MaskReduceOp, Box<Term>),
    /// `select(m, a, b)` - lane-wise blend under a SIMD mask.
    Select(Box<Term>, Box<Term>, Box<Term>),
    /// `Pair(...)`, `Quad(...)`, `Oct(...)` - fixed-lane vector construction.
    VectorLit(VectorShape, Vec<Term>),
    /// `splatPair x`, `splatQuad x`, `splatOct x` - duplicate one scalar lane.
    VectorSplat(VectorShape, Box<Term>),
    /// `loadPair(arr, i)` / `loadQuad(arr, i)` / `loadOct(arr, i)` — load
    /// `shape.lanes()` **contiguous** scalar elements of an `Array[E]`, starting
    /// at element index `i`, as one fixed-lane vector (a flat-scalar-buffer SIMD
    /// load — element `a[i]`, `a[i+1]`, … packed into `<lanes x E>`, NOT an
    /// `Array[Quad]` index). The array's element type must equal the vector's
    /// lane element type. `! {gc}` (a managed-array read). SIMD Sprint 2.
    VectorLoad {
        shape: VectorShape,
        arr: Box<Term>,
        idx: Box<Term>,
    },
    /// `storePair(arr, i, v)` / `storeQuad(arr, i, v)` / `storeOct(arr, i, v)` —
    /// the matching store: write vector `v`'s lanes to the `shape.lanes()`
    /// contiguous elements at index `i`. Yields `Unit`. `! {gc}`. SIMD Sprint 2.
    VectorStore {
        shape: VectorShape,
        arr: Box<Term>,
        idx: Box<Term>,
        value: Box<Term>,
    },
    /// `if c then a else b` — `c : Bool`, branches share a type.
    If(Box<Term>, Box<Term>, Box<Term>),
    /// `loop x = init, ... while cond do next_x, ... return result` — a structured
    /// accumulator loop. The `do` expressions compute the next accumulator values;
    /// the `return` expression computes the loop result when `cond` is false.
    Loop {
        vars: Vec<(String, Term)>,
        cond: Box<Term>,
        steps: Vec<Term>,
        result: Box<Term>,
    },
    /// `λx:A. e` (`fn x: A => e`) — or, with the annotation **omitted**
    /// (`fn x => e`), `λx. e`: the parameter type is then a fresh unification
    /// variable inferred from the body and the lambda's uses (rank-1, S2). An
    /// annotated parameter keeps its declared type exactly as before.
    Lam(String, Option<Type>, Box<Term>),
    /// `f a`
    App(Box<Term>, Box<Term>),
    /// `let x = e₁ in e₂`
    Let(String, Box<Term>, Box<Term>),
    /// Internal flattened declaration sequence. Surface syntax still parses to
    /// the classic nested forms; stdlib/module grafting compacts long declaration
    /// spines into this shape so "many declarations" is wide data rather than
    /// native recursion depth.
    Block(Vec<BlockItem>, Box<Term>),
    /// `let rec f : T = e₁ in e₂` — a recursive binding: `f : T` is in scope in
    /// `e₁` (its own definition) as well as `e₂`. The annotation `T` makes the
    /// function's type known before its body is checked. `e₁` is a function.
    LetRec(String, Type, Box<Term>, Box<Term>),

    /// `let mut x = e₁ in e₂` — a **non-escaping scalar mutable local**
    /// (mutability v1; `docs/mutability.md` §3, `docs/mutability-sprints.md`).
    /// `x` is bound *mutable* in `e₂`: it reads at its scalar type and may be
    /// reassigned with [`Term::Assign`]. The cell never escapes its scope, so the
    /// mutation is observationally pure — `let mut` is sugar for a sealed,
    /// non-escaping `Ref`. (Surface only in Sprint 1; typing/lowering follow.)
    LetMut(String, Box<Term>, Box<Term>),

    /// `x := e` — **assign** the mutable local `x` (a [`Term::LetMut`] binding) the
    /// value of `e`, in place; yields `Unit` (mutability v1; `docs/mutability.md`
    /// §1/§3). It is an expression, so it nests as `let _ = (x := e) in …`.
    ///
    /// The surface `x := e` is *one* form: a bare name on the left. Sema splits it
    /// by the binding's kind — a `let mut` cell → [`Node::Assign`] (the scalar slot
    /// store), a `Ref[T]`-typed name → [`Node::RefAssign`] (the heap-cell write,
    /// `docs/mutability.md` §1). This is the clean disambiguation: the surface stays
    /// one assignment, the cell kind decides the lowering.
    Assign(String, Box<Term>),

    /// `ref e` — allocate a fresh one-field **mutable heap cell** `Ref[T]`
    /// initialized to `e` (the value form of `let mut`; `docs/mutability.md` §1).
    /// A heap allocation, so it carries `{gc}`. `T` must be a scalar
    /// (`Int`/`Float`/`Bool`/`Unit`) in this sprint — a pointer-typed `Ref` awaits
    /// the GC write barrier (Sprint 3). The argument is an *atom* (like `len`/`sqrt`),
    /// so `ref e + 1` is `(ref e) + 1` and `ref a[i]` is `ref (a[i])`.
    RefNew(Box<Term>),

    /// `!r` — **dereference** (read) the mutable heap cell `r : Ref[T]`, yielding the
    /// `T` it holds (`docs/mutability.md` §1). Carries `{st}` (observable mutation —
    /// the cell is a first-class value, §5.2's conservative-but-honest posture). The
    /// argument is an atom: `!r + 1` is `(!r) + 1`, `!a` reads the ref named `a`. The
    /// expression-position `!` is unambiguous — a type's latent-row `!` only ever
    /// follows `->` inside a *type*, never at the head of an expression.
    Deref(Box<Term>),

    /// `extern "symbol"` or `extern "symbol" : T` — a foreign function (the OS /
    /// a DLL export). The optional `T` is the signature; **omit it** and the
    /// Win32 oracle (`locus-winapi`) supplies it (resolved before elaboration).
    /// The third field is the **minted label** the enclosing `boundary` module
    /// declared with `mints (L)` — `Some(crt)` inside a `mints (crt)` module,
    /// `None` (⟹ the default `winapi`) otherwise. It is injected on the innermost
    /// arrow; the symbol's DLL is the oracle's / loader's job, never named here.
    Extern(String, Option<Type>, Option<Label>),
    /// `extern asm "sym" : T` — a separately-assembled **Layer-0** symbol (D5,
    /// [`jasm-boundary-layer.md`]): a hand-written machine-code routine provided by
    /// a `.masm` unit, AOT-assembled and embedded in the app. The type is
    /// **required** (no oracle supplies it). It mints the **`asm`** capability —
    /// calling it infers `{asm}` — and lowers exactly like [`Extern`] (a `call` to
    /// the named symbol); the difference is the symbol comes from the embedded asm,
    /// not a DLL, and the row carries `asm`, the strongest sealed power.
    ExternAsm(String, Type),
    /// `perform ℓ e` — perform an effect operation on `e`, adding `ℓ` to
    /// the row.
    Perform(Label, Box<Term>),

    /// `handle e with H` — run `e`, intercepting its operations; the handler
    /// `H` **discharges** the labels it handles from `e`'s row (the source
    /// of "effects shrink", calculus §2.1 (op) / preservation §7).
    Handle(Box<Term>, Box<Handler>),

    /// `seal L { e }` — the **capability seal** ([`sealing-solution.md`] §4–§5):
    /// run `e`, remove the label `L` from its outward row, and **statically
    /// forbid `L` from escaping** through the result type. `nogc { e }` is sugar
    /// for `seal gc { e }`. The seal is the `runST`/`st` deep no-escape check
    /// relabeled to an arbitrary `L`: `Γ ⊢ e : A ! E ⟹ seal L { e } : A ! (E\{L})`
    /// provided `L` occurs in no row reachable from `A` (and, for `gc`, no
    /// gc-managed datum escapes). It is **runtime-transparent** — erased after
    /// elaboration; only the row removal and the boundary check remain. Violation
    /// is `RN-E0403 cap.seal-leak`.
    Seal(Label, Box<Term>),

    /// `quote e` — build code. **Raises** the stage: a stage-`s` body becomes
    /// a stage-`(s+1)` code value `□(A ! E_O)`. `δ` keeps the object effects
    /// inside the `□` and lets the generative ones out (§3.2/§3.3).
    Quote(Box<Term>),

    /// `${ c }` (splice) — embed a code value into the code being built.
    /// **Lowers** the stage; the spliced code's object effects join the
    /// surrounding (object) row (§3.3). It is also the **default locus** for
    /// `Insert` (§4.1).
    Splice(Box<Term>),

    /// `genlet c` ≡ `perform Insert(c)` (calculus §4.1) — request that code
    /// `c` be hoisted to a `let` at an enclosing **locus**, yielding a
    /// reference to the binding. A generation-stage *generative* effect.
    Genlet(Box<Term>),

    /// `letloc { e }` — a **locus**: where hoisted `let`s land. It is the
    /// handler for `Insert`, discharging it from `e`'s row (§4.1; a `splice`
    /// is the default outermost locus).
    Letloc(Box<Term>),

    /// An effect declaration, in scope for `body`. Two surfaces:
    /// `effect name : Param -> Result in body` (one op, named for the effect) and
    /// `effect Name { op : P -> R ; … } in body` (several ops grouped under one
    /// effect). **Type-level only:** it extends `Σ` so each `perform op` and the
    /// matching handler clause `op(x) => …` agree on the op's param/result; the
    /// declaration itself erases (no node, no runtime). `name` groups the ops
    /// (the future sealing boundary); the `ops` are the perform-able labels.
    Effect {
        name: String,
        ops: Vec<OpDecl>,
        body: Box<Term>,
    },

    /// `trait Name a [requires C1 a, …] { m1 : sig1 ; m2 : sig2 ; … } in body` —
    /// declare a **single-parameter trait** (D6, `trait-resolution.md` §1.1), in
    /// scope for `body`. `param` is the trait's type parameter (`a`); `supers` are
    /// its superclass constraints (`requires Eq a` ⟹ `Ord` entails `Eq`); each
    /// method is `m : sig`. **Nominal and registered**, like [`Term::TypeDef`]:
    /// elaboration registers the trait in a trait environment and **mints each
    /// method as a generic function** whose scheme carries the `Trait a`
    /// constraint (so `show : ∀a. Show a => a -> String`); the declaration then
    /// elaborates to its `body` (no runtime node — same passthrough as `TypeDef`).
    Trait {
        name: String,
        param: String,
        supers: Vec<Constraint>,
        methods: Vec<TraitMethodSig>,
        /// The **declaring module** name (traits v1 orphan check R5,
        /// `trait-resolution.md` §4). `None` at the parser (a bare program has no
        /// module); [`crate::stdlib::graft`] stamps the surrounding module's name
        /// when a `module … =` body is grafted, so sema can compare an instance's
        /// module against the trait's defining module.
        module: Option<String>,
        body: Box<Term>,
    },

    /// `instance Name Type [requires …] { m1 = e1 ; … } in body` — declare an
    /// **instance** of trait `Name` at the head type `head` (`trait-resolution.md`
    /// §1.1), in scope for `body`. `requires` are the instance's context
    /// constraints (the recursive sub-obligations of resolution, Sprint 2). Each
    /// method binds `m = e`. Elaboration registers `(trait, head)` → method impls
    /// and (lightly in Sprint 1) checks each body against the trait method
    /// signature instantiated at `head`; then elaborates to `body` (passthrough,
    /// like `TypeDef`). **No coherence/overlap/orphan/termination checks yet**
    /// (Sprint 2).
    Instance {
        trait_name: String,
        head: Type,
        requires: Vec<Constraint>,
        methods: Vec<InstanceMethod>,
        /// The **declaring module** name (traits v1 orphan check R5,
        /// `trait-resolution.md` §4). `None` at the parser; stamped by
        /// [`crate::stdlib::graft`]. An instance is an *orphan* (RN-E0232) unless
        /// its module defines the trait or the type head.
        module: Option<String>,
        body: Box<Term>,
    },

    // ── the `mem` capability: raw memory access (all `! {mem}`) ──────────────
    /// `peekW addr` — read `W` bits at the `Int` address, zero-extended to `Int`.
    Peek(MemWidth, Box<Term>),
    /// `pokeW addr val` — write the low `W` bits of `val` at the `Int` address;
    /// yields `Unit`.
    Poke(MemWidth, Box<Term>, Box<Term>),
    /// `fill dst byte count` — set `count` bytes at `dst` to the low byte of
    /// `byte` (memset); yields `Unit`.
    Fill(Box<Term>, Box<Term>, Box<Term>),
    /// `copy dst src count` — copy `count` bytes from `src` to `dst`,
    /// overlap-safe (memmove); yields `Unit`.
    Copy(Box<Term>, Box<Term>, Box<Term>),

    /// `a[i]` — the **array accessor**: read element `i` of `a`, the ergonomic
    /// surface over the `mem` capability. The element width comes from `a`'s
    /// type (`String` → 16-bit units, an `Int`/`Ptr` address → bytes), so the
    /// scaling is implicit; desugars to `peekW (a + i*stride)`. `! {mem}`.
    Index(Box<Term>, Box<Term>),
    /// `a[i] <- v` — the matching **store**: write `v` to element `i` of `a`;
    /// desugars to `pokeW (a + i*stride) v`, yields `Unit`. `! {mem}`.
    IndexSet(Box<Term>, Box<Term>, Box<Term>),

    /// `(e1, e2, …)` — a **tuple** value (two or more elements).
    Tuple(Vec<Term>),
    /// `let (x1, …, xn) = e in body` — destructure a tuple, binding each element.
    LetTuple(Vec<String>, Box<Term>, Box<Term>),

    /// `{ x = e1, y = e2, … }` — a **record** value (named fields).
    Record(Vec<(String, Term)>),
    /// `r.x` — project the field `x` of a record.
    Field(Box<Term>, String),

    /// `[e1, e2, …, en]` — an **array literal**: a fresh `Array[T]` of the given
    /// elements (homogeneous, at least one). Performs `gc` — it allocates.
    ArrayLit(Vec<Term>),
    /// `len a` — the **length** of an array (its element count): `Array[T] -> Int`.
    Len(Box<Term>),

    /// `type Name[a, …] = C1(T..) | C2 | … in body` — declare a (possibly
    /// recursive, possibly **parametric**) **sum type**, in scope for `body`.
    /// `params` are the declared type-parameter names in order (`[]` for a
    /// monomorphic sum — `type Color = Red | Green`). Each variant is
    /// `(ctor, field_types)`; a field type may name `Name` itself (recursion)
    /// **applied to its arguments** — `Cons(a, List[a])` is
    /// `("Cons", [Named("a", []), Named("List", [Named("a", [])])])` — and a
    /// param `a` appears as `Named("a", [])`, disambiguated from a nominal
    /// reference by membership in `params` (D9). The constructors enter scope as
    /// (polymorphic) values/functions for `body`.
    TypeDef {
        name: String,
        params: Vec<String>,
        variants: Vec<(String, Vec<Type>)>,
        /// The **declaring module** name — recorded only for the traits v1 orphan
        /// check (R5, `trait-resolution.md` §4): an `instance` is lawful in the
        /// type head's defining module. `None` at the parser; stamped by
        /// [`crate::stdlib::graft`]. Does not affect type elaboration.
        module: Option<String>,
        body: Box<Term>,
    },
    /// `C(e1, …, en)` (or `C` for a nullary constructor) — build a sum value with
    /// constructor `C`. The parser emits this for a **capitalised** identifier.
    Construct(String, Vec<Term>),
    /// `match e with | pat => body | …` — scrutinise a sum value, dispatch on its
    /// constructor, bind the payload, and evaluate the chosen arm.
    Match {
        scrutinee: Box<Term>,
        arms: Vec<MatchArm>,
    },
}

/// One item in an internal [`Term::Block`]. These are the declaration-like term
/// forms whose `body` was formerly another nested term.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BlockItem {
    Let(String, Term),
    LetRec(String, Type, Term),
    LetMut(String, Term),
    LetTuple(Vec<String>, Term),
    Effect {
        name: String,
        ops: Vec<OpDecl>,
    },
    TypeDef {
        name: String,
        params: Vec<String>,
        variants: Vec<(String, Vec<Type>)>,
        module: Option<String>,
    },
    Trait {
        name: String,
        param: String,
        supers: Vec<Constraint>,
        methods: Vec<TraitMethodSig>,
        module: Option<String>,
    },
    Instance {
        trait_name: String,
        head: Type,
        requires: Vec<Constraint>,
        methods: Vec<InstanceMethod>,
        module: Option<String>,
    },
}

/// A **module declaration** — a header over a let-chain body (`sealing-plan.md`
/// S1a). `module Name at <layer> seals (…) exposing (…) = <body>`. The body is
/// the existing let-chain-ending-in-`()` (or a `handle … with { … }` wrap), so
/// the graft mechanism is unchanged underneath.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModuleDecl {
    /// The module's name, possibly dotted (`Kernel.Console`).
    pub name: String,
    /// Its declared [`Layer`] (`at boundary`/…); `App` if omitted at the surface.
    pub layer: Layer,
    /// Labels this module **mints** — the raw capabilities its `extern`s create
    /// (`mints (winapi)` / `mints (crt)` / …). Only a `boundary` module may carry
    /// these; each `extern` in the body is stamped with the (first) mint label so
    /// sema injects it instead of the default `winapi`. Empty if absent.
    pub mints: Vec<Label>,
    /// Labels this module **seals** at its export edge — none of its exposed
    /// bindings may carry these in their type (checked in S4). Empty if absent.
    pub seals: Vec<Label>,
    /// The names it **exposes**; `None` exports every bound name (S1b/S4).
    pub exposing: Option<Vec<String>>,
    /// The module body — a `Term` (let-chain ending in `()`, or a handler wrap).
    pub body: Term,
}

/// A **parsed program**: zero or more module declarations and imports, then the
/// entry expression (`sealing-plan.md` S1a). The single-expression [`Term`]
/// produced by [`crate::parse::parse`] is the `modules`-empty, `imports`-empty
/// case with `entry` the whole program — so this is purely additive.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProgramSource {
    pub modules: Vec<ModuleDecl>,
    pub imports: Vec<String>,
    pub entry: Term,
}

/// One arm of a `match`: a pattern and the expression it guards.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MatchArm {
    pub pat: Pattern,
    pub body: Term,
}

/// A `match` pattern. **Flat** for now — a constructor binding its fields to
/// names, or a wildcard. (Nested patterns are a later refinement.)
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pattern {
    /// `C(x, y, …)` (or `C` for a nullary constructor) — match constructor `C`,
    /// binding each field positionally to a fresh name.
    Ctor(String, Vec<String>),
    /// `_` — match anything (the catch-all).
    Wild,
}

/// An effect operation's signature: `op : param => result` (calculus §1.1).
/// `perform op v` requires `v : param` and yields `result` — the value the
/// handler hands back through `resume`. The op's *label* enters the row.
///
/// An effect may group **several** ops (e.g. `State { get, put }`); each is its
/// own perform-able label, registered in `Σ` from an [`OpDecl`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OpSig {
    pub param: Type,
    pub result: Type,
}

/// One operation in an `effect` declaration — its name plus its signature
/// (`op : Param -> Result`). Lowered into `Σ` as `op ↦ OpSig{param, result}`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OpDecl {
    pub op: String,
    pub param: Type,
    pub result: Type,
}

/// A **trait constraint** `Trait τ` (`trait-resolution.md` §1.1) — the atom of a
/// qualified type `C a => τ` and an instance's `requires` context. `trait_name`
/// is the (single-parameter, D6) trait; `ty` is the type it constrains — a
/// variable (`Show a`), a base type (`Show Int`), or a `Named` head (`Show
/// List[a]`). Recorded on a [`crate::check::Scheme`] by `generalize`, copied into
/// a pending **obligation** by `instantiate`. Sprint 1 records and surfaces them;
/// Sprint 2 discharges them (entailment / resolution).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Constraint {
    pub trait_name: String,
    pub ty: Type,
}

impl std::fmt::Display for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.trait_name, self.ty)
    }
}

/// One method declared in a `trait` body — `m : sig` (`trait-resolution.md`
/// §1.1). The signature may itself be qualified / row-carrying; the trait's own
/// `Trait a` constraint is *implicit* (added when the method is minted as a
/// generic function), so `sig` here is only what the author wrote after `:`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraitMethodSig {
    pub name: String,
    pub sig: Type,
}

/// One method implemented in an `instance` body — `m = e` (`trait-resolution.md`
/// §1.1). The body `e` is checked against the trait's method signature
/// instantiated at the instance head (light in Sprint 1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InstanceMethod {
    pub name: String,
    pub body: Term,
}

/// A handler — `with { op(arg) -> …resume… ; return(var) -> … }` (calculus
/// §2.1 (op)/(return), §4 operational).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Handler {
    pub ops: Vec<OpClause>,
    pub ret: Return,
}

/// One operation clause: `op(arg) -> body`, with `resume` in scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OpClause {
    /// The operation handled — its label is discharged from the row.
    pub op: Label,
    /// Binds `arg : op.param`.
    pub arg: String,
    /// Binds `resume : op.result -> R` — the captured continuation.
    pub resume: String,
    pub body: Box<Term>,
}

/// The return clause: `return(var) -> body`, run on the handled value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Return {
    /// Binds `var : (the handled expression's type)`.
    pub var: String,
    pub body: Box<Term>,
}

// ── Display (for the CLI and diagnostics) ───────────────────────────────

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Label::Exn(x) => write!(f, "exn[{x}]"),
            Label::World(s) | Label::User(s) => write!(f, "{s}"),
            Label::Gc => write!(f, "gc"),
            Label::St => write!(f, "st"),
            Label::Insert => write!(f, "Insert"),
        }
    }
}

impl std::fmt::Display for Row {
    /// `{}` when closed-empty, `{a, b, …}` when closed, `{a, … | ρN}` when open.
    /// A **closed** row renders exactly as before (the tail clause is omitted
    /// when the tail set is empty), so monomorphic output is byte-for-byte unchanged.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("{")?;
        for (i, l) in self.labels.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{l}")?;
        }
        if !self.tails.is_empty() {
            if self.labels.is_empty() {
                f.write_str("| ")?;
            } else {
                f.write_str(" | ")?;
            }
            for (i, RowVarId(n)) in self.tails.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "ρ{n}")?;
            }
        }
        f.write_str("}")
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // A residual unification variable. Only ever seen in a diagnostic
            // mid-inference (zonk removes it before any later phase); spelled
            // `?N` to read as "an as-yet-unknown type".
            Type::Var(TyVarId(n)) => write!(f, "?{n}"),
            Type::Int => f.write_str("Int"),
            Type::Float => f.write_str("Float"),
            Type::Float32 => f.write_str("Float32"),
            Type::Vector(shape, elem) => write!(f, "{}[{elem}]", shape.name()),
            Type::Mask(shape) => write!(f, "Mask[{}]", shape.name()),
            Type::Bool => f.write_str("Bool"),
            Type::Unit => f.write_str("Unit"),
            Type::Str => f.write_str("String"),
            Type::I32 => f.write_str("I32"),
            Type::U32 => f.write_str("U32"),
            Type::Ptr => f.write_str("Ptr"),
            Type::Fun(a, b, r) if r.is_pure() => write!(f, "{a} -> {b}"),
            Type::Fun(a, b, r) => write!(f, "{a} -> {b} ! {r}"),
            Type::Code(a, r) if r.is_pure() => write!(f, "Code[{a}]"),
            Type::Code(a, r) => write!(f, "Code[{a} ! {r}]"),
            Type::Tuple(ts) => {
                f.write_str("(")?;
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{t}")?;
                }
                f.write_str(")")
            }
            Type::Record(fs) => {
                f.write_str("{")?;
                for (i, (name, t)) in fs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {t}")?;
                }
                f.write_str("}")
            }
            Type::Array(t) => match &**t {
                Type::Vector(shape, elem) => write!(f, "{}[{elem}]", shape.plural_name()),
                _ => write!(f, "Array[{t}]"),
            },
            // A monomorphic sum (`args == []`) renders as a bare name — **byte
            // for byte** the pre-S3 output (D8). A parametric instance renders
            // its arguments: `List[Int]`, `Pair[Int, Bool]`.
            Type::Named(n, args) if args.is_empty() => f.write_str(n),
            Type::Named(n, args) => {
                write!(f, "{n}[")?;
                for (i, t) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{t}")?;
                }
                f.write_str("]")
            }
        }
    }
}

/// The native ABI class/width of one foreign value on Win64.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Width {
    /// 32-bit, **sign**-extended on return (`int`, `BOOL`).
    I32,
    /// 32-bit, **zero**-extended on return (`DWORD`, `UINT`).
    U32,
    /// 64-bit, passed / returned as-is (`Int`, and pointers: `HANDLE`, `LP…`).
    W64,
    /// Native `float`, passed / returned in the Win64 FP lane.
    F32,
    /// Native `double`, passed / returned in the Win64 FP lane.
    F64,
}

/// The native call signature of an extern: one [`Width`] per parameter plus the
/// return width. Drives the trunc / sext / zext at the FFI boundary; the rest of
/// the language never sees it — values stay uniform `i64`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExternAbi {
    pub params: Vec<Width>,
    pub ret: Width,
}

/// The **runtime representation class** of a value, collapsing [`ValueLayout`]
/// to the one distinction polymorphic lowering cares about: does it live in a
/// single traced pointer cell (a GC handle) or as a raw scalar?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repr {
    /// One traced pointer cell — a GC handle. Sums, tuples, records, arrays,
    /// functions, and (by decision) type variables: a polymorphic slot is a
    /// uniform handle, so a generic body lowers once.
    Uniform,
    /// A raw scalar cell the collector copies verbatim: Int/Bool/Unit/Str and
    /// the unboxed numerics (Float/Float32, concrete vectors).
    Scalar,
    /// No representation decided yet — an unsolved layout (e.g. a vector over a
    /// type variable). The lowering guard rejects these rather than guess.
    Unknown,
}

/// A representation coercion inserted where a value's [`Repr`] differs from the
/// slot it flows into — the tag/untag that makes uniform-representation
/// polymorphism safe with **tags, not boxes** (`docs/repr-poly-impl.md`).
/// `None` is the overwhelmingly common case — and, crucially, the *passthrough*
/// when both sides are already uniform (`Var`→`Var`): the bits are already in
/// `Var` form, so the store is a verbatim raw-word copy, not a re-tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Coercion {
    None,
    /// A concrete tag-room **scalar** flowing into a uniform (`Var`) word cell:
    /// shift it into a tagged immediate, `value << 2` (low bits `00`, so the
    /// collector skips it). Guarded by an i62 overflow trap at lowering — a value
    /// with magnitude ≥ 2⁶¹ aborts loudly rather than truncate (the `Int`
    /// decision). Replaces the boxing-era `Box` (no heap allocation now).
    Tag,
    /// A uniform (`Var`) word value used at a concrete scalar type: arithmetic
    /// (sign-extending) `value >> 2`, recovering the i62 scalar. Replaces the
    /// boxing-era `Unbox` (no box dereference now). Exercised by the load side of
    /// the matrix (match-binder / consumed-at-scalar); the list_len slice does not
    /// hit it (TODO: load-side insertion is a T2 follow-up — see `tagcheck`).
    Untag,
    /// A concrete **managed handle** (a `0xABCD` table index) flowing into a
    /// uniform (`Var`) word cell: resolve it to its traced object word (`addr|10`)
    /// so the collector follows AND rewrites it on evacuation. The inverse of
    /// `FromPtr`. Lowered to a `locus_gc_to_ptr` call (NOT a shift) —
    /// `table[resolve(h)].raw()`, exactly `set_ptr`'s stored word.
    ToPtr,
    /// A uniform (`Var`) word read where a concrete **managed handle** is wanted:
    /// the word is an `addr|10` object pointer, so intern a fresh handle for it.
    /// The inverse of `ToPtr`. Lowered to a `locus_gc_from_ptr` call —
    /// `intern(Word::from_raw(w))`, exactly `get_ptr`'s interning tail.
    FromPtr,
}

impl Type {
    /// Storage layout for this value when it is stored as a single field in a
    /// managed object. A tuple/record/array/function/sum value is itself a
    /// handle, so its field layout is one traced pointer cell; its payload
    /// layout is described separately by [`Type::aggregate_storage_layout`].
    pub fn storage_layout(&self) -> ValueLayout {
        match self {
            Type::Fun(..)
            | Type::Tuple(_)
            | Type::Record(_)
            | Type::Array(_)
            | Type::Named(..)
            | Type::Str => ValueLayout::pointer_cell(),
            Type::Vector(shape, elem) if matches!(&**elem, Type::Float32) => {
                let bytes = shape.lanes() * 4;
                ValueLayout::scalar_bytes(bytes, bytes.min(16).max(4))
            }
            Type::Vector(shape, elem) if matches!(&**elem, Type::Float) => {
                let bytes = shape.lanes() * 8;
                ValueLayout::scalar_bytes(bytes, bytes.min(16).max(8))
            }
            Type::Vector(_, _) => ValueLayout::unknown_scalar_cell(),
            Type::Mask(_) => ValueLayout::scalar_cell(),
            Type::Float32 => ValueLayout::scalar_bytes(4, 4),
            // A `Type::Var` field is a repr-poly **word cell** (D4, "Lowering: the
            // Var-cell coercion matrix"): laid in the collector's traced range so
            // `classify` runs on it (counted in `pointer_cells`), but holding a raw
            // word — a real interior pointer (`addr|10`, followed) or a tag-room
            // scalar (`value<<2`, `00`, skipped). It is `known` (the boxing-era
            // `unknown_scalar_cell` flip is retired) and gc-reachable, and the
            // store path keys on `word` to use the verbatim primitive. This makes a
            // generic `List[Int]` lower; the lowering guard no longer fires on it.
            Type::Var(_) => ValueLayout::word_cell(),
            _ => ValueLayout::scalar_cell(),
        }
    }

    /// Combine field layouts for a tuple/record/constructor payload. Pointer
    /// cells and scalar cells occupy separate regions in the heap object.
    pub fn aggregate_storage_layout<'a>(fields: impl IntoIterator<Item = &'a Type>) -> ValueLayout {
        ValueLayout::aggregate(fields.into_iter().map(Type::storage_layout))
    }

    pub fn has_known_storage_layout(&self) -> bool {
        self.storage_layout().known
    }

    pub fn is_scalar_only(&self) -> bool {
        // `Var` now has a *known* (word-cell) layout, so the `!known` guard below
        // no longer catches it — but these predicates classify a **value**'s type,
        // which D6 zonks before any layout query (the recorded *slot* `Var` is read
        // through `storage_layout`, never here). A `Var` reaching here is still a
        // zonk-ordering bug, so flag it explicitly rather than report `word_cell`.
        if let Type::Var(v) = self {
            unreachable!("is_scalar_only on an un-zonked type variable {v:?}")
        }
        let layout = self.storage_layout();
        if !layout.known {
            unreachable!("is_scalar_only on an unknown layout");
        }
        layout.is_scalar_only()
    }

    pub fn is_gc_reachable(&self) -> bool {
        if let Type::Var(v) = self {
            unreachable!("is_gc_reachable on an un-zonked type variable {v:?}")
        }
        let layout = self.storage_layout();
        if !layout.known {
            unreachable!("is_gc_reachable on an unknown layout");
        }
        layout.is_gc_reachable()
    }

    /// Is a value of this type a **GC-managed heap reference** — stored in an
    /// object as a *traced pointer* cell the collector follows and rewrites — as
    /// opposed to an **opaque scalar** stored verbatim?
    ///
    /// Pointers: managed heap values (`Fun`, tuples, records, arrays, and named
    /// sums). Scalars: `Int`/`Bool`/`Unit` (immediates) and `Str` (pointer to
    /// immortal static text). `Code` is compile-time-only and never reaches the
    /// value heap.
    pub fn is_gc_ref(&self) -> bool {
        // An unsolved variable must be zonked before any GC-layout query (D6); the
        // recorded *slot* `Var` is classified via `storage_layout`/`is_word_cell`,
        // not here. `Var` now has a known word-cell layout, so flag it explicitly
        // rather than let it fall through as a (misleading) single pointer cell.
        if let Type::Var(v) = self {
            unreachable!("is_gc_ref on an un-zonked type variable {v:?}")
        }
        let layout = self.storage_layout();
        if !layout.known {
            unreachable!("is_gc_ref on an unknown layout");
        }
        layout.is_single_pointer_cell()
    }

    /// The **representation class** of this type — the pointer-vs-scalar
    /// distinction polymorphic lowering turns into tag/untag coercions. A type
    /// variable is [`Repr::Uniform`] *by decision*: every polymorphic slot is a
    /// uniform word cell, so a generic body lowers once. (`storage_layout(Var)` is
    /// now the traced [`ValueLayout::word_cell`] — the boxing-era unknown-scalar
    /// placeholder is retired — so this `Var` short-circuit and that layout agree:
    /// uniform, traced, verbatim-stored.)
    pub fn repr(&self) -> Repr {
        if let Type::Var(_) = self {
            return Repr::Uniform;
        }
        let layout = self.storage_layout();
        if !layout.known {
            Repr::Unknown
        } else if layout.is_single_pointer_cell() {
            Repr::Uniform
        } else {
            Repr::Scalar
        }
    }

    /// The **`Wide` kind** predicate (D5/D3): is this a value whose
    /// representation is *wider than tag-room* and therefore cannot inhabit a
    /// traced `Var` word cell? The exclusion set is exactly `Float`, `Float32`,
    /// and the SIMD `Pair`/`Quad`/`Oct` (128–512-bit). Everything else —
    /// `Int`/`Bool`/`Unit`, all handles, and (by decision) `Str`, which stays
    /// `Uniform`-eligible via its ≥4-aligned-pointer representation — is
    /// acceptable in a traced `Var` cell.
    ///
    /// This is the *orthogonal axis* [`Type::repr`] deliberately does not carry:
    /// `repr` lumps `Float` in with `Int` as `Repr::Scalar` (both copied verbatim
    /// by the collector), but `Float` is `Wide` and `Int` is tag-room, so only the
    /// kind query distinguishes them. Today T1 still consults this at the
    /// conservative unification guard; D3's intended binding site is the traced
    /// store, not a stack-only type-variable use.
    ///
    /// (// T1 Str-alignment decision: `Str` kept `Uniform`-eligible — it is a
    /// static, never-moving pointer that is ≥4-aligned, so it is sound in a `Var`
    /// cell as an `Immediate` low-bits-`00` word; it does *not* join the `Wide`
    /// set. See repr-poly-impl §"`Str`".)
    pub fn is_wide(&self) -> bool {
        matches!(
            self,
            Type::Float | Type::Float32 | Type::Vector(..) | Type::Mask(_)
        )
    }

    /// The coercion needed to place a `value` into a `slot` of possibly
    /// different representation. Driven by the slot's *declared* type (which
    /// retains the type variable at a polymorphic position) against the value's
    /// concrete type. An `Unknown` on either side yields `None`: the lowering
    /// guard, not a silently-wrong coercion, handles the undecided case.
    pub fn coercion(slot: &Type, value: &Type) -> Coercion {
        match (slot.repr(), value.repr()) {
            // A concrete scalar entering a uniform (`Var`) slot is tagged.
            (Repr::Uniform, Repr::Scalar) => Coercion::Tag,
            // A uniform (`Var`) value consumed at a concrete scalar is untagged.
            (Repr::Scalar, Repr::Uniform) => Coercion::Untag,
            // Both uniform — distinguish a `Var` word cell from a concrete managed
            // handle by SHAPE (`repr` collapses both to `Uniform`). `is_gc_ref`
            // panics on `Var`, so test `Var`-ness FIRST, then query the handle.
            (Repr::Uniform, Repr::Uniform) => {
                match (matches!(slot, Type::Var(_)), matches!(value, Type::Var(_))) {
                    // `Var`→`Var`: the verbatim passthrough (e.g. list_reverse's h).
                    (true, true) => Coercion::None,
                    // concrete managed handle → `Var` slot: resolve to `addr|10`.
                    // `Str` is Uniform-but-not-`gc_ref`, so it stays `None` (a
                    // static low-bits-`00` immediate, stored verbatim, never traced).
                    (true, false) => {
                        if value.is_gc_ref() {
                            Coercion::ToPtr
                        } else {
                            Coercion::None
                        }
                    }
                    // `Var` word read as a concrete managed handle: intern it.
                    (false, true) => {
                        if slot.is_gc_ref() {
                            Coercion::FromPtr
                        } else {
                            Coercion::None
                        }
                    }
                    // handle→handle (monomorphic), or anything involving `Str`.
                    (false, false) => Coercion::None,
                }
            }
            // Handle/Unknown joints (a `Wide` or undecided side) coerce nothing.
            _ => Coercion::None,
        }
    }

    /// The native width of a single (leaf) foreign type.
    pub fn width(&self) -> Width {
        match self {
            Type::I32 => Width::I32,
            Type::U32 => Width::U32,
            Type::Float32 => Width::F32,
            Type::Float => Width::F64,
            Type::Vector(shape, elem) => unreachable!(
                "width() on SIMD vector type {}[{elem}]; vector FFI is not supported yet",
                shape.name()
            ),
            Type::Mask(shape) => unreachable!(
                "width() on SIMD mask type Mask[{}]; vector FFI is not supported yet",
                shape.name()
            ),
            // A variable has no native width — externs are concrete (D7); a Var
            // here means an un-zonked FFI type, a bug.
            Type::Var(v) => unreachable!("width() on an un-zonked type variable {v:?}"),
            // Int, Ptr, Bool, Unit, and other scalar handles: 64-bit cells.
            _ => Width::W64,
        }
    }

    /// Replace the numeric FFI widths (`I32`/`U32`) with `Int` throughout, so the
    /// value world stays uniform. `Ptr` survives (an opaque pointer is its own
    /// value-world type). Arrow and `Code` structure are preserved.
    pub fn erase_widths(&self) -> Type {
        match self {
            // `Ptr` stays a value-world type (an opaque machine word — a handle
            // or string pointer); only the numeric widths collapse to `Int`.
            Type::I32 | Type::U32 => Type::Int,
            Type::Fun(a, b, r) => Type::Fun(
                Box::new(a.erase_widths()),
                Box::new(b.erase_widths()),
                r.clone(),
            ),
            Type::Code(t, r) => Type::Code(Box::new(t.erase_widths()), r.clone()),
            Type::Vector(shape, elem) => Type::Vector(*shape, Box::new(elem.erase_widths())),
            // A variable carries no FFI width to erase — identity. (Externs are
            // concrete, so this is reached only by the structural recursion above
            // on a hypothetical var-bearing signature; preserving it is correct.)
            Type::Var(_) => self.clone(),
            other => other.clone(),
        }
    }

    /// Read the native [`ExternAbi`] off a foreign function's declared type: the
    /// width of each argument along the arrow spine, and of the result. A lone
    /// `Unit` domain is the nullary call (no parameters).
    pub fn extern_abi(&self) -> ExternAbi {
        let mut params = Vec::new();
        let mut t = self;
        loop {
            match t {
                Type::Fun(a, b, _) => {
                    if matches!(**a, Type::Unit) && !matches!(**b, Type::Fun(..)) {
                        return ExternAbi {
                            params,
                            ret: b.width(),
                        };
                    }
                    params.push(a.width());
                    t = b;
                }
                // Externs are concrete (D7); a variable in an FFI signature is an
                // un-zonked bug. `width()` below rejects it loudly — make the
                // intent explicit here rather than leaning on the fall-through.
                Type::Var(v) => unreachable!("extern_abi on an un-zonked type variable {v:?}"),
                other => {
                    return ExternAbi {
                        params,
                        ret: other.width(),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod repr_tests {
    use super::*;

    fn var() -> Type {
        Type::Var(TyVarId(0))
    }
    fn list_of(t: Type) -> Type {
        Type::Named("List".into(), vec![t])
    }

    #[test]
    fn repr_classifies_pointer_vs_scalar() {
        // raw scalar cells
        assert_eq!(Type::Int.repr(), Repr::Scalar);
        assert_eq!(Type::Bool.repr(), Repr::Scalar);
        assert_eq!(Type::Float.repr(), Repr::Scalar);
        // traced handles — uniform
        assert_eq!(list_of(Type::Int).repr(), Repr::Uniform);
        assert_eq!(
            Type::Tuple(vec![Type::Int, Type::Int]).repr(),
            Repr::Uniform
        );
        // a type variable is uniform *by decision* (a polymorphic slot is a handle)
        assert_eq!(var().repr(), Repr::Uniform);
    }

    #[test]
    fn coercion_tags_scalar_into_polymorphic_slot() {
        // Int flowing into a type-variable field — e.g. Cons(1, _) — tags.
        assert_eq!(Type::coercion(&var(), &Type::Int), Coercion::Tag);
        // a concrete MANAGED HANDLE into a type-variable field must be resolved to
        // an `addr|10` interior pointer (ToPtr) — its `0xABCD` index bits are NOT a
        // valid word-cell word (this was the nested-container crash).
        assert_eq!(Type::coercion(&var(), &list_of(Type::Int)), Coercion::ToPtr);
        // the inverse: a `Var` word read where a managed handle is wanted interns it.
        assert_eq!(
            Type::coercion(&list_of(Type::Int), &var()),
            Coercion::FromPtr
        );
        // `Str` has SCALAR repr (its `storage_layout` is a scalar cell), so it is
        // tagged like any tag-room immediate: `Str << 2` is a low-bits-`00` word the
        // collector skips, and `>> 2` recovers the static, never-moving pointer. It
        // is NOT a managed handle, so it never takes the ToPtr/FromPtr path.
        assert_eq!(Type::coercion(&var(), &Type::Str), Coercion::ToPtr);
        assert_eq!(Type::coercion(&Type::Str, &var()), Coercion::FromPtr);
        // a polymorphic (uniform) value consumed at a concrete scalar untags.
        assert_eq!(Type::coercion(&Type::Int, &var()), Coercion::Untag);
        // monomorphic joints never coerce.
        assert_eq!(Type::coercion(&Type::Int, &Type::Int), Coercion::None);
        assert_eq!(
            Type::coercion(&list_of(Type::Int), &list_of(Type::Int)),
            Coercion::None
        );
        // two type variables (both uniform) — no coercion (the verbatim passthrough).
        assert_eq!(Type::coercion(&var(), &var()), Coercion::None);
    }

    #[test]
    fn a_type_variable_field_is_a_traced_word_cell() {
        // The D4 layout flip: a `Var` field is now a *known*, gc-reachable word
        // cell (laid in the traced pointer region, `classify` runs), distinguished
        // from a real pointer cell only by `is_word_cell` (the verbatim-store key).
        let layout = var().storage_layout();
        assert!(layout.known);
        assert!(layout.is_word_cell());
        assert!(layout.is_gc_reachable());
        assert!(layout.is_single_pointer_cell()); // shares the pointer-region shape
        assert!(!layout.is_scalar_only());
        // A concrete pointer cell is NOT a word cell (so the store path can split).
        assert!(!ValueLayout::pointer_cell().is_word_cell());
        assert!(!Type::Int.storage_layout().is_word_cell());
    }

    #[test]
    fn is_wide_pins_the_d5_exclusion_set() {
        // The `Wide` kind (D5, repr-poly-impl §"D5") is exactly the values too
        // wide for the 2-bit tag-room a uniform `Var` cell holds, so it is the
        // complete set of types that *cannot* instantiate a type variable. This
        // test pins that set against the matrix so a future edit to the predicate
        // can't silently widen or narrow the language cut.

        // Wide: the binary floats…
        assert!(Type::Float.is_wide());
        assert!(Type::Float32.is_wide());
        // …and every SIMD shape (128–512-bit), over any element.
        assert!(Type::Vector(VectorShape::Pair, Box::new(Type::Float)).is_wide());
        assert!(Type::Vector(VectorShape::Quad, Box::new(Type::Float32)).is_wide());
        assert!(Type::Vector(VectorShape::Oct, Box::new(Type::Float32)).is_wide());
        assert!(Type::Mask(VectorShape::Quad).is_wide());

        // NOT wide — tag-room scalars and immediates (the kept-uniform cases the
        // motivating `List[Int]` depends on).
        assert!(!Type::Int.is_wide());
        assert!(!Type::Bool.is_wide());
        assert!(!Type::Unit.is_wide());
        // `Str` stays Uniform-eligible *by decision* (≥4-aligned static pointer,
        // low-bits-`00` immediate); it must NOT be in the exclusion set.
        assert!(!Type::Str.is_wide());
        // Handles are uniform (they ride in the pointer cell, not the value bits).
        assert!(!list_of(Type::Int).is_wide());
        assert!(!Type::Tuple(vec![Type::Int, Type::Int]).is_wide());
        // A type variable is itself not wide — `is_wide` is about concrete values
        // too large for a traced `Var` word cell, so the var node is not wide.
        assert!(!var().is_wide());
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn rows_render() {
        assert_eq!(Row::pure().to_string(), "{}");
        assert_eq!(Row::single(Label::Gc).to_string(), "{gc}");
        assert_eq!(
            Row::single(Label::World("fs".into()))
                .union(&Row::single(Label::World("net".into())))
                .to_string(),
            "{fs, net}"
        );
        let composed =
            Row::open(BTreeSet::new(), RowVarId(1)).union(&Row::open(BTreeSet::new(), RowVarId(2)));
        assert_eq!(composed.tail_set().len(), 2);
        assert_eq!(composed.to_string(), "{| ρ1, ρ2}");
    }

    #[test]
    fn types_render() {
        assert_eq!(Type::Int.to_string(), "Int");
        assert_eq!(Type::Float.to_string(), "Float");
        assert_eq!(Type::Float32.to_string(), "Float32");
        assert_eq!(Type::Mask(VectorShape::Quad).to_string(), "Mask[Quad]");
        assert_eq!(
            Type::Array(Box::new(Type::Vector(
                VectorShape::Quad,
                Box::new(Type::Float32)
            )))
            .to_string(),
            "Quads[Float32]"
        );
        assert_eq!(
            Type::Fun(Box::new(Type::Int), Box::new(Type::Bool), Row::pure()).to_string(),
            "Int -> Bool"
        );
        assert_eq!(
            Type::Code(
                Box::new(Type::Unit),
                Row::single(Label::World("console".into()))
            )
            .to_string(),
            "Code[Unit ! {console}]"
        );
    }

    #[test]
    fn scalar_tuple_payload_has_no_pointer_cells() {
        let layout = Type::aggregate_storage_layout([&Type::Int, &Type::Bool, &Type::Unit]);
        assert!(layout.known);
        assert_eq!(layout.pointer_cells, 0);
        assert_eq!(layout.scalar_cells, 3);
        assert_eq!(layout.byte_width, 24);
        assert!(layout.is_scalar_only());
    }

    #[test]
    fn aggregate_payload_counts_handle_fields_exactly() {
        let fun = Type::Fun(Box::new(Type::Int), Box::new(Type::Int), Row::pure());
        let tuple = Type::Tuple(vec![Type::Int, Type::Bool]);
        let record = Type::Record(vec![("x".into(), Type::Int)]);

        let layout = Type::aggregate_storage_layout([&Type::Int, &fun, &tuple, &record]);

        assert!(layout.known);
        assert_eq!(layout.pointer_cells, 3);
        assert_eq!(layout.scalar_cells, 1);
        assert_eq!(layout.total_cells(), 4);
        assert!(fun.is_gc_ref());
        assert!(Type::Int.is_scalar_only());
    }

    #[test]
    fn a_type_variable_layout_is_a_known_traced_word_cell() {
        // Retired boxing-era behaviour: `storage_layout(Var)` was an *unknown*
        // scalar placeholder (so the lowering guard fired). With tags it is a
        // **known** word cell — one traced cell, stored verbatim — which is what
        // makes a generic `List[Int]` lower.
        let layout = Type::Var(TyVarId(7)).storage_layout();
        assert!(layout.known);
        assert!(layout.word);
        assert_eq!(layout.pointer_cells, 1);
        assert_eq!(layout.scalar_cells, 0);
        assert_eq!(layout.byte_width, 8);
        assert!(layout.is_word_cell());
    }

    #[test]
    fn float32_has_a_packed_scalar_layout_descriptor() {
        let layout = Type::Float32.storage_layout();
        assert!(layout.known);
        assert_eq!(layout.pointer_cells, 0);
        assert_eq!(layout.scalar_cells, 1);
        assert_eq!(layout.byte_width, 4);
        assert_eq!(layout.align, 4);
    }

    #[test]
    fn extern_abi_distinguishes_fp_native_classes() {
        let ty = Type::Fun(
            Box::new(Type::Float32),
            Box::new(Type::Fun(
                Box::new(Type::Float),
                Box::new(Type::Float),
                Row::pure(),
            )),
            Row::pure(),
        );
        let abi = ty.extern_abi();
        assert_eq!(abi.params, vec![Width::F32, Width::F64]);
        assert_eq!(abi.ret, Width::F64);
    }
}
