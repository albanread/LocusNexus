//! The **S1 oracle** — property tests for [`super::unify`] / [`super::unify_row`]
//! ([`../../../docs/polymorphism-impl.md`], "The oracle").
//!
//! Row unification (idempotent sets with a tail variable) is a classic
//! subtle-bug farm (polymorphism.md §9), so it ships with its laws, not just
//! examples. This is a hand-rolled property harness — the `locus` crate is
//! zero-dependency by design, so there is no `proptest`; instead a tiny
//! deterministic RNG drives thousands of generated cases, each on a **fresh
//! store**, and every property is *also* a no-panic / termination test (an
//! occurs-check must return `Err`, never loop or overflow).
//!
//! Labels are drawn from a **tiny alphabet** (`A`/`B`/`C`) so shared labels
//! actually occur — the interesting overlap cases are reached by construction,
//! not by luck.
//!
//! **Scope.** S1: row + type laws. **S2** (below the type laws): the
//! generalize / instantiate algebra — gen∘inst round-trips α-equivalently, two
//! instantiations of one scheme are variable-disjoint, principality, and the
//! **no-over-generalization** levels guard (the escaping-variable test). The
//! *term-level* value-restriction / effect-honesty properties live with the
//! elaborator (`sema.rs` tests), where there are `Term`s to bind.

use std::collections::BTreeSet;

use super::{
    generalize, instantiate, instantiate_ctor, push_obligation, take_obligations, unify, unify_row,
    zonk_row, zonk_ty, UnifStore, UnifyErr,
};
use crate::check::Scheme;
use crate::syntax::{Constraint, Label, Row, RowVarId, TyVarId, Type};

// ── a tiny deterministic RNG (xorshift64) ───────────────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // Avoid the zero fixed-point.
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

/// The tiny label alphabet — `{console:A, console:B, console:C}` as `World`
/// labels (concrete, comparable, and small enough that overlaps are common).
fn alphabet() -> [Label; 3] {
    [
        Label::World("A".into()),
        Label::World("B".into()),
        Label::World("C".into()),
    ]
}

/// A random subset of the alphabet.
fn gen_labels(rng: &mut Rng) -> BTreeSet<Label> {
    let mut s = BTreeSet::new();
    for l in alphabet() {
        if rng.chance(1, 2) {
            s.insert(l);
        }
    }
    s
}

/// A **closed** row from the alphabet.
fn gen_closed_row(rng: &mut Rng) -> Row {
    Row::with_tail(gen_labels(rng), None)
}

/// A row that is closed or open; an open one's tail is a **fresh** var from
/// `store` (so its id is valid). Returns the row.
fn gen_row(rng: &mut Rng, store: &mut UnifStore) -> Row {
    let labels = gen_labels(rng);
    if rng.chance(1, 2) {
        let tail = store.fresh_row();
        Row::open(labels, tail)
    } else {
        Row::with_tail(labels, None)
    }
}

/// A bounded random type. `vars` is a pool of pre-allocated type-var ids the
/// generator may reference; `rho` a pool of row-var ids for latent rows. Depth
/// is capped so generation always terminates.
fn gen_ty(rng: &mut Rng, depth: u32, vars: &[Type], rho: &[RowVarId]) -> Type {
    if depth == 0 || rng.chance(1, 3) {
        // A leaf: a base type, a monomorphic nominal, a *parametric* nominal
        // (`P[arg]` — so the Named-args paths are exercised), or a pooled var.
        return match rng.below(8) {
            0 => Type::Int,
            1 => Type::Bool,
            2 => Type::Unit,
            3 => Type::Str,
            4 => Type::Named("L".into(), vec![]),
            5 => {
                // A parametric nominal whose single argument is a pooled var (or
                // `Int` when the pool is empty) — drives congruence / zonk-into-
                // args / occurs-through-args.
                let arg = if vars.is_empty() {
                    Type::Int
                } else {
                    vars[rng.below(vars.len() as u64) as usize].clone()
                };
                Type::Named("P".into(), vec![arg])
            }
            _ => {
                if vars.is_empty() {
                    Type::Int
                } else {
                    vars[rng.below(vars.len() as u64) as usize].clone()
                }
            }
        };
    }
    match rng.below(4) {
        0 => {
            let a = gen_ty(rng, depth - 1, vars, rho);
            let b = gen_ty(rng, depth - 1, vars, rho);
            Type::Fun(Box::new(a), Box::new(b), gen_latent(rng, rho))
        }
        1 => Type::Array(Box::new(gen_ty(rng, depth - 1, vars, rho))),
        2 => {
            let n = 2 + rng.below(2) as usize;
            Type::Tuple((0..n).map(|_| gen_ty(rng, depth - 1, vars, rho)).collect())
        }
        _ => {
            let t = gen_ty(rng, depth - 1, vars, rho);
            Type::Code(Box::new(t), gen_latent(rng, rho))
        }
    }
}

/// A latent row for an arrow/Code: closed, or open on a pooled row var.
fn gen_latent(rng: &mut Rng, rho: &[RowVarId]) -> Row {
    let mut labels = BTreeSet::new();
    for l in alphabet() {
        if rng.chance(1, 3) {
            labels.insert(l);
        }
    }
    if !rho.is_empty() && rng.chance(1, 2) {
        Row::open(labels, rho[rng.below(rho.len() as u64) as usize])
    } else {
        Row::with_tail(labels, None)
    }
}

/// The concrete label set a row resolves to (tail dropped) — the observable for
/// comparing two solved rows.
fn resolved_labels(store: &UnifStore, r: &Row) -> BTreeSet<Label> {
    zonk_row(store, r).labels().cloned().collect()
}

const ITERS: u64 = 4000;

// ── ROW laws ─────────────────────────────────────────────────────────────

#[test]
fn row_reflexivity_and_idempotence() {
    // Unifying a row with itself always succeeds and leaves it unchanged (no
    // surplus, no spurious binding) — even an OPEN row against itself.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut store = UnifStore::new();
        let r = gen_row(&mut rng, &mut store);
        let before = resolved_labels(&store, &r);
        assert_eq!(
            unify_row(&mut store, &r, &r),
            Ok(()),
            "row {r} ~ itself must hold"
        );
        assert_eq!(
            resolved_labels(&store, &r),
            before,
            "self-unify changed {r}"
        );
        // And again — idempotent.
        assert_eq!(unify_row(&mut store, &r, &r), Ok(()));
    }
}

#[test]
fn row_commutativity() {
    // unify_row(a,b) succeeds exactly when unify_row(b,a) does, and on success
    // the two rows are made equal (same resolved label set) either way.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut s1 = UnifStore::new();
        let a = gen_row(&mut rng, &mut s1);
        let b = gen_row(&mut rng, &mut s1);
        // Build the mirror in a second store with the SAME structure. (Fresh
        // vars get the same ids because we allocate in the same order.)
        let mut rng2 = Rng::new(seed);
        let mut s2 = UnifStore::new();
        let a2 = gen_row(&mut rng2, &mut s2);
        let b2 = gen_row(&mut rng2, &mut s2);

        let fwd = unify_row(&mut s1, &a, &b);
        let bwd = unify_row(&mut s2, &b2, &a2);
        assert_eq!(
            fwd.is_ok(),
            bwd.is_ok(),
            "commutativity of success for {a} ~ {b}"
        );
        if fwd.is_ok() {
            assert_eq!(
                resolved_labels(&s1, &a),
                resolved_labels(&s1, &b),
                "a,b not equal after a~b"
            );
            assert_eq!(
                resolved_labels(&s2, &a2),
                resolved_labels(&s2, &b2),
                "a,b not equal after b~a"
            );
            // Same final label set both directions.
            assert_eq!(resolved_labels(&s1, &a), resolved_labels(&s2, &a2));
        }
    }
}

#[test]
fn row_order_independence() {
    // Two equations {a~b, a~c} solved in either order reach the same solution
    // (when both orders succeed). This is the associativity/order-independence
    // law that the fresh-tail in case D exists to guarantee.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut sa = UnifStore::new();
        let a = gen_row(&mut rng, &mut sa);
        let b = gen_row(&mut rng, &mut sa);
        let c = gen_row(&mut rng, &mut sa);

        let mut rng2 = Rng::new(seed);
        let mut sb = UnifStore::new();
        let a2 = gen_row(&mut rng2, &mut sb);
        let b2 = gen_row(&mut rng2, &mut sb);
        let c2 = gen_row(&mut rng2, &mut sb);

        let order1 = unify_row(&mut sa, &a, &b).and_then(|_| unify_row(&mut sa, &a, &c));
        let order2 = unify_row(&mut sb, &a2, &c2).and_then(|_| unify_row(&mut sb, &a2, &b2));

        if order1.is_ok() && order2.is_ok() {
            assert_eq!(
                resolved_labels(&sa, &a),
                resolved_labels(&sb, &a2),
                "order-dependent solution for a with {a},{b},{c}"
            );
        }
    }
}

#[test]
fn closed_rejects_surplus() {
    // The pinned boundary (case A): two CLOSED rows unify iff identical. When
    // they differ, the error names exactly the surplus on each side and there is
    // no panic.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut store = UnifStore::new();
        let a = gen_closed_row(&mut rng);
        let b = gen_closed_row(&mut rng);
        let la: BTreeSet<Label> = a.labels().cloned().collect();
        let lb: BTreeSet<Label> = b.labels().cloned().collect();
        let res = unify_row(&mut store, &a, &b);
        if la == lb {
            assert_eq!(res, Ok(()), "equal closed rows must unify: {a} ~ {b}");
        } else {
            match res {
                Err(UnifyErr::RowMismatch {
                    only_left,
                    only_right,
                }) => {
                    assert_eq!(only_left, la.difference(&lb).cloned().collect());
                    assert_eq!(only_right, lb.difference(&la).cloned().collect());
                }
                other => panic!("closed mismatch {a} ~ {b} gave {other:?}"),
            }
        }
    }
}

#[test]
fn open_absorbs_surplus() {
    // An OPEN row absorbs a closed row's extra labels: after `{L1 | ρ} ~ {L2}`
    // the open side's labels become exactly `L2`, with ρ solved. (Case C/B.)
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut store = UnifStore::new();
        let closed = gen_closed_row(&mut rng);
        let lc: BTreeSet<Label> = closed.labels().cloned().collect();
        // An open row whose concrete labels are a SUBSET of the closed one's, so
        // the surplus only flows one way and unification must succeed.
        let mut open_labels = BTreeSet::new();
        for l in &lc {
            if rng.chance(1, 2) {
                open_labels.insert(l.clone());
            }
        }
        let tail = store.fresh_row();
        let open = Row::open(open_labels, tail);

        assert_eq!(
            unify_row(&mut store, &open, &closed),
            Ok(()),
            "open {open} must absorb closed {closed}"
        );
        // The open row now resolves to exactly the closed row's labels.
        assert_eq!(
            resolved_labels(&store, &open),
            lc,
            "open did not absorb to the closed set"
        );
        // And the tail is bound (no longer an unbound open tail).
        assert!(
            zonk_row(&store, &open)
                .labels()
                .cloned()
                .collect::<BTreeSet<_>>()
                == lc
        );
    }
}

#[test]
fn open_open_unifies_and_shares_a_fresh_tail() {
    // Two open rows always unify (case D); afterwards both carry the UNION of the
    // two concrete label sets and share a common residual. No panic, no surplus.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut store = UnifStore::new();
        let la = gen_labels(&mut rng);
        let lb = gen_labels(&mut rng);
        let pa = store.fresh_row();
        let pb = store.fresh_row();
        let a = Row::open(la.clone(), pa);
        let b = Row::open(lb.clone(), pb);
        assert_eq!(
            unify_row(&mut store, &a, &b),
            Ok(()),
            "two open rows must unify"
        );
        let union: BTreeSet<Label> = la.union(&lb).cloned().collect();
        assert_eq!(resolved_labels(&store, &a), union);
        assert_eq!(resolved_labels(&store, &b), union);
    }
}

#[test]
fn row_occurs_check_fails_cleanly() {
    // The ρ1==ρ2 surplus edge first: {| ρ} ~ {A | ρ} has the same tail on both
    // sides and a non-empty surplus ⇒ a clean RowMismatch (never a loop).
    let mut store = UnifStore::new();
    let rho = store.fresh_row();
    let mut labels = BTreeSet::new();
    labels.insert(Label::World("A".into()));
    let recursive = Row::open(labels, rho);
    let bare = Row::open(BTreeSet::new(), rho);
    let res = unify_row(&mut store, &bare, &recursive);
    assert!(
        matches!(res, Err(UnifyErr::RowMismatch { .. })),
        "got {res:?}"
    );

    // The genuine occurs path (exercised directly on `bind_row`, since the
    // four-case `unify_row` always routes case D through a *fresh* tail and so
    // never constructs a self-cycle on its own): build a forwarding chain whose
    // resolved tail is the very tail we try to bind. ρ and σ fresh; bind
    // σ := {| ρ}, then bind ρ := {| σ}. Resolving {| σ} follows σ → {| ρ} whose
    // tail is ρ — the target — so the occurs-check must fire.
    let mut s2 = UnifStore::new();
    let rho = s2.fresh_row();
    let sigma = s2.fresh_row();
    // σ := {| ρ}
    assert_eq!(
        super::bind_row(&mut s2, sigma, &Row::open(BTreeSet::new(), rho)),
        Ok(())
    );
    // ρ := {| σ}  ⇒  resolves to tail ρ  ⇒  OccursRow
    let res = super::bind_row(&mut s2, rho, &Row::open(BTreeSet::new(), sigma));
    assert_eq!(
        res,
        Err(UnifyErr::OccursRow(rho)),
        "self-cycle must be OccursRow, got {res:?}"
    );
}

// ── TYPE laws ──────────────────────────────────────────────────────────────

/// Allocate a pool of `n` fresh type vars and `m` fresh row vars in `store`.
fn pools(store: &mut UnifStore, n: usize, m: usize) -> (Vec<Type>, Vec<RowVarId>) {
    let vars = (0..n).map(|_| store.fresh_ty()).collect();
    let rho = (0..m).map(|_| store.fresh_row()).collect();
    (vars, rho)
}

#[test]
fn type_reflexivity() {
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut store = UnifStore::new();
        let (vars, rho) = pools(&mut store, 3, 2);
        let t = gen_ty(&mut rng, 3, &vars, &rho);
        assert_eq!(unify(&mut store, &t, &t), Ok(()), "{t} ~ itself must hold");
    }
}

#[test]
fn type_commutativity() {
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut s1 = UnifStore::new();
        let (v1, r1) = pools(&mut s1, 3, 2);
        let a = gen_ty(&mut rng, 3, &v1, &r1);
        let b = gen_ty(&mut rng, 3, &v1, &r1);

        let mut rng2 = Rng::new(seed);
        let mut s2 = UnifStore::new();
        let (v2, r2) = pools(&mut s2, 3, 2);
        let a2 = gen_ty(&mut rng2, 3, &v2, &r2);
        let b2 = gen_ty(&mut rng2, 3, &v2, &r2);

        let fwd = unify(&mut s1, &a, &b);
        let bwd = unify(&mut s2, &b2, &a2);
        assert_eq!(fwd.is_ok(), bwd.is_ok(), "comm success for {a} ~ {b}");
        if fwd.is_ok() {
            // Both directions make the pair equal (same zonked type).
            assert_eq!(
                zonk_ty(&s1, &a),
                zonk_ty(&s1, &b),
                "{a} ~ {b} not equal fwd"
            );
            assert_eq!(zonk_ty(&s2, &a2), zonk_ty(&s2, &b2), "not equal bwd");
        }
    }
}

#[test]
fn type_order_independence() {
    // {a~b, a~c} in either order ⇒ same solution for `a` when both succeed.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut sa = UnifStore::new();
        let (va, ra) = pools(&mut sa, 3, 2);
        let a = gen_ty(&mut rng, 3, &va, &ra);
        let b = gen_ty(&mut rng, 3, &va, &ra);
        let c = gen_ty(&mut rng, 3, &va, &ra);

        let mut rng2 = Rng::new(seed);
        let mut sb = UnifStore::new();
        let (vb, rb) = pools(&mut sb, 3, 2);
        let a2 = gen_ty(&mut rng2, 3, &vb, &rb);
        let b2 = gen_ty(&mut rng2, 3, &vb, &rb);
        let c2 = gen_ty(&mut rng2, 3, &vb, &rb);

        let o1 = unify(&mut sa, &a, &b).and_then(|_| unify(&mut sa, &a, &c));
        let o2 = unify(&mut sb, &a2, &c2).and_then(|_| unify(&mut sb, &a2, &b2));
        if o1.is_ok() && o2.is_ok() {
            assert_eq!(
                zonk_ty(&sa, &a),
                zonk_ty(&sb, &a2),
                "order-dependent solution"
            );
        }
    }
}

#[test]
fn type_occurs_check() {
    // `a ~ Fun(a, Int, ∅)` is the infinite type — must be Err(OccursType), and
    // the same through Array/Tuple/Code wrappers. No loop, no overflow.
    let mut store = UnifStore::new();
    let a = store.fresh_ty();
    let recursive = Type::Fun(Box::new(a.clone()), Box::new(Type::Int), Row::pure());
    assert!(
        matches!(
            unify(&mut store, &a, &recursive),
            Err(UnifyErr::OccursType(..))
        ),
        "a ~ a->Int must fail occurs"
    );

    let mut s2 = UnifStore::new();
    let b = s2.fresh_ty();
    let nested = Type::Array(Box::new(Type::Tuple(vec![Type::Int, b.clone()])));
    assert!(
        matches!(unify(&mut s2, &b, &nested), Err(UnifyErr::OccursType(..))),
        "b ~ Array[(Int,b)] must fail occurs"
    );

    let mut s3 = UnifStore::new();
    let c = s3.fresh_ty();
    let in_code = Type::Code(Box::new(c.clone()), Row::pure());
    assert!(matches!(
        unify(&mut s3, &c, &in_code),
        Err(UnifyErr::OccursType(..))
    ));
}

#[test]
fn arrow_congruence_including_latent_row() {
    // Unifying two arrows unifies domain, codomain, AND the latent row. If the
    // latent rows are *closed and different*, unification must FAIL even when
    // domain/codomain agree — proving the arrow carries its row through unify.
    let mut store = UnifStore::new();
    let f1 = Type::Fun(
        Box::new(Type::Int),
        Box::new(Type::Bool),
        Row::single(Label::World("A".into())),
    );
    let f2 = Type::Fun(
        Box::new(Type::Int),
        Box::new(Type::Bool),
        Row::single(Label::World("B".into())),
    );
    assert!(
        unify(&mut store, &f1, &f2).is_err(),
        "arrows with different closed latent rows must not unify"
    );

    // With an OPEN latent on one side, the rows reconcile and the whole arrow
    // unifies; afterwards a fresh var in the domain is solved to the other's.
    let mut s2 = UnifStore::new();
    let a = s2.fresh_ty();
    let rho = s2.fresh_row();
    let g1 = Type::Fun(
        Box::new(a.clone()),
        Box::new(Type::Bool),
        Row::open(BTreeSet::new(), rho),
    );
    let g2 = Type::Fun(
        Box::new(Type::Int),
        Box::new(Type::Bool),
        Row::single(Label::World("A".into())),
    );
    assert_eq!(
        unify(&mut s2, &g1, &g2),
        Ok(()),
        "open-latent arrow must unify"
    );
    assert_eq!(
        zonk_ty(&s2, &a),
        Type::Int,
        "domain var solved through the arrow"
    );
    // The latent row absorbed {A}.
    assert_eq!(
        zonk_ty(&s2, &g1),
        Type::Fun(
            Box::new(Type::Int),
            Box::new(Type::Bool),
            Row::single(Label::World("A".into()))
        )
    );
}

#[test]
fn var_binds_and_zonks_through_structure() {
    // A solved var zonks to its solution everywhere it appears; an UNSOLVED var
    // defaults to Int (D6). Both must hold — and zonk must terminate.
    let mut store = UnifStore::new();
    let a = store.fresh_ty();
    // a ~ (Int, Bool)
    let pair = Type::Tuple(vec![Type::Int, Type::Bool]);
    assert_eq!(unify(&mut store, &a, &pair), Ok(()));
    assert_eq!(
        zonk_ty(&store, &a),
        pair,
        "solved var must zonk to its solution"
    );

    // A never-touched var defaults to Int.
    let b = store.fresh_ty();
    assert_eq!(
        zonk_ty(&store, &b),
        Type::Int,
        "unbound type var must default to Int (D6)"
    );

    // An unbound row tail defaults to the closed empty row.
    let rho = store.fresh_row();
    let open_empty = Row::open(BTreeSet::new(), rho);
    assert_eq!(zonk_row(&store, &open_empty), Row::pure());
    assert!(zonk_row(&store, &open_empty).is_pure());
}

#[test]
fn no_panic_smoke_over_many_pairs() {
    // Pure fuzz: unify random type pairs and random row pairs; the only
    // contract is that every call returns (Ok or Err) without panicking or
    // diverging. (Catches occurs/level/index regressions broadly.)
    for seed in 0..(ITERS * 2) {
        let mut rng = Rng::new(seed ^ 0x9E37_79B9);
        let mut store = UnifStore::new();
        let (vars, rho) = pools(&mut store, 4, 3);
        let a = gen_ty(&mut rng, 4, &vars, &rho);
        let b = gen_ty(&mut rng, 4, &vars, &rho);
        let _ = unify(&mut store, &a, &b);
        let r1 = gen_row(&mut rng, &mut store);
        let r2 = gen_row(&mut rng, &mut store);
        let _ = unify_row(&mut store, &r1, &r2);
        // Zonk everything — must also terminate and yield ground types/rows.
        let za = zonk_ty(&store, &a);
        assert!(!contains_var(&za), "zonk left a Var in {za}");
    }
}

/// True if a (supposedly zonked) type still contains a `Type::Var` — the D6
/// invariant's negation.
fn contains_var(t: &Type) -> bool {
    match t {
        Type::Var(_) => true,
        Type::Fun(a, b, _) => contains_var(a) || contains_var(b),
        Type::Code(t, _) => contains_var(t),
        Type::Array(e) => contains_var(e),
        Type::Tuple(ts) => ts.iter().any(contains_var),
        Type::Record(fs) => fs.iter().any(|(_, t)| contains_var(t)),
        Type::Named(_, args) => args.iter().any(contains_var),
        _ => false,
    }
}

// ── S2: generalize / instantiate ─────────────────────────────────────────

/// Collect every `Type::Var` id and every row-tail id appearing in a type
/// (structurally — the type is **not** resolved; we want the literal vars).
fn vars_in(t: &Type, tys: &mut BTreeSet<TyVarId>, rows: &mut BTreeSet<RowVarId>) {
    match t {
        Type::Var(id) => {
            tys.insert(*id);
        }
        Type::Fun(a, b, r) => {
            vars_in(a, tys, rows);
            vars_in(b, tys, rows);
            rows.extend(r.tail_set().iter().copied());
        }
        Type::Code(t2, r) => {
            vars_in(t2, tys, rows);
            rows.extend(r.tail_set().iter().copied());
        }
        Type::Array(e) => vars_in(e, tys, rows),
        Type::Tuple(ts) => ts.iter().for_each(|x| vars_in(x, tys, rows)),
        Type::Record(fs) => fs.iter().for_each(|(_, x)| vars_in(x, tys, rows)),
        Type::Named(_, args) => args.iter().for_each(|x| vars_in(x, tys, rows)),
        _ => {}
    }
}

/// Structural **α-equivalence** of two types: identical modulo a *consistent
/// renaming* of type-vars and row-tails. (Bijective: each var on the left maps
/// to one var on the right and vice-versa.)
fn alpha_eq(a: &Type, b: &Type) -> bool {
    fn go(
        a: &Type,
        b: &Type,
        tym: &mut std::collections::HashMap<TyVarId, TyVarId>,
        rowm: &mut std::collections::HashMap<RowVarId, RowVarId>,
    ) -> bool {
        match (a, b) {
            (Type::Var(x), Type::Var(y)) => *tym.entry(*x).or_insert(*y) == *y,
            (Type::Fun(a1, b1, r1), Type::Fun(a2, b2, r2)) => {
                go(a1, a2, tym, rowm) && go(b1, b2, tym, rowm) && rows_eq(r1, r2, rowm)
            }
            (Type::Code(t1, r1), Type::Code(t2, r2)) => {
                go(t1, t2, tym, rowm) && rows_eq(r1, r2, rowm)
            }
            (Type::Array(e1), Type::Array(e2)) => go(e1, e2, tym, rowm),
            (Type::Tuple(t1), Type::Tuple(t2)) => {
                t1.len() == t2.len() && t1.iter().zip(t2).all(|(x, y)| go(x, y, tym, rowm))
            }
            (Type::Record(f1), Type::Record(f2)) => {
                f1.len() == f2.len()
                    && f1
                        .iter()
                        .zip(f2)
                        .all(|((n1, x), (n2, y))| n1 == n2 && go(x, y, tym, rowm))
            }
            // Nominal types: same name + arity, args α-equal positionally (so a
            // `List[a]` and `List[b]` are α-equivalent under a consistent renaming).
            (Type::Named(n1, a1), Type::Named(n2, a2)) => {
                n1 == n2
                    && a1.len() == a2.len()
                    && a1.iter().zip(a2).all(|(x, y)| go(x, y, tym, rowm))
            }
            // Base types: equal by value.
            _ => a == b,
        }
    }
    fn rows_eq(
        r1: &Row,
        r2: &Row,
        rowm: &mut std::collections::HashMap<RowVarId, RowVarId>,
    ) -> bool {
        if r1.label_set() != r2.label_set() {
            return false;
        }
        if r1.tail_set().len() != r2.tail_set().len() {
            return false;
        }
        for (p, q) in r1.tail_set().iter().zip(r2.tail_set()) {
            if *rowm.entry(*p).or_insert(*q) != *q {
                return false;
            }
        }
        true
    }
    go(
        a,
        b,
        &mut std::collections::HashMap::new(),
        &mut std::collections::HashMap::new(),
    )
}

#[test]
fn generalize_then_instantiate_round_trips_alpha() {
    // Generalize a type whose vars are all younger than the level we generalize
    // at, then instantiate: the result is α-equivalent to the original (the
    // quantified vars are consistently renamed to fresh ones). This is the
    // gen∘inst ≅ id law.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut store = UnifStore::new();
        // Generalize at level 0; allocate the pooled vars at level 1 (deeper) so
        // they are all generalizable.
        store.enter_level();
        let (vars, rho) = pools(&mut store, 3, 2);
        let t = gen_ty(&mut rng, 3, &vars, &rho);
        store.leave_level();

        let sch = generalize(&store, &t);
        let inst = instantiate(&mut store, &sch);
        assert!(
            alpha_eq(&t, &inst),
            "gen∘inst not α-equivalent:\n  original: {t}\n  instance: {inst}"
        );
    }
}

#[test]
fn two_instantiations_are_variable_disjoint() {
    // Instantiating one scheme twice yields two copies that share NO variable —
    // so `id 1` and `id true` constrain independent vars and stay independent.
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed);
        let mut store = UnifStore::new();
        store.enter_level();
        let (vars, rho) = pools(&mut store, 3, 2);
        let t = gen_ty(&mut rng, 3, &vars, &rho);
        store.leave_level();

        let sch = generalize(&store, &t);
        // Only meaningful when something was actually quantified.
        if sch.ty_vars.is_empty() && sch.row_vars.is_empty() {
            continue;
        }
        let a = instantiate(&mut store, &sch);
        let b = instantiate(&mut store, &sch);

        let (mut at, mut ar) = (BTreeSet::new(), BTreeSet::new());
        let (mut bt, mut br) = (BTreeSet::new(), BTreeSet::new());
        vars_in(&a, &mut at, &mut ar);
        vars_in(&b, &mut bt, &mut br);
        assert!(
            at.is_disjoint(&bt),
            "two instantiations share a type var: {a} vs {b} ({at:?} ∩ {bt:?})"
        );
        assert!(
            ar.is_disjoint(&br),
            "two instantiations share a row var: {a} vs {b}"
        );
    }
}

#[test]
fn principal_scheme_of_identity_is_forall_a_a_to_a() {
    // `id`'s scheme is `∀a. a -> a` (one quantified var, used in both positions),
    // independent of how it is built — the principality anchor.
    let mut store = UnifStore::new();
    store.enter_level();
    let a = store.fresh_ty(); // level 1
    store.leave_level(); // back to 0
    let id_ty = Type::Fun(Box::new(a.clone()), Box::new(a.clone()), Row::pure());

    let sch = generalize(&store, &id_ty);
    assert_eq!(sch.ty_vars.len(), 1, "exactly one type var is quantified");
    assert!(sch.row_vars.is_empty(), "no row var to quantify in a -> a");
    // The body is `v -> v` for the single quantified `v`.
    match &sch.ty {
        Type::Fun(d, c, r) => {
            assert!(r.is_pure());
            assert_eq!(d, c, "domain and codomain are the SAME quantified var");
            assert!(matches!(**d, Type::Var(v) if v == sch.ty_vars[0]));
        }
        other => panic!("expected an arrow scheme body, got {other}"),
    }

    // Instantiating twice gives two distinct, internally-consistent copies.
    let i1 = instantiate(&mut store, &sch);
    let i2 = instantiate(&mut store, &sch);
    assert!(alpha_eq(&i1, &id_ty) && alpha_eq(&i2, &id_ty));
    let (mut t1, mut r1) = (BTreeSet::new(), BTreeSet::new());
    let (mut t2, mut r2) = (BTreeSet::new(), BTreeSet::new());
    vars_in(&i1, &mut t1, &mut r1);
    vars_in(&i2, &mut t2, &mut r2);
    assert!(
        t1.is_disjoint(&t2),
        "the two identity instances must be disjoint"
    );
}

#[test]
fn no_over_generalization_escaping_variable_stays_free() {
    // The soundness guard (`let f = λx. let g = λy. x in g in f`): when we
    // generalize the inner `g`, the variable shared with the OUTER `x` must NOT
    // be quantified — it is bound in the enclosing scope and escapes.
    //
    // Model the levels exactly: `x` is born at the outer let's level; `y` at the
    // inner let's (deeper) level. Generalizing `g : y -> x` at the inner level
    // must quantify `y` (deeper) but leave `x` (shallower — it escapes) FREE.
    let mut store = UnifStore::new();
    store.enter_level(); // level 1: the body of the outer `let f = …`
    let x = store.fresh_ty(); // `x`, born at level 1 (the outer parameter's var)
    store.enter_level(); // level 2: the body of the inner `let g = …`
    let y = store.fresh_ty(); // `y`, born at level 2 (the inner parameter's var)

    // We are about to generalize `g`'s type, which happens at level 1 (after the
    // inner let's RHS, having returned from level 2). `g : y -> x`.
    store.leave_level(); // back to level 1 — generalize here
    let g_ty = Type::Fun(Box::new(y.clone()), Box::new(x.clone()), Row::pure());
    let sch = generalize(&store, &g_ty);

    let Type::Var(xid) = x else { unreachable!() };
    let Type::Var(yid) = y else { unreachable!() };
    assert!(
        sch.ty_vars.contains(&yid),
        "the inner-bound `y` (deeper level) must generalize"
    );
    assert!(
        !sch.ty_vars.contains(&xid),
        "the outer-bound `x` ESCAPES — it must stay free, not be generalized (levels guard)"
    );
}

#[test]
fn value_restriction_at_the_generalize_level() {
    // A unification-level reflection of the value restriction's two halves:
    //
    //   * a FULLY GROUND type generalizes nothing (the monomorphic case — the
    //     stdlib's concrete combinators stay effectively monomorphic);
    //   * a type with a deep free var DOES quantify it (a value's principal
    //     type generalizes).
    let mut store = UnifStore::new();
    let ground = Type::Fun(Box::new(Type::Int), Box::new(Type::Bool), Row::pure());
    let sch = generalize(&store, &ground);
    assert!(
        sch.ty_vars.is_empty() && sch.row_vars.is_empty(),
        "a ground type quantifies nothing → effectively monomorphic"
    );
    assert_eq!(sch.ty, ground, "and its body is unchanged");

    store.enter_level();
    let v = store.fresh_ty();
    store.leave_level();
    let poly = Type::Fun(Box::new(v.clone()), Box::new(v), Row::pure());
    let sch2 = generalize(&store, &poly);
    assert_eq!(
        sch2.ty_vars.len(),
        1,
        "a deep free var generalizes (a value's type)"
    );
}

#[test]
fn generalize_quantifies_a_latent_row_var() {
    // Effect polymorphism: a deep unbound *row* tail on an arrow's latent row is
    // quantified, so a row-polymorphic combinator instantiates fresh tails.
    let mut store = UnifStore::new();
    store.enter_level();
    let rho = store.fresh_row(); // deep row var
    store.leave_level();
    // Int -> Int ! {A | ρ}
    let mut labels = BTreeSet::new();
    labels.insert(Label::World("A".into()));
    let t = Type::Fun(
        Box::new(Type::Int),
        Box::new(Type::Int),
        Row::open(labels, rho),
    );

    let sch = generalize(&store, &t);
    assert_eq!(
        sch.row_vars,
        vec![rho],
        "the deep latent row tail generalizes"
    );
    assert!(sch.ty_vars.is_empty(), "no type var here");

    // Two instantiations draw disjoint fresh tails.
    let a = instantiate(&mut store, &sch);
    let b = instantiate(&mut store, &sch);
    let (mut _t1, mut r1) = (BTreeSet::new(), BTreeSet::new());
    let (mut _t2, mut r2) = (BTreeSet::new(), BTreeSet::new());
    vars_in(&a, &mut _t1, &mut r1);
    vars_in(&b, &mut _t2, &mut r2);
    assert!(
        r1.is_disjoint(&r2),
        "instantiated latent tails must be disjoint"
    );
    // ...and each instance is α-equivalent to the original (labels preserved).
    assert!(alpha_eq(&a, &t) && alpha_eq(&b, &t));
}

#[test]
fn instantiate_a_mono_scheme_is_identity() {
    // A scheme that quantifies nothing instantiates to its body verbatim (no
    // fresh vars drawn) — the path every monomorphic binding takes.
    let mut store = UnifStore::new();
    let body = Type::Tuple(vec![Type::Int, Type::Str]);
    let sch = Scheme::mono(body.clone());
    assert_eq!(instantiate(&mut store, &sch), body);
}

// ── traits / qualified types v1: instantiate emits an obligation ────────────

#[test]
fn instantiating_a_constrained_scheme_emits_an_obligation() {
    // `show : ∀a. Show a => a -> String`. Instantiating it must (1) rename the
    // body's `a` to a fresh `?n`, and (2) re-emit `Show ?n` — the SAME `?n` — as a
    // pending obligation (trait-resolution.md §1.1).
    let _drain = take_obligations(); // clear any residue on this test thread
    let mut store = UnifStore::new();
    let a = match store.fresh_ty() {
        Type::Var(id) => id,
        _ => unreachable!(),
    };
    let sch = Scheme {
        ty_vars: vec![a],
        row_vars: vec![],
        constraints: vec![Constraint {
            trait_name: "Show".into(),
            ty: Type::Var(a),
        }],
        ty: Type::Fun(Box::new(Type::Var(a)), Box::new(Type::Str), Row::pure()),
    };
    let inst = instantiate(&mut store, &sch);
    let obligations = take_obligations();
    assert_eq!(obligations.len(), 1, "exactly one obligation emitted");
    assert_eq!(obligations[0].trait_name, "Show");
    // The obligation's `?n` is the very var in the instantiated body's domain.
    let Type::Fun(dom, _, _) = &inst else {
        panic!("instantiated to a function");
    };
    assert_eq!(
        obligations[0].ty, **dom,
        "the obligation shares the body's fresh var"
    );
    // ...and it is NOT the scheme's original `a` (a genuine fresh copy).
    assert_ne!(obligations[0].ty, Type::Var(a));
}

#[test]
fn generalize_collects_a_pending_obligation_over_a_quantified_var() {
    // A pending `Show ?n` whose `?n` is younger than the generalize level is
    // **lifted** into the scheme's constraints; nothing is left pending.
    let _drain = take_obligations();
    let mut store = UnifStore::new();
    store.enter_level();
    let v = store.fresh_ty(); // level 1 — generalizable at level 0
    push_obligation(Constraint {
        trait_name: "Show".into(),
        ty: v.clone(),
    });
    let t = Type::Fun(Box::new(v.clone()), Box::new(Type::Str), Row::pure());
    store.leave_level();
    let sch = generalize(&store, &t);
    assert_eq!(sch.ty_vars.len(), 1, "the `?n` generalizes");
    assert_eq!(sch.constraints.len(), 1, "its `Show ?n` is collected");
    assert_eq!(sch.constraints[0].trait_name, "Show");
    assert!(
        take_obligations().is_empty(),
        "the collected obligation is removed from pending"
    );
}

#[test]
fn generalize_defers_an_obligation_over_a_free_var() {
    // A pending `Show ?n` whose `?n` is at the enclosing (non-generalized) level
    // must NOT be collected — it defers to the enclosing scope, like a free var.
    let _drain = take_obligations();
    let mut store = UnifStore::new();
    let outer = store.fresh_ty(); // level 0 — NOT younger than the gen level
    push_obligation(Constraint {
        trait_name: "Show".into(),
        ty: outer.clone(),
    });
    // Generalize a type that mentions `outer` at level 0: nothing quantifies.
    let sch = generalize(&store, &outer);
    assert!(
        sch.ty_vars.is_empty(),
        "the level-0 var does not generalize"
    );
    assert!(
        sch.constraints.is_empty(),
        "so its obligation is not collected"
    );
    let pending = take_obligations();
    assert_eq!(pending.len(), 1, "the obligation stays pending (deferred)");
}

// ── S3: Named-args congruence / instantiate_ctor / zonk-into-args ──────────

#[test]
fn named_args_congruence_and_mismatch() {
    // `Named` unifies **positionally** on its arguments (D8): `P[?a] ~ P[Int]`
    // solves `?a := Int`; `P[Int] ~ P[Bool]` FAILS (the args clash); a NAME
    // mismatch or an ARITY mismatch fails too. No panic on any of these.
    let mut store = UnifStore::new();
    let a = store.fresh_ty();
    let pa = Type::Named("P".into(), vec![a.clone()]);
    let pint = Type::Named("P".into(), vec![Type::Int]);
    assert_eq!(
        unify(&mut store, &pa, &pint),
        Ok(()),
        "P[?a] ~ P[Int] must solve ?a"
    );
    assert_eq!(
        zonk_ty(&store, &a),
        Type::Int,
        "the argument var solved through Named"
    );

    // Distinct concrete args clash.
    let mut s2 = UnifStore::new();
    let pint = Type::Named("P".into(), vec![Type::Int]);
    let pbool = Type::Named("P".into(), vec![Type::Bool]);
    assert!(
        unify(&mut s2, &pint, &pbool).is_err(),
        "P[Int] ~ P[Bool] must clash on the arg"
    );

    // Different name: fail. Different arity: fail. (No panic.)
    let mut s3 = UnifStore::new();
    let q = Type::Named("Q".into(), vec![Type::Int]);
    let p1 = Type::Named("P".into(), vec![Type::Int]);
    assert!(
        unify(&mut s3, &q, &p1).is_err(),
        "different nominal names must not unify"
    );
    let p2 = Type::Named("P".into(), vec![Type::Int, Type::Int]);
    assert!(
        unify(&mut s3, &p1, &p2).is_err(),
        "different nominal arity must not unify"
    );
}

#[test]
fn occurs_check_reaches_through_named_args() {
    // The occurs-check must see a var hiding in a `Named`'s arguments: `a ~ P[a]`
    // is the infinite type and must be `OccursType`, never a loop. (This is the
    // exact regression the "recurse into args in occurs_and_lower_ty" change
    // guards — without it `a` inside `List[a]` escapes the occurs walk.)
    let mut store = UnifStore::new();
    let a = store.fresh_ty();
    let recursive = Type::Named("P".into(), vec![a.clone()]);
    assert!(
        matches!(
            unify(&mut store, &a, &recursive),
            Err(UnifyErr::OccursType(..))
        ),
        "a ~ P[a] must fail the occurs-check (the var is inside the args)"
    );
}

#[test]
fn instantiate_ctor_renames_consistently_and_disjointly() {
    // `instantiate_ctor` is the constructor specialization of `instantiate`: it
    // draws ONE fresh var per type-param and substitutes it through BOTH the
    // fields and the result (sharing the var), and two calls are variable-
    // disjoint. Model `Cons : a -> List[a] -> List[a]` as
    //   ty_params = [a]; fields = [a, List[a]]; result = List[a].
    let mut store = UnifStore::new();
    let a = store.fresh_ty();
    let Type::Var(aid) = a else { unreachable!() };
    let list_a = Type::Named("List".into(), vec![a.clone()]);
    let fields = vec![a.clone(), list_a.clone()];

    let (f1, r1) = instantiate_ctor(&mut store, &[aid], &fields, &list_a);
    // The instance's field[0], the arg inside field[1]'s List, and the result's
    // List arg are all the SAME fresh var (consistent renaming).
    let v = match &f1[0] {
        Type::Var(v) => *v,
        other => panic!("field[0] should instantiate to a var, got {other}"),
    };
    assert_eq!(
        f1[1],
        Type::Named("List".into(), vec![Type::Var(v)]),
        "field List shares the var"
    );
    assert_eq!(
        r1,
        Type::Named("List".into(), vec![Type::Var(v)]),
        "result List shares the var"
    );
    assert_ne!(
        v, aid,
        "instantiation draws a FRESH var, not the quantified one"
    );

    // A second instantiation is disjoint from the first.
    let (f2, _r2) = instantiate_ctor(&mut store, &[aid], &fields, &list_a);
    let (mut t1, mut _row1) = (BTreeSet::new(), BTreeSet::new());
    let (mut t2, mut _row2) = (BTreeSet::new(), BTreeSet::new());
    vars_in(&f1[0], &mut t1, &mut _row1);
    vars_in(&f2[0], &mut t2, &mut _row2);
    assert!(
        t1.is_disjoint(&t2),
        "two ctor instantiations must be variable-disjoint"
    );
}

#[test]
fn zonk_into_named_args_leaves_no_var() {
    // The D6 invariant **through `Named` arguments**: after zonk, no `Type::Var`
    // survives inside a nominal's args — a solved one becomes its solution, an
    // unbound one defaults to `Int`. (Guards the "recurse into args in zonk_ty"
    // change; a miss would leave a `Var` that later panics `is_gc_ref`.)
    let mut store = UnifStore::new();
    // A solved arg.
    let a = store.fresh_ty();
    assert_eq!(unify(&mut store, &a, &Type::Bool), Ok(()));
    // An unbound arg (never touched).
    let b = store.fresh_ty();
    let t = Type::Named("Pair".into(), vec![a, b, Type::Named("L".into(), vec![])]);
    let z = zonk_ty(&store, &t);
    assert!(!contains_var(&z), "zonk left a Var in Named args: {z}");
    assert_eq!(
        z,
        Type::Named(
            "Pair".into(),
            vec![Type::Bool, Type::Int, Type::Named("L".into(), vec![])]
        ),
        "solved arg → Bool, unbound arg → Int (D6), monomorphic nominal unchanged"
    );

    // And over the generated corpus: a zonked random type never holds a Var in
    // any Named arg (the parametric `P[..]` leaf makes this reachable).
    for seed in 0..ITERS {
        let mut rng = Rng::new(seed ^ 0x5151_5151);
        let mut s = UnifStore::new();
        let (vars, rho) = pools(&mut s, 3, 2);
        let ty = gen_ty(&mut rng, 3, &vars, &rho);
        // Partially solve some pool vars so zonk has substitutions to make.
        let _ = unify(&mut s, &vars[0], &Type::Int);
        let z = zonk_ty(&s, &ty);
        assert!(
            !contains_var(&z),
            "zonk left a Var (incl. in Named args): {z}"
        );
    }
}
