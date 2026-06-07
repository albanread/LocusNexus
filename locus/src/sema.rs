//! Semantic analysis — **the authoritative typed model** (`elaborate`).
//!
//! Type-checking answers one question — *what is this term's type and effect
//! row?* — and throws the rest away. **Sema keeps it all.** [`elaborate`]
//! walks the whole AST once and returns a [`Typed`] tree: *every* subterm
//! decorated with its `type ! row @ stage`. This is the single authoritative
//! model the later phases (IR, codegen) read — they never re-derive types
//! from the raw AST.
//!
//! The judgment is exactly `calculus.md` §2–§4 (each arm cites the same § as
//! before); the only change is that it **records** the decoration at every
//! node instead of discarding it. [`crate::check::infer`] is now a thin
//! projection of this — it runs `elaborate` and keeps the root's decoration.

use std::collections::BTreeSet;

use crate::check::{Binding, Ctx, Scheme, Sig, Stage, TypeErr};
use crate::diag::esc;
use crate::syntax::{
    BinOp, BlockItem, CastOp, Coercion, ExternAbi, FloatMathOp, Handler, Label, MaskReduceOp,
    MemWidth, OpSig, Pattern, Row, RowVarId, Term, TyVarId, Type, ValueLayout, VectorShape,
};
use crate::unify::{self, generalize, instantiate, instantiate_ctor, unify, unify_row, UnifyErr};
use std::collections::HashMap;

/// A **typing demand**: equate `expected` and `found` through unification, in
/// the thread-local store. On a clash it produces the *same* `Mismatch` the
/// monomorphic checker did — the `expected`/`found` are passed verbatim so the
/// error value is byte-for-byte identical (D3/D5). In S1 both sides are always
/// ground, so this succeeds exactly when `expected == found` did.
fn demand_eq(expected: &Type, found: &Type) -> Result<(), TypeErr> {
    unify::with_store(|s| unify(s, expected, found))
        .map_err(|e| wide_or_mismatch(e, expected, found))
}

/// Translate a [`UnifyErr`] into the right [`TypeErr`]. A **`WideTypeVar`** (the
/// D5/T1 kind rejection) becomes [`TypeErr::WideTypeVariable`] — *not* a generic
/// `Mismatch`, which would bury the kind rule's excellent diagnostic under "type
/// mismatch". Every other unify failure stays the byte-for-byte-identical
/// `Mismatch{ expected, found }` the monomorphic checker produced (D3/D5), so no
/// existing diagnostic changes. This is the single highest-value line in T1:
/// without it the kind rule "works" but reports the wrong error.
fn wide_or_mismatch(e: UnifyErr, expected: &Type, found: &Type) -> TypeErr {
    match e {
        UnifyErr::WideTypeVar(_, ty) => TypeErr::WideTypeVariable { ty },
        _ => TypeErr::Mismatch {
            expected: expected.clone(),
            found: found.clone(),
        },
    }
}

/// A **row** typing demand: equate two rows through `unify_row`. Not used by the
/// monomorphic arms (which only `union` rows), but available to S2; kept here so
/// the discipline (`union` accumulates, `unify_row` equates) has one home.
#[allow(dead_code)]
fn demand_row_eq(expected: &Row, found: &Row) -> Result<(), TypeErr> {
    unify::with_store(|s| unify_row(s, expected, found)).map_err(|_| TypeErr::Mismatch {
        // A row clash has no single `Type` to blame; surface the rows as `Code`
        // wrappers so the existing `Mismatch` diagnostic can render them. (Unused
        // in S1; S2 will give rows a first-class error.)
        expected: Type::Code(Box::new(Type::Unit), expected.clone()),
        found: Type::Code(Box::new(Type::Unit), found.clone()),
    })
}

/// A fully **decorated** term: a node plus the judgment `type ! row @ stage`
/// inferred for it. Children are themselves `Typed`, so the whole tree is
/// annotated — nothing is left for a later phase to recompute.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Typed {
    pub ty: Type,
    pub row: Row,
    pub stage: Stage,
    /// Whether this node's pointer/scalar layout decisions are monomorphic.
    ///
    /// The source language can type-check representation-polymorphic code (for
    /// example `fn x => [x]` or `fn x => Cons(x, Nil)`), but the current IR and
    /// LLVM backend need concrete pointer/scalar slots before lowering. We keep
    /// such programs typable and mark the affected nodes so codegen can reject
    /// them until monomorphization or representation-passing exists.
    pub layout_known: bool,
    pub node: Node,
}

/// The decorated node shapes — a mirror of [`Term`] whose children carry their
/// own decoration. (Binders — `param`, `let` name, handler `arg`/`resume` —
/// stay as plain `String`s; only sub*terms* become `Typed`.)
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Node {
    Var(String),
    Int(i64),
    Float(u64),
    Bool(bool),
    Unit,
    /// `brk` — the debug-crash leaf (see [`Term::Brk`]). A diverging leaf with
    /// no children; lowers to a trap.
    Brk,
    Str(String),
    /// `extern "symbol" : T` — a foreign function reference. The declared
    /// widths are erased to `Int` for the value world; the native [`ExternAbi`]
    /// is kept here for codegen's boundary conversion.
    Extern(String, ExternAbi),
    Bin(BinOp, Box<Typed>, Box<Typed>),
    Cast(CastOp, Box<Typed>),
    /// A representation coercion inserted at a polymorphic boundary (see
    /// `docs/repr-poly-impl.md`): **tag** a scalar into a uniform word cell
    /// (`value << 2`), or **untag** it back (`value >> 2`). `kind` is
    /// `Coercion::Tag` or `Coercion::Untag` (never `None`). Lowered by T3 (the
    /// shift, with an i62 overflow trap on `Tag`); unconstructed until sema
    /// inserts them.
    ///
    /// `slot` and `value` record the **declared boundary types** sema computed
    /// the coercion from — `slot` the declared slot type (a `Type::Var` at a
    /// polymorphic position, kept un-resolved so its `repr()` reads `Uniform`),
    /// `value` the value's resolved type. They are the audit trail the T0
    /// tag-completeness validator ([`crate::tagcheck`]) re-checks: a `Coerce` is
    /// correct iff `Type::coercion(slot, value) == kind`. Carrying them on the
    /// node — rather than re-deriving from a registry that is gone by zonk time —
    /// keeps sema the authoritative model, and is the slot/value pair T2's
    /// three-way matrix lowering will consume.
    Coerce {
        kind: Coercion,
        slot: Type,
        value: Type,
        inner: Box<Typed>,
    },
    FloatMathUnary(FloatMathOp, Box<Typed>),
    FloatMathBinary(FloatMathOp, Box<Typed>, Box<Typed>),
    FloatMathTernary(FloatMathOp, Box<Typed>, Box<Typed>, Box<Typed>),
    MaskReduce(MaskReduceOp, Box<Typed>),
    VectorSelect {
        mask: Box<Typed>,
        then_value: Box<Typed>,
        else_value: Box<Typed>,
    },
    VectorLit {
        shape: VectorShape,
        elems: Vec<Typed>,
    },
    VectorSplat {
        shape: VectorShape,
        value: Box<Typed>,
    },
    /// `loadShape(arr, idx)` — load `shape.lanes()` contiguous scalar elements of
    /// the array starting at element `idx` as one fixed-lane vector (SIMD Sprint
    /// 2). The node's `ty` is the resulting `Vector(shape, E)`, where `E` is the
    /// array's element type.
    VectorLoad {
        shape: VectorShape,
        arr: Box<Typed>,
        idx: Box<Typed>,
    },
    /// `storeShape(arr, idx, value)` — store `value`'s lanes to the
    /// `shape.lanes()` contiguous elements at `idx`; yields `Unit`.
    VectorStore {
        shape: VectorShape,
        arr: Box<Typed>,
        idx: Box<Typed>,
        value: Box<Typed>,
    },
    VectorExtract {
        vector: Box<Typed>,
        lane: usize,
    },
    If(Box<Typed>, Box<Typed>, Box<Typed>),
    Loop {
        vars: Vec<(String, Type, ValueLayout, Typed)>,
        cond: Box<Typed>,
        steps: Vec<Typed>,
        result: Box<Typed>,
    },
    Lam {
        param: String,
        param_ty: Type,
        body: Box<Typed>,
    },
    App {
        fun: Box<Typed>,
        arg: Box<Typed>,
    },
    Let {
        name: String,
        bound: Box<Typed>,
        body: Box<Typed>,
    },
    /// Internal flattened sequence of already-elaborated bindings followed by a
    /// tail expression. Surface Locus remains expression-shaped; this keeps long
    /// stdlib/module spines wide in the typed model instead of rebuilding a deep
    /// unary `Let` chain.
    Block {
        items: Vec<TypedBlockItem>,
        body: Box<Typed>,
    },
    /// `let mut name = bound in body` — a **non-escaping scalar mutable local**
    /// (mutability v1; `docs/mutability.md` §3). Typed like a `Let` (the body's
    /// type/row, the bound is a scalar), but the binder is *mutable*: reads of
    /// `name` are plain `Var`s, assignments are [`Node::Assign`]. Sprint 3 lowers
    /// this to a stack `alloca` + an initial `store`; reads become `load`s.
    LetMut {
        name: String,
        bound: Box<Typed>,
        body: Box<Typed>,
    },
    /// `name := value` — **assign** the mutable local `name` (`docs/mutability.md`
    /// §1/§3); yields `Unit`. Lowers to a `store` into `name`'s stack slot. The
    /// assignment itself adds no effect label (a stack store), so its row is just
    /// `value`'s. (Distinct from [`Node::RefAssign`], the *heap*-cell write.)
    Assign {
        name: String,
        value: Box<Typed>,
    },

    /// `ref e` — allocate a fresh `Ref[T]` heap cell holding `e`
    /// (`docs/mutability.md` §1.1). A one-field heap object, so it lowers exactly
    /// like a single-field tuple (`locus_gc_alloc` + a `set_scalar` of `e`'s cell)
    /// and its result `ty` is `Ref[T]` (a handle). Carries `{gc}` (allocation). The
    /// content cell's layout is derived in `ir.rs` from `value.ty` **after zonk**
    /// (always one scalar cell this sprint — a pointer-typed `Ref` is rejected at
    /// elaboration), never captured here where the content may still be a `Var`.
    RefNew {
        value: Box<Typed>,
    },

    /// `!r` — read (dereference) the heap cell `r : Ref[T]`, yielding `T`
    /// (`docs/mutability.md` §1.1). Lowers like a field projection at slot 0
    /// (`get_scalar`). Carries `{st}` (observable mutation). The content cell's
    /// layout is derived in `ir.rs` from this node's (zonked) result type `T`.
    Deref {
        cell: Box<Typed>,
    },

    /// `r := v` where `r : Ref[T]` — write `v` into the heap cell `r`, in place;
    /// yields `Unit` (`docs/mutability.md` §1.1). Lowers like a field store at slot
    /// 0 (`set_scalar`). Carries `{st}`. **No write barrier** — the content cell is
    /// scalar (it never holds a pointer), so a write can never create an old→young
    /// pointer (Sprint 3 adds the barrier for a pointer-typed `Ref`). `target` is
    /// the `Ref` handle expression; the content layout is `value.ty`'s (zonked).
    RefAssign {
        target: Box<Typed>,
        value: Box<Typed>,
    },
    Perform {
        label: Label,
        arg: Box<Typed>,
    },
    Handle {
        scrutinee: Box<Typed>,
        handler: TypedHandler,
    },
    Quote(Box<Typed>),
    Splice(Box<Typed>),
    Genlet(Box<Typed>),
    Letloc(Box<Typed>),
    /// `peekW addr` — read `W` bits at an `Int` address (`! {mem}`).
    Peek(MemWidth, Box<Typed>),
    /// `pokeW addr val` — write `W` bits at an `Int` address (`! {mem}`).
    Poke(MemWidth, Box<Typed>, Box<Typed>),
    /// `fill dst byte count` — memset `count` bytes (`! {mem}`).
    Fill(Box<Typed>, Box<Typed>, Box<Typed>),
    /// `copy dst src count` — memmove `count` bytes (`! {mem}`).
    Copy(Box<Typed>, Box<Typed>, Box<Typed>),
    /// `a[i]` — array read; the width is resolved from `a`'s type (`! {mem}`).
    Index(MemWidth, Box<Typed>, Box<Typed>),
    /// `a[i] <- v` — array store (`! {mem}`).
    IndexSet(MemWidth, Box<Typed>, Box<Typed>, Box<Typed>),
    /// `(e1, …, en)` — a tuple value.
    Tuple(Vec<Typed>),
    /// `let (x1, …, xn) = e in body` — tuple destructuring.
    LetTuple(Vec<String>, Box<Typed>, Box<Typed>),
    /// `{ x = e, … }` — a record value; fields **sorted by name** (the canonical
    /// layout). At runtime a tuple of the sorted values.
    Record(Vec<(String, Typed)>),
    /// `r.x` — project field `x` of a record. (IR resolves the slot from `r`'s
    /// record type.)
    Field(Box<Typed>, String),

    /// `[e1, …, en]` — an array literal. `elem_layout` records how each element
    /// is stored. (`! {gc}`.)
    ArrayLit {
        elems: Vec<Typed>,
        elem_layout: ValueLayout,
    },
    /// `len a` — an array's element count (`Array[T] -> Int`).
    Len(Box<Typed>),
    /// `a[i]` on an array — a bounds-checked element read. (`! {gc}`.)
    ArrayGet {
        arr: Box<Typed>,
        idx: Box<Typed>,
        elem_layout: ValueLayout,
    },
    /// `a[i] <- v` on an array — a bounds-checked element write, yields `Unit`.
    ArraySet {
        arr: Box<Typed>,
        idx: Box<Typed>,
        val: Box<Typed>,
        elem_layout: ValueLayout,
    },

    /// `C(args)` — build a sum value: a tagged GC object. `tag` discriminates the
    /// constructor; each arg carries its storage layout **and its declared field
    /// type** — the constructor's instantiated field slot (a `Type::Var` at a
    /// polymorphic position). At runtime, scalar field 0 is the tag; the args
    /// follow. (`! {gc}`.)
    ///
    /// The declared field type is recorded because it is the **boundary slot**
    /// the T0 validator ([`crate::tagcheck`]) needs to decide whether each arg
    /// required a coercion — and the constructor registry it came from
    /// (`TYENV`) is unwound by the time the tree is validated/zonked. T3 also
    /// reads it to lay `Var` fields in the collector's traced range.
    Construct {
        tag: i64,
        args: Vec<(Typed, ValueLayout, Type)>,
    },
    /// `match s with arms` — tag-dispatched elimination of a sum value.
    Match {
        scrutinee: Box<Typed>,
        arms: Vec<MatchArmT>,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypedBlockItem {
    Let {
        name: String,
        bound: Typed,
    },
    LetMut {
        name: String,
        bound: Typed,
    },
    LetTuple {
        names: Vec<String>,
        bound: Typed,
        fields_layout_known: bool,
    },
}

impl TypedBlockItem {
    fn bound(&self) -> &Typed {
        match self {
            TypedBlockItem::Let { bound, .. }
            | TypedBlockItem::LetMut { bound, .. }
            | TypedBlockItem::LetTuple { bound, .. } => bound,
        }
    }

    fn binds_name(&self, needle: &str) -> bool {
        match self {
            TypedBlockItem::Let { name, .. } | TypedBlockItem::LetMut { name, .. } => {
                name == needle
            }
            TypedBlockItem::LetTuple { names, .. } => names.iter().any(|name| name == needle),
        }
    }

    fn layout_known(&self) -> bool {
        match self {
            TypedBlockItem::Let { bound, .. } | TypedBlockItem::LetMut { bound, .. } => {
                bound.layout_known
            }
            TypedBlockItem::LetTuple {
                bound,
                fields_layout_known,
                ..
            } => bound.layout_known && *fields_layout_known,
        }
    }
}

/// One elaborated `match` arm: which tag it matches (`None` = wildcard), the
/// fields it binds (each: name, the physical region slot, and the field layout),
/// and the body.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MatchArmT {
    pub tag: Option<i64>,
    pub binds: Vec<(String, usize, ValueLayout, Type)>,
    pub body: Typed,
}

/// A decorated handler: each clause body is a `Typed` subtree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypedHandler {
    pub ops: Vec<TypedOpClause>,
    pub ret: TypedReturn,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypedOpClause {
    pub op: Label,
    pub arg: String,
    pub arg_ty: Type,
    pub arg_layout: ValueLayout,
    pub resume: String,
    pub resume_ty: Type,
    pub resume_layout: ValueLayout,
    pub body: Box<Typed>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypedReturn {
    pub var: String,
    pub var_ty: Type,
    pub var_layout: ValueLayout,
    pub body_ty: Type,
    pub body: Box<Typed>,
}

impl Typed {
    fn at(ty: Type, row: Row, stage: Stage, node: Node) -> Typed {
        Typed {
            ty,
            row,
            stage,
            layout_known: true,
            node,
        }
    }

    fn at_layout(ty: Type, row: Row, stage: Stage, layout_known: bool, node: Node) -> Typed {
        Typed {
            ty,
            row,
            stage,
            layout_known,
            node,
        }
    }

    /// The judgment carried here: `type ! row @ stage` (the `--brief` form).
    pub fn judgment(&self) -> String {
        format!("{} ! {} @ {}", self.ty, self.row, self.stage)
    }

    /// Does any node need a representation-polymorphic layout decision?
    pub fn has_unknown_layout(&self) -> bool {
        if !self.layout_known {
            return true;
        }
        match &self.node {
            Node::Var(_)
            | Node::Int(_)
            | Node::Float(_)
            | Node::Bool(_)
            | Node::Unit
            | Node::Brk
            | Node::Str(_)
            | Node::Extern(..) => false,
            Node::Bin(_, a, b)
            | Node::FloatMathBinary(_, a, b)
            | Node::Index(_, a, b)
            | Node::Poke(_, a, b) => a.has_unknown_layout() || b.has_unknown_layout(),
            Node::If(a, b, c)
            | Node::Fill(a, b, c)
            | Node::Copy(a, b, c)
            | Node::IndexSet(_, a, b, c) => {
                a.has_unknown_layout() || b.has_unknown_layout() || c.has_unknown_layout()
            }
            Node::Loop {
                vars,
                cond,
                steps,
                result,
            } => {
                vars.iter()
                    .any(|(_, _, layout, init)| !layout.known || init.has_unknown_layout())
                    || cond.has_unknown_layout()
                    || steps.iter().any(Typed::has_unknown_layout)
                    || result.has_unknown_layout()
            }
            Node::VectorSelect {
                mask,
                then_value,
                else_value,
            } => {
                mask.has_unknown_layout()
                    || then_value.has_unknown_layout()
                    || else_value.has_unknown_layout()
            }
            Node::Lam { body, .. }
            | Node::Perform { arg: body, .. }
            | Node::Quote(body)
            | Node::Splice(body)
            | Node::Genlet(body)
            | Node::Letloc(body)
            | Node::Cast(_, body)
            | Node::FloatMathUnary(_, body)
            | Node::MaskReduce(_, body)
            | Node::Peek(_, body)
            | Node::Len(body)
            | Node::Field(body, _) => body.has_unknown_layout(),
            Node::Coerce { inner, .. } => inner.has_unknown_layout(),
            Node::FloatMathTernary(_, a, b, c) => {
                a.has_unknown_layout() || b.has_unknown_layout() || c.has_unknown_layout()
            }
            Node::VectorSplat { value, .. } | Node::VectorExtract { vector: value, .. } => {
                value.has_unknown_layout()
            }
            Node::App { fun, arg } => fun.has_unknown_layout() || arg.has_unknown_layout(),
            Node::Block { items, body } => {
                items.iter().any(|item| item.bound().has_unknown_layout())
                    || body.has_unknown_layout()
                    || items.iter().any(|item| {
                        matches!(
                            item,
                            TypedBlockItem::LetTuple {
                                fields_layout_known: false,
                                ..
                            }
                        )
                    })
            }
            Node::Let { bound, body, .. }
            | Node::LetMut { bound, body, .. }
            | Node::LetTuple(_, bound, body) => {
                bound.has_unknown_layout() || body.has_unknown_layout()
            }
            Node::Assign { value, .. } => value.has_unknown_layout(),
            // `Ref` operators carry a scalar content cell (gate guarantees it), but
            // recurse into the sub-expressions for completeness.
            Node::RefNew { value, .. } => value.has_unknown_layout(),
            Node::Deref { cell, .. } => cell.has_unknown_layout(),
            Node::RefAssign { target, value, .. } => {
                target.has_unknown_layout() || value.has_unknown_layout()
            }
            Node::Tuple(es) | Node::ArrayLit { elems: es, .. } => {
                es.iter().any(Typed::has_unknown_layout)
            }
            Node::VectorLit { elems, .. } => elems.iter().any(Typed::has_unknown_layout),
            Node::Record(fs) => fs.iter().any(|(_, t)| t.has_unknown_layout()),
            Node::ArrayGet { arr, idx, .. } => arr.has_unknown_layout() || idx.has_unknown_layout(),
            Node::ArraySet { arr, idx, val, .. } => {
                arr.has_unknown_layout() || idx.has_unknown_layout() || val.has_unknown_layout()
            }
            Node::VectorLoad { arr, idx, .. } => {
                arr.has_unknown_layout() || idx.has_unknown_layout()
            }
            Node::VectorStore {
                arr, idx, value, ..
            } => arr.has_unknown_layout() || idx.has_unknown_layout() || value.has_unknown_layout(),
            Node::Construct { args, .. } => args.iter().any(|(t, _, _)| t.has_unknown_layout()),
            Node::Match { scrutinee, arms } => {
                scrutinee.has_unknown_layout()
                    || arms.iter().any(|arm| arm.body.has_unknown_layout())
            }
            Node::Handle { scrutinee, handler } => {
                scrutinee.has_unknown_layout()
                    || handler.ops.iter().any(|op| op.body.has_unknown_layout())
                    || handler.ret.body.has_unknown_layout()
            }
        }
    }
}

/// Add the minted `label` (the boundary module's `mints (L)`, default `winapi`)
/// to the **innermost** arrow of a foreign function's type — the arrow whose
/// result is a non-function, where the OS call fires. Partial applications of a
/// multi-argument extern stay pure. Returns `None` if `ty` is not a function
/// type at all.
///
/// **Risk-map defence (D7).** A [`Type::Var`] at the result position would, via
/// a silent catch-all, inject the label at the *wrong* arrow. Externs are
/// concrete, so a variable here is ill-formed; we reject it explicitly
/// (`None` → `NotAFunction`) rather than guess an injection point.
fn inject_mint(ty: &Type, label: &Label) -> Option<Type> {
    let Type::Fun(a, b, latent) = ty else {
        return None;
    };
    match &**b {
        // more arrows below: this one is a pure partial application — recurse.
        Type::Fun(..) => Some(Type::Fun(
            a.clone(),
            Box::new(inject_mint(b, label).expect("inner is a function")),
            latent.clone(),
        )),
        // An un-zonked / polymorphic result on an extern is not a real boundary
        // — refuse rather than inject at a place we cannot justify.
        Type::Var(_) => None,
        // innermost arrow: the call fires here, so the effect lands here.
        _ => Some(Type::Fun(
            a.clone(),
            b.clone(),
            latent.union(&Row::single(label.clone())),
        )),
    }
}

/// Does an argument of type `arg` satisfy a parameter of type `dom`? Usually
/// **unification** — but a **`Ptr`** parameter (only ever written in an `extern`
/// signature) accepts any machine word: a `Ptr`, a `Str` (which *is* a wide
/// pointer), or an `Int` (a null / a handle). This is the boundary coercion that
/// lets a wide `Str` reach a Win32 `…W` argument — a *subtyping* allowance, not
/// an equation, so it stays a special case ahead of `unify`.
///
/// On the general path the demand goes through `unify` (so a future
/// polymorphic parameter solves against the argument); in S1 both are ground and
/// it accepts exactly when `dom == arg` did. The error is the same `Mismatch`.
fn arg_matches(dom: &Type, arg: &Type) -> Result<(), TypeErr> {
    if *dom == Type::Ptr && matches!(arg, Type::Ptr | Type::Int) {
        return Ok(());
    }
    demand_eq(dom, arg)
}

/// True iff a concrete-arrow argument `actual` flowing into a callee parameter
/// arrow `p_dom -> p_cod` crosses a representation boundary at some position —
/// the callee arrow is representation-polymorphic (a `Var` component) where the
/// concrete callback is a scalar. The callee components are read **literally**: a
/// `Var` stays a `Var` even though `arg_matches` has just bound it in the store,
/// and that un-resolved `Var` is the only signal the slot is polymorphic
/// (`resolve(p_dom)` would read the pinned `Int` and lose it). Only the argument
/// side is resolved to its concrete `Int`/handle. (T6; docs/repr-poly-impl.md.)
fn arrow_needs_wrapper(p_dom: &Type, p_cod: &Type, actual: &Type) -> bool {
    if let Type::Fun(a_dom, a_cod, _) = actual {
        let ad = unify::with_store(|s| s.resolve_ty(a_dom));
        let ac = unify::with_store(|s| s.resolve_ty(a_cod));
        // Contravariant domain (an Untag before f) or covariant codomain (a Tag
        // after f). Either non-None boundary means the verbatim callback is
        // wrong: it would consume a tagged word as a raw scalar, or leave its
        // raw result un-tagged in the caller's word cell.
        !matches!(Type::coercion(&ad, p_dom), Coercion::None)
            || !matches!(Type::coercion(p_cod, &ac), Coercion::None)
    } else {
        false
    }
}

/// Synthesize a wrapper that untags / `FromPtr`s each argument before handing it
/// to the concrete callback `f`, and tags / `ToPtr`s `f`'s result before it lands
/// in the caller's `Var` cell. **Recurses the curried spine**: a callee
/// `a -> b -> c` (e.g. `list_fold`'s `b -> a -> b`) becomes
/// `fn $cbw0 => fn $cbw1 => coerce( f (coerce $cbw0) (coerce $cbw1) )` — one
/// wrapper lambda per callee parameter, the deepest coercing the final result.
/// Each side inserts exactly the coercion `Type::coercion` demands — a `None` at a
/// handle/`Var` position is a verbatim passthrough. The `Coerce` nodes record a
/// **fresh unbound var on the `Uniform` (word) side** so T0's resolving check
/// reads it `Uniform` (the literal callee `Var` is store-bound to the pinned
/// concrete type). `f` is inlined as the call spine's `fun` (used once; lowering
/// hoists it into a closure bind).
fn wrap_callback(callee_arrow: &Type, f: &Typed, stage: Stage) -> Typed {
    // Peel the callee + f arrow spines in lockstep: one wrapper parameter per
    // level, recording the callee slot (a `Var` when polymorphic) and f's concrete
    // domain. `fe` ends as the innermost arrow's effect — the full call's row.
    let mut params: Vec<(String, Type, Type)> = Vec::new();
    let mut ccod = callee_arrow.clone();
    let mut fty = unify::with_store(|s| s.resolve_ty(&f.ty));
    let mut fe = Row::pure();
    let mut i = 0usize;
    loop {
        let peeled = match (&ccod, &fty) {
            (Type::Fun(cd, c_next, _), Type::Fun(fd, f_next, f_row)) => Some((
                (**cd).clone(),
                (**fd).clone(),
                (**c_next).clone(),
                (**f_next).clone(),
                f_row.clone(),
            )),
            _ => None,
        };
        let Some((cd, fd, c_next, f_next, f_row)) = peeled else {
            break;
        };
        let f_dom = unify::with_store(|s| s.resolve_ty(&fd));
        params.push((format!("$cbw{i}"), cd, f_dom));
        fe = f_row;
        ccod = c_next;
        fty = unify::with_store(|s| s.resolve_ty(&f_next));
        i += 1;
    }
    if params.is_empty() {
        return f.clone();
    }
    // `ccod` is the callee's final codomain (a `Var` when polymorphic); `fty` is
    // f's concrete final result.
    let (p_cod, c) = (ccod, fty);
    // Build the application spine `f arg0 arg1 ...`, each arg coerced into f's
    // domain. The full application (last arg) carries `fe`; partials are pure.
    let n = params.len();
    let mut app = f.clone();
    // A **FromPtr** parameter (a managed-handle callee slot arriving as a raw
    // `addr|10` word) is bound under a fresh `$cbwi$c` name by a `let` laid down
    // INSIDE the param's own lambda (below), and f is applied to that name. This
    // matters for a CURRIED callback: an inner lambda that captures an earlier
    // FromPtr param must capture the interned HANDLE (`$cbwi$c`), not the raw word
    // — whose pointer-cell capture-store would `set_ptr` a magic-less `addr|10`
    // and crash `resolve` (the handle-accumulator-fold bug). An `Untag` (scalar)
    // or passthrough param needs no binding: a scalar/word capture is verbatim.
    let mut binds: Vec<Option<(String, Typed)>> = Vec::with_capacity(n);
    for (j, (wname, callee_dom, f_dom)) in params.iter().enumerate() {
        let param_ref = Typed::at_layout(
            callee_dom.clone(),
            Row::pure(),
            stage,
            true,
            Node::Var(wname.clone()),
        );
        let coerced = coerce_wrapper_arg(f_dom, callee_dom, param_ref, stage);
        let (arg, bind) = if matches!(Type::coercion(f_dom, callee_dom), Coercion::FromPtr) {
            let aname = format!("${wname}$c");
            let arg = Typed::at_layout(
                f_dom.clone(),
                Row::pure(),
                stage,
                true,
                Node::Var(aname.clone()),
            );
            (arg, Some((aname, coerced)))
        } else {
            // Passthrough (a still-`Var` arg): apply the param verbatim, no bind.
            (coerced, None)
        };
        binds.push(bind);
        let app_cod = match unify::with_store(|s| s.resolve_ty(&app.ty)) {
            Type::Fun(_, cod, _) => (*cod).clone(),
            other => other,
        };
        let app_row = if j + 1 == n { fe.clone() } else { Row::pure() };
        app = Typed::at_layout(
            app_cod,
            app_row,
            stage,
            true,
            Node::App {
                fun: Box::new(app),
                arg: Box::new(arg),
            },
        );
    }
    // Coerce f's concrete result into the callee's word-cell codomain.
    let (mut body, mut body_ty) = coerce_wrapper_result(&p_cod, &c, app, fe, stage);
    // Nest the wrapper lambdas innermost-out. Each coerced param is bound by a
    // `let` inside its own lambda (so inner lambdas capture the coerced value),
    // then the lambda wraps it. The innermost arrow carries the call's effect
    // (`body`'s row); each outer arrow is pure (its body is a value).
    for ((wname, callee_dom, _), bind) in params.iter().rev().zip(binds.into_iter().rev()) {
        if let Some((aname, coerced)) = bind {
            body = Typed::at_layout(
                body.ty.clone(),
                body.row.clone(),
                stage,
                body.layout_known,
                Node::Let {
                    name: aname,
                    bound: Box::new(coerced),
                    body: Box::new(body),
                },
            );
        }
        let wrapper_ty = Type::Fun(
            Box::new(callee_dom.clone()),
            Box::new(body_ty.clone()),
            body.row.clone(),
        );
        body = Typed::at_layout(
            wrapper_ty.clone(),
            Row::pure(),
            stage,
            true,
            Node::Lam {
                param: wname.clone(),
                param_ty: callee_dom.clone(),
                body: Box::new(body),
            },
        );
        body_ty = wrapper_ty;
    }
    body
}

/// Coerce a wrapper parameter `$cbwi` (a uniform `Var` word, typed `callee_dom`)
/// into f's concrete parameter `f_dom`: `Untag` (scalar) / `FromPtr` (handle), or
/// a verbatim passthrough. The `Coerce` records the concrete `f_dom` as `slot` and
/// a fresh `Var` as `value` (the Uniform side) for T0.
fn coerce_wrapper_arg(f_dom: &Type, callee_dom: &Type, param_ref: Typed, stage: Stage) -> Typed {
    let coer = Type::coercion(f_dom, callee_dom);
    if matches!(coer, Coercion::Untag | Coercion::FromPtr) {
        Typed::at_layout(
            f_dom.clone(),
            Row::pure(),
            stage,
            true,
            Node::Coerce {
                kind: coer,
                slot: f_dom.clone(),
                value: unify::with_store(|s| s.fresh_ty()),
                inner: Box::new(param_ref),
            },
        )
    } else {
        param_ref
    }
}

/// Coerce f's concrete result `c` into the callee's word-cell codomain `p_cod`:
/// `Tag` (scalar) / `ToPtr` (handle), or a verbatim passthrough. Returns the
/// coerced node and its type. The `Coerce` records a fresh `Var` as `slot` (the
/// Uniform side) and the concrete `c` as `value` for T0.
fn coerce_wrapper_result(
    p_cod: &Type,
    c: &Type,
    call: Typed,
    row: Row,
    stage: Stage,
) -> (Typed, Type) {
    let coer = Type::coercion(p_cod, c);
    if matches!(coer, Coercion::Tag | Coercion::ToPtr) {
        let slot = unify::with_store(|s| s.fresh_ty());
        let node = Typed::at_layout(
            slot.clone(),
            row,
            stage,
            true,
            Node::Coerce {
                kind: coer,
                slot: slot.clone(),
                value: c.clone(),
                inner: Box::new(call),
            },
        );
        (node, slot)
    } else {
        (call, c.clone())
    }
}

/// Coerce an App **result** back out of the callee's word-cell codomain when the
/// codomain is a bare type variable (`cod_lit` a literal `Var`) pinned to a
/// concrete type at this call: `Untag` (a scalar result) or `FromPtr` (a managed
/// handle result). The dual of the App-arg `Tag`/`ToPtr`. A concrete codomain, or
/// one still generic (`resolve` yields a `Var`), coerces `None` and the App result
/// rides verbatim — so the generic recursive call inside `list_fold` is untouched,
/// and only the outermost call (where `b` is pinned concrete) un-coerces once. The
/// `Coerce` records the resolved concrete type as `slot` and a fresh unbound `Var`
/// as `value` (the Uniform / word side) for T0 — the Match-binder `Untag` trick.
fn coerce_app_result(cod_lit: &Type, app: Typed, stage: Stage) -> Typed {
    let resolved = unify::with_store(|s| s.resolve_ty(cod_lit));
    let coer = Type::coercion(&resolved, cod_lit);
    if matches!(coer, Coercion::Untag | Coercion::FromPtr) {
        let row = app.row.clone();
        Typed::at_layout(
            resolved.clone(),
            row,
            stage,
            true,
            Node::Coerce {
                kind: coer,
                slot: resolved.clone(),
                value: unify::with_store(|s| s.fresh_ty()),
                inner: Box::new(app),
            },
        )
    } else {
        app
    }
}

/// The **`mem`** effect — raw memory access, a sealed kernel capability (the
/// same `World` machinery as `winapi`).
fn mem_effect() -> Row {
    Row::single(Label::World("mem".into()))
}

fn overflow_effect() -> Row {
    Row::single(Label::Exn("Overflow".into()))
}

fn callable_effect_label(sig: &Sig, ctx: &Ctx, name: &str) -> Option<Label> {
    if ctx.get(name).is_some() {
        return None;
    }
    let label = crate::prelude::op_label(name);
    if sig.contains_key(&label) {
        return Some(label);
    }
    let user = Label::User(name.to_string());
    if user != label && sig.contains_key(&user) {
        return Some(user);
    }
    None
}

fn resolved_type(ty: &Type) -> Type {
    unify::with_store(|s| unify::resolve_ty(s, ty))
}

fn float_or_default_int(a: &Type, b: &Type) -> Type {
    let a = resolved_type(a);
    let b = resolved_type(b);
    if matches!(a, Type::Float) || matches!(b, Type::Float) {
        Type::Float
    } else {
        Type::Int
    }
}

fn is_vector_lane_type(ty: &Type) -> bool {
    matches!(resolved_type(ty), Type::Float32 | Type::Float)
}

fn is_float_math_type(ty: &Type) -> bool {
    match resolved_type(ty) {
        Type::Float | Type::Float32 => true,
        Type::Vector(_, elem) => is_vector_lane_type(&elem),
        _ => false,
    }
}

fn float_math_unary_operand_type(a: &Type) -> Type {
    let a = resolved_type(a);
    if is_float_math_type(&a) {
        a
    } else {
        Type::Float
    }
}

fn vector_reduce_result_type(a: &Type) -> Option<Type> {
    match resolved_type(a) {
        Type::Vector(_, elem) if is_vector_lane_type(&elem) => Some(*elem),
        _ => None,
    }
}

fn vector_dot_operand_type(a: &Type, b: &Type) -> Option<(Type, Type)> {
    let ra = resolved_type(a);
    let rb = resolved_type(b);
    match (&ra, &rb) {
        (Type::Vector(sa, ea), Type::Vector(sb, eb))
            if sa == sb && ea == eb && is_vector_lane_type(ea) =>
        {
            Some((ra.clone(), (**ea).clone()))
        }
        _ => None,
    }
}

fn example_vector_type() -> Type {
    Type::Vector(VectorShape::Quad, Box::new(Type::Float32))
}

fn example_mask_type() -> Type {
    Type::Mask(VectorShape::Quad)
}

fn float_math_ternary_operand_type(a: &Type, b: &Type, c: &Type) -> Type {
    for ty in [resolved_type(a), resolved_type(b), resolved_type(c)] {
        if is_float_math_type(&ty) {
            return ty;
        }
    }
    Type::Float
}

fn numeric_operand_type(op: BinOp, a: &Type, b: &Type) -> Type {
    let ra = resolved_type(a);
    let rb = resolved_type(b);
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div)
        && matches!(
            (&ra, &rb),
            (Type::Vector(sa, ea), Type::Vector(sb, eb))
                if sa == sb && ea == eb && is_vector_lane_type(ea)
        )
    {
        return ra;
    }
    float_or_default_int(&ra, &rb)
}

fn is_vector_bin_op(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
    )
}

fn vector_bin_result_type(op: BinOp, shape: VectorShape, elem: &Type) -> Type {
    if op.is_comparison() {
        Type::Mask(shape)
    } else {
        Type::Vector(shape, Box::new(elem.clone()))
    }
}

fn vector_parts(ty: &Type) -> Option<(VectorShape, Type)> {
    match resolved_type(ty) {
        Type::Vector(shape, elem) => Some((shape, *elem)),
        _ => None,
    }
}

fn mask_shape(ty: &Type) -> Option<VectorShape> {
    match resolved_type(ty) {
        Type::Mask(shape) => Some(shape),
        _ => None,
    }
}

fn vector_vector_bin_type(op: BinOp, lhs: &Type, rhs: &Type) -> Option<Type> {
    if !is_vector_bin_op(op) {
        return None;
    }
    match (vector_parts(lhs), vector_parts(rhs)) {
        (Some((sa, ea)), Some((sb, eb))) if sa == sb && ea == eb && is_vector_lane_type(&ea) => {
            Some(vector_bin_result_type(op, sa, &ea))
        }
        _ => None,
    }
}

fn coerce_vector_scalar(elem_ty: &Type, value: Typed) -> Result<Typed, TypeErr> {
    let expected = resolved_type(elem_ty);
    let found = resolved_type(&value.ty);

    if matches!(
        (&expected, &found, &value.node),
        (Type::Float32, Type::Float, Node::Float(_))
    ) {
        let row = value.row.clone();
        let stage = value.stage;
        return Ok(Typed::at(
            Type::Float32,
            row,
            stage,
            Node::Cast(CastOp::ToFloat32, Box::new(value)),
        ));
    }

    demand_eq(&expected, &value.ty)?;
    Ok(value)
}

fn vector_splat_typed(shape: VectorShape, elem_ty: Type, value: Typed) -> Typed {
    let ty = Type::Vector(shape, Box::new(elem_ty));
    let row = value.row.clone();
    let stage = value.stage;
    let layout_known = storage_layout(&ty).known;
    Typed::at_layout(
        ty,
        row,
        stage,
        layout_known,
        Node::VectorSplat {
            shape,
            value: Box::new(value),
        },
    )
}

fn vector_scalar_bin_operands(
    op: BinOp,
    lhs: &Typed,
    rhs: &Typed,
) -> Result<Option<(Type, Typed, Typed)>, TypeErr> {
    if !is_vector_bin_op(op) {
        return Ok(None);
    }

    match (vector_parts(&lhs.ty), vector_parts(&rhs.ty)) {
        (Some((shape, elem)), None) => {
            let rhs = vector_splat_typed(
                shape,
                elem.clone(),
                coerce_vector_scalar(&elem, rhs.clone())?,
            );
            Ok(Some((
                vector_bin_result_type(op, shape, &elem),
                lhs.clone(),
                rhs,
            )))
        }
        (None, Some((shape, elem))) => {
            let lhs = vector_splat_typed(
                shape,
                elem.clone(),
                coerce_vector_scalar(&elem, lhs.clone())?,
            );
            Ok(Some((
                vector_bin_result_type(op, shape, &elem),
                lhs,
                rhs.clone(),
            )))
        }
        _ => Ok(None),
    }
}

/// Require an operand to be an `Int` (the uniform machine word). Used by the
/// `mem` primitives, whose addresses / values / counts are all words. Routed
/// through `unify`, so the same demand the rest of the checker makes; the error
/// is the identical `Mismatch{ expected: Int, found }`.
fn expect_int(t: &Typed) -> Result<(), TypeErr> {
    demand_eq(&Type::Int, &t.ty)
}

/// The element width a subscript `a[i]` reads, by the base's type: a `String` is
/// 16-bit (UTF-16) units; a raw `Int`/`Ptr` address is bytes. Anything else is
/// not an indexable machine word.
fn elem_width(ty: &Type) -> Result<MemWidth, TypeErr> {
    match ty {
        Type::Int | Type::Ptr => Ok(MemWidth::W8),
        other => Err(TypeErr::NotIndexable(other.clone())),
    }
}

/// Elaborate `Γ ⊢ e : A ! E @ stage` into a fully decorated [`Typed`] tree.
///
/// Identical rules to the old `infer`, but every recursive call's result is
/// *kept* and threaded into the node, so the whole tree is annotated.
/// A constructor's info — **scheme-shaped** (S3, D10/D11). The declared type
/// parameters of the sum are mapped to fresh quantified [`TyVarId`]s
/// (`ty_params`); the `fields` and `result` reference **those same ids** (built
/// once at declaration time by `rewrite_params`). A use site calls
/// [`instantiate_ctor`], which draws fresh vars for `ty_params` and substitutes
/// them through `fields` + `result` — so the constructor is polymorphic exactly
/// in its sum's parameters. A **monomorphic** sum has `ty_params == []`, and
/// `fields`/`result` are then ground (`result` = `Named(name, [])`), reproducing
/// the pre-S3 `CtorInfo` byte-for-byte.
#[derive(Clone)]
struct CtorInfo {
    /// The sum this constructor belongs to. Kept per D11 (the `CtorInfo` shape)
    /// for self-description; the sum *identity* now travels in `result`
    /// (`Named(type_name, …)`), which the `Construct`/`Match` arms use directly,
    /// so this field is informational (a wrong-sum ctor is now caught by the
    /// refinement unify in `Match`, not a separate name compare).
    #[allow(dead_code)]
    type_name: String,
    tag: i64,
    /// The sum's declared type parameters, as fresh quantified vars (D10).
    ty_params: Vec<TyVarId>,
    /// The constructor's field types, referencing `ty_params` (the scheme body).
    fields: Vec<Type>,
    /// The constructor's result type — `Named(type_name, ty_params images)`.
    result: Type,
}

/// The declared sum types in scope. A pragmatic per-thread registry rather than
/// a threaded parameter: `type … in body` registers before elaborating `body`,
/// and declarations accumulate within a program (no conflict — each program
/// declares the types it uses).
///
/// `sums` keeps each type's **(params, variants)** as written — `params` for
/// arity/Display, `variants` for the exhaustiveness check (which is by ctor
/// *name*, unchanged by S3).
#[derive(Clone, Default)]
struct TyEnv {
    sums: std::collections::HashMap<String, (Vec<String>, Vec<(String, Vec<Type>)>)>,
    ctors: std::collections::HashMap<String, CtorInfo>,
}

/// A registered **trait** declaration (traits/qualified types v1,
/// `trait-resolution.md` §1.1). Single-parameter (D6): `param` is the trait's
/// type variable name, `supers` its superclass constraints (`requires …`), and
/// `methods` each method's *declared* signature (the type written after `:`,
/// before the trait's own `Trait a` constraint is added). Registered before the
/// declaration's body is elaborated, so a method use in the body resolves to the
/// minted generic function; the registry is the keystone Sprint 2's resolution
/// reads (its `(trait, head)` instance lookup).
#[derive(Clone)]
struct TraitInfo {
    /// The trait's type-parameter **name** as written (`a`).
    param: String,
    /// Superclass constraints from the *trait* declaration (`trait Ord a requires
    /// Eq a`). Resolution's recursive superclass sub-obligations come from the
    /// *instance*'s `requires` ([`InstanceInfo::requires`]), so this trait-level
    /// copy is currently recorded but unread (it backs a future check that an
    /// instance's `requires` covers the trait's declared superclasses, O-T3).
    #[allow(dead_code)]
    supers: Vec<crate::syntax::Constraint>,
    /// Each method's `(name, declared signature)`.
    methods: Vec<(String, Type)>,
    /// The **declaring module** of the `trait` (traits v1 orphan check R5,
    /// `trait-resolution.md` §4); `None` for a bare (module-less) program.
    module: Option<String>,
}

/// A registered **instance** declaration (`trait-resolution.md` §1.1) —
/// `(trait, head)` → the method names it implements, plus its `requires`
/// context. Sprint 1 registers and lightly type-checks; Sprint 2 reads this for
/// coherence (R3), overlap (R4), orphan (R5), termination (R6), and the
/// `(trait, head)` resolution lookup.
// The registry is populated in Sprint 1 (so the data is in place + tested) and
// **read in Sprint 2** (resolution / coherence). The fields are recorded now but
// not yet consumed, hence the allow.
#[derive(Clone)]
struct InstanceInfo {
    trait_name: String,
    /// The instance head type (`Int` for `instance Show Int`); resolution keys on
    /// its outermost head constructor ([`head_key`]).
    head: Type,
    /// The instance's context constraints (`requires …`) — the recursive
    /// sub-obligations of R1 step 2, gated by Paterson termination (R6). Used for
    /// the structural-recursion case (`instance Show [a] requires Show a`).
    requires: Vec<crate::syntax::Constraint>,
    /// The trait's **type-parameter name** and **superclass constraints** captured
    /// at registration (`trait Ord a requires Eq a` ⟹ `("a", [Eq a])`). When
    /// resolving `Trait τ`, each superclass becomes a sub-obligation with the
    /// param bound to `τ` (R1.4, `RN-E0236` on failure) — distinct from the
    /// instance's own `requires`, and *not* subject to the Paterson check (the
    /// superclass DAG is a fixed, finite set of traits).
    trait_param: String,
    trait_supers: Vec<crate::syntax::Constraint>,
    /// The method names implemented (for the missing/extra-method check).
    #[allow(dead_code)]
    methods: Vec<String>,
    /// The **elaborated method bodies** (`m = e` ⟹ `(m, ⟦e⟧)`), captured at the
    /// instance declaration (traits v1 Sprint 3, dictionary-passing lowering,
    /// `object-system-design.md` §4). These are the **method closures** the
    /// dictionary *literal* is built from: when resolution discharges `Trait head`
    /// against this instance ([`DictEvidence::Instance`]), Sprint 3 lowers the
    /// dictionary to a `Node::Record` whose fields are these bodies (plus an
    /// embedded `super_<Trait>` field per superclass — O-T1 lean-embed). The
    /// bodies are stored **pre-zonk** (the unification store is still live when the
    /// dictionary transform runs, inside [`elaborate`], and zonks them there).
    ///
    /// Each entry is `(method, ⟦body⟧, uniform_arrow)`. `uniform_arrow` is the
    /// trait's *declared* method signature with the trait parameter kept as a
    /// **fresh uniform `Var`** (every other position concrete) — the abstract /
    /// uniform ABI a generic caller tags its arguments into. A method *call* through
    /// the dictionary tags scalar arguments into uniform words (repr-poly,
    /// `docs/repr-poly-impl.md`), so the stored concrete-typed body is wrapped (via
    /// [`wrap_callback`]) against this `uniform_arrow` to untag each argument and
    /// re-tag its result — the dictionary field thus presents the uniform ABI the
    /// call site expects, and the tag/untag round-trips. (Captured at registration,
    /// while the scoped [`TRAITENV`] still has the trait's method signatures.)
    method_bodies: Vec<(String, Typed, Type)>,
    /// The **declaring module** (traits v1 orphan check R5); `None` for a bare
    /// (module-less) program. The orphan check itself runs at the decl site
    /// against the live `module` (before registration), so this stored copy is
    /// recorded for completeness / future module-aware passes.
    #[allow(dead_code)]
    module: Option<String>,
}

/// The declared **traits** and **instances** in scope (traits v1). A per-thread
/// registry like [`TyEnv`]: `trait …`/`instance …` register before elaborating
/// their body, and accumulate within a program. Sprint 1 populates and reads it
/// for method minting + light instance checking; Sprint 2 reads it for the full
/// resolution + coherence checks.
#[derive(Clone, Default)]
struct TraitEnv {
    traits: std::collections::HashMap<String, TraitInfo>,
    /// **type name → declaring module** (traits v1 orphan check R5,
    /// `trait-resolution.md` §4): a `type T = …` in `module M` records `T ↦ M`, so
    /// the orphan check can ask whether an instance lives in its type head's module.
    /// `None`-module types (a bare program) are not recorded.
    type_modules: std::collections::HashMap<String, String>,
}

thread_local! {
    static TYENV: std::cell::RefCell<TyEnv> = std::cell::RefCell::new(TyEnv::default());

    /// The **scoped** trait registry (traits v1, `trait-resolution.md` §1.1) —
    /// trait declarations + the type→module map, save/restored per `trait`/`type`
    /// scope (so method minting + the declaration-time checks see exactly the
    /// in-scope decls). Reset per program by [`elaborate`].
    static TRAITENV: std::cell::RefCell<TraitEnv> =
        std::cell::RefCell::new(TraitEnv::default());

    /// The **program-global** instance set (traits v1). Coherence (R3) is a global
    /// property — *one* instance per `(trait, head)` in the whole program — so
    /// instances are not torn down at scope exit; they accumulate for the lifetime
    /// of one [`elaborate`] and the end-of-elaboration **resolution pass** reads
    /// them (the scoped `TRAITENV` is gone by then). Reset per program by
    /// [`elaborate`]; appended by the `Term::Instance` arm after its R3/R4/R5/R6
    /// checks pass.
    static INSTANCES: std::cell::RefCell<Vec<InstanceInfo>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// The **program-global** trait registry (traits v1 Sprint 3) — every `trait`
    /// declaration, by name, kept for the lifetime of one [`elaborate`] (unlike the
    /// scoped [`TRAITENV`], which is torn down at the trait's scope exit). The
    /// dictionary transform runs *after* the whole tree is elaborated, when
    /// `TRAITENV` is already restored to empty, yet still needs each trait's method
    /// names + superclasses to type a hidden dictionary *parameter* ([`dict_record_type`]).
    /// Populated by the `Term::Trait` arm; reset per program by [`elaborate`].
    static TRAIT_DEFS: std::cell::RefCell<std::collections::HashMap<String, TraitInfo>> =
        std::cell::RefCell::new(std::collections::HashMap::new());

    /// Pending **seal no-escape obligations** (`seal L { e }`,
    /// [`sealing-solution.md`] §5). A `seal` is erased to its body at elaboration
    /// (runtime-transparent); the deep no-escape side condition is checked
    /// **after zonk** so open row tails are resolved (D6). Each entry is
    /// `(sealed label, body's result type)`; [`elaborate`] drains and checks them
    /// once the whole tree is solved.
    static SEAL_OBLIGATIONS: std::cell::RefCell<Vec<(Label, Type)>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// **Resolved dictionary evidence** for the discharged trait obligations
    /// (traits v1, `trait-resolution.md` §1.2) — the bridge to **Sprint 3**
    /// (dictionary-passing lowering). Each top-level obligation that resolution
    /// discharges against a concrete `instance` records its [`DictEvidence`] tree
    /// here (the chosen instance + its `requires` sub-dictionaries, recursively).
    /// Reset per program by [`elaborate`]; readable via [`take_dict_evidence`].
    /// Obligations discharged by a *caller's dictionary* (R1 step 1) are not here —
    /// `generalize` lifted them into the enclosing [`Scheme::constraints`], which is
    /// where Sprint 3 reads the function's hidden dictionary parameters.
    static EVIDENCE: std::cell::RefCell<Vec<ResolvedObligation>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// **Trait-method-use side table** (traits v1 Sprint 3, the evidence→call-site
    /// bridge). Each *use* of a trait method (`show`, `compare`) is stamped, at its
    /// `Node::Var` site, with a fresh **use id** baked into the variable name via
    /// [`DICT_SENTINEL`] (`show\u{1}M7`). This table records, per use id, the trait,
    /// the method, and the **obligation type variable** that `instantiate` emitted
    /// for it (still a `Type::Var` here — unification solves it later). After the
    /// whole program is solved, [`resolve_dict_plans`] resolves each: a *concrete*
    /// head ⟹ the instance dictionary literal ([`DictEvidence::Instance`]); an
    /// *unbound* var (the constraint was generalized onto an enclosing scheme) ⟹ a
    /// hidden dictionary parameter ([`DictEvidence::DictParam`]). The
    /// [`dict_pass`] transform reads the resolved plan to rewrite the use.
    static METHOD_USES: std::cell::RefCell<Vec<MethodUse>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// **Constrained-generic-use side table** (traits v1 Sprint 3). Each *use* of a
    /// `let`-bound name whose scheme carries constraints (`min2 : Ord a => …`) is
    /// stamped, at its `Node::Var` site, with a fresh **use id** ([`DICT_SENTINEL`],
    /// `min2\u{1}G3`). This records, per use id, the obligation `(trait, type-var)`
    /// pairs `instantiate` emitted for that use, in the scheme's constraint order.
    /// [`resolve_dict_plans`] resolves each pair to a [`DictEvidence`]; the
    /// [`dict_pass`] transform threads them as leading dictionary arguments.
    static GENERIC_USES: std::cell::RefCell<Vec<GenericUse>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// **Resolved dictionary plans**, keyed by use id ([`DICT_SENTINEL`] tag) —
    /// the output of [`resolve_dict_plans`], the input to [`dict_pass`]. Method
    /// uses (`M<id>`) and generic uses (`G<id>`) share one id space.
    static DICT_PLANS: std::cell::RefCell<std::collections::HashMap<usize, DictPlan>> =
        std::cell::RefCell::new(std::collections::HashMap::new());

    /// A monotonically increasing **use-id** counter (traits v1 Sprint 3), reset per
    /// program by [`elaborate`]. Stamps every trait-method / constrained-generic use.
    static DICT_USE_ID: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// **Constrained-generic definitions**, keyed by def id (the `D<id>` tag baked
    /// into the `Node::Let` binder). The value is the list of **trait names** the
    /// generic is constrained by, in scheme order — one hidden dictionary parameter
    /// each (traits v1 Sprint 3). Reset per program by [`elaborate`].
    static CONSTRAINED_DEFS: std::cell::RefCell<std::collections::HashMap<usize, Vec<String>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The **dictionary-plumbing sentinel** (traits v1 Sprint 3): a control character
/// (`U+0001`) that cannot occur in a source identifier, used to bake a use/def id
/// into a `Node::Var` / `Node::Let` name so the post-zonk [`dict_pass`] transform
/// can find the sites it must rewrite **without a new `Node` variant** (zonk,
/// `tagcheck`, `stage`, `ir` all treat the name as an opaque string; the transform
/// strips the sentinel before lowering, so those passes never see a trait node).
/// The character after the sentinel tags the site kind: `M` = trait-method use,
/// `G` = constrained-generic use, `D` = constrained-generic definition.
const DICT_SENTINEL: char = '\u{1}';

/// A recorded **trait-method use** (the [`METHOD_USES`] entry). `obligation_ty` is
/// the `instantiate`-fresh type variable for the method's `Trait a` constraint.
#[derive(Clone)]
struct MethodUse {
    id: usize,
    trait_name: String,
    method: String,
    obligation_ty: Type,
    /// The method's **instantiated type at this use** (`?a -> Int ! ?r`). Its
    /// latent-row variable `?r` is bound, pre-zonk, to the resolved instance's
    /// real method row by [`bind_method_use_rows`] — so a variable-row method
    /// (`trait-resolution.md` §7.3) surfaces the instance's effect, not nothing.
    use_ty: Type,
}

/// A recorded **constrained-generic use** (the [`GENERIC_USES`] entry) — one
/// `(trait, obligation-type-var)` per constraint on the used name's scheme, in
/// scheme-constraint order (the order their hidden dictionary parameters take).
#[derive(Clone)]
struct GenericUse {
    id: usize,
    obligations: Vec<(String, Type)>,
}

/// The **resolved dictionary plan** for one use site (the [`DICT_PLANS`] value),
/// the bridge Step 1 firms up and Step 2 ([`dict_pass`]) consumes.
#[derive(Clone)]
enum DictPlan {
    /// A trait-method use: project `method` from the resolved dictionary, then the
    /// surrounding application calls it. `evidence` is the dictionary (an instance
    /// literal, or a threaded parameter for an abstract caller).
    Method {
        method: String,
        evidence: DictEvidence,
    },
    /// A constrained-generic use: supply these dictionaries as **leading arguments**
    /// (in scheme-constraint order), à la dictionary-passing.
    Generic { dicts: Vec<DictEvidence> },
}

/// Allocate a fresh dictionary use id (traits v1 Sprint 3).
fn fresh_dict_id() -> usize {
    DICT_USE_ID.with(|c| {
        let id = c.get();
        c.set(id + 1);
        id
    })
}

/// Bake a use/def id into a name with the [`DICT_SENTINEL`] tag (`min2\u{1}G3`).
fn dict_tag(name: &str, kind: char, id: usize) -> String {
    format!("{name}{DICT_SENTINEL}{kind}{id}")
}

/// Strip a [`DICT_SENTINEL`] tag, returning `(base name, kind, id)` if present.
fn dict_untag(name: &str) -> Option<(&str, char, usize)> {
    let (base, tag) = name.split_once(DICT_SENTINEL)?;
    let mut chars = tag.chars();
    let kind = chars.next()?;
    let id: usize = chars.as_str().parse().ok()?;
    Some((base, kind, id))
}

/// The canonical **hidden dictionary parameter name** for a constraint on `trait`
/// (traits v1 Sprint 3) — `$dict$Ord`. A constrained generic takes one such leading
/// parameter per constraint; a trait-method use discharged by a *threaded* caller
/// dictionary ([`DictEvidence::DictParam`]) projects its method from this name.
fn dict_param_name(trait_name: &str) -> String {
    format!("$dict${trait_name}")
}

/// The canonical **superclass field name** in a dictionary record (traits v1
/// Sprint 3, O-T1 lean-embed) — `super_Eq` for the `Eq` super-dictionary embedded
/// in an `Ord` dictionary.
fn super_field_name(trait_name: &str) -> String {
    format!("super_{trait_name}")
}

/// If `method` is a method declared by some in-scope trait, the **trait name**
/// (traits v1 Sprint 3). Distinguishes a trait-method use (`show`, projected from
/// its dictionary) from a constrained user generic (`min2`, fed dictionaries as
/// leading args) — both are constrained `Poly` bindings.
fn trait_of_method(method: &str) -> Option<String> {
    TRAITENV.with(|t| {
        t.borrow()
            .traits
            .iter()
            .find(|(_, info)| info.methods.iter().any(|(m, _)| m == method))
            .map(|(name, _)| name.clone())
    })
}

/// **Record a constrained use** at its `Node::Var` site (traits v1 Sprint 3, the
/// evidence→call-site bridge). `fresh` are the obligation constraints `instantiate`
/// just emitted for `name`'s scheme, in constraint order. Returns the
/// [`DICT_SENTINEL`]-tagged name the post-zonk [`dict_pass`] keys on. A *trait
/// method* (single own constraint, declared by a trait) is tagged `M`; any other
/// constrained binding (a user generic) is tagged `G`.
fn record_dict_use(name: &str, fresh: &[crate::syntax::Constraint], use_ty: &Type) -> String {
    let id = fresh_dict_id();
    if let Some(trait_name) = trait_of_method(name) {
        // A trait method's minted scheme carries exactly its own `Trait a`
        // constraint; that is the obligation whose dictionary supplies the method.
        let obligation_ty = fresh
            .iter()
            .find(|c| c.trait_name == trait_name)
            .map(|c| c.ty.clone())
            // Defensive: a method whose own constraint somehow is not first — fall
            // back to the first obligation (the minted scheme always has one).
            .or_else(|| fresh.first().map(|c| c.ty.clone()))
            .unwrap_or(Type::Unit);
        METHOD_USES.with(|m| {
            m.borrow_mut().push(MethodUse {
                id,
                trait_name,
                method: name.to_string(),
                obligation_ty,
                use_ty: use_ty.clone(),
            })
        });
        dict_tag(name, 'M', id)
    } else {
        GENERIC_USES.with(|g| {
            g.borrow_mut().push(GenericUse {
                id,
                obligations: fresh
                    .iter()
                    .map(|c| (c.trait_name.clone(), c.ty.clone()))
                    .collect(),
            })
        });
        dict_tag(name, 'G', id)
    }
}

/// **Record a constrained-generic definition** (traits v1 Sprint 3). Stores the
/// generic's constraint trait list under a fresh def id and returns the
/// [`DICT_SENTINEL`]-tagged binder name (`min2\u{1}D2`) the [`dict_pass`] keys on
/// to wrap `bound` in its hidden leading dictionary parameters.
fn record_constrained_def(name: &str, traits: &[String]) -> String {
    let id = fresh_dict_id();
    CONSTRAINED_DEFS.with(|d| d.borrow_mut().insert(id, traits.to_vec()));
    dict_tag(name, 'D', id)
}

/// The maximum recursive resolution depth — a generous **fuel backstop**
/// (`trait-resolution.md` §5.2, mirroring `RN-E0332`'s `@expansion_limit`). The
/// Paterson check (R6) makes every accepted instance shrink its context, so a
/// well-formed program never approaches this; it exists only so a gap in the
/// static conditions degrades to a loud `RN-E0233` rather than a hung compile.
const RESOLUTION_DEPTH_BUDGET: usize = 100;

/// A **resolved dictionary expression** (traits v1, `trait-resolution.md` §1.2)
/// — the evidence Sprint 3's dictionary-passing lowering emits at a call site.
/// Built by the entailment pass when an obligation is discharged.
#[derive(Clone, Debug)]
pub enum DictEvidence {
    /// Discharged by a **concrete instance** `instance Trait head` (R1 step 2). The
    /// `subdicts` are the evidence for the instance's `requires` context
    /// constraints (e.g. the `Eq Int` super-dictionary embedded in `Ord Int`),
    /// recursively resolved. Sprint 3 emits the instance's dictionary *literal*,
    /// embedding each sub-dictionary (O-T1 lean-embed).
    Instance {
        trait_name: String,
        head: Type,
        subdicts: Vec<DictEvidence>,
    },
    /// Discharged by a **dictionary in scope** (R1 step 1) — the caller's evidence
    /// for an abstract variable, threaded as a hidden parameter. Recorded for
    /// completeness; the *end-of-elaboration* resolver never emits this (such
    /// obligations were lifted into a scheme by `generalize` and never reach it),
    /// but the variant documents the discharge Sprint 3 threads.
    #[allow(dead_code)]
    DictParam {
        constraint: crate::syntax::Constraint,
    },
}

/// One discharged top-level obligation + its resolved [`DictEvidence`] — the
/// Sprint-3 side-table entry. (Sprint 3 maps the obligation back to its
/// originating trait-method call site; see the module note in
/// [`check_trait_obligations`].)
#[derive(Clone, Debug)]
pub struct ResolvedObligation {
    pub constraint: crate::syntax::Constraint,
    pub evidence: DictEvidence,
}

/// Drain the resolved dictionary evidence accumulated during the last
/// [`elaborate`] (the Sprint-3 bridge). Returns one [`ResolvedObligation`] per
/// top-level obligation discharged against a concrete instance, in resolution
/// order.
pub fn take_dict_evidence() -> Vec<ResolvedObligation> {
    EVIDENCE.with(|e| std::mem::take(&mut *e.borrow_mut()))
}

/// **Validate the type-argument arity of every nominal in an annotation type**
/// (S3, D14). Walks `t`; for each `Named(name, args)` whose `name` is a
/// **declared** sum (in `TYENV.sums`), the number of arguments must equal the
/// sum's declared parameter count, else `ArityMismatch` (RN-E0225 — e.g.
/// `List[Int, Bool]` for a one-parameter `List`). A nominal that is **not** a
/// declared sum is left alone — it may be a type parameter (inside a `type`
/// declaration's field types) or a forward/undeclared reference, which the
/// value path reports separately (`UnknownCtor`) and `unify`'s same-arity check
/// backstops. Recurses through the structural types so a nested `(List[Int,Bool],
/// Int)` is caught too.
fn validate_named_arity(t: &Type) -> Result<(), TypeErr> {
    match t {
        Type::Named(name, args) => {
            if let Some((params, _)) = TYENV.with(|e| e.borrow().sums.get(name).cloned()) {
                if params.len() != args.len() {
                    return Err(TypeErr::ArityMismatch {
                        name: name.clone(),
                        expected: params.len(),
                        found: args.len(),
                    });
                }
            }
            for a in args {
                validate_named_arity(a)?;
            }
            Ok(())
        }
        Type::Fun(a, b, _) => {
            validate_named_arity(a)?;
            validate_named_arity(b)
        }
        Type::Code(t2, _) => validate_named_arity(t2),
        Type::Array(e) => validate_named_arity(e),
        Type::Vector(_, e) => validate_named_arity(e),
        Type::Tuple(ts) => {
            for x in ts {
                validate_named_arity(x)?;
            }
            Ok(())
        }
        Type::Record(fs) => {
            for (_, x) in fs {
                validate_named_arity(x)?;
            }
            Ok(())
        }
        // Base types / vars: nothing to check.
        _ => Ok(()),
    }
}

/// **Type-declaration well-formedness (P2)** — the *structural* checks that must
/// run before the registration `HashMap`s silently dedup. Rejects a duplicate
/// type **parameter** (`type T[a, a]`, where the param substitution would collapse
/// the second `a`) and a duplicate **constructor** (`type T = A | A`, where the
/// second would overwrite the first). The *arity* of nominal uses in field types
/// is checked separately, after registration, by [`validate_named_arity`].
fn check_typedef_decl(
    name: &str,
    params: &[String],
    variants: &[(String, Vec<Type>)],
) -> Result<(), TypeErr> {
    let mut seen = std::collections::HashSet::new();
    for p in params {
        if !seen.insert(p.as_str()) {
            return Err(TypeErr::DuplicateTypeParam {
                ty: name.to_string(),
                param: p.clone(),
            });
        }
    }
    let mut seen_ctors = std::collections::HashSet::new();
    for (ctor, _) in variants {
        if !seen_ctors.insert(ctor.as_str()) {
            return Err(TypeErr::DuplicateConstructor {
                ty: name.to_string(),
                ctor: ctor.clone(),
            });
        }
    }
    Ok(())
}

/// **Rewrite declared type-parameter names to their quantified vars** (S3, D10)
/// — `subst_ty`'s *declaration-time* sibling. A field type written `a` parses to
/// `Named("a", [])`; if `"a"` is one of the sum's parameters, this swaps it for
/// the fresh `Type::Var` the parameter maps to. Every other nominal name (an
/// actual sum reference, recursive or not — `List[a]`, `Int`-via-`Named`-never)
/// is left as a `Named`, but its **arguments are rewritten recursively** (so the
/// `a` inside `List[a]` becomes the param var). Structural types recurse; base
/// types are returned as-is.
///
/// This is what makes a constructor's `fields`/`result` reference the shared
/// `ty_params` ids, so `instantiate_ctor` can rename them all consistently.
fn rewrite_params(t: &Type, subst: &HashMap<String, Type>) -> Type {
    match t {
        // A bare nominal that names a parameter → its quantified var. (A param is
        // always written `a`, i.e. zero args; a parameter applied to arguments is
        // not part of the surface — `a[Int]` would be a higher-kinded use we do
        // not support — so we only rewrite the zero-arg, name-matches case.)
        Type::Named(n, args) if args.is_empty() && subst.contains_key(n) => subst[n].clone(),
        // Any other nominal: keep the name, rewrite its arguments (recursion into
        // `List[a]`, `Pair[a, b]`, …).
        Type::Named(n, args) => Type::Named(
            n.clone(),
            args.iter().map(|x| rewrite_params(x, subst)).collect(),
        ),
        Type::Fun(a, b, r) => Type::Fun(
            Box::new(rewrite_params(a, subst)),
            Box::new(rewrite_params(b, subst)),
            r.clone(),
        ),
        Type::Code(t2, r) => Type::Code(Box::new(rewrite_params(t2, subst)), r.clone()),
        Type::Array(e) => Type::Array(Box::new(rewrite_params(e, subst))),
        Type::Vector(shape, e) => Type::Vector(*shape, Box::new(rewrite_params(e, subst))),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|x| rewrite_params(x, subst)).collect()),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, x)| (n.clone(), rewrite_params(x, subst)))
                .collect(),
        ),
        // Base types (and `Var`, which a declared field type never contains) — id.
        _ => t.clone(),
    }
}

/// **Turn an annotation's free lowercase names into fresh type variables** (S3.5,
/// D15). In a type annotation a lowercase-leading, zero-arg nominal — `a` in
/// `List[a]`, `(a -> b)` — is a *type variable*, not a nominal sum: Locus's real
/// types are all `Uppercase` (`Int`/`Bool` are base types, sums are `List`/`Option`),
/// so the case split is unambiguous. Each distinct name maps to one fresh
/// `Type::Var` (consistent within the one annotation), and [`rewrite_params`] does
/// the substitution. Applied at the two annotation-entry sites — a `Lam` parameter
/// and a `let rec` signature — *before* [`validate_named_arity`] (which then sees a
/// `Var` and skips it). The fresh vars are born at the store's **current level**, so
/// the enclosing `let` / `let rec` generalises them.
fn instantiate_annotation(store: &mut unify::UnifStore, ty: &Type) -> Type {
    let mut names = Vec::new();
    collect_annotation_vars(ty, &mut names);
    let subst: HashMap<String, Type> = names.into_iter().map(|n| (n, store.fresh_ty())).collect();
    let mut row_names = Vec::new();
    collect_annotation_row_vars(ty, &mut row_names);
    let row_subst: HashMap<RowVarId, RowVarId> = row_names
        .into_iter()
        .map(|id| (id, store.fresh_row()))
        .collect();
    rewrite_annotation_vars(ty, &subst, &row_subst)
}

/// The distinct lowercase-leading, zero-arg nominal names in `ty` (an annotation's
/// free type variables), in first-occurrence order.
fn collect_annotation_vars(ty: &Type, out: &mut Vec<String>) {
    match ty {
        Type::Named(n, args)
            if args.is_empty() && n.chars().next().map_or(false, char::is_lowercase) =>
        {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
        Type::Named(_, args) => {
            for a in args {
                collect_annotation_vars(a, out);
            }
        }
        Type::Fun(a, b, _) => {
            collect_annotation_vars(a, out);
            collect_annotation_vars(b, out);
        }
        Type::Code(t, _) => collect_annotation_vars(t, out),
        Type::Array(e) => collect_annotation_vars(e, out),
        Type::Vector(_, e) => collect_annotation_vars(e, out),
        Type::Tuple(ts) => ts.iter().for_each(|x| collect_annotation_vars(x, out)),
        Type::Record(fs) => fs.iter().for_each(|(_, x)| collect_annotation_vars(x, out)),
        _ => {}
    }
}

/// The distinct parser-placeholder row variables in an annotation, in
/// first-occurrence order. Parser placeholders are high-numbered ids; they are
/// not valid unification-store ids until this annotation pass rewrites them.
fn collect_annotation_row_vars(ty: &Type, out: &mut Vec<RowVarId>) {
    fn row(r: &Row, out: &mut Vec<RowVarId>) {
        for &id in r.tail_set() {
            if id.parsed_index().is_some() && !out.contains(&id) {
                out.push(id);
            }
        }
    }

    match ty {
        Type::Fun(a, b, r) => {
            collect_annotation_row_vars(a, out);
            collect_annotation_row_vars(b, out);
            row(r, out);
        }
        Type::Code(t, r) => {
            collect_annotation_row_vars(t, out);
            row(r, out);
        }
        Type::Array(e) => collect_annotation_row_vars(e, out),
        Type::Vector(_, e) => collect_annotation_row_vars(e, out),
        Type::Tuple(ts) => ts.iter().for_each(|x| collect_annotation_row_vars(x, out)),
        Type::Record(fs) => fs
            .iter()
            .for_each(|(_, x)| collect_annotation_row_vars(x, out)),
        Type::Named(_, args) => args
            .iter()
            .for_each(|x| collect_annotation_row_vars(x, out)),
        _ => {}
    }
}

fn rewrite_annotation_vars(
    t: &Type,
    ty_subst: &HashMap<String, Type>,
    row_subst: &HashMap<RowVarId, RowVarId>,
) -> Type {
    match t {
        Type::Named(n, args) if args.is_empty() && ty_subst.contains_key(n) => ty_subst[n].clone(),
        Type::Named(n, args) => Type::Named(
            n.clone(),
            args.iter()
                .map(|x| rewrite_annotation_vars(x, ty_subst, row_subst))
                .collect(),
        ),
        Type::Fun(a, b, r) => Type::Fun(
            Box::new(rewrite_annotation_vars(a, ty_subst, row_subst)),
            Box::new(rewrite_annotation_vars(b, ty_subst, row_subst)),
            rewrite_annotation_row(r, row_subst),
        ),
        Type::Code(t2, r) => Type::Code(
            Box::new(rewrite_annotation_vars(t2, ty_subst, row_subst)),
            rewrite_annotation_row(r, row_subst),
        ),
        Type::Array(e) => Type::Array(Box::new(rewrite_annotation_vars(e, ty_subst, row_subst))),
        Type::Vector(shape, e) => Type::Vector(
            *shape,
            Box::new(rewrite_annotation_vars(e, ty_subst, row_subst)),
        ),
        Type::Tuple(ts) => Type::Tuple(
            ts.iter()
                .map(|x| rewrite_annotation_vars(x, ty_subst, row_subst))
                .collect(),
        ),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, x)| (n.clone(), rewrite_annotation_vars(x, ty_subst, row_subst)))
                .collect(),
        ),
        _ => t.clone(),
    }
}

fn rewrite_annotation_row(r: &Row, row_subst: &HashMap<RowVarId, RowVarId>) -> Row {
    let tails = r
        .tail_set()
        .iter()
        .filter_map(|&id| {
            if id.parsed_index().is_some() {
                row_subst.get(&id).copied()
            } else {
                Some(id)
            }
        })
        .collect();
    Row::with_tails(r.label_set().clone(), tails)
}

/// Resolve the type far enough to classify storage without applying D6's
/// `Var -> Int` default. A direct unsolved variable remains a scalar
/// placeholder with `known = false`.
fn storage_layout(ty: &Type) -> ValueLayout {
    unify::with_store(|s| unify::resolve_ty(s, ty)).storage_layout()
}

fn layout_known<'a>(tys: impl IntoIterator<Item = &'a Type>) -> bool {
    ValueLayout::aggregate(tys.into_iter().map(storage_layout)).known
}

fn ctor_field_slot(fields: &[Type], i: usize) -> (usize, ValueLayout) {
    // Classify by the **structural** (un-resolved) field type — `Type::storage_layout`,
    // NOT the resolving local `storage_layout` — so the read addresses the same
    // region/slot the store wrote. The store side keys on the declared field type
    // (`decl_fty.storage_layout()` at the Construct arm), where a param `Var` is a
    // `word_cell` in the pointer region. The match's `inst_fields[i]` is still a
    // `Var` *node* even though the store has bound it to `Int` via refinement;
    // resolving it here (the old bug) collapsed `word_cell -> scalar_cell`, slotting
    // the element binder into the scalar region (`get_scalar`) while the write used
    // `set_word` at a pointer cell — so the element read a constant 0. Both `fields[i]`
    // and the prefix sums must use the structural layout to stay consistent.
    let layout = fields[i].storage_layout();
    if layout.is_gc_reachable() {
        let slot = fields[..i]
            .iter()
            .map(Type::storage_layout)
            .map(|layout| layout.pointer_cells)
            .sum();
        (slot, layout)
    } else {
        let slot = 1 + fields[..i]
            .iter()
            .map(Type::storage_layout)
            .map(|layout| layout.scalar_cells)
            .sum::<usize>();
        (slot, layout)
    }
}

/// Elaborate `Γ ⊢ e : A ! E @ stage` into a fully decorated, **zonked** tree.
///
/// The public entry: it (1) resets the thread-local unification store so each
/// program starts from a clean substitution, (2) runs the recursive worker
/// [`elaborate_inner`] (which threads typing demands through `unify`), then (3)
/// **zonks** the whole tree (D6) — replacing every solved variable by its
/// solution and defaulting any residue (unbound type var → `Int`, unbound row
/// tail → closed). After this no [`Type::Var`] survives, so IR/stage see ground
/// types exactly as before. In S1 no variable is ever created, so zonk is an
/// identity walk and the result is byte-for-byte the monomorphic tree (D5).
pub fn elaborate(sig: &Sig, ctx: &Ctx, stage: Stage, e: &Term) -> Result<Typed, TypeErr> {
    unify::reset_store();
    TYENV.with(|t| *t.borrow_mut() = TyEnv::default());
    TRAITENV.with(|t| *t.borrow_mut() = TraitEnv::default());
    INSTANCES.with(|i| i.borrow_mut().clear());
    TRAIT_DEFS.with(|t| t.borrow_mut().clear());
    SEAL_OBLIGATIONS.with(|s| s.borrow_mut().clear());
    EVIDENCE.with(|e| e.borrow_mut().clear());
    METHOD_USES.with(|m| m.borrow_mut().clear());
    GENERIC_USES.with(|g| g.borrow_mut().clear());
    DICT_PLANS.with(|p| p.borrow_mut().clear());
    CONSTRAINED_DEFS.with(|d| d.borrow_mut().clear());
    DICT_USE_ID.with(|c| c.set(0));
    let tree = elaborate_inner(sig, ctx, stage, e)?;
    // **T0 — tag-completeness (the sole memory-safety gate).** Run BEFORE zonk,
    // while a polymorphic slot still reads `Type::Var` (→ `Repr::Uniform`) and
    // the unification store is live; zonk would default every `Var` to `Int`,
    // erasing the boundary signal. A surviving mismatch is a **compiler bug**
    // (`docs/repr-poly-impl.md` §0), not a user type error — so it panics loudly
    // here rather than returning a `TypeErr`. T0 gates the `Coerce → tag/untag`
    // lowering (T3): that lowering is sound *because* this gate proves every
    // `Var` boundary is coerced and `Repr`-correct for the whole program.
    if let Err(bug) = crate::tagcheck::check_tags(&tree) {
        panic!("{bug}");
    }
    // **Effect transparency through trait dispatch (`trait-resolution.md` §7.3).**
    // Bind each concrete-head method use's latent-row variable to its resolved
    // instance's real method row — *before* zonk defaults the variable to empty.
    // Without this, a variable-row method (`tick : b -> Int ! {|r}`) silently
    // drops the instance's effect from the caller's row, negating the calculus's
    // "every effect is in the type" guarantee.
    bind_method_use_rows();
    let tree = unify::with_store(|s| unify::zonk(s, &tree));
    // **Seal no-escape (RN-E0403).** Now that every row is solved, check each
    // pending `seal L { e }` obligation: the sealed label may not escape through
    // the body's result type (sealing-solution.md §5). Checked here, post-zonk,
    // because an open tail could otherwise absorb `L` after the seal site.
    check_seal_obligations()?;
    // **Trait obligations / entailment (R1, traits v1).** Any obligation a
    // trait-method use recorded (via `instantiate`) that `generalize` did *not*
    // lift into an enclosing scheme is resolved here, post-zonk: it is over a
    // variable that unification either solved to a concrete type (→ unique-instance
    // lookup, R1 step 2) or left genuinely free (→ ambiguity, R7). An obligation
    // discharged by a caller's dictionary (R1 step 1) never reaches here —
    // `generalize` already moved it onto the enclosing scheme's `constraints`.
    check_trait_obligations()?;
    // **Traits v1 Sprint 3 — dictionary-passing lowering.** Sprint 2's checks have
    // passed (every obligation is dischargeable). Now (1) resolve each recorded
    // call site to its dictionary plan (the evidence→call-site bridge, [`Step 1`]),
    // and (2) rewrite the tree so trait methods *run*: a constrained generic gains
    // hidden leading dictionary parameters, a trait-method use projects its method
    // from the resolved dictionary, and a constrained-generic call threads the
    // dictionaries as leading arguments. The transform produces only ordinary
    // `Record`/`Field`/`Lam`/`App`/`Var` nodes — reusing the existing record +
    // closure lowering, introducing no runtime mechanism (`trait-resolution.md`
    // §7.5) — and strips every `DICT_SENTINEL` tag, so `stage_reduce`, `ir`, and
    // the backend never see a trait construct. The store is still live, so it
    // zonks the freshly-injected instance method closures here.
    resolve_dict_plans()?;
    let tree = dict_pass(&tree, &DictScope::default());
    Ok(tree)
}

/// **Entailment / resolution pass (R1, `trait-resolution.md` §1.2).** Drain the
/// pending trait obligations and discharge each. An obligation reaches here only
/// if `generalize` did *not* lift it onto an enclosing scheme — i.e. its variable
/// was not quantified — so its type is, after unification, either:
///
/// - a **bare unbound variable** (`resolve_ty` leaves a `Var`): nothing in the
///   term's type determines it and no caller dictionary covers it ⇒ **ambiguous**
///   (`RN-E0234`, R7). v1 does no defaulting.
/// - a **fixed type** (a base type / `Named` head / structural type): resolved by
///   the unique-instance lookup (R1 step 2). No matching instance ⇒ `RN-E0230`;
///   an instance whose `requires` superclass has no instance ⇒ `RN-E0236`.
///
/// The discharge of each top-level obligation records a [`DictEvidence`] tree in
/// [`EVIDENCE`] (the Sprint-3 bridge).
///
/// **Sprint-3 note (evidence → call site).** Resolution records evidence *per
/// obligation*, in the order obligations were emitted by `instantiate` at each
/// trait-method use. Sprint 3 maps an entry back to its call site by replaying
/// the same emission order while lowering, or (the cleaner follow-on) by keying
/// the obligation on a call-site id stamped at `instantiate` time. For abstract
/// obligations, Sprint 3 instead reads the enclosing `Scheme::constraints`
/// (`generalize`'s lifted dictionaries) for the hidden leading dict parameters.
fn check_trait_obligations() -> Result<(), TypeErr> {
    let obligations = unify::take_obligations();
    for c in obligations {
        // R1 step 1 (dictionary-in-scope) was discharged by `generalize` for any
        // quantified variable; what survives is resolved by its now-fixed type.
        let ty = unify::with_store(|s| unify::resolve_ty(s, &c.ty));
        if is_unresolved_var(&ty) {
            // R7 — the constraint's variable is not determined by the term's
            // visible type (it survived generalization unquantified and unsolved).
            let ty = unify::with_store(|s| unify::zonk_ty(s, &ty));
            return Err(TypeErr::TraitAmbiguous {
                constraint: crate::syntax::Constraint {
                    trait_name: c.trait_name,
                    ty,
                },
            });
        }
        let ty = unify::with_store(|s| unify::zonk_ty(s, &ty));
        let constraint = crate::syntax::Constraint {
            trait_name: c.trait_name.clone(),
            ty: ty.clone(),
        };
        let evidence = resolve_instance(&c.trait_name, &ty, &constraint, 0)?;
        EVIDENCE.with(|e| {
            e.borrow_mut().push(ResolvedObligation {
                constraint: constraint.clone(),
                evidence,
            })
        });
    }
    Ok(())
}

/// Is `ty` (already `resolve_ty`'d) a still-unbound unification variable — the R7
/// ambiguity signal? (A bare top-level `Var`; `resolve_ty` does not default it.)
fn is_unresolved_var(ty: &Type) -> bool {
    matches!(ty, Type::Var(_))
}

/// **Bind each trait-method use's latent row to its resolved instance's row**
/// (`trait-resolution.md` §7.3 — "the resolved method's row surfaces in the
/// caller's row exactly as a direct call would"). Runs **pre-zonk**, store live.
///
/// A method declared with a *variable* row (`tick : b -> Int ! {|r}`) instantiates
/// a fresh `?r` at every use (`mint_method_scheme` quantifies the row var). The App
/// rule unions `?r` into the caller, but nothing else constrains it — so zonk
/// defaults it to the empty row and the *resolved instance's* real effect vanishes
/// from the caller's row. That is silent effect under-reporting: the worst failure
/// for a language whose contract is "every effect is in the type".
///
/// The fix: for each method use whose obligation resolves to a **concrete head**,
/// unify the use-site method type (carrying `?r`) against the instance's actual
/// method type (carrying its real latent row) — binding `?r` to that row before
/// zonk. *Abstract* uses (head still a variable, e.g. a method called on a
/// constrained generic's own parameter) are left untouched: their row rides the
/// enclosing scheme via generalization, exactly as for ordinary row polymorphism.
/// A fixed-row method is a no-op here (no variable to bind).
fn bind_method_use_rows() {
    let uses = METHOD_USES.with(|m| m.borrow().clone());
    for u in uses {
        let head = unify::with_store(|s| unify::resolve_ty(s, &u.obligation_ty));
        // Abstract use — the constraint variable is undetermined here; the row is
        // carried by the enclosing scheme, not resolvable to an instance row yet.
        if is_unresolved_var(&head) {
            continue;
        }
        let Some(info) = lookup_instance(&u.trait_name, &head) else {
            continue; // no instance (a real error) — surfaced by check_trait_obligations
        };
        let Some((_, body, _)) = info.method_bodies.iter().find(|(m, ..)| *m == u.method) else {
            continue;
        };
        // Unify the use-site method type with the instance's actual method type.
        // For a ground-head instance the instance type is concrete, so this only
        // ever *binds* the use's free latent-row variable (and re-checks the
        // already-agreeing domain/codomain); it never constrains the instance.
        let inst_ty = body.ty.clone();
        let _ = unify::with_store(|s| unify::unify(s, &u.use_ty, &inst_ty));
    }
}

/// **Generalize `ty`, but first pin any ready trait-method rows.** A `let`
/// generalizes its RHS *during* elaboration, before the end-of-program method-row
/// pass ([`bind_method_use_rows`]) runs — so a helper like `let save = fn c =>
/// db_exec c "…"` would quantify the method's still-free latent-row variable into
/// `save`'s scheme and *lose* the instance's effect. Binding the ground-head
/// method rows here, immediately before [`generalize`], keeps those rows out of
/// the quantified set: `save`'s effect becomes concrete, not spuriously
/// polymorphic. No-op (and zero-cost) for any program with no trait-method uses.
fn generalize_resolved(ty: &Type) -> Scheme {
    bind_method_use_rows();
    unify::with_store(|s| generalize(s, ty))
}

/// **R1 step 2 — discharge `Trait ty` by the unique matching instance.** Looks up
/// the instance keyed by `(trait, head-constructor)` (coherence/non-overlap make
/// this a function, never a search), one-way-matches its head against `ty` to
/// bind the instance's head variables, turns each `requires` context constraint
/// into a recursive sub-obligation under that binding, and builds the
/// [`DictEvidence`]. `depth` is the recursion fuel (R6 backstop).
///
/// Failures: no instance for the head ⇒ `RN-E0230`; a `requires` sub-obligation
/// with no instance ⇒ `RN-E0236` (superclass-unsatisfied); fuel exhausted ⇒
/// `RN-E0233` (resolution-diverges backstop).
fn resolve_instance(
    trait_name: &str,
    ty: &Type,
    constraint: &crate::syntax::Constraint,
    depth: usize,
) -> Result<DictEvidence, TypeErr> {
    if depth > RESOLUTION_DEPTH_BUDGET {
        return Err(TypeErr::TraitResolutionDiverges {
            trait_name: trait_name.to_string(),
            head: ty.clone(),
            context: constraint.clone(),
            why: format!(
                "resolution exceeded the depth budget ({RESOLUTION_DEPTH_BUDGET}) — the \
                 instance chain does not bottom out"
            ),
        });
    }
    let key = head_key(ty);
    // The unique instance for `(trait, head)`. Coherence (R3) + non-overlap (R4),
    // enforced at each instance decl, guarantee at most one candidate, so this is
    // a keyed lookup — no tie-breaking, no backtracking.
    let inst = INSTANCES.with(|t| {
        t.borrow()
            .iter()
            .find(|i| i.trait_name == trait_name && key.as_deref() == head_key(&i.head).as_deref())
            .cloned()
    });
    let Some(inst) = inst else {
        return Err(TypeErr::TraitNoInstance {
            constraint: constraint.clone(),
        });
    };
    // One-way match the instance head against `ty`: instance head variables bind,
    // `ty`'s structure is fixed. Yields the binding from head-var name → subterm.
    let mut bindings: HashMap<String, Type> = HashMap::new();
    // The match cannot fail given the head-key agreement, but a structural
    // mismatch (e.g. arity) falls back to a no-instance failure rather than panic.
    if !match_head(&inst.head, ty, &mut bindings) {
        return Err(TypeErr::TraitNoInstance {
            constraint: constraint.clone(),
        });
    }
    // **Traits v1 lowering gate (`RN-E0246`).** The selected instance's HEAD is
    // non-ground (it contains a type variable — `Show [a]`, `Show (Pair a b)`),
    // i.e. a *generic instance*. v1 builds method closures for such an instance in
    // an empty dictionary scope, so a method-internal sub-dictionary resolves to an
    // unbound `$dict$…` — at best a cryptic "unbound variable" at codegen, at worst
    // a silent capture (in practice the body's `match`/use surfaces a misleading
    // downstream type error). v1 cannot lower a generic instance, so reject *using*
    // it loud and clear here (resolution returns `Result`, the earliest clean
    // point), rather than at the infallible `build_dict`. NOTE: the discriminator
    // is a **non-ground HEAD** — a ground-head instance with a superclass / context
    // `requires` (`instance Ord Int requires Eq Int`) still lowers and is untouched.
    // Declaring a generic instance stays legal (Paterson etc. still check it at the
    // declaration site); only its runtime use is rejected in v1.
    if head_is_non_ground(&inst.head) {
        return Err(TypeErr::TraitV1Unsupported {
            what: format!(
                "a generic instance `{} {}` (its head contains a type variable); traits v1 runs \
                 only ground-head instances (`instance {} Int`) — use it at a concrete type via a \
                 concrete wrapper (a newtype this module owns), or await recursive-instance \
                 lowering",
                inst.trait_name, inst.head, inst.trait_name
            ),
        });
    }
    let mut subdicts = Vec::new();
    // (a) **Trait superclasses** (R1.4): `trait Ord a requires Eq a` ⟹ resolving
    // `Ord τ` also needs `Eq τ`. Substitute the trait param with `τ` and recurse;
    // a missing superclass instance is `RN-E0236` (superclass-unsatisfied).
    for sup in &inst.trait_supers {
        let one = std::iter::once((inst.trait_param.clone(), ty.clone())).collect();
        let sub_ty = subst_named_vars(&sup.ty, &one);
        let sub_constraint = crate::syntax::Constraint {
            trait_name: sup.trait_name.clone(),
            ty: sub_ty.clone(),
        };
        match resolve_instance(&sup.trait_name, &sub_ty, &sub_constraint, depth + 1) {
            Ok(ev) => subdicts.push(ev),
            Err(TypeErr::TraitNoInstance { .. }) => {
                return Err(TypeErr::TraitSuperclassUnsatisfied {
                    constraint: constraint.clone(),
                    superclass: sub_constraint,
                });
            }
            Err(other) => return Err(other),
        }
    }
    // (b) **Instance context** (`instance Show [a] requires Show a`): each
    // `requires C τ'` becomes a sub-obligation with the head-match binding applied.
    // A missing instance here is also surfaced as superclass-unsatisfied (RN-E0236).
    for req in &inst.requires {
        let sub_ty = subst_named_vars(&req.ty, &bindings);
        let sub_constraint = crate::syntax::Constraint {
            trait_name: req.trait_name.clone(),
            ty: sub_ty.clone(),
        };
        match resolve_instance(&req.trait_name, &sub_ty, &sub_constraint, depth + 1) {
            Ok(ev) => subdicts.push(ev),
            Err(TypeErr::TraitNoInstance { .. }) => {
                return Err(TypeErr::TraitSuperclassUnsatisfied {
                    constraint: constraint.clone(),
                    superclass: sub_constraint,
                });
            }
            Err(other) => return Err(other),
        }
    }
    Ok(DictEvidence::Instance {
        trait_name: trait_name.to_string(),
        head: ty.clone(),
        subdicts,
    })
}

// ── Sprint 3: the evidence→call-site bridge + dictionary-passing lowering ────

/// **Step 1 — resolve each recorded call site to its dictionary plan** (traits v1
/// Sprint 3, `trait-resolution.md` §1.2). Runs after [`check_trait_obligations`]
/// (so every obligation is known dischargeable) with the unification store still
/// live. For each recorded trait-method / constrained-generic use, resolve the
/// obligation type variable it captured at the `Var` site:
///
/// - a **concrete** type ⟹ the unique-instance dictionary literal
///   ([`DictEvidence::Instance`], a monomorphic call site — R1 step 2);
/// - an **unbound** variable ⟹ the obligation was generalized onto an enclosing
///   scheme, so it is discharged by that caller's **hidden dictionary parameter**
///   ([`DictEvidence::DictParam`] — R1 step 1; the polymorphic-threading case).
///
/// The result is the [`DICT_PLANS`] map [`dict_pass`] consumes. Ambiguity (R7) and
/// no-instance (R1.3) were already surfaced by [`check_trait_obligations`]; this
/// pass never re-errors (a concrete head here always has an instance, since the
/// same obligation was pending and discharged), so it cannot regress Sprint 2.
fn resolve_dict_plans() -> Result<(), TypeErr> {
    let method_uses = METHOD_USES.with(|m| m.borrow().clone());
    for u in method_uses {
        let evidence = resolve_obligation_evidence(&u.trait_name, &u.obligation_ty)?;
        DICT_PLANS.with(|p| {
            p.borrow_mut().insert(
                u.id,
                DictPlan::Method {
                    method: u.method,
                    evidence,
                },
            )
        });
    }
    let generic_uses = GENERIC_USES.with(|g| g.borrow().clone());
    for u in generic_uses {
        let mut dicts = Vec::with_capacity(u.obligations.len());
        for (trait_name, ty) in &u.obligations {
            dicts.push(resolve_obligation_evidence(trait_name, ty)?);
        }
        DICT_PLANS.with(|p| p.borrow_mut().insert(u.id, DictPlan::Generic { dicts }));
    }
    Ok(())
}

/// Resolve one obligation type variable (against the live store) to a
/// [`DictEvidence`] (traits v1 Sprint 3): a concrete head ⟹ the instance
/// dictionary literal; an unbound variable ⟹ a threaded caller dictionary
/// parameter. See [`resolve_dict_plans`].
fn resolve_obligation_evidence(trait_name: &str, ty: &Type) -> Result<DictEvidence, TypeErr> {
    let resolved = unify::with_store(|s| unify::resolve_ty(s, ty));
    if is_unresolved_var(&resolved) {
        // Generalized onto an enclosing scheme — the caller threads its dictionary
        // parameter for this trait (R1 step 1).
        Ok(DictEvidence::DictParam {
            constraint: crate::syntax::Constraint {
                trait_name: trait_name.to_string(),
                ty: resolved,
            },
        })
    } else {
        let head = unify::with_store(|s| unify::zonk_ty(s, &resolved));
        let constraint = crate::syntax::Constraint {
            trait_name: trait_name.to_string(),
            ty: head.clone(),
        };
        resolve_instance(trait_name, &head, &constraint, 0)
    }
}

/// The **lexical dictionary scope** of [`dict_pass`] (traits v1 Sprint 3): the
/// trait→parameter-name bindings in force, i.e. the hidden dictionary parameters
/// of every enclosing constrained generic. A [`DictEvidence::DictParam`] for trait
/// `T` lowers to a reference to `params[T]` (`$dict$T`).
#[derive(Clone, Default)]
struct DictScope {
    params: std::collections::HashMap<String, String>,
}

impl DictScope {
    /// Extend the scope with one constrained generic's hidden dictionary params
    /// (one per `trait`), each named [`dict_param_name`].
    fn with_params(&self, traits: &[String]) -> DictScope {
        let mut params = self.params.clone();
        for t in traits {
            params.insert(t.clone(), dict_param_name(t));
        }
        DictScope { params }
    }
}

/// **Step 2 — the dictionary-passing transform** (traits v1 Sprint 3,
/// `object-system-design.md` §4). A post-zonk tree rewrite that makes a
/// constrained generic *run*. It walks the decorated tree and:
///
/// - **constrained-generic definition** (`Node::Let` binder tagged `D`): wraps the
///   bound value in one hidden leading **dictionary parameter** per constraint
///   (`fn $dict$Ord => …`), and strips the binder tag back to the plain name;
/// - **trait-method use** (`Node::Var` tagged `M`): projects the method field from
///   the resolved dictionary and (the enclosing `App` then) calls it — a monomorphic
///   site builds the instance **dictionary literal**, an abstract site references
///   the threaded `$dict$T` **parameter**;
/// - **constrained-generic use** (`Node::Var` tagged `G`): applies the resolved
///   dictionaries as **leading arguments** (`min2 $dict …`), threading a parameter
///   onward where the caller is itself polymorphic;
/// - every other node: a structural copy, recursing with the scope extended at each
///   constrained-generic body.
///
/// The dictionary value is an ordinary **record handle** whose fields are method
/// `Fun` closures and embedded `super_<Trait>` sub-dictionaries (O-T1 lean-embed);
/// it reuses the existing record + closure lowering verbatim (`trait-resolution.md`
/// §7.5 — no new runtime mechanism, no new traced-store hazard).
fn dict_pass(t: &Typed, scope: &DictScope) -> Typed {
    let node = match &t.node {
        // ── a tagged `Var` — a constrained use to rewrite ────────────────────
        Node::Var(name) => match dict_untag(name) {
            Some((base, 'M', id)) => {
                // A trait-method use *not in function position* — a bare first-class
                // reference (`let f = show in …`) or an argument. **R2 devirt fires
                // only for the directly-applied call** (the `Node::App` arm below);
                // a bare reference keeps the dictionary projection. Devirtualizing it
                // would substitute the *concrete* method body for a value whose type
                // is the method's **uniform ABI** arrow (the dictionary field type a
                // later application coerces its argument into) — handing the concrete
                // body a tagged word and miscomputing. The dictionary field is
                // `wrap_callback`-wrapped to exactly that uniform ABI, so the
                // projection is the correct lowering for a value-position method use.
                let plan = DICT_PLANS.with(|p| p.borrow().get(&id).cloned());
                match plan {
                    Some(DictPlan::Method {
                        evidence, method, ..
                    }) => {
                        let dict = build_dict(&evidence, scope);
                        // `t.ty` is the method's instantiated arrow — the projected
                        // field's type. Project it from the dictionary record.
                        return Typed::at_layout(
                            t.ty.clone(),
                            t.row.clone(),
                            t.stage,
                            t.layout_known,
                            Node::Field(Box::new(dict), method),
                        );
                    }
                    // No plan (should not happen) — degrade to the bare name.
                    _ => Node::Var(base.to_string()),
                }
            }
            Some((base, 'G', id)) => {
                // Constrained-generic use: apply the resolved dictionaries as
                // leading arguments (`min2 $dict$Ord …`). The lowered definition
                // takes one hidden dictionary parameter per constraint *ahead* of
                // its written parameters, so `Var(base)` has type
                // `dict₀ -> … -> dictₙ -> τ` where `τ = t.ty` is the use's
                // (instance-solved) written type. Each application peels one
                // dictionary arrow.
                let plan = DICT_PLANS.with(|p| p.borrow().get(&id).cloned());
                let dicts = match plan {
                    Some(DictPlan::Generic { dicts }) => dicts,
                    _ => Vec::new(),
                };
                let built: Vec<Typed> = dicts.iter().map(|ev| build_dict(ev, scope)).collect();
                // Build `Var(base)`'s type: the dictionary arrows in front of `t.ty`.
                let mut var_ty = t.ty.clone();
                for d in built.iter().rev() {
                    var_ty = Type::Fun(Box::new(d.ty.clone()), Box::new(var_ty), Row::pure());
                }
                let mut acc = Typed::at_layout(
                    var_ty,
                    t.row.clone(),
                    t.stage,
                    t.layout_known,
                    Node::Var(base.to_string()),
                );
                // Apply each dictionary; the result type peels one leading arrow.
                for dict in built {
                    let res_ty = match &acc.ty {
                        Type::Fun(_, cod, _) => (**cod).clone(),
                        _ => t.ty.clone(),
                    };
                    acc = Typed::at_layout(
                        res_ty,
                        t.row.clone(),
                        t.stage,
                        t.layout_known,
                        Node::App {
                            fun: Box::new(acc),
                            arg: Box::new(dict),
                        },
                    );
                }
                return acc;
            }
            // An untagged var, or a `D`-tagged name reaching here as a value
            // reference (a constrained generic referenced bare) — copy the base.
            Some((base, _, _)) => Node::Var(base.to_string()),
            None => Node::Var(name.clone()),
        },

        // ── a `let` — possibly a constrained-generic definition ──────────────
        Node::Let { name, bound, body } => {
            if let Some((base, 'D', id)) = dict_untag(name) {
                let traits = CONSTRAINED_DEFS
                    .with(|d| d.borrow().get(&id).cloned())
                    .unwrap_or_default();
                let inner = scope.with_params(&traits);
                // The bound value runs under the hidden dictionary parameters, so
                // transform it in the extended scope, then wrap it in one `Lam`
                // per constraint (outermost = first constraint).
                let mut lowered_bound = dict_pass(bound, &inner);
                for trait_name in traits.iter().rev() {
                    let param = dict_param_name(trait_name);
                    let dict_ty = dict_record_type(trait_name);
                    let body_ty = lowered_bound.ty.clone();
                    let body_row = lowered_bound.row.clone();
                    lowered_bound = Typed::at(
                        Type::Fun(Box::new(dict_ty.clone()), Box::new(body_ty), Row::pure()),
                        Row::pure(),
                        lowered_bound.stage,
                        Node::Lam {
                            param,
                            param_ty: dict_ty,
                            body: Box::new(Typed {
                                row: body_row,
                                ..lowered_bound
                            }),
                        },
                    );
                }
                Node::Let {
                    name: base.to_string(),
                    bound: Box::new(lowered_bound),
                    body: Box::new(dict_pass(body, scope)),
                }
            } else {
                Node::Let {
                    name: name.clone(),
                    bound: Box::new(dict_pass(bound, scope)),
                    body: Box::new(dict_pass(body, scope)),
                }
            }
        }

        // ── a flat block — same binding rules as a `let` spine, without the spine
        Node::Block { items, body } => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    TypedBlockItem::Let { name, bound } => {
                        if let Some((base, 'D', id)) = dict_untag(name) {
                            let traits = CONSTRAINED_DEFS
                                .with(|d| d.borrow().get(&id).cloned())
                                .unwrap_or_default();
                            let inner = scope.with_params(&traits);
                            let mut lowered_bound = dict_pass(bound, &inner);
                            for trait_name in traits.iter().rev() {
                                let param = dict_param_name(trait_name);
                                let dict_ty = dict_record_type(trait_name);
                                let body_ty = lowered_bound.ty.clone();
                                let body_row = lowered_bound.row.clone();
                                lowered_bound = Typed::at(
                                    Type::Fun(
                                        Box::new(dict_ty.clone()),
                                        Box::new(body_ty),
                                        Row::pure(),
                                    ),
                                    Row::pure(),
                                    lowered_bound.stage,
                                    Node::Lam {
                                        param,
                                        param_ty: dict_ty,
                                        body: Box::new(Typed {
                                            row: body_row,
                                            ..lowered_bound
                                        }),
                                    },
                                );
                            }
                            out.push(TypedBlockItem::Let {
                                name: base.to_string(),
                                bound: lowered_bound,
                            });
                        } else {
                            out.push(TypedBlockItem::Let {
                                name: name.clone(),
                                bound: dict_pass(bound, scope),
                            });
                        }
                    }
                    TypedBlockItem::LetMut { name, bound } => {
                        out.push(TypedBlockItem::LetMut {
                            name: name.clone(),
                            bound: dict_pass(bound, scope),
                        });
                    }
                    TypedBlockItem::LetTuple {
                        names,
                        bound,
                        fields_layout_known,
                    } => {
                        out.push(TypedBlockItem::LetTuple {
                            names: names.clone(),
                            bound: dict_pass(bound, scope),
                            fields_layout_known: *fields_layout_known,
                        });
                    }
                }
            }
            Node::Block {
                items: out,
                body: Box::new(dict_pass(body, scope)),
            }
        }

        // ── an `App` — possibly a *directly applied* trait-method use (R2) ───
        Node::App { .. } => {
            if let Some((body, spine)) = devirt_method_app(t) {
                // **R2 devirt of the direct monomorphic call** — β-inline the
                // instance method body against the (dict-passed) call arguments.
                // No dictionary `Node::Record` is built and no indirect call
                // remains: the body is straight-line at the call site.
                //
                // The call site coerced its scalar/handle arguments into the
                // method's **uniform ABI** (`Tag`/`ToPtr`) — because, not yet
                // devirtualized, it saw the method-arrow's domain as the abstract
                // trait-parameter `Var` (the dictionary field's wrapped uniform
                // type). Devirt knows the concrete instance, so the **unwrapped**
                // concrete body wants the *raw* argument: strip that uniform-ABI
                // coercion (`strip_uniform_coerce`) so `Tag 5` becomes `5` and the
                // body's `Int` parameter receives an `Int`. (The dual result
                // un-coercion is stripped in the `Node::Coerce` arm.)
                let args: Vec<Typed> = spine
                    .iter()
                    .map(|a| dict_pass(strip_uniform_coerce(a), scope))
                    .collect();
                return beta_inline(body, args, &t.ty, &t.row, t.stage);
            }
            // Not a concrete-instance method call (an ordinary call, or a
            // polymorphic `DictParam`/`Generic` use): structural recursion. The
            // `Node::Var` arm handles a threaded-dict method head; the `'G'` arm
            // handles a constrained-generic head.
            map_children(&t.node, &|c| dict_pass(c, scope))
        }

        // ── a result un-coercion around a devirtualized method call (R2) ─────
        // The call site `Untag`s/`FromPtr`s a method's *result* when the method's
        // codomain is the abstract trait parameter (e.g. `Num`'s `add : a->a->a`
        // returns a uniform word at `Int`). Once the inner call is devirtualized,
        // the concrete body already yields a concrete result, so this un-coercion
        // is undone — strip it and devirt the inner `App` directly. (A `Coerce`
        // whose inner is *not* a devirtable method call is ordinary structural
        // recursion.)
        Node::Coerce {
            kind: Coercion::Untag | Coercion::FromPtr,
            inner,
            ..
        } if devirt_method_app(inner).is_some() => {
            let (body, spine) = devirt_method_app(inner).expect("just checked");
            let args: Vec<Typed> = spine
                .iter()
                .map(|a| dict_pass(strip_uniform_coerce(a), scope))
                .collect();
            // The outer `Coerce`'s type/row is the post-un-coercion concrete type —
            // exactly the devirtualized body's result. Stamp it from the `Coerce`.
            return beta_inline(body, args, &t.ty, &t.row, t.stage);
        }

        // ── everything else: structural recursion ───────────────────────────
        other => map_children(other, &|c| dict_pass(c, scope)),
    };
    Typed {
        ty: t.ty.clone(),
        row: t.row.clone(),
        stage: t.stage,
        layout_known: t.layout_known,
        node,
    }
}

/// Build the **dictionary value** for a resolved [`DictEvidence`] (traits v1
/// Sprint 3, `object-system-design.md` §4) — an ordinary record `Typed` node:
///
/// - [`DictEvidence::Instance`]: a `Node::Record` whose fields are the instance's
///   **method closures** (the elaborated bodies stored on [`InstanceInfo`], zonked
///   here against the live store) plus one embedded `super_<Trait>` field per
///   superclass sub-dictionary (O-T1 lean-embed). The method closures are
///   themselves run through [`dict_pass`] (in the *empty* scope — an instance body's
///   own trait-method uses are monomorphic at the instance head, so they resolve to
///   literals, not threaded params).
/// - [`DictEvidence::DictParam`]: a reference to the enclosing generic's hidden
///   dictionary parameter `$dict$T` (the threaded caller dictionary).
fn build_dict(ev: &DictEvidence, scope: &DictScope) -> Typed {
    match ev {
        DictEvidence::DictParam { constraint } => {
            let param = scope
                .params
                .get(&constraint.trait_name)
                .cloned()
                .unwrap_or_else(|| dict_param_name(&constraint.trait_name));
            Typed::at(
                dict_record_type(&constraint.trait_name),
                Row::pure(),
                0,
                Node::Var(param),
            )
        }
        DictEvidence::Instance {
            trait_name,
            head,
            subdicts,
        } => {
            let info = lookup_instance(trait_name, head);
            let mut fields: Vec<(String, Typed)> = Vec::new();
            // Method closures — the instance's elaborated bodies, dict-passed (the
            // empty scope: an instance method's own trait-method uses are at the
            // concrete head, resolving to literals), then **wrapped to the uniform
            // ABI** (`wrap_callback`): a generic caller tags scalar arguments into
            // uniform words, so the wrapper untags each before the concrete body and
            // re-tags its result, balancing the call-site coercion (repr-poly,
            // `docs/repr-poly-impl.md`). The wrap is computed pre-zonk (the literal
            // `Var` of `uniform_arrow` reads `Uniform`), then the whole field is
            // zonked for lowering.
            if let Some(info) = &info {
                for (m, body, uniform_arrow) in &info.method_bodies {
                    let lowered = dict_pass(body, &DictScope::default());
                    let wrapped = wrap_callback(uniform_arrow, &lowered, lowered.stage);
                    let zonked = unify::with_store(|s| unify::zonk(s, &wrapped));
                    fields.push((m.clone(), zonked));
                }
            }
            // Embedded super-dictionaries (O-T1): one `super_<Trait>` field per
            // resolved sub-dictionary, named by the sub-dictionary's trait.
            for sub in subdicts {
                if let Some(name) = dict_evidence_trait(sub) {
                    let sub_dict = build_dict(sub, scope);
                    fields.push((super_field_name(&name), sub_dict));
                }
            }
            // Records are laid out **sorted by name** (the canonical layout —
            // matching `Term::Record` elaboration); sort so the record type and the
            // later `Node::Field` slot resolution agree.
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            let tys: Vec<(String, Type)> = fields
                .iter()
                .map(|(n, t)| (n.clone(), t.ty.clone()))
                .collect();
            let row = fields
                .iter()
                .fold(Row::single(Label::Gc), |r, (_, t)| r.union(&t.row));
            let layout_known = layout_known(tys.iter().map(|(_, t)| t));
            Typed::at_layout(
                Type::Record(tys),
                row,
                0,
                layout_known,
                Node::Record(fields),
            )
        }
    }
}

/// **Devirtualize a concrete-instance method use** (traits v1 **R2**,
/// `trait-resolution.md` §1.3 — the stage-0 erasure). The *unwrapped* method
/// closure `build_dict` would store in the dictionary's `method` field, returned
/// as a standalone value so a direct monomorphic call site can use it **without
/// building the dictionary record** (no `Node::Record`, no GC handle).
///
/// It is the instance's elaborated body, dict-passed in the *empty* scope (an
/// instance method's own trait-method uses are at the concrete head — they resolve
/// to literals, not threaded params), then zonked — but **not** `wrap_callback`-ed:
/// the uniform tag/untag wrapper exists only so a *generic* caller (which tags
/// scalar args into uniform words) can call through the dictionary's uniform ABI.
/// A direct monomorphic site passes concrete, untagged args (`Type::coercion`
/// emits no `Tag`/`Untag` at a concrete→concrete boundary), so the raw concrete
/// body is exactly what the call expects — its type matches the use's instantiated
/// method arrow. Returns `None` only if the instance or method is somehow missing
/// (a layering bug; the caller falls back to the dictionary path).
fn instance_method_body(trait_name: &str, head: &Type, method: &str) -> Option<Typed> {
    let info = lookup_instance(trait_name, head)?;
    let (_, body, _) = info.method_bodies.iter().find(|(m, ..)| m == method)?;
    let lowered = dict_pass(body, &DictScope::default());
    Some(unify::with_store(|s| unify::zonk(s, &lowered)))
}

/// **Compile-time β-inline** a method body against the arguments of a direct
/// monomorphic trait-method call (traits v1 **R2**). The pipeline does **not**
/// β-reduce an immediately-applied lambda — `App(Lam, arg)` lowers the `Lam` to a
/// GC-allocated closure (`ir.rs` lowers the `App`'s `fun` via `atom`, which binds
/// the `Comp::Lam` closure value; `locus-llvm`'s `lower_closure` then
/// `locus_gc_alloc`s it) and then **indirect-calls** it. So to truly erase the
/// dictionary indirection the method body must be reduced *here*, at elaboration.
///
/// `body` is the method closure (a curried `Lam` spine); `args` are the call's
/// spine arguments (already dict-passed), in order. Each argument consumed by a
/// `Lam` parameter is bound by a **`let param = arg in …`** — capture-safe by
/// construction (the binder scopes only the body; `arg` is evaluated in the outer
/// scope, never under `param`), and call-by-value-faithful (the arg is evaluated
/// exactly once, like the indirect call it replaces). Surplus arguments
/// (over-application — the method returns a function) are re-applied with `App`;
/// surplus parameters (partial application) stay as residual `Lam`s (the only case
/// that keeps a closure — a *partially applied* trait method, which still builds
/// no dictionary record). `result_ty`/`result_row` are the original outermost
/// `App`'s, stamped on the reduced expression so the surrounding tree is unchanged.
fn beta_inline(
    body: Typed,
    args: Vec<Typed>,
    result_ty: &Type,
    result_row: &Row,
    stage: Stage,
) -> Typed {
    let mut args = args.into_iter();
    // Peel `Lam` params, binding each to the next argument with a `let`.
    fn peel(f: Typed, args: &mut std::vec::IntoIter<Typed>) -> Typed {
        match f.node {
            Node::Lam { param, body, .. } => match args.next() {
                Some(arg) => {
                    let inner = peel(*body, args);
                    Typed::at_layout(
                        inner.ty.clone(),
                        inner.row.union(&arg.row),
                        inner.stage,
                        inner.layout_known,
                        Node::Let {
                            name: param,
                            bound: Box::new(arg),
                            body: Box::new(inner),
                        },
                    )
                }
                // No more args: a partially applied method — keep the residual
                // lambda (builds no dictionary record; only a closure, as a direct
                // partial application of any function would).
                None => Typed {
                    node: Node::Lam {
                        param,
                        param_ty: match &f.ty {
                            Type::Fun(d, _, _) => (**d).clone(),
                            _ => Type::Unit,
                        },
                        body,
                    },
                    ..f
                },
            },
            // The body is not (or no longer) a lambda but arguments remain: an
            // over-application (the method yields a function). Re-apply the rest.
            _ => f,
        }
    }
    let mut acc = peel(body, &mut args);
    // Any leftover arguments (over-application): apply them with `App`, peeling one
    // arrow off the accumulator type per application.
    for arg in args {
        let res_ty = match &acc.ty {
            Type::Fun(_, cod, _) => (**cod).clone(),
            _ => result_ty.clone(),
        };
        acc = Typed::at_layout(
            res_ty,
            acc.row.union(&arg.row),
            acc.stage,
            acc.layout_known,
            Node::App {
                fun: Box::new(acc),
                arg: Box::new(arg),
            },
        );
    }
    // Stamp the original application's result type/row so the surrounding tree is
    // unperturbed (the reduced expression sits exactly where the call did).
    Typed::at_layout(
        result_ty.clone(),
        result_row.clone(),
        stage,
        acc.layout_known,
        acc.node,
    )
}

/// Count the **arity** (leading `Lam` count) of a method body — how many arguments
/// the curried closure consumes before yielding a value.
fn lam_arity(body: &Typed) -> usize {
    let mut n = 0;
    let mut t = body;
    while let Node::Lam { body, .. } = &t.node {
        n += 1;
        t = body;
    }
    n
}

/// If `t` is a **fully-applied, directly-called trait-method use discharged by a
/// concrete instance** (traits v1 **R2**), return the instance's (unwrapped) method
/// body and the call's spine arguments — the inputs the devirt β-inline consumes.
/// Returns `None` for an ordinary application, a polymorphic `DictParam`/`Generic`
/// head, a method whose body is somehow missing, or a **partial application** —
/// those all keep the dictionary path.
///
/// **Why only the fully-applied case.** The call site coerces its arguments to the
/// method's *uniform ABI* (a scalar `Tag`); devirt strips that coercion and feeds
/// the raw value to the *concrete* body — which is sound only when **every** param
/// is bound here, because each consumed arg's coercion is stripped 1:1. A partial
/// application (`let f = eq 3 in …`) would leave a residual closure whose value is
/// later applied through the **uniform ABI** the original (un-devirtualized) call
/// site still expects — handing the concrete body a *tagged* word and miscomputing.
/// So a partial application stays on the dictionary path (the dictionary field is
/// `wrap_callback`-wrapped to that uniform ABI), where the residual closure is
/// correct. Full application is the direct-call case the sprint targets.
fn devirt_method_app(t: &Typed) -> Option<(Typed, Vec<&Typed>)> {
    let mut spine: Vec<&Typed> = Vec::new();
    let mut head = t;
    while let Node::App { fun, arg } = &head.node {
        spine.push(arg);
        head = fun;
    }
    spine.reverse();
    let Node::Var(name) = &head.node else {
        return None;
    };
    let (_, 'M', id) = dict_untag(name)? else {
        return None;
    };
    match DICT_PLANS.with(|p| p.borrow().get(&id).cloned()) {
        Some(DictPlan::Method {
            evidence: DictEvidence::Instance {
                trait_name, head, ..
            },
            method,
        }) => {
            let body = instance_method_body(&trait_name, &head, &method)?;
            // Only the fully-applied call is devirtualized (see the doc comment):
            // a partial application keeps a residual closure whose later use goes
            // through the uniform ABI, so it stays on the dictionary path.
            if spine.len() != lam_arity(&body) {
                return None;
            }
            Some((body, spine))
        }
        _ => None,
    }
}

/// Strip a **uniform-ABI argument coercion** (traits v1 **R2** devirt). The call
/// site of a not-yet-devirtualized trait-method use `Tag`s/`ToPtr`s a scalar/handle
/// argument into the method's abstract trait-parameter slot (the dictionary field's
/// uniform ABI). When the use is devirtualized against a concrete instance, the
/// *unwrapped* concrete body receives the raw argument, so this coercion is undone:
/// a `Coerce { Tag | ToPtr, inner }` peels to `inner`. Any other node is returned as
/// is (a concrete→concrete argument carries no coercion).
fn strip_uniform_coerce(arg: &Typed) -> &Typed {
    match &arg.node {
        Node::Coerce {
            kind: Coercion::Tag | Coercion::ToPtr,
            inner,
            ..
        } => inner,
        _ => arg,
    }
}

/// The trait a [`DictEvidence`] is for (traits v1 Sprint 3) — for naming the
/// embedded `super_<Trait>` field.
fn dict_evidence_trait(ev: &DictEvidence) -> Option<String> {
    match ev {
        DictEvidence::Instance { trait_name, .. } => Some(trait_name.clone()),
        DictEvidence::DictParam { constraint } => Some(constraint.trait_name.clone()),
    }
}

/// Look up the registered instance for `(trait, head)` (traits v1 Sprint 3) — the
/// source of a dictionary literal's method closures. Keyed by head constructor
/// ([`head_key`]); coherence (R3) guarantees at most one.
fn lookup_instance(trait_name: &str, head: &Type) -> Option<InstanceInfo> {
    let key = head_key(head);
    INSTANCES.with(|i| {
        i.borrow()
            .iter()
            .find(|inst| {
                inst.trait_name == trait_name && head_key(&inst.head).as_deref() == key.as_deref()
            })
            .cloned()
    })
}

/// The **record type** of a dictionary for `trait` (traits v1 Sprint 3) — the
/// declared method fields (named, in sorted order) plus an embedded `super_<S>`
/// field per superclass. Used to type a hidden dictionary *parameter* and a
/// threaded-parameter reference, where no instance literal is in hand. A field's
/// type is the trait's declared method signature; precise enough for record-slot
/// resolution (the IR keys field projection on the field *name*, not on a deep
/// structural type match), and the dictionary handle is one traced pointer cell
/// regardless (`trait-resolution.md` §7.5).
fn dict_record_type(trait_name: &str) -> Type {
    let info = TRAIT_DEFS.with(|t| t.borrow().get(trait_name).cloned());
    let mut fields: Vec<(String, Type)> = Vec::new();
    if let Some(info) = &info {
        for (m, sig) in &info.methods {
            fields.push((m.clone(), sig.clone()));
        }
        for sup in &info.supers {
            fields.push((
                super_field_name(&sup.trait_name),
                dict_record_type(&sup.trait_name),
            ));
        }
    }
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    Type::Record(fields)
}

/// Apply `f` to every immediate `Typed` child of `node`, returning the rebuilt
/// node (traits v1 Sprint 3 — the structural-recursion arm of [`dict_pass`], for
/// every node that is not itself a dictionary site). Mirrors the child set of
/// [`crate::unify::zonk`].
fn map_children(node: &Node, f: &dyn Fn(&Typed) -> Typed) -> Node {
    let b = |c: &Box<Typed>| Box::new(f(c));
    match node {
        // Leaves (handled by the caller's tagged-`Var` arm; a plain `Var` here).
        Node::Var(_)
        | Node::Int(_)
        | Node::Float(_)
        | Node::Bool(_)
        | Node::Unit
        | Node::Brk
        | Node::Str(_)
        | Node::Extern(..) => node.clone(),
        Node::Bin(op, a, c) => Node::Bin(*op, b(a), b(c)),
        Node::Cast(op, a) => Node::Cast(*op, b(a)),
        Node::Coerce {
            kind,
            slot,
            value,
            inner,
        } => Node::Coerce {
            kind: *kind,
            slot: slot.clone(),
            value: value.clone(),
            inner: b(inner),
        },
        Node::FloatMathUnary(op, a) => Node::FloatMathUnary(*op, b(a)),
        Node::FloatMathBinary(op, a, c) => Node::FloatMathBinary(*op, b(a), b(c)),
        Node::FloatMathTernary(op, a, c, d) => Node::FloatMathTernary(*op, b(a), b(c), b(d)),
        Node::MaskReduce(op, a) => Node::MaskReduce(*op, b(a)),
        Node::VectorSelect {
            mask,
            then_value,
            else_value,
        } => Node::VectorSelect {
            mask: b(mask),
            then_value: b(then_value),
            else_value: b(else_value),
        },
        Node::VectorLit { shape, elems } => Node::VectorLit {
            shape: *shape,
            elems: elems.iter().map(f).collect(),
        },
        Node::VectorSplat { shape, value } => Node::VectorSplat {
            shape: *shape,
            value: b(value),
        },
        Node::VectorLoad { shape, arr, idx } => Node::VectorLoad {
            shape: *shape,
            arr: b(arr),
            idx: b(idx),
        },
        Node::VectorStore {
            shape,
            arr,
            idx,
            value,
        } => Node::VectorStore {
            shape: *shape,
            arr: b(arr),
            idx: b(idx),
            value: b(value),
        },
        Node::VectorExtract { vector, lane } => Node::VectorExtract {
            vector: b(vector),
            lane: *lane,
        },
        Node::If(c, th, el) => Node::If(b(c), b(th), b(el)),
        Node::Loop {
            vars,
            cond,
            steps,
            result,
        } => Node::Loop {
            vars: vars
                .iter()
                .map(|(n, ty, layout, init)| (n.clone(), ty.clone(), *layout, f(init)))
                .collect(),
            cond: b(cond),
            steps: steps.iter().map(f).collect(),
            result: b(result),
        },
        Node::Lam {
            param,
            param_ty,
            body,
        } => Node::Lam {
            param: param.clone(),
            param_ty: param_ty.clone(),
            body: b(body),
        },
        Node::App { fun, arg } => Node::App {
            fun: b(fun),
            arg: b(arg),
        },
        Node::Let { name, bound, body } => Node::Let {
            name: name.clone(),
            bound: b(bound),
            body: b(body),
        },
        Node::Block { items, body } => Node::Block {
            items: items
                .iter()
                .map(|item| match item {
                    TypedBlockItem::Let { name, bound } => TypedBlockItem::Let {
                        name: name.clone(),
                        bound: f(bound),
                    },
                    TypedBlockItem::LetMut { name, bound } => TypedBlockItem::LetMut {
                        name: name.clone(),
                        bound: f(bound),
                    },
                    TypedBlockItem::LetTuple {
                        names,
                        bound,
                        fields_layout_known,
                    } => TypedBlockItem::LetTuple {
                        names: names.clone(),
                        bound: f(bound),
                        fields_layout_known: *fields_layout_known,
                    },
                })
                .collect(),
            body: b(body),
        },
        Node::LetMut { name, bound, body } => Node::LetMut {
            name: name.clone(),
            bound: b(bound),
            body: b(body),
        },
        Node::Assign { name, value } => Node::Assign {
            name: name.clone(),
            value: b(value),
        },
        Node::RefNew { value } => Node::RefNew { value: b(value) },
        Node::Deref { cell } => Node::Deref { cell: b(cell) },
        Node::RefAssign { target, value } => Node::RefAssign {
            target: b(target),
            value: b(value),
        },
        Node::Perform { label, arg } => Node::Perform {
            label: label.clone(),
            arg: b(arg),
        },
        Node::Handle { scrutinee, handler } => Node::Handle {
            scrutinee: b(scrutinee),
            handler: map_handler(handler, f),
        },
        Node::Quote(a) => Node::Quote(b(a)),
        Node::Splice(a) => Node::Splice(b(a)),
        Node::Genlet(a) => Node::Genlet(b(a)),
        Node::Letloc(a) => Node::Letloc(b(a)),
        Node::Peek(w, a) => Node::Peek(*w, b(a)),
        Node::Poke(w, a, c) => Node::Poke(*w, b(a), b(c)),
        Node::Fill(a, c, d) => Node::Fill(b(a), b(c), b(d)),
        Node::Copy(a, c, d) => Node::Copy(b(a), b(c), b(d)),
        Node::Index(w, a, c) => Node::Index(*w, b(a), b(c)),
        Node::IndexSet(w, a, c, d) => Node::IndexSet(*w, b(a), b(c), b(d)),
        Node::Tuple(es) => Node::Tuple(es.iter().map(f).collect()),
        Node::LetTuple(names, e, body) => Node::LetTuple(names.clone(), b(e), b(body)),
        Node::Record(fs) => Node::Record(fs.iter().map(|(n, v)| (n.clone(), f(v))).collect()),
        Node::Field(r, name) => Node::Field(b(r), name.clone()),
        Node::ArrayLit { elems, elem_layout } => Node::ArrayLit {
            elems: elems.iter().map(f).collect(),
            elem_layout: *elem_layout,
        },
        Node::Len(a) => Node::Len(b(a)),
        Node::ArrayGet {
            arr,
            idx,
            elem_layout,
        } => Node::ArrayGet {
            arr: b(arr),
            idx: b(idx),
            elem_layout: *elem_layout,
        },
        Node::ArraySet {
            arr,
            idx,
            val,
            elem_layout,
        } => Node::ArraySet {
            arr: b(arr),
            idx: b(idx),
            val: b(val),
            elem_layout: *elem_layout,
        },
        Node::Construct { tag, args } => Node::Construct {
            tag: *tag,
            args: args
                .iter()
                .map(|(a, layout, ty)| (f(a), *layout, ty.clone()))
                .collect(),
        },
        Node::Match { scrutinee, arms } => Node::Match {
            scrutinee: b(scrutinee),
            arms: arms
                .iter()
                .map(|arm| MatchArmT {
                    tag: arm.tag,
                    binds: arm.binds.clone(),
                    body: f(&arm.body),
                })
                .collect(),
        },
    }
}

/// Apply `f` to every `Typed` child of a handler (traits v1 Sprint 3) — the
/// op-clause and return bodies.
fn map_handler(h: &TypedHandler, f: &dyn Fn(&Typed) -> Typed) -> TypedHandler {
    TypedHandler {
        ops: h
            .ops
            .iter()
            .map(|c| TypedOpClause {
                body: Box::new(f(&c.body)),
                ..c.clone()
            })
            .collect(),
        ret: TypedReturn {
            body: Box::new(f(&h.ret.body)),
            ..h.ret.clone()
        },
    }
}

/// The **head-constructor key** of a type (`trait-resolution.md` §2.1) — the
/// outermost type constructor on which coherence (R3) and resolution (R1) are
/// keyed. `Int`/`Bool`/… are their own keys; `Named(n, _)` keys on `n` (so
/// `List[Int]` and `List[Bool]` share the head `List`); structural heads
/// (`Fun`/`Tuple`/`Record`/`Array`/`Code`/`Vector`/`Mask`) key on a shape tag. A
/// **bare type variable** has no head (`None`) — it never selects an instance
/// (R1 step 1 only). A `Named(n, [])` with a lowercase `n` is also a variable.
fn head_key(ty: &Type) -> Option<String> {
    match ty {
        Type::Var(_) => None,
        Type::Named(n, _) if n.chars().next().is_some_and(char::is_lowercase) => None,
        Type::Named(n, _) => Some(format!("N:{n}")),
        Type::Int => Some("Int".into()),
        Type::Float => Some("Float".into()),
        Type::Float32 => Some("Float32".into()),
        Type::Bool => Some("Bool".into()),
        Type::Unit => Some("Unit".into()),
        Type::Str => Some("Str".into()),
        Type::I32 => Some("I32".into()),
        Type::U32 => Some("U32".into()),
        Type::Ptr => Some("Ptr".into()),
        Type::Fun(..) => Some("->".into()),
        Type::Tuple(ts) => Some(format!("Tuple/{}", ts.len())),
        Type::Record(fs) => {
            let mut names: Vec<&str> = fs.iter().map(|(n, _)| n.as_str()).collect();
            names.sort_unstable();
            Some(format!("Record/{}", names.join(",")))
        }
        Type::Array(_) => Some("Array".into()),
        Type::Code(..) => Some("Code".into()),
        Type::Vector(s, _) => Some(format!("Vector/{s:?}")),
        Type::Mask(s) => Some(format!("Mask/{s:?}")),
    }
}

/// **One-way structural match** of an instance head `pat` against a fixed type
/// `ty` (`trait-resolution.md` §1.2, R1 step 2): a lowercase zero-arg
/// `Named(v, [])` in `pat` is an instance head **variable** that binds to the
/// corresponding `ty` subterm (consistently — a repeat must agree); everything
/// else must match structurally with `ty` fixed (`ty`'s own variables never bind).
/// Returns whether the match succeeded, accumulating bindings in `out`.
fn match_head(pat: &Type, ty: &Type, out: &mut HashMap<String, Type>) -> bool {
    match (pat, ty) {
        (Type::Named(v, pargs), _) if pargs.is_empty() && is_var_name(v) => match out.get(v) {
            Some(prev) => prev == ty,
            None => {
                out.insert(v.clone(), ty.clone());
                true
            }
        },
        (Type::Named(pn, pargs), Type::Named(tn, targs)) => {
            pn == tn
                && pargs.len() == targs.len()
                && pargs.iter().zip(targs).all(|(p, t)| match_head(p, t, out))
        }
        (Type::Fun(pa, pb, _), Type::Fun(ta, tb, _)) => {
            match_head(pa, ta, out) && match_head(pb, tb, out)
        }
        (Type::Tuple(ps), Type::Tuple(ts)) => {
            ps.len() == ts.len() && ps.iter().zip(ts).all(|(p, t)| match_head(p, t, out))
        }
        (Type::Record(pfs), Type::Record(tfs)) => {
            pfs.len() == tfs.len()
                && pfs
                    .iter()
                    .zip(tfs)
                    .all(|((pn, p), (tn, t))| pn == tn && match_head(p, t, out))
        }
        (Type::Array(pe), Type::Array(te)) => match_head(pe, te, out),
        (Type::Code(pe, _), Type::Code(te, _)) => match_head(pe, te, out),
        (Type::Vector(ps, pe), Type::Vector(ts, te)) => ps == ts && match_head(pe, te, out),
        // Base types / masks: equal iff identical.
        _ => pat == ty,
    }
}

/// A trait/instance **head-variable name** — a lowercase-leading identifier (the
/// annotation-variable convention used throughout sema for `Named(v, [])`).
fn is_var_name(n: &str) -> bool {
    n.chars().next().is_some_and(char::is_lowercase)
}

/// Does an **instance head** contain a type variable — is it *non-ground*? An
/// instance head variable is a lowercase-leading zero-arg `Named(v, [])` (the
/// [`match_head`] convention); a raw `Type::Var` is also non-ground (defensive).
/// `Show Int` / `Ord Int` are ground; `Show [a]` (`Array`/`List` of a head var),
/// `Show (Pair a b)`, `Eq (Wrap a)` are non-ground. Used by [`resolve_instance`]
/// to gate the traits-v1 lowering (`RN-E0246`): v1 lowers only ground-head
/// instances. The discriminator is the HEAD only — a ground head with a context
/// `requires` is still ground.
fn head_is_non_ground(ty: &Type) -> bool {
    match ty {
        Type::Var(_) => true,
        Type::Named(n, args) if args.is_empty() && is_var_name(n) => true,
        Type::Named(_, args) => args.iter().any(head_is_non_ground),
        Type::Fun(a, b, _) => head_is_non_ground(a) || head_is_non_ground(b),
        Type::Code(t, _) => head_is_non_ground(t),
        Type::Array(e) => head_is_non_ground(e),
        Type::Vector(_, e) => head_is_non_ground(e),
        Type::Tuple(ts) => ts.iter().any(head_is_non_ground),
        Type::Record(fs) => fs.iter().any(|(_, t)| head_is_non_ground(t)),
        _ => false,
    }
}

/// Substitute instance head-variable names (the [`match_head`] bindings) through a
/// `requires` context constraint's type, turning `Show a` into `Show Int` once the
/// head match has bound `a := Int`. A `Named(v, [])` whose `v` is a bound head
/// variable is replaced; everything else is copied structurally.
fn subst_named_vars(ty: &Type, bindings: &HashMap<String, Type>) -> Type {
    match ty {
        Type::Named(n, args) if args.is_empty() && bindings.contains_key(n) => bindings[n].clone(),
        Type::Named(n, args) => Type::Named(
            n.clone(),
            args.iter().map(|a| subst_named_vars(a, bindings)).collect(),
        ),
        Type::Fun(a, b, r) => Type::Fun(
            Box::new(subst_named_vars(a, bindings)),
            Box::new(subst_named_vars(b, bindings)),
            r.clone(),
        ),
        Type::Code(t, r) => Type::Code(Box::new(subst_named_vars(t, bindings)), r.clone()),
        Type::Array(e) => Type::Array(Box::new(subst_named_vars(e, bindings))),
        Type::Vector(s, e) => Type::Vector(*s, Box::new(subst_named_vars(e, bindings))),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| subst_named_vars(t, bindings)).collect()),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, t)| (n.clone(), subst_named_vars(t, bindings)))
                .collect(),
        ),
        _ => ty.clone(),
    }
}

/// **Coherence (R3) / overlap (R4) / duplicate (R7-degenerate) check** for a new
/// `instance Trait head`, against the instances already in scope
/// (`trait-resolution.md` §2–§3). Run at the declaration site, before the new
/// instance is registered:
///
/// - an **exact-head** prior instance for the same `(trait, head-key)` whose head
///   is structurally identical ⇒ `RN-E0237 trait.duplicate-instance`;
/// - an **overlapping** prior instance — same `(trait, head-key)` but the heads
///   merely *unify* (could match a common type) ⇒ `RN-E0231 trait.overlapping-instances`.
///
/// Single-parameter traits (D6): the key is one head-constructor, so two instances
/// conflict exactly when their head keys agree.
fn check_instance_coherence(trait_name: &str, head: &Type) -> Result<(), TypeErr> {
    let key = head_key(head);
    let prior = INSTANCES.with(|t| {
        t.borrow()
            .iter()
            .filter(|i| i.trait_name == trait_name)
            .map(|i| i.head.clone())
            .collect::<Vec<_>>()
    });
    for ph in prior {
        // Same trait. A bare-variable head (`instance Show a`) has key `None` and
        // overlaps *every* other head; otherwise overlap is key agreement.
        let overlaps = match (head_key(head), head_key(&ph)) {
            (None, _) | (_, None) => true,
            (a, b) => a == b,
        };
        if !overlaps {
            continue;
        }
        // A structurally identical head is a duplicate; otherwise the heads
        // unify-but-differ ⇒ overlap.
        if heads_identical(head, &ph) {
            return Err(TypeErr::TraitDuplicateInstance {
                trait_name: trait_name.to_string(),
                head: head.clone(),
            });
        }
        return Err(TypeErr::TraitOverlappingInstances {
            trait_name: trait_name.to_string(),
            head1: ph,
            head2: head.clone(),
        });
    }
    let _ = key;
    Ok(())
}

/// Two instance heads are **structurally identical up to head-variable renaming**
/// (so `instance Show List[a]` and `instance Show List[b]` are duplicates, not a
/// fresh instance). Compares structure, treating any two lowercase head variables
/// as equal in matching positions (a consistent bijection is not required — a v1
/// single-param head is shallow enough that positional lowercase-equality is
/// exact for the heads the grammar admits).
fn heads_identical(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::Named(an, aa), Type::Named(bn, ba)) => {
            let av = an.is_empty() || is_var_name(an);
            let bv = bn.is_empty() || is_var_name(bn);
            if aa.is_empty() && ba.is_empty() && (av || bv) {
                // Two head variables (or a var vs a var-like nullary) — identical.
                return av && bv;
            }
            an == bn
                && aa.len() == ba.len()
                && aa.iter().zip(ba).all(|(x, y)| heads_identical(x, y))
        }
        (Type::Fun(a1, a2, _), Type::Fun(b1, b2, _)) => {
            heads_identical(a1, b1) && heads_identical(a2, b2)
        }
        (Type::Tuple(at), Type::Tuple(bt)) => {
            at.len() == bt.len() && at.iter().zip(bt).all(|(x, y)| heads_identical(x, y))
        }
        (Type::Record(af), Type::Record(bf)) => {
            af.len() == bf.len()
                && af
                    .iter()
                    .zip(bf)
                    .all(|((an, x), (bn, y))| an == bn && heads_identical(x, y))
        }
        (Type::Array(x), Type::Array(y)) => heads_identical(x, y),
        (Type::Code(x, _), Type::Code(y, _)) => heads_identical(x, y),
        (Type::Vector(s1, x), Type::Vector(s2, y)) => s1 == s2 && heads_identical(x, y),
        _ => a == b,
    }
}

/// **Termination (R6 — Paterson conditions, `trait-resolution.md` §5).** At an
/// instance declaration, every `requires` context constraint must be structurally
/// **smaller** than the head: (1) no type variable occurs more times in the
/// context than in the head, and (2) the context has strictly fewer
/// constructors + variables (size) than the head. (Condition 3 — type-function
/// growth — is vacuous in v1; no associated types, D6.) A violation is
/// `RN-E0233 trait.resolution-diverges`, naming the offending context + why.
fn check_instance_termination(
    trait_name: &str,
    head: &Type,
    requires: &[crate::syntax::Constraint],
) -> Result<(), TypeErr> {
    let head_size = type_size(head);
    let head_occ = var_occurrences(head);
    for req in requires {
        // Condition 1 — no variable occurs more often in the context than head.
        let req_occ = var_occurrences(&req.ty);
        if let Some((v, n)) = req_occ.iter().find_map(|(v, &n)| {
            let h = head_occ.get(v).copied().unwrap_or(0);
            if n > h {
                Some((v.clone(), n))
            } else {
                None
            }
        }) {
            return Err(TypeErr::TraitResolutionDiverges {
                trait_name: trait_name.to_string(),
                head: head.clone(),
                context: req.clone(),
                why: format!(
                    "the variable `{v}` occurs {n} time(s), more than in the head (Paterson \
                     condition 1)"
                ),
            });
        }
        // Condition 2 — the context must be strictly smaller than the head.
        let req_size = type_size(&req.ty);
        if req_size >= head_size {
            return Err(TypeErr::TraitResolutionDiverges {
                trait_name: trait_name.to_string(),
                head: head.clone(),
                context: req.clone(),
                why: format!(
                    "its structural size ({req_size}) is not strictly smaller than the head's \
                     ({head_size}) (Paterson condition 2)"
                ),
            });
        }
    }
    Ok(())
}

/// The **structural size** of a type — its constructor + variable count (the
/// well-founded measure Paterson's condition 2 shrinks, `trait-resolution.md` §5).
fn type_size(ty: &Type) -> usize {
    match ty {
        Type::Named(_, args) => 1 + args.iter().map(type_size).sum::<usize>(),
        Type::Fun(a, b, _) => 1 + type_size(a) + type_size(b),
        Type::Code(t, _) => 1 + type_size(t),
        Type::Array(e) => 1 + type_size(e),
        Type::Vector(_, e) => 1 + type_size(e),
        Type::Tuple(ts) => 1 + ts.iter().map(type_size).sum::<usize>(),
        Type::Record(fs) => 1 + fs.iter().map(|(_, t)| type_size(t)).sum::<usize>(),
        _ => 1,
    }
}

/// Count occurrences of each **head variable** (a lowercase nullary `Named`) in a
/// type — Paterson's condition 1 measure (`trait-resolution.md` §5).
fn var_occurrences(ty: &Type) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    fn go(ty: &Type, out: &mut HashMap<String, usize>) {
        match ty {
            Type::Named(n, args) if args.is_empty() && is_var_name(n) => {
                *out.entry(n.clone()).or_insert(0) += 1;
            }
            Type::Named(_, args) => args.iter().for_each(|a| go(a, out)),
            Type::Fun(a, b, _) => {
                go(a, out);
                go(b, out);
            }
            Type::Code(t, _) => go(t, out),
            Type::Array(e) | Type::Vector(_, e) => go(e, out),
            Type::Tuple(ts) => ts.iter().for_each(|t| go(t, out)),
            Type::Record(fs) => fs.iter().for_each(|(_, t)| go(t, out)),
            _ => {}
        }
    }
    go(ty, &mut out);
    out
}

/// **Orphan check (R5, `trait-resolution.md` §4).** An `instance Trait head` is
/// lawful only in the module that defines the trait *or* the module that defines
/// the type head; any third module is an orphan (`RN-E0232`). Module identity
/// rides the graft stamp (see [`crate::stdlib::graft`]): a bare (module-less)
/// program leaves every stamp `None`, in which case there *is* no module
/// structure to violate, so the check is a no-op.
///
/// For a **structural head** (tuple/record/base type/arrow) there is no user
/// type module, so per R5 only the trait's module is lawful.
fn check_instance_orphan(
    trait_name: &str,
    head: &Type,
    inst_module: Option<&str>,
) -> Result<(), TypeErr> {
    // No module structure ⇒ nothing can be an orphan (bare program / test snippet).
    let Some(inst_module) = inst_module else {
        return Ok(());
    };
    let trait_module = TRAITENV.with(|t| {
        t.borrow()
            .traits
            .get(trait_name)
            .and_then(|i| i.module.clone())
    });
    // The type head's module — only a `Named` user head has one.
    let head_module = match head {
        Type::Named(n, _) if !is_var_name(n) => {
            TRAITENV.with(|t| t.borrow().type_modules.get(n).cloned())
        }
        _ => None,
    };
    let lawful =
        trait_module.as_deref() == Some(inst_module) || head_module.as_deref() == Some(inst_module);
    if lawful {
        Ok(())
    } else {
        Err(TypeErr::TraitOrphanInstance {
            trait_name: trait_name.to_string(),
            head: head.clone(),
            module: inst_module.to_string(),
        })
    }
}

/// Drain and check the pending [`SEAL_OBLIGATIONS`]. The recorded body types are
/// re-zonked against the now-final store (an obligation captured a type while the
/// store was still solving) before running [`no_escape`]. Returns the first
/// `RN-E0403 cap.seal-leak` found.
fn check_seal_obligations() -> Result<(), TypeErr> {
    let obligations = SEAL_OBLIGATIONS.with(|s| std::mem::take(&mut *s.borrow_mut()));
    for (label, ty) in obligations {
        let ty = unify::with_store(|s| unify::zonk_ty(s, &ty));
        if let Some(escaping) = seal_escape(&label, &ty) {
            return Err(TypeErr::SealLeak {
                label,
                ty: escaping,
            });
        }
    }
    Ok(())
}

/// The seal no-escape side condition ([`sealing-solution.md`] §5), the `runST`
/// deep escape check relabeled to `L`. Returns `Some(offending_type)` if `label`
/// escapes through `ty`, else `None`.
///
/// - **Any label** escapes if it appears in *any* effect row reachable from `ty`
///   (an arrow's latent row, a `Code` row), recursively through the structural
///   types. A returned closure that still performs `L` is the canonical leak.
/// - **`gc` additionally** escapes if `ty` is a **gc-managed datum**
///   (`Array`/`Named`/`Tuple`/`Record`): a live heap value carries gc-liability,
///   so it may not leave a `seal gc` / `nogc` region even when no row names `gc`.
///   (A bare returned closure is exempt — its gc is its latent row, covered
///   above, and a capture-free closure may not allocate.)
///
/// Shared by the region seal (`seal L { e }`, checked post-zonk here) and the
/// module `seals (…)` clause ([`crate::capability::check_module_seals`], S4).
pub fn seal_escape(label: &Label, ty: &Type) -> Option<Type> {
    if *label == Label::Gc && is_gc_datum(ty) {
        return Some(ty.clone());
    }
    if type_mentions_label(ty, label) {
        return Some(ty.clone());
    }
    None
}

/// Elaborate a heap ref-assign `target := value` where `target : Ref[T]`
/// (`docs/mutability.md` §1): `(:=) : Ref[T] -> T -> Unit ! {st}`. Demand
/// `target : Ref[α]`, `value : α`, reject a pointer-typed cell (`RN-E0247`), and
/// build a `Node::RefAssign` yielding `Unit` with `{st}` joined into the row
/// (target's effects ∪ value's effects ∪ `{st}`). Shared by the bare-name `x := e`
/// dispatch (a `Ref`-typed name) — the one place the heap-write rule lives.
fn elaborate_ref_assign(
    sig: &Sig,
    ctx: &Ctx,
    stage: Stage,
    target: Typed,
    value: &Term,
) -> Result<Typed, TypeErr> {
    let resolved = ref_content_or_pin(&target.ty)?;
    if ref_content_is_pointer_typed(&resolved) {
        return Err(TypeErr::RefPointerContent { ty: resolved });
    }
    let tv = elaborate_inner(sig, ctx, stage, value)?;
    // The written value must have the cell's content type — a unification demand.
    demand_eq(&resolved, &tv.ty)?;
    let row = target.row.union(&tv.row).union(&Row::single(Label::St));
    Ok(Typed::at(
        Type::Unit,
        row,
        stage,
        Node::RefAssign {
            target: Box::new(target),
            value: Box::new(tv),
        },
    ))
}

/// Is `ty` a **scalar** a `let mut` cell may hold (mutability v1, scalar-only —
/// `docs/mutability.md` §1.1)? Exactly `Int`/`Float`/`Bool`. Everything else
/// (`Unit`, `Str`, `Ptr`, `Array`, `Named`, `Tuple`, `Record`, `Fun`, `Code`,
/// `Vector`/`Mask`, `Float32`/`I32`/`U32`) is rejected with `RN-E0244`: a mutable
/// gc-managed or wide cell needs the heap-`Ref[T]` / `st[T]` path, a later effort.
/// The bound type is resolved before this is called, so a `Var` never reaches it.
fn is_mut_scalar(ty: &Type) -> bool {
    matches!(ty, Type::Int | Type::Float | Type::Bool)
}

/// The nominal name of the `Ref[T]` heap-cell type. A `Ref` reuses the existing
/// `Type::Named` machinery (layout = one traced pointer cell — a handle — exactly
/// like every other heap value), so it needs no new `Type` variant; this constant
/// is the one place the name is written. (`docs/mutability.md` §1.1.)
const REF_TYPE: &str = "Ref";

/// Build the `Ref[T]` type for a content type `T`.
fn ref_type(content: Type) -> Type {
    Type::Named(REF_TYPE.to_string(), vec![content])
}

/// If `ty` is `Ref[T]` (resolved), return its content type `T`. Used to type `!r`
/// and `r := v` and to dispatch the `x := e` surface (a `Ref`-typed name → the
/// heap-cell write, vs a `let mut` cell → the slot store).
fn as_ref_content(ty: &Type) -> Option<Type> {
    match resolved_type(ty) {
        Type::Named(n, args) if n == REF_TYPE && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

/// The content type `T` of an operand the `Ref` operators expect to be `Ref[T]`.
/// If `ty` resolves to a concrete `Ref[T]`, return `T` **directly** (no
/// unification — crucial so a concrete `Ref[Float]` does not trip the conservative
/// `Var ~ Wide` kind guard by binding a fresh content var to `Float`). If `ty` is
/// still an unsolved `Var` (an unannotated parameter — `fn r => !r`), **pin** it to
/// `Ref[α]` for a fresh `α` and return `α`, to be solved by the surrounding uses.
/// Any other type is a `Ref` mismatch — surfaced through unification (so the error
/// reads as "expected `Ref[…]`, found …").
fn ref_content_or_pin(ty: &Type) -> Result<Type, TypeErr> {
    if let Some(content) = as_ref_content(ty) {
        return Ok(content);
    }
    let content = unify::with_store(|s| s.fresh_ty());
    demand_eq(&ref_type(content.clone()), ty)?;
    Ok(resolved_type(&content))
}

/// Is `ty` a **pointer-typed** `Ref` content cell — the clean deferred error
/// `RN-E0247 ref.pointer-content` (the GC write barrier is Sprint 3,
/// `docs/mutability.md` §6.1)? A *scalar* `Ref` (`Int`/`Float`/`Bool`/`Unit`) holds
/// no pointer in its content cell — so a write can never create an old→young
/// pointer and needs no barrier, unconditionally sound under the moving/
/// generational collector — and is **allowed** (returns `false`). Everything with a
/// traced/handle or wide-but-not-`Float` content cell (`Str`, `Array`, `Named`,
/// `Tuple`, `Record`, `Fun`, `Code`, `Float32`, `Vector`/`Mask`, `I32`/`U32`/`Ptr`,
/// even a nested `Ref`) is a pointer-typed cell → rejected.
///
/// A still-unsolved `Var` is **not** rejected here: it is deferred (D6 defaults a
/// residual content var to `Int`, a scalar — `!r` on an unannotated `fn r => …`
/// where the body pins `r`'s content later). The bare-name `r := v` always has a
/// resolved `Ref[T]`, so the concrete check fires there.
fn ref_content_is_pointer_typed(ty: &Type) -> bool {
    match resolved_type(ty) {
        Type::Int | Type::Float | Type::Bool | Type::Unit => false,
        Type::Var(_) => false,
        _ => true,
    }
}

/// The `let mut` **no-escape** side condition (`RN-E0241 mut.escapes`), the
/// `runST`/`st` deep escape check (`docs/mutability.md` §2–§3) applied to a single
/// local cell named `name`. A `let mut` cell may not be reachable from the body's
/// result type `body_ty` — it is a *sealed*, non-escaping `Ref`.
///
/// Returns `Some(offending_type)` if `name`'s cell escapes, else `None`. For a
/// **scalar** v1 cell (`Int`/`Float`/`Bool`) a returned *value* is a copy of the
/// scalar, not the cell, so no scalar result can carry the cell and this is `None`
/// by construction — there is no `RN-E0241`-violating program expressible in v1.
/// The check is wired anyway so the diagnostic and the escape predicate exist for
/// the Sprint-3 `Ref` reuse, where a returned `Ref`/closure *can* carry a cell.
fn mut_escape(name: &str, body_ty: &Type) -> Option<Type> {
    // v1: the cell has no first-class `Ref` type, so a result type cannot name it
    // — the cell can only escape once it is a value, which it is not until Sprint
    // 3. We reuse the `seal`/`runST` reachability shape: a *cell label* would be
    // the region marker, and escape = the result type mentions it. Until `Ref`
    // exists, no `body_ty` can mention the cell, so this is always `None`.
    //
    // Wired (not skipped) so the predicate/diagnostic are in place for Sprint 3:
    // there `type_mentions_label(body_ty, &cell_label(name))` (or the value-escape
    // analysis on a returned `Ref`/closure) replaces the body below.
    if cell_escapes(name, body_ty) {
        Some(body_ty.clone())
    } else {
        None
    }
}

/// Does the `let mut` cell `name` escape through `body_ty`? The `runST`/`st` deep
/// escape reachability check (`docs/mutability.md` §2.2), shaped like
/// [`type_mentions_label`]: in scalar v1 the cell has no type-level footprint
/// (no `Ref` type, no effect label), so nothing in `body_ty` can name it and this
/// is always `false`. Kept as a named predicate so Sprint 3 swaps in the real
/// reachability test (a returned `Ref[τ]` / a closure capturing the cell) here.
fn cell_escapes(_name: &str, _body_ty: &Type) -> bool {
    false
}

/// The **structural** `let mut` non-escape gate (`RN-E0241 mut.escapes`), the
/// closure-capture route the *type-based* [`mut_escape`] cannot see.
///
/// v1 supports only **non-escaping** scalar mutable locals: Sprint 3 lowers a
/// `let mut` cell to a **stack slot**, so a closure that captures the mutable
/// local and then leaves the `let mut` scope would read a **dangling** slot. A
/// type-based check can't catch this — the escaping closure's type (`Int -> Int`)
/// names no cell — so we need this structural test on the elaborated body. v1 has
/// no `st[T]` effect / first-class `Ref[T]` to *track* the cell's lifetime (that
/// is the future feature, `docs/mutability.md` §8), so v1 **conservatively
/// rejects any closure capture** of a mutable local. This is not crippling:
/// straight-line imperative use (`let mut x = 1 in let _ = x := x + 1 in x`) and
/// direct mutation are unaffected, and the callback-accumulator pattern is served
/// by the native `loop … do … return` accumulator and `list_fold`.
///
/// Returns `true` iff `name` occurs as a free [`Node::Var`] inside **any**
/// [`Node::Lam`] body anywhere in `node`'s tree — i.e. some lambda captures the
/// cell. Shadowing is respected: descent stops at the shadowed region of any
/// inner binder that rebinds `name` (a `Lam` parameter, a `Let`/`LetMut` name, a
/// `LetTuple`/match-arm/handler-clause binder), so a lambda whose *own* parameter
/// is `name` (`fn x: Int => x`) does **not** count as capturing the outer cell.
fn captured_in_closure(name: &str, node: &Node) -> bool {
    match node {
        // A lambda: if `name` is free in its body, the cell is captured. (Unless
        // the lambda's parameter shadows `name`, in which case the body's `x` is
        // the parameter, not the cell.) Either way we also keep descending so a
        // *nested* lambda inside the body is still examined by `occurs_free`.
        Node::Lam { param, body, .. } => {
            if param != name && occurs_free(name, &body.node) {
                return true;
            }
            // Parameter shadows `name`: nothing below can reference the cell.
            param != name && captured_in_closure(name, &body.node)
        }

        // Binders that shadow `name`: once rebound, the cell is no longer in scope
        // for the shadowed sub-tree, so we only descend the still-in-scope parts.
        Node::Let {
            name: b,
            bound,
            body,
        } => {
            captured_in_closure(name, &bound.node)
                || (b != name && captured_in_closure(name, &body.node))
        }
        Node::LetMut {
            name: b,
            bound,
            body,
        } => {
            captured_in_closure(name, &bound.node)
                || (b != name && captured_in_closure(name, &body.node))
        }
        Node::Block { items, body } => {
            for item in items {
                if captured_in_closure(name, &item.bound().node) {
                    return true;
                }
                if item.binds_name(name) {
                    return false;
                }
            }
            captured_in_closure(name, &body.node)
        }
        Node::LetTuple(binders, bound, body) => {
            captured_in_closure(name, &bound.node)
                || (!binders.iter().any(|n| n == name) && captured_in_closure(name, &body.node))
        }
        Node::Match { scrutinee, arms } => {
            captured_in_closure(name, &scrutinee.node)
                || arms.iter().any(|arm| {
                    !arm.binds.iter().any(|(n, ..)| n == name)
                        && captured_in_closure(name, &arm.body.node)
                })
        }
        Node::Handle { scrutinee, handler } => {
            captured_in_closure(name, &scrutinee.node)
                || handler.ops.iter().any(|op| {
                    // an op clause binds its argument and its `resume` continuation.
                    op.arg != name && op.resume != name && captured_in_closure(name, &op.body.node)
                })
                || (handler.ret.var != name && captured_in_closure(name, &handler.ret.body.node))
        }
        Node::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            // loop vars are binders; their initialisers are in the outer scope.
            if vars
                .iter()
                .any(|(_, _, _, init)| captured_in_closure(name, &init.node))
            {
                return true;
            }
            if vars.iter().any(|(n, ..)| n == name) {
                return false; // the loop rebinds `name`: body/cond/result are shadowed.
            }
            captured_in_closure(name, &cond.node)
                || steps.iter().any(|s| captured_in_closure(name, &s.node))
                || captured_in_closure(name, &result.node)
        }

        // Leaves — no sub-terms, no lambda.
        Node::Var(_)
        | Node::Int(_)
        | Node::Float(_)
        | Node::Bool(_)
        | Node::Unit
        | Node::Brk
        | Node::Str(_)
        | Node::Extern(..) => false,

        // Everything else is a binder-free interior node: recurse into all children.
        Node::Bin(_, a, b)
        | Node::FloatMathBinary(_, a, b)
        | Node::Index(_, a, b)
        | Node::Poke(_, a, b) => {
            captured_in_closure(name, &a.node) || captured_in_closure(name, &b.node)
        }
        Node::If(a, b, c)
        | Node::Fill(a, b, c)
        | Node::Copy(a, b, c)
        | Node::FloatMathTernary(_, a, b, c)
        | Node::IndexSet(_, a, b, c) => {
            captured_in_closure(name, &a.node)
                || captured_in_closure(name, &b.node)
                || captured_in_closure(name, &c.node)
        }
        Node::VectorSelect {
            mask,
            then_value,
            else_value,
        } => {
            captured_in_closure(name, &mask.node)
                || captured_in_closure(name, &then_value.node)
                || captured_in_closure(name, &else_value.node)
        }
        Node::App { fun, arg } => {
            captured_in_closure(name, &fun.node) || captured_in_closure(name, &arg.node)
        }
        Node::Assign { value, .. } => captured_in_closure(name, &value.node),
        // `Ref` operators — recurse into the sub-expressions (the cell handle and
        // the written/read value); none is itself a binder.
        Node::RefNew { value, .. } => captured_in_closure(name, &value.node),
        Node::Deref { cell, .. } => captured_in_closure(name, &cell.node),
        Node::RefAssign { target, value, .. } => {
            captured_in_closure(name, &target.node) || captured_in_closure(name, &value.node)
        }
        Node::Perform { arg: body, .. }
        | Node::Quote(body)
        | Node::Splice(body)
        | Node::Genlet(body)
        | Node::Letloc(body)
        | Node::Cast(_, body)
        | Node::Coerce { inner: body, .. }
        | Node::FloatMathUnary(_, body)
        | Node::MaskReduce(_, body)
        | Node::Peek(_, body)
        | Node::Len(body)
        | Node::Field(body, _)
        | Node::VectorSplat { value: body, .. }
        | Node::VectorExtract { vector: body, .. } => captured_in_closure(name, &body.node),
        Node::Tuple(es) | Node::ArrayLit { elems: es, .. } | Node::VectorLit { elems: es, .. } => {
            es.iter().any(|t| captured_in_closure(name, &t.node))
        }
        Node::Record(fs) => fs.iter().any(|(_, t)| captured_in_closure(name, &t.node)),
        Node::ArrayGet { arr, idx, .. } => {
            captured_in_closure(name, &arr.node) || captured_in_closure(name, &idx.node)
        }
        Node::ArraySet { arr, idx, val, .. } => {
            captured_in_closure(name, &arr.node)
                || captured_in_closure(name, &idx.node)
                || captured_in_closure(name, &val.node)
        }
        Node::VectorLoad { arr, idx, .. } => {
            captured_in_closure(name, &arr.node) || captured_in_closure(name, &idx.node)
        }
        Node::VectorStore {
            arr, idx, value, ..
        } => {
            captured_in_closure(name, &arr.node)
                || captured_in_closure(name, &idx.node)
                || captured_in_closure(name, &value.node)
        }
        Node::Construct { args, .. } => args
            .iter()
            .any(|(t, ..)| captured_in_closure(name, &t.node)),
    }
}

/// Standard free-variable test: does `name` occur as a free [`Node::Var`] in
/// `node`, stopping at any binder that shadows `name`? Used by
/// [`captured_in_closure`] to decide whether a lambda body references the cell.
fn occurs_free(name: &str, node: &Node) -> bool {
    match node {
        Node::Var(v) => v == name,

        // Binders: descend the binder's bound term in the outer scope, and the
        // body only if it does not rebind `name`.
        Node::Lam { param, body, .. } => param != name && occurs_free(name, &body.node),
        Node::Let {
            name: b,
            bound,
            body,
        } => occurs_free(name, &bound.node) || (b != name && occurs_free(name, &body.node)),
        Node::LetMut {
            name: b,
            bound,
            body,
        } => occurs_free(name, &bound.node) || (b != name && occurs_free(name, &body.node)),
        Node::Block { items, body } => {
            for item in items {
                if occurs_free(name, &item.bound().node) {
                    return true;
                }
                if item.binds_name(name) {
                    return false;
                }
            }
            occurs_free(name, &body.node)
        }
        Node::LetTuple(binders, bound, body) => {
            occurs_free(name, &bound.node)
                || (!binders.iter().any(|n| n == name) && occurs_free(name, &body.node))
        }
        Node::Match { scrutinee, arms } => {
            occurs_free(name, &scrutinee.node)
                || arms.iter().any(|arm| {
                    !arm.binds.iter().any(|(n, ..)| n == name) && occurs_free(name, &arm.body.node)
                })
        }
        Node::Handle { scrutinee, handler } => {
            occurs_free(name, &scrutinee.node)
                || handler.ops.iter().any(|op| {
                    op.arg != name && op.resume != name && occurs_free(name, &op.body.node)
                })
                || (handler.ret.var != name && occurs_free(name, &handler.ret.body.node))
        }
        Node::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            if vars
                .iter()
                .any(|(_, _, _, init)| occurs_free(name, &init.node))
            {
                return true;
            }
            if vars.iter().any(|(n, ..)| n == name) {
                return false;
            }
            occurs_free(name, &cond.node)
                || steps.iter().any(|s| occurs_free(name, &s.node))
                || occurs_free(name, &result.node)
        }

        // Leaves.
        Node::Int(_)
        | Node::Float(_)
        | Node::Bool(_)
        | Node::Unit
        | Node::Brk
        | Node::Str(_)
        | Node::Extern(..) => false,

        // Binder-free interior nodes. `Assign`'s `name` is a *use* of the cell, not
        // a binder, so an assignment to `name` counts as a free occurrence too.
        Node::Assign { name: n, value } => n == name || occurs_free(name, &value.node),
        Node::RefAssign { target, value, .. } => {
            occurs_free(name, &target.node) || occurs_free(name, &value.node)
        }
        Node::RefNew { value: body, .. } | Node::Deref { cell: body, .. } => {
            occurs_free(name, &body.node)
        }
        Node::Bin(_, a, b)
        | Node::FloatMathBinary(_, a, b)
        | Node::Index(_, a, b)
        | Node::Poke(_, a, b) => occurs_free(name, &a.node) || occurs_free(name, &b.node),
        Node::If(a, b, c)
        | Node::Fill(a, b, c)
        | Node::Copy(a, b, c)
        | Node::FloatMathTernary(_, a, b, c)
        | Node::IndexSet(_, a, b, c) => {
            occurs_free(name, &a.node) || occurs_free(name, &b.node) || occurs_free(name, &c.node)
        }
        Node::VectorSelect {
            mask,
            then_value,
            else_value,
        } => {
            occurs_free(name, &mask.node)
                || occurs_free(name, &then_value.node)
                || occurs_free(name, &else_value.node)
        }
        Node::App { fun, arg } => occurs_free(name, &fun.node) || occurs_free(name, &arg.node),
        Node::Perform { arg: body, .. }
        | Node::Quote(body)
        | Node::Splice(body)
        | Node::Genlet(body)
        | Node::Letloc(body)
        | Node::Cast(_, body)
        | Node::Coerce { inner: body, .. }
        | Node::FloatMathUnary(_, body)
        | Node::MaskReduce(_, body)
        | Node::Peek(_, body)
        | Node::Len(body)
        | Node::Field(body, _)
        | Node::VectorSplat { value: body, .. }
        | Node::VectorExtract { vector: body, .. } => occurs_free(name, &body.node),
        Node::Tuple(es) | Node::ArrayLit { elems: es, .. } | Node::VectorLit { elems: es, .. } => {
            es.iter().any(|t| occurs_free(name, &t.node))
        }
        Node::Record(fs) => fs.iter().any(|(_, t)| occurs_free(name, &t.node)),
        Node::ArrayGet { arr, idx, .. } => {
            occurs_free(name, &arr.node) || occurs_free(name, &idx.node)
        }
        Node::ArraySet { arr, idx, val, .. } => {
            occurs_free(name, &arr.node)
                || occurs_free(name, &idx.node)
                || occurs_free(name, &val.node)
        }
        Node::VectorLoad { arr, idx, .. } => {
            occurs_free(name, &arr.node) || occurs_free(name, &idx.node)
        }
        Node::VectorStore {
            arr, idx, value, ..
        } => {
            occurs_free(name, &arr.node)
                || occurs_free(name, &idx.node)
                || occurs_free(name, &value.node)
        }
        Node::Construct { args, .. } => args.iter().any(|(t, ..)| occurs_free(name, &t.node)),
    }
}

/// Is `ty` a directly **gc-managed heap datum** — a value whose existence implies
/// an allocation the collector owns? `Array`/`Named`/`Tuple`/`Record`/`Str`.
/// Closures (`Fun`), `Code`, and scalars are not.
fn is_gc_datum(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Array(_) | Type::Named(..) | Type::Tuple(_) | Type::Record(_) | Type::Str
    )
}

/// The first **gc-managed (movable) datum** named anywhere in an `extern asm`
/// signature — walking arrow domains and results — or `None` if the whole ABI is
/// GC-blind. The A6 gate (jasm-boundary-layer §A6): Layer-0 asm runs with no
/// safepoints, read barriers, or handle indirection, so a moving collector could
/// relocate such a value underneath it. Scalars, raw `Ptr`, `Unit`, `Vector`
/// (unboxed), `Code`, and closures are all GC-blind here.
fn arrow_mentions_gc_datum(ty: &Type) -> Option<Type> {
    match ty {
        Type::Fun(a, b, _) => arrow_mentions_gc_datum(a).or_else(|| arrow_mentions_gc_datum(b)),
        t if is_gc_datum(t) => Some(t.clone()),
        _ => None,
    }
}

/// Does `label` appear in any effect row reachable from `ty`? Deep over arrows'
/// latent rows, `Code` rows, and recursively through the structural types.
fn type_mentions_label(ty: &Type, label: &Label) -> bool {
    let row_has = |r: &Row| r.labels().any(|l| l == label);
    match ty {
        Type::Fun(a, b, row) => {
            row_has(row) || type_mentions_label(a, label) || type_mentions_label(b, label)
        }
        Type::Code(a, row) => row_has(row) || type_mentions_label(a, label),
        Type::Array(a) | Type::Vector(_, a) => type_mentions_label(a, label),
        Type::Tuple(fields) => fields.iter().any(|t| type_mentions_label(t, label)),
        Type::Record(fields) => fields.iter().any(|(_, t)| type_mentions_label(t, label)),
        Type::Named(_, args) => args.iter().any(|t| type_mentions_label(t, label)),
        _ => false,
    }
}

/// The **value restriction** predicate (D4, the soundness lynchpin). A `let`
/// generalizes its bound expression's type **only when this returns `true`** —
/// i.e. only for a *syntactic value*: a literal, a variable, a lambda, an
/// `extern`, or an aggregate (`Construct`/`Tuple`/`Record`) all of whose
/// components are themselves values. Everything else — an application, a
/// `perform`, a `handle`, an `if`, a `Bin`, an index/field/splice/match, … — is
/// a **non-value** and stays monomorphic. Generalizing a non-value is the
/// classic unsoundness (the polymorphic-`ref` family), so this is deliberately
/// the narrow V1 predicate, no more.
///
/// **D4 carve-out.** A tuple / record / constructor of values **is** a value and
/// **does** generalize, *even though* elaborating it performs the `gc` effect
/// (allocation). `gc` is **not** a generalization barrier: an allocation cannot
/// be unsoundly *varied* at different instantiations the way a mutable `ref`
/// cell can — the cell is what makes the `ref` example unsound, and there is no
/// cell here. The `gc` label still unions into the binding's row unconditionally
/// (the manifest stays honest about the heap touch); it just does not stop the
/// *type* from generalizing.
fn is_value(e: &Term) -> bool {
    match e {
        // Literals, variables, lambdas, externs — manifestly values.
        Term::Int(_)
        | Term::Float(_)
        | Term::Bool(_)
        | Term::Unit
        | Term::Str(_)
        | Term::Var(_)
        | Term::Lam(..)
        | Term::Extern(..)
        | Term::ExternAsm(..) => true,
        Term::VectorLit(_, args) => args.iter().all(is_value),
        Term::VectorSplat(_, value) => is_value(value),
        // Aggregates are values iff every component is — the recursive case
        // (and the D4 carve-out: `gc` despite this does not bar generalization).
        Term::Construct(_, args) => args.iter().all(is_value),
        Term::Tuple(es) => es.iter().all(is_value),
        Term::Record(fields) => fields.iter().all(|(_, e)| is_value(e)),
        // Everything else is a non-value → never generalized. Listed by class
        // for clarity (App, Perform, Handle, If, Bin, Index/IndexSet, Field,
        // Splice/Quote/Genlet/Letloc, Match, the mem primitives, Let/LetRec,
        // ArrayLit, Len, the type/effect decl forms).
        _ => false,
    }
}

fn wrap_block_suffix(item: &TypedBlockItem, suffix: Typed, stage: Stage) -> Typed {
    match item {
        TypedBlockItem::Let { name, bound } => {
            let row = bound.row.union(&suffix.row);
            let ty = suffix.ty.clone();
            Typed::at(
                ty,
                row,
                stage,
                Node::Let {
                    name: name.clone(),
                    bound: Box::new(bound.clone()),
                    body: Box::new(suffix),
                },
            )
        }
        TypedBlockItem::LetMut { name, bound } => {
            let row = bound.row.union(&suffix.row);
            let ty = suffix.ty.clone();
            Typed::at(
                ty,
                row,
                stage,
                Node::LetMut {
                    name: name.clone(),
                    bound: Box::new(bound.clone()),
                    body: Box::new(suffix),
                },
            )
        }
        TypedBlockItem::LetTuple {
            names,
            bound,
            fields_layout_known,
        } => {
            let row = bound.row.union(&suffix.row);
            let ty = suffix.ty.clone();
            Typed::at_layout(
                ty,
                row,
                stage,
                *fields_layout_known,
                Node::LetTuple(names.clone(), Box::new(bound.clone()), Box::new(suffix)),
            )
        }
    }
}

fn elaborate_block(
    sig: &Sig,
    ctx: &Ctx,
    stage: Stage,
    items: &[BlockItem],
    body: &Term,
) -> Result<Typed, TypeErr> {
    let mut saved_tyenvs = Vec::new();
    let mut saved_traitenvs = Vec::new();

    let result = (|| -> Result<Typed, TypeErr> {
        let mut sig2 = sig.clone();
        let mut ctx2 = ctx.clone();
        let mut pending = Vec::with_capacity(items.len());

        for item in items {
            match item {
                BlockItem::Let(x, e1) => {
                    unify::with_store(|s| s.enter_level());
                    let t1 = elaborate_inner(&sig2, &ctx2, stage, e1);
                    unify::with_store(|s| s.leave_level());
                    let t1 = t1?;

                    let binding = if is_value(e1) {
                        Binding::Poly(generalize_resolved(&t1.ty))
                    } else {
                        Binding::Mono(t1.ty.clone())
                    };
                    let bind_name = match &binding {
                        Binding::Poly(scheme) if !scheme.constraints.is_empty() => {
                            let traits: Vec<String> = scheme
                                .constraints
                                .iter()
                                .map(|c| c.trait_name.clone())
                                .collect();
                            record_constrained_def(x, &traits)
                        }
                        _ => x.clone(),
                    };
                    ctx2.insert(x.clone(), (binding, stage));
                    pending.push(TypedBlockItem::Let {
                        name: bind_name,
                        bound: t1,
                    });
                }
                BlockItem::LetRec(f, ty, e1) => {
                    unify::with_store(|s| s.enter_level());
                    let ty = unify::with_store(|s| instantiate_annotation(s, ty));
                    let valid = validate_named_arity(&ty);
                    let mut ctxr = ctx2.clone();
                    ctxr.insert(f.clone(), (Binding::Mono(ty.clone()), stage));
                    let t1 = valid.and_then(|()| elaborate_inner(&sig2, &ctxr, stage, e1));
                    unify::with_store(|s| s.leave_level());
                    let t1 = t1?;
                    demand_eq(&ty, &t1.ty)?;
                    let scheme = generalize_resolved(&ty);
                    if !scheme.constraints.is_empty() {
                        let constraints: Vec<String> =
                            scheme.constraints.iter().map(|c| c.to_string()).collect();
                        return Err(TypeErr::TraitV1Unsupported {
                            what: format!(
                                "a recursive (`let rec`) function `{f}` with a trait constraint \
                                 ({}); make it non-recursive (a plain `let`), or monomorphize it \
                                 (annotate `{f}` at a concrete type so no constraint is generalized)",
                                constraints.join(", ")
                            ),
                        });
                    }
                    ctx2.insert(f.clone(), (Binding::Poly(scheme), stage));
                    pending.push(TypedBlockItem::Let {
                        name: f.clone(),
                        bound: t1,
                    });
                }
                BlockItem::LetMut(x, e1) => {
                    let t1 = elaborate_inner(&sig2, &ctx2, stage, e1)?;
                    let scalar_ty = resolved_type(&t1.ty);
                    if !is_mut_scalar(&scalar_ty) {
                        return Err(TypeErr::MutNonScalar { ty: scalar_ty });
                    }
                    ctx2.insert(x.clone(), (Binding::Mut(t1.ty.clone()), stage));
                    pending.push(TypedBlockItem::LetMut {
                        name: x.clone(),
                        bound: t1,
                    });
                }
                BlockItem::LetTuple(names, e) => {
                    let te = elaborate_inner(&sig2, &ctx2, stage, e)?;
                    let te_ty = resolved_type(&te.ty);
                    let Type::Tuple(elem_tys) = &te_ty else {
                        return Err(TypeErr::Mismatch {
                            expected: Type::Tuple(vec![Type::Unit; names.len()]),
                            found: te.ty.clone(),
                        });
                    };
                    if elem_tys.len() != names.len() {
                        return Err(TypeErr::Mismatch {
                            expected: Type::Tuple(vec![Type::Unit; names.len()]),
                            found: te.ty.clone(),
                        });
                    }
                    for (name, ty) in names.iter().zip(elem_tys) {
                        ctx2.insert(name.clone(), (Binding::Mono(ty.clone()), stage));
                    }
                    pending.push(TypedBlockItem::LetTuple {
                        names: names.clone(),
                        bound: te,
                        fields_layout_known: layout_known(elem_tys.iter()),
                    });
                }
                BlockItem::Effect { ops, .. } => {
                    for op in ops {
                        sig2.insert(
                            Label::User(op.op.clone()),
                            OpSig {
                                param: op.param.clone(),
                                result: op.result.clone(),
                            },
                        );
                    }
                }
                BlockItem::TypeDef {
                    name,
                    params,
                    variants,
                    module,
                } => {
                    check_typedef_decl(name, params, variants)?;
                    if let Some(m) = module {
                        TRAITENV.with(|t| {
                            t.borrow_mut().type_modules.insert(name.clone(), m.clone());
                        });
                    }
                    let (saved, subst) = TYENV.with(|t| {
                        let mut env = t.borrow_mut();
                        let saved = env.clone();
                        env.sums
                            .insert(name.clone(), (params.clone(), variants.clone()));
                        let param_vars: Vec<TyVarId> = params
                            .iter()
                            .map(|_| match unify::with_store(|s| s.fresh_ty()) {
                                Type::Var(id) => id,
                                _ => unreachable!("fresh_ty yields a Var"),
                            })
                            .collect();
                        let subst: HashMap<String, Type> = params
                            .iter()
                            .zip(&param_vars)
                            .map(|(p, &id)| (p.clone(), Type::Var(id)))
                            .collect();
                        let result = Type::Named(
                            name.clone(),
                            param_vars.iter().map(|&id| Type::Var(id)).collect(),
                        );
                        for (tag, (ctor, fields)) in variants.iter().enumerate() {
                            let fields = fields.iter().map(|f| rewrite_params(f, &subst)).collect();
                            env.ctors.insert(
                                ctor.clone(),
                                CtorInfo {
                                    type_name: name.clone(),
                                    tag: tag as i64,
                                    ty_params: param_vars.clone(),
                                    fields,
                                    result: result.clone(),
                                },
                            );
                        }
                        (saved, subst)
                    });
                    saved_tyenvs.push(saved);
                    for (_, fields) in variants {
                        for f in fields {
                            validate_named_arity(&rewrite_params(f, &subst))?;
                        }
                    }
                }
                BlockItem::Trait {
                    name,
                    param,
                    supers,
                    methods,
                    module,
                } => {
                    let info = TraitInfo {
                        param: param.clone(),
                        supers: supers.clone(),
                        methods: methods
                            .iter()
                            .map(|m| (m.name.clone(), m.sig.clone()))
                            .collect(),
                        module: module.clone(),
                    };
                    TRAIT_DEFS.with(|t| t.borrow_mut().insert(name.clone(), info.clone()));
                    let saved = TRAITENV.with(|t| {
                        let mut env = t.borrow_mut();
                        let saved = env.clone();
                        env.traits.insert(name.clone(), info);
                        saved
                    });
                    saved_traitenvs.push(saved);
                    for m in methods {
                        let scheme =
                            unify::with_store(|s| mint_method_scheme(s, name, param, &m.sig));
                        ctx2.insert(m.name.clone(), (Binding::Poly(scheme), stage));
                    }
                }
                BlockItem::Instance {
                    trait_name,
                    head,
                    requires,
                    methods,
                    module,
                } => {
                    check_instance_coherence(trait_name, head)?;
                    check_instance_termination(trait_name, head, requires)?;
                    check_instance_orphan(trait_name, head, module.as_deref())?;
                    let (trait_param, trait_supers) = TRAITENV.with(|t| {
                        t.borrow()
                            .traits
                            .get(trait_name)
                            .map(|i| (i.param.clone(), i.supers.clone()))
                            .unwrap_or_default()
                    });
                    let method_bodies =
                        check_instance(&sig2, &ctx2, stage, trait_name, head, methods)?;
                    INSTANCES.with(|i| {
                        i.borrow_mut().push(InstanceInfo {
                            trait_name: trait_name.clone(),
                            head: head.clone(),
                            requires: requires.clone(),
                            trait_param,
                            trait_supers,
                            methods: methods.iter().map(|m| m.name.clone()).collect(),
                            method_bodies,
                            module: module.clone(),
                        })
                    });
                }
            }
        }

        let body_t = elaborate_inner(&sig2, &ctx2, stage, body)?;
        let mut suffix = body_t.clone();
        for item in pending.iter().rev() {
            if let TypedBlockItem::LetMut { name, .. } = item {
                if let Some(escaping) = mut_escape(name, &suffix.ty) {
                    return Err(TypeErr::MutEscapes { ty: escaping });
                }
                if captured_in_closure(name, &suffix.node) {
                    return Err(TypeErr::MutEscapes {
                        ty: suffix.ty.clone(),
                    });
                }
            }
            suffix = wrap_block_suffix(item, suffix, stage);
        }

        if pending.is_empty() {
            return Ok(body_t);
        }

        let row = pending
            .iter()
            .fold(body_t.row.clone(), |row, item| row.union(&item.bound().row));
        let layout_known = body_t.layout_known && pending.iter().all(TypedBlockItem::layout_known);
        Ok(Typed::at_layout(
            body_t.ty.clone(),
            row,
            stage,
            layout_known,
            Node::Block {
                items: pending,
                body: Box::new(body_t),
            },
        ))
    })();

    for saved in saved_traitenvs.into_iter().rev() {
        TRAITENV.with(|t| *t.borrow_mut() = saved);
    }
    for saved in saved_tyenvs.into_iter().rev() {
        TYENV.with(|t| *t.borrow_mut() = saved);
    }
    result
}

/// The recursive elaboration worker (the judgment proper). Calls itself and
/// [`elaborate_handle`]; reaches unification through the thread-local store. The
/// public [`elaborate`] wraps this with store-reset and zonk.
fn elaborate_inner(sig: &Sig, ctx: &Ctx, stage: Stage, e: &Term) -> Result<Typed, TypeErr> {
    match e {
        Term::Int(n) => Ok(Typed::at(Type::Int, Row::pure(), stage, Node::Int(*n))),
        Term::Float(bits) => Ok(Typed::at(
            Type::Float,
            Row::pure(),
            stage,
            Node::Float(*bits),
        )),
        Term::Bool(b) => Ok(Typed::at(Type::Bool, Row::pure(), stage, Node::Bool(*b))),
        Term::Unit => Ok(Typed::at(Type::Unit, Row::pure(), stage, Node::Unit)),
        // `brk` diverges, so it inhabits any expected type — a fresh variable
        // that unifies with the context (like a bottom). Pure row: it never
        // returns, so there is no observable effect to attribute. Parsing
        // already gated it behind `--brk-enable`; reaching here means it's on.
        Term::Brk => Ok(Typed::at(
            unify::with_store(|s| s.fresh_ty()),
            Row::pure(),
            stage,
            Node::Brk,
        )),
        Term::Str(s) => Ok(Typed::at(
            Type::Str,
            Row::single(Label::Gc),
            stage,
            Node::Str(s.clone()),
        )),
        Term::Block(items, body) => elaborate_block(sig, ctx, stage, items, body),

        // (extern) — a foreign function. The `winapi` effect is injected onto
        // the arrow (calling the OS always shows in the row); the reference
        // itself is a pure value. Its DLL is the oracle's job, not the surface.
        Term::Extern(sym, Some(ty), mint) => {
            // Read the native widths off the declared type, then **erase** them
            // to `Int` so the value world stays uniform — only the foreign call
            // converts. The minted label (`mints (L)`, default `winapi`) rides on
            // the *innermost* arrow (where the OS call fires); a multi-argument
            // extern's partial applications stay pure.
            let abi = ty.extern_abi();
            let erased = ty.erase_widths();
            let label = mint
                .clone()
                .unwrap_or_else(|| Label::World("winapi".into()));
            let injected =
                inject_mint(&erased, &label).ok_or_else(|| TypeErr::NotAFunction(ty.clone()))?;
            Ok(Typed::at(
                injected,
                Row::pure(),
                stage,
                Node::Extern(sym.clone(), abi),
            ))
        }
        // A bare `extern "sym"` — its signature comes from the Win32 oracle,
        // which is wired into `locusc` (run/build) and fills it in before we get
        // here. Reaching here means no oracle ran (the std-only `locus` checker).
        Term::Extern(sym, None, _) => Err(TypeErr::BareExtern(sym.clone())),

        // `extern asm "sym" : T` — a Layer-0 symbol (D5). Identical lowering to a
        // typed `extern` (a `Node::Extern` call), but it mints **`asm`** (the
        // strongest sealed power) on the innermost arrow, and its symbol is
        // AOT-embedded from a `.masm` unit rather than resolved from a DLL.
        Term::ExternAsm(sym, ty) => {
            // A6 GC-safety gate: asm is GC-blind, so its signature must be too —
            // no gc-managed (movable) value may cross the boundary.
            if let Some(bad) = arrow_mentions_gc_datum(ty) {
                return Err(TypeErr::AsmGcType { ty: bad });
            }
            let abi = ty.extern_abi();
            let erased = ty.erase_widths();
            let injected = inject_mint(&erased, &Label::World("asm".into()))
                .ok_or_else(|| TypeErr::NotAFunction(ty.clone()))?;
            Ok(Typed::at(
                injected,
                Row::pure(),
                stage,
                Node::Extern(sym.clone(), abi),
            ))
        }

        // (bin) — bare arithmetic can be monomorphic `Int` or `Float` depending
        // on operands (`Int` remains the default for unconstrained variables).
        // Explicit wrapping/checked overflow and bitwise/shifts are `Int` only.
        // Checked overflow operators add `exn[Overflow]`.
        Term::Bin(op, a, b) => {
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            let tb = elaborate_inner(sig, ctx, stage, b)?;
            if let Some((ty, ta, tb)) = vector_scalar_bin_operands(*op, &ta, &tb)? {
                let row = ta.row.union(&tb.row);
                return Ok(Typed::at(
                    ty,
                    row,
                    stage,
                    Node::Bin(*op, Box::new(ta), Box::new(tb)),
                ));
            }
            if let Some(ty) = vector_vector_bin_type(*op, &ta.ty, &tb.ty) {
                let row = ta.row.union(&tb.row);
                return Ok(Typed::at(
                    ty,
                    row,
                    stage,
                    Node::Bin(*op, Box::new(ta), Box::new(tb)),
                ));
            }

            let operand_ty = match op {
                BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Div
                | BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Le
                | BinOp::Gt
                | BinOp::Ge => numeric_operand_type(*op, &ta.ty, &tb.ty),
                BinOp::AddWrap
                | BinOp::SubWrap
                | BinOp::MulWrap
                | BinOp::AddChecked
                | BinOp::SubChecked
                | BinOp::MulChecked
                | BinOp::Mod
                | BinOp::And
                | BinOp::Or
                | BinOp::Xor
                | BinOp::Shl
                | BinOp::Shr => Type::Int,
            };
            demand_eq(&operand_ty, &ta.ty)?;
            demand_eq(&operand_ty, &tb.ty)?;
            let ty = if op.is_comparison() {
                Type::Bool
            } else {
                operand_ty
            };
            let mut row = ta.row.union(&tb.row);
            if op.is_checked_overflow() {
                row = row.union(&overflow_effect());
            }
            Ok(Typed::at(
                ty,
                row,
                stage,
                Node::Bin(*op, Box::new(ta), Box::new(tb)),
            ))
        }

        Term::Cast(op, a) => {
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            let (from, to) = match op {
                CastOp::ToFloat => (Type::Int, Type::Float),
                CastOp::Floor | CastOp::Round => (Type::Float, Type::Int),
                CastOp::ToFloat32 => (Type::Float, Type::Float32),
                CastOp::FromFloat32 => (Type::Float32, Type::Float),
            };
            demand_eq(&from, &ta.ty)?;
            let row = ta.row.clone();
            Ok(Typed::at(to, row, stage, Node::Cast(*op, Box::new(ta))))
        }

        Term::Sqrt(a) => {
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            let operand_ty = float_math_unary_operand_type(&ta.ty);
            demand_eq(&operand_ty, &ta.ty)?;
            let row = ta.row.clone();
            Ok(Typed::at(
                operand_ty,
                row,
                stage,
                Node::FloatMathUnary(FloatMathOp::Sqrt, Box::new(ta)),
            ))
        }

        Term::Sum(a) | Term::Length(a) => {
            let op = if matches!(e, Term::Sum(_)) {
                FloatMathOp::Sum
            } else {
                FloatMathOp::Length
            };
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            let ty = vector_reduce_result_type(&ta.ty).ok_or_else(|| TypeErr::Mismatch {
                expected: example_vector_type(),
                found: ta.ty.clone(),
            })?;
            let row = ta.row.clone();
            Ok(Typed::at(
                ty,
                row,
                stage,
                Node::FloatMathUnary(op, Box::new(ta)),
            ))
        }

        Term::Dot(a, b) => {
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            let tb = elaborate_inner(sig, ctx, stage, b)?;
            let Some((operand_ty, ty)) = vector_dot_operand_type(&ta.ty, &tb.ty) else {
                let expected = vector_parts(&ta.ty)
                    .map(|_| ta.ty.clone())
                    .unwrap_or_else(example_vector_type);
                let found = if vector_parts(&ta.ty).is_some() {
                    tb.ty.clone()
                } else {
                    ta.ty.clone()
                };
                return Err(TypeErr::Mismatch { expected, found });
            };
            demand_eq(&operand_ty, &ta.ty)?;
            demand_eq(&operand_ty, &tb.ty)?;
            let row = ta.row.union(&tb.row);
            Ok(Typed::at(
                ty,
                row,
                stage,
                Node::FloatMathBinary(FloatMathOp::Dot, Box::new(ta), Box::new(tb)),
            ))
        }

        Term::MaskReduce(op, a) => {
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            mask_shape(&ta.ty).ok_or_else(|| TypeErr::Mismatch {
                expected: example_mask_type(),
                found: ta.ty.clone(),
            })?;
            let row = ta.row.clone();
            Ok(Typed::at(
                Type::Bool,
                row,
                stage,
                Node::MaskReduce(*op, Box::new(ta)),
            ))
        }

        Term::Select(mask, then_value, else_value) => {
            let tm = elaborate_inner(sig, ctx, stage, mask)?;
            let Some(shape) = mask_shape(&tm.ty) else {
                return Err(TypeErr::Mismatch {
                    expected: example_mask_type(),
                    found: tm.ty.clone(),
                });
            };
            let tt = elaborate_inner(sig, ctx, stage, then_value)?;
            let te = elaborate_inner(sig, ctx, stage, else_value)?;
            let Some((then_shape, elem_ty)) = vector_parts(&tt.ty) else {
                return Err(TypeErr::Mismatch {
                    expected: Type::Vector(shape, Box::new(Type::Float32)),
                    found: tt.ty.clone(),
                });
            };
            if then_shape != shape || !is_vector_lane_type(&elem_ty) {
                return Err(TypeErr::Mismatch {
                    expected: Type::Vector(shape, Box::new(Type::Float32)),
                    found: tt.ty.clone(),
                });
            }
            let expected = Type::Vector(shape, Box::new(elem_ty));
            demand_eq(&expected, &te.ty)?;
            let row = tm.row.union(&tt.row).union(&te.row);
            Ok(Typed::at(
                expected,
                row,
                stage,
                Node::VectorSelect {
                    mask: Box::new(tm),
                    then_value: Box::new(tt),
                    else_value: Box::new(te),
                },
            ))
        }

        Term::Fma(a, b, c) => {
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            let tb = elaborate_inner(sig, ctx, stage, b)?;
            let tc = elaborate_inner(sig, ctx, stage, c)?;
            let operand_ty = float_math_ternary_operand_type(&ta.ty, &tb.ty, &tc.ty);
            demand_eq(&operand_ty, &ta.ty)?;
            demand_eq(&operand_ty, &tb.ty)?;
            demand_eq(&operand_ty, &tc.ty)?;
            let row = ta.row.union(&tb.row).union(&tc.row);
            Ok(Typed::at(
                operand_ty,
                row,
                stage,
                Node::FloatMathTernary(FloatMathOp::Fma, Box::new(ta), Box::new(tb), Box::new(tc)),
            ))
        }

        Term::VectorLit(shape, es) => {
            if es.len() != shape.lanes() {
                return Err(TypeErr::CtorArity {
                    ctor: shape.name().to_string(),
                    expected: shape.lanes(),
                    found: es.len(),
                });
            }
            let mut typed = Vec::with_capacity(es.len());
            let mut row = Row::pure();
            let mut elem_ty: Option<Type> = None;
            for e in es {
                let t = elaborate_inner(sig, ctx, stage, e)?;
                row = row.union(&t.row);
                match &elem_ty {
                    None => elem_ty = Some(t.ty.clone()),
                    Some(et) => demand_eq(et, &t.ty)?,
                }
                typed.push(t);
            }
            let elem_ty = elem_ty.expect("shape arity is non-zero");
            if !is_vector_lane_type(&elem_ty) {
                return Err(TypeErr::Mismatch {
                    expected: Type::Float32,
                    found: elem_ty,
                });
            }
            let ty = Type::Vector(*shape, Box::new(elem_ty));
            let layout_known = storage_layout(&ty).known;
            Ok(Typed::at_layout(
                ty,
                row,
                stage,
                layout_known,
                Node::VectorLit {
                    shape: *shape,
                    elems: typed,
                },
            ))
        }

        Term::VectorSplat(shape, value) => {
            let value = elaborate_inner(sig, ctx, stage, value)?;
            if !is_vector_lane_type(&value.ty) {
                return Err(TypeErr::Mismatch {
                    expected: Type::Float32,
                    found: value.ty,
                });
            }
            let ty = Type::Vector(*shape, Box::new(value.ty.clone()));
            let row = value.row.clone();
            let layout_known = storage_layout(&ty).known;
            Ok(Typed::at_layout(
                ty,
                row,
                stage,
                layout_known,
                Node::VectorSplat {
                    shape: *shape,
                    value: Box::new(value),
                },
            ))
        }

        // `loadShape(arr, i)` — a packed array vector load (SIMD Sprint 2). The
        // array must be an `Array[E]` whose element type `E` is exactly the
        // vector's lane type (a clear error if you load an `Array[Int]` as a
        // float vector — `Int` is not a lane type — or mismatch element/lane).
        // `i : Int`. The result is `Vector(shape, E)`. `! {gc}` (a managed read).
        Term::VectorLoad { shape, arr, idx } => {
            let a = elaborate_inner(sig, ctx, stage, arr)?;
            let i = elaborate_inner(sig, ctx, stage, idx)?;
            expect_int(&i)?;
            let Type::Array(elem) = &a.ty else {
                return Err(TypeErr::NotArray(a.ty.clone()));
            };
            let elem_ty = (**elem).clone();
            // The array element must be a vector lane type — and so the vector's
            // lane type equals it. Loading an `Array[Int]` is rejected here.
            if !is_vector_lane_type(&elem_ty) {
                return Err(TypeErr::Mismatch {
                    expected: Type::Vector(*shape, Box::new(Type::Float32)),
                    found: a.ty.clone(),
                });
            }
            let ty = Type::Vector(*shape, Box::new(elem_ty));
            let row = a.row.union(&i.row).union(&Row::single(Label::Gc));
            let layout_known = storage_layout(&ty).known;
            Ok(Typed::at_layout(
                ty,
                row,
                stage,
                layout_known,
                Node::VectorLoad {
                    shape: *shape,
                    arr: Box::new(a),
                    idx: Box::new(i),
                },
            ))
        }

        // `storeShape(arr, i, v)` — the matching packed store. The vector `v`'s
        // type must be exactly `Vector(shape, E)` for the array's element type
        // `E` (so both the SHAPE and the lane/element type must agree). Yields
        // `Unit`. `! {gc}`.
        Term::VectorStore {
            shape,
            arr,
            idx,
            value,
        } => {
            let a = elaborate_inner(sig, ctx, stage, arr)?;
            let i = elaborate_inner(sig, ctx, stage, idx)?;
            let v = elaborate_inner(sig, ctx, stage, value)?;
            expect_int(&i)?;
            let Type::Array(elem) = &a.ty else {
                return Err(TypeErr::NotArray(a.ty.clone()));
            };
            let elem_ty = (**elem).clone();
            if !is_vector_lane_type(&elem_ty) {
                return Err(TypeErr::Mismatch {
                    expected: Type::Vector(*shape, Box::new(Type::Float32)),
                    found: a.ty.clone(),
                });
            }
            // The stored vector's type is exactly `shape` over the element type;
            // `demand_eq` reports a clean mismatch on either a shape or a lane
            // disagreement (e.g. a `Quad[Float]` into an `Array[Float32]`).
            let want = Type::Vector(*shape, Box::new(elem_ty));
            demand_eq(&want, &v.ty)?;
            let row = a
                .row
                .union(&i.row)
                .union(&v.row)
                .union(&Row::single(Label::Gc));
            Ok(Typed::at(
                Type::Unit,
                row,
                stage,
                Node::VectorStore {
                    shape: *shape,
                    arr: Box::new(a),
                    idx: Box::new(i),
                    value: Box::new(v),
                },
            ))
        }

        // (mem) — raw memory access. Every operand is an `Int` (addresses,
        // values, byte and count are all machine words in the uniform model);
        // each primitive performs the **`mem`** effect (a sealed kernel
        // capability, like `winapi`). `peek` yields `Int`; the rest yield `Unit`.
        Term::Peek(w, addr) => {
            let a = elaborate_inner(sig, ctx, stage, addr)?;
            expect_int(&a)?;
            let row = a.row.union(&mem_effect());
            Ok(Typed::at(
                Type::Int,
                row,
                stage,
                Node::Peek(*w, Box::new(a)),
            ))
        }
        Term::Poke(w, addr, val) => {
            let a = elaborate_inner(sig, ctx, stage, addr)?;
            let v = elaborate_inner(sig, ctx, stage, val)?;
            expect_int(&a)?;
            expect_int(&v)?;
            let row = a.row.union(&v.row).union(&mem_effect());
            Ok(Typed::at(
                Type::Unit,
                row,
                stage,
                Node::Poke(*w, Box::new(a), Box::new(v)),
            ))
        }
        Term::Fill(dst, byte, count) => {
            let d = elaborate_inner(sig, ctx, stage, dst)?;
            let b = elaborate_inner(sig, ctx, stage, byte)?;
            let n = elaborate_inner(sig, ctx, stage, count)?;
            expect_int(&d)?;
            expect_int(&b)?;
            expect_int(&n)?;
            let row = d.row.union(&b.row).union(&n.row).union(&mem_effect());
            Ok(Typed::at(
                Type::Unit,
                row,
                stage,
                Node::Fill(Box::new(d), Box::new(b), Box::new(n)),
            ))
        }
        Term::Copy(dst, src, count) => {
            let d = elaborate_inner(sig, ctx, stage, dst)?;
            let s = elaborate_inner(sig, ctx, stage, src)?;
            let n = elaborate_inner(sig, ctx, stage, count)?;
            expect_int(&d)?;
            expect_int(&s)?;
            expect_int(&n)?;
            let row = d.row.union(&s.row).union(&n.row).union(&mem_effect());
            Ok(Typed::at(
                Type::Unit,
                row,
                stage,
                Node::Copy(Box::new(d), Box::new(s), Box::new(n)),
            ))
        }

        // (index) — the array accessor. The base is a string or address (its
        // type fixes the element width); the index is `Int`. A read yields `Int`,
        // a store `Unit`; both perform `mem`. Sugar over `peek`/`poke` (the IR
        // computes `a + i*stride` and loads/stores) — so a `String` base never
        // needs to surface in pointer arithmetic.
        // `a[i]` — DISPATCH on `a`'s type: a high-level managed `Array[T]` read
        // (bounds-checked, typed, `! {gc}`), or the low-level `mem` accessor over
        // a `String`/address (`! {mem}`).
        Term::Index(base, idx) => {
            let b = elaborate_inner(sig, ctx, stage, base)?;
            let i = elaborate_inner(sig, ctx, stage, idx)?;
            expect_int(&i)?;
            if let Type::Array(elem) = &b.ty {
                let elem_ty = (**elem).clone();
                let elem_layout = storage_layout(&elem_ty);
                let row = b.row.union(&i.row).union(&Row::single(Label::Gc));
                Ok(Typed::at_layout(
                    elem_ty,
                    row,
                    stage,
                    elem_layout.known,
                    Node::ArrayGet {
                        arr: Box::new(b),
                        idx: Box::new(i),
                        elem_layout,
                    },
                ))
            } else if b.ty == Type::Str {
                let elem_layout = ValueLayout::scalar_bytes(2, 2);
                let row = b.row.union(&i.row).union(&Row::single(Label::Gc));
                Ok(Typed::at_layout(
                    Type::Int,
                    row,
                    stage,
                    elem_layout.known,
                    Node::ArrayGet {
                        arr: Box::new(b),
                        idx: Box::new(i),
                        elem_layout,
                    },
                ))
            } else {
                let w = elem_width(&b.ty)?;
                let row = b.row.union(&i.row).union(&mem_effect());
                Ok(Typed::at(
                    Type::Int,
                    row,
                    stage,
                    Node::Index(w, Box::new(b), Box::new(i)),
                ))
            }
        }
        Term::IndexSet(base, idx, val) => {
            let b = elaborate_inner(sig, ctx, stage, base)?;
            let i = elaborate_inner(sig, ctx, stage, idx)?;
            let v = elaborate_inner(sig, ctx, stage, val)?;
            expect_int(&i)?;
            if let Type::Array(elem) = &b.ty {
                let elem_ty = (**elem).clone();
                demand_eq(&elem_ty, &v.ty)?;
                let elem_layout = storage_layout(&elem_ty);
                let row = b
                    .row
                    .union(&i.row)
                    .union(&v.row)
                    .union(&Row::single(Label::Gc));
                Ok(Typed::at_layout(
                    Type::Unit,
                    row,
                    stage,
                    elem_layout.known,
                    Node::ArraySet {
                        arr: Box::new(b),
                        idx: Box::new(i),
                        val: Box::new(v),
                        elem_layout,
                    },
                ))
            } else {
                expect_int(&v)?;
                let w = elem_width(&b.ty)?;
                let row = b.row.union(&i.row).union(&v.row).union(&mem_effect());
                Ok(Typed::at(
                    Type::Unit,
                    row,
                    stage,
                    Node::IndexSet(w, Box::new(b), Box::new(i), Box::new(v)),
                ))
            }
        }

        // `[e1, …, en]` — a homogeneous array literal. Performs `gc` (it allocates).
        Term::ArrayLit(es) => {
            let mut typed = Vec::with_capacity(es.len());
            let mut row = Row::single(Label::Gc);
            let mut elem_ty: Option<Type> = None;
            for e in es {
                let t = elaborate_inner(sig, ctx, stage, e)?;
                row = row.union(&t.row);
                match &elem_ty {
                    None => elem_ty = Some(t.ty.clone()),
                    // Every element shares the first's type — a typing demand.
                    Some(et) => demand_eq(et, &t.ty)?,
                }
                typed.push(t);
            }
            let elem_ty = elem_ty.expect("array literal has >= 1 element (parser-checked)");
            let elem_layout = storage_layout(&elem_ty);
            Ok(Typed::at_layout(
                Type::Array(Box::new(elem_ty)),
                row,
                stage,
                elem_layout.known,
                Node::ArrayLit {
                    elems: typed,
                    elem_layout,
                },
            ))
        }
        // `len a` — an array's element count.
        Term::Len(a) => {
            let ta = elaborate_inner(sig, ctx, stage, a)?;
            if !matches!(ta.ty, Type::Array(_) | Type::Str) {
                return Err(TypeErr::NotArray(ta.ty.clone()));
            }
            let row = ta.row.union(&Row::single(Label::Gc));
            Ok(Typed::at(Type::Int, row, stage, Node::Len(Box::new(ta))))
        }

        // `type Name[a, …] = … in body` (D9/D10) — register the sum type's
        // constructors (scheme-shaped) for the body's scope. The declaration has
        // no value; the result IS the body.
        //
        // For each declared parameter we mint **one fresh `TyVarId`**, shared by
        // every constructor of this type (so `Nil` and `Cons` of `List[a]` both
        // quantify the same `a`); a `Construct`/`Match` use then instantiates it
        // fresh. The param-name→var map drives `rewrite_params` over each field
        // type and over the result `Named(Name, [param vars])`. A monomorphic sum
        // has no params: the map is empty, `rewrite_params` is the identity, and
        // `result` is `Named(Name, [])` — byte-for-byte the pre-S3 registration.
        Term::TypeDef {
            name,
            params,
            variants,
            module,
            body,
        } => {
            // **Type-declaration well-formedness (P2).** Reject a duplicate
            // parameter (`type T[a, a]`) or constructor (`type T = A | A`) before
            // the registration `HashMap`s silently collapse them
            // (`review-findings-2026-06-01.md`).
            check_typedef_decl(name, params, variants)?;
            // Record the type's declaring module for the orphan check (R5); a
            // bare (module-less) `type` records nothing.
            if let Some(m) = module {
                TRAITENV.with(|t| {
                    t.borrow_mut().type_modules.insert(name.clone(), m.clone());
                });
            }
            let (saved, subst) = TYENV.with(|t| {
                let mut env = t.borrow_mut();
                let saved = env.clone();
                env.sums
                    .insert(name.clone(), (params.clone(), variants.clone()));
                // Fresh quantified var per parameter, shared across all ctors.
                let param_vars: Vec<TyVarId> = params
                    .iter()
                    .map(|_| match unify::with_store(|s| s.fresh_ty()) {
                        Type::Var(id) => id,
                        _ => unreachable!("fresh_ty yields a Var"),
                    })
                    .collect();
                let subst: HashMap<String, Type> = params
                    .iter()
                    .zip(&param_vars)
                    .map(|(p, &id)| (p.clone(), Type::Var(id)))
                    .collect();
                // The result type all ctors of this sum produce: `Name[a, …]`
                // with the params as (quantified) args.
                let result = Type::Named(
                    name.clone(),
                    param_vars.iter().map(|&id| Type::Var(id)).collect(),
                );
                for (tag, (ctor, fields)) in variants.iter().enumerate() {
                    let fields = fields.iter().map(|f| rewrite_params(f, &subst)).collect();
                    env.ctors.insert(
                        ctor.clone(),
                        CtorInfo {
                            type_name: name.clone(),
                            tag: tag as i64,
                            ty_params: param_vars.clone(),
                            fields,
                            result: result.clone(),
                        },
                    );
                }
                (saved, subst)
            });
            // **P2 arity (D14).** Every nominal in a field type must be named with
            // the right type-argument count — *including the recursive
            // self-reference* (`type List[a] = … Cons(a, List[a, Int])` is now
            // `RN-E0225`, not silently accepted). Validate the **rewritten** field
            // types: params are now `Var`s (skipped), while the self `name` (just
            // registered) and any other declared sum are checked. Run inside the
            // restore flow so `TYENV` unwinds whether this or the body errors.
            let result = (|| -> Result<Typed, TypeErr> {
                for (_, fields) in variants {
                    for f in fields {
                        validate_named_arity(&rewrite_params(f, &subst))?;
                    }
                }
                elaborate_inner(sig, ctx, stage, body)
            })();
            TYENV.with(|t| *t.borrow_mut() = saved);
            result
        }

        // `C(args)` — build a sum value (D12). **Instantiate** the constructor's
        // scheme (fresh vars for its sum's params), unify each argument against
        // its instantiated field type — which **solves** the param images — then
        // the result is `Named(Name, [solved param images])`. For a monomorphic
        // sum, `ty_params == []`: instantiation is the identity and this is the
        // pre-S3 concrete `demand_eq(fty,…)` loop. Performs `gc`.
        Term::Construct(ctor, args) => {
            let info = TYENV.with(|t| t.borrow().ctors.get(ctor).cloned());
            let Some(info) = info else {
                return Err(TypeErr::UnknownCtor(ctor.clone()));
            };
            if args.len() != info.fields.len() {
                return Err(TypeErr::CtorArity {
                    ctor: ctor.clone(),
                    expected: info.fields.len(),
                    found: args.len(),
                });
            }
            // Fresh copy of the constructor's (field types, result) — the param
            // vars in both are the SAME fresh vars, so solving a field solves the
            // result's argument.
            let (inst_fields, inst_result) = unify::with_store(|s| {
                instantiate_ctor(s, &info.ty_params, &info.fields, &info.result)
            });
            let mut typed = Vec::with_capacity(args.len());
            let mut row = Row::single(Label::Gc);
            let mut fields_layout_known = true;
            for ((arg, fty), decl_fty) in args.iter().zip(&inst_fields).zip(&info.fields) {
                let ta = elaborate_inner(sig, ctx, stage, arg)?;
                // Solve the field's param image to the argument's type.
                demand_eq(fty, &ta.ty)?;
                row = row.union(&ta.row);
                // The field's STORAGE layout keys on the **declared** field type,
                // not the solved one: a declared `Var` field is a traced *word
                // cell* (D4 — laid in the pointer region, stored verbatim) no
                // matter what concrete scalar/handle the argument happens to be.
                // Reading the resolved `fty` here would collapse a `Var` field to
                // its argument's layout (`Int` → scalar cell) and store a raw
                // scalar in an untraced region — the boxing-era bug this slice
                // retires. (`decl_fty` is the raw param `Var` from the registry,
                // un-resolved, so `Type::storage_layout` returns `word_cell`.)
                let layout = decl_fty.storage_layout();
                fields_layout_known &= layout.known;
                // Representation coercion (repr-poly; docs/repr-poly-impl.md): a
                // scalar argument flowing into a type-variable field slot is
                // **tagged** (`value << 2`) into a uniform word. The DECLARED field
                // type keeps the sum's type-param as a `Var` — the polymorphic-slot
                // signal — while the resolved argument type gives its concrete
                // representation. A still-polymorphic argument resolves to a `Var`
                // and is left alone (the verbatim passthrough); a concrete handle
                // is already uniform. Its tag lands wherever a scalar is pinned.
                let arg_ty = unify::with_store(|s| s.resolve_ty(&ta.ty));
                // Tag a scalar arg, or ToPtr a concrete managed-handle arg, into a
                // `Var`-declared field (the matrix decides which). A still-`Var`
                // (passthrough) or `Str` arg coerces `None` and rides verbatim.
                let coer = Type::coercion(decl_fty, &arg_ty);
                let ta = if matches!(coer, Coercion::Tag | Coercion::ToPtr) {
                    let (ty, r, st) = (ta.ty.clone(), ta.row.clone(), ta.stage);
                    Typed::at_layout(
                        ty,
                        r,
                        st,
                        true,
                        Node::Coerce {
                            kind: coer,
                            // The boundary T0 re-checks: the DECLARED field slot
                            // (the param `Var`) and the value's resolved type.
                            slot: decl_fty.clone(),
                            value: arg_ty.clone(),
                            inner: Box::new(ta),
                        },
                    )
                } else {
                    ta
                };
                // Record the declared field slot alongside the typed arg so T0 can
                // decide the un-coerced fields too (the registry is gone by then).
                typed.push((ta, layout, decl_fty.clone()));
            }
            // The result type carries the solved param images (`List[Int]`); zonk
            // happens at the end via the public `elaborate` wrapper, but the
            // *structure* (`Named(Name, args)`) is fixed here.
            Ok(Typed::at_layout(
                inst_result,
                row,
                stage,
                fields_layout_known,
                Node::Construct {
                    tag: info.tag,
                    args: typed,
                },
            ))
        }

        // `match s with | pat => body | …` — tag-dispatched elimination (D13).
        Term::Match { scrutinee, arms } => {
            let scrut = elaborate_inner(sig, ctx, stage, scrutinee)?;
            // The scrutinee must be a nominal sum; we keep `scrut.ty` (with its
            // type arguments, e.g. `List[Int]`) to **refine** each arm against.
            // RESOLVE through the store first: a match binder whose declared field
            // type is a refined `Var` (bound to a Named sum but not yet zonked, e.g.
            // a FromPtr-rebound handle binder `h`) must read its SOLVED head, or it
            // spuriously fails `NotASum`. `resolve` is the identity on a concrete
            // head; `scrut.ty` itself still drives the per-arm refinement below.
            let Type::Named(tyname, _) = resolved_type(&scrut.ty) else {
                return Err(TypeErr::NotASum(scrut.ty.clone()));
            };
            let entry = TYENV.with(|t| t.borrow().sums.get(&tyname).cloned());
            let Some((_params, variants)) = entry else {
                return Err(TypeErr::NotASum(scrut.ty.clone()));
            };
            let mut typed_arms = Vec::new();
            let mut result_ty: Option<Type> = None;
            let mut row = scrut.row.union(&Row::single(Label::Gc));
            let mut covered = std::collections::HashSet::new();
            let mut has_wild = false;
            let mut match_layout_known = true;
            for arm in arms {
                // Resolve the pattern → (tag, the fields it binds with their
                // *refined* types).
                let (tag, mut binds): (Option<i64>, Vec<(String, usize, ValueLayout, Type)>) =
                    match &arm.pat {
                        Pattern::Wild => {
                            has_wild = true;
                            (None, Vec::new())
                        }
                        Pattern::Ctor(ctor, names) => {
                            let info = TYENV.with(|t| t.borrow().ctors.get(ctor).cloned());
                            let Some(info) = info else {
                                return Err(TypeErr::UnknownCtor(ctor.clone()));
                            };
                            if names.len() != info.fields.len() {
                                return Err(TypeErr::CtorArity {
                                    ctor: ctor.clone(),
                                    expected: info.fields.len(),
                                    found: names.len(),
                                });
                            }
                            // **Instantiate** the ctor (fresh param vars in its fields
                            // + result), then **refine**: unify its result against the
                            // scrutinee's actual type. For `Cons` matched on
                            // `List[Int]`, this solves the ctor's `?a := Int`, so its
                            // field `?a` becomes `Int`. A ctor of a DIFFERENT sum has a
                            // different `Named` name and this unify FAILS — exactly the
                            // role the old nominal-name `demand_eq` played (now folded
                            // into the refinement). The error stays a `Mismatch`.
                            let (inst_fields, inst_result) = unify::with_store(|s| {
                                instantiate_ctor(s, &info.ty_params, &info.fields, &info.result)
                            });
                            demand_eq(&scrut.ty, &inst_result)?;
                            covered.insert(ctor.clone());
                            // Preserve representation-polymorphic fields for binder
                            // types, but mark this match so lowering refuses the
                            // placeholder layout.
                            match_layout_known &= layout_known(inst_fields.iter());
                            let binds = names
                                .iter()
                                .enumerate()
                                .map(|(i, nm)| {
                                    let (slot, layout) = ctor_field_slot(&inst_fields, i);
                                    // Keep the binder's refined type before D6
                                    // defaulting, so generic recursive code remains
                                    // polymorphic.
                                    (nm.clone(), slot, layout, inst_fields[i].clone())
                                })
                                .collect();
                            (Some(info.tag), binds)
                        }
                    };
                let mut ctx2 = ctx.clone();
                for (nm, _, _, fty) in &binds {
                    // A matched payload field is a monomorphic binder, at its
                    // *refined* type (so `Cons(h, t)` on `List[Int]` binds `h:Int`).
                    ctx2.insert(nm.clone(), (Binding::Mono(fty.clone()), stage));
                }
                let mut body = elaborate_inner(sig, &ctx2, stage, &arm.body)?;
                row = row.union(&body.row);
                match &result_ty {
                    None => result_ty = Some(body.ty.clone()),
                    // All arms agree on a result type — a typing demand.
                    Some(rt) => demand_eq(rt, &body.ty)?,
                }
                // Read-side coercion (T6, Untag): a `Var`-cell binder (a `word_cell`,
                // stored as a tagged word) that **resolves to a concrete scalar** is a
                // tagged word read as a raw int — its uses need `Untag` (>>2). A binder
                // that stays a `Var` (the passthrough, e.g. `list_reverse`'s `h`)
                // coerces `None` and rides verbatim. Computed PRE-ZONK via `resolve_ty`
                // (zonk defaults the generic `Var` to `Int` and would erase this). We
                // bind the raw word under a fresh `$nm$tag` and rebind the user name to
                // its untagged value over the body, reusing the `let` + `Coerce{Untag}`
                // lowering — so T0 sees the coercion present and Repr-correct.
                for b in binds.iter_mut() {
                    let fty = b.3.clone();
                    let resolved = unify::with_store(|s| s.resolve_ty(&fty));
                    // Untag a scalar-resolved binder, or FromPtr a managed-handle-
                    // resolved binder (intern the `addr|10` word back to a real
                    // handle). A still-`Var` (passthrough) binder coerces `None`.
                    let coer = Type::coercion(&resolved, &fty);
                    if matches!(coer, Coercion::Untag | Coercion::FromPtr) {
                        let orig = b.0.clone();
                        let raw = format!("${orig}$tag");
                        let var_raw = Typed::at_layout(
                            fty.clone(),
                            Row::pure(),
                            stage,
                            true,
                            Node::Var(raw.clone()),
                        );
                        let untag = Typed::at(
                            resolved.clone(),
                            Row::pure(),
                            stage,
                            Node::Coerce {
                                // The binder rides as a uniform word (its `word_cell`
                                // storage), so T0's slot/value check must read it
                                // `Uniform`. The refined `fty` (`?65`) resolves to
                                // `Int` and would read as a no-op `None`; a fresh
                                // unbound var stays `Uniform` under T0's resolve —
                                // the read-side analog of the Construct boundary's
                                // unresolved ctor param var. (Lowering keys on `kind`,
                                // never `value`, so the fresh var is inert past T0.)
                                // For FromPtr the same holds: slot is the resolved
                                // handle, value the fresh `Var` — coercion reads
                                // (handle, Var) -> FromPtr.
                                kind: coer,
                                slot: resolved.clone(),
                                value: unify::with_store(|s| s.fresh_ty()),
                                inner: Box::new(var_raw),
                            },
                        );
                        body = Typed::at_layout(
                            body.ty.clone(),
                            body.row.clone(),
                            body.stage,
                            body.layout_known,
                            Node::Let {
                                name: orig,
                                bound: Box::new(untag),
                                body: Box::new(body),
                            },
                        );
                        b.0 = raw;
                    }
                }
                typed_arms.push(MatchArmT { tag, binds, body });
            }
            if !has_wild {
                for (ctor, _) in &variants {
                    if !covered.contains(ctor) {
                        return Err(TypeErr::NonExhaustive {
                            ty: tyname.clone(),
                            missing: ctor.clone(),
                        });
                    }
                }
            }
            let result_ty = result_ty.ok_or(TypeErr::EmptyMatch)?;
            Ok(Typed::at_layout(
                result_ty,
                row,
                stage,
                match_layout_known,
                Node::Match {
                    scrutinee: Box::new(scrut),
                    arms: typed_arms,
                },
            ))
        }

        // (tuple) — a product. Each element keeps its type; the row is their
        // union; the value is `(T1, …, Tn)`.
        Term::Tuple(es) => {
            let mut typed = Vec::with_capacity(es.len());
            // Building the heap struct performs the `gc` effect: a tuple is a
            // managed-heap allocation, so the row is honest about touching it.
            let mut row = Row::single(Label::Gc);
            let mut tys = Vec::with_capacity(es.len());
            for e in es {
                let t = elaborate_inner(sig, ctx, stage, e)?;
                row = row.union(&t.row);
                tys.push(t.ty.clone());
                typed.push(t);
            }
            let fields_layout_known = layout_known(tys.iter());
            Ok(Typed::at_layout(
                Type::Tuple(tys),
                row,
                stage,
                fields_layout_known,
                Node::Tuple(typed),
            ))
        }

        // (let-tuple) — destructure a tuple: the bound expression must be a tuple
        // of matching arity; each name takes the corresponding element type.
        Term::LetTuple(names, e, body) => {
            let te = elaborate_inner(sig, ctx, stage, e)?;
            // Resolve through the store (a refined `Var` tuple binder, not yet
            // zonked, would otherwise spuriously fail the Tuple destructure).
            let te_ty = resolved_type(&te.ty);
            let Type::Tuple(elem_tys) = &te_ty else {
                return Err(TypeErr::Mismatch {
                    expected: Type::Tuple(vec![Type::Unit; names.len()]),
                    found: te.ty.clone(),
                });
            };
            if elem_tys.len() != names.len() {
                return Err(TypeErr::Mismatch {
                    expected: Type::Tuple(vec![Type::Unit; names.len()]),
                    found: te.ty.clone(),
                });
            }
            let mut ctx2 = ctx.clone();
            for (name, ty) in names.iter().zip(elem_tys) {
                // A destructured tuple element is a monomorphic binder.
                ctx2.insert(name.clone(), (Binding::Mono(ty.clone()), stage));
            }
            let tb = elaborate_inner(sig, &ctx2, stage, body)?;
            let row = te.row.union(&tb.row);
            let ty = tb.ty.clone();
            let fields_layout_known = layout_known(elem_tys.iter());
            Ok(Typed::at_layout(
                ty,
                row,
                stage,
                fields_layout_known,
                Node::LetTuple(names.clone(), Box::new(te), Box::new(tb)),
            ))
        }

        // (record) — a product with named fields. Elaborate each, then SORT by
        // name (so field order is irrelevant), rejecting duplicate names.
        Term::Record(fields) => {
            let mut typed: Vec<(String, Typed)> = Vec::with_capacity(fields.len());
            // Like a tuple, a record is a heap allocation → it performs `gc`.
            let mut row = Row::single(Label::Gc);
            for (name, e) in fields {
                if typed.iter().any(|(n, _)| n == name) {
                    return Err(TypeErr::DupField(name.clone()));
                }
                let t = elaborate_inner(sig, ctx, stage, e)?;
                row = row.union(&t.row);
                typed.push((name.clone(), t));
            }
            typed.sort_by(|a, b| a.0.cmp(&b.0));
            let tys = typed
                .iter()
                .map(|(n, t)| (n.clone(), t.ty.clone()))
                .collect();
            let fields_layout_known = layout_known(typed.iter().map(|(_, t)| &t.ty));
            Ok(Typed::at_layout(
                Type::Record(tys),
                row,
                stage,
                fields_layout_known,
                Node::Record(typed),
            ))
        }

        // (field) — project a named field; the record must have it.
        Term::Field(r, name) => {
            let tr = elaborate_inner(sig, ctx, stage, r)?;
            // Resolve through the store (a refined `Var` record/vector binder).
            let tr_ty = resolved_type(&tr.ty);
            match &tr_ty {
                Type::Record(fields) => {
                    let field_ty = fields
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| TypeErr::NoField(name.clone(), tr.ty.clone()))?;
                    let fields_layout_known = layout_known(fields.iter().map(|(_, t)| t));
                    Ok(Typed::at_layout(
                        field_ty,
                        tr.row.clone(),
                        stage,
                        fields_layout_known,
                        Node::Field(Box::new(tr), name.clone()),
                    ))
                }
                Type::Vector(shape, elem) => {
                    let lane = shape
                        .lane_index(name)
                        .ok_or_else(|| TypeErr::NoField(name.clone(), tr.ty.clone()))?;
                    Ok(Typed::at(
                        (**elem).clone(),
                        tr.row.clone(),
                        stage,
                        Node::VectorExtract {
                            vector: Box::new(tr),
                            lane,
                        },
                    ))
                }
                _ => Err(TypeErr::NotRecord(tr.ty.clone())),
            }
        }

        // (if) — the condition is `Bool`; the branches must agree on a type.
        Term::If(c, t, e) => {
            let tc = elaborate_inner(sig, ctx, stage, c)?;
            demand_eq(&Type::Bool, &tc.ty)?;
            let tt = elaborate_inner(sig, ctx, stage, t)?;
            let te = elaborate_inner(sig, ctx, stage, e)?;
            // The branches must agree — a typing demand (the old `==`).
            demand_eq(&tt.ty, &te.ty)?;
            let ty = tt.ty.clone();
            let row = tc.row.union(&tt.row).union(&te.row);
            Ok(Typed::at(
                ty,
                row,
                stage,
                Node::If(Box::new(tc), Box::new(tt), Box::new(te)),
            ))
        }

        // (loop) - a structured accumulator loop. Initializers are evaluated in
        // the outer context once; the condition, steps, and final result see the
        // accumulator names at their initializer types. Each step must produce
        // the next value for the corresponding accumulator.
        Term::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            if vars.is_empty() || vars.len() != steps.len() {
                return Err(TypeErr::LoopArity {
                    expected: vars.len(),
                    found: steps.len(),
                });
            }

            let mut row = Row::pure();
            let mut loop_vars = Vec::with_capacity(vars.len());
            let mut ctx2 = ctx.clone();
            for (name, init) in vars {
                let typed_init = elaborate_inner(sig, ctx, stage, init)?;
                row = row.union(&typed_init.row);
                let ty = typed_init.ty.clone();
                let layout = storage_layout(&ty);
                ctx2.insert(name.clone(), (Binding::Mono(ty.clone()), stage));
                loop_vars.push((name.clone(), ty, layout, typed_init));
            }

            let typed_cond = elaborate_inner(sig, &ctx2, stage, cond)?;
            demand_eq(&Type::Bool, &typed_cond.ty)?;
            row = row.union(&typed_cond.row);

            let mut typed_steps = Vec::with_capacity(steps.len());
            for (step, (_, ty, _, _)) in steps.iter().zip(&loop_vars) {
                let typed_step = elaborate_inner(sig, &ctx2, stage, step)?;
                demand_eq(ty, &typed_step.ty)?;
                row = row.union(&typed_step.row);
                typed_steps.push(typed_step);
            }

            let typed_result = elaborate_inner(sig, &ctx2, stage, result)?;
            row = row.union(&typed_result.row);
            let ty = typed_result.ty.clone();
            let layout_known = loop_vars
                .iter()
                .all(|(_, _, layout, init)| layout.known && init.layout_known)
                && typed_cond.layout_known
                && typed_steps.iter().all(|step| step.layout_known)
                && typed_result.layout_known;

            Ok(Typed::at_layout(
                ty,
                row,
                stage,
                layout_known,
                Node::Loop {
                    vars: loop_vars,
                    cond: Box::new(typed_cond),
                    steps: typed_steps,
                    result: Box::new(typed_result),
                },
            ))
        }

        // (var) + SO-1 (§9): a binder is usable only at stages ≤ where it was
        // bound. Higher-stage values may be used lower (CSP); never the reverse.
        // A `Poly` binding is **instantiated** with fresh variables at this use
        // (so two uses of `id` are independent); a `Mono` binding uses its type
        // directly.
        Term::Var(x) => match ctx.get(x) {
            Some((binding, bound)) => {
                if *bound < stage {
                    Err(TypeErr::StageEscape {
                        var: x.clone(),
                        bound: *bound,
                        used: stage,
                    })
                } else {
                    // For a `Poly` binding, `instantiate` emits one pending
                    // obligation per scheme constraint (under the fresh renaming).
                    // **Traits v1 Sprint 3 — the evidence→call-site bridge:** if this
                    // use is constrained, stamp it so the post-zonk dictionary pass
                    // can rewrite it. The obligation type variables `instantiate`
                    // just pushed are exactly the constraints' renamed `?a`s — read
                    // them straight back off the pending list (in push order) and
                    // record them against a fresh use id baked into the var name.
                    let t = match binding {
                        // A mutable local reads at its scalar type, exactly like a
                        // `Mono` binder — the `Mut` marker only gates *assignment*.
                        Binding::Mono(t) | Binding::Mut(t) => t.clone(),
                        Binding::Poly(scheme) => {
                            if scheme.constraints.is_empty() {
                                unify::with_store(|s| instantiate(s, scheme))
                            } else {
                                let before = unify::peek_obligations_len();
                                let t = unify::with_store(|s| instantiate(s, scheme));
                                // Clone (not drain) the obligations `instantiate`
                                // appended, in order — they stay pending for the
                                // Sprint-2 resolution pass; we only read their fresh
                                // type variables for the dictionary bridge.
                                let fresh = unify::peek_obligations_since(before);
                                let name = record_dict_use(x, &fresh, &t);
                                return Ok(Typed::at(t, Row::pure(), stage, Node::Var(name)));
                            }
                        }
                    };
                    Ok(Typed::at(t, Row::pure(), stage, Node::Var(x.clone())))
                }
            }
            None => Err(TypeErr::Unbound(x.clone())),
        },

        // (lam) — binds at the current stage; the body's row becomes latent.
        // The parameter type is the annotation when present, else a **fresh
        // unification variable at the current level** (NOT a deeper one): a
        // lambda parameter is monomorphic in its own body (rank-1), so we do
        // *not* bump the level — the var is only generalizable if an enclosing
        // `let` later finds it (e.g. `let id = fn x => x` generalizes the
        // RHS-born var). The binder is `Mono` either way.
        Term::Lam(x, ann, body) => {
            let a = match ann {
                Some(t) => {
                    // A lowercase name in the annotation is a TYPE VARIABLE (S3.5,
                    // D15): convert before validating arity (which then sees a
                    // `Var` and skips it). So `fn r: Result[a, b] => …` binds
                    // `r : Result[?a, ?b]`, and the enclosing `let` generalises it.
                    let t = unify::with_store(|s| instantiate_annotation(s, t));
                    // A nominal must be named with the right type-argument arity
                    // (D14) — `fn x: List[Int, Bool] => …` is RN-E0225 here.
                    validate_named_arity(&t)?;
                    t
                }
                None => unify::with_store(|s| s.fresh_ty()),
            };
            let mut ctx2 = ctx.clone();
            ctx2.insert(x.clone(), (Binding::Mono(a.clone()), stage));
            let b = elaborate_inner(sig, &ctx2, stage, body)?;
            let ty = Type::Fun(Box::new(a.clone()), Box::new(b.ty.clone()), b.row.clone());
            Ok(Typed::at(
                ty,
                Row::pure(),
                stage,
                Node::Lam {
                    param: x.clone(),
                    param_ty: a,
                    body: Box::new(b),
                },
            ))
        }

        // (app) — pay f's, the arg's, and the latent rows.
        Term::App(f, arg) => {
            if let Term::Var(name) = &**f {
                if let Some(label) = callable_effect_label(sig, ctx, name) {
                    let perform = Term::Perform(label, arg.clone());
                    return elaborate_inner(sig, ctx, stage, &perform);
                }
                if ctx.get(name).is_none() {
                    match name.as_str() {
                        "sum" => {
                            let builtin = Term::Sum(arg.clone());
                            return elaborate_inner(sig, ctx, stage, &builtin);
                        }
                        "length" => {
                            let builtin = Term::Length(arg.clone());
                            return elaborate_inner(sig, ctx, stage, &builtin);
                        }
                        "any" => {
                            let builtin = Term::MaskReduce(MaskReduceOp::Any, arg.clone());
                            return elaborate_inner(sig, ctx, stage, &builtin);
                        }
                        "all" => {
                            let builtin = Term::MaskReduce(MaskReduceOp::All, arg.clone());
                            return elaborate_inner(sig, ctx, stage, &builtin);
                        }
                        "select" => {
                            if let Term::Tuple(args) = &**arg {
                                if args.len() == 3 {
                                    let builtin = Term::Select(
                                        Box::new(args[0].clone()),
                                        Box::new(args[1].clone()),
                                        Box::new(args[2].clone()),
                                    );
                                    return elaborate_inner(sig, ctx, stage, &builtin);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Term::App(dot_fun, lhs) = &**f {
                if let Term::Var(name) = &**dot_fun {
                    if name == "dot" && ctx.get(name).is_none() {
                        let builtin = Term::Dot(lhs.clone(), arg.clone());
                        return elaborate_inner(sig, ctx, stage, &builtin);
                    }
                }
            }
            if let Term::App(select_fun, then_value) = &**f {
                if let Term::App(select_name, mask) = &**select_fun {
                    if let Term::Var(name) = &**select_name {
                        if name == "select" && ctx.get(name).is_none() {
                            let builtin =
                                Term::Select(mask.clone(), then_value.clone(), arg.clone());
                            return elaborate_inner(sig, ctx, stage, &builtin);
                        }
                    }
                }
            }
            let ft = elaborate_inner(sig, ctx, stage, f)?;
            let at = elaborate_inner(sig, ctx, stage, arg)?;
            match &ft.ty {
                // A **concrete** arrow (the monomorphic case, and every extern):
                // keep the existing path, including the FFI `arg_matches`
                // allowance that lets raw `Ptr`/`Int` words reach a `Ptr`
                // parameter.
                Type::Fun(dom, cod, latent) => {
                    arg_matches(dom, &at.ty)?;
                    let ty = (**cod).clone();
                    // The literal codomain — a `Var` when the callee returns a bare
                    // type variable (e.g. list_fold's `b`). Captured UN-resolved for
                    // the App-result un-coercion below (the dual of the arg `Tag`).
                    let cod_lit = ty.clone();
                    let row = ft.row.union(&at.row).union(latent);
                    // App-boundary coercion (repr-poly; docs/repr-poly-impl.md): a
                    // scalar argument into a type-variable parameter (a uniform
                    // word slot) is **tagged** — the same rule as the Construct
                    // boundary, with `dom` (the callee's declared parameter type,
                    // a `Var` when polymorphic) as the slot and the resolved
                    // argument type as the value.
                    let arg_ty = unify::with_store(|s| s.resolve_ty(&at.ty));
                    // Tag a scalar arg, or ToPtr a concrete managed-handle arg, into
                    // a `Var` parameter (the matrix decides). The arrow-wrapper
                    // branch below still handles a concrete callback into a generic
                    // arrow param.
                    let coer = Type::coercion(&**dom, &arg_ty);
                    let at = if matches!(coer, Coercion::Tag | Coercion::ToPtr) {
                        let (aty, ar, ast) = (at.ty.clone(), at.row.clone(), at.stage);
                        Typed::at_layout(
                            aty,
                            ar,
                            ast,
                            true,
                            Node::Coerce {
                                kind: coer,
                                // The boundary T0 re-checks: the callee's declared
                                // parameter slot (a `Var` when polymorphic) and the
                                // value's resolved type.
                                slot: (**dom).clone(),
                                value: arg_ty.clone(),
                                inner: Box::new(at),
                            },
                        )
                    } else if let Type::Fun(p_dom, p_cod, _) = &**dom {
                        // A concrete callback flowing into a representation-
                        // polymorphic arrow parameter (e.g. `Int -> Int` into
                        // list_map's `a -> b`): the callee hands the callback a
                        // tagged word and stores a tagged word back, but the
                        // callback reads and writes raw scalars. Wrap it to untag
                        // its argument and tag its result (T6).
                        if arrow_needs_wrapper(p_dom, p_cod, &arg_ty) {
                            wrap_callback(&**dom, &at, stage)
                        } else {
                            at
                        }
                    } else {
                        at
                    };
                    let app = Typed::at(
                        ty,
                        row,
                        stage,
                        Node::App {
                            fun: Box::new(ft),
                            arg: Box::new(at),
                        },
                    );
                    // App-RESULT coercion (the dual of the arg `Tag`/`ToPtr`): when
                    // the callee returns a bare `Var` pinned to a concrete type at
                    // this call, its result is a tagged word / interior pointer — untag
                    // it (scalar) / intern it (handle). A concrete or still-generic
                    // codomain coerces `None` and rides verbatim.
                    Ok(coerce_app_result(&cod_lit, app, stage))
                }
                // The callee's type is not (yet) a concrete arrow — e.g. a bare
                // parameter `f` in `fn f => f 1`, or any inferred function.
                // **Unify it against a fresh arrow** `dom -> cod ! latent` to
                // discover the shape, then unify the argument against `dom`. This
                // is the general HM application rule.
                _ => {
                    // Fresh arrow skeleton. The latent row is an *open* tail so
                    // it can absorb whatever effect row the callee carries.
                    let (dom, cod, latent) = unify::with_store(|s| {
                        (
                            s.fresh_ty(),
                            s.fresh_ty(),
                            Row::open(BTreeSet::new(), s.fresh_row()),
                        )
                    });
                    let arrow =
                        Type::Fun(Box::new(dom.clone()), Box::new(cod.clone()), latent.clone());
                    // `ft.ty ~ (dom -> cod ! latent)`: a non-arrow callee (Int, …)
                    // fails here → the same `NotAFunction` the checker gave. A
                    // `WideTypeVar` cannot arise here (the arrow skeleton binds
                    // `ft.ty`'s var to a `Fun`, never a wide type), but route it
                    // faithfully rather than mislabel it `NotAFunction`.
                    unify::with_store(|s| unify(s, &ft.ty, &arrow)).map_err(|e| match e {
                        UnifyErr::WideTypeVar(_, ty) => TypeErr::WideTypeVariable { ty },
                        _ => TypeErr::NotAFunction(ft.ty.clone()),
                    })?;
                    // `arg.ty ~ dom`: an argument of the wrong type fails here →
                    // a `Mismatch`, exactly like a concrete-arrow application.
                    demand_eq(&dom, &at.ty)?;
                    let row = ft.row.union(&at.row).union(&latent);
                    Ok(Typed::at(
                        cod,
                        row,
                        stage,
                        Node::App {
                            fun: Box::new(ft),
                            arg: Box::new(at),
                        },
                    ))
                }
            }
        }

        // (let / bind) — rows union; the **value restriction** (D4) decides
        // whether the binding generalizes.
        //
        // Elaborate the RHS one **level deeper** (`enter_level`/`leave_level`):
        // any type/row variable born inside `e1` is then younger than this
        // `let`'s level, so `generalize` (the `level > current_level` test) picks
        // exactly those up and nothing from the enclosing scope. **Only if `e1`
        // is a syntactic value** do we generalize it into a `Poly` scheme;
        // otherwise it stays `Mono` (a non-value — `f y`, `perform …` — must not
        // be polymorphic, the soundness lynchpin). The result row unions `e1`'s
        // row **unconditionally** — even a generalized value's `gc` stays in the
        // manifest (D4 carve-out).
        Term::Let(x, e1, e2) => {
            unify::with_store(|s| s.enter_level());
            let t1 = elaborate_inner(sig, ctx, stage, e1);
            unify::with_store(|s| s.leave_level());
            let t1 = t1?;

            let binding = if is_value(e1) {
                Binding::Poly(generalize_resolved(&t1.ty))
            } else {
                Binding::Mono(t1.ty.clone())
            };
            // **Traits v1 Sprint 3 — mark a constrained generic definition.** If the
            // binding's scheme carries constraints (`min2 : Ord a => …`), tag the
            // `Node::Let` binder with the **traits it is constrained by**, in scheme
            // order, so the dictionary pass wraps `bound` in one leading dictionary
            // parameter per constraint. A trait *method* (also a constrained `Poly`)
            // is never `let`-bound here — its binder is minted, not user-written —
            // so this only ever fires for a user generic.
            let bind_name = match &binding {
                Binding::Poly(scheme) if !scheme.constraints.is_empty() => {
                    let traits: Vec<String> = scheme
                        .constraints
                        .iter()
                        .map(|c| c.trait_name.clone())
                        .collect();
                    record_constrained_def(x, &traits)
                }
                _ => x.clone(),
            };
            let mut ctx2 = ctx.clone();
            ctx2.insert(x.clone(), (binding, stage));
            let t2 = elaborate_inner(sig, &ctx2, stage, e2)?;
            let row = t1.row.union(&t2.row);
            let ty = t2.ty.clone();
            Ok(Typed::at(
                ty,
                row,
                stage,
                Node::Let {
                    name: bind_name,
                    bound: Box::new(t1),
                    body: Box::new(t2),
                },
            ))
        }

        // (let rec) — `f : T` is in scope in BOTH its own definition and the
        // body; the annotation `T` fixes the type before the body is checked.
        // Produces a plain `Node::Let` — the recursion is *structural* (the
        // bound lambda's body references `f`), which later phases pick up.
        Term::LetRec(f, ty, e1, e2) => {
            // GENERIC `let rec` (S3.5, D16). The annotation's free lowercase names
            // are TYPE VARIABLES (D15), made fresh at a **deeper level** so they
            // generalise. `f` is bound MONOMORPHICALLY in its own definition
            // (monomorphic recursion — decidable; the recursive calls share one
            // instantiation), and the annotation fixes its type before the body is
            // checked. After the body, `f`'s type is GENERALISED for `e2` — sound
            // because a recursive binding is a value (a lambda), so the value
            // restriction (D4) holds. A monomorphic annotation (no lowercase names)
            // generalises to a trivial scheme, reproducing today's behaviour.
            unify::with_store(|s| s.enter_level());
            let ty = unify::with_store(|s| instantiate_annotation(s, ty));
            // A nominal in the annotation must be named with the right arity (D14).
            let valid = validate_named_arity(&ty);
            let mut ctxr = ctx.clone();
            ctxr.insert(f.clone(), (Binding::Mono(ty.clone()), stage));
            let t1 = valid.and_then(|()| elaborate_inner(sig, &ctxr, stage, e1));
            unify::with_store(|s| s.leave_level()); // balanced on the error path
            let t1 = t1?;
            // The body's inferred type must match the annotation — a demand.
            demand_eq(&ty, &t1.ty)?;
            // Generalise the (now solved) annotation: its deeper-level vars become
            // the scheme's `∀`, and `e2` instantiates `f` fresh at each use.
            let scheme = unify::with_store(|s| generalize(s, &ty));
            // **Traits v1 lowering gate (`RN-E0246`).** A `let rec` whose generalized
            // scheme carries trait `constraints` is a *recursive constrained generic*.
            // Its self-calls inside `e1` were checked under the **monomorphic** `f`
            // (so they carry no obligation and are not dictionary-plumbed), but the
            // dict-passing transform wraps the *definition* in a hidden leading dict
            // parameter (`fn $dict$C => …`). The self-call `f x` would then apply the
            // dict-expecting `f` to `x` as if `x` were the dictionary — a miscompile
            // (it surfaces as a downstream tag/compiler-bug panic, never a clean
            // diagnostic). v1 cannot lower this, so reject it loud and clear at the
            // earliest clean point (here, where `Result` is still available) rather
            // than letting it reach `dict_pass`. (`Term::Let`, the non-recursive
            // case, threads the dictionary correctly and is unaffected.)
            if !scheme.constraints.is_empty() {
                let constraints: Vec<String> =
                    scheme.constraints.iter().map(|c| c.to_string()).collect();
                return Err(TypeErr::TraitV1Unsupported {
                    what: format!(
                        "a recursive (`let rec`) function `{f}` with a trait constraint \
                         ({}); make it non-recursive (a plain `let`), or monomorphize it \
                         (annotate `{f}` at a concrete type so no constraint is generalized)",
                        constraints.join(", ")
                    ),
                });
            }
            let bind_name = f.clone();
            let mut ctx2 = ctx.clone();
            ctx2.insert(f.clone(), (Binding::Poly(scheme), stage));
            let t2 = elaborate_inner(sig, &ctx2, stage, e2)?;
            let row = t1.row.union(&t2.row);
            let rty = t2.ty.clone();
            Ok(Typed::at(
                rty,
                row,
                stage,
                Node::Let {
                    name: bind_name,
                    bound: Box::new(t1),
                    body: Box::new(t2),
                },
            ))
        }

        // (let mut) — a non-escaping scalar mutable local (mutability v1,
        // `docs/mutability.md` §3; `mutability-sprints.md` Sprint 2). Elaborate the
        // initializer, REQUIRE its type be a scalar (`Int`/`Float`/`Bool` — v1 is
        // scalar-only, no heap cell/GC), bind `x : τ` **mutable**, elaborate the
        // body, then run the no-escape check: the mutable cell may not escape its
        // scope. The result type/row are the body's; the binding is **never**
        // generalized (a mutable cell is not a value — the same restriction the
        // value restriction enforces, here total).
        Term::LetMut(x, e1, e2) => {
            let t1 = elaborate_inner(sig, ctx, stage, e1)?;
            // v1 scalar gate (`RN-E0244`). Resolve far enough to classify; an
            // unsolved `Var` (D6 defaults it to `Int`, a scalar) is accepted.
            let scalar_ty = resolved_type(&t1.ty);
            if !is_mut_scalar(&scalar_ty) {
                return Err(TypeErr::MutNonScalar { ty: scalar_ty });
            }
            let mut ctx2 = ctx.clone();
            ctx2.insert(x.clone(), (Binding::Mut(t1.ty.clone()), stage));
            let t2 = elaborate_inner(sig, &ctx2, stage, e2)?;
            // No-escape check (`RN-E0241`): the mutable cell must not leave its
            // scope through the body's result. `let mut` is sugar for a *sealed*,
            // non-escaping `Ref`, so this reuses the `seal`/`runST` escape spirit
            // (`seal_escape` / `type_mentions_label`). For a *scalar* v1 cell a
            // value cannot carry the cell, so the check is satisfied by
            // construction — but it is wired here so the diagnostic exists and the
            // machinery is in place for the Sprint-3 `Ref` reuse. (See the report
            // note: no violating case is expressible in scalar v1.)
            if let Some(escaping) = mut_escape(x, &t2.ty) {
                return Err(TypeErr::MutEscapes { ty: escaping });
            }
            // Structural no-escape gate (`RN-E0241`, the closure-capture route the
            // type-based check above cannot see). v1 lowers the cell to a stack
            // slot, so a closure that captures `x` could carry it out of this scope
            // and read a dangling slot; the escaping closure's type names no cell,
            // so a type-based check misses it. v1 has no `st[T]`/`Ref[T]` to track
            // the cell's lifetime (the future feature, `docs/mutability.md` §8), so
            // v1 conservatively rejects *any* closure capture of a mutable local.
            // (Straight-line imperative use and direct mutation are unaffected.)
            if captured_in_closure(x, &t2.node) {
                return Err(TypeErr::MutEscapes { ty: t2.ty.clone() });
            }
            let row = t1.row.union(&t2.row);
            let ty = t2.ty.clone();
            Ok(Typed::at(
                ty,
                row,
                stage,
                Node::LetMut {
                    name: x.clone(),
                    bound: Box::new(t1),
                    body: Box::new(t2),
                },
            ))
        }

        // (:=) — assign through a bare name `x` (`docs/mutability.md` §1/§3). The
        // surface `x := e` is ONE form; sema splits it by `x`'s binding kind — the
        // clean disambiguation between the `let mut` slot store and the heap-`Ref`
        // write (the two are NOT separate surface syntaxes):
        //   • a `let mut` cell (`Binding::Mut τ`) → `Node::Assign` (a stack store;
        //     `e : τ`, no effect label — the row is just `e`'s);
        //   • a `Ref[T]`-typed name (any `Mono`/`Poly` binding whose type resolves
        //     to `Ref[T]`) → `Node::RefAssign` (the heap-cell write `(:=) : Ref[T]
        //     -> T -> Unit ! {st}`; `e : T`, the row gains `{st}`);
        //   • anything else (immutable non-`Ref`, or unbound) → `RN-E0245`.
        Term::Assign(x, e) => {
            match ctx.get(x) {
                // A `let mut` scalar cell — the stack-slot store (unchanged path).
                Some((Binding::Mut(t), _)) => {
                    let bound_ty = t.clone();
                    let te = elaborate_inner(sig, ctx, stage, e)?;
                    demand_eq(&bound_ty, &te.ty)?;
                    let row = te.row.clone();
                    Ok(Typed::at(
                        Type::Unit,
                        row,
                        stage,
                        Node::Assign {
                            name: x.clone(),
                            value: Box::new(te),
                        },
                    ))
                }
                // A `Ref[T]`-typed name — the heap-cell write. Reuse the general
                // ref-assign elaboration with the bare `Var x` as the target.
                Some((binding, bind_stage)) => {
                    let xty = match binding {
                        Binding::Mono(t) => t.clone(),
                        // A `Ref` is not a syntactic value, so the value restriction
                        // keeps it `Mono`; a `Poly` Ref-binding is unreachable in
                        // practice, but instantiate it correctly for robustness.
                        Binding::Poly(scheme) => unify::with_store(|s| instantiate(s, scheme)),
                        Binding::Mut(_) => unreachable!("Mut handled above"),
                    };
                    // Route to the heap-write when the name is a `Ref[T]` — OR when
                    // its type is still an unsolved `Var` (an unannotated parameter
                    // `fn s => s := …`): `:=` on a non-`let-mut` binding can ONLY be
                    // a ref-assign (the sole other assignable form), so pinning the
                    // param to `Ref[T]` via `elaborate_ref_assign`'s unify is sound.
                    // A *concretely* non-`Ref` binding (`let x = 0 in x := 1`) is
                    // genuinely not assignable → `RN-E0245`.
                    let resolved = resolved_type(&xty);
                    if as_ref_content(&resolved).is_some() || matches!(resolved, Type::Var(_)) {
                        let target = Typed::at(xty, Row::pure(), *bind_stage, Node::Var(x.clone()));
                        elaborate_ref_assign(sig, ctx, stage, target, e)
                    } else {
                        // Bound, but not a `Ref` and not a `let mut` — not assignable.
                        Err(TypeErr::MutAssignImmutable { name: x.clone() })
                    }
                }
                None => Err(TypeErr::MutAssignImmutable { name: x.clone() }),
            }
        }

        // (ref) — allocate a fresh `Ref[T]` heap cell holding `e`
        // (`docs/mutability.md` §1): `ref : T -> Ref[T] ! {gc}`. Elaborate `e`,
        // REQUIRE its content type be a **scalar** `Ref` cell
        // (`Int`/`Float`/`Bool`/`Unit` — Sprint 1; a pointer-typed `Ref` is the
        // clean deferred `RN-E0247`), then build a `Ref[T]` whose row is `e`'s plus
        // `{gc}` (the allocation). The cell is a one-field heap object; lowering
        // reuses the tuple/scalar-cell machinery (one `set_scalar`).
        Term::RefNew(e) => {
            let te = elaborate_inner(sig, ctx, stage, e)?;
            let content = resolved_type(&te.ty);
            if ref_content_is_pointer_typed(&content) {
                return Err(TypeErr::RefPointerContent { ty: content });
            }
            let row = te.row.union(&Row::single(Label::Gc));
            Ok(Typed::at(
                ref_type(te.ty.clone()),
                row,
                stage,
                Node::RefNew {
                    value: Box::new(te),
                },
            ))
        }

        // (!) — dereference (read) a `Ref[T]` cell (`docs/mutability.md` §1):
        // `(!_) : Ref[T] -> T ! {st}`. Elaborate `e`, recover its content type `T`,
        // reject a pointer-typed cell (`RN-E0247`), and yield `T` with `{st}` added
        // (observable mutation).
        Term::Deref(e) => {
            let te = elaborate_inner(sig, ctx, stage, e)?;
            let content = ref_content_or_pin(&te.ty)?;
            if ref_content_is_pointer_typed(&content) {
                return Err(TypeErr::RefPointerContent { ty: content });
            }
            let row = te.row.union(&Row::single(Label::St));
            Ok(Typed::at(
                content,
                row,
                stage,
                Node::Deref { cell: Box::new(te) },
            ))
        }

        // (perform) — adds the op's label; result type from its signature.
        Term::Perform(label, arg) => {
            let at = elaborate_inner(sig, ctx, stage, arg)?;
            let result = match sig.get(label) {
                Some(op) => {
                    // The argument must match the op's declared param — a demand.
                    demand_eq(&op.param, &at.ty)?;
                    op.result.clone()
                }
                None => Type::Unit,
            };
            let row = at.row.union(&Row::single(label.clone()));
            Ok(Typed::at(
                result,
                row,
                stage,
                Node::Perform {
                    label: label.clone(),
                    arg: Box::new(at),
                },
            ))
        }

        // (handle) — discharges the handled labels (§2.1 (op)).
        Term::Handle(scrutinee, h) => elaborate_handle(sig, ctx, stage, scrutinee, h),

        // (seal) — `seal L { e }` (sealing-solution.md §5). Remove `L` from the
        // outward row and record the no-escape obligation (checked post-zonk in
        // `check_seal_obligations`). The seal is **runtime-transparent**: we erase
        // it to the body's own node, keeping only the row removal — so no later
        // phase needs to know about `seal`. (Same shape as `effect …` erasing to
        // its body, but adjusting the row.)
        Term::Seal(label, body) => {
            let b = elaborate_inner(sig, ctx, stage, body)?;
            // D-S3: a seal may only *remove* a label that discharges at the
            // boundary. Native powers (`gc`, `World` syscalls) bottom out in a
            // runtime call; a non-native effect (`User`/`Exn`) still in the body's
            // row is unhandled and would escape at runtime, so sealing it would
            // hide a fault. (A handled user effect is already gone from the row,
            // so this permits the design's "local user-effect scoping" use.)
            if !label.is_native() && b.row.labels().any(|l| l == label) {
                return Err(TypeErr::SealUnhandled {
                    label: label.clone(),
                });
            }
            SEAL_OBLIGATIONS.with(|s| s.borrow_mut().push((label.clone(), b.ty.clone())));
            Ok(Typed {
                ty: b.ty,
                row: b.row.without(std::slice::from_ref(label)),
                stage: b.stage,
                layout_known: b.layout_known,
                node: b.node,
            })
        }

        // (quote) — δ at the boundary (§3.2/§3.3). Single-stage: generation
        // only. The body is checked one stage *down*; its row splits — object
        // effects stay inside the □, generative ones come out here.
        Term::Quote(body) => {
            if stage != 1 {
                return Err(TypeErr::StageMisuse {
                    what: "quote",
                    at: stage,
                });
            }
            let b = elaborate_inner(sig, ctx, stage - 1, body)?;
            let (object, generative) = b.row.partition();
            let ty = Type::Code(Box::new(b.ty.clone()), object);
            Ok(Typed::at(ty, generative, stage, Node::Quote(Box::new(b))))
        }

        // (splice) — embed code (§3.3). Single-stage: object stage only. Also
        // the default outermost locus: it discharges `Insert`.
        Term::Splice(c) => {
            if stage != 0 {
                return Err(TypeErr::StageMisuse {
                    what: "splice",
                    at: stage,
                });
            }
            let cc = elaborate_inner(sig, ctx, stage + 1, c)?;
            match &cc.ty {
                Type::Code(a, object) => {
                    let ty = (**a).clone();
                    let row = object.union(&cc.row.without(&[Label::Insert]));
                    Ok(Typed::at(ty, row, stage, Node::Splice(Box::new(cc))))
                }
                other => Err(TypeErr::NotCode(other.clone())),
            }
        }

        // genlet c ≡ perform Insert(c) (§4.1) — generation-stage, generative.
        Term::Genlet(c) => {
            if stage != 1 {
                return Err(TypeErr::StageMisuse {
                    what: "genlet",
                    at: stage,
                });
            }
            let cc = elaborate_inner(sig, ctx, stage, c)?;
            if matches!(cc.ty, Type::Code(_, _)) {
                let ty = cc.ty.clone();
                let row = cc.row.union(&Row::single(Label::Insert));
                Ok(Typed::at(ty, row, stage, Node::Genlet(Box::new(cc))))
            } else {
                Err(TypeErr::NotCode(cc.ty.clone()))
            }
        }

        // letloc { e } — an explicit locus: the handler for `Insert` (§4.1).
        Term::Letloc(body) => {
            if stage != 1 {
                return Err(TypeErr::StageMisuse {
                    what: "letloc",
                    at: stage,
                });
            }
            let b = elaborate_inner(sig, ctx, stage, body)?;
            let ty = b.ty.clone();
            let row = b.row.without(&[Label::Insert]);
            Ok(Typed::at(ty, row, stage, Node::Letloc(Box::new(b))))
        }

        // `effect name : Param -> Result in body` — extend `Σ` for `body`. The
        // declaration is type-level only; it erases (we return the body's tree).
        // (effect) — register each declared op's signature in `Σ` for the body,
        // then erase: a declaration is type-level only, producing no node. (The
        // effect `name` groups the ops; the ops are the perform-able labels.)
        Term::Effect { name: _, ops, body } => {
            let mut sig2 = sig.clone();
            for op in ops {
                sig2.insert(
                    Label::User(op.op.clone()),
                    OpSig {
                        param: op.param.clone(),
                        result: op.result.clone(),
                    },
                );
            }
            elaborate_inner(&sig2, ctx, stage, body)
        }

        // (trait) — declare a single-parameter trait (D6, `trait-resolution.md`
        // §1.1), in scope for `body`. **Nominal and registered**, like `TypeDef`:
        // register the trait, **mint each method as a generic function** whose
        // scheme carries the `Trait a` constraint (so a use of the method
        // instantiates and emits the obligation `Trait ?a`), bind those methods
        // into `ctx`, and elaborate `body`. No runtime node — passthrough.
        Term::Trait {
            name,
            param,
            supers,
            methods,
            module,
            body,
        } => {
            // Register the trait (the declared method sigs, before constraint
            // minting) so the body — and resolution — can find it.
            let info = TraitInfo {
                param: param.clone(),
                supers: supers.clone(),
                methods: methods
                    .iter()
                    .map(|m| (m.name.clone(), m.sig.clone()))
                    .collect(),
                module: module.clone(),
            };
            // Persist program-globally (for the post-elaboration dictionary
            // transform, after the scoped `TRAITENV` is torn down).
            TRAIT_DEFS.with(|t| t.borrow_mut().insert(name.clone(), info.clone()));
            let saved = TRAITENV.with(|t| {
                let mut env = t.borrow_mut();
                let saved = env.clone();
                env.traits.insert(name.clone(), info);
                saved
            });
            // Mint each method as `∀a ᾱ. Trait a => sig`: the trait param `a`
            // becomes one fresh quantified var, every *other* free lowercase name
            // in the sig becomes its own fresh quantified var, and the scheme
            // carries the constraint `Trait a`. A use of the method (a `Var`)
            // instantiates this scheme — re-emitting `Trait ?a` as an obligation.
            let mut ctx2 = ctx.clone();
            for m in methods {
                let scheme = unify::with_store(|s| mint_method_scheme(s, name, param, &m.sig));
                ctx2.insert(m.name.clone(), (Binding::Poly(scheme), stage));
            }
            let result = elaborate_inner(sig, &ctx2, stage, body);
            TRAITENV.with(|t| *t.borrow_mut() = saved);
            result
        }

        // (instance) — declare an instance of `trait_name` at `head`
        // (`trait-resolution.md` §1.1), in scope for `body`. The **§3.5
        // well-formedness checks** run *before* registration: coherence/duplicate
        // (R3 → `RN-E0237`), overlap (R4 → `RN-E0231`), termination/Paterson (R6 →
        // `RN-E0233`), and orphan (R5 → `RN-E0232`, when modules are tracked). Then
        // register `(trait, head)` → method impls and light-check each method body
        // against the trait's signature instantiated at `head` (`RN-E0239`/`RN-E0238`).
        // Finally elaborate `body` (passthrough).
        Term::Instance {
            trait_name,
            head,
            requires,
            methods,
            module,
            body,
        } => {
            // Coherence (R3) / overlap (R4) / duplicate (R7-degenerate) against the
            // program-global instance set, then Paterson termination (R6) and orphan
            // (R5). Static, declaration-time checks (run before this instance is
            // itself registered, so it cannot collide with itself).
            check_instance_coherence(trait_name, head)?;
            check_instance_termination(trait_name, head, requires)?;
            check_instance_orphan(trait_name, head, module.as_deref())?;
            // Capture the trait's param + superclasses *now* (the scoped `TRAITENV`
            // still has the trait) so the global instance set carries everything
            // resolution needs after scope teardown.
            let (trait_param, trait_supers) = TRAITENV.with(|t| {
                t.borrow()
                    .traits
                    .get(trait_name)
                    .map(|i| (i.param.clone(), i.supers.clone()))
                    .unwrap_or_default()
            });
            // Light-check (R0/RN-E0238/RN-E0239) **and** capture the elaborated
            // method bodies — the dictionary-literal field closures Sprint 3 lowers
            // (`object-system-design.md` §4). The check elaborates each body against
            // the trait method signature at `head`; the dictionary transform reuses
            // those same elaborations as the method closures.
            let method_bodies = check_instance(sig, ctx, stage, trait_name, head, methods)?;
            // Register **program-globally** (not torn down at scope exit): coherence
            // is global, and the end-of-elaboration resolution reads this set after
            // the scoped `TRAITENV` is gone.
            INSTANCES.with(|i| {
                i.borrow_mut().push(InstanceInfo {
                    trait_name: trait_name.clone(),
                    head: head.clone(),
                    requires: requires.clone(),
                    trait_param,
                    trait_supers,
                    methods: methods.iter().map(|m| m.name.clone()).collect(),
                    method_bodies,
                    module: module.clone(),
                })
            });
            elaborate_inner(sig, ctx, stage, body)
        }
    }
}

/// **Mint a trait method's generic function scheme** `∀a ᾱ. Trait a => sig`
/// (traits v1, `trait-resolution.md` §1.1). The declared `sig` is the type
/// written after the method's `:` (e.g. `a -> String`); the trait's own
/// `Trait a` constraint is *added here*, not written by the author. The trait
/// parameter name `param` maps to one fresh quantified type var; every **other**
/// free lowercase name in `sig` (a method that is itself polymorphic) maps to its
/// own fresh quantified var. The scheme quantifies all of them, carries the one
/// constraint `Trait param`, and its body is the rewritten `sig`. A use of the
/// method instantiates this scheme (`unify::instantiate`), which re-emits
/// `Trait ?a` as a pending obligation under the fresh renaming.
fn mint_method_scheme(
    store: &mut unify::UnifStore,
    trait_name: &str,
    param: &str,
    sig: &Type,
) -> Scheme {
    // Every free lowercase name in the sig is a quantified var; the trait param is
    // one of them (added even if the sig does not mention it, so `Trait a` is
    // always well-formed). First-occurrence order, param first.
    let mut names = vec![param.to_string()];
    collect_annotation_vars(sig, &mut names);
    let subst: HashMap<String, Type> = names
        .iter()
        .map(|n| (n.clone(), store.fresh_ty()))
        .collect();
    // Row vars in the sig become fresh too (so a method's latent-row variable is
    // quantified) — reuse the annotation row-var rewrite.
    let mut row_names = Vec::new();
    collect_annotation_row_vars(sig, &mut row_names);
    let row_subst: HashMap<RowVarId, RowVarId> = row_names
        .into_iter()
        .map(|id| (id, store.fresh_row()))
        .collect();
    let body = rewrite_annotation_vars(sig, &subst, &row_subst);
    let ty_vars: Vec<TyVarId> = names
        .iter()
        .map(|n| match &subst[n] {
            Type::Var(id) => *id,
            _ => unreachable!("fresh_ty yields a Var"),
        })
        .collect();
    let row_vars: Vec<RowVarId> = row_subst.values().copied().collect();
    let param_ty = subst[param].clone();
    Scheme {
        ty_vars,
        row_vars,
        constraints: vec![crate::syntax::Constraint {
            trait_name: trait_name.to_string(),
            ty: param_ty,
        }],
        ty: body,
    }
}

/// **Light Sprint-1 instance well-formedness** (`trait-resolution.md` §1.1). The
/// trait must be in scope; the instance must implement **exactly** the trait's
/// methods (`RN-E0239` for a missing one or an unknown extra); each method body is
/// elaborated and unified against the trait's declared method signature
/// instantiated at the instance head (`RN-E0238` on a clash). Coherence / overlap
/// / orphan / termination are **out of scope for Sprint 1** (Sprint 2). A trait
/// that is not in scope is `RN-E0235 trait.no-method` for its first method.
fn check_instance(
    sig: &Sig,
    ctx: &Ctx,
    stage: Stage,
    trait_name: &str,
    head: &Type,
    methods: &[crate::syntax::InstanceMethod],
) -> Result<Vec<(String, Typed, Type)>, TypeErr> {
    let info = TRAITENV.with(|t| t.borrow().traits.get(trait_name).cloned());
    let Some(info) = info else {
        // No such trait in scope — report against the first method name (or the
        // trait name itself if the instance is empty).
        let method = methods
            .first()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| trait_name.to_string());
        return Err(TypeErr::TraitNoMethod { method });
    };
    // RN-E0239: every declared method must be implemented, and no extra.
    for (decl_name, _) in &info.methods {
        if !methods.iter().any(|m| &m.name == decl_name) {
            return Err(TypeErr::TraitMissingMethod {
                trait_name: trait_name.to_string(),
                method: decl_name.clone(),
                missing: true,
            });
        }
    }
    for m in methods {
        if !info.methods.iter().any(|(n, _)| n == &m.name) {
            return Err(TypeErr::TraitMissingMethod {
                trait_name: trait_name.to_string(),
                method: m.name.clone(),
                missing: false,
            });
        }
    }
    // RN-E0238 (light): elaborate each body and unify against the declared method
    // signature with the trait parameter set to the instance head. A clash is the
    // method-row-violation (the latent-row check rides the same unification). The
    // elaborated bodies are **kept** (the dictionary-literal field closures Sprint 3
    // lowers — `object-system-design.md` §4).
    let mut method_bodies: Vec<(String, Typed, Type)> = Vec::with_capacity(methods.len());
    let head = unify::with_store(|s| instantiate_annotation(s, head));
    validate_named_arity(&head)?;
    for m in methods {
        let (_, decl_sig) = info
            .methods
            .iter()
            .find(|(n, _)| n == &m.name)
            .expect("method presence checked above");
        // The method's *other* free lowercase names become fresh vars so the body
        // can be (independently) polymorphic in them — shared by both the concrete
        // (`expected`) and the uniform (`uniform_arrow`) substitutions below.
        let other_names: Vec<(String, Type)> = {
            let mut names = Vec::new();
            collect_annotation_vars(decl_sig, &mut names);
            names.retain(|n| n != &info.param);
            names
                .into_iter()
                .map(|n| (n.clone(), unify::with_store(|s| s.fresh_ty())))
                .collect()
        };
        // The declared method type at the head: substitute the trait param name
        // for the instance head throughout the signature.
        let subst: HashMap<String, Type> = std::iter::once((info.param.clone(), head.clone()))
            .chain(other_names.iter().cloned())
            .collect();
        // The **uniform ABI** arrow (traits v1 Sprint 3): the same declared
        // signature but with the trait parameter kept as a **fresh, never-unified
        // `Var`** — its `repr` stays `Uniform`, marking exactly the positions a
        // generic caller tags. `wrap_callback` reads this in `build_dict` to
        // untag/re-tag the concrete instance body to the uniform interface.
        let uniform_param = unify::with_store(|s| s.fresh_ty());
        let uniform_subst: HashMap<String, Type> =
            std::iter::once((info.param.clone(), uniform_param))
                .chain(other_names.iter().cloned())
                .collect();
        let mut row_names = Vec::new();
        collect_annotation_row_vars(decl_sig, &mut row_names);
        let row_subst: HashMap<RowVarId, RowVarId> = row_names
            .into_iter()
            .map(|id| (id, unify::with_store(|s| s.fresh_row())))
            .collect();
        let expected = rewrite_annotation_vars(decl_sig, &subst, &row_subst);
        let uniform_arrow = rewrite_annotation_vars(decl_sig, &uniform_subst, &row_subst);
        let actual = elaborate_inner(sig, ctx, stage, &m.body)?;
        if unify::with_store(|s| unify(s, &expected, &actual.ty)).is_err() {
            let expected = unify::with_store(|s| unify::zonk_ty(s, &expected));
            let found = unify::with_store(|s| unify::zonk_ty(s, &actual.ty));
            return Err(TypeErr::TraitMethodRowViolation {
                trait_name: trait_name.to_string(),
                method: m.name.clone(),
                expected,
                found,
            });
        }
        method_bodies.push((m.name.clone(), actual, uniform_arrow));
    }
    Ok(method_bodies)
}

/// `handle e with H` (§2.1 (op)) — discharges the handled labels; the
/// `return` clause fixes `R`; each op clause produces `R` with
/// `resume : op.result -> R` in scope.
fn elaborate_handle(
    sig: &Sig,
    ctx: &Ctx,
    stage: Stage,
    scrutinee: &Term,
    h: &Handler,
) -> Result<Typed, TypeErr> {
    let scrut = elaborate_inner(sig, ctx, stage, scrutinee)?;

    let mut ctx_ret = ctx.clone();
    // The `return(var)` binder takes the handled value's type, monomorphically.
    ctx_ret.insert(h.ret.var.clone(), (Binding::Mono(scrut.ty.clone()), stage));
    let ret_body = elaborate_inner(sig, &ctx_ret, stage, &h.ret.body)?;
    let r = ret_body.ty.clone();

    let handled: Vec<Label> = h.ops.iter().map(|c| c.op.clone()).collect();
    let mut row = scrut.row.without(&handled).union(&ret_body.row);
    let ret_layout = storage_layout(&scrut.ty);
    let mut handler_layout_known = ret_layout.known;

    let mut ops = Vec::with_capacity(h.ops.len());
    for c in &h.ops {
        let (param, result) = match sig.get(&c.op) {
            Some(op) => (op.param.clone(), op.result.clone()),
            None => (Type::Unit, Type::Unit),
        };
        let arg_layout = storage_layout(&param);
        handler_layout_known &= arg_layout.known;
        let resume_ty = Type::Fun(Box::new(result.clone()), Box::new(r.clone()), Row::pure());
        let resume_layout = storage_layout(&resume_ty);
        handler_layout_known &= resume_layout.known;
        let mut ctx_op = ctx.clone();
        // The op's argument and its `resume` continuation are monomorphic binders.
        ctx_op.insert(c.arg.clone(), (Binding::Mono(param.clone()), stage));
        ctx_op.insert(c.resume.clone(), (Binding::Mono(resume_ty.clone()), stage));
        let body = elaborate_inner(sig, &ctx_op, stage, &c.body)?;
        // Each op clause produces the handler's result type `R` — a demand.
        demand_eq(&r, &body.ty)?;
        row = row.union(&body.row);
        ops.push(TypedOpClause {
            op: c.op.clone(),
            arg: c.arg.clone(),
            arg_ty: param,
            arg_layout,
            resume: c.resume.clone(),
            resume_ty,
            resume_layout,
            body: Box::new(body),
        });
    }

    let handler = TypedHandler {
        ops,
        ret: TypedReturn {
            var: h.ret.var.clone(),
            var_ty: scrut.ty.clone(),
            var_layout: ret_layout,
            body_ty: r.clone(),
            body: Box::new(ret_body),
        },
    };
    Ok(Typed::at_layout(
        r,
        row,
        stage,
        handler_layout_known,
        Node::Handle {
            scrutinee: Box::new(scrut),
            handler,
        },
    ))
}

// ── Rendering: the model, made observable ───────────────────────────────

impl Typed {
    /// The node's own one-word head (no annotation).
    fn head(&self) -> String {
        match &self.node {
            Node::Var(x) => format!("var {x}"),
            Node::Int(n) => format!("int {n}"),
            Node::Float(bits) => format!("float {}", f64::from_bits(*bits)),
            Node::Bool(b) => format!("bool {b}"),
            Node::Unit => "unit".into(),
            Node::Brk => "brk".into(),
            Node::Str(s) => format!("str {s:?}"),
            Node::Extern(sym, _) => format!("extern {sym:?}"),
            Node::Bin(op, ..) => format!("bin {}", op.symbol()),
            Node::Cast(op, _) => format!("cast {}", op.symbol()),
            Node::Coerce { kind, .. } => format!("coerce {kind:?}"),
            Node::FloatMathUnary(op, _) => op.symbol().into(),
            Node::FloatMathBinary(op, ..) => op.symbol().into(),
            Node::FloatMathTernary(op, ..) => op.symbol().into(),
            Node::MaskReduce(op, _) => op.symbol().into(),
            Node::VectorSelect { .. } => "select".into(),
            Node::VectorLit { shape, .. } => format!("vec {}", shape.name()),
            Node::VectorSplat { shape, .. } => format!("splat {}", shape.name()),
            Node::VectorLoad { shape, .. } => format!("load {}", shape.name()),
            Node::VectorStore { shape, .. } => format!("store {}", shape.name()),
            Node::VectorExtract { lane, .. } => format!("lane {lane}"),
            Node::If(..) => "if".into(),
            Node::Loop { vars, .. } => {
                let names = vars
                    .iter()
                    .map(|(name, _, _, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("loop {names}")
            }
            Node::Lam {
                param, param_ty, ..
            } => format!("lam {param}:{param_ty}"),
            Node::App { .. } => "app".into(),
            Node::Let { name, .. } => format!("let {name}"),
            Node::Block { items, .. } => format!("block/{}", items.len()),
            Node::LetMut { name, .. } => format!("let mut {name}"),
            Node::Assign { name, .. } => format!("{name} :="),
            Node::RefNew { .. } => "ref".into(),
            Node::Deref { .. } => "!".into(),
            Node::RefAssign { .. } => ":=".into(),
            Node::Perform { label, .. } => format!("perform {label}"),
            Node::Handle { .. } => "handle".into(),
            Node::Quote(_) => "quote".into(),
            Node::Splice(_) => "splice".into(),
            Node::Genlet(_) => "genlet".into(),
            Node::Letloc(_) => "letloc".into(),
            Node::Peek(w, _) => format!("peek{}", w.bits()),
            Node::Poke(w, ..) => format!("poke{}", w.bits()),
            Node::Fill(..) => "fill".into(),
            Node::Copy(..) => "copy".into(),
            Node::Index(w, ..) => format!("index/{}", w.bits()),
            Node::IndexSet(w, ..) => format!("index<- /{}", w.bits()),
            Node::Tuple(es) => format!("tuple/{}", es.len()),
            Node::LetTuple(names, ..) => format!("let ({})", names.join(", ")),
            Node::Record(fs) => {
                format!(
                    "record {{{}}}",
                    fs.iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Node::Field(_, name) => format!("field .{name}"),
            Node::ArrayLit { elems, .. } => format!("array/{}", elems.len()),
            Node::Len(_) => "len".to_string(),
            Node::ArrayGet { .. } => "a[i]".to_string(),
            Node::ArraySet { .. } => "a[i]<-".to_string(),
            Node::Construct { tag, args } => format!("construct#{tag}/{}", args.len()),
            Node::Match { arms, .. } => format!("match/{}", arms.len()),
        }
    }

    /// A short JSON tag for the node kind.
    fn tag(&self) -> &'static str {
        match &self.node {
            Node::Var(_) => "var",
            Node::Int(_) => "int",
            Node::Float(_) => "float",
            Node::Bool(_) => "bool",
            Node::Unit => "unit",
            Node::Brk => "brk",
            Node::Str(_) => "str",
            Node::Extern(..) => "extern",
            Node::Bin(..) => "bin",
            Node::Cast(..) => "cast",
            Node::Coerce { .. } => "coerce",
            Node::FloatMathUnary(..) => "floatMathUnary",
            Node::FloatMathBinary(..) => "floatMathBinary",
            Node::FloatMathTernary(..) => "floatMathTernary",
            Node::MaskReduce(..) => "maskReduce",
            Node::VectorSelect { .. } => "vectorSelect",
            Node::VectorLit { .. } => "vectorLit",
            Node::VectorSplat { .. } => "vectorSplat",
            Node::VectorLoad { .. } => "vectorLoad",
            Node::VectorStore { .. } => "vectorStore",
            Node::VectorExtract { .. } => "vectorExtract",
            Node::If(..) => "if",
            Node::Loop { .. } => "loop",
            Node::Lam { .. } => "lam",
            Node::App { .. } => "app",
            Node::Let { .. } => "let",
            Node::Block { .. } => "block",
            Node::LetMut { .. } => "letMut",
            Node::Assign { .. } => "assign",
            Node::RefNew { .. } => "refNew",
            Node::Deref { .. } => "deref",
            Node::RefAssign { .. } => "refAssign",
            Node::Perform { .. } => "perform",
            Node::Handle { .. } => "handle",
            Node::Quote(_) => "quote",
            Node::Splice(_) => "splice",
            Node::Genlet(_) => "genlet",
            Node::Letloc(_) => "letloc",
            Node::Peek(..) => "peek",
            Node::Poke(..) => "poke",
            Node::Fill(..) => "fill",
            Node::Copy(..) => "copy",
            Node::Index(..) => "index",
            Node::IndexSet(..) => "indexSet",
            Node::Tuple(_) => "tuple",
            Node::LetTuple(..) => "letTuple",
            Node::Record(_) => "record",
            Node::Field(..) => "field",
            Node::ArrayLit { .. } => "arrayLit",
            Node::Len(_) => "len",
            Node::ArrayGet { .. } => "arrayGet",
            Node::ArraySet { .. } => "arraySet",
            Node::Construct { .. } => "construct",
            Node::Match { .. } => "match",
        }
    }

    /// An indented tree — every line `<head>  : <type> ! <row> @ <stage>`.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        self.write_text(&mut s, 0);
        s
    }

    fn write_text(&self, s: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        // Omit a pure row here: it is the common case (every value node), and
        // hiding `! {}` keeps function types — whose arrow already shows a
        // latent row — from reading as a confusing double `! .. ! {}`.
        let anno = if self.row.is_pure() {
            format!("{} @ {}", self.ty, self.stage)
        } else {
            format!("{} ! {} @ {}", self.ty, self.row, self.stage)
        };
        s.push_str(&format!("{pad}{}  : {anno}\n", self.head()));
        let inner = depth + 1;
        match &self.node {
            Node::Var(_)
            | Node::Int(_)
            | Node::Float(_)
            | Node::Bool(_)
            | Node::Unit
            | Node::Brk
            | Node::Str(_)
            | Node::Extern(..) => {}
            Node::Coerce { inner: child, .. } => child.write_text(s, inner),
            Node::Lam { body, .. } => body.write_text(s, inner),
            Node::Cast(_, a) => a.write_text(s, inner),
            Node::FloatMathUnary(_, a) => a.write_text(s, inner),
            Node::MaskReduce(_, a) => a.write_text(s, inner),
            Node::FloatMathBinary(_, a, b) => {
                a.write_text(s, inner);
                b.write_text(s, inner);
            }
            Node::FloatMathTernary(_, a, b, c) => {
                a.write_text(s, inner);
                b.write_text(s, inner);
                c.write_text(s, inner);
            }
            Node::VectorSplat { value, .. } => value.write_text(s, inner),
            Node::VectorLoad { arr, idx, .. } => {
                arr.write_text(s, inner);
                idx.write_text(s, inner);
            }
            Node::VectorStore {
                arr, idx, value, ..
            } => {
                arr.write_text(s, inner);
                idx.write_text(s, inner);
                value.write_text(s, inner);
            }
            Node::VectorExtract { vector, .. } => vector.write_text(s, inner),
            Node::VectorSelect {
                mask,
                then_value,
                else_value,
            } => {
                mask.write_text(s, inner);
                then_value.write_text(s, inner);
                else_value.write_text(s, inner);
            }
            Node::Bin(_, a, b) => {
                a.write_text(s, inner);
                b.write_text(s, inner);
            }
            Node::VectorLit { elems, .. } => {
                for e in elems {
                    e.write_text(s, inner);
                }
            }
            Node::If(c, t, e) => {
                c.write_text(s, inner);
                t.write_text(s, inner);
                e.write_text(s, inner);
            }
            Node::Loop {
                vars,
                cond,
                steps,
                result,
            } => {
                for (_, _, _, init) in vars {
                    init.write_text(s, inner);
                }
                cond.write_text(s, inner);
                for step in steps {
                    step.write_text(s, inner);
                }
                result.write_text(s, inner);
            }
            Node::App { fun, arg } => {
                fun.write_text(s, inner);
                arg.write_text(s, inner);
            }
            Node::Let { bound, body, .. } | Node::LetMut { bound, body, .. } => {
                bound.write_text(s, inner);
                body.write_text(s, inner);
            }
            Node::Block { items, body } => {
                for item in items {
                    item.bound().write_text(s, inner);
                }
                body.write_text(s, inner);
            }
            Node::Assign { value, .. } => value.write_text(s, inner),
            Node::RefNew { value, .. } => value.write_text(s, inner),
            Node::Deref { cell, .. } => cell.write_text(s, inner),
            Node::RefAssign { target, value, .. } => {
                target.write_text(s, inner);
                value.write_text(s, inner);
            }
            Node::Perform { arg, .. } => arg.write_text(s, inner),
            Node::Quote(b) | Node::Splice(b) | Node::Genlet(b) | Node::Letloc(b) => {
                b.write_text(s, inner)
            }
            Node::Peek(_, a) => a.write_text(s, inner),
            Node::Poke(_, a, b) => {
                a.write_text(s, inner);
                b.write_text(s, inner);
            }
            Node::Fill(a, b, c) | Node::Copy(a, b, c) => {
                a.write_text(s, inner);
                b.write_text(s, inner);
                c.write_text(s, inner);
            }
            Node::Index(_, a, b) => {
                a.write_text(s, inner);
                b.write_text(s, inner);
            }
            Node::IndexSet(_, a, b, c) => {
                a.write_text(s, inner);
                b.write_text(s, inner);
                c.write_text(s, inner);
            }
            Node::Tuple(es) => {
                for e in es {
                    e.write_text(s, inner);
                }
            }
            Node::LetTuple(_, e, body) => {
                e.write_text(s, inner);
                body.write_text(s, inner);
            }
            Node::Record(fs) => {
                for (_, t) in fs {
                    t.write_text(s, inner);
                }
            }
            Node::Field(r, _) => r.write_text(s, inner),
            Node::ArrayLit { elems, .. } => {
                for e in elems {
                    e.write_text(s, inner);
                }
            }
            Node::Len(a) => a.write_text(s, inner),
            Node::ArrayGet { arr, idx, .. } => {
                arr.write_text(s, inner);
                idx.write_text(s, inner);
            }
            Node::ArraySet { arr, idx, val, .. } => {
                arr.write_text(s, inner);
                idx.write_text(s, inner);
                val.write_text(s, inner);
            }
            Node::Construct { args, .. } => {
                for (a, _, _) in args {
                    a.write_text(s, inner);
                }
            }
            Node::Match { scrutinee, arms } => {
                scrutinee.write_text(s, inner);
                for arm in arms {
                    arm.body.write_text(s, inner);
                }
            }
            Node::Handle { scrutinee, handler } => {
                scrutinee.write_text(s, inner);
                let p = "  ".repeat(inner);
                for op in &handler.ops {
                    s.push_str(&format!(
                        "{p}op {}({}) resume {}\n",
                        op.op, op.arg, op.resume
                    ));
                    op.body.write_text(s, inner + 1);
                }
                s.push_str(&format!("{p}return {}\n", handler.ret.var));
                handler.ret.body.write_text(s, inner + 1);
            }
        }
    }

    /// Machine-readable tree (schema `locus-sema/1`).
    pub fn to_json(&self) -> String {
        format!(
            "{{\"schema\":\"locus-sema/1\",\"ok\":true,\"tree\":{}}}",
            self.json_node()
        )
    }

    fn json_node(&self) -> String {
        let row: Vec<String> = self
            .row
            .labels()
            .map(|l| format!("\"{}\"", esc(&l.to_string())))
            .collect();
        let mut f = format!(
            "\"node\":\"{}\",\"type\":\"{}\",\"row\":[{}],\"stage\":{}",
            self.tag(),
            esc(&self.ty.to_string()),
            row.join(","),
            self.stage
        );
        match &self.node {
            Node::Var(x) => f += &format!(",\"name\":\"{}\"", esc(x)),
            Node::Int(n) => f += &format!(",\"value\":{n}"),
            Node::Float(bits) => {
                f += &format!(",\"bits\":{},\"value\":{}", bits, f64::from_bits(*bits))
            }
            Node::Bool(b) => f += &format!(",\"value\":{b}"),
            Node::Str(s) => f += &format!(",\"value\":\"{}\"", esc(s)),
            Node::Extern(sym, _) => f += &format!(",\"symbol\":\"{}\"", esc(sym)),
            Node::Bin(op, a, b) => {
                f += &format!(
                    ",\"op\":\"{}\",\"children\":[{},{}]",
                    op.symbol(),
                    a.json_node(),
                    b.json_node()
                );
            }
            Node::Cast(op, a) => {
                f += &format!(
                    ",\"op\":\"{}\",\"children\":[{}]",
                    op.symbol(),
                    a.json_node()
                );
            }
            Node::Coerce { kind, inner, .. } => {
                f += &format!(
                    ",\"kind\":\"{kind:?}\",\"children\":[{}]",
                    inner.json_node()
                );
            }
            Node::FloatMathUnary(op, a) => {
                f += &format!(
                    ",\"op\":\"{}\",\"children\":[{}]",
                    op.symbol(),
                    a.json_node()
                );
            }
            Node::FloatMathBinary(op, a, b) => {
                f += &format!(
                    ",\"op\":\"{}\",\"children\":[{},{}]",
                    op.symbol(),
                    a.json_node(),
                    b.json_node()
                );
            }
            Node::FloatMathTernary(op, a, b, c) => {
                f += &format!(
                    ",\"op\":\"{}\",\"children\":[{},{},{}]",
                    op.symbol(),
                    a.json_node(),
                    b.json_node(),
                    c.json_node()
                );
            }
            Node::MaskReduce(op, a) => {
                f += &format!(
                    ",\"op\":\"{}\",\"children\":[{}]",
                    op.symbol(),
                    a.json_node()
                );
            }
            Node::VectorSelect {
                mask,
                then_value,
                else_value,
            } => {
                f += &format!(
                    ",\"children\":[{},{},{}]",
                    mask.json_node(),
                    then_value.json_node(),
                    else_value.json_node()
                );
            }
            Node::VectorLit { shape, elems } => {
                let kids: Vec<String> = elems.iter().map(|e| e.json_node()).collect();
                f += &format!(
                    ",\"shape\":\"{}\",\"children\":[{}]",
                    shape.name(),
                    kids.join(",")
                );
            }
            Node::VectorSplat { shape, value } => {
                f += &format!(
                    ",\"shape\":\"{}\",\"children\":[{}]",
                    shape.name(),
                    value.json_node()
                );
            }
            Node::VectorLoad { shape, arr, idx } => {
                f += &format!(
                    ",\"shape\":\"{}\",\"children\":[{},{}]",
                    shape.name(),
                    arr.json_node(),
                    idx.json_node()
                );
            }
            Node::VectorStore {
                shape,
                arr,
                idx,
                value,
            } => {
                f += &format!(
                    ",\"shape\":\"{}\",\"children\":[{},{},{}]",
                    shape.name(),
                    arr.json_node(),
                    idx.json_node(),
                    value.json_node()
                );
            }
            Node::VectorExtract { vector, lane } => {
                f += &format!(",\"lane\":{},\"children\":[{}]", lane, vector.json_node());
            }
            Node::If(c, t, e) => {
                f += &format!(
                    ",\"children\":[{},{},{}]",
                    c.json_node(),
                    t.json_node(),
                    e.json_node()
                );
            }
            Node::Loop {
                vars,
                cond,
                steps,
                result,
            } => {
                let names: Vec<String> = vars
                    .iter()
                    .map(|(name, _, _, _)| format!("\"{}\"", esc(name)))
                    .collect();
                let inits: Vec<String> = vars
                    .iter()
                    .map(|(_, _, _, init)| init.json_node())
                    .collect();
                let steps: Vec<String> = steps.iter().map(|step| step.json_node()).collect();
                f += &format!(
                    ",\"names\":[{}],\"initializers\":[{}],\"condition\":{},\"steps\":[{}],\"result\":{}",
                    names.join(","),
                    inits.join(","),
                    cond.json_node(),
                    steps.join(","),
                    result.json_node()
                );
            }
            Node::Unit => {}
            Node::Brk => {}
            Node::Lam {
                param,
                param_ty,
                body,
            } => {
                f += &format!(
                    ",\"param\":\"{}\",\"paramType\":\"{}\",\"children\":[{}]",
                    esc(param),
                    esc(&param_ty.to_string()),
                    body.json_node()
                );
            }
            Node::App { fun, arg } => {
                f += &format!(",\"children\":[{},{}]", fun.json_node(), arg.json_node())
            }
            Node::Let { name, bound, body } | Node::LetMut { name, bound, body } => {
                f += &format!(
                    ",\"name\":\"{}\",\"children\":[{},{}]",
                    esc(name),
                    bound.json_node(),
                    body.json_node()
                )
            }
            Node::Block { items, body } => {
                let binders: Vec<String> = items
                    .iter()
                    .map(|item| match item {
                        TypedBlockItem::Let { name, .. } | TypedBlockItem::LetMut { name, .. } => {
                            format!("[\"{}\"]", esc(name))
                        }
                        TypedBlockItem::LetTuple { names, .. } => {
                            let names: Vec<String> = names
                                .iter()
                                .map(|name| format!("\"{}\"", esc(name)))
                                .collect();
                            format!("[{}]", names.join(","))
                        }
                    })
                    .collect();
                let mut kids: Vec<String> =
                    items.iter().map(|item| item.bound().json_node()).collect();
                kids.push(body.json_node());
                f += &format!(
                    ",\"binders\":[{}],\"children\":[{}]",
                    binders.join(","),
                    kids.join(",")
                );
            }
            Node::Assign { name, value } => {
                f += &format!(
                    ",\"name\":\"{}\",\"children\":[{}]",
                    esc(name),
                    value.json_node()
                )
            }
            Node::RefNew { value, .. } => f += &format!(",\"children\":[{}]", value.json_node()),
            Node::Deref { cell, .. } => f += &format!(",\"children\":[{}]", cell.json_node()),
            Node::RefAssign { target, value, .. } => {
                f += &format!(
                    ",\"children\":[{},{}]",
                    target.json_node(),
                    value.json_node()
                )
            }
            Node::Perform { label, arg } => {
                f += &format!(
                    ",\"label\":\"{}\",\"children\":[{}]",
                    esc(&label.to_string()),
                    arg.json_node()
                )
            }
            Node::Quote(b) | Node::Splice(b) | Node::Genlet(b) | Node::Letloc(b) => {
                f += &format!(",\"children\":[{}]", b.json_node())
            }
            Node::Peek(w, a) => {
                f += &format!(",\"width\":{},\"children\":[{}]", w.bits(), a.json_node())
            }
            Node::Poke(w, a, b) => {
                f += &format!(
                    ",\"width\":{},\"children\":[{},{}]",
                    w.bits(),
                    a.json_node(),
                    b.json_node()
                )
            }
            Node::Fill(a, b, c) | Node::Copy(a, b, c) => {
                f += &format!(
                    ",\"children\":[{},{},{}]",
                    a.json_node(),
                    b.json_node(),
                    c.json_node()
                )
            }
            Node::Index(w, a, b) => {
                f += &format!(
                    ",\"width\":{},\"children\":[{},{}]",
                    w.bits(),
                    a.json_node(),
                    b.json_node()
                )
            }
            Node::IndexSet(w, a, b, c) => {
                f += &format!(
                    ",\"width\":{},\"children\":[{},{},{}]",
                    w.bits(),
                    a.json_node(),
                    b.json_node(),
                    c.json_node()
                )
            }
            Node::Tuple(es) => {
                let kids: Vec<String> = es.iter().map(|e| e.json_node()).collect();
                f += &format!(",\"children\":[{}]", kids.join(","));
            }
            Node::LetTuple(names, e, body) => {
                let ns: Vec<String> = names.iter().map(|n| format!("\"{}\"", esc(n))).collect();
                f += &format!(
                    ",\"names\":[{}],\"children\":[{},{}]",
                    ns.join(","),
                    e.json_node(),
                    body.json_node()
                );
            }
            Node::Record(fs) => {
                let ns: Vec<String> = fs.iter().map(|(n, _)| format!("\"{}\"", esc(n))).collect();
                let kids: Vec<String> = fs.iter().map(|(_, t)| t.json_node()).collect();
                f += &format!(
                    ",\"fields\":[{}],\"children\":[{}]",
                    ns.join(","),
                    kids.join(",")
                );
            }
            Node::Field(r, name) => {
                f += &format!(
                    ",\"field\":\"{}\",\"children\":[{}]",
                    esc(name),
                    r.json_node()
                );
            }
            Node::ArrayLit { elems, .. } => {
                let kids: Vec<String> = elems.iter().map(|e| e.json_node()).collect();
                f += &format!(",\"children\":[{}]", kids.join(","));
            }
            Node::Len(a) => {
                f += &format!(",\"children\":[{}]", a.json_node());
            }
            Node::ArrayGet { arr, idx, .. } => {
                f += &format!(",\"children\":[{},{}]", arr.json_node(), idx.json_node());
            }
            Node::ArraySet { arr, idx, val, .. } => {
                f += &format!(
                    ",\"children\":[{},{},{}]",
                    arr.json_node(),
                    idx.json_node(),
                    val.json_node()
                );
            }
            Node::Construct { tag, args } => {
                let kids: Vec<String> = args.iter().map(|(a, _, _)| a.json_node()).collect();
                f += &format!(",\"tag\":{tag},\"children\":[{}]", kids.join(","));
            }
            Node::Match { scrutinee, arms } => {
                let kids: Vec<String> = std::iter::once(scrutinee.json_node())
                    .chain(arms.iter().map(|a| a.body.json_node()))
                    .collect();
                f += &format!(",\"children\":[{}]", kids.join(","));
            }
            Node::Handle { scrutinee, handler } => {
                let ops: Vec<String> = handler
                    .ops
                    .iter()
                    .map(|op| {
                        format!(
                            "{{\"op\":\"{}\",\"arg\":\"{}\",\"resume\":\"{}\",\"body\":{}}}",
                            esc(&op.op.to_string()),
                            esc(&op.arg),
                            esc(&op.resume),
                            op.body.json_node()
                        )
                    })
                    .collect();
                f += &format!(
                    ",\"scrutinee\":{},\"handler\":{{\"ops\":[{}],\"return\":{{\"var\":\"{}\",\"body\":{}}}}}",
                    scrutinee.json_node(),
                    ops.join(","),
                    esc(&handler.ret.var),
                    handler.ret.body.json_node()
                );
            }
        }
        format!("{{{f}}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{Label, Row, Term, Type, VectorShape};

    fn world(s: &str) -> Label {
        Label::World(s.to_string())
    }
    fn el(stage: Stage, e: &Term) -> Typed {
        elaborate(&Sig::new(), &Ctx::new(), stage, e).unwrap()
    }

    #[test]
    fn every_node_is_decorated() {
        // (λx:Unit. perform console x) ()   :  Unit ! {console} @ 0
        let f = Term::Lam(
            "x".into(),
            Some(Type::Unit),
            Box::new(Term::Perform(
                world("console"),
                Box::new(Term::Var("x".into())),
            )),
        );
        let t = el(0, &Term::App(Box::new(f), Box::new(Term::Unit)));
        assert_eq!(t.ty, Type::Unit);
        assert_eq!(t.row, Row::single(world("console")));
        assert_eq!(t.stage, 0);

        let Node::App { fun, arg } = &t.node else {
            panic!("root should be App")
        };
        assert_eq!(arg.ty, Type::Unit);
        let Node::Lam { body, .. } = &fun.node else {
            panic!("fun should be Lam")
        };
        // the deepest node — the perform — is itself decorated with {console}.
        assert_eq!(body.ty, Type::Unit);
        assert_eq!(body.row, Row::single(world("console")));
    }

    #[test]
    fn operation_call_sugar_elaborates_to_perform() {
        let e = crate::parse::parse("effect ask : Unit -> Int in ask()").unwrap();
        let t = el(0, &e);
        assert_eq!(t.ty, Type::Int);
        assert_eq!(t.row, Row::single(Label::User("ask".into())));
        assert!(matches!(
            t.node,
            Node::Perform {
                label: Label::User(ref name),
                ..
            } if name == "ask"
        ));
    }

    #[test]
    fn handler_sugar_discharges_operation_call_sugar() {
        let e = crate::parse::parse(
            "effect ask : Unit -> Int in handle (ask() + 1) with { ask(_) -> 41 }",
        )
        .unwrap();
        let t = el(0, &e);
        assert_eq!(t.ty, Type::Int);
        assert!(t.row.is_pure(), "handler should discharge ask: {}", t.row);
        assert!(matches!(t.node, Node::Handle { .. }));
    }

    #[test]
    fn operation_call_sugar_respects_local_shadowing() {
        let e =
            crate::parse::parse("effect ask : Unit -> Int in let ask = fn x: Unit => 7 in ask()")
                .unwrap();
        let t = el(0, &e);
        assert_eq!(t.ty, Type::Int);
        assert!(
            t.row.is_pure(),
            "local function call should be pure: {}",
            t.row
        );
    }

    #[test]
    fn construct_tags_scalar_arg_into_a_type_variable_field() {
        // In `Cons(1, Nil)` the first field of `Cons` is the type-param `a`
        // (a uniform word slot), so the scalar `1` is wrapped in a Tag
        // coercion; the second field is `List[a]` — already a handle — so `Nil`
        // is left alone. The repr-poly boundary, made concrete.
        let e = crate::parse::parse("type List[a] = Nil | Cons(a, List[a]) in Cons(1, Nil)")
            .expect("parses");
        let t = el(0, &e);
        let Node::Construct { args, .. } = &t.node else {
            panic!("root should be a Cons construct, got {:?}", t.node)
        };
        assert!(
            matches!(
                args[0].0.node,
                Node::Coerce {
                    kind: crate::syntax::Coercion::Tag,
                    ..
                }
            ),
            "a scalar into a type-variable field is tagged, got {:?}",
            args[0].0.node
        );
        // The tagged field's recorded layout is the traced **word cell** (D4):
        // laid in the pointer region (`classify` runs) but stored verbatim.
        assert!(
            args[0].1.is_word_cell(),
            "a type-variable field is a word cell, got {:?}",
            args[0].1
        );
        assert!(
            !matches!(args[1].0.node, Node::Coerce { .. }),
            "a handle-typed field is not coerced, got {:?}",
            args[1].0.node
        );
    }

    #[test]
    fn app_tags_scalar_arg_into_a_type_variable_parameter() {
        // `id 5`: id's parameter is the type-var `a` (a uniform word slot), so
        // the scalar `5` is tagged at the application boundary — the App-side
        // mirror of the Construct tagging in the test above.
        let e = crate::parse::parse("let id = fn x => x in id 5").expect("parses");
        let t = el(0, &e);
        let Node::Let { body, .. } = &t.node else {
            panic!("root should be a let, got {:?}", t.node)
        };
        // `id 5`: id returns its argument at type `a`, pinned to `Int` at this
        // call, so the App RESULT is wrapped in the App-result `Untag` (T7, the
        // dual of the arg `Tag`). Peel it to reach the application underneath.
        let app_node = match &body.node {
            Node::Coerce {
                kind: crate::syntax::Coercion::Untag,
                inner,
                ..
            } => &inner.node,
            other => other,
        };
        let Node::App { arg, .. } = app_node else {
            panic!("body should be an application, got {:?}", app_node)
        };
        assert!(
            matches!(
                arg.node,
                Node::Coerce {
                    kind: crate::syntax::Coercion::Tag,
                    ..
                }
            ),
            "a scalar into a type-variable parameter is tagged, got {:?}",
            arg.node
        );
    }

    #[test]
    fn elaborate_agrees_with_infer_at_the_root() {
        let e = Term::Let(
            "x".into(),
            Box::new(Term::Int(1)),
            Box::new(Term::Var("x".into())),
        );
        let t = el(0, &e);
        let (ty, row) = crate::check::infer(&Sig::new(), &Ctx::new(), 0, &e).unwrap();
        assert_eq!((t.ty, t.row), (ty, row));
    }

    #[test]
    fn quote_decorates_the_body_one_stage_down() {
        // quote(perform console ()) @1 : Code[Unit ! {console}] @ 1; body @ 0
        let q = Term::Quote(Box::new(Term::Perform(
            world("console"),
            Box::new(Term::Unit),
        )));
        let t = el(1, &q);
        assert_eq!(t.stage, 1);
        assert!(matches!(t.ty, Type::Code(_, _)));
        let Node::Quote(b) = &t.node else { panic!() };
        assert_eq!(b.stage, 0, "the quoted body lives one stage down");
        assert_eq!(b.row, Row::single(world("console")));
    }

    #[test]
    fn text_tree_is_indented_and_annotated() {
        let t = el(
            0,
            &Term::Let(
                "x".into(),
                Box::new(Term::Int(1)),
                Box::new(Term::Var("x".into())),
            ),
        );
        let txt = t.to_text();
        // pure rows are omitted in the tree (machine JSON keeps them).
        assert!(txt.starts_with("let x  : Int @ 0\n"), "got:\n{txt}");
        assert!(txt.contains("\n  int 1  : Int @ 0\n"), "got:\n{txt}");
        assert!(txt.contains("\n  var x  : Int @ 0\n"), "got:\n{txt}");
    }

    #[test]
    fn json_tree_carries_schema_and_nodes() {
        let t = el(0, &Term::Perform(world("console"), Box::new(Term::Unit)));
        let j = t.to_json();
        assert!(j.starts_with(r#"{"schema":"locus-sema/1","ok":true,"tree":{"#));
        assert!(j.contains(r#""node":"perform""#));
        assert!(j.contains(r#""label":"console""#));
        assert!(j.contains(r#""row":["console"]"#));
    }

    #[test]
    fn accumulator_loop_decorates_accumulators() {
        let term =
            crate::parse::parse("loop i = 0, acc = 0 while i < 10 do i + 1, acc + i else acc")
                .unwrap();
        let t = el(0, &term);
        assert_eq!(t.ty, Type::Int);
        assert!(t.row.is_pure(), "integer loop should be pure: {}", t.row);
        let Node::Loop {
            vars,
            cond,
            steps,
            result,
        } = &t.node
        else {
            panic!("expected loop")
        };
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].0, "i");
        assert_eq!(vars[0].1, Type::Int);
        assert_eq!(vars[1].0, "acc");
        assert_eq!(vars[1].1, Type::Int);
        assert_eq!(cond.ty, Type::Bool);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].ty, Type::Int);
        assert_eq!(steps[1].ty, Type::Int);
        assert_eq!(result.ty, Type::Int);
    }

    #[test]
    fn accumulator_loop_step_arity_is_checked() {
        // A genuine arity mismatch (more steps than accumulators) still fires
        // LoopArity. (A *single* step for a multi-var loop is now the tuple
        // form — see the two tests below.)
        let term = crate::parse::parse(
            "loop i = 0, acc = 0 while i < 10 do i + 1, acc + 1, i else acc",
        )
        .unwrap();
        let err = elaborate(&Sig::new(), &Ctx::new(), 0, &term).unwrap_err();
        assert_eq!(err, TypeErr::LoopArity { expected: 2, found: 3 });
    }

    #[test]
    fn multi_var_loop_accepts_a_single_tuple_step() {
        // The natural shape: a multi-variable loop whose single step is a tuple
        // of the next accumulator values (so a `let`/`match` can be shared
        // across them). Desugars to a single-accumulator tuple loop.
        let term = crate::parse::parse(
            "loop a = 0, b = 100 while a < 10 do let m = a + 1 in (m, b - m) else b",
        )
        .unwrap();
        let t = el(0, &term);
        assert_eq!(t.ty, Type::Int);
    }

    #[test]
    fn multi_var_loop_single_scalar_step_is_a_tuple_mismatch() {
        // A single *non-tuple* step for a multi-var loop is a clear type error:
        // the step must be an N-tuple of the accumulators.
        let term =
            crate::parse::parse("loop i = 0, acc = 0 while i < 10 do i + 1 else acc").unwrap();
        let err = elaborate(&Sig::new(), &Ctx::new(), 0, &term).unwrap_err();
        match err {
            TypeErr::Mismatch { expected, found } => {
                assert_eq!(expected, Type::Tuple(vec![Type::Int, Type::Int]));
                assert_eq!(found, Type::Int);
            }
            other => panic!("expected a tuple Mismatch, got {other:?}"),
        }
    }

    // ── S2: parametric polymorphism (schemes / value restriction) ───────────

    /// `fn x => x` (un-annotated parameter) **infers** `a -> a`: a fresh
    /// parameter variable shared by domain and codomain. We read this off the
    /// *pre-zonk* tree (the public `elaborate` would default the residual var to
    /// `Int` per D6); the structural fact is domain == codomain == the same var.
    #[test]
    fn unannotated_lambda_infers_a_to_a() {
        let id = Term::Lam("x".into(), None, Box::new(Term::Var("x".into())));
        unify::reset_store();
        let t = elaborate_inner(&Sig::new(), &Ctx::new(), 0, &id).unwrap();
        match &t.ty {
            Type::Fun(dom, cod, row) => {
                assert!(row.is_pure(), "the identity is pure");
                assert!(
                    matches!(**dom, Type::Var(_)),
                    "domain is a fresh var, got {dom}"
                );
                assert_eq!(
                    dom, cod,
                    "`fn x => x` infers a -> a (same variable both sides)"
                );
            }
            other => panic!("expected an arrow, got {other}"),
        }
    }

    /// The headline **milestone**: `let id = fn x => x in (id 1, id true)`
    /// type-checks. `id` is a value (a lambda), so its type generalizes to
    /// `∀a. a -> a`; each use instantiates fresh — one solved to `Int`, one to
    /// `Bool` — and the tuple is `(Int, Bool)` performing `gc`.
    #[test]
    fn milestone_id_at_two_types() {
        let id = Term::Lam("x".into(), None, Box::new(Term::Var("x".into())));
        let body = Term::Tuple(vec![
            Term::App(Box::new(Term::Var("id".into())), Box::new(Term::Int(1))),
            Term::App(Box::new(Term::Var("id".into())), Box::new(Term::Bool(true))),
        ]);
        let e = Term::Let("id".into(), Box::new(id), Box::new(body));
        let t = el(0, &e);
        assert_eq!(t.ty, Type::Tuple(vec![Type::Int, Type::Bool]));
        assert_eq!(
            t.row,
            Row::single(Label::Gc),
            "the tuple allocation shows gc"
        );
    }

    /// `fn f => f 1` type-checks: applying a bare parameter forces `App` to
    /// **unify the callee against a fresh arrow** to discover its shape. `f`'s
    /// domain solves to `Int`; the lambda is `(Int -> a) -> a`, which — with the
    /// unconstrained `a` defaulted by zonk (D6) — is `(Int -> Int) -> Int`.
    #[test]
    fn app_discovers_the_arrow_from_a_bare_parameter() {
        let e = Term::Lam(
            "f".into(),
            None,
            Box::new(Term::App(
                Box::new(Term::Var("f".into())),
                Box::new(Term::Int(1)),
            )),
        );
        // The zonked top-level type: `f` was discovered to be an arrow with
        // domain Int; the (free) codomain defaults to Int.
        let t = el(0, &e);
        assert_eq!(
            t.ty,
            Type::Fun(
                Box::new(Type::Fun(
                    Box::new(Type::Int),
                    Box::new(Type::Int),
                    Row::pure()
                )),
                Box::new(Type::Int),
                Row::pure(),
            ),
            "fn f => f 1  ⇒  (Int -> Int) -> Int (the arrow was discovered by unification)"
        );
    }

    /// The **value restriction** (D4) end-to-end through sema: a polymorphic use
    /// of a value-bound `id` succeeds at three distinct types **including a
    /// function type**, which is only possible if it genuinely generalized.
    #[test]
    fn value_let_generalizes_across_a_function_type() {
        // let id = fn x => x in (id 1, id true, id (fn y: Int => y))
        let id = Term::Lam("x".into(), None, Box::new(Term::Var("x".into())));
        let inner_fn = Term::Lam("y".into(), Some(Type::Int), Box::new(Term::Var("y".into())));
        let body = Term::Tuple(vec![
            Term::App(Box::new(Term::Var("id".into())), Box::new(Term::Int(1))),
            Term::App(Box::new(Term::Var("id".into())), Box::new(Term::Bool(true))),
            Term::App(Box::new(Term::Var("id".into())), Box::new(inner_fn)),
        ]);
        let e = Term::Let("id".into(), Box::new(id), Box::new(body));
        let t = el(0, &e);
        assert_eq!(
            t.ty,
            Type::Tuple(vec![
                Type::Int,
                Type::Bool,
                Type::Fun(Box::new(Type::Int), Box::new(Type::Int), Row::pure()),
            ])
        );
    }

    /// The soundness lynchpin: a **non-value** `let` does **not** generalize. The
    /// RHS `(fn x => x) (fn z => z)` is an application (a non-value), so the
    /// binding `h` is monomorphic; using it at both `Int` and `Bool` therefore
    /// **fails** — exactly the unsoundness the value restriction forbids.
    #[test]
    fn nonvalue_let_stays_monomorphic() {
        // let h = (fn x => x) (fn z => z) in (h 1, h true)  ⇒  type error
        let idx = Term::Lam("x".into(), None, Box::new(Term::Var("x".into())));
        let idz = Term::Lam("z".into(), None, Box::new(Term::Var("z".into())));
        let rhs = Term::App(Box::new(idx), Box::new(idz)); // an App — NOT a value
        let body = Term::Tuple(vec![
            Term::App(Box::new(Term::Var("h".into())), Box::new(Term::Int(1))),
            Term::App(Box::new(Term::Var("h".into())), Box::new(Term::Bool(true))),
        ]);
        let e = Term::Let("h".into(), Box::new(rhs), Box::new(body));
        let res = elaborate(&Sig::new(), &Ctx::new(), 0, &e);
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "a non-value binding must stay monomorphic — using it at two types must clash, got {res:?}"
        );
    }

    /// **Effect honesty** (D4 carve-out): a value aggregate generalizes its
    /// *type* but its `gc` row is **not** lost — the binding's row still carries
    /// the allocation, surfacing on the whole `let`.
    #[test]
    fn value_aggregate_keeps_its_gc_row() {
        // let p = (1, 2) in p   ⇒  (Int, Int) ! {gc}  (gc survives generalization)
        let e = Term::Let(
            "p".into(),
            Box::new(Term::Tuple(vec![Term::Int(1), Term::Int(2)])),
            Box::new(Term::Var("p".into())),
        );
        let t = el(0, &e);
        assert_eq!(t.ty, Type::Tuple(vec![Type::Int, Type::Int]));
        assert_eq!(
            t.row,
            Row::single(Label::Gc),
            "gc is recorded despite the value generalizing"
        );
    }

    // ── S3: generic declarations (parametric sums) ──────────────────────────

    /// Parse a snippet and elaborate it at stage 0 (the S3 surface is written in
    /// source — `type List[a] = …` — so the parser is the natural front door).
    fn check_src(src: &str) -> Result<Typed, TypeErr> {
        let term = crate::parse::parse(src).expect("S3 test snippet must parse");
        elaborate(&Sig::new(), &Ctx::new(), 0, &term)
    }
    /// A monomorphic `Named` (`args == []`) — the byte-for-byte pre-S3 shape.
    fn named0(n: &str) -> Type {
        Type::Named(n.into(), vec![])
    }
    /// `List[arg]`.
    fn list_of(arg: Type) -> Type {
        Type::Named("List".into(), vec![arg])
    }

    /// **The S3 headline milestone.** One `List` declaration, used at TWO element
    /// types in a tuple, checks as `(List[Int], List[Bool])` — the constructor is
    /// genuinely parametric (its `?a` is instantiated fresh per use, one solved to
    /// `Int`, one to `Bool`). Performs `gc` (each `Cons` allocates).
    #[test]
    fn parametric_list_round_trips_at_two_types() {
        let t =
            check_src("type List[a] = Nil | Cons(a, List[a]) in (Cons(1, Nil), Cons(true, Nil))")
                .expect("the round-trip program type-checks");
        assert_eq!(
            t.ty,
            Type::Tuple(vec![list_of(Type::Int), list_of(Type::Bool)])
        );
        assert_eq!(
            t.row,
            Row::single(Label::Gc),
            "the Cons allocations show gc"
        );
    }

    /// The same round-trip read through `Display` — `(List[Int], List[Bool])`,
    /// proving the rendered surface (what `locus check` prints) is right.
    #[test]
    fn parametric_list_renders_its_arguments() {
        let t =
            check_src("type List[a] = Nil | Cons(a, List[a]) in (Cons(1, Nil), Cons(true, Nil))")
                .unwrap();
        assert_eq!(t.ty.to_string(), "(List[Int], List[Bool])");
    }

    /// **`match` refines the payload** (D13). `match Cons(7, Nil) with Cons(h,_) =>
    /// h | Nil => 0` is `Int`: unifying the `Cons` arm's `List[?a]` against the
    /// scrutinee's `List[Int]` solves `?a := Int`, so `h` (originally `?a`) is
    /// `Int` and the arm's `h` typechecks as the `Int` result.
    #[test]
    fn match_refines_the_payload_to_int() {
        let t = check_src(
            "type List[a] = Nil | Cons(a, List[a]) in \
             match Cons(7, Nil) with | Cons(h, t) => h | Nil => 0",
        )
        .expect("the refining match type-checks");
        assert_eq!(t.ty, Type::Int);
    }

    /// **The discriminating failure** (the proof the refinement is *real*, not a
    /// default to `Int`). `match Cons(true, Nil) with Cons(h,_) => h + 1 | Nil =>
    /// 0` MUST fail: `h` refines to `Bool` (the scrutinee is `List[Bool]`), so
    /// `h + 1` is an `Int`/`Bool` clash. If refinement silently defaulted `h` to
    /// `Int`, this would (wrongly) pass.
    #[test]
    fn match_refinement_is_real_bool_payload_rejects_plus_one() {
        let res = check_src(
            "type List[a] = Nil | Cons(a, List[a]) in \
             match Cons(true, Nil) with | Cons(h, t) => h + 1 | Nil => 0",
        );
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "h is refined to Bool, so `h + 1` must clash — got {res:?}"
        );
    }

    /// A **recursive** generic sum (`Tree[a]`) used at `Int`: `Node(Leaf(1),
    /// Leaf(2))` is `Tree[Int]`. The recursion (`Node(Tree[a], Tree[a])`) and the
    /// parameter compose — the field-type `Tree[a]` parses and refines like any
    /// other nominal.
    #[test]
    fn recursive_generic_tree_checks() {
        let t =
            check_src("type Tree[a] = Leaf(a) | Node(Tree[a], Tree[a]) in Node(Leaf(1), Leaf(2))")
                .expect("the recursive generic type-checks");
        assert_eq!(t.ty, Type::Named("Tree".into(), vec![Type::Int]));
    }

    /// A nested instance: `Cons(Cons(1, Nil), Nil)` is `List[List[Int]]` — the
    /// element type is itself a parametric instance, so the args nest.
    #[test]
    fn nested_parametric_instance() {
        let t =
            check_src("type List[a] = Nil | Cons(a, List[a]) in Cons(Cons(1, Nil), Nil)").unwrap();
        assert_eq!(t.ty, list_of(list_of(Type::Int)));
    }

    /// **Type-argument arity is checked** (D14, RN-E0225): a `List` named with two
    /// arguments in an annotation is rejected, distinct from a field-count error.
    #[test]
    fn wrong_type_argument_arity_is_rejected() {
        let res =
            check_src("type List[a] = Nil | Cons(a, List[a]) in (fn x: List[Int, Bool] => x)");
        assert_eq!(
            res,
            Err(TypeErr::ArityMismatch {
                name: "List".into(),
                expected: 1,
                found: 2
            }),
            "List[Int, Bool] names a one-parameter type with two arguments (RN-E0225)"
        );
    }

    /// **The differential (D8): a monomorphic sum is unchanged byte-for-byte.** A
    /// nullary-and-payload sum with NO parameters checks exactly as before — the
    /// result is a bare `Named(name, [])`, renders as the bare name, and matches
    /// refine to the concrete field types just as the pre-S3 checker did.
    #[test]
    fn monomorphic_sum_is_unchanged() {
        // The constructed value is the bare nominal `Color` (empty args).
        let t = check_src("type Color = Red | Green | Blue in Green").unwrap();
        assert_eq!(t.ty, named0("Color"));
        assert_eq!(
            t.ty.to_string(),
            "Color",
            "no `[]` suffix on a monomorphic sum"
        );

        // A monomorphic payload still binds at its concrete field type in a match.
        let t2 = check_src(
            "type Shape = Circle(Int) | Square(Int) in \
             match Circle(3) with | Circle(r) => r | Square(s) => s",
        )
        .unwrap();
        assert_eq!(t2.ty, Type::Int);
    }

    /// A type declaration's constructors are scoped to its body.
    #[test]
    fn type_declaration_scope_does_not_escape_body() {
        let res = check_src("let x = type A = C in 0 in C");
        assert_eq!(
            res,
            Err(TypeErr::UnknownCtor("C".into())),
            "constructor C must be scoped to the type body"
        );
    }

    #[test]
    fn type_environment_resets_between_elaborations() {
        check_src("type A = C in C").expect("C is in scope inside the type body");
        let res = check_src("C");
        assert_eq!(
            res,
            Err(TypeErr::UnknownCtor("C".into())),
            "a prior elaboration must not leak type declarations"
        );
    }

    #[test]
    fn generic_array_element_layout_is_a_known_word_cell() {
        // Retired boxing-era assertion (was: layout *unknown* so lowering refuses).
        // With repr-poly tags a generic `Var` array element is a **known** traced
        // word cell, so the front-end no longer freezes it. (Word-cell *array*
        // codegen — a traced, verbatim-stored element payload — is out of scope for
        // the list_len/sum-type slice; that is a locus-llvm follow-up. Here we only
        // pin that the front-end layout is now decided, not unknown.)
        let t = check_src("let singleton = fn x => [x] in singleton (1, 2)")
            .expect("generic array allocation still type-checks");
        assert_eq!(
            t.ty,
            Type::Array(Box::new(Type::Tuple(vec![Type::Int, Type::Int])))
        );
        assert!(
            !t.has_unknown_layout(),
            "a generic Var array element is now a known word cell, not an unknown layout"
        );
    }

    #[test]
    fn vector_arithmetic_and_lane_projection_typecheck() {
        let t = check_src(
            "let a = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
             let b = splatQuad (toFloat32 10.0) in \
             let c = a + b in c.z",
        )
        .expect("local Float32 vector arithmetic type-checks");
        assert_eq!(t.ty, Type::Float32);
        assert!(t.row.is_pure());

        let t = check_src("let v = Pair(1.25, 2.5) in v.lane1")
            .expect("Float vector lane aliases type-check");
        assert_eq!(t.ty, Type::Float);
    }

    #[test]
    fn vector_scalar_arithmetic_broadcasts_the_scalar_operand() {
        let t = check_src(
            "let a = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
             let c = a * 4.0 in c.w",
        )
        .expect("Float32 vector multiplied by a literal broadcasts the literal");
        assert_eq!(t.ty, Type::Float32);

        let t = check_src("let a = Pair(1.0, 2.0) in let c = 4.0 * a in c.x")
            .expect("Float vector multiplied by a scalar broadcasts on the left");
        assert_eq!(t.ty, Type::Float);

        let t = check_src(
            "let a = splatQuad (toFloat32 2.0) in \
             let s = toFloat32 4.0 in \
             let c = s + a in c.x",
        )
        .expect("Float32 scalar variables work when explicitly narrowed");
        assert_eq!(t.ty, Type::Float32);

        let res = check_src(
            "let a = splatQuad (toFloat32 2.0) in \
             let s = 4.0 in \
             a * s",
        );
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "non-literal Float variables should not implicitly narrow to Float32"
        );
    }

    #[test]
    fn explicit_float_math_typechecks_for_scalars_and_vectors() {
        let t = check_src("sqrt (toFloat32 4.0)").expect("Float32 sqrt type-checks");
        assert_eq!(t.ty, Type::Float32);

        let t = check_src("fma(1.5, 2.0, 0.25)").expect("Float fma type-checks");
        assert_eq!(t.ty, Type::Float);

        let t = check_src(
            "let a = splatQuad (toFloat32 2.0) in \
             let b = splatQuad (toFloat32 3.0) in \
             let c = splatQuad (toFloat32 4.0) in \
             (fma(a, b, c)).x",
        )
        .expect("vector fma type-checks");
        assert_eq!(t.ty, Type::Float32);
    }

    #[test]
    fn vector_reductions_typecheck_to_lane_scalars() {
        let t = check_src("sum (Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0))")
            .expect("Quad[Float32] sum type-checks");
        assert_eq!(t.ty, Type::Float32);

        let t =
            check_src("dot(Pair(1.0, 2.0), Pair(3.0, 4.0))").expect("Pair[Float] dot type-checks");
        assert_eq!(t.ty, Type::Float);

        let t = check_src("length (splatQuad (toFloat32 3.0))")
            .expect("Quad[Float32] length type-checks");
        assert_eq!(t.ty, Type::Float32);

        let res = check_src("sum 1.0");
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "sum should require a lane vector"
        );

        let res = check_src("dot(Pair(1.0, 2.0), Quad(1.0, 2.0, 3.0, 4.0))");
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "dot should require matching lane shapes"
        );
    }

    #[test]
    fn vector_reduction_names_remain_shadowable() {
        let t = check_src("let length = fn x: Int => x + 1 in length 41")
            .expect("local length binding should win over the reduction helper");
        assert_eq!(t.ty, Type::Int);

        let t = check_src("let dot = fn x: Int => fn y: Int => x + y in dot 4 5")
            .expect("local dot binding should win over the reduction helper");
        assert_eq!(t.ty, Type::Int);
    }

    #[test]
    fn vector_comparisons_produce_masks() {
        let t = check_src(
            "let a = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
             let b = splatQuad (toFloat32 3.0) in \
             a < b",
        )
        .expect("vector comparison type-checks");
        assert_eq!(t.ty, Type::Mask(VectorShape::Quad));

        let t = check_src(
            "let a = Pair(1.0, 2.0) in \
             a == 2.0",
        )
        .expect("vector-scalar comparison broadcasts the scalar");
        assert_eq!(t.ty, Type::Mask(VectorShape::Pair));
    }

    #[test]
    fn vector_masks_select_and_reduce() {
        let t = check_src(
            "let a = Quad(toFloat32 1.0, toFloat32 5.0, toFloat32 2.0, toFloat32 8.0) in \
             let b = splatQuad (toFloat32 4.0) in \
             let c = select(a < b, b, a) in c.y",
        )
        .expect("select blends same-shape vectors under a mask");
        assert_eq!(t.ty, Type::Float32);

        let t = check_src(
            "let a = Pair(1.0, 2.0) in \
             any (a < 1.5)",
        )
        .expect("any reduces a mask to Bool");
        assert_eq!(t.ty, Type::Bool);

        let t = check_src(
            "let a = Pair(1.0, 2.0) in \
             all (a < 3.0)",
        )
        .expect("all reduces a mask to Bool");
        assert_eq!(t.ty, Type::Bool);

        let res = check_src("any (splatQuad (toFloat32 1.0))");
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "any should require a mask"
        );

        let res = check_src(
            "let a = Pair(1.0, 2.0) in \
             let b = Quad(1.0, 2.0, 3.0, 4.0) in \
             select(a < 2.0, b, b)",
        );
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "select should require the mask shape to match the vector arms"
        );
    }

    #[test]
    fn vector_mask_helper_names_remain_shadowable() {
        let t = check_src("let any = fn x: Int => x + 1 in any 41")
            .expect("local any binding should win over the mask helper");
        assert_eq!(t.ty, Type::Int);

        let t = check_src("let all = fn x: Int => x + 2 in all 40")
            .expect("local all binding should win over the mask helper");
        assert_eq!(t.ty, Type::Int);

        let t = check_src(
            "let select = fn x: Int => fn y: Int => fn z: Int => x + y + z in select 1 2 3",
        )
        .expect("local select binding should win over the mask helper");
        assert_eq!(t.ty, Type::Int);
    }

    #[test]
    fn vector_literals_enforce_lane_count() {
        let res = check_src("Pair(toFloat32 1.0)");
        assert_eq!(
            res,
            Err(TypeErr::CtorArity {
                ctor: VectorShape::Pair.name().into(),
                expected: 2,
                found: 1
            })
        );
    }

    #[test]
    fn generic_constructor_field_layout_is_a_known_word_cell() {
        // Retired boxing-era assertion (was: layout *unknown* so lowering refuses).
        // A generic `Var` constructor field is now a **known** traced word cell
        // (D4): laid in the pointer region, stored verbatim. This is exactly the
        // wall the tag slice lifts — a generic `Cons`-over-`Var` lowers.
        let t = check_src(
            "type List[a] = Nil | Cons(a, List[a]) in \
             let singleton = fn x => Cons(x, Nil) in singleton (1, 2)",
        )
        .expect("generic constructor allocation still type-checks");
        assert_eq!(t.ty, list_of(Type::Tuple(vec![Type::Int, Type::Int])));
        assert!(
            !t.has_unknown_layout(),
            "a generic Var constructor field is now a known word cell, not an unknown layout"
        );
    }

    /// **`List[Int]` ≠ `List[Bool]`** under unification (D8): a match arm whose
    /// scrutinee and result mix the two instantiations clashes. Here, building a
    /// tuple `(xs, Cons(true, xs))` where `xs : List[Int]` forces `Cons`'s element
    /// (`Int`, from `xs`) against `true` (`Bool`) — a clash, proving the type
    /// argument participates in unification.
    #[test]
    fn distinct_instantiations_do_not_unify() {
        let res = check_src(
            "type List[a] = Nil | Cons(a, List[a]) in \
             let xs = Cons(1, Nil) in Cons(true, xs)",
        );
        assert!(
            matches!(res, Err(TypeErr::Mismatch { .. })),
            "Cons(true, xs) with xs:List[Int] must clash Bool vs Int — got {res:?}"
        );
    }

    /// A nullary constructor of a parametric sum (`Nil`) used in isolation leaves
    /// its element type **free**, which zonk defaults to `Int` (D6) — so a bare
    /// `Nil` is `List[Int]`. (It carries no `gc`: a nullary ctor still allocates a
    /// tag object, so the row is `{gc}`.) This pins the "phantom param defaults"
    /// corner the Match/Construct zonk-before-classify relies on.
    #[test]
    fn bare_nullary_ctor_defaults_its_param() {
        let t = check_src("type List[a] = Nil | Cons(a, List[a]) in Nil").unwrap();
        assert_eq!(
            t.ty,
            list_of(Type::Int),
            "an unconstrained element defaults to Int (D6)"
        );
        assert_eq!(t.row, Row::single(Label::Gc));
    }

    /// S3.5: a generic recursive binding whose annotated parameter is a nominal
    /// type remains polymorphic after matching. The pattern binder `t` in
    /// `Cons(h, t)` must keep the refined `List[?a]` type; if match reuses its
    /// D6-zonked layout type for binders, `t` becomes `List[Int]` and the Bool
    /// use below fails.
    #[test]
    fn generic_let_rec_match_binders_do_not_default_nominal_params() {
        let t = check_src(
            "type List[a] = Nil | Cons(a, List[a]) in \
             let rec list_len : List[a] -> Int ! {gc} = \
               fn xs: List[a] => match xs with | Nil => 0 | Cons(h, t) => 1 + list_len t \
             in (list_len (Cons(1, Nil)), list_len (Cons(true, Nil)))",
        )
        .unwrap();
        assert_eq!(t.ty, Type::Tuple(vec![Type::Int, Type::Int]));
    }

    // ── traits / qualified types v1 (trait-resolution.md §1.1) ───────────

    /// A bare `trait`+`instance` declaration with no method *use* elaborates: the
    /// trait registers + mints its methods, the instance registers + light-checks,
    /// and the body (`0`) is the result. No obligation is produced (nothing uses a
    /// method), so the `RN-E0230` drain stays empty.
    #[test]
    fn a_trait_and_instance_program_elaborates() {
        let t = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             0",
        )
        .expect("the trait/instance program elaborates");
        assert_eq!(t.ty, Type::Int);
    }

    /// **Sprint 2 — a trait-method use now RESOLVES** (R1, replacing the Sprint-1
    /// NYIMP placeholder). `show 5` instantiates `show : ∀a. Show a => a -> String`,
    /// emitting `Show ?a`; the `5` argument solves `?a := Int`; the obligation
    /// `Show Int` is discharged by `instance Show Int`. The whole program type-checks
    /// as `String` with no trait error.
    #[test]
    fn using_a_trait_method_resolves_against_its_instance() {
        let t = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             show 5",
        )
        .expect("`Show Int` discharges the obligation (Sprint 2)");
        assert_eq!(t.ty, Type::Str);
    }

    /// An `instance` that omits a declared method is `RN-E0239 trait.missing-method`.
    #[test]
    fn an_instance_missing_a_method_is_rn_e0239() {
        let err = check_src(
            "trait TwoM a { foo : a -> Int ; bar : a -> Bool } in \
             instance TwoM Int { foo = fn x => 0 } in \
             0",
        )
        .expect_err("the instance omits `bar`");
        assert_eq!(err.code(), "RN-E0239");
        let TypeErr::TraitMissingMethod {
            method, missing, ..
        } = &err
        else {
            panic!("expected TraitMissingMethod, got {err:?}");
        };
        assert_eq!(method, "bar");
        assert!(*missing);
    }

    /// An `instance` implementing an unknown method is `RN-E0239` (extra method).
    #[test]
    fn an_instance_with_an_unknown_method_is_rn_e0239() {
        let err = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" ; nope = fn x => 0 } in \
             0",
        )
        .expect_err("`nope` is not a declared method");
        assert_eq!(err.code(), "RN-E0239");
        let TypeErr::TraitMissingMethod {
            method, missing, ..
        } = &err
        else {
            panic!("expected TraitMissingMethod, got {err:?}");
        };
        assert_eq!(method, "nope");
        assert!(!*missing);
    }

    /// A constrained scheme whose constraint generalizes is carried on the
    /// `Scheme`: binding `let f = show` over the minted `show` makes `f` carry the
    /// `Show a` constraint, re-emitted at the use `f 7`. In Sprint 2 that obligation
    /// (`Show Int`, after `7` solves the variable) **resolves** against
    /// `instance Show Int` — the whole program type-checks as `String`.
    #[test]
    fn a_constraint_carried_through_a_let_resolves_at_the_use() {
        let t = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             let f = show in f 7",
        )
        .expect("the constraint rides `f` and resolves at the use (Sprint 2)");
        assert_eq!(t.ty, Type::Str);
    }

    // ── Sprint 2: resolution (R1) + the static checks (R3–R7) ────────────

    /// Elaborate a program through the **module graft** (so module-stamped
    /// declarations reach sema for the orphan check R5). Uses `crate::program`,
    /// which grafts the stdlib + user modules, then elaborates at stage 0.
    fn check_program(src: &str) -> Result<Typed, TypeErr> {
        let term = crate::stdlib::program(src).expect("the program parses + grafts");
        elaborate(&Sig::new(), &Ctx::new(), 0, &term)
    }

    /// **The §1.4 worked example.** `Eq`/`Ord` (superclass `Ord ⇒ Eq`), `instance
    /// Eq Int`, `instance Ord Int`, a constrained `min2 : Ord a => a -> a -> a`,
    /// and `min2 3 7`. The obligation `Ord Int` discharges against `instance Ord
    /// Int`; its superclass `Eq Int` (from `trait Ord a requires Eq a`) discharges
    /// against `instance Eq Int`. Type-checks as `Int`, no NYIMP / no trait error.
    #[test]
    fn worked_example_min2_resolves_ord_int_and_its_eq_superclass() {
        // The method bodies are kept trivial (no stdlib dependency in the bare
        // `check_src` env) — the point is resolution, not the comparison logic; the
        // bodies still type-check against the declared method signatures at `Int`.
        let t = check_src(
            "type Ordering = LT | EQ | GT in \
             trait Eq a { eq : a -> a -> Bool } in \
             trait Ord a requires Eq a { compare : a -> a -> Ordering ! {gc} } in \
             instance Eq Int { eq = fn x => fn y => true } in \
             instance Ord Int { compare = fn x => fn y => LT } in \
             let min2 = fn x => fn y => match compare x y with | LT => x | EQ => x | GT => y in \
             min2 3 7",
        )
        .expect("the §1.4 worked example type-checks with Ord Int + Eq Int discharged");
        assert_eq!(t.ty, Type::Int);
    }

    /// **R1 step 1 — a constraint on an abstract variable passes via the caller's
    /// evidence.** A generic `describe = fn x => show x` calls a trait method on
    /// its own constrained parameter: the obligation `Show ?a` is generalized into
    /// `describe`'s scheme (the caller will supply the dictionary), so the body
    /// type-checks with no instance lookup. `describe`'s *use* at `Int` then
    /// resolves against `instance Show Int`. The whole program is `String`.
    #[test]
    fn a_constraint_on_an_abstract_var_passes_via_caller_evidence() {
        let t = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             let describe = fn x => show x in describe 5",
        )
        .expect("the abstract `Show a` rides describe's scheme; its use resolves at Int");
        assert_eq!(t.ty, Type::Str);
    }

    /// **Effect transparency through trait dispatch with a FIXED method row.**
    /// Regression guard: a trait method declared `! {mem_access}` surfaces that
    /// effect at a concrete use. (This already works; it pins the contrast with
    /// the variable-row case below.)
    #[test]
    fn trait_method_with_fixed_row_surfaces_its_effect() {
        let t = check_src(
            "effect mem_access : Int -> Int in \
             type MemC = MemC(Int) in \
             trait Backend b { tick : b -> Int ! {mem_access} } in \
             instance Backend MemC { tick = fn c: MemC => mem_access 1 } in \
             tick (MemC(0))",
        )
        .expect("type-checks");
        let row = format!("{}", t.row);
        assert!(row.contains("mem_access"), "fixed-row method effect, got `{row}`");
    }

    /// **Effect transparency through trait dispatch with a VARIABLE method row
    /// (`trait-resolution.md` §7.3).** A method declared `! {|r}` lets instances
    /// differ; the *resolved instance's* latent row must surface in the caller —
    /// otherwise generic dispatch silently drops effects, negating "every effect
    /// is in the type". The `MemC` instance performs `mem_access`, so using `tick`
    /// at `MemC` must put `mem_access` in the program's row.
    #[test]
    fn trait_method_with_variable_row_propagates_resolved_instance_effect() {
        let t = check_src(
            "effect mem_access : Int -> Int in \
             type MemC = MemC(Int) in \
             trait Backend b { tick : b -> Int ! {|r} } in \
             instance Backend MemC { tick = fn c: MemC => mem_access 1 } in \
             tick (MemC(0))",
        )
        .expect("type-checks");
        let row = format!("{}", t.row);
        assert!(
            row.contains("mem_access"),
            "a variable-row trait method must surface the resolved instance's \
             latent row (here `mem_access`); dropping it negates effect \
             transparency. got `{row}`"
        );
    }

    /// **The effect survives `let`-generalization of a wrapping helper.** The
    /// harder case: `go` is a generalized helper that calls a variable-row method
    /// on a concrete instance. Without binding the method row *before* generalize
    /// (`generalize_resolved`), `go`'s scheme would quantify the still-free row var
    /// and lose `mem_access`. The program's row must carry it.
    #[test]
    fn trait_method_variable_row_effect_survives_let_generalization() {
        let t = check_src(
            "effect mem_access : Int -> Int in \
             type MemC = MemC(Int) in \
             trait Backend b { tick : b -> Int ! {|r} } in \
             instance Backend MemC { tick = fn c: MemC => mem_access 1 } in \
             let go = fn u: Unit => tick (MemC(0)) in go ()",
        )
        .expect("type-checks");
        let row = format!("{}", t.row);
        assert!(
            row.contains("mem_access"),
            "the instance effect must survive let-generalization of the wrapping \
             helper (generalize_resolved binds it pre-quantification), got `{row}`"
        );
    }

    /// **R1 step 3 — no instance (`RN-E0230`).** A trait method used at a type with
    /// no instance: `show` at `Bool` has no `instance Show Bool`.
    #[test]
    fn a_use_with_no_instance_is_rn_e0230() {
        let err = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             show true",
        )
        .expect_err("there is no `instance Show Bool`");
        assert_eq!(err.code(), "RN-E0230");
        assert_eq!(err.slug(), "trait.no-instance");
        let TypeErr::TraitNoInstance { constraint } = &err else {
            panic!("expected TraitNoInstance, got {err:?}");
        };
        assert_eq!(constraint.trait_name, "Show");
        assert_eq!(constraint.ty, Type::Bool);
    }

    /// **R4 — overlapping instances (`RN-E0231`).** Two instances of one trait
    /// whose heads unify (`List[a]` and `List[Int]`, same head `List`).
    #[test]
    fn two_overlapping_instances_are_rn_e0231() {
        let err = check_src(
            "type List[a] = Nil | Cons(a, List[a]) in \
             trait Show a { show : a -> String ! {gc} } in \
             instance Show List[a] { show = fn x => \"l\" } in \
             instance Show List[Int] { show = fn x => \"li\" } in \
             0",
        )
        .expect_err("the two `Show List` instances overlap");
        assert_eq!(err.code(), "RN-E0231");
        assert_eq!(err.slug(), "trait.overlapping-instances");
        assert!(matches!(err, TypeErr::TraitOverlappingInstances { .. }));
    }

    /// **R7-degenerate — duplicate instance (`RN-E0237`).** The same `(trait, head)`
    /// declared twice (identical heads).
    #[test]
    fn a_duplicate_instance_is_rn_e0237() {
        let err = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"a\" } in \
             instance Show Int { show = fn x => \"b\" } in \
             0",
        )
        .expect_err("`Show Int` is declared twice");
        assert_eq!(err.code(), "RN-E0237");
        assert_eq!(err.slug(), "trait.duplicate-instance");
        assert!(matches!(err, TypeErr::TraitDuplicateInstance { .. }));
    }

    /// **R6 — a divergent instance (`RN-E0233`, §5.3).** `instance Eq (Wrap a)
    /// requires Eq (Wrap (Wrap a))` — the context is structurally LARGER than the
    /// head, so resolution would not terminate. Caught at the declaration site.
    #[test]
    fn a_divergent_instance_is_rn_e0233() {
        let err = check_src(
            "type Wrap[a] = Wrap(a) in \
             trait Eq a { eq : a -> a -> Bool } in \
             instance Eq Wrap[a] requires Eq Wrap[Wrap[a]] { eq = fn x => fn y => true } in \
             0",
        )
        .expect_err("the context `Eq Wrap[Wrap[a]]` is larger than the head `Eq Wrap[a]`");
        assert_eq!(err.code(), "RN-E0233");
        assert_eq!(err.slug(), "trait.resolution-diverges");
        assert!(matches!(err, TypeErr::TraitResolutionDiverges { .. }));
    }

    /// **R7 — ambiguity (`RN-E0234`).** A constraint whose type variable is not
    /// determined by the term's visible type. `let g = fn x => show (id x)` where
    /// `id` is the identity does not help; instead the classic shape: a value bound
    /// to a use of `show` on a fresh unconstrained variable that nothing pins.
    /// Here `show readback` where `readback : String -> a` (a generic producing an
    /// unpinned `a`) leaves `Show ?a` with `?a` undetermined.
    #[test]
    fn an_ambiguous_constraint_is_rn_e0234() {
        // `read : ∀a. Read a => String -> a` produces an `a` nothing pins; feeding
        // it to `show` (consuming an `a`) makes the intermediate type ambiguous.
        let err = check_src(
            "trait Read a { read : String -> a } in \
             trait Show a { show : a -> String ! {gc} } in \
             instance Read Int { read = fn s => 0 } in \
             instance Show Int { show = fn x => \"n\" } in \
             let roundtrip = fn s => show (read s) in 0",
        )
        .expect_err("the intermediate type between `read` and `show` is ambiguous");
        assert_eq!(err.code(), "RN-E0234");
        assert_eq!(err.slug(), "trait.ambiguous");
        assert!(matches!(err, TypeErr::TraitAmbiguous { .. }));
    }

    /// **R1.4 — superclass unsatisfied (`RN-E0236`).** `instance Ord Int` exists but
    /// its trait superclass `Eq Int` (from `trait Ord a requires Eq a`) has no
    /// instance. Resolving `Ord Int` (via `min2 3 7`) fails on the missing `Eq Int`.
    #[test]
    fn a_missing_superclass_instance_is_rn_e0236() {
        let err = check_src(
            "type Ordering = LT | EQ | GT in \
             trait Eq a { eq : a -> a -> Bool } in \
             trait Ord a requires Eq a { compare : a -> a -> Ordering ! {gc} } in \
             instance Ord Int { compare = fn x => fn y => LT } in \
             let min2 = fn x => fn y => match compare x y with | LT => x | EQ => x | GT => y in \
             min2 3 7",
        )
        .expect_err("`Ord Int` resolves but its superclass `Eq Int` has no instance");
        assert_eq!(err.code(), "RN-E0236");
        assert_eq!(err.slug(), "trait.superclass-unsatisfied");
        let TypeErr::TraitSuperclassUnsatisfied { superclass, .. } = &err else {
            panic!("expected TraitSuperclassUnsatisfied, got {err:?}");
        };
        assert_eq!(superclass.trait_name, "Eq");
        assert_eq!(superclass.ty, Type::Int);
    }

    /// **R5 — orphan instance (`RN-E0232`), module tracking wired.** Trait `Show`
    /// lives in module `Display`, type `Point` in module `Geometry`, and the
    /// instance `Show Point` in a third module `App` — an orphan (neither home).
    #[test]
    fn an_orphan_instance_is_rn_e0232() {
        let err = check_program(
            "module Display at services = \
               trait Show a { show : a -> String ! {gc} } in () \
             module Geometry at services = \
               type Point = Point(Int) in () \
             module App at app = \
               instance Show Point { show = fn p => \"pt\" } in () \
             0",
        )
        .expect_err("`Show Point` is in neither `Display` nor `Geometry`");
        assert_eq!(err.code(), "RN-E0232");
        assert_eq!(err.slug(), "trait.orphan-instance");
        let TypeErr::TraitOrphanInstance { module, .. } = &err else {
            panic!("expected TraitOrphanInstance, got {err:?}");
        };
        assert_eq!(module, "App");
    }

    /// An instance in the **type head's module** is lawful (not an orphan), even
    /// though the trait is defined elsewhere — the standard coherence-preserving
    /// home (R5). `Show` in `Display`, `Point` + `instance Show Point` in `Geometry`.
    #[test]
    fn an_instance_in_the_type_module_is_lawful() {
        let t = check_program(
            "module Display at services = \
               trait Show a { show : a -> String ! {gc} } in () \
             module Geometry at services = \
               type Point = Point(Int) in \
               instance Show Point { show = fn p => \"pt\" } in () \
             0",
        )
        .expect("an instance in the type head's module is lawful");
        assert_eq!(t.ty, Type::Int);
    }

    /// **Traits-v1 lowering limit — recursive constrained generic (`RN-E0246`).** A
    /// `let rec` whose body calls a trait method on its generic parameter AND
    /// recurses on it keeps that parameter abstract, so `Show ?a` is generalized
    /// onto `f`'s scheme. Its self-call was checked monomorphically (untagged), but
    /// the dict-passing transform would wrap `f`'s definition in a hidden leading
    /// dict parameter — so the self-call would apply the dict-expecting `f` to its
    /// value argument as if it were the dictionary (a miscompile / downstream
    /// compiler-bug panic). v1 rejects it loud and clear at the `Term::LetRec`
    /// elaboration point — a clean `RN-E0246`, NOT a wrong run result / panic.
    #[test]
    fn a_recursive_constrained_generic_is_rn_e0246() {
        let err = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             let rec f : a -> String ! {gc} = fn x => let _ = show x in f x in \
             f 5",
        )
        .expect_err("a recursive `let rec` carrying a trait constraint cannot lower in v1");
        assert_eq!(err.code(), "RN-E0246");
        assert_eq!(err.slug(), "trait.v1-unsupported");
        let TypeErr::TraitV1Unsupported { what } = &err else {
            panic!("expected TraitV1Unsupported, got {err:?}");
        };
        // The message names the construct (recursive `let rec`), the trait, and a
        // concrete workaround — never a bare "no".
        assert!(what.contains("let rec"), "names the construct: {what}");
        assert!(what.contains("Show"), "names the constraint: {what}");
        assert!(
            what.contains("non-recursive") || what.contains("monomorphize"),
            "offers a workaround: {what}"
        );
    }

    /// **Traits-v1 lowering limit — generic-instance use (`RN-E0246`).** Using a
    /// trait method at a type whose only matching instance has a **non-ground head**
    /// (`Show List[a]`) is well-typed and resolves, but v1 cannot build a runtime
    /// dictionary for a generic instance (its method closures would resolve a
    /// sub-dictionary to an unbound `$dict$…`, surfacing as a cryptic downstream
    /// error). v1 rejects *using* it at `resolve_instance` — the selected instance's
    /// head is non-ground — with a clean `RN-E0246`, NOT "unbound variable" / a
    /// misleading "needs a sum type". DECLARING the generic instance stays legal.
    #[test]
    fn a_generic_instance_use_is_rn_e0246() {
        // The instance head `Show List[a]` is non-ground; the body elaborates fine
        // (no abstract destructuring), so DECLARING it is legal — the gate fires on
        // the *use* at `resolve_instance`, not the declaration. (A body that needs
        // to destructure an abstract `a` would fail even earlier, at the declaration
        // body-check; the use-site gate is the clean point the brief specifies.)
        let err = check_src(
            "type List[a] = Nil | Cons(a, List[a]) in \
             trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             instance Show List[a] requires Show a { show = fn xs => \"list\" } in \
             show (Cons(7, Nil))",
        )
        .expect_err("a generic (non-ground-head) instance cannot be lowered in v1");
        assert_eq!(err.code(), "RN-E0246");
        assert_eq!(err.slug(), "trait.v1-unsupported");
        let TypeErr::TraitV1Unsupported { what } = &err else {
            panic!("expected TraitV1Unsupported, got {err:?}");
        };
        assert!(
            what.contains("generic instance"),
            "names the construct: {what}"
        );
        assert!(
            what.contains("ground-head") || what.contains("concrete"),
            "offers a workaround: {what}"
        );
    }

    /// **The discriminator is the HEAD, not the `requires`.** A ground-head instance
    /// with a *superclass*/context `requires` (`instance Ord Int requires Eq Int`,
    /// the §1.4 shape) must still resolve and run — `RN-E0246` keys on a non-ground
    /// head only. (Guards the regression that the new gate over-fires.)
    #[test]
    fn a_ground_head_instance_with_a_requires_still_resolves() {
        let t = check_src(
            "type Ordering = LT | EQ | GT in \
             trait Eq a { eq : a -> a -> Bool } in \
             trait Ord a requires Eq a { compare : a -> a -> Ordering ! {gc} } in \
             instance Eq Int { eq = fn x => fn y => true } in \
             instance Ord Int { compare = fn x => fn y => LT } in \
             let min2 = fn x => fn y => match compare x y with | LT => x | EQ => x | GT => y in \
             min2 3 7",
        )
        .expect("a ground-head `Ord Int` with a superclass `requires` resolves (not RN-E0246)");
        assert_eq!(t.ty, Type::Int);
    }

    /// The **dictionary evidence side-table** is populated for Sprint 3: resolving
    /// `show 5` records one `DictEvidence::Instance` for `Show Int`.
    #[test]
    fn resolution_records_dictionary_evidence_for_sprint3() {
        let _ = check_src(
            "trait Show a { show : a -> String ! {gc} } in \
             instance Show Int { show = fn x => \"n\" } in \
             show 5",
        )
        .expect("resolves");
        let evidence = take_dict_evidence();
        assert_eq!(evidence.len(), 1, "one discharged obligation");
        assert_eq!(evidence[0].constraint.trait_name, "Show");
        assert_eq!(evidence[0].constraint.ty, Type::Int);
        let DictEvidence::Instance {
            trait_name,
            head,
            subdicts,
        } = &evidence[0].evidence
        else {
            panic!(
                "expected an Instance dictionary, got {:?}",
                evidence[0].evidence
            );
        };
        assert_eq!(trait_name, "Show");
        assert_eq!(*head, Type::Int);
        assert!(subdicts.is_empty(), "Show has no superclasses");
    }

    /// The worked example's evidence embeds the **superclass sub-dictionary**:
    /// `Ord Int`'s evidence carries `Eq Int` as a sub-dict (O-T1 lean-embed).
    #[test]
    fn ord_int_evidence_embeds_its_eq_int_superdict() {
        let _ = check_src(
            "type Ordering = LT | EQ | GT in \
             trait Eq a { eq : a -> a -> Bool } in \
             trait Ord a requires Eq a { compare : a -> a -> Ordering ! {gc} } in \
             instance Eq Int { eq = fn x => fn y => true } in \
             instance Ord Int { compare = fn x => fn y => LT } in \
             let min2 = fn x => fn y => match compare x y with | LT => x | EQ => x | GT => y in \
             min2 3 7",
        )
        .expect("resolves");
        let evidence = take_dict_evidence();
        assert_eq!(evidence.len(), 1);
        let DictEvidence::Instance {
            trait_name,
            subdicts,
            ..
        } = &evidence[0].evidence
        else {
            panic!("expected an Instance dictionary");
        };
        assert_eq!(trait_name, "Ord");
        assert_eq!(
            subdicts.len(),
            1,
            "Ord Int embeds its Eq Int super-dictionary"
        );
        let DictEvidence::Instance {
            trait_name: sup, ..
        } = &subdicts[0]
        else {
            panic!("expected the Eq Int sub-dictionary");
        };
        assert_eq!(sup, "Eq");
    }

    // ── mutable heap references — `Ref[T]` / `st` (mutability §1–§2) ───────────

    fn has_label(row: &Row, want: &Label) -> bool {
        row.labels().any(|l| l == want)
    }

    /// `ref e : Ref[T] ! {gc}` — allocation, so the row is exactly `{gc}` (no `st`:
    /// `ref` allocates, it does not read/write). The type is `Ref[Int]`.
    #[test]
    fn ref_new_is_ref_of_t_and_carries_gc() {
        let t = check_src("ref 0").expect("`ref 0` type-checks");
        assert_eq!(t.ty, Type::Named("Ref".into(), vec![Type::Int]));
        assert_eq!(t.row, Row::single(Label::Gc), "ref allocates ⇒ {{gc}} only");
    }

    /// `!r : T ! {st}` and `r := v : Unit ! {st}` — the read/write carry the
    /// observable-mutation effect. The whole program `let r = ref 0 in !r` shows
    /// **both** `gc` (the alloc) and `st` (the read).
    #[test]
    fn deref_and_assign_carry_st() {
        let t = check_src("let r = ref 0 in !r").expect("deref type-checks");
        assert_eq!(t.ty, Type::Int);
        assert!(
            has_label(&t.row, &Label::Gc),
            "the `ref` alloc ⇒ gc: {}",
            t.row
        );
        assert!(
            has_label(&t.row, &Label::St),
            "the `!r` read ⇒ st: {}",
            t.row
        );

        // A write through a Ref-typed name: `r := !r + 1` is Unit ! {st} (plus gc).
        let w = check_src("let r = ref 0 in r := !r + 1")
            .expect("ref-assign through a Ref-typed name type-checks");
        assert_eq!(w.ty, Type::Unit);
        assert!(
            has_label(&w.row, &Label::St),
            "the `:=`/`!` ⇒ st: {}",
            w.row
        );
    }

    /// The headline gate program type-checks to `Int` and its row carries `st`
    /// (the read/write) and `gc` (the alloc): `let r = ref 0 in let _ = (r := !r +
    /// 41) in !r`. (Its value — 42 — is the JIT/AOT gate; this is the typing side.)
    #[test]
    fn the_counter_gate_program_types_with_st_and_gc() {
        let t = check_src("let r = ref 0 in let _ = (r := !r + 41) in !r")
            .expect("the counter program type-checks");
        assert_eq!(t.ty, Type::Int);
        assert!(has_label(&t.row, &Label::St), "row carries st: {}", t.row);
        assert!(has_label(&t.row, &Label::Gc), "row carries gc: {}", t.row);
    }

    /// A `Ref[Float]` round-trips through the type checker (the value gate is in the
    /// LLVM crate's float path). `let r = ref 1.5 in (r := !r +. 2.0); !r : Float`.
    #[test]
    fn ref_float_type_checks() {
        let t = check_src("let r = ref 1.5 in let _ = (r := !r + 2.0) in !r")
            .expect("a Float Ref type-checks");
        assert_eq!(t.ty, Type::Float);
        assert!(has_label(&t.row, &Label::St), "row carries st: {}", t.row);
    }

    /// **The escaping case (§7.2 `make_counter` shape) type-checks with `st`.** A
    /// returned `Ref[Int]` and a closure that reads/writes it — the cell escapes (no
    /// `withRef` seal yet, Sprint 2), so the closure's row keeps `st`, honestly. The
    /// whole thing is well-typed: this is the case Sprint 2's seal will discharge.
    #[test]
    fn a_returned_ref_and_closure_over_it_type_check_with_st() {
        // `let c = ref 0 in let tick = fn u => (let _ = (c := !c + 1) in !c) in (c, tick)`
        let t = check_src(
            "let c = ref 0 in \
             let tick = fn u => (let _ = (c := !c + 1) in !c) in \
             (c, tick)",
        )
        .expect("the make_counter shape type-checks (the Ref escapes, st stays)");
        // The result is a pair (Ref[Int], Unit -> Int ! {st}).
        let Type::Tuple(elems) = &t.ty else {
            panic!("expected a tuple, got {}", t.ty);
        };
        assert_eq!(elems[0], Type::Named("Ref".into(), vec![Type::Int]));
        let Type::Fun(_, ret, latent) = &elems[1] else {
            panic!(
                "second element should be the tick closure, got {}",
                elems[1]
            );
        };
        assert_eq!(**ret, Type::Int);
        assert!(
            has_label(latent, &Label::St),
            "the escaping closure's latent row keeps st: {latent}"
        );
    }

    /// A **pointer-typed** `Ref` is the clean deferred error `RN-E0247` (Sprint 3
    /// needs the GC write barrier), NEVER a miscompile. `ref "hi"` (a `Ref[String]`)
    /// and `ref [1,2,3]` (a `Ref[Array[Int]]`) are both rejected with the message
    /// steering to a scalar `Ref`.
    #[test]
    fn a_pointer_typed_ref_is_rejected_rn_e0247() {
        let err = check_src(r#"ref "hi""#).expect_err("a Ref[String] is deferred");
        assert_eq!(err.code(), "RN-E0247");
        assert_eq!(err.slug(), "ref.pointer-content");
        assert!(matches!(err, TypeErr::RefPointerContent { .. }));
        assert!(
            err.hint().is_some_and(|h| h.contains("scalar `Ref`")),
            "the hint steers to a scalar Ref"
        );

        let err = check_src("ref [1, 2, 3]").expect_err("a Ref[Array[Int]] is deferred");
        assert_eq!(err.code(), "RN-E0247");

        // A `Ref` of a sum/record is likewise deferred (a handle content cell).
        let err = check_src("type T = A | B in ref A").expect_err("a Ref[T] (sum) is deferred");
        assert_eq!(err.code(), "RN-E0247");
    }

    /// `ref` and `!` parse + type as PREFIX operators over an atom: `!r + 41` is
    /// `(!r) + 41`, and a deref binds an annotated lambda parameter to a `Ref`.
    #[test]
    fn deref_pins_an_unannotated_ref_parameter() {
        // `fn r => !r + 1` — `r` is inferred `Ref[Int]` from the deref + the `+ 1`.
        let t = check_src("fn r => !r + 1").expect("a deref pins the parameter to Ref[Int]");
        let Type::Fun(dom, cod, latent) = &t.ty else {
            panic!("expected a function, got {}", t.ty);
        };
        assert_eq!(**dom, Type::Named("Ref".into(), vec![Type::Int]));
        assert_eq!(**cod, Type::Int);
        assert!(
            has_label(latent, &Label::St),
            "the body reads ⇒ st in the arrow: {latent}"
        );
    }

    /// `x := e` on a plain (immutable, non-`Ref`) `let` binding is still
    /// `RN-E0245` — the dispatch only routes a `let mut` or a `Ref`-typed name.
    #[test]
    fn assign_to_an_immutable_non_ref_is_still_rn_e0245() {
        let err = check_src("let x = 0 in x := 1").expect_err("an immutable Int is not assignable");
        assert_eq!(err.code(), "RN-E0245");
    }
}
