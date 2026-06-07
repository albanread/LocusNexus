//! Unification — **types and effect rows**, with Rémy/OCaml **levels**.
//!
//! This is the testable core of the polymorphism arc
//! ([`../../docs/polymorphism-impl.md`], "The algorithm"). It owns:
//!
//!   * [`UnifStore`] — two union-find arrays (one for [`Type`] cells, one for
//!     [`Row`] tails), each cell an `Unbound{level}` or a `Bound(_)`
//!     forwarding, plus a `current_level`.
//!   * [`unify`] — first-order type unification with **occurs-and-lower** done
//!     in one walk: it both rejects the infinite type *and* lowers every var in
//!     the structure to the binding var's level. Arrows unify domain, codomain,
//!     **and the latent row** — the one thing beyond textbook HM.
//!   * [`unify_row`] — idempotent set-rows with an open tail: the four cases
//!     (both closed / one closed / both open) from the doc, with a **fresh** tail
//!     in the both-open case (essential for commutativity & associativity) and a
//!     **row occurs-check**.
//!   * [`zonk`] — replace every solved var by its solution and **default** the
//!     residue (D6): an unbound type var → `Int`, an unbound row tail → the
//!     closed empty row. After zonk a [`Type::Var`] cannot survive into IR/stage.
//!
//! **Threading (D2).** The core functions take an explicit `&mut UnifStore`, so
//! the oracle can spin up fresh stores trivially. `elaborate` reaches the store
//! through a thread-local ([`with_store`]), mirroring sema's `TYENV`.
//!
//! **S1 is monomorphic.** The checker never *creates* a type var in S1 (every
//! type is already concrete — lambdas are annotated, `let rec` is annotated),
//! so `unify` is only ever asked to equate two ground types, where it succeeds
//! exactly when `==` did and fails otherwise: a provable refactor (D5). The full
//! machinery is here so S2 (schemes, generalize/instantiate) layers on without
//! re-touching it, and so the oracle below can exercise it with synthetic vars.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use crate::check::Scheme;
use crate::syntax::{Constraint, Label, Row, RowVarId, TyVarId, Type};

/// A type-variable cell.
#[derive(Clone, Debug)]
enum TyVarState {
    /// Not yet solved; born at this `let`-depth (its generalization level).
    Unbound { level: u32 },
    /// Solved: forwards to this type (which may itself contain variables).
    Bound(Type),
}

/// A row-variable (tail) cell — the row analogue of [`TyVarState`].
#[derive(Clone, Debug)]
enum RowVarState {
    Unbound { level: u32 },
    Bound(Row),
}

/// The unification state: type cells, row cells, and the current `let`-depth.
///
/// `current_level` starts at 0 and **only `let` moves it** (S2 will: `+1`
/// entering the RHS, `-1` leaving). `fresh_ty`/`fresh_row` stamp a new var with
/// `current_level`; generalization (S2) is then the O(1) test `level >
/// current_level`. In S1 the level stays 0 throughout and nothing generalizes.
#[derive(Clone, Debug, Default)]
pub struct UnifStore {
    tys: Vec<TyVarState>,
    rows: Vec<RowVarState>,
    current_level: u32,
}

/// The outcome of a failed unification — enough to render a real diagnostic.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UnifyErr {
    /// Two incompatible structured types (`Int` vs `Bool`, arrow vs tuple, …).
    TypeMismatch(Type, Type),
    /// The occurs-check tripped: binding `a` would make an infinite type.
    OccursType(TyVarId, Type),
    /// Two closed rows that cannot be made equal — `only_left` are labels the
    /// left has that the right (closed) forbids, and vice-versa. This is the
    /// **pinned-boundary rejection** (case A).
    RowMismatch {
        only_left: BTreeSet<Label>,
        only_right: BTreeSet<Label>,
    },
    /// A closed row asked to absorb labels it does not have (cases B/C): the
    /// open side performs effects the pinned side forbids.
    RowClosed { offending: BTreeSet<Label> },
    /// The row occurs-check tripped: a tail would contain itself (infinite row).
    OccursRow(RowVarId),
    /// **D5/D3 representation-kind violation (T1)**: a type variable was unified
    /// with a `Wide` type (`Float`/`Float32`/`Pair`/`Quad`/`Oct`) under the
    /// current conservative guard. This protects traced `Var` word cells while
    /// the compiler still lacks use-inferred kind constraints and a D3-safe
    /// generic float call ABI.
    WideTypeVar(TyVarId, Type),
}

impl UnifStore {
    /// A fresh, empty store at level 0.
    pub fn new() -> UnifStore {
        UnifStore::default()
    }

    /// The current generalization level.
    pub fn level(&self) -> u32 {
        self.current_level
    }

    /// Enter a deeper `let` level (S2). Kept here so the level discipline lives
    /// with the store; S1 never calls it.
    pub fn enter_level(&mut self) {
        self.current_level += 1;
    }

    /// Leave a `let` level (S2). Symmetric to [`enter_level`].
    pub fn leave_level(&mut self) {
        self.current_level -= 1;
    }

    /// A fresh **type** variable stamped with the current level.
    pub fn fresh_ty(&mut self) -> Type {
        let id = TyVarId(self.tys.len() as u32);
        self.tys.push(TyVarState::Unbound {
            level: self.current_level,
        });
        Type::Var(id)
    }

    /// A fresh **row** tail variable stamped with the current level.
    pub fn fresh_row(&mut self) -> RowVarId {
        let id = RowVarId(self.rows.len() as u32);
        self.rows.push(RowVarState::Unbound {
            level: self.current_level,
        });
        id
    }

    // ── resolution (union-find "find") ──────────────────────────────────

    /// Follow a type's leading variable-chain to either a non-variable type or
    /// an **unbound** `Type::Var`. (Shallow: the returned structure may still
    /// contain variables one level down — that is intentional, unification
    /// recurses.)
    pub fn resolve_ty(&self, t: &Type) -> Type {
        let mut cur = t.clone();
        while let Type::Var(id) = cur {
            match &self.tys[id.0 as usize] {
                TyVarState::Bound(inner) => cur = inner.clone(),
                TyVarState::Unbound { .. } => return Type::Var(id),
            }
        }
        cur
    }

    /// Resolve a row's tail graph: returns `(labels, tails)` where `labels`
    /// accumulates every concrete label found along the chain and `tails` are
    /// the final unbound tails. A single-tail S1 row still resolves to zero or
    /// one tail; S4+ unions may preserve several independent tails.
    fn resolve_row(&self, r: &Row) -> (BTreeSet<Label>, BTreeSet<RowVarId>) {
        let mut labels = r.label_set().clone();
        let mut pending: Vec<RowVarId> = r.tail_set().iter().copied().collect();
        let mut tails = BTreeSet::new();
        let mut seen_bound = BTreeSet::new();
        while let Some(id) = pending.pop() {
            match &self.rows[id.0 as usize] {
                RowVarState::Bound(inner) => {
                    if !seen_bound.insert(id) {
                        continue;
                    }
                    labels.extend(inner.label_set().iter().cloned());
                    pending.extend(inner.tail_set().iter().copied());
                }
                RowVarState::Unbound { .. } => {
                    tails.insert(id);
                }
            }
        }
        (labels, tails)
    }

    fn ty_level(&self, id: TyVarId) -> u32 {
        match &self.tys[id.0 as usize] {
            TyVarState::Unbound { level } => *level,
            // A bound var has no level of its own; callers only ask of unbound.
            TyVarState::Bound(_) => u32::MAX,
        }
    }

    fn row_level(&self, id: RowVarId) -> u32 {
        match &self.rows[id.0 as usize] {
            RowVarState::Unbound { level } => *level,
            RowVarState::Bound(_) => u32::MAX,
        }
    }

    fn lower_ty(&mut self, id: TyVarId, to: u32) {
        if let TyVarState::Unbound { level } = &mut self.tys[id.0 as usize] {
            if *level > to {
                *level = to;
            }
        }
    }

    fn lower_row(&mut self, id: RowVarId, to: u32) {
        if let RowVarState::Unbound { level } = &mut self.rows[id.0 as usize] {
            if *level > to {
                *level = to;
            }
        }
    }
}

// ── type unification ────────────────────────────────────────────────────

/// Unify two types, solving variables in `store`. On success the two types are
/// equal under the resulting substitution; on failure nothing meaningful is
/// left bound (the caller turns the error into a typing diagnostic).
///
/// Variables: `Var(a) ~ Var(b)` binds younger→older keeping the **min** level;
/// `Var(a) ~ t` runs **occurs-and-lower** then binds. Arrows recurse on domain,
/// codomain **and latent row**. Other structured types recurse structurally;
/// base types succeed iff equal.
pub fn unify(store: &mut UnifStore, a: &Type, b: &Type) -> Result<(), UnifyErr> {
    let ra = store.resolve_ty(a);
    let rb = store.resolve_ty(b);
    match (&ra, &rb) {
        // Same unbound variable — already equal.
        (Type::Var(x), Type::Var(y)) if x == y => Ok(()),

        // Two distinct variables: bind the **younger** (higher id) to the older,
        // and lower the survivor to the min of the two levels.
        (Type::Var(x), Type::Var(y)) => {
            let (older, younger) = if x.0 <= y.0 { (*x, *y) } else { (*y, *x) };
            let lvl = store.ty_level(older).min(store.ty_level(younger));
            store.lower_ty(older, lvl);
            store.tys[younger.0 as usize] = TyVarState::Bound(Type::Var(older));
            Ok(())
        }

        // Variable vs structure (either order): occurs-and-lower, then bind.
        (Type::Var(x), _) => bind_ty(store, *x, &rb),
        (_, Type::Var(y)) => bind_ty(store, *y, &ra),

        // The arrow — domain, codomain, AND latent row.
        (Type::Fun(a1, b1, r1), Type::Fun(a2, b2, r2)) => {
            unify(store, a1, a2)?;
            unify(store, b1, b2)?;
            unify_row(store, r1, r2)
        }

        // `Code[A ! E]` — element type and object row.
        (Type::Code(t1, r1), Type::Code(t2, r2)) => {
            unify(store, t1, t2)?;
            unify_row(store, r1, r2)
        }

        (Type::Array(e1), Type::Array(e2)) => unify(store, e1, e2),

        (Type::Vector(s1, e1), Type::Vector(s2, e2)) if s1 == s2 => unify(store, e1, e2),
        (Type::Mask(s1), Type::Mask(s2)) if s1 == s2 => Ok(()),

        (Type::Tuple(ts1), Type::Tuple(ts2)) => {
            if ts1.len() != ts2.len() {
                return Err(UnifyErr::TypeMismatch(ra.clone(), rb.clone()));
            }
            for (x, y) in ts1.iter().zip(ts2) {
                unify(store, x, y)?;
            }
            Ok(())
        }

        (Type::Record(fs1), Type::Record(fs2)) => {
            if fs1.len() != fs2.len() {
                return Err(UnifyErr::TypeMismatch(ra.clone(), rb.clone()));
            }
            // Records are kept sorted by name (sema's canonical layout), so a
            // positional walk is a name-and-type walk.
            for ((n1, t1), (n2, t2)) in fs1.iter().zip(fs2) {
                if n1 != n2 {
                    return Err(UnifyErr::TypeMismatch(ra.clone(), rb.clone()));
                }
                unify(store, t1, t2)?;
            }
            Ok(())
        }

        // Base types: equal-or-fail.
        (Type::Int, Type::Int)
        | (Type::Float, Type::Float)
        | (Type::Float32, Type::Float32)
        | (Type::Bool, Type::Bool)
        | (Type::Unit, Type::Unit)
        | (Type::Str, Type::Str)
        | (Type::I32, Type::I32)
        | (Type::U32, Type::U32)
        | (Type::Ptr, Type::Ptr) => Ok(()),

        // Nominal types (D8): **same name + same arity**, then unify each type
        // argument **positionally**. `List[Int] ~ List[Bool]` unifies down to
        // `Int ~ Bool` and fails; `List[?a] ~ List[Int]` solves `?a := Int` (the
        // refinement that makes `match` work). A monomorphic sum is `args == []`,
        // so this reduces to the old name-only equality byte-for-byte.
        (Type::Named(n1, a1), Type::Named(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
            for (x, y) in a1.iter().zip(a2) {
                unify(store, x, y)?;
            }
            Ok(())
        }

        _ => Err(UnifyErr::TypeMismatch(ra, rb)),
    }
}

/// Bind variable `id` to the structured type `t`: one walk that (i) fails the
/// **occurs-check** if `t` mentions `id`, and (ii) **lowers** every unbound var
/// in `t` to `id`'s level. Then `tys[id] := t`.
fn bind_ty(store: &mut UnifStore, id: TyVarId, t: &Type) -> Result<(), UnifyErr> {
    // **D5 / T1 — current conservative kind chokepoint.** The ratified D3 rule is
    // use-inferred: reject `Wide` only when a value reaches a traced `Var` word
    // cell. The compiler still lacks that per-variable kind constraint plus a
    // D3-safe generic float call ABI, so for now every `Var ~ Wide` flows through
    // this guard. Resolve first so a `Var`-chain that *forwards* to a wide type
    // is also caught. Checked before the occurs walk and the store write, so a
    // rejected binding leaves the store untouched.
    let solved = store.resolve_ty(t);
    if solved.is_wide() {
        return Err(UnifyErr::WideTypeVar(id, solved));
    }
    let level = store.ty_level(id);
    occurs_and_lower_ty(store, id, level, t)?;
    store.tys[id.0 as usize] = TyVarState::Bound(t.clone());
    Ok(())
}

/// The single occurs-and-lower walk of `t` against variable `id` at `level`.
fn occurs_and_lower_ty(
    store: &mut UnifStore,
    id: TyVarId,
    level: u32,
    t: &Type,
) -> Result<(), UnifyErr> {
    match store.resolve_ty(t) {
        Type::Var(v) => {
            if v == id {
                return Err(UnifyErr::OccursType(id, t.clone()));
            }
            store.lower_ty(v, level);
            Ok(())
        }
        Type::Fun(a, b, r) => {
            occurs_and_lower_ty(store, id, level, &a)?;
            occurs_and_lower_ty(store, id, level, &b)?;
            occurs_and_lower_row(store, level, &r)
        }
        Type::Code(t2, r) => {
            occurs_and_lower_ty(store, id, level, &t2)?;
            occurs_and_lower_row(store, level, &r)
        }
        Type::Array(e) => occurs_and_lower_ty(store, id, level, &e),
        Type::Vector(_, e) => occurs_and_lower_ty(store, id, level, &e),
        Type::Mask(_) => Ok(()),
        Type::Tuple(ts) => {
            for x in &ts {
                occurs_and_lower_ty(store, id, level, x)?;
            }
            Ok(())
        }
        Type::Record(fs) => {
            for (_, x) in &fs {
                occurs_and_lower_ty(store, id, level, x)?;
            }
            Ok(())
        }
        // A nominal type's **arguments** carry variables (D8) — recurse into them
        // (or `a` could hide in `List[a]` and escape both the occurs-check and the
        // level-lowering: the highest-risk S3 omission).
        Type::Named(_, args) => {
            for x in &args {
                occurs_and_lower_ty(store, id, level, x)?;
            }
            Ok(())
        }
        // Base types: no variables, nothing to lower.
        Type::Int
        | Type::Float
        | Type::Float32
        | Type::Bool
        | Type::Unit
        | Type::Str
        | Type::I32
        | Type::U32
        | Type::Ptr => Ok(()),
    }
}

/// Lower every unbound tail reachable from row `r` to `level` (the row half of
/// occurs-and-lower; rows have no in-type occurs hazard against a *type* var).
fn occurs_and_lower_row(store: &mut UnifStore, level: u32, r: &Row) -> Result<(), UnifyErr> {
    let (_, tails) = store.resolve_row(r);
    for id in tails {
        store.lower_row(id, level);
    }
    Ok(())
}

// ── row unification ─────────────────────────────────────────────────────

/// Unify two effect rows (idempotent sets + open tails). The original S1 four
/// cases still apply directly when each side has zero or one tail. Multi-tail
/// rows arise from effect accumulation (`ρ_f ∪ ρ_g`); the solver preserves the
/// principled single-tail cases and handles equal resolved tail sets directly.
///
/// Let `shared = L1∩L2`, `only1 = L1\L2`, `only2 = L2\L1` after fully resolving
/// both tails. Shared labels match for free (idempotent — no multiplicity).
///
/// * **A — both closed:** succeed iff `only1` and `only2` are both empty, else
///   `RowMismatch` (the pinned-boundary rejection — this is the old `==`).
/// * **B — left closed, right open:** `only2` must be empty (the right would
///   else perform what a pinned left forbids); bind `ρ2 := {only1}` closed.
/// * **C — left open, right closed:** symmetric to B.
/// * **D — both open:** a **fresh** `ρ`; bind `ρ1 := {only2 | ρ}` and
///   `ρ2 := {only1 | ρ}`. The fresh `ρ` is what makes the operation commutative
///   and associative; reusing a side's tail would not. Edge: `ρ1 == ρ2` ⇒
///   succeed iff `only1 ∪ only2` is empty.
pub fn unify_row(store: &mut UnifStore, r1: &Row, r2: &Row) -> Result<(), UnifyErr> {
    let (l1, t1) = store.resolve_row(r1);
    let (l2, t2) = store.resolve_row(r2);

    let only1: BTreeSet<Label> = l1.difference(&l2).cloned().collect();
    let only2: BTreeSet<Label> = l2.difference(&l1).cloned().collect();

    if t1 == t2 {
        return if only1.is_empty() && only2.is_empty() {
            Ok(())
        } else {
            Err(UnifyErr::RowMismatch {
                only_left: only1,
                only_right: only2,
            })
        };
    }

    match (t1.len(), t2.len()) {
        // A — both closed.
        (0, 0) => {
            if only1.is_empty() && only2.is_empty() {
                Ok(())
            } else {
                Err(UnifyErr::RowMismatch {
                    only_left: only1,
                    only_right: only2,
                })
            }
        }

        // B — left closed, right open: the right must not need labels the closed
        // left lacks; the right's tail soaks up the left's surplus.
        (0, 1) => {
            if !only2.is_empty() {
                return Err(UnifyErr::RowClosed { offending: only2 });
            }
            let p2 = *t2.iter().next().expect("one tail");
            bind_row(store, p2, &Row::with_tail(only1, None))
        }

        // C — left open, right closed: symmetric.
        (1, 0) => {
            if !only1.is_empty() {
                return Err(UnifyErr::RowClosed { offending: only1 });
            }
            let p1 = *t1.iter().next().expect("one tail");
            bind_row(store, p1, &Row::with_tail(only2, None))
        }

        // D — both open.
        (1, 1) => {
            let p1 = *t1.iter().next().expect("one tail");
            let p2 = *t2.iter().next().expect("one tail");
            let fresh = store.fresh_row();
            // ρ1 must carry what only ρ2's side had, plus the shared fresh
            // tail; and symmetrically for ρ2.
            bind_row(store, p1, &Row::open(only2, fresh))?;
            bind_row(store, p2, &Row::open(only1, fresh))
        }

        // Multi-tail against a closed row is only decidable without arbitrary
        // label splitting when there is no surplus to distribute. In that case
        // the open tails are forced pure. If surplus labels remain, later row
        // normalization needs a richer delayed-constraint representation.
        (0, _) => {
            if !only2.is_empty() {
                return Err(UnifyErr::RowClosed { offending: only2 });
            }
            if !only1.is_empty() {
                return Err(UnifyErr::RowClosed { offending: only1 });
            }
            for p in t2 {
                bind_row(store, p, &Row::pure())?;
            }
            Ok(())
        }
        (_, 0) => {
            if !only1.is_empty() {
                return Err(UnifyErr::RowClosed { offending: only1 });
            }
            if !only2.is_empty() {
                return Err(UnifyErr::RowClosed { offending: only2 });
            }
            for p in t1 {
                bind_row(store, p, &Row::pure())?;
            }
            Ok(())
        }

        // For genuinely different multi-tail sets, bridge each side through a
        // fresh shared residual. This is conservative for equality demands and
        // keeps same-tail-set accumulation (`ρ_f ∪ ρ_g`) independent in the
        // common compose path.
        _ => {
            let fresh = store.fresh_row();
            let left_extra: Vec<RowVarId> = t1.difference(&t2).copied().collect();
            let right_extra: Vec<RowVarId> = t2.difference(&t1).copied().collect();
            for p in left_extra {
                bind_row(store, p, &Row::open(only2.clone(), fresh))?;
            }
            for p in right_extra {
                bind_row(store, p, &Row::open(only1.clone(), fresh))?;
            }
            Ok(())
        }
    }
}

/// Bind row tail `id` to row `r` (`unify_row_tail`): resolve forwarding, run the
/// **row occurs-check** (`r`'s tail chain must not be `id`), level-lower the
/// unbound tail in `r`, then `rows[id] := r`.
fn bind_row(store: &mut UnifStore, id: RowVarId, r: &Row) -> Result<(), UnifyErr> {
    let (labels, tails) = store.resolve_row(r);
    if tails.contains(&id) {
        // {… | id} bound to id would be an infinite row.
        return Err(UnifyErr::OccursRow(id));
    }
    for t in &tails {
        let level = store.row_level(id);
        store.lower_row(*t, level);
    }
    store.rows[id.0 as usize] = RowVarState::Bound(Row::with_tails(labels, tails));
    Ok(())
}

// ── zonk (D6) ───────────────────────────────────────────────────────────

/// Fully resolve a type, substituting solved variables and **defaulting** the
/// residue: an unbound type var → `Int`, an unbound row tail → the closed empty
/// row. After this no [`Type::Var`] remains (D6) — safe for IR/stage.
pub fn zonk_ty(store: &UnifStore, t: &Type) -> Type {
    match store.resolve_ty(t) {
        // Unbound type variable: default to `Int` (representation-irrelevant
        // under the uniform `i64` model — D6).
        Type::Var(_) => Type::Int,
        Type::Fun(a, b, r) => Type::Fun(
            Box::new(zonk_ty(store, &a)),
            Box::new(zonk_ty(store, &b)),
            zonk_row(store, &r),
        ),
        Type::Code(t2, r) => Type::Code(Box::new(zonk_ty(store, &t2)), zonk_row(store, &r)),
        Type::Array(e) => Type::Array(Box::new(zonk_ty(store, &e))),
        Type::Vector(shape, e) => Type::Vector(shape, Box::new(zonk_ty(store, &e))),
        Type::Mask(shape) => Type::Mask(shape),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|x| zonk_ty(store, x)).collect()),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, x)| (n.clone(), zonk_ty(store, x)))
                .collect(),
        ),
        // A nominal type's **arguments** must be zonked too (D8): an unrefined
        // `List[?a]` becomes `List[Int]` (the `?a` defaulting to `Int`), never a
        // residual `Var` in the args. (The oracle's "zonk leaves no Var in Named
        // args" property guards this.)
        Type::Named(n, args) => Type::Named(n, args.iter().map(|x| zonk_ty(store, x)).collect()),
        other => other,
    }
}

/// Resolve a type's leading variable chain without applying D6 defaults.
///
/// This is for representation decisions that must distinguish "solved to a
/// concrete pointer/scalar type" from "still genuinely representation
/// polymorphic". Unlike [`zonk_ty`], an unbound variable remains `Type::Var`.
pub fn resolve_ty(store: &UnifStore, t: &Type) -> Type {
    store.resolve_ty(t)
}

/// Fully resolve a row: collapse the tail chain to its concrete labels, and
/// **drop an unbound tail** (default to closed — D6). The result is always a
/// closed row, so zonked output renders byte-for-byte like the monomorphic
/// checker's.
pub fn zonk_row(store: &UnifStore, r: &Row) -> Row {
    let (labels, _tail) = store.resolve_row(r);
    Row::with_tail(labels, None)
}

// ── instantiate / generalize (S2) ───────────────────────────────────────

/// **Instantiate** a scheme `∀ᾱ ρ̄. A` at the use site: replace each quantified
/// type/row variable with a **fresh** one at the *current* level, consistently
/// across the body, and return the resulting type
/// ([`polymorphism-impl.md`], "Generalize/instantiate").
///
/// Each quantified `αᵢ`/`ρⱼ` maps to one fresh variable shared everywhere it
/// occurs (so `∀a. a -> a` instantiates to `?n -> ?n`, the same `?n`), and two
/// separate calls draw disjoint fresh variables (so `id 1` and `id true`
/// constrain independent copies). A scheme that quantifies nothing instantiates
/// to its body unchanged — the monomorphic case.
pub fn instantiate(store: &mut UnifStore, sch: &Scheme) -> Type {
    // Build the quantified-var → fresh-var maps once, at the current level.
    let ty_sub: HashMap<TyVarId, Type> =
        sch.ty_vars.iter().map(|&a| (a, store.fresh_ty())).collect();
    let row_sub: HashMap<RowVarId, RowVarId> = sch
        .row_vars
        .iter()
        .map(|&p| (p, store.fresh_row()))
        .collect();
    // Qualified types (traits v1, `trait-resolution.md` §1.1): copy each of the
    // scheme's constraints under the **same** fresh renaming, so a `Show ?a`
    // obligation shares the `?a` that the scheme body's `?a -> String` got, and
    // re-emit it as a pending obligation. A scheme with no constraints (every
    // ordinary scheme) pushes nothing, so existing programs are unaffected.
    for c in &sch.constraints {
        OBLIGATIONS.with(|o| {
            o.borrow_mut().push(Constraint {
                trait_name: c.trait_name.clone(),
                ty: subst_ty(&c.ty, &ty_sub, &row_sub),
            })
        });
    }
    subst_ty(&sch.ty, &ty_sub, &row_sub)
}

/// **Instantiate a constructor** (S3, D12): a thin specialization of
/// [`instantiate`] for a sum constructor, whose quantified set is the declared
/// type parameters (`ty_params`) and whose body is split into the **field
/// types** and the **result type**, all sharing one fresh-var map. Returns the
/// fields and result with each `ty_param` replaced by a **fresh** type var at
/// the current level, consistently across both — so a `Cons : a -> List[a] ->
/// List[a]` instantiates to `?n -> List[?n] -> List[?n]` (the same `?n`), ready
/// for the `Construct`/`Match` unifications to solve.
///
/// Reuses [`subst_ty`] verbatim — **no new substitution logic**. (Rows are not
/// involved: a constructor's fields are value types, never latent arrows of the
/// declaration's params.)
pub fn instantiate_ctor(
    store: &mut UnifStore,
    ty_params: &[TyVarId],
    fields: &[Type],
    result: &Type,
) -> (Vec<Type>, Type) {
    let ty_sub: HashMap<TyVarId, Type> = ty_params.iter().map(|&p| (p, store.fresh_ty())).collect();
    let row_sub: HashMap<RowVarId, RowVarId> = HashMap::new();
    let fields = fields
        .iter()
        .map(|f| subst_ty(f, &ty_sub, &row_sub))
        .collect();
    let result = subst_ty(result, &ty_sub, &row_sub);
    (fields, result)
}

/// Substitute quantified variables throughout a type (the instantiation walk).
/// Quantified vars are replaced by their fresh image; every other var/label is
/// copied verbatim. (Bound vars cannot occur in a scheme body — `generalize`
/// only ever quantifies *unbound* vars — so a plain structural walk suffices.)
fn subst_ty(
    t: &Type,
    ty_sub: &HashMap<TyVarId, Type>,
    row_sub: &HashMap<RowVarId, RowVarId>,
) -> Type {
    match t {
        Type::Var(id) => ty_sub.get(id).cloned().unwrap_or(Type::Var(*id)),
        Type::Fun(a, b, r) => Type::Fun(
            Box::new(subst_ty(a, ty_sub, row_sub)),
            Box::new(subst_ty(b, ty_sub, row_sub)),
            subst_row(r, row_sub),
        ),
        Type::Code(t2, r) => Type::Code(
            Box::new(subst_ty(t2, ty_sub, row_sub)),
            subst_row(r, row_sub),
        ),
        Type::Array(e) => Type::Array(Box::new(subst_ty(e, ty_sub, row_sub))),
        Type::Vector(shape, e) => Type::Vector(*shape, Box::new(subst_ty(e, ty_sub, row_sub))),
        Type::Mask(_) => t.clone(),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|x| subst_ty(x, ty_sub, row_sub)).collect()),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, x)| (n.clone(), subst_ty(x, ty_sub, row_sub)))
                .collect(),
        ),
        // A nominal type's **arguments** carry quantified variables (D8) — a
        // constructor scheme's body is `Cons : ?a -> List[?a] -> List[?a]`, whose
        // `List[?a]`'s argument must be substituted. (This is how `instantiate`
        // and `instantiate_ctor` rename the param vars through the result type.)
        Type::Named(n, args) => Type::Named(
            n.clone(),
            args.iter().map(|x| subst_ty(x, ty_sub, row_sub)).collect(),
        ),
        // Base types carry no variables.
        Type::Int
        | Type::Float
        | Type::Float32
        | Type::Bool
        | Type::Unit
        | Type::Str
        | Type::I32
        | Type::U32
        | Type::Ptr => t.clone(),
    }
}

/// Substitute a quantified **row tail** (instantiation): if the row's tail is a
/// quantified `ρ`, swap in its fresh image; the labels carry through. A `None`
/// (closed) tail and an un-quantified tail are left as-is.
fn subst_row(r: &Row, row_sub: &HashMap<RowVarId, RowVarId>) -> Row {
    let tails = r
        .tail_set()
        .iter()
        .map(|p| row_sub.get(p).copied().unwrap_or(*p))
        .collect();
    Row::with_tails(r.label_set().clone(), tails)
}

/// **Generalize** a type into a scheme by quantifying every variable reachable
/// from it whose level is **deeper than the current level** — the O(1)-per-var
/// levels test ([`polymorphism-impl.md`], "The algorithm"), *not* an
/// environment scan.
///
/// A variable born inside this `let`'s RHS (level `L+1`) satisfies `level > L`
/// after we return to `L`, so it generalizes; a variable shared with the
/// enclosing scope (level `≤ L`, e.g. a captured lambda parameter) does **not**
/// — which is exactly the soundness guard the escaping-variable test checks.
///
/// The body is the type with its *solved* leading variables resolved away but
/// its *unbound* generalizable variables left in place (so the scheme still
/// mentions the `αᵢ`/`ρⱼ` it quantifies). Quantified variables are listed in
/// first-occurrence order, deduplicated.
pub fn generalize(store: &UnifStore, t: &Type) -> Scheme {
    let mut ty_vars = Vec::new();
    let mut row_vars = Vec::new();
    let body = collect_generalizable_ty(store, t, &mut ty_vars, &mut row_vars);
    // Qualified types (traits v1, `trait-resolution.md` §1.1): an obligation that
    // mentions any variable we just quantified is **generalized** into this
    // scheme's `constraints` (so `let show = …` over a `Show ?a` whose `?a`
    // generalizes yields `∀a. Show a => …`); one over a still-free / lower-level
    // variable is **not** generalized — it is left pending and deferred to the
    // enclosing scope, exactly as a free type variable is. With no pending
    // obligations (every non-trait program) this is a no-op, so existing
    // polymorphism is byte-for-byte unaffected.
    let constraints = if ty_vars.is_empty() {
        // Fast path / no-churn guarantee: nothing was quantified, so no obligation
        // can mention a quantified var — leave the obligation list untouched.
        Vec::new()
    } else {
        let mut kept = Vec::new();
        OBLIGATIONS.with(|o| {
            let mut pending = o.borrow_mut();
            let mut deferred = Vec::with_capacity(pending.len());
            for c in pending.drain(..) {
                if constraint_mentions_any(store, &c.ty, &ty_vars) {
                    kept.push(Constraint {
                        trait_name: c.trait_name,
                        // Store the resolved type so the scheme's constraint is in
                        // terms of the quantified vars (not a solved-away forward).
                        ty: store.resolve_ty(&c.ty),
                    });
                } else {
                    deferred.push(c);
                }
            }
            *pending = deferred;
        });
        kept
    };
    Scheme {
        ty_vars,
        row_vars,
        constraints,
        ty: body,
    }
}

/// Does the (resolved) constraint type `t` mention any of the type variables in
/// `vars` — the variables `generalize` just quantified? Walks `t`, resolving
/// solved leading vars, looking for an unbound `Var(id)` with `id ∈ vars`. A
/// constraint that mentions a quantified var is collected into the scheme; one
/// that does not is deferred (it is over a still-free / enclosing-scope var).
fn constraint_mentions_any(store: &UnifStore, t: &Type, vars: &[TyVarId]) -> bool {
    match store.resolve_ty(t) {
        Type::Var(id) => vars.contains(&id),
        Type::Fun(a, b, _) => {
            constraint_mentions_any(store, &a, vars) || constraint_mentions_any(store, &b, vars)
        }
        Type::Code(t2, _) => constraint_mentions_any(store, &t2, vars),
        Type::Array(e) | Type::Vector(_, e) => constraint_mentions_any(store, &e, vars),
        Type::Tuple(ts) => ts.iter().any(|x| constraint_mentions_any(store, x, vars)),
        Type::Record(fs) => fs
            .iter()
            .any(|(_, x)| constraint_mentions_any(store, x, vars)),
        Type::Named(_, args) => args.iter().any(|x| constraint_mentions_any(store, x, vars)),
        _ => false,
    }
}

/// Walk a type, **resolving** solved variables, and collect every *unbound*
/// variable with `level > current_level` into `tys`/`rows` (first-occurrence
/// order, no duplicates). Returns the resolved-but-not-defaulted body — unbound
/// generalizable vars survive as `Type::Var`/open tails (the scheme references
/// them), unlike `zonk` which would default them to `Int`/closed.
fn collect_generalizable_ty(
    store: &UnifStore,
    t: &Type,
    tys: &mut Vec<TyVarId>,
    rows: &mut Vec<RowVarId>,
) -> Type {
    match store.resolve_ty(t) {
        Type::Var(id) => {
            // Unbound (resolve_ty stops at an unbound var). Generalize it iff it
            // is younger than the level we are generalizing at.
            if store.ty_level(id) > store.current_level && !tys.contains(&id) {
                tys.push(id);
            }
            Type::Var(id)
        }
        Type::Fun(a, b, r) => Type::Fun(
            Box::new(collect_generalizable_ty(store, &a, tys, rows)),
            Box::new(collect_generalizable_ty(store, &b, tys, rows)),
            collect_generalizable_row(store, &r, rows),
        ),
        Type::Code(t2, r) => Type::Code(
            Box::new(collect_generalizable_ty(store, &t2, tys, rows)),
            collect_generalizable_row(store, &r, rows),
        ),
        Type::Array(e) => Type::Array(Box::new(collect_generalizable_ty(store, &e, tys, rows))),
        Type::Vector(shape, e) => Type::Vector(
            shape,
            Box::new(collect_generalizable_ty(store, &e, tys, rows)),
        ),
        Type::Mask(shape) => Type::Mask(shape),
        Type::Tuple(ts) => Type::Tuple(
            ts.iter()
                .map(|x| collect_generalizable_ty(store, x, tys, rows))
                .collect(),
        ),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, x)| (n.clone(), collect_generalizable_ty(store, x, tys, rows)))
                .collect(),
        ),
        // A nominal type's **arguments** can be generalizable vars (D8): a value
        // of type `List[?a]` whose `?a` is deep must quantify it. (Without this
        // recursion, generalizing a `List[?a]`-typed value would silently drop
        // the `?a` and never generalize it.)
        Type::Named(n, args) => Type::Named(
            n,
            args.iter()
                .map(|x| collect_generalizable_ty(store, x, tys, rows))
                .collect(),
        ),
        other => other,
    }
}

/// The row half of [`collect_generalizable_ty`]: resolve the tail chain, collect
/// a generalizable unbound tail, and return the row with its concrete labels and
/// the (still-unbound) tail preserved.
fn collect_generalizable_row(store: &UnifStore, r: &Row, rows: &mut Vec<RowVarId>) -> Row {
    let (labels, tails) = store.resolve_row(r);
    for id in &tails {
        if store.row_level(*id) > store.current_level && !rows.contains(id) {
            rows.push(*id);
        }
    }
    Row::with_tails(labels, tails)
}

/// Zonk a whole decorated tree in place of its types/rows (the public D6 entry,
/// called once after `elaborate` and before stage/IR). Walks every node's
/// `ty`/`row` and recurses into children.
pub fn zonk(store: &UnifStore, t: &crate::sema::Typed) -> crate::sema::Typed {
    use crate::sema::{
        MatchArmT, Node, Typed, TypedBlockItem, TypedHandler, TypedOpClause, TypedReturn,
    };

    let z = |c: &Typed| zonk(store, c);
    let zb = |c: &Typed| Box::new(zonk(store, c));

    let node = match &t.node {
        // Leaves (no sub-`Typed`).
        Node::Var(x) => Node::Var(x.clone()),
        Node::Int(n) => Node::Int(*n),
        Node::Float(bits) => Node::Float(*bits),
        Node::Bool(b) => Node::Bool(*b),
        Node::Unit => Node::Unit,
        Node::Brk => Node::Brk,
        Node::Str(s) => Node::Str(s.clone()),
        Node::Extern(s, abi) => Node::Extern(s.clone(), abi.clone()),

        Node::Bin(op, a, b) => Node::Bin(*op, zb(a), zb(b)),
        Node::Cast(op, a) => Node::Cast(*op, zb(a)),
        Node::Coerce {
            kind,
            slot,
            value,
            inner,
        } => Node::Coerce {
            kind: *kind,
            slot: zonk_ty(store, slot),
            value: zonk_ty(store, value),
            inner: zb(inner),
        },
        Node::FloatMathUnary(op, a) => Node::FloatMathUnary(*op, zb(a)),
        Node::FloatMathBinary(op, a, b) => Node::FloatMathBinary(*op, zb(a), zb(b)),
        Node::FloatMathTernary(op, a, b, c) => Node::FloatMathTernary(*op, zb(a), zb(b), zb(c)),
        Node::MaskReduce(op, a) => Node::MaskReduce(*op, zb(a)),
        Node::VectorSelect {
            mask,
            then_value,
            else_value,
        } => Node::VectorSelect {
            mask: zb(mask),
            then_value: zb(then_value),
            else_value: zb(else_value),
        },
        Node::VectorLit { shape, elems } => Node::VectorLit {
            shape: *shape,
            elems: elems.iter().map(&z).collect(),
        },
        Node::VectorSplat { shape, value } => Node::VectorSplat {
            shape: *shape,
            value: zb(value),
        },
        Node::VectorLoad { shape, arr, idx } => Node::VectorLoad {
            shape: *shape,
            arr: zb(arr),
            idx: zb(idx),
        },
        Node::VectorStore {
            shape,
            arr,
            idx,
            value,
        } => Node::VectorStore {
            shape: *shape,
            arr: zb(arr),
            idx: zb(idx),
            value: zb(value),
        },
        Node::VectorExtract { vector, lane } => Node::VectorExtract {
            vector: zb(vector),
            lane: *lane,
        },
        Node::If(c, th, el) => Node::If(zb(c), zb(th), zb(el)),
        Node::Loop {
            vars,
            cond,
            steps,
            result,
        } => Node::Loop {
            vars: vars
                .iter()
                .map(|(name, ty, layout, init)| {
                    (name.clone(), zonk_ty(store, ty), *layout, z(init))
                })
                .collect(),
            cond: zb(cond),
            steps: steps.iter().map(&z).collect(),
            result: zb(result),
        },
        Node::Lam {
            param,
            param_ty,
            body,
        } => Node::Lam {
            param: param.clone(),
            param_ty: zonk_ty(store, param_ty),
            body: zb(body),
        },
        Node::App { fun, arg } => Node::App {
            fun: zb(fun),
            arg: zb(arg),
        },
        Node::Let { name, bound, body } => Node::Let {
            name: name.clone(),
            bound: zb(bound),
            body: zb(body),
        },
        Node::Block { items, body } => Node::Block {
            items: items
                .iter()
                .map(|item| match item {
                    TypedBlockItem::Let { name, bound } => TypedBlockItem::Let {
                        name: name.clone(),
                        bound: z(bound),
                    },
                    TypedBlockItem::LetMut { name, bound } => TypedBlockItem::LetMut {
                        name: name.clone(),
                        bound: z(bound),
                    },
                    TypedBlockItem::LetTuple {
                        names,
                        bound,
                        fields_layout_known,
                    } => TypedBlockItem::LetTuple {
                        names: names.clone(),
                        bound: z(bound),
                        fields_layout_known: *fields_layout_known,
                    },
                })
                .collect(),
            body: zb(body),
        },
        Node::LetMut { name, bound, body } => Node::LetMut {
            name: name.clone(),
            bound: zb(bound),
            body: zb(body),
        },
        Node::Assign { name, value } => Node::Assign {
            name: name.clone(),
            value: zb(value),
        },
        Node::RefNew { value } => Node::RefNew { value: zb(value) },
        Node::Deref { cell } => Node::Deref { cell: zb(cell) },
        Node::RefAssign { target, value } => Node::RefAssign {
            target: zb(target),
            value: zb(value),
        },
        Node::Perform { label, arg } => Node::Perform {
            label: label.clone(),
            arg: zb(arg),
        },
        Node::Quote(b) => Node::Quote(zb(b)),
        Node::Splice(b) => Node::Splice(zb(b)),
        Node::Genlet(b) => Node::Genlet(zb(b)),
        Node::Letloc(b) => Node::Letloc(zb(b)),
        Node::Peek(w, a) => Node::Peek(*w, zb(a)),
        Node::Poke(w, a, b) => Node::Poke(*w, zb(a), zb(b)),
        Node::Fill(a, b, c) => Node::Fill(zb(a), zb(b), zb(c)),
        Node::Copy(a, b, c) => Node::Copy(zb(a), zb(b), zb(c)),
        Node::Index(w, a, b) => Node::Index(*w, zb(a), zb(b)),
        Node::IndexSet(w, a, b, c) => Node::IndexSet(*w, zb(a), zb(b), zb(c)),
        Node::Tuple(es) => Node::Tuple(es.iter().map(&z).collect()),
        Node::LetTuple(names, e, body) => Node::LetTuple(names.clone(), zb(e), zb(body)),
        Node::Record(fs) => Node::Record(fs.iter().map(|(n, c)| (n.clone(), z(c))).collect()),
        Node::Field(r, name) => Node::Field(zb(r), name.clone()),
        Node::ArrayLit { elems, elem_layout } => Node::ArrayLit {
            elems: elems.iter().map(&z).collect(),
            elem_layout: *elem_layout,
        },
        Node::Len(a) => Node::Len(zb(a)),
        Node::ArrayGet {
            arr,
            idx,
            elem_layout,
        } => Node::ArrayGet {
            arr: zb(arr),
            idx: zb(idx),
            elem_layout: *elem_layout,
        },
        Node::ArraySet {
            arr,
            idx,
            val,
            elem_layout,
        } => Node::ArraySet {
            arr: zb(arr),
            idx: zb(idx),
            val: zb(val),
            elem_layout: *elem_layout,
        },
        Node::Construct { tag, args } => Node::Construct {
            tag: *tag,
            args: args
                .iter()
                .map(|(c, layout, slot)| (z(c), *layout, zonk_ty(store, slot)))
                .collect(),
        },
        Node::Match { scrutinee, arms } => Node::Match {
            scrutinee: zb(scrutinee),
            arms: arms
                .iter()
                .map(|a| MatchArmT {
                    tag: a.tag,
                    binds: a
                        .binds
                        .iter()
                        .map(|(name, slot, layout, ty)| {
                            (name.clone(), *slot, *layout, zonk_ty(store, ty))
                        })
                        .collect(),
                    body: z(&a.body),
                })
                .collect(),
        },
        Node::Handle { scrutinee, handler } => Node::Handle {
            scrutinee: zb(scrutinee),
            handler: TypedHandler {
                ops: handler
                    .ops
                    .iter()
                    .map(|c| TypedOpClause {
                        op: c.op.clone(),
                        arg: c.arg.clone(),
                        arg_ty: zonk_ty(store, &c.arg_ty),
                        arg_layout: c.arg_layout,
                        resume: c.resume.clone(),
                        resume_ty: zonk_ty(store, &c.resume_ty),
                        resume_layout: c.resume_layout,
                        body: zb(&c.body),
                    })
                    .collect(),
                ret: TypedReturn {
                    var: handler.ret.var.clone(),
                    var_ty: zonk_ty(store, &handler.ret.var_ty),
                    var_layout: handler.ret.var_layout,
                    body_ty: zonk_ty(store, &handler.ret.body_ty),
                    body: zb(&handler.ret.body),
                },
            },
        },
    };

    Typed {
        ty: zonk_ty(store, &t.ty),
        row: zonk_row(store, &t.row),
        stage: t.stage,
        layout_known: t.layout_known,
        node,
    }
}

// ── thread-local store for `elaborate` (D2, mirrors sema's TYENV) ────────

thread_local! {
    static STORE: RefCell<UnifStore> = RefCell::new(UnifStore::new());

    /// Pending **trait obligations** (`trait-resolution.md` §1.1; traits/qualified
    /// types v1, Sprint 1). The obligation list `instantiate` *emits into* and
    /// `generalize` *collects from* — the model is the `SEAL_OBLIGATIONS`
    /// thread-local in `sema.rs`. Each entry is a [`Constraint`] whose `ty` may
    /// still be a unification `Var` (it is re-resolved when read). `instantiate`
    /// pushes one obligation per scheme constraint under the fresh renaming;
    /// `generalize` drains those mentioning a quantified variable into the new
    /// scheme; whatever remains is *deferred* to the enclosing scope. `elaborate`
    /// resets it per program and drains the residue at the end (Sprint 1: a coded
    /// `RN-E0230` NYIMP per undischarged obligation; Sprint 2: real resolution).
    static OBLIGATIONS: RefCell<Vec<Constraint>> = const { RefCell::new(Vec::new()) };
}

/// Reset the thread-local store to empty/level-0 — called at the start of a
/// fresh top-level [`crate::sema::elaborate`] so each program elaborates against
/// a clean substitution.
pub fn reset_store() {
    STORE.with(|s| *s.borrow_mut() = UnifStore::new());
    OBLIGATIONS.with(|o| o.borrow_mut().clear());
}

/// Run `f` with mutable access to the thread-local store (the handle sema's
/// arms reach unification through).
pub fn with_store<R>(f: impl FnOnce(&mut UnifStore) -> R) -> R {
    STORE.with(|s| f(&mut s.borrow_mut()))
}

/// Record a fresh pending trait **obligation** `C` (traits v1). Pushed by
/// [`instantiate`] (one per constrained-scheme constraint) and by the trait /
/// instance elaboration when it needs to register a sub-obligation; consumed by
/// [`generalize`] (which keeps the ones mentioning a quantified var) and finally
/// drained by `elaborate`. Modelled on `sema::SEAL_OBLIGATIONS`.
pub fn push_obligation(c: Constraint) {
    OBLIGATIONS.with(|o| o.borrow_mut().push(c));
}

/// Drain **all** pending obligations (taking them out of the list). Called by
/// `elaborate` after zonk to surface any obligation left undischarged in Sprint
/// 1 (the `RN-E0230` NYIMP placeholder); Sprint 2's resolution pass drains and
/// discharges instead.
pub fn take_obligations() -> Vec<Constraint> {
    OBLIGATIONS.with(|o| std::mem::take(&mut *o.borrow_mut()))
}

/// The current number of pending obligations (traits v1 Sprint 3) — a marker so a
/// caller can read **only the obligations a following [`instantiate`] appends**,
/// without disturbing the list (the Sprint-2 R7/no-instance drain still sees them).
pub fn peek_obligations_len() -> usize {
    OBLIGATIONS.with(|o| o.borrow().len())
}

/// **Clone** (not remove) the pending obligations appended since marker `from`
/// (traits v1 Sprint 3). Used at a constrained-scheme use to capture the fresh
/// obligation type variables for the dictionary-passing bridge while leaving them
/// pending for the Sprint-2 resolution pass.
pub fn peek_obligations_since(from: usize) -> Vec<Constraint> {
    OBLIGATIONS.with(|o| {
        let o = o.borrow();
        o.get(from..)
            .map(<[Constraint]>::to_vec)
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod oracle;
