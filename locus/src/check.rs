//! Type errors and the `infer` projection.
//!
//! The typing judgment `Γ ⊢ e : A ! E @ s` (calculus §2–§4) is implemented
//! **once**, in [`crate::sema::elaborate`], which decorates *every* node —
//! *sema is the authoritative model*. `infer` is the thin projection for
//! callers that want only the top-level `(type, row)`: it runs the elaborator
//! and keeps the root's decoration.
//!
//! This module owns the shared vocabulary — the stage / context / signature
//! aliases and [`TypeErr`], with its stable `RN-Exxxx` code, spec citation,
//! and hint (design §8).

use crate::sema::elaborate;
use crate::syntax::{Constraint, Label, OpSig, Row, RowVarId, Term, TyVarId, Type};
use std::collections::HashMap;

/// A stage (§0.5). 0 = runtime; 1 = generation. (Single-stage: only `{0,1}`.)
pub type Stage = u32;

/// A **type scheme** `∀ᾱ ρ̄. A` — a type with some type-vars and row-vars
/// **quantified** (`polymorphism-impl.md`, S2). Produced by
/// [`crate::unify::generalize`] when a `let` binds a syntactic *value* (D4),
/// and consumed by [`crate::unify::instantiate`], which replaces each
/// quantified variable with a fresh one at the use site — so two uses of `id`
/// (`id 1`, `id true`) are independent. A scheme with **no** quantified
/// variables is an ordinary monomorphic type wrapped up (the common case: the
/// stdlib's concrete combinators generalize *nothing*, so they stay effectively
/// monomorphic and monomorphic programs are unaffected).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Scheme {
    /// The quantified **type** variables `ᾱ` (the `∀a` of `∀a. a -> a`).
    pub ty_vars: Vec<TyVarId>,
    /// The quantified **row** variables `ρ̄` — quantified latent rows, so a
    /// polymorphic combinator is also *effect*-polymorphic where sound.
    pub row_vars: Vec<RowVarId>,
    /// The **qualified-type constraints** `C̄` of `∀ᾱ ρ̄. C̄ => A`
    /// (`trait-resolution.md` §1.1; traits/qualified-types v1, Sprint 1). A
    /// constraint mentions one or more of the quantified `ᾱ` (e.g. `Show a` for
    /// `show : ∀a. Show a => a -> String`). [`crate::unify::generalize`] collects
    /// every pending obligation mentioning a quantified variable here;
    /// [`crate::unify::instantiate`] copies each constraint under the same fresh
    /// renaming, re-emitting it as a pending obligation. **Empty** for every
    /// ordinary (non-trait) scheme — so monomorphic and existing polymorphic
    /// programs are byte-for-byte unaffected.
    pub constraints: Vec<Constraint>,
    /// The scheme body (mentions the quantified variables; `instantiate`
    /// substitutes fresh vars for them).
    pub ty: Type,
}

impl Scheme {
    /// A **monomorphic** scheme — a type with nothing quantified. (Used by the
    /// oracle and as the `Poly` form of a value whose type happens to be ground.)
    pub fn mono(ty: Type) -> Scheme {
        Scheme {
            ty_vars: Vec::new(),
            row_vars: Vec::new(),
            constraints: Vec::new(),
            ty,
        }
    }
}

/// What a name is bound to in the context (S2). The **value restriction** (D4)
/// decides which: a `let` bound to a syntactic *value* gets a generalized
/// [`Scheme`] (`Poly`); a non-value (an `App`, a `perform`, …) stays
/// monomorphic (`Mono`) — its type a single shared variable/type, never
/// generalized, which is the soundness lynchpin.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Binding {
    /// A monomorphic binding — a plain type (a lambda parameter, a non-value
    /// `let`, a `match`/handler binder). Used at exactly this type.
    Mono(Type),
    /// A polymorphic binding — a generalized scheme, **instantiated fresh** at
    /// each use.
    Poly(Scheme),
    /// A **mutable** local (`let mut x = e`, mutability v1). Monomorphic at its
    /// scalar type `τ` — a read of `x` types at `τ` exactly like a `Mono`, but the
    /// extra variant *marks* the binding so an assignment `x := e` can require `x`
    /// to be mutable (a `Mono`/`Poly` `let` is not assignable → `RN-E0243`). The
    /// marker lives in the cloned [`Ctx`], so it scopes and shadows like any
    /// binding (`docs/mutability.md` §3; `mutability-sprints.md` Sprint 2).
    Mut(Type),
}

/// A typing context `Γ` — variable ↦ (binding, the **stage it was bound at**).
/// The binding stage drives the stage-ordering check (SO-1, §9); the
/// [`Binding`] is `Mono`/`Poly` per the value restriction (S2, D4).
pub type Ctx = HashMap<String, (Binding, Stage)>;

/// Operation signatures `Σ` — each `op : param => result` (calculus §1.1).
pub type Sig = HashMap<Label, OpSig>;

/// A typing failure.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeErr {
    Unbound(String),
    NotAFunction(Type),
    /// Spliced a value that is not `Code`.
    NotCode(Type),
    Mismatch {
        expected: Type,
        found: Type,
    },
    /// SO-1 (§9): a binder bound at a lower stage used at a higher one — the
    /// generator trying to read a variable of the program it is building.
    StageEscape {
        var: String,
        bound: Stage,
        used: Stage,
    },
    /// `quote` away from the generation stage, or `splice` away from the
    /// object stage — i.e. nested staging, which single-stage (§3.0) forbids.
    StageMisuse {
        what: &'static str,
        at: Stage,
    },
    /// A bare `extern "sym"` (no `: T`) reached the std-only core checker, which
    /// has no Win32 oracle. The oracle resolver (wired into `locusc`) fills these
    /// in before elaboration; reaching here means it didn't run.
    BareExtern(String),
    /// Subscripted a value that is not an indexable machine word — the `mem`
    /// accessor `a[i]` needs a `String` (16-bit units), a `Ptr`, or an `Int`
    /// address (bytes).
    NotIndexable(Type),
    /// A record literal repeated a field name.
    DupField(String),
    /// `r.x` where `r` is not a record.
    NotRecord(Type),
    /// `r.x` where the record has no field `x`.
    NoField(String, Type),
    /// `len a` where `a` is not an array.
    NotArray(Type),
    /// A constructor name that no in-scope `type` declares.
    UnknownCtor(String),
    /// A constructor applied to (or matched with) the wrong number of fields.
    CtorArity {
        ctor: String,
        expected: usize,
        found: usize,
    },
    /// A structured accumulator loop has a different number of `do` step
    /// expressions than accumulator binders.
    LoopArity {
        expected: usize,
        found: usize,
    },
    /// A nominal type **named with the wrong number of type arguments** (S3, D14)
    /// — `List[Int, Bool]` where `List` declares one parameter. Distinct from
    /// `CtorArity` (a wrong *field* count): this is a wrong *type-argument* count
    /// on a type annotation. `unify`'s same-arity check is the backstop.
    ArityMismatch {
        name: String,
        expected: usize,
        found: usize,
    },
    /// `match`ed a value that is not a sum type.
    NotASum(Type),
    /// A `match` that omits a constructor and has no wildcard.
    NonExhaustive {
        ty: String,
        missing: String,
    },
    /// A `match` with no arms.
    EmptyMatch,
    /// The program type-checks, but current IR/LLVM cannot yet choose concrete
    /// pointer/scalar slots for a representation-polymorphic allocation or
    /// projection.
    RepresentationPolymorphicLayout,
    /// **D5/D3 representation-kind violation (T1)**: a `Wide` type
    /// (`Float`/`Float32`/`Pair`/`Quad`/`Oct`) reached a currently-uniform
    /// representation-polymorphic slot. The ratified language rule binds this at
    /// traced-store sites only; the current compiler still over-approximates in
    /// some generic-call paths because they use the tagged uniform-word ABI.
    /// Either way, a wide value has no tag-room and cannot inhabit a traced
    /// `Var` word cell. Use an untraced concrete container (`Array[Float]`) or
    /// monomorphize the containing type.
    WideTypeVariable {
        ty: Type,
    },
    /// **Seal no-escape violation (`RN-E0403 cap.seal-leak`).** A `seal L { e }`
    /// (or `nogc { e } ≝ seal gc { e }`) region let the sealed label `L` escape
    /// through its result type `ty` — either `L` appears in a row reachable from
    /// `ty` (a returned closure that still performs `L`), or, for `gc`, a
    /// gc-managed datum left the region. The seal removes a power; a value
    /// carrying it may not cross the boundary (sealing-solution.md §5, the
    /// `runST`/`st` deep escape check relabeled).
    SealLeak {
        label: Label,
        ty: Type,
    },
    /// **Seal of an unhandled effect (`RN-E0403 cap.seal-leak`).** A `seal L { e }`
    /// tried to remove a **non-runtime** effect (`User`/`Exn`) that `e` still
    /// performs unhandled. Native powers (`gc`, `World` syscalls) discharge at the
    /// boundary into a runtime call, so sealing them is sound; a user effect /
    /// exception escapes to the caller unless `handle`d, so removing it would turn
    /// a checked obligation into an unchecked runtime fault (sealing-solution.md
    /// §5 — a seal is "a handler + a boundary check"; the handler must exist).
    SealUnhandled {
        label: Label,
    },
    /// **Module seal leak (`RN-E0403 cap.seal-leak`).** A module's `seals (L)`
    /// clause is violated: an **exported** binding's type carries the sealed label
    /// `L` — the kernel/service export boundary leaks the raw power it was meant to
    /// hide (`sealing-plan.md` S4). Distinct from the region [`SealLeak`] so the
    /// diagnostic can name the offending module and binding.
    ModuleSealLeak {
        module: String,
        binding: String,
        label: Label,
        ty: Type,
    },
    /// **Duplicate type parameter (`RN-E0229 type.malformed-decl`).** A `type`
    /// declares the same parameter name twice — `type T[a, a] = …`. The
    /// declaration-time substitution would silently collapse the duplicate (the
    /// second `a` shadowing the first), so it is rejected before registration.
    DuplicateTypeParam {
        ty: String,
        param: String,
    },
    /// **Duplicate constructor (`RN-E0229 type.malformed-decl`).** A `type`
    /// declares the same constructor twice — `type T = A | A`. The second would
    /// silently overwrite the first in the constructor environment.
    DuplicateConstructor {
        ty: String,
        ctor: String,
    },
    /// **GC-blind asm signature (`RN-E0405 cap.asm-gc-type`).** An `extern asm`
    /// signature names a **gc-managed, movable** value (`Array` / a sum or record
    /// `Named` / `Tuple`) in an argument or result. Layer-0 asm runs with no
    /// safepoints, read barriers, or handle indirection (jasm-boundary-layer.md
    /// §A6), so a moving collector could relocate the object underneath it. Only
    /// GC-blind leaves — scalars, raw `Ptr`, `Unit`, `String` — may cross the asm
    /// boundary until a pinning contract exists.
    AsmGcType {
        ty: Type,
    },
    /// **Non-scalar mutable local (`RN-E0241 mut.non-scalar`).** A `let mut x = e`
    /// bound a value whose type is not a scalar (`Int`/`Float`/`Bool`). Mutability
    /// v1 is deliberately scalar-only — a mutable cell holding a gc-managed datum
    /// would need the heap-`Ref[T]` / `st[T]` machinery and a write barrier, which
    /// is a later effort (`docs/mutability.md` §1.1, §6; `mutability-sprints.md`
    /// v1 scope). The offending bound type is reported.
    MutNonScalar {
        ty: Type,
    },
    /// **Mutable local escapes its scope (`RN-E0241 mut.escapes`).** A `let mut`
    /// binding's cell escaped its scope — the body's result type carries the cell
    /// (a returned closure that still mutates it, a datum that holds it). The `mut`
    /// sugar requires non-escape: it is a *sealed*, non-escaping `Ref`, so the cell
    /// is always observationally pure (`docs/mutability.md` §2–§3, the `runST`/`st`
    /// escape boundary `let mut` reuses). The offending result type is reported.
    /// In scalar v1 a *value* cannot carry the cell, so this is wired for the
    /// Sprint-3 `Ref` reuse; the diagnostic and machinery exist now.
    MutEscapes {
        ty: Type,
    },
    /// **Assignment to an immutable / unbound name (`RN-E0243
    /// mut.assign-immutable`).** An assignment `x := e` named an `x` that is either
    /// not in scope or bound by a plain (immutable) `let` rather than a `let mut`.
    /// Only a `let mut` binding (or a `Ref[T]`-typed name) may be reassigned
    /// (`docs/mutability.md` §1/§3).
    MutAssignImmutable {
        name: String,
    },

    /// **Pointer-typed `Ref` is deferred (`RN-E0247 ref.pointer-content`).** A
    /// `ref e` / `!r` / `r := v` whose cell type `T` is a *pointer* kind — a handle:
    /// `Ref[String]`, `Ref[Array[..]]`, `Ref[record]`, `Ref[Named]`, even a nested
    /// `Ref[Ref[..]]`. A scalar `Ref` (`Int`/`Float`/`Bool`/`Unit`) needs no GC
    /// write barrier (its cell holds no pointer, so a write can never create an
    /// old→young pointer); a **pointer-typed** `Ref` write *can*, and so requires
    /// the collector's write barrier — Sprint 3 (`docs/mutability.md` §6.1,
    /// `mutability-ref-sprints.md`). Rejected cleanly (never a miscompile); the
    /// offending content type is reported. The fix: use a scalar `Ref`, or await
    /// the barrier.
    RefPointerContent {
        ty: Type,
    },

    /// **Trait resolution not-yet-implemented (`RN-E0230 trait.no-instance`).** A
    /// trait constraint `Trait τ` (an obligation, recorded by `instantiate` from a
    /// constrained scheme) was left **undischarged** at the end of elaboration.
    /// Sprint 1 records and surfaces obligations but does not yet *resolve* them
    /// (entailment is Sprint 2): any obligation that reaches here is reported with
    /// this coded placeholder rather than a panic or a silently-wrong type. Sprint
    /// 2 replaces this with real R1 entailment (a dictionary in scope / a matching
    /// instance / `RN-E0230` only when genuinely no instance exists).
    TraitResolutionNYIMP {
        constraint: Constraint,
    },
    /// **No such trait method (`RN-E0235 trait.no-method`).** A name was used as a
    /// trait method but no in-scope `trait` declares it (`trait-resolution.md`
    /// §1.1). (Wired for Sprint 1; the minting path enters every declared method
    /// into scope, so this fires only for a name that *looks* like a method use
    /// but has no trait — Sprint 2 sharpens the use-site detection.)
    TraitNoMethod {
        method: String,
    },
    /// **Instance method row violation (`RN-E0238 trait.method-row-violation`).**
    /// An `instance` method's body does not match the trait's declared method
    /// signature (instantiated at the instance head) — in Sprint 1 this surfaces
    /// the underlying type/row clash against the declared method type
    /// (`trait-resolution.md` §7.3). `RN-E0238`.
    TraitMethodRowViolation {
        trait_name: String,
        method: String,
        expected: Type,
        found: Type,
    },
    /// **Missing / extra instance method (`RN-E0239 trait.missing-method`).** An
    /// `instance` omits a method the trait declares, or implements a name the
    /// trait does not declare (`trait-resolution.md` §1.1). `RN-E0239`.
    TraitMissingMethod {
        trait_name: String,
        method: String,
        /// `true` when the method is declared by the trait but missing from the
        /// instance; `false` when the instance implements an unknown method.
        missing: bool,
    },

    // ── Sprint 2: resolution + the static checks (trait-resolution.md §2–§6) ──
    /// **No instance (`RN-E0230 trait.no-instance`, R1).** A constraint `Trait τ`
    /// (with `τ` a fixed type — not a bare variable) has neither an in-scope
    /// dictionary nor a matching `instance`. The genuine entailment failure
    /// (`trait-resolution.md` §1.2 R1 step 3), and Sprint 2's replacement for the
    /// Sprint-1 NYIMP placeholder.
    TraitNoInstance {
        constraint: Constraint,
    },
    /// **Overlapping instances (`RN-E0231 trait.overlapping-instances`, R4).** Two
    /// instances of one trait whose heads **unify** (could match a common type) —
    /// forbidden in v1 (`trait-resolution.md` §3). Names the trait and both heads.
    TraitOverlappingInstances {
        trait_name: String,
        head1: Type,
        head2: Type,
    },
    /// **Orphan instance (`RN-E0232 trait.orphan-instance`, R5).** An instance is
    /// declared in neither the trait's module nor the type-head's module
    /// (`trait-resolution.md` §4). Names the trait, head, and the offending module.
    TraitOrphanInstance {
        trait_name: String,
        head: Type,
        module: String,
    },
    /// **Resolution diverges (`RN-E0233 trait.resolution-diverges`, R6).** An
    /// instance fails the Paterson conditions — a `requires` context constraint is
    /// not structurally smaller than the head, so resolution could loop
    /// (`trait-resolution.md` §5). Names the offending context + why. Also raised
    /// by the resolution depth-budget backstop.
    TraitResolutionDiverges {
        trait_name: String,
        head: Type,
        context: Constraint,
        why: String,
    },
    /// **Ambiguous constraint (`RN-E0234 trait.ambiguous`, R7).** A constraint's
    /// type variable is not determined by the term's visible type — it appears
    /// only inside the constraint, never in the result type — so nothing pins it.
    /// No defaulting (`trait-resolution.md` §6).
    TraitAmbiguous {
        constraint: Constraint,
    },
    /// **Superclass unsatisfied (`RN-E0236 trait.superclass-unsatisfied`).** A
    /// matching instance was found for a constraint, but one of its `requires`
    /// superclass sub-obligations has no instance (`trait-resolution.md` §1.4, R1
    /// step 2 context). Names the outer constraint and the missing superclass.
    TraitSuperclassUnsatisfied {
        constraint: Constraint,
        superclass: Constraint,
    },
    /// **Duplicate instance (`RN-E0237 trait.duplicate-instance`).** The *same*
    /// `(trait, head)` instance is declared twice — degenerate overlap
    /// (`trait-resolution.md` §2). Names the trait and head.
    TraitDuplicateInstance {
        trait_name: String,
        head: Type,
    },
    /// **Traits-v1 unsupported construct (`RN-E0246 trait.v1-unsupported`).** The
    /// program is well-typed and would resolve, but its dictionary-passing
    /// *lowering* is not yet implemented in traits v1 — so rather than miscompile
    /// (a self-call applied to a value as if it were the dictionary) or fail with a
    /// cryptic downstream error (an unbound `$dict$` / a misleading "needs a sum
    /// type"), it is rejected **loud and clear** at the earliest clean point. Two
    /// constructs reach here in v1:
    ///
    /// 1. a **recursive constrained generic** — a `let rec` whose generalized
    ///    scheme carries trait `constraints` (its self-call was checked
    ///    monomorphically, so the dict-passing transform mis-threads it);
    /// 2. a **generic-instance use** — an obligation resolves to an `instance`
    ///    whose **head is non-ground** (contains a type variable, e.g.
    ///    `Show [a]`), which v1 cannot build a runtime dictionary for.
    ///
    /// The code is shared; `what` names the specific construct and the workaround.
    /// (Declaring a generic instance stays legal — Paterson etc. still check it;
    /// only *using* it at runtime is rejected.) The code sits at `RN-E0246`, the
    /// first free slot adjacent to the (full) `RN-E0230`–`RN-E0239` trait block.
    TraitV1Unsupported {
        what: String,
    },
}

impl TypeErr {
    /// A short, stable kind tag.
    pub fn kind(&self) -> &'static str {
        match self {
            TypeErr::Unbound(_) => "Unbound",
            TypeErr::NotAFunction(_) => "NotAFunction",
            TypeErr::NotCode(_) => "NotCode",
            TypeErr::Mismatch { .. } => "Mismatch",
            TypeErr::StageEscape { .. } => "StageEscape",
            TypeErr::StageMisuse { .. } => "StageMisuse",
            TypeErr::BareExtern(_) => "BareExtern",
            TypeErr::NotIndexable(_) => "NotIndexable",
            TypeErr::DupField(_) => "DupField",
            TypeErr::NotRecord(_) => "NotRecord",
            TypeErr::NoField(..) => "NoField",
            TypeErr::NotArray(_) => "NotArray",
            TypeErr::UnknownCtor(_) => "UnknownCtor",
            TypeErr::CtorArity { .. } => "CtorArity",
            TypeErr::LoopArity { .. } => "LoopArity",
            TypeErr::ArityMismatch { .. } => "ArityMismatch",
            TypeErr::NotASum(_) => "NotASum",
            TypeErr::NonExhaustive { .. } => "NonExhaustive",
            TypeErr::EmptyMatch => "EmptyMatch",
            TypeErr::RepresentationPolymorphicLayout => "RepresentationPolymorphicLayout",
            TypeErr::WideTypeVariable { .. } => "WideTypeVariable",
            TypeErr::SealLeak { .. } => "SealLeak",
            TypeErr::SealUnhandled { .. } => "SealUnhandled",
            TypeErr::ModuleSealLeak { .. } => "ModuleSealLeak",
            TypeErr::DuplicateTypeParam { .. } => "DuplicateTypeParam",
            TypeErr::DuplicateConstructor { .. } => "DuplicateConstructor",
            TypeErr::AsmGcType { .. } => "AsmGcType",
            TypeErr::MutNonScalar { .. } => "MutNonScalar",
            TypeErr::MutEscapes { .. } => "MutEscapes",
            TypeErr::MutAssignImmutable { .. } => "MutAssignImmutable",
            TypeErr::RefPointerContent { .. } => "RefPointerContent",
            TypeErr::TraitResolutionNYIMP { .. } => "TraitResolutionNYIMP",
            TypeErr::TraitNoMethod { .. } => "TraitNoMethod",
            TypeErr::TraitMethodRowViolation { .. } => "TraitMethodRowViolation",
            TypeErr::TraitMissingMethod { .. } => "TraitMissingMethod",
            TypeErr::TraitNoInstance { .. } => "TraitNoInstance",
            TypeErr::TraitOverlappingInstances { .. } => "TraitOverlappingInstances",
            TypeErr::TraitOrphanInstance { .. } => "TraitOrphanInstance",
            TypeErr::TraitResolutionDiverges { .. } => "TraitResolutionDiverges",
            TypeErr::TraitAmbiguous { .. } => "TraitAmbiguous",
            TypeErr::TraitSuperclassUnsatisfied { .. } => "TraitSuperclassUnsatisfied",
            TypeErr::TraitDuplicateInstance { .. } => "TraitDuplicateInstance",
            TypeErr::TraitV1Unsupported { .. } => "TraitV1Unsupported",
        }
    }

    /// A stable diagnostic **code** (`RN-Exxxx`; design §8).
    pub fn code(&self) -> &'static str {
        match self {
            TypeErr::Unbound(_) => "RN-E0101",
            TypeErr::NotAFunction(_) => "RN-E0201",
            TypeErr::NotCode(_) => "RN-E0202",
            TypeErr::Mismatch { .. } => "RN-E0203",
            TypeErr::StageEscape { .. } => "RN-E0301",
            TypeErr::StageMisuse { .. } => "RN-E0302",
            TypeErr::BareExtern(_) => "RN-E0401",
            TypeErr::NotIndexable(_) => "RN-E0204",
            TypeErr::DupField(_) => "RN-E0210",
            TypeErr::NotRecord(_) => "RN-E0211",
            TypeErr::NoField(..) => "RN-E0212",
            TypeErr::NotArray(_) => "RN-E0213",
            TypeErr::UnknownCtor(_) => "RN-E0220",
            TypeErr::CtorArity { .. } => "RN-E0221",
            TypeErr::LoopArity { .. } => "RN-E0228",
            TypeErr::NotASum(_) => "RN-E0222",
            TypeErr::NonExhaustive { .. } => "RN-E0223",
            TypeErr::EmptyMatch => "RN-E0224",
            TypeErr::ArityMismatch { .. } => "RN-E0225",
            TypeErr::RepresentationPolymorphicLayout => "RN-E0226",
            TypeErr::WideTypeVariable { .. } => "RN-E0227",
            TypeErr::SealLeak { .. }
            | TypeErr::SealUnhandled { .. }
            | TypeErr::ModuleSealLeak { .. } => "RN-E0403",
            TypeErr::DuplicateTypeParam { .. } | TypeErr::DuplicateConstructor { .. } => "RN-E0229",
            TypeErr::AsmGcType { .. } => "RN-E0405",
            // Canonical numbering (mutability.md §8): RN-E0241 `mut.escapes` is
            // the cell-escapes-its-scope code. The other two v1 checks are
            // v1-specific (the full Ref design has no equivalent), so they take
            // fresh codes from the reserved RN-E0244–0249 mutability range, leaving
            // RN-E0240/0242/0243 for the future first-class `Ref` errors.
            TypeErr::MutEscapes { .. } => "RN-E0241",
            TypeErr::MutNonScalar { .. } => "RN-E0244",
            TypeErr::MutAssignImmutable { .. } => "RN-E0245",
            // Pointer-typed `Ref` deferred to Sprint 3 (needs the GC write barrier).
            // RN-E0247 from the mutability family's reserved tail (E0246 ceded to
            // traits, E0247–0249 reserved — `docs/mutability.md` §8).
            TypeErr::RefPointerContent { .. } => "RN-E0247",
            // Traits / qualified types v1 (trait-resolution.md §8, range
            // RN-E0230–RN-E0239). Sprint 1 wires E0230 (the resolution-NYIMP
            // placeholder, replaced by real entailment in Sprint 2), E0235, E0238,
            // E0239.
            TypeErr::TraitResolutionNYIMP { .. } => "RN-E0230",
            TypeErr::TraitNoMethod { .. } => "RN-E0235",
            TypeErr::TraitMethodRowViolation { .. } => "RN-E0238",
            TypeErr::TraitMissingMethod { .. } => "RN-E0239",
            // Sprint 2 (trait-resolution.md §2–§6 + §8).
            TypeErr::TraitNoInstance { .. } => "RN-E0230",
            TypeErr::TraitOverlappingInstances { .. } => "RN-E0231",
            TypeErr::TraitOrphanInstance { .. } => "RN-E0232",
            TypeErr::TraitResolutionDiverges { .. } => "RN-E0233",
            TypeErr::TraitAmbiguous { .. } => "RN-E0234",
            TypeErr::TraitSuperclassUnsatisfied { .. } => "RN-E0236",
            TypeErr::TraitDuplicateInstance { .. } => "RN-E0237",
            // Traits v1 NYIMP-lowering limit (trait-resolution.md §8). The trait
            // block RN-E0230–RN-E0239 is full and all in use, so this takes the
            // first free adjacent slot (RN-E0246), carved from the mutability
            // reserved tail (mutability.md §8 amended to 0247–0249).
            TypeErr::TraitV1Unsupported { .. } => "RN-E0246",
        }
    }

    /// The catalog **slug** — the human-readable dotted name paired with the
    /// `RN-Exxxx` code (e.g. `RN-E0223 match.non-exhaustive`). Mirrors [`code`]
    /// arm-for-arm; the two must stay in lockstep with `docs/error-catalog.md`.
    pub fn slug(&self) -> &'static str {
        match self {
            TypeErr::Unbound(_) => "scope.unbound",
            TypeErr::NotAFunction(_) => "type.not-a-function",
            TypeErr::NotCode(_) => "stage.not-code",
            TypeErr::Mismatch { .. } => "type.mismatch",
            TypeErr::StageEscape { .. } => "stage.escape",
            TypeErr::StageMisuse { .. } => "stage.misuse",
            TypeErr::BareExtern(_) => "extern.bare",
            TypeErr::NotIndexable(_) => "mem.not-indexable",
            TypeErr::DupField(_) => "record.duplicate-field",
            TypeErr::NotRecord(_) => "record.not-a-record",
            TypeErr::NoField(..) => "record.no-field",
            TypeErr::NotArray(_) => "array.not-an-array",
            TypeErr::UnknownCtor(_) => "sum.unknown-constructor",
            TypeErr::CtorArity { .. } => "sum.constructor-arity",
            TypeErr::LoopArity { .. } => "loop.step-arity",
            TypeErr::NotASum(_) => "match.not-a-sum",
            TypeErr::NonExhaustive { .. } => "match.non-exhaustive",
            TypeErr::EmptyMatch => "match.empty",
            TypeErr::ArityMismatch { .. } => "type.arity-mismatch",
            TypeErr::RepresentationPolymorphicLayout => "layout.representation-polymorphic",
            TypeErr::WideTypeVariable { .. } => "kind.wide-into-traced-slot",
            TypeErr::SealLeak { .. }
            | TypeErr::SealUnhandled { .. }
            | TypeErr::ModuleSealLeak { .. } => "cap.seal-leak",
            TypeErr::DuplicateTypeParam { .. } | TypeErr::DuplicateConstructor { .. } => {
                "type.malformed-decl"
            }
            TypeErr::AsmGcType { .. } => "cap.asm-gc-type",
            TypeErr::MutNonScalar { .. } => "mut.non-scalar",
            TypeErr::MutEscapes { .. } => "mut.escapes",
            TypeErr::MutAssignImmutable { .. } => "mut.assign-immutable",
            TypeErr::RefPointerContent { .. } => "ref.pointer-content",
            TypeErr::TraitResolutionNYIMP { .. } => "trait.no-instance",
            TypeErr::TraitNoMethod { .. } => "trait.no-method",
            TypeErr::TraitMethodRowViolation { .. } => "trait.method-row-violation",
            TypeErr::TraitMissingMethod { .. } => "trait.missing-method",
            TypeErr::TraitNoInstance { .. } => "trait.no-instance",
            TypeErr::TraitOverlappingInstances { .. } => "trait.overlapping-instances",
            TypeErr::TraitOrphanInstance { .. } => "trait.orphan-instance",
            TypeErr::TraitResolutionDiverges { .. } => "trait.resolution-diverges",
            TypeErr::TraitAmbiguous { .. } => "trait.ambiguous",
            TypeErr::TraitSuperclassUnsatisfied { .. } => "trait.superclass-unsatisfied",
            TypeErr::TraitDuplicateInstance { .. } => "trait.duplicate-instance",
            TypeErr::TraitV1Unsupported { .. } => "trait.v1-unsupported",
        }
    }

    /// The calculus section that defines this rule (spec-citing, design §8).
    pub fn spec(&self) -> &'static str {
        match self {
            TypeErr::Unbound(_) => "calculus §2.1 (var)",
            TypeErr::NotAFunction(_) | TypeErr::Mismatch { .. } => "calculus §2.1 (app/bind)",
            TypeErr::NotCode(_) => "calculus §3.3 (splice)",
            TypeErr::StageEscape { .. } => "calculus §9 (SO-1)",
            TypeErr::StageMisuse { .. } => "calculus §3.0 (single-stage)",
            TypeErr::BareExtern(_) => "design §3 (the FFI surface)",
            TypeErr::NotIndexable(_) => "design §3 (the mem accessor)",
            TypeErr::DupField(_) | TypeErr::NotRecord(_) | TypeErr::NoField(..) => {
                "data types (records)"
            }
            TypeErr::NotArray(_) => "data types (arrays)",
            TypeErr::UnknownCtor(_)
            | TypeErr::CtorArity { .. }
            | TypeErr::ArityMismatch { .. }
            | TypeErr::NotASum(_)
            | TypeErr::NonExhaustive { .. }
            | TypeErr::EmptyMatch => "data types (sum types)",
            TypeErr::LoopArity { .. } => "structured loops",
            TypeErr::RepresentationPolymorphicLayout | TypeErr::WideTypeVariable { .. } => {
                "data layout (polymorphic representation)"
            }
            TypeErr::SealLeak { .. }
            | TypeErr::SealUnhandled { .. }
            | TypeErr::ModuleSealLeak { .. } => {
                "calculus §5 (SEAL) / capabilities (sealing rule 2)"
            }
            TypeErr::DuplicateTypeParam { .. } | TypeErr::DuplicateConstructor { .. } => {
                "data types (type-declaration well-formedness)"
            }
            TypeErr::AsmGcType { .. } => "jasm-boundary-layer §A6 (asm GC-safety) / capabilities",
            TypeErr::MutNonScalar { .. } => "mutability §1.1/§6 (scalar-only v1) / mutability.md",
            TypeErr::MutEscapes { .. } => "mutability §2–§3 (the runST/st escape boundary)",
            TypeErr::MutAssignImmutable { .. } => {
                "mutability §1/§3 (only a `let mut` is assignable)"
            }
            TypeErr::RefPointerContent { .. } => {
                "mutability §6.1 (pointer-typed Ref needs the GC write barrier — Sprint 3)"
            }
            TypeErr::TraitResolutionNYIMP { .. } => "trait-resolution §1 (R1 entailment)",
            TypeErr::TraitNoMethod { .. } => "trait-resolution §1.1 (trait methods)",
            TypeErr::TraitMethodRowViolation { .. } => "trait-resolution §7.3 (method latent row)",
            TypeErr::TraitMissingMethod { .. } => "trait-resolution §1.1 (instance methods)",
            TypeErr::TraitNoInstance { .. } => "trait-resolution §1.2 (R1 entailment)",
            TypeErr::TraitOverlappingInstances { .. } => "trait-resolution §3 (R4 overlap)",
            TypeErr::TraitOrphanInstance { .. } => "trait-resolution §4 (R5 orphans)",
            TypeErr::TraitResolutionDiverges { .. } => "trait-resolution §5 (R6 Paterson)",
            TypeErr::TraitAmbiguous { .. } => "trait-resolution §6 (R7 ambiguity)",
            TypeErr::TraitSuperclassUnsatisfied { .. } => "trait-resolution §1.4 (R1 superclass)",
            TypeErr::TraitDuplicateInstance { .. } => "trait-resolution §2 (R3 coherence)",
            TypeErr::TraitV1Unsupported { .. } => "trait-resolution §8 (v1 lowering scope)",
        }
    }

    /// A suggested next step, when there is an obvious one.
    pub fn hint(&self) -> Option<String> {
        match self {
            TypeErr::StageMisuse { what, .. } if *what == "quote" => {
                Some("`quote` builds code at the generation stage — try `--stage 1`".into())
            }
            TypeErr::StageMisuse { what, .. } if *what == "splice" => {
                Some("`splice` / `genlet` belong at the object stage, inside a `quote`".into())
            }
            TypeErr::NotCode(_) => Some(
                "`splice` / `genlet` expect a `Code[..]` value — did you `quote(...)` it?".into(),
            ),
            TypeErr::BareExtern(_) => Some(
                "give it an explicit `: T`, or build/run with `locusc` — it resolves the \
                 signature from the Win32 oracle"
                    .into(),
            ),
            TypeErr::Mismatch { expected, found }
                if type_mentions_overflow(expected) || type_mentions_overflow(found) =>
            {
                Some(
                    "checked integer arithmetic (`+?`, `-?`, `*?`) carries `exn[Overflow]`; \
                     use explicit wrapping (`+%`, `-%`, `*%`) or allow `! {exn[Overflow]}`"
                        .into(),
                )
            }
            TypeErr::NotIndexable(_) => Some(
                "`a[i]` reads raw memory — `a` must be a `String` (16-bit units), a `Ptr`, \
                 or an `Int` address (bytes)"
                    .into(),
            ),
            TypeErr::RepresentationPolymorphicLayout => Some(
                "instantiate or specialize this generic allocation before asking for IR/LLVM"
                    .into(),
            ),
            TypeErr::WideTypeVariable { .. } => Some(
                "wide values are allowed on the stack and in untraced concrete containers, \
                 but not in a traced `Var` word cell; use `Array[Float]` or monomorphize \
                 the containing type"
                    .into(),
            ),
            TypeErr::SealLeak { label, .. } if *label == Label::Gc => Some(
                "a `nogc` / `seal gc` region must not let a gc-managed value (array, sum, \
                 tuple, record) or a still-allocating closure escape — consume it inside, \
                 or return a scalar"
                    .into(),
            ),
            TypeErr::SealLeak { label, .. } => Some(format!(
                "the region performs `{label}` internally but must not expose it — return an \
                 abstraction over `{label}`, not a value (closure/handler) that still performs it"
            )),
            TypeErr::SealUnhandled { label } => Some(format!(
                "`{label}` is a user effect / exception that escapes to the caller unless \
                 handled — `handle` it inside the region, or only seal runtime powers \
                 (`gc`, `mem`, `winapi`) that discharge at the boundary"
            )),
            TypeErr::ModuleSealLeak {
                module,
                binding,
                label,
                ..
            } => Some(format!(
                "module `{module}` must not expose `{binding}` carrying `{label}` — wrap the raw \
                 power in a handler and export an abstract effect (as `Console` does for \
                 `winapi`), or drop `{binding}` from `exposing (…)`"
            )),
            TypeErr::Unbound(name) => Some(format!(
                "`{name}` is not in scope — bind it with `let`, add it as a parameter, import \
                 the module that defines it, or fix the spelling"
            )),
            TypeErr::NonExhaustive { missing, .. } => Some(format!(
                "add an arm for `{missing}` — every constructor must be covered — or a `_` \
                 wildcard (or variable) pattern to catch the remaining cases"
            )),
            TypeErr::UnknownCtor(name) => Some(format!(
                "no in-scope `type` declares `{name}` — declare or import the `type` that \
                 defines it, or fix the spelling"
            )),
            TypeErr::CtorArity {
                ctor,
                expected,
                found,
            } => Some(format!(
                "`{ctor}` takes {expected} field(s) but got {found} — match the declared field count"
            )),
            TypeErr::ArityMismatch {
                name,
                expected,
                found,
            } => Some(format!(
                "`{name}` takes {expected} type argument(s) but got {found} — give the declared \
                 number of type arguments"
            )),
            TypeErr::LoopArity { expected, found } => Some(format!(
                "the loop has {expected} accumulator(s) but {found} `do` step expression(s) — \
                 provide exactly one next-value expression per accumulator"
            )),
            TypeErr::DupField(name) => Some(format!(
                "the record repeats field `{name}` — drop or rename the duplicate"
            )),
            TypeErr::DuplicateTypeParam { ty, param } => Some(format!(
                "`type {ty}` repeats the parameter `{param}` — rename it so each type parameter \
                 is distinct"
            )),
            TypeErr::DuplicateConstructor { ty, ctor } => Some(format!(
                "`type {ty}` declares constructor `{ctor}` twice — rename or remove the duplicate"
            )),
            TypeErr::AsmGcType { ty } => Some(format!(
                "a moving collector could relocate `{ty}` underneath GC-blind asm — pass a scalar \
                 or a raw `Ptr`, or copy into untraced memory at the boundary; pinning a managed \
                 datum for asm is not yet supported"
            )),
            TypeErr::MutNonScalar { ty } => Some(format!(
                "`let mut` v1 holds only a scalar (`Int`/`Float`/`Bool`), not `{ty}` — a \
                 mutable gc-managed cell needs the heap-`Ref[T]` / `st[T]` path (a later effort); \
                 keep the value immutable, or mutate a scalar field of it"
            )),
            TypeErr::MutEscapes { ty } => Some(format!(
                "a `let mut` cell must not escape its scope, but the body's result `{ty}` carries \
                 it — return a scalar snapshot of the final value, or use an explicit `Ref[T]` \
                 (and accept `st[T]`) once it lands"
            )),
            TypeErr::MutAssignImmutable { name } => Some(format!(
                "`{name}` is not an assignable mutable local — bind it with `let mut {name} = …` \
                 to allow `{name} := …`, or it is simply out of scope"
            )),
            TypeErr::RefPointerContent { ty } => Some(format!(
                "a pointer-typed `Ref[{ty}]` needs the GC write barrier — Sprint 3; use a scalar \
                 `Ref` (`Ref[Int]`/`Ref[Float]`/`Ref[Bool]`/`Ref[Unit]`) or await it"
            )),
            TypeErr::TraitResolutionNYIMP { constraint } => Some(format!(
                "the constraint `{constraint}` is recorded but trait resolution is not yet \
                 implemented (Sprint 2) — for now, only fully-unconstrained generic code runs; \
                 Sprint 2 discharges this against an `instance {constraint}`"
            )),
            TypeErr::TraitNoMethod { method } => Some(format!(
                "`{method}` is used as a trait method but no in-scope `trait` declares it — \
                 import the trait, or check the method name"
            )),
            TypeErr::TraitMethodRowViolation {
                trait_name,
                method,
                ..
            } => Some(format!(
                "the `{method}` body in this `instance {trait_name}` does not match the trait's \
                 declared `{method}` signature — match its argument/result types and latent row"
            )),
            TypeErr::TraitMissingMethod {
                trait_name,
                method,
                missing,
            } => Some(if *missing {
                format!(
                    "this `instance {trait_name}` omits the method `{method}` — implement exactly \
                     the trait's methods"
                )
            } else {
                format!(
                    "this `instance {trait_name}` implements `{method}`, which the trait does not \
                     declare — implement exactly the trait's methods"
                )
            }),
            TypeErr::TraitNoInstance { constraint } => Some(format!(
                "no in-scope `instance {constraint}` and no dictionary in scope satisfies it — \
                 write `instance {constraint}` (in the trait's or the type's module), or import \
                 the module that defines it"
            )),
            TypeErr::TraitOverlappingInstances {
                trait_name,
                head1,
                head2,
            } => Some(format!(
                "instances `{trait_name} {head1}` and `{trait_name} {head2}` overlap (their heads \
                 unify) — v1 forbids overlap; remove or merge one, or give one a distinct head via \
                 a newtype"
            )),
            TypeErr::TraitOrphanInstance {
                trait_name,
                head,
                module,
            } => Some(format!(
                "`instance {trait_name} {head}` lives in module `{module}`, which defines neither \
                 the trait `{trait_name}` nor the type `{head}` — move it to one of them, or wrap \
                 `{head}` in a newtype this module owns"
            )),
            TypeErr::TraitResolutionDiverges {
                trait_name,
                head,
                context,
                why,
            } => Some(format!(
                "`instance {trait_name} {head}`: context `{context}` {why} — make each context \
                 constraint structurally smaller than the head (e.g. `Show a`, not `Show [a]`)"
            )),
            TypeErr::TraitAmbiguous { constraint } => Some(format!(
                "the constraint `{constraint}`'s type variable is not determined by the term's \
                 type — add a type annotation that pins it (e.g. `read s : Int`); v1 does no \
                 implicit defaulting"
            )),
            TypeErr::TraitSuperclassUnsatisfied {
                constraint,
                superclass,
            } => Some(format!(
                "resolving `{constraint}` needs its superclass `{superclass}`, which has no \
                 instance — add the missing `instance {superclass}` (e.g. `Eq T` before `Ord T`)"
            )),
            TypeErr::TraitDuplicateInstance { trait_name, head } => Some(format!(
                "`instance {trait_name} {head}` is declared twice — delete the duplicate"
            )),
            TypeErr::TraitV1Unsupported { what } => Some(format!(
                "{what} — this is a traits-v1 limitation; the construct is well-typed but its \
                 dictionary-passing lowering is not yet implemented"
            )),
            _ => None,
        }
    }
}

fn row_mentions_overflow(row: &Row) -> bool {
    row.labels()
        .any(|label| matches!(label, Label::Exn(name) if name == "Overflow"))
}

fn type_mentions_overflow(ty: &Type) -> bool {
    match ty {
        Type::Fun(a, b, row) => {
            row_mentions_overflow(row) || type_mentions_overflow(a) || type_mentions_overflow(b)
        }
        Type::Code(a, row) => row_mentions_overflow(row) || type_mentions_overflow(a),
        Type::Array(a) => type_mentions_overflow(a),
        Type::Vector(_, a) => type_mentions_overflow(a),
        Type::Mask(_) => false,
        Type::Tuple(fields) => fields.iter().any(type_mentions_overflow),
        Type::Record(fields) => fields
            .iter()
            .any(|(_, field)| type_mentions_overflow(field)),
        Type::Named(_, args) => args.iter().any(type_mentions_overflow),
        _ => false,
    }
}

impl std::fmt::Display for TypeErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeErr::Unbound(x) => write!(f, "unbound variable `{x}`"),
            TypeErr::NotAFunction(t) => write!(f, "expected a function, found `{t}`"),
            TypeErr::NotCode(t) => write!(f, "expected code `Code[..]`, found `{t}`"),
            TypeErr::Mismatch { expected, found } => {
                write!(f, "type mismatch: expected `{expected}`, found `{found}`")
            }
            TypeErr::StageEscape { var, bound, used } => write!(
                f,
                "stage escape: `{var}` is bound at stage {bound} but used at stage {used} \
                 (the generator cannot read a runtime binder — SO-1)"
            ),
            TypeErr::StageMisuse { what, at } => write!(
                f,
                "`{what}` at stage {at}: single-stage allows `quote` only at generation \
                 and `splice` only at the object stage"
            ),
            TypeErr::BareExtern(sym) => write!(
                f,
                "`extern {sym:?}` has no signature — the Win32 oracle resolves it under `locusc`, \
                 not the std-only checker"
            ),
            TypeErr::NotIndexable(t) => {
                write!(
                    f,
                    "cannot index a value of type `{t}` — `a[i]` needs a string or address"
                )
            }
            TypeErr::DupField(x) => write!(f, "record field `{x}` is given more than once"),
            TypeErr::NotRecord(t) => write!(f, "`.field` needs a record, found `{t}`"),
            TypeErr::NoField(x, t) => write!(f, "record `{t}` has no field `{x}`"),
            TypeErr::NotArray(t) => write!(f, "`len` needs an array, found `{t}`"),
            TypeErr::UnknownCtor(c) => write!(
                f,
                "unknown constructor `{c}` — no `type` in scope declares it"
            ),
            TypeErr::CtorArity {
                ctor,
                expected,
                found,
            } => {
                write!(
                    f,
                    "constructor `{ctor}` takes {expected} field(s), got {found}"
                )
            }
            TypeErr::LoopArity { expected, found } => {
                write!(
                    f,
                    "`loop` has {expected} accumulator(s), but {found} step expression(s)"
                )
            }
            TypeErr::ArityMismatch {
                name,
                expected,
                found,
            } => {
                write!(
                    f,
                    "type `{name}` takes {expected} type argument(s), got {found}"
                )
            }
            TypeErr::NotASum(t) => write!(f, "`match` needs a sum type, found `{t}`"),
            TypeErr::NonExhaustive { ty, missing } => {
                write!(
                    f,
                    "non-exhaustive `match` on `{ty}`: constructor `{missing}` not covered"
                )
            }
            TypeErr::EmptyMatch => write!(f, "`match` has no arms"),
            TypeErr::RepresentationPolymorphicLayout => {
                write!(f, "cannot lower representation-polymorphic layout yet")
            }
            TypeErr::WideTypeVariable { ty } => write!(
                f,
                "wide type `{ty}` cannot inhabit the current uniform representation slot"
            ),
            TypeErr::SealLeak { label, ty } => write!(
                f,
                "sealed effect `{label}` escapes the seal through the result type `{ty}` \
                 — a `seal {label}` region may not expose a value carrying `{label}`"
            ),
            TypeErr::SealUnhandled { label } => write!(
                f,
                "cannot seal `{label}`: it is still performed unhandled in the region, so \
                 removing it would hide an effect that escapes at runtime"
            ),
            TypeErr::ModuleSealLeak {
                module,
                binding,
                label,
                ty,
            } => write!(
                f,
                "module `{module}` seals `{label}`, but its exported binding `{binding}` has \
                 type `{ty}`, which carries `{label}` — a sealed power may not appear in an export"
            ),
            TypeErr::DuplicateTypeParam { ty, param } => write!(
                f,
                "type `{ty}` declares the type parameter `{param}` more than once"
            ),
            TypeErr::DuplicateConstructor { ty, ctor } => write!(
                f,
                "type `{ty}` declares the constructor `{ctor}` more than once"
            ),
            TypeErr::AsmGcType { ty } => write!(
                f,
                "an `extern asm` signature may not pass a gc-managed value `{ty}` — Layer-0 asm \
                 is GC-blind (no safepoints or read barriers)"
            ),
            TypeErr::MutNonScalar { ty } => write!(
                f,
                "a `let mut` local must be a scalar (`Int`/`Float`/`Bool`), found `{ty}` \
                 — mutability v1 is scalar-only"
            ),
            TypeErr::MutEscapes { ty } => write!(
                f,
                "a `let mut` cell escapes its scope through the result type `{ty}` \
                 — a mutable local may not be exposed (it is a sealed, non-escaping cell)"
            ),
            TypeErr::MutAssignImmutable { name } => write!(
                f,
                "cannot assign to `{name}`: it is not a mutable local (`let mut`) — it is \
                 immutably bound or out of scope"
            ),
            TypeErr::RefPointerContent { ty } => write!(
                f,
                "a pointer-typed `Ref[{ty}]` is not supported yet — it needs the GC write \
                 barrier (Sprint 3); use a scalar `Ref` (`Int`/`Float`/`Bool`/`Unit`)"
            ),
            TypeErr::TraitResolutionNYIMP { constraint } => write!(
                f,
                "unresolved trait constraint `{constraint}`: trait resolution is not yet \
                 implemented (Sprint 2)"
            ),
            TypeErr::TraitNoMethod { method } => write!(
                f,
                "`{method}` is used as a trait method, but no in-scope `trait` declares it"
            ),
            TypeErr::TraitMethodRowViolation {
                trait_name,
                method,
                expected,
                found,
            } => write!(
                f,
                "instance `{trait_name}` method `{method}`: expected `{expected}`, found `{found}` \
                 — the body does not match the trait's declared method signature"
            ),
            TypeErr::TraitMissingMethod {
                trait_name,
                method,
                missing,
            } => {
                if *missing {
                    write!(
                        f,
                        "instance `{trait_name}` is missing the method `{method}` declared by the trait"
                    )
                } else {
                    write!(
                        f,
                        "instance `{trait_name}` implements `{method}`, which the trait does not declare"
                    )
                }
            }
            TypeErr::TraitNoInstance { constraint } => write!(
                f,
                "no instance for `{constraint}`: no in-scope dictionary and no matching `instance` \
                 discharge this constraint"
            ),
            TypeErr::TraitOverlappingInstances {
                trait_name,
                head1,
                head2,
            } => write!(
                f,
                "overlapping instances: `{trait_name} {head1}` and `{trait_name} {head2}` have \
                 heads that unify — v1 forbids overlap"
            ),
            TypeErr::TraitOrphanInstance {
                trait_name,
                head,
                module,
            } => write!(
                f,
                "orphan instance `{trait_name} {head}` in module `{module}`: an instance must be \
                 declared in the trait's module or the type's module"
            ),
            TypeErr::TraitResolutionDiverges {
                trait_name,
                head,
                context,
                why,
            } => write!(
                f,
                "instance `{trait_name} {head}` would not terminate: context `{context}` {why} \
                 (Paterson conditions, trait-resolution §5)"
            ),
            TypeErr::TraitAmbiguous { constraint } => write!(
                f,
                "ambiguous constraint `{constraint}`: its type variable is not determined by the \
                 term's type, and v1 does no implicit defaulting"
            ),
            TypeErr::TraitSuperclassUnsatisfied {
                constraint,
                superclass,
            } => write!(
                f,
                "unsatisfied superclass: resolving `{constraint}` requires `{superclass}`, which \
                 has no instance"
            ),
            TypeErr::TraitDuplicateInstance { trait_name, head } => write!(
                f,
                "duplicate instance `{trait_name} {head}`: the same `(trait, head)` is declared \
                 more than once"
            ),
            TypeErr::TraitV1Unsupported { what } => write!(f, "unsupported in traits v1: {what}"),
        }
    }
}

/// Infer `Γ ⊢ e : A ! E @ stage` — the **top-level** `(type, row)`. A thin
/// projection of [`elaborate`], which records the same judgment at *every*
/// node; callers needing the full decorated tree use [`crate::sema`] directly.
pub fn infer(sig: &Sig, ctx: &Ctx, stage: Stage, e: &Term) -> Result<(Type, Row), TypeErr> {
    let t = elaborate(sig, ctx, stage, e)?;
    Ok((t.ty, t.row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{BinOp, CastOp, Handler, Label, OpClause, OpSig, Return, Row, Term, Type};

    fn ctx() -> Ctx {
        Ctx::new()
    }
    fn nosig() -> Sig {
        Sig::new()
    }
    fn world(s: &str) -> Label {
        Label::World(s.to_string())
    }
    /// Runtime-stage inference (stage 0).
    fn at0(e: &Term) -> Result<(Type, Row), TypeErr> {
        infer(&nosig(), &ctx(), 0, e)
    }

    // ── slice 1/2: effects (now at the runtime stage) ───────────────────

    #[test]
    fn literals_are_pure() {
        let (t, r) = at0(&Term::Int(42)).unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());

        let (t, r) = at0(&Term::Float(1.25f64.to_bits())).unwrap();
        assert_eq!(t, Type::Float);
        assert!(r.is_pure());
    }

    #[test]
    fn bare_float_arithmetic_is_typed_as_float() {
        let e = Term::Bin(
            BinOp::Div,
            Box::new(Term::Float(3.0f64.to_bits())),
            Box::new(Term::Float(2.0f64.to_bits())),
        );
        let (t, r) = at0(&e).unwrap();
        assert_eq!(t, Type::Float);
        assert!(r.is_pure());
    }

    #[test]
    fn mixed_int_float_arithmetic_has_no_implicit_coercion() {
        let e = Term::Bin(
            BinOp::Add,
            Box::new(Term::Int(1)),
            Box::new(Term::Float(2.0f64.to_bits())),
        );
        assert!(matches!(
            at0(&e),
            Err(TypeErr::Mismatch {
                expected: Type::Float,
                found: Type::Int
            })
        ));
    }

    #[test]
    fn checked_overflow_spelling_is_integer_only() {
        let e = Term::Bin(
            BinOp::AddChecked,
            Box::new(Term::Float(1.0f64.to_bits())),
            Box::new(Term::Float(2.0f64.to_bits())),
        );
        assert!(matches!(
            at0(&e),
            Err(TypeErr::Mismatch {
                expected: Type::Int,
                found: Type::Float
            })
        ));
    }

    #[test]
    fn integer_division_surface_typechecks_but_is_pure() {
        let e = Term::Bin(BinOp::Div, Box::new(Term::Int(6)), Box::new(Term::Int(3)));
        let (t, r) = at0(&e).unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());
    }

    #[test]
    fn integer_remainder_surface_typechecks_but_is_pure() {
        let e = Term::Bin(BinOp::Mod, Box::new(Term::Int(7)), Box::new(Term::Int(3)));
        let (t, r) = at0(&e).unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());

        let f = Term::Bin(
            BinOp::Mod,
            Box::new(Term::Float(7.0f64.to_bits())),
            Box::new(Term::Float(3.0f64.to_bits())),
        );
        assert!(matches!(
            at0(&f),
            Err(TypeErr::Mismatch {
                expected: Type::Int,
                found: Type::Float
            })
        ));
    }

    #[test]
    fn bool_negation_surface_typechecks_but_is_pure() {
        let (t, r) = infer_src("~true").unwrap();
        assert_eq!(t, Type::Bool);
        assert!(r.is_pure());

        assert!(matches!(
            infer_src("~1"),
            Err(TypeErr::Mismatch {
                expected: Type::Bool,
                found: Type::Int
            })
        ));
    }

    #[test]
    fn endloop_surface_typechecks_as_unit() {
        let (t, r) = infer_src("loop i = 0 while i < 3 do i + 1 endloop").unwrap();
        assert_eq!(t, Type::Unit);
        assert!(r.is_pure());
    }

    #[test]
    fn loop_return_surface_typechecks_as_result() {
        let (t, r) =
            infer_src("loop i = 0, acc = 0 while i < 3 do i + 1, acc + i return acc").unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());
    }

    #[test]
    fn explicit_numeric_conversions_typecheck() {
        let (t, r) = at0(&Term::Cast(CastOp::ToFloat, Box::new(Term::Int(7)))).unwrap();
        assert_eq!(t, Type::Float);
        assert!(r.is_pure());

        let (t, r) = at0(&Term::Cast(
            CastOp::Floor,
            Box::new(Term::Float(7.5f64.to_bits())),
        ))
        .unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());

        let (t, r) = at0(&Term::Cast(
            CastOp::FromFloat32,
            Box::new(Term::Cast(
                CastOp::ToFloat32,
                Box::new(Term::Float(1.25f64.to_bits())),
            )),
        ))
        .unwrap();
        assert_eq!(t, Type::Float);
        assert!(r.is_pure());
    }

    #[test]
    fn explicit_numeric_conversions_reject_wrong_input_type() {
        let e = Term::Cast(CastOp::ToFloat, Box::new(Term::Float(1.0f64.to_bits())));
        assert!(matches!(
            at0(&e),
            Err(TypeErr::Mismatch {
                expected: Type::Int,
                found: Type::Float
            })
        ));
    }

    #[test]
    fn float_extern_signatures_are_accepted_by_fp_abi() {
        let ty = Type::Fun(Box::new(Type::Float), Box::new(Type::Float), Row::pure());
        let e = Term::Extern("sin".into(), Some(ty), None);
        let (t, r) = at0(&e).unwrap();
        assert_eq!(
            t,
            Type::Fun(
                Box::new(Type::Float),
                Box::new(Type::Float),
                Row::single(Label::World("winapi".into()))
            )
        );
        assert!(r.is_pure());
    }

    #[test]
    fn checked_overflow_arithmetic_carries_overflow_row() {
        let e = Term::Bin(
            BinOp::AddChecked,
            Box::new(Term::Int(1)),
            Box::new(Term::Int(2)),
        );
        let (t, r) = at0(&e).unwrap();
        assert_eq!(t, Type::Int);
        assert_eq!(r, Row::single(Label::Exn("Overflow".into())));
    }

    #[test]
    fn explicit_wrapping_arithmetic_is_pure() {
        let e = Term::Bin(
            BinOp::AddWrap,
            Box::new(Term::Int(i64::MAX)),
            Box::new(Term::Int(1)),
        );
        let (t, r) = at0(&e).unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());
    }

    #[test]
    fn pure_annotation_rejects_checked_overflow() {
        let ty = Type::Fun(Box::new(Type::Int), Box::new(Type::Int), Row::pure());
        let body = Term::Lam(
            "x".into(),
            Some(Type::Int),
            Box::new(Term::Bin(
                BinOp::AddChecked,
                Box::new(Term::Var("x".into())),
                Box::new(Term::Int(1)),
            )),
        );
        let e = Term::LetRec(
            "f".into(),
            ty,
            Box::new(body),
            Box::new(Term::App(
                Box::new(Term::Var("f".into())),
                Box::new(Term::Int(1)),
            )),
        );
        let err = at0(&e).expect_err("pure annotation must reject `+?`");
        assert!(matches!(err, TypeErr::Mismatch { .. }));
        assert!(
            err.hint()
                .as_deref()
                .is_some_and(|hint| hint.contains("exn[Overflow]")),
            "hint should explain checked overflow, got {err:?}"
        );
    }

    #[test]
    fn perform_adds_a_label() {
        let e = Term::Perform(world("console"), Box::new(Term::Unit));
        assert_eq!(at0(&e).unwrap().1, Row::single(world("console")));
    }

    #[test]
    fn let_unions_effects() {
        let e = Term::Let(
            "_".into(),
            Box::new(Term::Perform(world("fs"), Box::new(Term::Unit))),
            Box::new(Term::Perform(world("net"), Box::new(Term::Unit))),
        );
        assert_eq!(
            at0(&e).unwrap().1,
            Row::single(world("fs")).union(&Row::single(world("net")))
        );
    }

    #[test]
    fn latent_effect_fires_on_application() {
        let f = Term::Lam(
            "x".into(),
            Some(Type::Unit),
            Box::new(Term::Perform(Label::Gc, Box::new(Term::Var("x".into())))),
        );
        let app = Term::App(Box::new(f), Box::new(Term::Unit));
        assert_eq!(at0(&app).unwrap().1, Row::single(Label::Gc));
    }

    #[test]
    fn tuple_construction_performs_gc() {
        // `(1, 2)` allocates a heap struct, so its row carries `gc` — allocation
        // is the gc effect, visible in the type, not a hidden malloc.
        let t = Term::Tuple(vec![Term::Int(1), Term::Int(2)]);
        let (ty, row) = at0(&t).unwrap();
        assert_eq!(ty, Type::Tuple(vec![Type::Int, Type::Int]));
        assert_eq!(row, Row::single(Label::Gc));
    }

    #[test]
    fn record_construction_performs_gc() {
        let r = Term::Record(vec![("x".into(), Term::Int(1)), ("y".into(), Term::Int(2))]);
        assert_eq!(at0(&r).unwrap().1, Row::single(Label::Gc));
    }

    #[test]
    fn gc_propagates_through_a_binding() {
        // A `let` whose bound expression allocates surfaces `gc` on the whole
        // expression's row — the effect flows outward like any other.
        let e = Term::Let(
            "p".into(),
            Box::new(Term::Tuple(vec![Term::Int(1), Term::Int(2)])),
            Box::new(Term::Var("p".into())),
        );
        assert_eq!(at0(&e).unwrap().1, Row::single(Label::Gc));
    }

    #[test]
    fn unbound_variable_errors() {
        assert_eq!(
            at0(&Term::Var("nope".into())),
            Err(TypeErr::Unbound("nope".into()))
        );
    }

    // a one-op effect `Ask : () => Int`, plus a handler-builder, for slice 2.
    fn ask() -> Label {
        Label::User("Ask".into())
    }
    fn ask_sig() -> Sig {
        Sig::from([(
            ask(),
            OpSig {
                param: Type::Unit,
                result: Type::Int,
            },
        )])
    }
    fn handle_ask(scrutinee: Term, resume_with: Term) -> Term {
        Term::Handle(
            Box::new(scrutinee),
            Box::new(Handler {
                ops: vec![OpClause {
                    op: ask(),
                    arg: "x".into(),
                    resume: "resume".into(),
                    body: Box::new(Term::App(
                        Box::new(Term::Var("resume".into())),
                        Box::new(resume_with),
                    )),
                }],
                ret: Return {
                    var: "y".into(),
                    body: Box::new(Term::Var("y".into())),
                },
            }),
        )
    }

    #[test]
    fn handler_discharges_the_effect() {
        let e = handle_ask(Term::Perform(ask(), Box::new(Term::Unit)), Term::Int(7));
        let (t, r) = infer(&ask_sig(), &ctx(), 0, &e).unwrap();
        assert_eq!(t, Type::Int);
        assert!(
            r.is_pure(),
            "handling Ask discharges it — the row shrinks to ∅"
        );
    }

    #[test]
    fn handle_discharges_only_what_it_handles() {
        let body = Term::Let(
            "_".into(),
            Box::new(Term::Perform(world("fs"), Box::new(Term::Unit))),
            Box::new(Term::Perform(ask(), Box::new(Term::Unit))),
        );
        let e = handle_ask(body, Term::Int(0));
        assert_eq!(
            infer(&ask_sig(), &ctx(), 0, &e).unwrap().1,
            Row::single(world("fs")),
            "Ask discharged; the unhandled fs remains"
        );
    }

    #[test]
    fn resume_is_typed_by_the_ops_result() {
        let e = handle_ask(Term::Perform(ask(), Box::new(Term::Unit)), Term::Bool(true));
        assert_eq!(
            infer(&ask_sig(), &ctx(), 0, &e),
            Err(TypeErr::Mismatch {
                expected: Type::Int,
                found: Type::Bool
            }),
            "resume : Int -> R, so resume(true) must not typecheck"
        );
    }

    // ── slice 3: staging ────────────────────────────────────────────────

    /// Generation-stage inference (stage 1), where `quote` lives.
    fn at1(e: &Term) -> Result<(Type, Row), TypeErr> {
        infer(&nosig(), &ctx(), 1, e)
    }
    fn code(a: Type, row: Row) -> Type {
        Type::Code(Box::new(a), row)
    }

    #[test]
    fn quote_of_a_literal_is_pure_code() {
        // quote(1) @1  :  Code[Int ! ∅] ! ∅
        let (t, r) = at1(&Term::Quote(Box::new(Term::Int(1)))).unwrap();
        assert_eq!(t, code(Type::Int, Row::pure()));
        assert!(r.is_pure());
    }

    #[test]
    fn object_effect_stays_inside_the_code() {
        // quote(perform console ()) @1  :  Code[Unit ! {console}] ! ∅
        let q = Term::Quote(Box::new(Term::Perform(
            world("console"),
            Box::new(Term::Unit),
        )));
        let (t, r) = at1(&q).unwrap();
        assert_eq!(t, code(Type::Unit, Row::single(world("console"))));
        assert!(
            r.is_pure(),
            "an object effect stays in the □; the quote itself is pure"
        );
    }

    #[test]
    fn generative_effect_distributes_out() {
        // quote(perform Insert ()) @1  :  Code[Unit ! ∅] ! {Insert}
        let q = Term::Quote(Box::new(Term::Perform(Label::Insert, Box::new(Term::Unit))));
        let (t, r) = at1(&q).unwrap();
        assert_eq!(t, code(Type::Unit, Row::pure()));
        assert_eq!(
            r,
            Row::single(Label::Insert),
            "Insert (generative) comes out of the □"
        );
    }

    #[test]
    fn delta_splits_object_from_generative() {
        // quote( let _ = perform console () in perform Insert () ) @1
        //   ⇒ object {console} stays in the Code; generative {Insert} comes out
        let body = Term::Let(
            "_".into(),
            Box::new(Term::Perform(world("console"), Box::new(Term::Unit))),
            Box::new(Term::Perform(Label::Insert, Box::new(Term::Unit))),
        );
        let (t, r) = at1(&Term::Quote(Box::new(body))).unwrap();
        assert_eq!(t, code(Type::Unit, Row::single(world("console"))));
        assert_eq!(r, Row::single(Label::Insert));
    }

    #[test]
    fn splice_unwraps_code() {
        // ${ quote(1) } @0  :  Int ! ∅
        let e = Term::Splice(Box::new(Term::Quote(Box::new(Term::Int(1)))));
        let (t, r) = at0(&e).unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());
    }

    #[test]
    fn splice_runs_the_codes_object_effects_here() {
        // ${ quote(perform console ()) } @0  :  Unit ! {console}
        let inner = Term::Quote(Box::new(Term::Perform(
            world("console"),
            Box::new(Term::Unit),
        )));
        let e = Term::Splice(Box::new(inner));
        let (t, r) = at0(&e).unwrap();
        assert_eq!(t, Type::Unit);
        assert_eq!(
            r,
            Row::single(world("console")),
            "the code's object effect fires when spliced"
        );
    }

    #[test]
    fn quote_at_runtime_is_rejected() {
        // single-stage: you cannot build code at runtime.
        assert_eq!(
            at0(&Term::Quote(Box::new(Term::Int(1)))),
            Err(TypeErr::StageMisuse {
                what: "quote",
                at: 0
            })
        );
    }

    #[test]
    fn splice_outside_a_quote_is_rejected() {
        // single-stage: splice only at the object stage (no nested staging).
        let e = Term::Splice(Box::new(Term::Quote(Box::new(Term::Int(1)))));
        assert_eq!(
            at1(&e),
            Err(TypeErr::StageMisuse {
                what: "splice",
                at: 1
            })
        );
    }

    #[test]
    fn so1_generator_cannot_read_a_runtime_binder() {
        // x bound at runtime (stage 0); used at generation (stage 1) ⇒ escape.
        let c = Ctx::from([("x".to_string(), (Binding::Mono(Type::Int), 0))]);
        assert_eq!(
            infer(&nosig(), &c, 1, &Term::Var("x".into())),
            Err(TypeErr::StageEscape {
                var: "x".into(),
                bound: 0,
                used: 1
            })
        );
    }

    #[test]
    fn cross_stage_persistence_is_allowed() {
        // y bound at generation (stage 1); used at runtime (stage 0) ⇒ ok (CSP).
        let c = Ctx::from([("y".to_string(), (Binding::Mono(Type::Int), 1))]);
        let (t, r) = infer(&nosig(), &c, 0, &Term::Var("y".into())).unwrap();
        assert_eq!(t, Type::Int);
        assert!(r.is_pure());
    }

    // ── slice 4: let-insertion (genlet / loci) ──────────────────────────

    /// `genlet(quote(1))` — hoist the code `1`.
    fn genlet_one() -> Term {
        Term::Genlet(Box::new(Term::Quote(Box::new(Term::Int(1)))))
    }

    #[test]
    fn genlet_performs_insert() {
        // genlet(quote(1)) @1  :  Code[Int ! ∅] ! {Insert}
        let (t, r) = at1(&genlet_one()).unwrap();
        assert_eq!(t, code(Type::Int, Row::pure()));
        assert_eq!(
            r,
            Row::single(Label::Insert),
            "genlet performs the generative Insert effect"
        );
    }

    #[test]
    fn letloc_discharges_insert() {
        // letloc { genlet(quote(1)) } @1  ⇒  Insert discharged ⇒ row ∅
        let (t, r) = at1(&Term::Letloc(Box::new(genlet_one()))).unwrap();
        assert_eq!(t, code(Type::Int, Row::pure()));
        assert!(
            r.is_pure(),
            "the locus discharges Insert — the let lands here"
        );
    }

    #[test]
    fn splice_is_the_default_locus() {
        // ${ genlet(quote(1)) } @0  ⇒  the splice catches Insert ⇒ row ∅
        let (t, r) = at0(&Term::Splice(Box::new(genlet_one()))).unwrap();
        assert_eq!(t, Type::Int);
        assert!(
            r.is_pure(),
            "splice is the default locus — it discharges genlet's Insert"
        );
    }

    #[test]
    fn genlet_without_a_locus_leaves_insert_pending() {
        // genlet alone (no enclosing locus) ⇒ Insert remains in the row.
        // (At a program boundary this would be RN-E0313; that check is later.)
        assert_eq!(at1(&genlet_one()).unwrap().1, Row::single(Label::Insert));
    }

    #[test]
    fn genlet_requires_a_code_value() {
        assert_eq!(
            at1(&Term::Genlet(Box::new(Term::Int(1)))),
            Err(TypeErr::NotCode(Type::Int))
        );
    }

    #[test]
    fn genlet_at_runtime_is_rejected() {
        assert_eq!(
            at0(&genlet_one()),
            Err(TypeErr::StageMisuse {
                what: "genlet",
                at: 0
            })
        );
    }

    // ── T1: current conservative kind rule around uniform repr slots ──
    //
    // D3's language rule forbids `Wide` (`Float`/`Float32`/SIMD) only at traced
    // `Var`-cell stores. The current compiler still rejects some generic-call
    // instantiations conservatively because that ABI is tagged uniform-word.
    // The rejection is surfaced as `WideTypeVariable` (RN-E0227), *not* a generic
    // `Mismatch`.

    /// Infer the type of parsed `src` at the runtime stage.
    fn infer_src(src: &str) -> Result<(Type, Row), TypeErr> {
        let term = crate::parse::parse(src).expect("test source parses");
        at0(&term)
    }

    #[test]
    fn current_uniform_call_abi_rejects_id_applied_to_a_float() {
        // `id 3.0`: `id : ∀a. a -> a`, so the argument instantiates `a := Float`
        // on the current tagged uniform-word call ABI. D3 wants this allowed once
        // generic calls are widened/specialized; for now RN-E0227 is the
        // conservative guard.
        let err = infer_src("let id = fn x => x in id 3.0")
            .expect_err("current uniform call ABI rejects id 3.0");
        assert_eq!(
            err,
            TypeErr::WideTypeVariable { ty: Type::Float },
            "the conservative wide guard is RN-E0227, not a generic mismatch"
        );
        assert_eq!(err.code(), "RN-E0227");
        // The diagnostic must steer toward an untraced concrete container.
        assert!(
            err.hint()
                .as_deref()
                .is_some_and(|h| h.contains("Array[Float]")),
            "the kind diagnostic must steer to a container, got {:?}",
            err.hint()
        );
    }

    #[test]
    fn unbound_and_non_exhaustive_carry_actionable_hints() {
        // Transparency: the two most common hint-less errors now steer the user.
        // RN-E0101 names the binding and the ways to bring it into scope.
        let unbound = TypeErr::Unbound("foo".into());
        assert_eq!(unbound.code(), "RN-E0101");
        let h = unbound.hint().expect("unbound now has a hint");
        assert!(h.contains("foo") && h.contains("scope"), "got {h:?}");

        // RN-E0223 names the missing constructor and offers the wildcard escape.
        let non_exh = TypeErr::NonExhaustive {
            ty: "Option[Int]".into(),
            missing: "None".into(),
        };
        assert_eq!(non_exh.code(), "RN-E0223");
        let h = non_exh.hint().expect("non-exhaustive now has a hint");
        assert!(h.contains("None") && h.contains("_"), "got {h:?}");
    }

    #[test]
    fn catalog_specified_hints_are_now_emitted() {
        // Every error the catalog gives a non-"—" hint for must actually emit one,
        // interpolating the offending name/count so it is specific.
        let cases: &[(TypeErr, &[&str])] = &[
            (TypeErr::UnknownCtor("Foo".into()), &["Foo", "type"]),
            (
                TypeErr::CtorArity {
                    ctor: "Cons".into(),
                    expected: 2,
                    found: 1,
                },
                &["Cons", "2", "1"],
            ),
            (
                TypeErr::ArityMismatch {
                    name: "List".into(),
                    expected: 1,
                    found: 2,
                },
                &["List", "type argument"],
            ),
            (
                TypeErr::LoopArity {
                    expected: 2,
                    found: 1,
                },
                &["accumulator", "step"],
            ),
            (TypeErr::DupField("x".into()), &["x", "duplicate"]),
            (
                TypeErr::DuplicateConstructor {
                    ty: "T".into(),
                    ctor: "A".into(),
                },
                &["T", "A", "twice"],
            ),
        ];
        for (err, needles) in cases {
            let h = err
                .hint()
                .unwrap_or_else(|| panic!("{err:?} should have a hint now"));
            for n in *needles {
                assert!(h.contains(n), "hint for {err:?} missing {n:?}: {h:?}");
            }
        }
    }

    #[test]
    fn extern_asm_rejects_gc_managed_signatures_but_allows_scalars() {
        // A6 GC-safety gate: Layer-0 asm is GC-blind, so a movable gc datum may
        // not cross the boundary. An `Array` argument is rejected (RN-E0405)...
        let err = infer_src(r#"extern asm "f" : Array[Int] -> Int"#)
            .expect_err("asm may not take a gc-managed array");
        assert_eq!(err.code(), "RN-E0405");
        assert_eq!(err.slug(), "cap.asm-gc-type");
        assert!(matches!(err, TypeErr::AsmGcType { ty: Type::Array(_) }));

        // ...and so is a gc datum in the result position.
        let err2 = infer_src(r#"extern asm "g" : Int -> Array[Float]"#)
            .expect_err("asm may not return a gc-managed array");
        assert_eq!(err2.code(), "RN-E0405");

        // A scalar/Ptr ABI is GC-blind and fine — this is what every shipped
        // runtime asm primitive uses (rotl/popcount/mandel/getstdout).
        infer_src(r#"extern asm "ok" : Int -> Int"#).expect("a scalar asm signature is allowed");
        infer_src(r#"extern asm "ok2" : Float -> Float -> Int -> Int"#)
            .expect("a mixed scalar asm signature is allowed");
    }

    #[test]
    fn code_and_slug_agree_with_the_catalog() {
        // The `RN-Exxxx slug` style: each code is paired with its catalog slug.
        // Pin representative pairs across the families so code()/slug() can't drift.
        let pairs: &[(TypeErr, &str, &str)] = &[
            (TypeErr::Unbound("x".into()), "RN-E0101", "scope.unbound"),
            (
                TypeErr::NonExhaustive {
                    ty: "Option[Int]".into(),
                    missing: "None".into(),
                },
                "RN-E0223",
                "match.non-exhaustive",
            ),
            (
                TypeErr::WideTypeVariable { ty: Type::Float },
                "RN-E0227",
                "kind.wide-into-traced-slot",
            ),
            (
                TypeErr::SealUnhandled {
                    label: Label::World("winapi".into()),
                },
                "RN-E0403",
                "cap.seal-leak",
            ),
            (
                TypeErr::DuplicateTypeParam {
                    ty: "T".into(),
                    param: "a".into(),
                },
                "RN-E0229",
                "type.malformed-decl",
            ),
        ];
        for (err, code, slug) in pairs {
            assert_eq!(err.code(), *code, "code for {err:?}");
            assert_eq!(err.slug(), *slug, "slug for {err:?}");
        }
    }

    #[test]
    fn ill_typed_programs_fire_their_catalog_diagnostic() {
        // Diagnostics-completeness sweep: each minimal ill-typed program must
        // produce its stable `RN-Exxxx` code AND matching slug, so the catalog
        // (`docs/error-catalog.md`) can't silently drift from the compiler. One
        // row per diagnostic; extend as more triggers are added.
        let cases: &[(&str, &str, &str)] = &[
            ("let x = 1 in x 2", "RN-E0201", "type.not-a-function"),
            ("let b = true in b[0]", "RN-E0204", "mem.not-indexable"),
            ("{ a = 1, a = 2 }", "RN-E0210", "record.duplicate-field"),
            ("let n = 1 in n.x", "RN-E0211", "record.not-a-record"),
            ("let r = { a = 1 } in r.b", "RN-E0212", "record.no-field"),
            ("len 1", "RN-E0213", "array.not-an-array"),
            ("Nope", "RN-E0220", "sum.unknown-constructor"),
            (
                "loop a = 1, b = 2 while true do a + 1 else a",
                "RN-E0228",
                "loop.step-arity",
            ),
            (r#"extern "GetStdHandle""#, "RN-E0401", "extern.bare"),
        ];
        for (src, code, slug) in cases {
            let err = infer_src(src)
                .err()
                .unwrap_or_else(|| panic!("`{src}` should be rejected as {code}"));
            assert_eq!(err.code(), *code, "code for `{src}`");
            assert_eq!(err.slug(), *slug, "slug for `{src}`");
        }
    }

    #[test]
    fn a_scalar_let_mut_typechecks_at_its_type() {
        // mutability v1 (`docs/mutability-sprints.md` Sprint 2): a well-typed
        // `let mut` of a scalar type-checks, with the body's type/row as the
        // result. `let mut x = 1 in x` is `Int`, pure.
        let (ty, row) = infer_src("let mut x = 1 in x").expect("a scalar `let mut` type-checks");
        assert_eq!(ty, Type::Int);
        assert!(row.is_pure());
    }

    #[test]
    fn an_assignment_to_a_mutable_local_yields_unit() {
        // `x := 2` where `x` is a `let mut` binding: the assignment is an
        // expression yielding `Unit`; v1 assignment adds no effect (a stack store),
        // so the body row stays pure.
        let (ty, row) =
            infer_src("let mut x = 1 in (x := 2)").expect("assigning a mutable local type-checks");
        assert_eq!(ty, Type::Unit);
        assert!(row.is_pure());
        // The assigned value must match the cell's type (a unification demand).
        let err = infer_src("let mut x = 1 in (x := true)")
            .expect_err("assigning a Bool to an Int cell is a mismatch");
        assert_eq!(err.code(), "RN-E0203");
        assert!(matches!(err, TypeErr::Mismatch { .. }));
    }

    #[test]
    fn a_non_scalar_let_mut_is_rejected() {
        // mutability v1 is scalar-only (`Int`/`Float`/`Bool`); a `let mut` of an
        // array (a gc-managed datum) is `RN-E0244 mut.non-scalar`.
        let err = infer_src("let mut x = [1, 2] in x")
            .expect_err("a non-scalar mutable local is rejected in v1");
        assert_eq!(err.code(), "RN-E0244");
        assert_eq!(err.slug(), "mut.non-scalar");
        assert!(matches!(err, TypeErr::MutNonScalar { .. }));
    }

    #[test]
    fn assigning_an_immutable_let_binding_is_rejected() {
        // Only a `let mut` binding is assignable; assigning a plain (immutable)
        // `let` is `RN-E0245 mut.assign-immutable`.
        let err = infer_src("let x = 1 in (x := 2)")
            .expect_err("a plain `let` binding is not assignable");
        assert_eq!(err.code(), "RN-E0245");
        assert_eq!(err.slug(), "mut.assign-immutable");
        assert!(matches!(err, TypeErr::MutAssignImmutable { name } if name == "x"));
        // An unbound name is the same diagnostic (not in scope as a mutable local).
        let err = infer_src("y := 2").expect_err("assigning an unbound name is rejected");
        assert_eq!(err.code(), "RN-E0245");
    }

    #[test]
    fn the_mut_diagnostics_carry_codes_and_hints() {
        // The mutability family (mutability.md §8 canonical numbering): RN-E0241
        // `mut.escapes` (the cell-escapes code), and the v1-specific RN-E0244
        // `mut.non-scalar` / RN-E0245 `mut.assign-immutable` from the reserved
        // RN-E0244–0249 range. Each carries its slug + an actionable hint.
        let non_scalar = TypeErr::MutNonScalar {
            ty: Type::Array(Box::new(Type::Int)),
        };
        assert_eq!(non_scalar.code(), "RN-E0244");
        assert_eq!(non_scalar.slug(), "mut.non-scalar");
        assert!(non_scalar.hint().is_some_and(|h| h.contains("scalar")));

        let escapes = TypeErr::MutEscapes { ty: Type::Int };
        assert_eq!(escapes.code(), "RN-E0241");
        assert_eq!(escapes.slug(), "mut.escapes");
        assert!(escapes.hint().is_some_and(|h| h.contains("escape")));

        let immut = TypeErr::MutAssignImmutable { name: "x".into() };
        assert_eq!(immut.code(), "RN-E0245");
        assert_eq!(immut.slug(), "mut.assign-immutable");
        assert!(immut.hint().is_some_and(|h| h.contains("let mut")));
    }

    #[test]
    fn a_closure_capturing_a_mutable_local_is_rejected() {
        // mutability v1 (`docs/mutability.md` §8): a `let mut` cell lowers to a
        // stack slot, so a closure that captures it could carry it out of scope and
        // read a dangling slot. The escaping closure's type (`Int -> Int`) names no
        // cell, so the *type-based* escape check can't see it — the structural
        // `captured_in_closure` gate rejects it with `RN-E0241 mut.escapes`.
        let err = infer_src("let mut x = 0 in fn y: Int => x")
            .expect_err("a closure capturing the mutable local escapes");
        assert_eq!(err.code(), "RN-E0241");
        assert_eq!(err.slug(), "mut.escapes");
        assert!(matches!(err, TypeErr::MutEscapes { .. }));

        // Nested: capture by an inner lambda inside an outer lambda also escapes.
        let err = infer_src("let mut x = 0 in fn a: Int => (fn b: Int => x)")
            .expect_err("capture by a nested closure escapes");
        assert_eq!(err.code(), "RN-E0241");
        assert!(matches!(err, TypeErr::MutEscapes { .. }));
    }

    #[test]
    fn straight_line_and_non_capturing_mutation_still_type_checks() {
        // The conservative gate must not regress the supported v1 patterns.
        // Straight-line imperative use: read-modify-write then read the scalar.
        let (ty, row) = infer_src("let mut x = 1 in let _ = x := x + 1 in x")
            .expect("straight-line mutation type-checks");
        assert_eq!(ty, Type::Int);
        assert!(row.is_pure());

        // A lambda is present but does NOT reference the mutable cell — no capture,
        // so no escape. `f`'s body uses its own parameter `y`, not `x`.
        let (ty, _row) = infer_src("let mut x = 1 in let f = (fn y: Int => y + 1) in x")
            .expect("a non-capturing lambda does not make the cell escape");
        assert_eq!(ty, Type::Int);
    }

    #[test]
    fn a_lambda_parameter_shadowing_the_mutable_is_accepted() {
        // Shadowing sanity: the lambda's own parameter `x` shadows the outer
        // mutable cell, so `x` in its body is the *parameter*, not the cell — no
        // capture, so this is accepted. The `let mut` body is the closure itself
        // (`Int -> Int`), and the cell never leaves as a scalar value.
        let (ty, _row) = infer_src("let mut x = 1 in (fn x: Int => x)")
            .expect("a lambda parameter shadowing the mutable is not a capture");
        assert_eq!(
            ty,
            Type::Fun(Box::new(Type::Int), Box::new(Type::Int), Row::pure())
        );
    }

    #[test]
    fn constructing_a_list_of_float_is_a_kind_error() {
        // `Cons(3.0, Nil)`: the first field of `Cons` is the type-param `a`, so a
        // `Float` would be stored in a traced `Var` word cell — rejected. (Without
        // D5 this would silently lay a raw 64-bit float in a tagged cell.)
        let err = infer_src("type List[a] = Nil | Cons(a, List[a]) in Cons(3.0, Nil)")
            .expect_err("Cons(3.0, Nil) is a kind error");
        assert_eq!(err, TypeErr::WideTypeVariable { ty: Type::Float });
    }

    #[test]
    fn a_shared_variable_pinned_to_float_through_a_value_let_is_rejected() {
        // `let f = fn x => x in f 3.0`: this is the same current ABI limitation
        // as `id 3.0`; D3 wants stack-only generic float uses allowed once the
        // call representation is no longer a tagged word.
        let err = infer_src("let f = fn x => x in f 3.0")
            .expect_err("current uniform call ABI rejects f 3.0");
        assert_eq!(err, TypeErr::WideTypeVariable { ty: Type::Float });
    }

    #[test]
    fn constructing_a_list_of_int_still_typechecks() {
        // The kind rule excludes only `Wide`; `Int` is tag-room `Uniform`, so a
        // generic `List[Int]` is fine (the motivating case the rule preserves).
        let (ty, _) = infer_src("type List[a] = Nil | Cons(a, List[a]) in Cons(1, Nil)")
            .expect("Cons(1, Nil) type-checks");
        assert_eq!(ty, Type::Named("List".into(), vec![Type::Int]));
    }

    #[test]
    fn a_concrete_float_local_is_unaffected_by_the_kind_rule() {
        // The kind guard does not touch concrete stack/register float math:
        // `3.0 + 4.0 : Float`.
        let (ty, _) = infer_src("3.0 + 4.0").expect("concrete float arithmetic type-checks");
        assert_eq!(ty, Type::Float);
    }

    // ── Sealing — `seal L { e }` / `nogc { e }` (sealing-solution.md §4–§5) ──

    #[test]
    fn nogc_removes_gc_when_only_a_scalar_escapes() {
        // The region allocates an array internally but returns a scalar, so no
        // gc-managed value escapes: `gc` is sealed out of the outward row.
        let (ty, row) = infer_src("nogc { let a = [1] in len a }")
            .expect("a scalar-returning nogc region type-checks");
        assert_eq!(ty, Type::Int);
        assert_eq!(row, Row::pure(), "`nogc` must seal `gc` out of the row");
    }

    #[test]
    fn seal_gc_is_exactly_nogc() {
        // `nogc { e }` ≝ `seal gc { e }` — same judgment.
        let a = infer_src("nogc { let a = [1] in len a }").unwrap();
        let b = infer_src("seal gc { let a = [1] in len a }").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn nogc_rejects_a_gc_managed_datum_escaping() {
        // Returning a freshly-allocated array from a `nogc` region lets a
        // gc-managed value escape — RN-E0403, even though no row names `gc`.
        let err = infer_src("nogc { [1] }").expect_err("a gc datum may not escape a nogc region");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::SealLeak {
                label: Label::Gc,
                ..
            }
        ));
    }

    #[test]
    fn seal_rejects_a_closure_that_still_performs_the_label() {
        // The body is a *value* (a lambda), so the seal's row guard passes — but
        // the returned closure's latent row still performs `ask`, so the label
        // escapes through the result type. The deep no-escape check (post-zonk)
        // catches it.
        let err = infer_src("effect ask : Int -> Int in seal ask { fn x: Int => perform ask x }")
            .expect_err("a closure carrying the sealed effect may not escape");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::SealLeak {
                label: Label::User(ref n),
                ..
            } if n == "ask"
        ));
    }

    #[test]
    fn cannot_seal_an_unhandled_user_effect() {
        // `ask` is a user effect with no handler and no runtime default, so it
        // escapes to the caller — sealing it would hide a runtime fault (D-S3).
        let err = infer_src("effect ask : Int -> Int in seal ask { perform ask 3 }")
            .expect_err("an unhandled user effect cannot be sealed");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::SealUnhandled { label: Label::User(ref n) } if n == "ask"
        ));
    }

    #[test]
    fn sealing_a_handled_user_effect_is_a_sound_noop() {
        // The user effect is discharged by an inner `handle`, so it is already
        // gone from the body's row; the seal removes nothing and is allowed
        // (the design's "local user-effect scoping" use).
        let (ty, row) = infer_src(
            "effect ask : Int -> Int in \
             seal ask { handle perform ask 3 with { ask(x) => resume (x + 1) ; return(y) => y } }",
        )
        .expect("sealing an already-handled effect is sound");
        assert_eq!(ty, Type::Int);
        assert_eq!(row, Row::pure());
    }

    #[test]
    fn seal_of_a_native_world_power_discharges_at_the_boundary() {
        // `mem` is a native power (it bottoms out in a runtime memory op), so a
        // region seal removes it from the outward row — the boundary discharge.
        let (ty, row) = infer_src("seal mem { let buf = 1024 in poke8 buf 65 }")
            .expect("sealing a native power is sound");
        assert_eq!(ty, Type::Unit);
        assert_eq!(row, Row::pure());
    }

    // ── type-declaration well-formedness (P2, review-findings 2026-06-01) ──

    #[test]
    fn a_duplicate_type_parameter_is_rejected() {
        let err = infer_src("type T[a, a] = Mk(a) in Mk(true)")
            .expect_err("a duplicate type parameter is malformed");
        assert_eq!(err.code(), "RN-E0229");
        assert!(matches!(
            err,
            TypeErr::DuplicateTypeParam { ref ty, ref param } if ty == "T" && param == "a"
        ));
    }

    #[test]
    fn a_duplicate_constructor_is_rejected() {
        let err =
            infer_src("type T = A | A in A").expect_err("a duplicate constructor is malformed");
        assert_eq!(err.code(), "RN-E0229");
        assert!(matches!(
            err,
            TypeErr::DuplicateConstructor { ref ty, ref ctor } if ty == "T" && ctor == "A"
        ));
    }

    #[test]
    fn a_bad_recursive_arity_in_a_field_is_rejected() {
        // `List[a, Int]` inside `type List[a] = …` uses the one-parameter `List`
        // with two arguments — caught at the declaration, not deferred to use.
        let err = infer_src("type List[a] = Nil | Cons(a, List[a, Int]) in Nil")
            .expect_err("a wrong-arity self-reference is malformed");
        assert_eq!(err.code(), "RN-E0225");
        assert!(matches!(
            err,
            TypeErr::ArityMismatch { ref name, expected: 1, found: 2 } if name == "List"
        ));
    }

    #[test]
    fn a_well_formed_recursive_parametric_type_still_checks() {
        // The motivating case the checks must not regress.
        let (ty, _) = infer_src("type List[a] = Nil | Cons(a, List[a]) in Cons(1, Nil)")
            .expect("a well-formed recursive type checks");
        assert_eq!(ty, Type::Named("List".into(), vec![Type::Int]));
    }
}
