# `formal/` — the Locus core calculus in Lean 4

A Lean **v4.28.0** formalization of [the Locus calculus](../README.md#locus-language--calculus-mechanization-status--theory),
the graded modal calculus at the heart of Locus: a **monadic effect grade**
joined to a **comonadic stage grade** by a **distributive law `δ`**.

- Toolchain: `leanprover/lean4:v4.28.0` (pinned in [`lean-toolchain`](lean-toolchain)).
- Dependencies: **Lean core only** — no Mathlib, no Batteries.
- Entry point: [`LocusCalculus.lean`](LocusCalculus.lean).

## Build

```sh
cd formal
lake build        # fetches the pinned v4.28.0 toolchain via elan, then elaborates
```

> **Build status: verified with Lean v4.28.0** (`lake build`, exit 0). The file
> elaborates clean; the only warnings are the two intended `sorry` obligations
> (`preservation`, `progress`). The toolchain lives on `F:\elan`
> (`ELAN_HOME=F:\elan`, persisted at user scope), so `lake build` works from a
> fresh shell.

## What is proved vs. left as an obligation

The split mirrors `calculus.md` itself, which is "a scaffold of obligations."

**Proved outright — the grade algebra and the four δ-coherence squares (§3.6):**

| Lean fact | Calculus content |
|---|---|
| `Row.append_assoc`, `Row.empty_append`, `Row.append_empty` | §1.1 rows form a monoid |
| `split_hom` | **§3.6 μ-square** — *split is a monoid homomorphism* |
| `Stage.box_idem` | **§3.6 δ_□-square** — `□□ ≅ □`, trivial under idempotence |
| `split_empty` | §3.6 η/ε bookkeeping |
| `genPart_allG`, `objPart_allO` | §3.5 the O/G partition is respected |
| `quote_residual_generative` | §3.5 a `quote`'s residue is generative-only |
| `Mult.join_comm/assoc/idem`, `Mult.le_*` | §1.3 the 3-point multiplicity lattice `0 ≤ 1 ≤ ω` |
| `var_pure`, `var_stage` | §7 value purity / §9 SO-1, variable case |

These are the genuinely *mathematical* core: the law that makes `δ` lawful
(`split` is a monoid hom), the idempotence that makes the comultiplication
square collapse, and the grade structures.

**Also proved — the sealing and representation-kind extensions** (the §13 / §11
content, now summarized in [the calculus status](../README.md#locus-language--calculus-mechanization-status--theory)):

| Lean fact | Calculus content |
|---|---|
| `Row.sealOut_append`, `Row.sealOut_empty`, `Row.sealOut_removes` | **§13 sealing** — the seal's row-algebra; `sealOut_removes` (`L ∉ sealOut L E`) is the no-escape, the `runST`/`∀s` condition at the row level |
| `Ty.rkind`, `Ty.tracedStorable`, `Ty.wide_not_tracedStorable`, `RKind.uniform_ne_wide` | **§11 representation kinds (D3)** — a `Wide` type is *never* traced-storable: the type-level core of the GC's traced-cell invariant (`classify` only ever sees a `uniform` word — the §11 payoff / T0) |

These ride the same techniques as the core algebra (`filter` lemmas, `decide`); the
typing-relation extensions (the `seal`/`Ref`/`par` rules and §11's `t-store` premise)
and their preservation are the next step (companion §16).

The unproved part splits into **two tiers, and they are not equally complete** —
a distinction worth keeping sharp.

**Tier 1 — stated precisely, proof owed (`sorry`):**

| Lean theorem | The proposition that is pinned | Calculus § | Why the proof is owed |
|---|---|---|---|
| `preservation` | `∃ E', E' ⊆ E ∧ Typed Γ s e' A E'` — types kept, effects only *shrink* | §7 | needs the substitution lemma → the §4.1/§6.6 hygiene development (the encoded `subst` is capture-unsafe by design) |
| `progress` | value, or steps, or stuck at an op *in its row* (`op ∈ E`) | §8 | needs the canonical-forms lemma over the full syntax |

Both carry their **real statements** — the propositions elaborate and type-check
as well-formed; only the proof body is `sorry`, exactly the cases `calculus.md`
marks "modulo routine typed-handler bookkeeping." `sorry` here is the honest
Lean reflection of that status, not a gap introduced by the encoding.

**Tier 2 — named, proposition not yet formalized (`True := trivial`):**

| Lean stub | Calculus § | What it still needs before it can even be *stated* |
|---|---|---|
| `stage_ordering_SO1` | §9 | the real ∀-over-free-occurrences statement — the *variable case* is already proved, as `var_stage` |
| `stage_ordering_SO2` | §9 | a statement about generation evaluation contexts not descending under `quote` |
| `zero_cost` | §5.2 | the evidence-passing translation `⟦·⟧` (§5.1, a Phase-3 deliverable) |

These three are `True` today, **not** `sorry`: their honest status is *named and
deferred*, one notch weaker than tier 1's *stated and owed*. `True := trivial`
is a placeholder for a proposition; it is not yet a proof obligation against a
real one. Keeping the two tiers distinct is the point.

## `Substitution.lean` — type safety (preservation + progress), proved `sorry`-free (the keystone)

`Substitution.lean` (a second `lean_lib`, built by the same `lake build`)
re-develops the **effectful λ-core** (var / lam / app / let / perform) in
**de Bruijn** form — function contexts (`Nat → Ty`) and parallel substitution —
and proves, **with no `sorry`**, the keystone the main file leaves open:

| Lean theorem | Content |
|---|---|
| `rename_typed` | renaming (weakening) preserves typing |
| `subst_typed` | **the substitution lemma** — a context-respecting parallel substitution preserves typing |
| `subst0_typed` | substituting a typed term for de Bruijn index 0 preserves typing |
| `value_pure` | values (`lam`) are pure |
| `preservation` | **types preserved, effect rows only shrink** (`E' ⊆ E`) — over β, `let`-value, and the four congruence rules |
| `progress_pure` | **pure progress** — a *closed*, pure term is a value or steps (no stuck states) |
| `Stuck` + `progress` | **effectful progress** — a closed term is a value, steps, or is `Stuck` (blocked on an unhandled `perform`, via the `K[perform op w]` evaluation-context decomposition) |
| `stuck_op_in_row` | a `Stuck` term's blocking op **is in its row** (`op ∈ E`) — stuck only at a declared effect |

It is capture-safe **by construction** (de Bruijn), so the substitution lemma —
*false* for the main file's deliberately naive string-name `subst` — holds and is
machine-checked. `lake build` is exit 0 with **no `sorry` in this module**.

**Type safety for the effectful λ-core, machine-checked.** `preservation` +
`progress` + `stuck_op_in_row` together say: a closed well-typed term is a value,
takes a step, or is blocked on an unhandled `perform` whose operation its row
declares — a well-typed program never "goes wrong." Effectful progress uses a
`Stuck` predicate that *is* the `K[perform op w]` evaluation-context decomposition,
recorded inductively over evaluation positions.

**Remaining:** migrate `LocusCalculus.lean`'s term layer to this representation
(to discharge *its* `preservation` / `progress` `sorry`s), and extend the
development to the staging constructs (quote / splice / genlet), whose
value-purity-of-`quote` subtlety the source doc itself defers (§7).

## `Handlers.lean` — deep effect handlers, preservation under D4 set-discharge (`sorry`-free)

`Handlers.lean` (a third `lean_lib`) reuses the de Bruijn technique to mechanize
the part the source calls "the difference between Locus and everything else": the
**effect handler**, and the soundness of **set-rows (D4)**. It adds `handle` to
the term language (var / lam / app / perform / handle) with an operation
signature `S`, and proves — **no `sorry`** — that the handler reductions preserve
typing.

The handler is **deep** and discharge is **set-remove**, both verified against
source *before* proving: the resumption re-installs the handler
(`docs/calculus.md:455` — `resume ↦ λz. handle K[z] with H`; the
`locus-llvm/src/lower.rs` CPS transform re-runs the continuation under the same
handler), and rows are sets (`docs/calculus.md:91,201` — `E ∪ {ε}`). Discharge is
therefore the side-condition `Eb ⊆ op :: Eo` — the set-remove of `op` from the
body's row.

| Lean theorem | Content |
|---|---|
| `Ectx`, `plug`, `rename_plug` | handler-free (binder-free) evaluation contexts `K`, plugging, and renaming-commutes-with-plug |
| `plug_retype` | **monotone context typing** — refilling a hole at a smaller row keeps `plug K e` typed at a row `⊆`; the lemma that makes the deep resumption typecheck |
| `preservation_handleOp` | **the `(op)`-case** — a handled deep operation preserves typing: `λz. handle K[z] H` typechecks via `plug_retype`, then the set-discharge side-condition feeds the inner `handle` |
| `preservation` | full preservation over β, the congruence rules, **and** the three handler rules (`handleRet` / `handleCong` / `handleOp`) |

**This is the D4 reconciliation that mattered.** Having confirmed that deep
handlers + set-rows is *the* known-sound combination, we proved it here rather
than asserting it: set-discharge (removing `op` from the row entirely) is
type-safe precisely because the deep resumption re-installs the handler, so the
inner `handle K[z]` re-discharges any further `op`. No soundness bug surfaced —
the proof *confirms* the implementation, mechanically.

Scope (honest): one operation per handler; identity return clause
(`handle v ↦ v`); **preservation** (not progress) for the handler rules. The
development reuses `Substitution.lean`'s technique in a separate module, leaving
the keystone file untouched.

## Scope of the guarantee — proof vs. implementation

The theorems are about the **calculus**, not a running implementation. They say
the *language design* has no soundness bug; they do **not** say a compiled Locus
program is safe. Those are different claims, and the gap between them is a
**trusted base** the proof deliberately does not reach:

- **The compiler is unverified.** AST→DFM lowering, LLVM codegen, the JIT — a bug
  in any of them can violate a source-level guarantee the calculus proves. The
  theorem covers the source language, not its translation.
- **The GC is assumed correct.** Type safety assumes values stay well-formed at
  runtime — which is exactly what the moving handle-based collector must deliver:
  `classify` reading every word's 2-bit tag correctly, the handle stack rooting
  everything live, evacuation rewriting every pointer. `T0` (tag-completeness)
  checks this by *validation*, not proof. It is the load-bearing runtime
  assumption under the whole story.
- **JASM / sealed `asm` (D5) and FFI sit outside the calculus by design** — the
  trusted escape hatches real systems need. The design does not claim they are
  safe; it *seals* them (a named `asm` capability; FFI checked only at the
  boundary) so each hole is auditable rather than hidden. That is the mature
  posture, not a gap in the proof.
- **D1 encodes a "real life intrudes" ruling in the canon:** an `i62` bit pattern
  that is representable but unsafe in a traced cell gets a *loud panic*, not a
  proof.
- **Type safety never promised liveness or resources.** "Never goes wrong" is the
  narrow, precise statement proved above — it says nothing about OOM,
  nontermination, deadlock, or I/O that fails. No type system delivers those.

The honest summary: **the design is proven sound; an implementation is sound only
insofar as it faithfully realizes the design and its trusted base holds.**
Conflating the two is how "verified" languages get oversold — keeping them
distinct is the transparency the effect system exists to provide.

## Encoding choices

- **Rows** = `List Label` (the *scoped* rows of §1.1: order and duplicates
  significant; `++` is `∪`, `[]` is `∅`). `handle` discharges the **nearest**
  matching label via `List.erase` — scoped-row semantics, faithfully.
- **`split`** = a pair of complementary `List.filter`s, so `δ`'s totality is
  immediate and the μ-square is `List.filter_append` applied twice.
- **Stages** are two-valued (`obj`/`gen`) per the §3.0 idempotence commitment;
  `quote` raises to `gen`, which absorbs.
- **Binders** use string names; capture-avoidance is *abstracted* (the `subst`
  is naive), which is exactly why preservation is an obligation rather than a
  proof here — matching the document's separation of the distributive-law story
  (done) from the hygiene story (§4.1, a separate lemma).

The `quote` typing rule literally reads `δ`: from `e : A ! E` at stage `s` it
produces `□(A ! objPart E)` with residual row `genPart E` at `s.box` — so the
proved `split`/`objPart`/`genPart` lemmas are the same functions the type system
runs.
