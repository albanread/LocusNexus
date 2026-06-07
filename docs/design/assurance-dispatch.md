# Assurance spike — does type-directed dispatch's effect-polymorphism elaborate?

*2026-06-07. Pre-implementation spike for [`database-access.md`](database-access.md)
review-fix **B** (type-directed dispatch: `db_exec : Conn[b] -> Sql -> Rows ! eff(b)`).
Principle: **write small assurance tests before any never-tested code.** Docs
describe; the checker decides. Run each probe with `locusc effects FILE` (type-level)
— it infers + prints the row without needing a runtime handler.*

## Second finding (2026-06-07) — a T0 coercion gap blocks the scope combinators

Building the `with_memory`/`with_file`/`with_query`/`with_transaction` combinators
(design Q3) surfaced a **second compiler bug**, distinct from the trait one below.
Passing a phantom-typed handle `Conn[SqliteMem]` (a single-field constructor,
scalar-repr'd) into a polymorphic combinator body trips tag-completeness (T0):

```
locusc: tag-completeness (compiler bug): app arg -> parameter — a `ToPtr` coercion
is required to move a `Conn[SqliteMem]` across a `?N` (Var) cell boundary, but none
is present
```

T0 (the memory-safety gate) demands a `ToPtr` (box the scalar into the uniform
cell) at the body-application boundary, but sema's coercion-insertion did not place
one — a genuine gap, classified by T0 itself as a compiler bug. Notes:
- The analogous combinator over **`Int`** bodies works (the D-series probes), so the
  trigger is specifically a **constructor-typed argument** crossing the body's
  polymorphic slot.
- It fires only when a combinator is **called**; merely defining them in `Database`
  does not break other users (verified — all committed demos still pass).
- Minimal repro: [`assurance-scope-combinators.locus`](assurance-scope-combinators.locus).

**Root cause (pinned 2026-06-07 via `LOCUS_T0_DEBUG` instrumentation).** The
failing application is a **beta-redex synthesized by `wrap_callback`** (sema.rs —
the lowering that adapts a *concrete* callback to a *uniform* callee ABI when a
constructor-typed value flows through a polymorphic body param). The debug print:

```
T0 app: fun.node=Lam  fun.ty = ?6 -> Int ! {gc}   dom(raw)=?6   dom(resolved)=Box[?7]
        arg.node=Var($cbw0)   arg.ty=Box[Tag]
```

So `f`'s domain `?6` is a **store-bound** unification var that *resolves to a
concrete `Box`*, but T0 (running pre-zonk) reads the **literal** `?6` as a uniform
cell and demands a `ToPtr`. `wrap_callback` decided no coercion (it effectively
sees the resolved `Box`); T0 checks the literal `Var`. Insertion and checking
**diverge** — the very thing the module claims they never do.

**Why T0 can't simply resolve the slot (the trap).** The `id 5` case has the same
shape — `id`'s param var is also store-bound to the scalar `Int` arg — but there a
`ToPtr` genuinely *is* needed (a generic's param cell is uniform) and *is* inserted.
If T0 resolved the App-param slot, it would compute `coercion(Int, Int) = None` and
**silently miss a missing box** at exactly the scalar→uniform boundary that is the
heap-corruption hazard T0 exists to catch. Boundness doesn't distinguish the two
cases; only "is this function a generic with a uniform param" does, which isn't on
the slot var. **So the fix belongs in `wrap_callback`** (emit a T0-consistent
tree — record the resolved param type, or insert exactly the coercion T0 derives),
not in T0.

**Decision:** combinators **deferred** (removed from `Database`; bare open/close
remain) until `wrap_callback` is made T0-consistent. T0 is the sole memory-safety
mechanism, so this is a careful, dedicated change — not rushed alongside feature
work. The diagnosis above is the starting point.

---

## The question

Fix B needs an operation whose **effect row is determined by a type parameter** (the
backend): `SqliteMem` ops carry `{sqlite_access}`, `SqliteFs` ops carry
`{sqlite_access, sqlite_fs}`, and a program incurs *exactly* the backends it opened.
`trait-resolution.md` §7.3 says this is supported via a trait method whose row is a
**variable `ρ`** "so instances may differ", with the resolved instance's row surfacing
in the caller. Does it actually?

## Probes and results

| # | probe | inferred row | verdict |
|---|-------|-------------|---------|
| D1 | plain fn performs `mem_access` | `{ mem_access }` | ✓ direct effects work |
| D4 | ordinary **row-variable** fn `(Int -> Int ! {\|r})` threading an effectful arg | `{ mem_access }` | ✓ row-var polymorphism works for ordinary fns |
| D2 | trait method with **fixed** row `! {mem_access}`, used at a concrete instance | `{ gc, mem_access }` | ✓ instance effect surfaces |
| **D3** | trait method with **variable** row `! {\|r}`, used at a concrete instance | `{ gc }` | ✗ **instance effect DROPPED** |

D3 is the headline. Program:

```locus
effect mem_access : Int -> Int in
type MemC = MemC(Int) in
trait Backend b { exec : b -> Int ! {|r} } in
instance Backend MemC { exec = fn c: MemC => mem_access 1 } in
let go = fn u: Unit => let m = MemC(0) in exec m in
go ()
-- expected { gc, mem_access } ;  actual { gc }
```

## Conclusion (original finding)

**The variable-row trait path — exactly what fix B as written relies on — did not
propagate the resolved instance's effect.** A *trait-specific* gap: D4 proves ordinary
row-variable functions propagate fine, D2 proves *fixed*-row trait methods propagate
fine. The defect was the interaction of **trait resolution + a variable method row**:
the resolved instance's latent row was not unioned into the caller (contra
`trait-resolution.md` §7.3). Silent **under-reporting** — a program touching
`sqlite_fs` would not say so.

## ✅ RESOLUTION — fixed in the compiler (2026-06-07)

We treated this as a soundness defect (it negates "every effect is in the type") and
fixed it before any database work, per *build on solid ground*. The fix
(`locus/src/sema.rs`):

- **`bind_method_use_rows()`** — pre-zonk, store live: for every method use whose
  obligation resolves to a **concrete head**, unify the use-site method type against
  the resolved instance's actual method type, binding the use's free latent-row
  variable to the instance's real row.
- **`generalize_resolved()`** — runs that binding *before* each `let` generalizes, so a
  wrapping helper (`let go = fn u => tick (MemC 0)`) can't quantify the still-free row
  var into its scheme and lose the effect.

Both are **zero-cost for programs with no trait-method uses** (the pass loops over an
empty `METHOD_USES`). Three regression tests added (`trait_method_with_*`,
`..._survives_let_generalization`); full suite green (389 + 174).

Updated probe results:

| probe | before | after fix |
|-------|--------|-----------|
| D3 (variable-row, concrete use) | `{ gc }` | **`{ gc, mem_access }`** ✓ |
| wrapped in a generalized `let go` | `{ gc }` | **`{ gc, mem_access }`** ✓ |
| two backends, direct use — mem | — | **`{ gc, sqlite_access }`** ✓ |
| two backends, direct use — fs | — | **`{ gc, sqlite_access, sqlite_fs }`** ✓ |

So **fix B's goal is achieved for the case the design actually uses**: open a *concrete*
backend, call `db_exec` on it, and the row carries exactly that backend's effects —
`sqlite_fs` distinguishes disk from memory through generic dispatch.

### Remaining boundary (a genuine v2 feature, not a regression)

A **backend-*generic*** helper — `let run = fn c => db_exec c in run (MemC 0)` — still
drops the effect (`{ gc }`). Here `run`'s scheme quantifies the method's row var
*decoupled from the type parameter* `c` (`∀c r. Backend c => c -> Int ! {gc | r}`);
standard HM cannot express "`r` *is* the `Backend`-method row of `c`". Pinning it needs
**associated effects** (effect-as-a-function-of-the-instance) — the same v2 territory as
associated types (D6). App code that names a concrete backend never hits this; only
code abstract *over* the backend does. Documented, not relied upon.

## What this means for the design

Two realizations work **today**, one needs compiler work:

1. **(Works now) Put the distinguishing effect on the *open*, keep ops fixed-row.**
   `mem_open : … -> Conn[SqliteMem] ! {sqlite_access}` and
   `file_open : … -> Conn[SqliteFs] ! {sqlite_access, sqlite_fs}` are ordinary
   fixed-row functions (propagate per D1). `db_exec`/`db_query` carry a fixed
   `{sqlite_access}`. A program's row is the union of what it called, so an fs program
   surfaces `{sqlite_access, sqlite_fs}` and a mem program `{sqlite_access}` — **the
   mem/fs transparency claim (Q2) holds without the broken path**, because the
   *open* carries the distinction, not a varying-effect generic op. Limitation:
   `db_exec` is fixed to the SQLite family's effect, so a *single* generic op across
   *different DBMS families* (sqlite vs mysql) is not expressible this way.

2. **(Needs the bug fixed) Variable-row trait methods.** Fixing the §7.3 propagation
   (union the resolved instance's latent row even when the declared row is a variable)
   unlocks the fully-generic cross-DBMS `db_exec : Conn[b] -> Sql -> Rows ! eff(b)`.
   This is the principled fix and aligns the implementation with its own spec; it is
   compiler work in the trait-resolution row-flow.

3. **(Probed) Staged / generation-time dispatch.** Staged effects genuinely *do*
   ride in the code (`distributive.locus`): `quote(mem_access 0)` has type
   **`Code[Int ! {mem_access}]`** — the object effect is carried by the `Code` and
   fires when spliced. So *if the backend is known at generation time*, you can emit
   that backend's code and the concrete effect comes with it — no row variable, no
   trait. **But two constraints surfaced (probes S1/S2):**

   - A generation-time `if` requires **both arms to unify**, including their effects.
     `if … then quote(mem_access 0) else quote(fs_access 0)` is **rejected** —
     `expected Code[Int ! {mem_access}], found Code[Int ! {fs_access}]`.
     `distributive.locus` only compiles because both its arms share `{winapi}`. So a
     single staged `if` cannot fan out to *different-effect* backends; per-backend
     code must be separate staged paths, each independently typed.
   - Staging resolves at **generation time**, so it only helps when the backend is a
     compile-time constant. The credential-by-name model (§4) chooses the backend
     from *runtime* vault data, which staging cannot see. Staging is the right tool
     for "this app talks to SQLite-mem" (known at build), not for "connect to
     whatever `prod.analytics` resolves to" (known at run).

   Net: staging is powerful and the effect-carrying is real, but it is **not** a
   general substitute for runtime cross-DBMS dispatch with honest effects. It shines
   when the backend is statically known.

## Recommendation (updated post-fix)

Realization **(2)** — the §7.3 propagation fix — is **done**, so a variable-row
`db_exec : Conn[b] -> Sql -> Rows ! {|r}` now carries the right per-backend effect when
called on a **concrete** connection. Build the SQLite slice on type-directed dispatch as
designed; the `sqlite_fs` distinction (Q2) is real through generic dispatch. Keep using
fixed-row opens for the open functions either way (they're simplest). The only construct
to still avoid is a helper *generic over the backend* (it silently drops the effect
pending associated effects) — and the regression tests will flag if that ever changes.

## These probes are guardrails

The four programs live as regression probes; D3 flipping to `{ gc, mem_access }` is the
signal that realization (2) became available. (TODO: promote to `locus` Rust tests that
assert the inferred row, so CI catches the transition either way.)
