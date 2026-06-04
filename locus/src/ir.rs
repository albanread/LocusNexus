//! The intermediate representation — **A-normal form** (the first lowering).
//!
//! Sema gives an authoritative typed tree; IR is the first *compiler* stage
//! that consumes it. The shape is **ANF**: every intermediate computation is
//! named by a `let`, and every operand is an **atom** (a variable or literal).
//! Two reasons this is the right first IR:
//!
//! 1. It makes **evaluation order** explicit — which is exactly what effect
//!    sequencing needs. A `perform` in non-tail position becomes
//!    `let x = perform op a in …`, where the `…` *is* the continuation the
//!    evidence-passing translation (`calculus.md` §5.1, a later slice) plumbs.
//! 2. It is the standard substrate for SSA / codegen.
//!
//! This slice lowers structure only — `perform` / `handle` / `quote` / … stay
//! as explicit IR ops; turning handlers into threaded **evidence** (§5) is the
//! next step. Each `let` is annotated with the **effect row** its computation
//! performs, so the IR dump shows *where the effects are*.

use std::collections::HashMap;

use crate::check::Stage;
use crate::sema::{Node, Typed, TypedHandler};
use crate::syntax::{
    BinOp, CastOp, ExternAbi, FloatMathOp, Label, MaskReduceOp, MemWidth, Row, Type, ValueLayout,
    VectorShape,
};

/// A **trivial** operand — no evaluation step, no effects.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Atom {
    Var(String),
    Int(i64),
    Float(u64),
    Bool(bool),
    Unit,
    Str(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoopVar {
    pub name: String,
    pub ty: Type,
    pub layout: ValueLayout,
    pub init: Atom,
}

/// A single **computation** — one step, possibly effectful. Operands are
/// atoms (already named); blocks (`Box<Ir>`) are sub-computations.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Comp {
    /// Return a trivial value.
    Atom(Atom),
    /// `f a` — both already atoms, with the source-level argument/result types
    /// retained so codegen can choose a typed ABI for unboxed values.
    App {
        fun: Atom,
        arg: Atom,
        arg_ty: Type,
        ret_ty: Type,
    },
    /// `extern "symbol"` — a foreign function *reference*; `usize` is its arg
    /// count (the arrow-spine length; a single `Unit` domain is 0). A bare
    /// reference is only ever a `let`-binding — calls are collected into
    /// `Foreign` below.
    Extern(String, usize),
    /// `sym(a₁, …, aₙ)` — a fully-applied **foreign call**: the spine of a
    /// curried extern application gathered into one node. Operands are atoms;
    /// the [`ExternAbi`] carries the native widths for the boundary conversion;
    /// the `winapi` effect rides on the binding's row.
    Foreign(String, Vec<Atom>, ExternAbi),
    /// `a ⊕ b` — a primitive binary op, both already atoms.
    Bin(BinOp, Atom, Atom),
    /// `a op b` on `Float` operands. Kept separate so the LLVM backend cannot
    /// accidentally reinterpret float bits as integer arithmetic while FPWork
    /// lowering is still incomplete.
    FloatBin(BinOp, Atom, Atom),
    /// An explicit numeric conversion.
    Cast(CastOp, Atom),
    /// **Tag** a concrete tag-room scalar into a uniform repr-poly word
    /// (`docs/repr-poly-impl.md` D7): `value << 2` (low bits `00`, the collector
    /// reads it as an inert immediate). Emitted for `Node::Coerce{Tag}` — a scalar
    /// crossing into a `Var` cell. Lowering **guards it with an i62 overflow
    /// trap**: a value whose magnitude needs more than 62 bits (≥ 2⁶¹) aborts
    /// loudly at the tag site rather than silently truncating (the `Int` decision).
    Tag(Atom),
    /// **Untag** a uniform repr-poly word back to its concrete scalar: arithmetic
    /// (sign-preserving) `value >> 2`. Emitted for `Node::Coerce{Untag}` — a `Var`
    /// value consumed at a concrete scalar type. (Not exercised by the list_len
    /// slice; present for the load side of the matrix.)
    Untag(Atom),
    /// **ToPtr** a concrete managed handle into a uniform repr-poly word: resolve
    /// it to its traced object pointer (`addr|10`) via `locus_gc_to_ptr`. Emitted
    /// for `Node::Coerce{ToPtr}` — a managed handle crossing into a `Var` cell.
    ToPtr(Atom),
    /// **FromPtr** a uniform repr-poly word (an `addr|10` object pointer read from
    /// a `Var` cell) back to a fresh managed handle via `locus_gc_from_ptr`.
    /// Emitted for `Node::Coerce{FromPtr}` — a `Var` word consumed as a handle.
    FromPtr(Atom),
    /// Explicit floating unary math (`sqrt`) over scalar or vector operands.
    FloatMathUnary {
        op: FloatMathOp,
        ty: Type,
        value: Atom,
    },
    /// Explicit floating binary math over vector operands (`dot`).
    FloatMathBinary {
        op: FloatMathOp,
        ty: Type,
        lhs: Atom,
        rhs: Atom,
    },
    /// Explicit floating ternary math (`fma`) over scalar or vector operands.
    FloatMathTernary {
        op: FloatMathOp,
        ty: Type,
        a: Atom,
        b: Atom,
        c: Atom,
    },
    /// Fixed-lane vector construction from scalar lane atoms.
    VectorLit {
        shape: VectorShape,
        elem_ty: Type,
        elems: Vec<Atom>,
    },
    /// Fixed-lane vector splat from one scalar atom.
    VectorSplat {
        shape: VectorShape,
        elem_ty: Type,
        value: Atom,
    },
    /// **Array vector load** (SIMD Sprint 2): read `shape.lanes()` contiguous
    /// scalar elements of the managed array `arr` (each of LLVM type `elem_ty`),
    /// starting at element index `idx`, as one packed `<lanes x elem_ty>` value —
    /// a single SIMD load at the array payload's byte address `base + idx*elem_bytes`,
    /// bounds-checked (`idx + lanes <= len`). `elem_ty` is the lane element type
    /// (`Float32`/`Float`), the array's element type.
    VectorLoad {
        shape: VectorShape,
        elem_ty: Type,
        arr: Atom,
        idx: Atom,
    },
    /// **Array vector store** (SIMD Sprint 2): write the packed vector `value`'s
    /// lanes to the `shape.lanes()` contiguous elements at `idx` — a single SIMD
    /// store, bounds-checked. Yields `Unit`.
    VectorStore {
        shape: VectorShape,
        elem_ty: Type,
        arr: Atom,
        idx: Atom,
        value: Atom,
    },
    /// Elementwise vector arithmetic.
    VectorBin {
        op: BinOp,
        shape: VectorShape,
        elem_ty: Type,
        lhs: Atom,
        rhs: Atom,
    },
    /// Elementwise vector comparison, producing a fixed-lane SIMD mask.
    VectorCompare {
        op: BinOp,
        shape: VectorShape,
        elem_ty: Type,
        lhs: Atom,
        rhs: Atom,
    },
    /// Lane-wise vector blend under a SIMD mask.
    VectorSelect {
        shape: VectorShape,
        elem_ty: Type,
        mask: Atom,
        then_value: Atom,
        else_value: Atom,
    },
    /// Horizontal mask reduction to a scalar Bool.
    MaskReduce {
        op: MaskReduceOp,
        shape: VectorShape,
        mask: Atom,
    },
    /// Static lane extraction from a vector atom.
    VectorExtract {
        vector: Atom,
        lane: usize,
        elem_ty: Type,
    },
    /// `if c then <block> else <block>` — the condition is already an atom.
    If(Atom, Box<Ir>, Box<Ir>),
    /// Structured accumulator loop:
    /// `loop x = init, ... while <cond> do <next_x>, ... else <result>`.
    Loop {
        vars: Vec<LoopVar>,
        cond: Box<Ir>,
        steps: Vec<Ir>,
        result: Box<Ir>,
    },
    /// `λparam. <block>` — a closure value. `param_layout` records the
    /// parameter's storage layout when captured or stored in managed object
    /// layout.
    Lam {
        param: String,
        param_ty: Option<Type>,
        param_layout: ValueLayout,
        ret_ty: Type,
        body: Box<Ir>,
    },
    /// `perform op a`.
    Perform(Label, Atom),
    /// `handle <block> with H`, installed at `stage`. Its evidence is a
    /// **generation-stage value** when `stage >= 1` — *in force at compile
    /// time*, the precondition for zero-cost elimination (`calculus.md` §5.2).
    Handle {
        stage: Stage,
        scrutinee: Box<Ir>,
        handler: IrHandler,
    },
    /// `quote <block>` — build code (the block is one stage down).
    Quote(Box<Ir>),
    /// `splice a` — embed a code atom.
    Splice(Atom),
    /// `genlet a` — hoist a code atom (≡ perform Insert).
    Genlet(Atom),
    /// `letloc <block>` — an Insert locus.
    Letloc(Box<Ir>),
    /// `peekW addr` — read `W` bits at an atom address (`mem` rides on the row).
    Peek(MemWidth, Atom),
    /// `pokeW addr val` — write `W` bits at an atom address.
    Poke(MemWidth, Atom, Atom),
    /// `fill dst byte count` — memset; operands are atoms.
    Fill(Atom, Atom, Atom),
    /// `copy dst src count` — memmove; operands are atoms.
    Copy(Atom, Atom, Atom),
    /// `(a1, …, an)` — build a tuple/record (a managed heap struct); the value
    /// is its **handle**. Each field carries its storage layout. Fields are in
    /// SOURCE order; codegen lays the pointers out first (the traced range).
    Tuple(Vec<(Atom, ValueLayout)>),
    /// `[a1, ..., an]` - build a managed array. Unlike tuples, arrays carry a
    /// logical element count and store scalar elements in a byte-strided payload.
    ArrayLit {
        elems: Vec<Atom>,
        elem_layout: ValueLayout,
    },
    /// `t.i` — project a field. `layout` is the projected value layout; `slot`
    /// is the physical index *within that field's region*
    /// (pointers and scalars are numbered separately, matching the heap's
    /// `set_ptr`/`set_scalar`). Destructuring `let (x, …) = t` lowers to one
    /// `Proj` per name.
    Proj {
        tup: Atom,
        slot: usize,
        layout: ValueLayout,
        /// The projected field's value type. For a single-cell field this only
        /// rides along; for a **multi-cell scalar** field (a vector — a
        /// `Quad[Float32]` spans 2 scalar cells) it tells codegen which LLVM
        /// vector to reassemble from the contiguous cells (`layout` alone can't
        /// distinguish `Quad[Float32]` from `Pair[Float]` — both 16 B).
        ty: Type,
    },
    /// `len a` — an array's element count (read from its object header).
    Len(Atom),
    /// `a[i]` on an array — a bounds-checked element read.
    ArrayGet {
        arr: Atom,
        idx: Atom,
        elem_layout: ValueLayout,
        /// The element value type — needed (as for [`Comp::Proj`]) to reassemble
        /// a multi-cell scalar element (e.g. `Array[Quad[Float32]]`) into its
        /// LLVM vector on read.
        elem_ty: Type,
    },
    /// `a[i] <- v` on an array — a bounds-checked element write (yields `Unit`).
    ArraySet {
        arr: Atom,
        idx: Atom,
        val: Atom,
        elem_layout: ValueLayout,
    },
    /// **Allocate a mutable scalar stack slot** for `let mut name = init`
    /// (mutability v1, `docs/mutability.md` §3): codegen emits one `alloca` (an
    /// `i64` cell — every scalar rides the uniform word model) and a `store` of
    /// the initial atom into it. The cell is **function-local**: a mutable local
    /// is never captured by a closure (the Sprint-2 escape check guarantees it),
    /// so the slot never escapes and needs no GC root, handle, or `mem` boundary.
    /// Yields `Unit` (the `let mut`'s value is its body, lowered after this bind).
    SlotInit(String, Atom),
    /// **Read** a mutable local: a `load` from `name`'s stack slot. A source-level
    /// read of a `let mut` arrives as `Node::Var(name)`; lowering routes it here
    /// (rather than to an SSA `Atom::Var`) whenever `name` is a live mutable slot.
    SlotLoad(String),
    /// **Assign** a mutable local (`name := value`): a `store` of the atom into
    /// `name`'s stack slot. Yields `Unit`.
    SlotStore(String, Atom),

    /// **Allocate a one-field mutable heap cell** for `ref e` (`Ref[T]`,
    /// `docs/mutability.md` §1.1). A `Ref` is a one-field heap object — a handle —
    /// so this lowers exactly like a single-field `Tuple`: `locus_gc_alloc` for the
    /// cell's region, then a `set_scalar` of the init atom into slot 0. `layout` is
    /// the content cell's layout (one scalar cell this sprint — the v1 gate rejects
    /// a pointer-typed `Ref`). Yields the cell's **handle**.
    RefNew(Atom, ValueLayout),
    /// **Read** a heap cell (`!r`): resolve the handle atom + `get_scalar` slot 0 —
    /// like a `Proj` of the one-field object. `layout` is the content cell's layout.
    RefGet(Atom, ValueLayout),
    /// **Write** a heap cell (`r := v`): resolve the handle atom + `set_scalar` the
    /// value atom into slot 0 — like a one-field-object store. Yields `Unit`. **No
    /// write barrier** — a scalar content cell never holds a pointer, so no
    /// old→young pointer can be created (Sprint 3 adds the barrier for a pointer
    /// cell). `layout` is the content cell's layout.
    RefSet(Atom, Atom, ValueLayout),
}

/// An ANF block: a sequence of `let`-bindings ending in a tail computation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ir {
    /// `let name = <comp>  (! row)  in <rest>`.
    Let {
        name: String,
        /// The binding's value type. Codegen uses this together with `layout`
        /// when packing unboxed closure captures.
        ty: Type,
        /// Storage layout for this binding's value. Codegen uses this for typed
        /// closure-capture layout.
        layout: ValueLayout,
        row: Row,
        comp: Comp,
        rest: Box<Ir>,
    },
    /// The block's result — its tail computation (and the row it performs).
    Ret { row: Row, comp: Comp },
}

/// A handler, with each clause body lowered to its own block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IrHandler {
    pub ops: Vec<IrOpClause>,
    pub ret: IrReturn,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IrOpClause {
    pub op: Label,
    pub arg: String,
    pub arg_ty: Type,
    pub arg_layout: ValueLayout,
    pub resume: String,
    pub resume_ty: Type,
    pub resume_layout: ValueLayout,
    pub body: Box<Ir>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IrReturn {
    pub var: String,
    pub var_ty: Type,
    pub var_layout: ValueLayout,
    pub body_ty: Type,
    pub body: Box<Ir>,
}

/// Lower a typed tree to ANF.
pub fn lower(t: &Typed) -> Ir {
    lower_with_imports(t, &[])
}

/// Lower a typed tree to ANF, **seeding the extern map** with cross-module
/// imports (`docs/separate-compilation-sprints.md` Sprint 3). Each `(name,
/// symbol, abi)` makes a fully-applied call to the imported `name` collapse to a
/// single [`Comp::Foreign`] direct external call to the producer's mangled
/// `symbol` — the same spine-collector path an in-program `extern` uses, reached
/// **without** the capability mint (`Node::Extern`'s gate). The client object
/// then declares `symbol` external; the linker resolves it to the producer
/// object. `lower` delegates here with no imports (the single-unit path).
pub fn lower_with_imports(t: &Typed, imports: &[(String, String, ExternAbi)]) -> Ir {
    assert!(
        !t.has_unknown_layout(),
        "cannot lower representation-polymorphic layout yet"
    );
    let externs = imports
        .iter()
        .map(|(name, sym, abi)| (name.clone(), (sym.clone(), abi.clone())))
        .collect();
    Lower {
        next: 0,
        externs,
        mut_slots: std::collections::HashSet::new(),
    }
    .block(t)
}

/// Lower a **producer function body** to an ANF block (Sprint 3 cross-module
/// codegen). The body is the innermost expression of an uncurried exported
/// lambda chain; its parameters are already in scope (the backend binds them as
/// the flat symbol's `i64` arguments), so this is just the body's [`block`]. Any
/// `imports` the producer itself consumes are seeded exactly as in
/// [`lower_with_imports`]. Separate from `lower` because a library export has **no
/// `__locus_main`** and **no closure env** — only the body's straight ANF.
pub fn lower_function_body(body: &Typed, imports: &[(String, String, ExternAbi)]) -> Ir {
    assert!(
        !body.has_unknown_layout(),
        "cannot lower representation-polymorphic layout yet"
    );
    let externs = imports
        .iter()
        .map(|(name, sym, abi)| (name.clone(), (sym.clone(), abi.clone())))
        .collect();
    Lower {
        next: 0,
        externs,
        mut_slots: std::collections::HashSet::new(),
    }
    .block(body)
}

/// Is a spine of `n` arguments a *complete* application of an extern with this
/// ABI? Either the nullary call `f ()` (no params, one elided `Unit` argument)
/// or an n-ary call whose argument count matches the parameters.
fn foreign_applies(abi: &ExternAbi, n: usize) -> bool {
    if abi.params.is_empty() {
        n == 1
    } else {
        n == abi.params.len()
    }
}

/// Physical layout of logical field `i` in a managed object whose fields have
/// types `tys` (source order). Returns the region slot and field layout. Traced
/// pointers and opaque scalars occupy separate, independently-numbered regions
/// (the object is laid out pointers-first), so `region_slot` is the count of
/// preceding cells in the same region, matching the heap's `set_ptr` /
/// `set_scalar` and `get_ptr` / `get_scalar` APIs.
fn storage_layout_or_panic(ty: &Type) -> ValueLayout {
    let layout = ty.storage_layout();
    assert!(
        layout.known,
        "cannot lower representation-polymorphic layout yet"
    );
    layout
}

fn field_layout(tys: &[Type], i: usize) -> (usize, ValueLayout) {
    let layout = storage_layout_or_panic(&tys[i]);
    if layout.is_gc_reachable() {
        assert!(
            layout.scalar_cells == 0,
            "mixed pointer/scalar fields need first-class value packing"
        );
        let slot = tys[..i]
            .iter()
            .map(storage_layout_or_panic)
            .map(|layout| layout.pointer_cells)
            .sum();
        (slot, layout)
    } else {
        let slot = tys[..i]
            .iter()
            .map(storage_layout_or_panic)
            .map(|layout| layout.scalar_cells)
            .sum();
        (slot, layout)
    }
}

/// Walk a left-nested application `((f a) b) …` down to its head, collecting
/// the arguments left-to-right into `out`.
fn collect_spine<'a>(e: &'a Typed, out: &mut Vec<&'a Typed>) -> &'a Typed {
    match &e.node {
        Node::App { fun, arg } => {
            let head = collect_spine(fun, out);
            out.push(arg);
            head
        }
        _ => e,
    }
}

/// One prerequisite binding accumulated while normalizing a block.
struct Bind {
    name: String,
    ty: Type,
    layout: ValueLayout,
    row: Row,
    comp: Comp,
}

/// The normalizer — just a fresh-name counter. (Pure structural transform;
/// no context needed, since sema already resolved everything.)
struct Lower {
    next: u32,
    /// `let`-bound foreign functions in scope: name → (symbol, native ABI), so
    /// the application-spine collector can recognise a fully-applied extern.
    externs: HashMap<String, (String, ExternAbi)>,
    /// Mutable locals (`let mut`) in scope. A `Node::Var` naming one of these is a
    /// **slot read** (`SlotLoad`), not an SSA reference; an [`Atom::Var`] never
    /// names a mutable slot. Scoped by save/restore around each `LetMut`, so an
    /// inner `let mut x` that shadows an outer binding is handled correctly.
    mut_slots: std::collections::HashSet<String>,
}

impl Lower {
    fn fresh(&mut self) -> String {
        let n = self.next;
        self.next += 1;
        format!("t{n}")
    }

    /// Push a synthetic computation as a fresh binding, returning a reference to
    /// it. Used to materialise the address arithmetic a subscript desugars to.
    fn bind(&mut self, comp: Comp, row: Row, binds: &mut Vec<Bind>) -> Atom {
        let name = self.fresh();
        binds.push(Bind {
            name: name.clone(),
            ty: Type::Int,
            layout: ValueLayout::scalar_cell(),
            row,
            comp,
        });
        Atom::Var(name)
    }

    /// The byte address `base + index*stride` for a subscript `a[i]`, as an atom.
    /// Pure arithmetic — the `mem` effect rides on the enclosing `peek`/`poke`.
    fn index_addr(
        &mut self,
        w: MemWidth,
        base: &Typed,
        idx: &Typed,
        binds: &mut Vec<Bind>,
    ) -> Atom {
        let b = self.atom(base, binds);
        let i = self.atom(idx, binds);
        let stride = w.bytes();
        let off = if stride == 1 {
            i
        } else {
            self.bind(
                Comp::Bin(BinOp::Mul, i, Atom::Int(stride)),
                Row::pure(),
                binds,
            )
        };
        self.bind(Comp::Bin(BinOp::Add, b, off), Row::pure(), binds)
    }

    /// Normalize `e` into a complete block: gather its prerequisite bindings,
    /// then a tail computation.
    fn block(&mut self, e: &Typed) -> Ir {
        let mut binds = Vec::new();
        let (tail, row) = self.comp(e, &mut binds);
        let mut ir = Ir::Ret { row, comp: tail };
        for b in binds.into_iter().rev() {
            ir = Ir::Let {
                name: b.name,
                ty: b.ty,
                layout: b.layout,
                row: b.row,
                comp: b.comp,
                rest: Box::new(ir),
            };
        }
        ir
    }

    /// The Ir block for one `match` arm: bind each field of the matched
    /// constructor (a projection from the scrutinee `s`), then the arm's body.
    fn match_arm_block(&mut self, s: &Atom, arm: &crate::sema::MatchArmT) -> Ir {
        let mut binds = Vec::new();
        for (name, slot, layout, ty) in &arm.binds {
            binds.push(Bind {
                name: name.clone(),
                ty: ty.clone(),
                layout: *layout,
                row: Row::pure(),
                comp: Comp::Proj {
                    tup: s.clone(),
                    slot: *slot,
                    layout: *layout,
                    ty: ty.clone(),
                },
            });
        }
        let (tail, row) = self.comp(&arm.body, &mut binds);
        let mut ir = Ir::Ret { row, comp: tail };
        for b in binds.into_iter().rev() {
            ir = Ir::Let {
                name: b.name,
                ty: b.ty,
                layout: b.layout,
                row: b.row,
                comp: b.comp,
                rest: Box::new(ir),
            };
        }
        ir
    }

    /// Normalize `e` to a single `Comp`, appending any prerequisite bindings
    /// (in evaluation order) to `binds`. Returns the comp and the row it
    /// performs **at this step** — `e`'s row minus whatever its operands
    /// hoisted into fresh bindings (those effects now show on their own lines).
    fn comp(&mut self, e: &Typed, binds: &mut Vec<Bind>) -> (Comp, Row) {
        let start = binds.len();
        let c = match &e.node {
            // A read of a mutable local is a slot LOAD, not an SSA reference; any
            // other `Var` is a plain atom. (`mut_slots` is scoped per `LetMut`.)
            Node::Var(x) if self.mut_slots.contains(x) => Comp::SlotLoad(x.clone()),
            Node::Var(x) => Comp::Atom(Atom::Var(x.clone())),
            Node::Int(n) => Comp::Atom(Atom::Int(*n)),
            Node::Float(bits) => Comp::Atom(Atom::Float(*bits)),
            Node::Bool(b) => Comp::Atom(Atom::Bool(*b)),
            Node::Unit => Comp::Atom(Atom::Unit),
            Node::Str(s) => Comp::Atom(Atom::Str(s.clone())),
            // Representation coercion at a `Var` boundary (T3 lowering, gated by
            // T0). `Tag` shifts a concrete scalar into a uniform word (`value<<2`,
            // i62-trapped at codegen); `Untag` shifts it back (`value>>2`). The
            // inner value is named first, so the coercion operates on an atom. The
            // **passthrough** (`Var`→`Var`) never reaches here — sema inserts no
            // coercion for it (a verbatim word copy), so there is no `Coerce` node.
            Node::Coerce { kind, inner, .. } => {
                let a = self.atom(inner, binds);
                match kind {
                    crate::syntax::Coercion::Tag => Comp::Tag(a),
                    crate::syntax::Coercion::Untag => Comp::Untag(a),
                    crate::syntax::Coercion::ToPtr => Comp::ToPtr(a),
                    crate::syntax::Coercion::FromPtr => Comp::FromPtr(a),
                    // `None` is never built as a `Coerce` node (sema only emits
                    // Tag/Untag/ToPtr/FromPtr); reaching here is a layering bug.
                    crate::syntax::Coercion::None => {
                        unreachable!("Node::Coerce carrying Coercion::None reached lowering")
                    }
                }
            }

            Node::Lam {
                param,
                param_ty,
                body,
            } => Comp::Lam {
                param: param.clone(),
                param_ty: Some(param_ty.clone()),
                param_layout: storage_layout_or_panic(param_ty),
                ret_ty: body.ty.clone(),
                body: Box::new(self.block(body)),
            },

            Node::App { fun, arg } => {
                // Collect the application spine; if its head is a fully-applied
                // extern, emit one foreign call rather than a curried chain.
                let mut spine: Vec<&Typed> = Vec::new();
                let head = collect_spine(e, &mut spine);
                let ext = match &head.node {
                    Node::Extern(sym, abi) => Some((sym.clone(), abi.clone())),
                    Node::Var(x) => self.externs.get(x).cloned(),
                    _ => None,
                };
                match ext {
                    // a fully-applied foreign call — nullary `f ()` (one elided
                    // `Unit`, no params) or an n-ary call matching the arity.
                    Some((sym, abi)) if foreign_applies(&abi, spine.len()) => {
                        let mut atoms = Vec::with_capacity(abi.params.len());
                        if !abi.params.is_empty() {
                            for &a in &spine {
                                atoms.push(self.atom(a, binds));
                            }
                        }
                        Comp::Foreign(sym, atoms, abi)
                    }
                    // not an extern, or a partial / over-application: an
                    // ordinary curried application.
                    _ => {
                        let f = self.atom(fun, binds);
                        let a = self.atom(arg, binds);
                        Comp::App {
                            fun: f,
                            arg: a,
                            arg_ty: arg.ty.clone(),
                            ret_ty: e.ty.clone(),
                        }
                    }
                }
            }

            Node::Extern(sym, abi) => Comp::Extern(sym.clone(), abi.params.len()),

            Node::Bin(op, a, b) => {
                let av = self.atom(a, binds);
                let bv = self.atom(b, binds);
                if let Type::Vector(shape, elem) = &e.ty {
                    Comp::VectorBin {
                        op: *op,
                        shape: *shape,
                        elem_ty: (**elem).clone(),
                        lhs: av,
                        rhs: bv,
                    }
                } else if let Type::Mask(shape) = &e.ty {
                    let elem_ty = match &a.ty {
                        Type::Vector(_, elem) => (**elem).clone(),
                        other => panic!("vector comparison has non-vector lhs type {other:?}"),
                    };
                    Comp::VectorCompare {
                        op: *op,
                        shape: *shape,
                        elem_ty,
                        lhs: av,
                        rhs: bv,
                    }
                } else if a.ty == Type::Float || b.ty == Type::Float {
                    Comp::FloatBin(*op, av, bv)
                } else {
                    Comp::Bin(*op, av, bv)
                }
            }
            Node::Cast(op, a) => {
                let av = self.atom(a, binds);
                Comp::Cast(*op, av)
            }
            Node::FloatMathUnary(op, value) => {
                let ty = value.ty.clone();
                let value = self.atom(value, binds);
                Comp::FloatMathUnary { op: *op, ty, value }
            }
            Node::FloatMathBinary(op, lhs, rhs) => {
                let lhs_ty = lhs.ty.clone();
                let lhs = self.atom(lhs, binds);
                let rhs = self.atom(rhs, binds);
                Comp::FloatMathBinary {
                    op: *op,
                    ty: lhs_ty,
                    lhs,
                    rhs,
                }
            }
            Node::FloatMathTernary(op, a, b, c) => {
                let a = self.atom(a, binds);
                let b = self.atom(b, binds);
                let c = self.atom(c, binds);
                Comp::FloatMathTernary {
                    op: *op,
                    ty: e.ty.clone(),
                    a,
                    b,
                    c,
                }
            }
            Node::MaskReduce(op, mask) => {
                let shape = match &mask.ty {
                    Type::Mask(shape) => *shape,
                    other => panic!("mask reduction has non-mask type {other:?}"),
                };
                let mask = self.atom(mask, binds);
                Comp::MaskReduce {
                    op: *op,
                    shape,
                    mask,
                }
            }
            Node::VectorSelect {
                mask,
                then_value,
                else_value,
            } => {
                let (shape, elem_ty) = match &e.ty {
                    Type::Vector(shape, elem) => (*shape, (**elem).clone()),
                    other => panic!("vector select has non-vector result type {other:?}"),
                };
                let mask = self.atom(mask, binds);
                let then_value = self.atom(then_value, binds);
                let else_value = self.atom(else_value, binds);
                Comp::VectorSelect {
                    shape,
                    elem_ty,
                    mask,
                    then_value,
                    else_value,
                }
            }
            Node::VectorLit { shape, elems } => {
                let elem_ty = match &e.ty {
                    Type::Vector(_, elem) => (**elem).clone(),
                    other => panic!("vector literal has non-vector type {other:?}"),
                };
                let elems = elems.iter().map(|e| self.atom(e, binds)).collect();
                Comp::VectorLit {
                    shape: *shape,
                    elem_ty,
                    elems,
                }
            }
            Node::VectorSplat { shape, value } => {
                let elem_ty = match &e.ty {
                    Type::Vector(_, elem) => (**elem).clone(),
                    other => panic!("vector splat has non-vector type {other:?}"),
                };
                let value = self.atom(value, binds);
                Comp::VectorSplat {
                    shape: *shape,
                    elem_ty,
                    value,
                }
            }
            // A packed array vector load: the node's `ty` is `Vector(shape, E)`,
            // so the lane element type `E` (which codegen needs to pick the LLVM
            // vector type and the element byte stride) comes straight off it.
            Node::VectorLoad { shape, arr, idx } => {
                let elem_ty = match &e.ty {
                    Type::Vector(_, elem) => (**elem).clone(),
                    other => panic!("vector load has non-vector type {other:?}"),
                };
                let a = self.atom(arr, binds);
                let i = self.atom(idx, binds);
                Comp::VectorLoad {
                    shape: *shape,
                    elem_ty,
                    arr: a,
                    idx: i,
                }
            }
            // A packed array vector store: the node's `ty` is `Unit`, so the lane
            // element type comes from the *array's* element type (`arr : Array[E]`).
            Node::VectorStore {
                shape,
                arr,
                idx,
                value,
            } => {
                let elem_ty = match &arr.ty {
                    Type::Array(elem) => (**elem).clone(),
                    other => panic!("vector store target has non-array type {other:?}"),
                };
                let a = self.atom(arr, binds);
                let i = self.atom(idx, binds);
                let v = self.atom(value, binds);
                Comp::VectorStore {
                    shape: *shape,
                    elem_ty,
                    arr: a,
                    idx: i,
                    value: v,
                }
            }
            Node::VectorExtract { vector, lane } => {
                let elem_ty = e.ty.clone();
                let vector = self.atom(vector, binds);
                Comp::VectorExtract {
                    vector,
                    lane: *lane,
                    elem_ty,
                }
            }

            // The condition is named; each branch becomes its own block.
            Node::If(c, t, e) => {
                let cv = self.atom(c, binds);
                let tb = self.block(t);
                let eb = self.block(e);
                Comp::If(cv, Box::new(tb), Box::new(eb))
            }

            Node::Loop {
                vars,
                cond,
                steps,
                result,
            } => {
                let vars = vars
                    .iter()
                    .map(|(name, ty, layout, init)| LoopVar {
                        name: name.clone(),
                        ty: ty.clone(),
                        layout: *layout,
                        init: self.atom(init, binds),
                    })
                    .collect();
                let cond = Box::new(self.block(cond));
                let steps = steps.iter().map(|step| self.block(step)).collect();
                let result = Box::new(self.block(result));
                Comp::Loop {
                    vars,
                    cond,
                    steps,
                    result,
                }
            }

            Node::Perform { label, arg } => {
                let a = self.atom(arg, binds);
                Comp::Perform(label.clone(), a)
            }

            Node::Handle { scrutinee, handler } => Comp::Handle {
                stage: e.stage,
                scrutinee: Box::new(self.block(scrutinee)),
                handler: self.handler(handler),
            },

            Node::Quote(b) => Comp::Quote(Box::new(self.block(b))),
            Node::Splice(b) => {
                let a = self.atom(b, binds);
                Comp::Splice(a)
            }
            Node::Genlet(b) => {
                let a = self.atom(b, binds);
                Comp::Genlet(a)
            }
            Node::Letloc(b) => Comp::Letloc(Box::new(self.block(b))),

            Node::Peek(w, addr) => {
                let a = self.atom(addr, binds);
                Comp::Peek(*w, a)
            }
            Node::Poke(w, addr, val) => {
                let a = self.atom(addr, binds);
                let v = self.atom(val, binds);
                Comp::Poke(*w, a, v)
            }
            Node::Fill(dst, byte, count) => {
                let d = self.atom(dst, binds);
                let b = self.atom(byte, binds);
                let n = self.atom(count, binds);
                Comp::Fill(d, b, n)
            }
            Node::Copy(dst, src, count) => {
                let d = self.atom(dst, binds);
                let s = self.atom(src, binds);
                let n = self.atom(count, binds);
                Comp::Copy(d, s, n)
            }

            // The array accessor is pure SUGAR over the `mem` primitives: compute
            // the byte address `base + i*stride` (the scaling that would not
            // type-check at the surface — `Str + Int` — happens here on the
            // already-i64 address), then `peek`/`poke` at the element width.
            Node::Index(w, base, idx) => {
                let addr = self.index_addr(*w, base, idx, binds);
                Comp::Peek(*w, addr)
            }
            Node::IndexSet(w, base, idx, val) => {
                let addr = self.index_addr(*w, base, idx, binds);
                let v = self.atom(val, binds);
                Comp::Poke(*w, addr, v)
            }

            Node::Tuple(es) => {
                let fields = es
                    .iter()
                    .map(|e| (self.atom(e, binds), storage_layout_or_panic(&e.ty)))
                    .collect();
                Comp::Tuple(fields)
            }

            // `let (x1, …, xn) = e in body` — bind the tuple, then bind each name
            // to a projection; the block's tail is the *body's* tail.
            Node::LetTuple(names, e, body) => {
                let elem_tys: Vec<_> = match &e.ty {
                    crate::syntax::Type::Tuple(ts) => ts.clone(),
                    // A type variable must have been zonked away before lowering
                    // (D6); reaching IR with one is a zonk-ordering bug.
                    crate::syntax::Type::Var(v) => {
                        unreachable!("let-tuple on an un-zonked type variable {v:?}")
                    }
                    _ => unreachable!("let-tuple on a non-tuple (sema checked)"),
                };
                let tup = self.atom(e, binds);
                for (i, name) in names.iter().enumerate() {
                    let (slot, layout) = field_layout(&elem_tys, i);
                    binds.push(Bind {
                        name: name.clone(),
                        ty: elem_tys[i].clone(),
                        layout,
                        row: Row::pure(),
                        comp: Comp::Proj {
                            tup: tup.clone(),
                            slot,
                            layout,
                            ty: elem_tys[i].clone(),
                        },
                    });
                }
                return self.comp(body, binds);
            }

            // A record is a tuple of its (sorted) field values; field access is a
            // projection at the field's sorted slot, resolved from `r`'s type.
            Node::Record(fields) => {
                let flds = fields
                    .iter()
                    .map(|(_, v)| (self.atom(v, binds), storage_layout_or_panic(&v.ty)))
                    .collect();
                Comp::Tuple(flds)
            }
            Node::Field(r, name) => {
                let (slot, layout) = match &r.ty {
                    crate::syntax::Type::Record(fs) => {
                        let j = fs
                            .iter()
                            .position(|(n, _)| n == name)
                            .expect("field exists (sema checked)");
                        let tys: Vec<_> = fs.iter().map(|(_, t)| t.clone()).collect();
                        field_layout(&tys, j)
                    }
                    // Zonked away before lowering (D6); a Var here is a bug.
                    crate::syntax::Type::Var(v) => {
                        unreachable!("field access on an un-zonked type variable {v:?}")
                    }
                    _ => unreachable!("field access on a non-record (sema checked)"),
                };
                let rv = self.atom(r, binds);
                // `e.ty` is the field's (projected) type — the value this `Proj`
                // yields, which codegen needs to reassemble a multi-cell vector.
                Comp::Proj {
                    tup: rv,
                    slot,
                    layout,
                    ty: e.ty.clone(),
                }
            }

            // Arrays lower to their own representation: a logical length plus
            // either traced pointer slots or a byte-strided scalar payload.
            Node::ArrayLit { elems, elem_layout } => {
                let elems = elems.iter().map(|e| self.atom(e, binds)).collect();
                Comp::ArrayLit {
                    elems,
                    elem_layout: *elem_layout,
                }
            }
            Node::Len(a) => {
                let av = self.atom(a, binds);
                Comp::Len(av)
            }
            Node::ArrayGet {
                arr,
                idx,
                elem_layout,
            } => {
                let a = self.atom(arr, binds);
                let i = self.atom(idx, binds);
                // `e.ty` is the element type read out — codegen reassembles a
                // multi-cell vector element from it (`Array[Quad[Float32]]`).
                Comp::ArrayGet {
                    arr: a,
                    idx: i,
                    elem_layout: *elem_layout,
                    elem_ty: e.ty.clone(),
                }
            }
            Node::ArraySet {
                arr,
                idx,
                val,
                elem_layout,
            } => {
                let a = self.atom(arr, binds);
                let i = self.atom(idx, binds);
                let v = self.atom(val, binds);
                Comp::ArraySet {
                    arr: a,
                    idx: i,
                    val: v,
                    elem_layout: *elem_layout,
                }
            }

            // A constructor value is a tagged heap struct: the tag is an extra
            // leading scalar (field 0 after the field_layout reorders pointers
            // first), then the args. So it reuses the tuple alloc.
            Node::Construct { tag, args } => {
                let mut fields = vec![(Atom::Int(*tag), ValueLayout::scalar_cell())];
                for (a, layout, _slot) in args {
                    fields.push((self.atom(a, binds), *layout));
                }
                Comp::Tuple(fields)
            }

            // `match` lowers to: read the tag (scalar 0), then an `if`-chain that
            // tests each constructor's tag, binding its fields (projections) in
            // the chosen arm. The default (a wildcard, or the last arm of an
            // exhaustive match) is the final `else` — no test.
            Node::Match { scrutinee, arms } => {
                let s = self.atom(scrutinee, binds);
                let default_idx = arms
                    .iter()
                    .position(|a| a.tag.is_none())
                    .unwrap_or(arms.len() - 1);
                let cond_arms = &arms[..default_idx];
                let default_arm = &arms[default_idx];

                if cond_arms.is_empty() {
                    // Only the default can match: bind its fields here, be its body.
                    for (name, slot, layout, ty) in &default_arm.binds {
                        binds.push(Bind {
                            name: name.clone(),
                            ty: ty.clone(),
                            layout: *layout,
                            row: Row::pure(),
                            comp: Comp::Proj {
                                tup: s.clone(),
                                slot: *slot,
                                layout: *layout,
                                ty: ty.clone(),
                            },
                        });
                    }
                    return self.comp(&default_arm.body, binds);
                }

                let tag = self.bind(
                    Comp::Proj {
                        tup: s.clone(),
                        slot: 0,
                        layout: ValueLayout::scalar_cell(),
                        // The constructor tag is a single-cell `Int` scalar.
                        ty: Type::Int,
                    },
                    Row::pure(),
                    binds,
                );
                let row = e.row.clone();
                // Build the else-chain inside-out; arm 0 becomes the outer `if`.
                let mut acc: Ir = self.match_arm_block(&s, default_arm);
                for arm in cond_arms[1..].iter().rev() {
                    let then_ir = self.match_arm_block(&s, arm);
                    let armtag = arm.tag.expect("conditional arm has a tag");
                    let cond = self.bind(
                        Comp::Bin(BinOp::Eq, tag.clone(), Atom::Int(armtag)),
                        Row::pure(),
                        binds,
                    );
                    acc = Ir::Ret {
                        row: row.clone(),
                        comp: Comp::If(cond, Box::new(then_ir), Box::new(acc)),
                    };
                }
                let a0 = &cond_arms[0];
                let then0 = self.match_arm_block(&s, a0);
                let cond0 = self.bind(
                    Comp::Bin(BinOp::Eq, tag.clone(), Atom::Int(a0.tag.expect("tag"))),
                    Row::pure(),
                    binds,
                );
                Comp::If(cond0, Box::new(then0), Box::new(acc))
            }

            // `let name = bound in body` flattens straight into the binding
            // list; the block's tail is the *body's* tail.
            Node::Let { name, bound, body } => {
                // Propagate an extern marker through a `let`-ALIAS: `let pow =
                // crt_pow in …` stays marker-only, so a later `pow a b` still
                // collapses to ONE foreign call. Without this the alias binds `pow`
                // to the dropped extern value (`crt_pow` lowers to no value) and
                // calls it indirectly — a miscompile. The sharp CRT-service trick:
                // an alias of an extern emits no binding, just forwards the entry.
                if let Node::Var(x) = &bound.node {
                    if let Some(entry) = self.externs.get(x).cloned() {
                        self.externs.insert(name.clone(), entry);
                        return self.comp(body, binds);
                    }
                }
                let (cb, rb) = self.comp(bound, binds);
                // Track foreign-function bindings so a later call through `name`
                // is recognised as an extern spine (with its native ABI).
                if let Node::Extern(sym, abi) = &bound.node {
                    self.externs
                        .insert(name.clone(), (sym.clone(), abi.clone()));
                }
                binds.push(Bind {
                    name: name.clone(),
                    ty: bound.ty.clone(),
                    layout: storage_layout_or_panic(&bound.ty),
                    row: rb,
                    comp: cb,
                });
                return self.comp(body, binds);
            }

            // `let mut name = bound in body` — a non-escaping scalar mutable
            // local (mutability v1). Lower `bound` to an atom, allocate a stack
            // slot for `name` (a `SlotInit` bind: codegen does the `alloca` +
            // initial `store`), mark `name` mutable so reads in `body` route to a
            // `SlotLoad`, then lower `body` (whose tail is this expression's
            // value). `mut_slots` is saved/restored so the binding's scope ends
            // with the body and a shadowed outer `name` is undisturbed.
            Node::LetMut { name, bound, body } => {
                let init = self.atom(bound, binds);
                binds.push(Bind {
                    name: self.fresh(),
                    ty: Type::Unit,
                    layout: ValueLayout::scalar_cell(),
                    row: Row::pure(),
                    comp: Comp::SlotInit(name.clone(), init),
                });
                // `insert` returns true when newly added (no shadowed outer
                // mutable `name`); only then do we remove it when the scope ends.
                let newly_added = self.mut_slots.insert(name.clone());
                let out = self.comp(body, binds);
                if newly_added {
                    self.mut_slots.remove(name);
                }
                return out;
            }

            // `name := value` — a stack store into the mutable local's slot,
            // yielding `Unit`. `value` is lowered to an atom first (evaluation
            // order), then stored.
            Node::Assign { name, value } => {
                let v = self.atom(value, binds);
                Comp::SlotStore(name.clone(), v)
            }

            // `ref e` — allocate a one-field heap cell holding `e`. Lower `e` to an
            // atom (its scalar value), then a `RefNew` (codegen: `locus_gc_alloc`
            // one scalar cell + `set_scalar` slot 0), exactly the one-field-tuple
            // alloc path. The content cell's layout is `e`'s storage layout, taken
            // from the **zonked** `value.ty` here (a `Var` is resolved to its scalar
            // by now — never the early word-cell layout). The result is the cell's
            // handle. (`! {gc}`.)
            Node::RefNew { value } => {
                let layout = storage_layout_or_panic(&value.ty);
                let v = self.atom(value, binds);
                Comp::RefNew(v, layout)
            }

            // `!r` — read the cell: resolve the handle atom, then a `RefGet`
            // (codegen: `get_scalar` slot 0), like a `Proj` of the one-field object.
            // The content cell's layout is this node's (zonked) result type — the
            // content type `T`.
            Node::Deref { cell } => {
                let layout = storage_layout_or_panic(&e.ty);
                let r = self.atom(cell, binds);
                Comp::RefGet(r, layout)
            }

            // `r := v` — write the cell: resolve the handle atom and the value atom
            // (in evaluation order — the handle first), then a `RefSet` (codegen:
            // `set_scalar` slot 0). Yields `Unit`. No write barrier (scalar cell).
            // The content cell's layout is the written value's (zonked) type.
            Node::RefAssign { target, value } => {
                let layout = storage_layout_or_panic(&value.ty);
                let r = self.atom(target, binds);
                let v = self.atom(value, binds);
                Comp::RefSet(r, v, layout)
            }
        };
        // This step's own effects: `e`'s row minus what the operands hoisted.
        // (For block-bodied comps nothing is hoisted here, so `own == e.row`.)
        let mut hoisted = Row::pure();
        for b in &binds[start..] {
            hoisted = hoisted.union(&b.row);
        }
        let labels: Vec<Label> = hoisted.labels().cloned().collect();
        (c, e.row.without(&labels))
    }

    /// Normalize `e` to an **atom**, binding it to a fresh name first if it is
    /// not already trivial.
    fn atom(&mut self, e: &Typed, binds: &mut Vec<Bind>) -> Atom {
        match &e.node {
            // A read of a mutable local is NOT trivial — it is a slot `load`,
            // ordered against the slot's stores — so it falls through to the
            // general arm, which names the `SlotLoad` with a fresh binding.
            Node::Var(x) if self.mut_slots.contains(x) => {
                let name = self.fresh();
                binds.push(Bind {
                    name: name.clone(),
                    ty: e.ty.clone(),
                    layout: storage_layout_or_panic(&e.ty),
                    row: Row::pure(),
                    comp: Comp::SlotLoad(x.clone()),
                });
                Atom::Var(name)
            }
            Node::Var(x) => Atom::Var(x.clone()),
            Node::Int(n) => Atom::Int(*n),
            Node::Float(bits) => Atom::Float(*bits),
            Node::Bool(b) => Atom::Bool(*b),
            Node::Unit => Atom::Unit,
            Node::Str(s) => Atom::Str(s.clone()),
            _ => {
                let (c, row) = self.comp(e, binds);
                let name = self.fresh();
                binds.push(Bind {
                    name: name.clone(),
                    ty: e.ty.clone(),
                    layout: storage_layout_or_panic(&e.ty),
                    row,
                    comp: c,
                });
                Atom::Var(name)
            }
        }
    }

    fn handler(&mut self, h: &TypedHandler) -> IrHandler {
        let mut ops = Vec::with_capacity(h.ops.len());
        for c in &h.ops {
            let body = Box::new(self.block(&c.body));
            ops.push(IrOpClause {
                op: c.op.clone(),
                arg: c.arg.clone(),
                arg_ty: c.arg_ty.clone(),
                arg_layout: c.arg_layout,
                resume: c.resume.clone(),
                resume_ty: c.resume_ty.clone(),
                resume_layout: c.resume_layout,
                body,
            });
        }
        let ret = IrReturn {
            var: h.ret.var.clone(),
            var_ty: h.ret.var_ty.clone(),
            var_layout: h.ret.var_layout,
            body_ty: h.ret.body_ty.clone(),
            body: Box::new(self.block(&h.ret.body)),
        };
        IrHandler { ops, ret }
    }
}

// ── Rendering ────────────────────────────────────────────────────────────

fn atom_str(a: &Atom) -> String {
    match a {
        Atom::Var(x) => x.clone(),
        Atom::Int(n) => n.to_string(),
        Atom::Float(bits) => f64::from_bits(*bits).to_string(),
        Atom::Bool(b) => b.to_string(),
        Atom::Unit => "()".into(),
        Atom::Str(s) => format!("{s:?}"),
    }
}

impl Ir {
    /// An indented ANF listing — each `let` line tagged with its effect row.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        write_ir(&mut s, self, 0);
        s
    }

    /// Machine-readable wrapper (schema `locus-ir/1`) carrying the listing.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"schema\":\"locus-ir/1\",\"ok\":true,\"ir\":\"{}\"}}",
            crate::diag::esc(&self.to_text())
        )
    }
}

fn write_ir(s: &mut String, ir: &Ir, depth: usize) {
    match ir {
        Ir::Let {
            name,
            ty: _,
            layout: _,
            row,
            comp,
            rest,
        } => {
            write_comp(s, &format!("let {name} = "), row, comp, depth);
            write_ir(s, rest, depth);
        }
        Ir::Ret { row, comp } => write_comp(s, "", row, comp, depth),
    }
}

fn layout_suffix(layout: ValueLayout) -> String {
    if layout.is_word_cell() {
        // A repr-poly `Var` word cell: traced like a pointer but stored verbatim.
        // Rendered distinctly from a real pointer (`*`) so the dump shows where the
        // tag/word matrix applies (`set_word`/`get_word`, not `set_ptr`).
        "~".to_string()
    } else if layout.is_single_pointer_cell() {
        "*".to_string()
    } else if layout.is_single_scalar_cell() {
        if layout.byte_width == 8 && layout.align == 8 {
            String::new()
        } else {
            format!("[{}B]", layout.byte_width)
        }
    } else if !layout.known {
        "?".to_string()
    } else {
        format!(
            "[p{}s{}:{}B]",
            layout.pointer_cells, layout.scalar_cells, layout.byte_width
        )
    }
}

fn write_comp(s: &mut String, prefix: &str, row: &Row, comp: &Comp, depth: usize) {
    let pad = "  ".repeat(depth);
    let tag = if row.is_pure() {
        String::new()
    } else {
        format!("   ! {row}")
    };
    let mut line = |body: String| s.push_str(&format!("{pad}{prefix}{body}{tag}\n"));
    match comp {
        Comp::Atom(a) => line(atom_str(a)),
        Comp::App { fun, arg, .. } => line(format!("{} {}", atom_str(fun), atom_str(arg))),
        Comp::Bin(op, a, b) => line(format!("{} {} {}", atom_str(a), op.symbol(), atom_str(b))),
        Comp::FloatBin(op, a, b) => line(format!(
            "float {} {} {}",
            atom_str(a),
            op.symbol(),
            atom_str(b)
        )),
        Comp::Cast(op, a) => line(format!("{} {}", op.symbol(), atom_str(a))),
        Comp::Tag(a) => line(format!("tag {}", atom_str(a))),
        Comp::Untag(a) => line(format!("untag {}", atom_str(a))),
        Comp::ToPtr(a) => line(format!("to_ptr {}", atom_str(a))),
        Comp::FromPtr(a) => line(format!("from_ptr {}", atom_str(a))),
        Comp::FloatMathUnary { op, value, .. } => {
            line(format!("{} {}", op.symbol(), atom_str(value)))
        }
        Comp::FloatMathBinary { op, lhs, rhs, .. } => line(format!(
            "{}({}, {})",
            op.symbol(),
            atom_str(lhs),
            atom_str(rhs)
        )),
        Comp::FloatMathTernary { op, a, b, c, .. } => line(format!(
            "{}({}, {}, {})",
            op.symbol(),
            atom_str(a),
            atom_str(b),
            atom_str(c)
        )),
        Comp::VectorLit { shape, elems, .. } => {
            let a = elems.iter().map(atom_str).collect::<Vec<_>>().join(", ");
            line(format!("{}({a})", shape.name()))
        }
        Comp::VectorSplat { shape, value, .. } => {
            line(format!("splat{} {}", shape.name(), atom_str(value)))
        }
        Comp::VectorLoad { shape, arr, idx, .. } => line(format!(
            "load{}({}, {})",
            shape.name(),
            atom_str(arr),
            atom_str(idx)
        )),
        Comp::VectorStore {
            shape,
            arr,
            idx,
            value,
            ..
        } => line(format!(
            "store{}({}, {}, {})",
            shape.name(),
            atom_str(arr),
            atom_str(idx),
            atom_str(value)
        )),
        Comp::VectorBin { op, lhs, rhs, .. } => line(format!(
            "vec {} {} {}",
            atom_str(lhs),
            op.symbol(),
            atom_str(rhs)
        )),
        Comp::VectorCompare { op, lhs, rhs, .. } => line(format!(
            "mask {} {} {}",
            atom_str(lhs),
            op.symbol(),
            atom_str(rhs)
        )),
        Comp::VectorSelect {
            mask,
            then_value,
            else_value,
            ..
        } => line(format!(
            "select({}, {}, {})",
            atom_str(mask),
            atom_str(then_value),
            atom_str(else_value)
        )),
        Comp::MaskReduce { op, mask, .. } => line(format!("{} {}", op.symbol(), atom_str(mask))),
        Comp::VectorExtract { vector, lane, .. } => {
            line(format!("{}.lane{}", atom_str(vector), lane))
        }
        Comp::Extern(sym, _) => line(format!("extern {sym:?}")),
        Comp::Foreign(sym, args, _) => {
            let a = args.iter().map(atom_str).collect::<Vec<_>>().join(", ");
            line(format!("foreign {sym:?}({a})"))
        }
        Comp::Perform(l, a) => line(format!("perform {l} {}", atom_str(a))),
        Comp::Splice(a) => line(format!("splice {}", atom_str(a))),
        Comp::Genlet(a) => line(format!("genlet {}", atom_str(a))),
        Comp::Peek(w, a) => line(format!("peek{} {}", w.bits(), atom_str(a))),
        Comp::Poke(w, a, v) => line(format!("poke{} {} {}", w.bits(), atom_str(a), atom_str(v))),
        Comp::Fill(d, b, n) => line(format!(
            "fill {} {} {}",
            atom_str(d),
            atom_str(b),
            atom_str(n)
        )),
        Comp::Copy(d, s, n) => line(format!(
            "copy {} {} {}",
            atom_str(d),
            atom_str(s),
            atom_str(n)
        )),
        Comp::Tuple(fields) => {
            let parts: Vec<_> = fields
                .iter()
                .map(|(a, layout)| format!("{}{}", atom_str(a), layout_suffix(*layout)))
                .collect();
            line(format!("({})", parts.join(", ")))
        }
        Comp::ArrayLit { elems, elem_layout } => {
            let parts: Vec<_> = elems.iter().map(atom_str).collect();
            line(format!(
                "[{}]{}",
                parts.join(", "),
                layout_suffix(*elem_layout)
            ))
        }
        Comp::Proj {
            tup, slot, layout, ..
        } => line(format!(
            "{}.{}{}",
            atom_str(tup),
            slot,
            layout_suffix(*layout)
        )),
        Comp::Len(a) => line(format!("len {}", atom_str(a))),
        Comp::ArrayGet {
            arr,
            idx,
            elem_layout,
            ..
        } => line(format!(
            "{}[{}]{}",
            atom_str(arr),
            atom_str(idx),
            layout_suffix(*elem_layout)
        )),
        Comp::ArraySet {
            arr,
            idx,
            val,
            elem_layout,
        } => line(format!(
            "{}[{}] <- {}{}",
            atom_str(arr),
            atom_str(idx),
            atom_str(val),
            layout_suffix(*elem_layout)
        )),
        Comp::SlotInit(name, init) => line(format!("slot {name} = {}", atom_str(init))),
        Comp::SlotLoad(name) => line(format!("load {name}")),
        Comp::SlotStore(name, val) => line(format!("{name} := {}", atom_str(val))),
        Comp::RefNew(init, layout) => {
            line(format!("ref {}{}", atom_str(init), layout_suffix(*layout)))
        }
        Comp::RefGet(r, _) => line(format!("!{}", atom_str(r))),
        Comp::RefSet(r, val, _) => line(format!("{} := {}", atom_str(r), atom_str(val))),
        // block-bodied comps: a head line, then the indented sub-block(s).
        Comp::If(cond, then, els) => {
            s.push_str(&format!("{pad}{prefix}if {}{tag}\n", atom_str(cond)));
            write_ir(s, then, depth + 1);
            s.push_str(&format!("{pad}else\n"));
            write_ir(s, els, depth + 1);
        }
        Comp::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            let header = vars
                .iter()
                .map(|v| format!("{} = {}", v.name, atom_str(&v.init)))
                .collect::<Vec<_>>()
                .join(", ");
            s.push_str(&format!("{pad}{prefix}loop {header}{tag}\n"));
            s.push_str(&format!("{pad}while\n"));
            write_ir(s, cond, depth + 1);
            s.push_str(&format!("{pad}do\n"));
            for step in steps {
                write_ir(s, step, depth + 1);
            }
            s.push_str(&format!("{pad}else\n"));
            write_ir(s, result, depth + 1);
        }
        Comp::Lam { param, body, .. } => {
            s.push_str(&format!("{pad}{prefix}lam {param} =>{tag}\n"));
            write_ir(s, body, depth + 1);
        }
        Comp::Quote(body) => {
            s.push_str(&format!("{pad}{prefix}quote{tag}\n"));
            write_ir(s, body, depth + 1);
        }
        Comp::Letloc(body) => {
            s.push_str(&format!("{pad}{prefix}letloc{tag}\n"));
            write_ir(s, body, depth + 1);
        }
        Comp::Handle {
            stage,
            scrutinee,
            handler,
        } => {
            s.push_str(&format!("{pad}{prefix}handle @{stage}{tag}\n"));
            write_ir(s, scrutinee, depth + 1);
            let p = "  ".repeat(depth + 1);
            for op in &handler.ops {
                s.push_str(&format!(
                    "{p}op {}({}) resume {} =>\n",
                    op.op, op.arg, op.resume
                ));
                write_ir(s, &op.body, depth + 2);
            }
            s.push_str(&format!("{p}return {} =>\n", handler.ret.var));
            write_ir(s, &handler.ret.body, depth + 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{elaborate, Ctx, Sig};

    fn ir_text(src: &str, stage: crate::Stage) -> String {
        let term = crate::parse(src).unwrap();
        let tree = elaborate(&Sig::new(), &Ctx::new(), stage, &term).unwrap();
        lower(&tree).to_text()
    }

    #[test]
    fn application_names_the_callee_and_tags_the_effect() {
        // (λx. perform console x) ()  ⇒  the lam is named, the call is the
        // tail, and the tail carries the residual {console}.
        let txt = ir_text("(fn x: Unit => perform console x) ()", 0);
        assert!(txt.contains("lam x =>"), "got:\n{txt}");
        assert!(txt.contains("perform console x"), "got:\n{txt}");
        assert!(
            txt.contains("! {console}"),
            "the call's effect is visible:\n{txt}"
        );
    }

    #[test]
    fn let_mut_lowers_to_a_slot_with_loads_and_a_store() {
        // `let mut x = 1 in let _ = x := x + 41 in x` — the mutable local becomes a
        // stack slot (`slot x = …`); the read in `x + 41` is a `load x`; the
        // assignment is a `x := …` store; and the trailing read of `x` is a load.
        let txt = ir_text("let mut x = 1 in let _ = x := x + 41 in x", 0);
        assert!(txt.contains("slot x = 1"), "slot init:\n{txt}");
        assert!(txt.contains("load x"), "reads are slot loads:\n{txt}");
        assert!(txt.contains("x := "), "assignment is a slot store:\n{txt}");
        // The read never appears as a bare SSA reference to `x` on a tail line.
        assert!(
            !txt.lines().any(|l| l.trim() == "x"),
            "a mutable read must be a `load`, not an SSA `x`:\n{txt}"
        );
    }

    #[test]
    fn ref_ops_lower_to_a_heap_cell_with_get_and_set() {
        // `let r = ref 0 in let _ = (r := !r + 41) in !r` — `ref 0` allocates a
        // one-field heap cell (`ref 0`), the write is a `r := …` (set_scalar slot
        // 0), and the reads are `!r` (get_scalar slot 0). The `gc` (alloc) and `st`
        // (read/write) effects show on their lines.
        let txt = ir_text("let r = ref 0 in let _ = (r := !r + 41) in !r", 0);
        assert!(txt.contains("ref 0"), "the alloc is a `ref`:\n{txt}");
        assert!(txt.contains("! {gc}"), "the alloc carries gc:\n{txt}");
        assert!(txt.contains(":= "), "the write is a ref store:\n{txt}");
        assert!(txt.contains("! {st}"), "a read/write carries st:\n{txt}");
        // The deref lowers to `!<handle>` (a heap read), not a bare SSA reference.
        assert!(txt.contains('!'), "reads are heap derefs:\n{txt}");
    }

    #[test]
    fn a_non_atomic_argument_is_let_bound_first() {
        // perform console (perform fs ())  ⇒  the inner perform is named t0,
        // then used as the outer arg. (The ANF essence: operands are atoms.)
        let txt = ir_text("perform console (perform fs ())", 0);
        assert!(txt.contains("let t0 = perform fs ()"), "got:\n{txt}");
        assert!(txt.contains("perform console t0"), "got:\n{txt}");
    }

    #[test]
    fn let_flattens_and_each_binding_shows_its_row() {
        // let a = perform fs () in perform net ()
        let txt = ir_text("let a = perform fs () in perform net ()", 0);
        assert!(txt.contains("let a = perform fs ()"), "got:\n{txt}");
        assert!(txt.contains("! {fs}"), "got:\n{txt}");
        assert!(txt.contains("perform net ()"), "got:\n{txt}");
        assert!(txt.contains("! {net}"), "got:\n{txt}");
    }

    #[test]
    fn a_pure_program_has_no_effect_tags() {
        let txt = ir_text("let id = fn x: Int => x in id 1", 0);
        assert!(!txt.contains('!'), "nothing pure should be tagged:\n{txt}");
    }

    #[test]
    fn accumulator_loop_lowers_to_loop_ir() {
        let txt = ir_text(
            "loop i = 0, acc = 0 while i < 10 do i + 1, acc + i else acc",
            0,
        );
        assert!(txt.contains("loop i = 0, acc = 0"), "got:\n{txt}");
        assert!(txt.contains("while"), "got:\n{txt}");
        assert!(txt.contains("do"), "got:\n{txt}");
        assert!(txt.contains("else"), "got:\n{txt}");
    }

    #[test]
    fn float_arithmetic_uses_a_distinct_ir_node() {
        let txt = ir_text("1.5 + 2.0", 0);
        assert!(
            txt.contains("float 1.5 + 2"),
            "float ops must not lower into integer Bin IR:\n{txt}"
        );
    }

    #[test]
    fn function_values_in_aggregates_are_pointer_fields() {
        let txt = ir_text("let f = fn x: Int => x in (f, 0)", 0);
        assert!(
            txt.contains("(f*, 0)"),
            "function values stored in managed objects must be traced:\n{txt}"
        );
    }

    #[test]
    fn json_carries_the_ir_schema() {
        let term = crate::parse("perform console ()").unwrap();
        let tree = elaborate(&Sig::new(), &Ctx::new(), 0, &term).unwrap();
        let j = lower(&tree).to_json();
        assert!(j.starts_with(r#"{"schema":"locus-ir/1","ok":true,"ir":"#));
    }

    #[test]
    fn a_multi_arg_extern_call_collapses_to_one_foreign() {
        // `f a b c` where f is a 3-arg extern → a single foreign call carrying
        // all three atoms (not a curried chain), tagged with {winapi}.
        let txt = ir_text(
            r#"let f = extern "MulDiv" : I32 -> I32 -> I32 -> I32 in f 10 7 2"#,
            0,
        );
        assert!(txt.contains(r#"foreign "MulDiv"(10, 7, 2)"#), "got:\n{txt}");
        assert!(
            txt.contains("! {winapi}"),
            "the call carries winapi:\n{txt}"
        );
        assert!(
            !txt.contains("f 10"),
            "the extern spine must not stay curried:\n{txt}"
        );
    }

    // ── repr-poly tag lowering (the slice this enables) ──────────────────────

    /// Lower a program *with the stdlib grafted* (so `list_len`/`list_reverse`
    /// and `List`/`Cons`/`Nil` are in scope), to its ANF listing.
    fn ir_text_stdlib(src: &str) -> String {
        let term = crate::stdlib::program(src).expect("parses with stdlib");
        let tree = elaborate(&Sig::new(), &Ctx::new(), 0, &term).expect("type-checks");
        lower(&tree).to_text()
    }

    #[test]
    fn generic_list_len_over_int_lowers() {
        // THE GOAL: a generic `list_len` applied to a concrete `List[Int]` used to
        // panic in lowering with "cannot lower representation-polymorphic layout
        // yet" — the `Var` field of `Cons` had an *unknown* layout. With repr-poly
        // tags it lowers: the `a` field is a traced **word cell**, the scalar
        // elements are **tagged** (`value << 2`) into it, and `list_len` walks the
        // word-cell rest pointers. We assert only that lowering succeeds and the
        // tag is emitted (the exact ANF shape is an internal detail).
        let txt = ir_text_stdlib("list_len (Cons(1, Cons(2, Nil)))");
        assert!(
            txt.contains("tag 1") && txt.contains("tag 2"),
            "each scalar element is tagged into the Var word cell:\n{txt}"
        );
    }

    #[test]
    fn cons_of_a_scalar_tags_into_a_word_cell_field() {
        // `Cons(1, Nil)` directly: the scalar `1` is tagged, and it is stored into
        // the constructor's **word cell** (the `a` slot, rendered `~` — a verbatim
        // traced word, NOT a plain `*` pointer nor a scalar). The recursive `List`
        // rest is a real pointer (`*`). This is the store side of the matrix.
        let txt = ir_text_stdlib("Cons(1, Nil)");
        assert!(txt.contains("tag 1"), "the scalar field is tagged:\n{txt}");
        // The tagged value lands in a `~` word cell — the verbatim/traced store.
        assert!(
            txt.contains('~'),
            "the type-variable field is a word cell (rendered ~):\n{txt}"
        );
    }

    #[test]
    fn generic_list_reverse_over_int_lowers_with_passthrough() {
        // `list_reverse` recurses `Cons(h, acc)` where `h` is read from a word cell
        // and re-stored into a word cell — the **passthrough** (verbatim word copy,
        // neither re-tagged nor unboxed). The earlier scalar insertions still tag.
        // This exercises both the store (tag) and the load+restore (word cell)
        // sides end to end; lowering must succeed.
        let txt = ir_text_stdlib("list_reverse (Cons(1, Cons(2, Nil)))");
        assert!(
            txt.contains("tag 1") && txt.contains("tag 2"),
            "the seed scalars are tagged:\n{txt}"
        );
        // The recursive `Cons(h, acc)` must NOT re-tag the already-Var `h`: a
        // passthrough is a verbatim copy, so there is no `tag h` in the listing.
        assert!(
            !txt.contains("tag h"),
            "a Var->Var passthrough must not re-tag:\n{txt}"
        );
    }
}
