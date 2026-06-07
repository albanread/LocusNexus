//! **T0 — the tag-completeness validator** (the sole memory-safety mechanism).
//!
//! With representation-polymorphic lowering moving from *boxing* to *tags*
//! (`docs/repr-poly-impl.md` §0), the boxing runtime backstop — `set_ptr`'s
//! `0xABCD` magic gate that caught a mis-stored scalar before it entered the
//! heap — is **gone** (D6: no magic on the trace path). A `Var` cell is a
//! genuine pointer cell in the collector's scanned range, and the evacuator
//! follows *any* word whose low bits are `10` with no validity check (the
//! GAP-010 hazard). So **a single missed coercion at a `Var` boundary is
//! non-deterministic heap corruption that survives green tests** — the precise
//! failure class the design exists to prevent.
//!
//! T0 is the gate that makes the compiler the *provably-exhaustive* owner of
//! that safety before any lowering is allowed to consume a coercion. It is a
//! post-elaboration pass that visits **every** site where a value enters or
//! leaves a `Var` cell — Construct field, App arg→var-param (both the concrete
//! arrow and the **non-arrow** path a prior audit found inserts none),
//! match-binder, `let` binding, and function return — and asserts a coercion
//! ([`Node::Coerce`]) is present and `Repr`-correct. A surviving mismatch is a
//! **compiler bug**, reported, not a warning. This is the analog of boxing's
//! retired D9, and tagging needs it *more*, because it is the only thing left.
//!
//! ## When it runs (the central design point)
//!
//! T0 runs **pre-zonk**, inside [`crate::sema::elaborate`] between
//! `elaborate_inner` and `zonk`. Zonk defaults every solved/residual
//! `Type::Var` to `Int` (D6), which would erase the very `Uniform`-slot signal
//! T0 reasons about: a polymorphic field's declared type reads `Var` (→ `repr`
//! `Uniform`) before zonk and `Int` (→ `Scalar`) after. So the check **must**
//! see the un-zonked tree. The coercion nodes are already present pre-zonk
//! (sema inserts them during `elaborate_inner`), and the unification store is
//! still live, so [`check_tags`] resolves value types through it exactly as the
//! insertion sites did — insertion and checking share one predicate,
//! [`Type::coercion`].
//!
//! ## What it consumes
//!
//! The boundary *slot* type — the declared field / parameter type, a
//! [`Type::Var`] at a polymorphic position — is the load-bearing input. It is
//! **recorded on the tree** (on [`Node::Coerce`] and on each [`Node::Construct`]
//! arg) precisely because the constructor registry it came from (`TYENV`) is
//! unwound by the time elaboration returns. *Sema is the authoritative model* —
//! T0 never re-pokes a registry; it reads what sema recorded.
//!
//! ## Scope this sprint
//!
//! T0 adds the **check**; `Coerce → tag/untag` lowering is live for the current
//! repr-poly slice. The two boundary classes that store into uniform cells —
//! **Construct field** and **App arg** — are validated in full, in **both**
//! directions:
//!   * *present* — every [`Node::Coerce`] is checked `Repr`-correct
//!     (`coercion(slot, value) == kind`), so a `Box` where `Unbox` is demanded
//!     is caught;
//!   * *missing* — at each Construct field and App argument the demanded
//!     coercion is **re-derived** from the recorded slot and the resolved value;
//!     if non-`None`, a matching `Coerce` child must be present.
//!
//! The **non-arrow App** path is deliberately not treated as a stored `Var` cell:
//! sema discovers a monomorphic arrow from the argument itself, so there is no
//! recorded uniform slot to check. Match binders now insert read-side `Untag`
//! when a tagged word is refined to a concrete scalar; `let`/return remain
//! no-ops *by construction* (the binder's / lambda codomain's type **is** the
//! value's type — slot equals value, no boundary). See the per-arm notes below.

use crate::sema::TypedBlockItem;
use crate::sema::{MatchArmT, Node, Typed, TypedHandler};
use crate::syntax::{Coercion, Type};
use crate::unify;

/// A T0 failure — a `Var` boundary whose coercion is missing or wrong. Carries
/// enough to render "compiler bug: <site> at this boundary".
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TagError {
    /// The enumerated entry/exit site (`docs/repr-poly-impl.md` §0).
    pub site: &'static str,
    /// What went wrong, with the boundary types.
    pub kind: TagErrorKind,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TagErrorKind {
    /// A coercion is **demanded** at this `Var` boundary (the slot/value reprs
    /// differ) but none — or the wrong child — is present. This is the missed
    /// tag that becomes silent heap corruption.
    Missing {
        slot: Type,
        value: Type,
        demanded: Coercion,
    },
    /// A coercion **is** present but the wrong kind — a `Box` where `Unbox` is
    /// demanded, or vice versa. Equally a bug: it would tag a handle / deref a
    /// scalar.
    WrongKind {
        slot: Type,
        value: Type,
        demanded: Coercion,
        found: Coercion,
    },
}

impl std::fmt::Display for TagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            TagErrorKind::Missing {
                slot,
                value,
                demanded,
            } => write!(
                f,
                "tag-completeness (compiler bug): {} — a `{demanded:?}` coercion is required to \
                 move a `{value}` across a `{slot}` (Var) cell boundary, but none is present",
                self.site
            ),
            TagErrorKind::WrongKind {
                slot,
                value,
                demanded,
                found,
            } => write!(
                f,
                "tag-completeness (compiler bug): {} — boundary `{slot}` <- `{value}` demands a \
                 `{demanded:?}` coercion but a `{found:?}` is present",
                self.site
            ),
        }
    }
}

/// Resolve a type's leading variable chain through the live unification store
/// (D6's `resolve`, *not* `zonk` — an unbound var stays a `Var`). This is the
/// exact operation sema's insertion sites use to read a value's representation,
/// so T0 re-derives the same coercion they inserted from.
fn resolve(t: &Type) -> Type {
    unify::with_store(|s| s.resolve_ty(t))
}

/// **Run T0 over a decorated, pre-zonk tree.** Returns the first boundary that
/// violates tag-completeness, or `Ok(())` if every `Var` boundary is coerced
/// and `Repr`-correct. Must be called *before* `zonk` (see module docs).
pub fn check_tags(t: &Typed) -> Result<(), TagError> {
    visit(t)
}

/// The recursive worker — an **exhaustive** match over [`Node`]. Each arm both
/// (a) applies the boundary check for that node's `Var` entry/exit sites and
/// (b) recurses into children. The exhaustiveness is load-bearing: a future
/// `Node` variant that can hold a `Var` cell must be classified here or the
/// compiler rejects the file (GAP-M1 — "sema is the authoritative model;
/// exhaustively walk the whole AST").
fn visit(t: &Typed) -> Result<(), TagError> {
    match &t.node {
        // ── leaves — no children, no Var boundary ───────────────────────────
        Node::Var(_)
        | Node::Int(_)
        | Node::Float(_)
        | Node::Bool(_)
        | Node::Unit
        | Node::Brk
        | Node::Str(_)
        | Node::Extern(..) => Ok(()),

        // ── a coercion node — the *present* direction, checked everywhere ────
        // Every `Coerce` in the tree must be `Repr`-correct: the coercion its
        // boundary demands (`coercion(slot, value)`) must equal the kind sema
        // stamped. A `Box` where `Unbox` is demanded is a bug. `slot`/`value`
        // are the declared boundary types sema recorded; resolving `value`
        // guards a still-unbound chain (it is already resolved at insertion, but
        // be defensive).
        Node::Coerce {
            kind,
            slot,
            value,
            inner,
        } => {
            let demanded = Type::coercion(slot, &resolve(value));
            if demanded != *kind {
                return Err(TagError {
                    site: "coercion node",
                    kind: TagErrorKind::WrongKind {
                        slot: slot.clone(),
                        value: value.clone(),
                        demanded,
                        found: *kind,
                    },
                });
            }
            visit(inner)
        }

        // ── Construct field — store into a Var cell (the canonical boundary) ─
        // For each arg, re-derive the demand from the DECLARED field slot (the
        // sum's type-param `Var`, recorded on the arg) and the value's resolved
        // type. If a coercion is demanded, the arg node must be that coercion.
        // This both re-proves the `Box` sema inserts and catches a future drop
        // of that insertion.
        Node::Construct { args, .. } => {
            for (arg, _layout, slot) in args {
                check_boundary("construct field", slot, arg)?;
                visit(arg)?;
            }
            Ok(())
        }

        // ── App argument → parameter cell — the second boundary class ────────
        // The slot is the callee's *declared* parameter type, read **exactly as
        // sema reads it** to decide a box: the literal domain of `fun.ty` when
        // `fun.ty` is *syntactically* a `Fun`. On that arrow path the callee is a
        // known function — an extern, or a `let`-generalised scheme instantiated
        // to `?n -> ?n` whose generic body expects a uniform parameter cell — so
        // a literal `Type::Var` domain is a genuine uniform slot and a scalar
        // argument must be boxed. T0 re-derives that demand and requires the box.
        //
        // The **non-arrow path** (`fun.ty` is a bare `Var`, e.g. `f` in
        // `fn f => f 1`) is deliberately *not* a boundary here: sema discovers a
        // *monomorphic* arrow by unifying the callee against a fresh skeleton and
        // pinning its domain to the argument, so the parameter cell is concrete,
        // not a uniform `Var` — no coercion is needed and sema inserts none. (The
        // adversary's C2 latent hazard — a non-arrow domain kept polymorphic
        // while the argument is scalar — cannot arise in the current elaborator,
        // because the domain's only constraint is the argument itself; it becomes
        // a checked site when a recorded slot makes the demand derivable, exactly
        // as for `Construct`.) Matching `fun.ty` un-resolved gives precisely this
        // split: a literal `Fun` is the arrow path, a `Var` is the non-arrow one.
        Node::App { fun, arg } => {
            if let Type::Fun(dom, _, _) = &fun.ty {
                check_boundary("app arg -> parameter", dom, arg)?;
            }
            visit(fun)?;
            visit(arg)
        }

        // ── match — the scrutinee, then each arm body ───────────────────────
        // Match-BINDER load coercions (`Untag` a tagged scalar field, `intern`
        // a handle field) are a T2 addition: sema today binds a matched field at
        // its refined concrete type with no coercion node, so there is nothing
        // *present* for T0 to validate and no recorded slot on `MatchArmT.binds`
        // to derive a *missing* demand from. The walk still visits every arm
        // body (so the structure is covered); the binder load boundary becomes a
        // checked site when T2 inserts the load-side matrix and records the
        // field slot. (`docs/repr-poly-impl.md` load matrix; adversary L1.)
        Node::Match { scrutinee, arms } => {
            visit(scrutinee)?;
            for MatchArmT { body, .. } in arms {
                visit(body)?;
            }
            Ok(())
        }

        // ── let binding — a store into the binder cell ──────────────────────
        // No coercion site **by construction**: a `let x = e` binds `x` at
        // exactly `e`'s type (Mono) or its generalization (Poly) — the slot type
        // *is* the value type, so `coercion(bound.ty, bound.ty)` is `None`. A
        // generic value stored into a differently-repr'd binder cell does not
        // arise in the surface today; this stays a no-op until it does (plan: the
        // `Let` row is a no-op until generic let-stored values exist). Visit both.
        Node::Let { bound, body, .. } | Node::LetTuple(_, bound, body) => {
            visit(bound)?;
            visit(body)
        }
        Node::Block { items, body } => {
            for item in items {
                match item {
                    TypedBlockItem::Let { bound, .. }
                    | TypedBlockItem::LetMut { bound, .. }
                    | TypedBlockItem::LetTuple { bound, .. } => visit(bound)?,
                }
            }
            visit(body)
        }

        // ── let mut binding / assignment — a store into a mutable scalar slot ──
        // No coercion site **by construction**: a `let mut x = e` binds `x` at
        // exactly `e`'s scalar type (mutability v1 is scalar-only), and `x := v`
        // demands `v : τ` equal to the slot, so neither crosses a repr boundary.
        // Visit the children.
        Node::LetMut { bound, body, .. } => {
            visit(bound)?;
            visit(body)
        }
        Node::Assign { value, .. } => visit(value),

        // ── `ref` / `!` / `:=` (heap `Ref[T]` cell) — no coercion site ──────────
        // No `Var` boundary by construction (mutability §1.1): the content cell is
        // a *scalar* `Ref` (the v1 gate rejects a pointer-typed cell), `ref e` /
        // `!r` / `r := v` all operate at the cell's exact scalar type, so nothing
        // crosses a repr boundary (no tag/untag/to_ptr) — exactly like `let mut`.
        // Visit the children.
        Node::RefNew { value, .. } => visit(value),
        Node::Deref { cell, .. } => visit(cell),
        Node::RefAssign { target, value, .. } => {
            visit(target)?;
            visit(value)
        }

        // ── function return — body flows into the arrow codomain ────────────
        // No coercion site **by construction**: a lambda's arrow codomain is
        // *built from* `body.ty` (and a `let rec`'s declared return type is
        // unified with the body), so the return slot equals the body type — no
        // boundary. Recorded explicitly as an enumerated §0 site; visit the body.
        Node::Lam { body, .. } => visit(body),

        // ── structural recursion (no Var boundary of their own) ─────────────
        Node::Bin(_, a, b)
        | Node::FloatMathBinary(_, a, b)
        | Node::Index(_, a, b)
        | Node::Poke(_, a, b) => {
            visit(a)?;
            visit(b)
        }
        Node::If(a, b, c)
        | Node::Fill(a, b, c)
        | Node::Copy(a, b, c)
        | Node::IndexSet(_, a, b, c)
        | Node::FloatMathTernary(_, a, b, c) => {
            visit(a)?;
            visit(b)?;
            visit(c)
        }
        Node::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            for (_, _, _, init) in vars {
                visit(init)?;
            }
            visit(cond)?;
            for step in steps {
                visit(step)?;
            }
            visit(result)
        }
        Node::VectorSelect {
            mask,
            then_value,
            else_value,
        } => {
            visit(mask)?;
            visit(then_value)?;
            visit(else_value)
        }
        Node::Cast(_, a)
        | Node::FloatMathUnary(_, a)
        | Node::MaskReduce(_, a)
        | Node::Perform { arg: a, .. }
        | Node::Quote(a)
        | Node::Splice(a)
        | Node::Genlet(a)
        | Node::Letloc(a)
        | Node::Peek(_, a)
        | Node::Len(a)
        | Node::Field(a, _)
        | Node::VectorSplat { value: a, .. }
        | Node::VectorExtract { vector: a, .. } => visit(a),
        Node::Tuple(es) | Node::ArrayLit { elems: es, .. } | Node::VectorLit { elems: es, .. } => {
            for e in es {
                visit(e)?;
            }
            Ok(())
        }
        Node::Record(fs) => {
            for (_, e) in fs {
                visit(e)?;
            }
            Ok(())
        }
        Node::ArrayGet { arr, idx, .. } => {
            visit(arr)?;
            visit(idx)
        }
        Node::ArraySet { arr, idx, val, .. } => {
            visit(arr)?;
            visit(idx)?;
            visit(val)
        }
        Node::VectorLoad { arr, idx, .. } => {
            visit(arr)?;
            visit(idx)
        }
        Node::VectorStore {
            arr, idx, value, ..
        } => {
            visit(arr)?;
            visit(idx)?;
            visit(value)
        }
        Node::Handle { scrutinee, handler } => {
            visit(scrutinee)?;
            let TypedHandler { ops, ret } = handler;
            for op in ops {
                visit(&op.body)?;
            }
            visit(&ret.body)
        }
    }
}

/// The shared boundary assertion: given the declared `slot` type and a `value`
/// node about to cross it, re-derive the demanded coercion and require the node
/// to carry it. `None` demanded ⇒ the node must **not** be a bogus coercion (a
/// `Coerce` where none is needed is itself a layering bug, caught by the
/// `Coerce`-node arm); a non-`None` demand ⇒ the node must be a `Coerce` of the
/// matching kind. Shares [`Type::coercion`] with the insertion sites, so insert
/// and check can never drift.
fn check_boundary(site: &'static str, slot: &Type, value_node: &Typed) -> Result<(), TagError> {
    let value = resolve(&value_node.ty);
    let demanded = Type::coercion(slot, &value);
    if demanded == Coercion::None {
        // Nothing required here. (If the node *is* a `Coerce`, its own arm
        // verifies it is internally consistent; a needless coercion would show
        // up there as `coercion(slot,value) == None != kind`.)
        return Ok(());
    }
    // A coercion is demanded — the child must be exactly it.
    match &value_node.node {
        Node::Coerce { kind, .. } if *kind == demanded => Ok(()),
        _ => Err(TagError {
            site,
            kind: TagErrorKind::Missing {
                slot: slot.clone(),
                value,
                demanded,
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{Ctx, Sig, Stage};
    use crate::sema::elaborate;
    use crate::syntax::{TyVarId, ValueLayout};

    /// A leaf `Typed` node carrying a ground type (no store needed to resolve).
    fn leaf(ty: Type, node: Node) -> Typed {
        Typed {
            ty,
            row: crate::syntax::Row::pure(),
            stage: 0,
            layout_known: true,
            node,
        }
    }

    fn var() -> Type {
        Type::Var(TyVarId(0))
    }

    /// `elaborate` runs T0 internally (and panics on a bug). A clean run means
    /// the whole program is tag-complete.
    fn el(stage: Stage, src: &str) -> Typed {
        let term = crate::parse::parse(src).expect("parses");
        elaborate(&Sig::new(), &Ctx::new(), stage, &term).expect("type-checks")
    }

    // ── direct `check_tags` unit tests over hand-built trees ────────────────

    #[test]
    fn a_construct_with_the_required_tag_passes() {
        // A scalar `Int` into a `Var` field, correctly wrapped in `Coerce{Tag}`:
        // the boundary is satisfied. (`slot` = the declared param Var; `value` =
        // the scalar.) This is the shape sema produces for `Cons(1, _)`. The
        // recorded layout is the traced **word cell** the field is stored into.
        crate::unify::reset_store();
        let inner = leaf(Type::Int, Node::Int(1));
        let coerced = leaf(
            Type::Int,
            Node::Coerce {
                kind: Coercion::Tag,
                slot: var(),
                value: Type::Int,
                inner: Box::new(inner),
            },
        );
        let cons = leaf(
            Type::Named("List".into(), vec![Type::Int]),
            Node::Construct {
                tag: 1,
                args: vec![(coerced, ValueLayout::word_cell(), var())],
            },
        );
        assert_eq!(check_tags(&cons), Ok(()));
    }

    #[test]
    fn a_construct_missing_its_tag_is_a_reported_compiler_bug() {
        // The same boundary — `Int` into a `Var` field — but the `Tag` was NOT
        // inserted (the arg is a bare `Int`). This is the missed tag that would
        // lay a raw scalar in a scanned `Var` cell; T0 reports it.
        crate::unify::reset_store();
        let bare = leaf(Type::Int, Node::Int(1));
        let cons = leaf(
            Type::Named("List".into(), vec![Type::Int]),
            Node::Construct {
                tag: 1,
                args: vec![(bare, ValueLayout::word_cell(), var())],
            },
        );
        let err = check_tags(&cons).expect_err("a missing tag at a Var field is a T0 failure");
        assert!(
            matches!(
                err.kind,
                TagErrorKind::Missing {
                    demanded: Coercion::Tag,
                    ..
                }
            ),
            "expected a Missing(Tag) report, got {err:?}"
        );
        assert_eq!(err.site, "construct field");
    }

    #[test]
    fn a_wrong_kind_coercion_is_caught() {
        // A `Coerce{Untag}` sitting where the boundary `Var <- Int` demands `Tag`
        // — an untag where a tag was needed. Equally a bug.
        crate::unify::reset_store();
        let inner = leaf(Type::Int, Node::Int(1));
        let mis = leaf(
            Type::Int,
            Node::Coerce {
                kind: Coercion::Untag,
                slot: var(),
                value: Type::Int,
                inner: Box::new(inner),
            },
        );
        let err = check_tags(&mis).expect_err("an Untag where Tag is demanded is a T0 failure");
        assert!(
            matches!(
                err.kind,
                TagErrorKind::WrongKind {
                    demanded: Coercion::Tag,
                    found: Coercion::Untag,
                    ..
                }
            ),
            "expected WrongKind(Tag vs Untag), got {err:?}"
        );
    }

    #[test]
    fn a_monomorphic_construct_needs_no_coercion() {
        // A field declared at a concrete scalar type (`Int`, not a `Var`) takes a
        // scalar verbatim — no boundary, no coercion, and T0 must not demand one.
        crate::unify::reset_store();
        let arg = leaf(Type::Int, Node::Int(1));
        let cons = leaf(
            Type::Named("Box".into(), vec![]),
            Node::Construct {
                tag: 0,
                args: vec![(arg, ValueLayout::scalar_cell(), Type::Int)],
            },
        );
        assert_eq!(check_tags(&cons), Ok(()));
    }

    // ── end-to-end through `elaborate` (the real gate) ──────────────────────

    #[test]
    fn elaborate_passes_t0_on_a_boxed_construct() {
        // `Cons(1, Nil)` boxes the scalar into the `a`-field; the whole program
        // is tag-complete, so `elaborate` (which runs T0) returns cleanly.
        let t = el(0, "type List[a] = Nil | Cons(a, List[a]) in Cons(1, Nil)");
        assert_eq!(t.ty, Type::Named("List".into(), vec![Type::Int]));
    }

    #[test]
    fn elaborate_passes_t0_on_a_boxed_application() {
        // `id 5` boxes the scalar at the App-arrow boundary (id's generic body
        // expects a uniform parameter cell); T0 verifies the box is present.
        let _ = el(0, "let id = fn x => x in id 5");
    }

    #[test]
    fn non_arrow_application_is_not_a_false_positive() {
        // `fn f => f 1` discovers a *monomorphic* arrow `Int -> Int` for `f` via
        // the non-arrow App path: the parameter cell is concrete, not a uniform
        // `Var`, so no coercion is needed and none is present. T0 must NOT flag
        // it (the regression guard for the non-arrow handling).
        let _ = el(0, "fn f => f 1");
    }

    #[test]
    fn higher_rank_application_through_the_non_arrow_path_is_tag_complete() {
        // The non-arrow App path discovers each callee's arrow by pinning its
        // domain to the argument, so even higher-rank shapes lower with concrete
        // parameter cells — no uncoerced `Var` boundary. T0 must pass all of
        // these (they exercise the non-arrow path repeatedly). A regression here
        // would mean the path started leaving a real boxing demand uninserted.
        for src in [
            "(fn f => f) (fn x => x) 3",
            "let ap = fn g => fn y => g y in ap (fn x => x) 3",
            "let twice = fn g => fn y => g (g y) in twice (fn x => x) 3",
        ] {
            let _ = el(0, src);
        }
    }

    #[test]
    fn elaborate_passes_t0_on_the_generic_recursive_body() {
        // A generic `let rec` whose recursive case `Cons(h, acc)` copies an
        // already-`Var`-typed `h` into a `Var` field: a Var↔Var boundary, so no
        // coercion is demanded (passthrough). T0 passes — the generic body is
        // uniformly polymorphic, with no scalar crossing a Var cell.
        let _ = el(
            0,
            "type List[a] = Nil | Cons(a, List[a]) in \
             let rec cp : List[a] -> List[a] ! {gc} = \
               fn xs: List[a] => match xs with | Nil => Nil | Cons(h, t) => Cons(h, cp t) \
             in cp (Cons(1, Nil))",
        );
    }
}
