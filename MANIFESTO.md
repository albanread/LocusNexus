# LocusNexus — Manifesto and Declaration of Intent

*Drafted 2026-05-30.*

 

> **Why Locus exists — the idea the rest serves.** Locus's effect system is *calculus-driven* ([`calculus.md`](docs/calculus.md)): effects are a graded monad, so **every effect a computation can have is written in its type** — nothing hidden, ambient, or implicit, down to `{gc}` itself. That one fact is what sets Locus apart from everything else: it buys **transparency** (you read what code *does* straight from its signature) and **safety** (the effect row is a checked upper bound and capabilities seal every raw power, so the dangerous thing cannot happen silently or nonlocally). Every other feature exists to keep that guarantee total.

## What this is

Locus is a small language for **people and their AI colleagues** — built so
that what a program *does* is legible in what it *says*. It rests on a single
graded-modal calculus, taken all the way down: the things mainstream languages
accrete as separate, magical features — effect/exception tracking, macros and
compile-time evaluation, code generation — are not many features but **facets
of one mechanism**: two dual graded modalities (a monadic **effect** grade and a
comonadic **stage** grade) and the **distributive law** between them.

The discipline is that every construct earns its place by a **rational, logical
design choice backed by a written specification** — not by accretion. The same
demand the core makes of the calculus, the surface makes of its grammar: no
spec-by-implementation, at either layer. And the same demand reaches the
*runtime*: there is no privileged, hidden machinery — the runtime is ordinary
Locus over a thin, *declared* boundary to the OS, so a reader (human or AI) can
follow a program top to bottom and find nothing that escaped the rules.

The surface is **ML-flavored and explicit** (`let … in`, `fn x: T => e`,
`if/then/else`, `let rec`), chosen for clarity and ease of reasoning rather than
a particular look. Effects and staging are first-class, not afterthoughts.

> The effect system was first **inspired by Nim** — its `raises`/`tags`/`gcsafe`
> tracking showed that effects belong in the type. Locus takes that seed and
> makes it the trunk: a single *real* effect row, not three accreted pragmas.

 

## For teams — with Claude as a colleague

Locus is built for teams whose colleagues include AI agents, and it says so
upfront, because that audience changes design decisions rather than
decorating them. Claude is a first-class user — it will read, write, edit,
test, and review Locus beside humans — so **"good for Claude to use" is a
standing design goal, not an afterthought.**

Two facts about an AI colleague set the constraints — and they are good for
human colleagues too:

- **It has almost no prior exposure to a brand-new language.** It cannot
  lean on a decade of accumulated lore; it leans on the *spec*, on *source
  it can read*, and on *the compiler's feedback*. Novelty must be paid down
  by design, not assumed away.
- **Its worst mistakes are nonlocal and silent** — the ones that compile
  and then misbehave at a distance. So the language's job is to keep
  consequences *local and visible*, and to turn distant footguns into near
  typechecker messages. The mechanized answer to exactly that enemy is the
  calculus-driven effect row: because every effect a computation can have is
  written in its type, the silent-and-distant mistake has nowhere to hide — it
  is a checked row violation at the call, not a surprise three modules away.

The response is the same one that makes the language humane:

1. **A very small kernel.** The language proper is a tiny core — the
   two-grade calculus. What a reader must hold fits in the head.
2. **Everything else is discoverable text.** The surface
   (`unless`/`for`/`with-*`, the prelude, the libraries) is defined *in
   Locus*, as readable code — not as compiler magic. To learn what a
   construct means you *read it*; you do not reverse-engineer a compiler.
   (The portfolio's "define the surface with macros" lesson, made a
   promise.)
3. **Good utilities; feedback as an interface.** The compiler is the
   colleague's main loop, so its output is designed *as data*: structured,
   spec-citing diagnostics with suggested edits; introspection verbs
   (`expand`, `explain`, `eval`); a queryable semantic model; a canonical
   formatter. A new language earns reliability through its tools, not
   through familiarity.
4. **The contract is on the tin.** Effects and stages are *visible in
   signatures* as plain text — `! {fs, exn[IOError]}`, `Code[T]` — so a
   reader knows what a function may do without reading its body. Reading a
   row takes no theory; it is a list of labels.

And the load-bearing division of labor: **the complex algorithms are
implementation concerns, never user concerns.** The graded-modal
metatheory, the distributive law, evidence-passing, continuation
reification — that is machinery, discharged once on paper and in the
compiler so that *no colleague ever has to think about it*. Its whole
purpose is to remove surprises: the sophistication buys safety and silence
at the edges, then gets out of the way. The kernel you write against stays
small and plain; the cleverness lives underneath it, paying for the absence
of footguns — at runtime, and at the keyboard.

## The thesis, stated precisely

Effects and staging are **not** the same mechanism — and the way they
fail to be the same is exactly the design. They are **dual graded
modalities**, and the language we want is the single graded-modal
calculus that hosts both, plus the distributive law between them.

- An effect type `A ! E` is a **graded monad**: the grade `E` — a row, a
  monoid under union — tracks what a computation *does* on the way to
  producing an `A`. (Koka; Katsumata's graded monads.) **Monad = what you
  emit.**
- A staging modality — `□A`, "code-of-`A`" — is a **graded comonad**,
  i.e. a coeffect: it tracks what a computation *requires* from its
  context — which stage it is available at, and which surrounding
  bindings it depends on. (The Davies–Pfenning modal reading of staging,
  `○`/`□` as "next"/"necessity"; Petricek–Orchard–Mycroft coeffects on
  the grading side.) **Comonad = what you demand.**

Staging is fundamentally a *demand on context* — this fragment is not
runnable here, it needs a later stage — so it lands on the comonadic
side. Locus's core is therefore a calculus with **two grades**: a monadic
**effect grade** and a comonadic **stage grade**. The entire substance of
the design lives in the **distributive law** that says how they commute.

The general shape — effects-and-coeffects-via-grading-with-a-distributive-
law — was worked out by Gaboardi, Katsumata et al.; **Granule** is the
closest shipped research vehicle. What nobody has done is integrate it
into a *usable, systems-oriented, ML-flavored language with a real
backend*. **That gap is Locus's actual contribution, and it is a
defensible one:** the pieces exist in the literature; the integration
aimed at a working language does not.

## Why the duality earns its keep

The duality is not category-theory garnish. The concrete payoffs all fall
out of taking it seriously.

1. **Splices are handlers; let-insertion is an effect.** This is the demo
   that proves the unification is not decorative. In MetaOCaml you need a
   bespoke `genlet` because naive bracket/escape cannot hoist a binding to
   a sensible scope — Kiselyov's fix routed let-insertion through
   delimited control, i.e. generation-time is secretly effectful. In Locus
   that is explicit and free: `genlet e` is just `perform Insert(e)`, a
   generation-stage writer/control operation, and a splice `${ … }` is the
   handler that catches `Insert` and emits a `let` at that point. The user
   writes ordinary effect code in the metaprogram; binding placement and
   hygiene fall out of *where the handler sits*. Template Haskell already
   gestures at this — its `Q` is literally a monad, generation is already
   effectful — it just never graded `Q` or connected it to the object
   language's effects. **We finish the thought TH started.**

2. **`static`, `const`, `macro`, and CTFE collapse into one
   staged-evaluation mechanism.** This is the single biggest
   "designed, not evolved" win available. Conventionally these are four accreted
   features. In a system where stages are first-class grades:
   `static` is "this value is demanded at stage 0"; `const` is "force
   to stage 0"; a `macro` is "a stage-(n+1) function producing stage-n
   code"; and CTFE is "run the stage-0 handlers." **Four features become
   one apparatus at different grades.** That consolidation is what makes a
   spec short — the whole point of the project.

3. **Effect annotations stop being a bolted-on half-system** and become the
   *same row system the metalanguage is typed in*. Conventionally, effect
   tracking and the macro layer are two separate, unfinished things — in
   Locus they are facets of one row discipline once unified. The macro that
   generates code is typed in effect rows; the code it generates carries
   object-level effect rows *inside* the stage modality. Same machinery,
   two grades.

4. **Statically-handled effects are zero-cost** — the argument that wins
   over systems programmers. Koka compiles effects via **evidence
   passing** (Xie–Leijen): handlers become dictionaries threaded
   implicitly. If effects compile to evidence and you have staging, then
   any handler known at generation time can be partially evaluated away —
   the evidence is a stage-0 value, so you specialize against it and the
   effect vanishes from the runtime. (Precisely — the calculus grades this
   (§5): staging removes the *dispatch* for any statically-resolved handler;
   the residual is only the continuation its resumption shape demands —
   *nothing* for tail-resumptive handlers, a reified continuation for
   multi-shot ones.) "Effects you resolve statically cost
   nothing" is the rebuttal to the standard objection that effect systems
   are too slow for systems work. **The unification pays rent in cycles,
   not just elegance. Stage the evidence; the abstraction evaporates.**

## The effect system is the centerpiece

Set staging aside for a moment. The effect system *alone* is the feature
that makes Locus worth building rather than "yet another tidier language."
Mainstream effect tracking is half-built — opt-in, applied inconsistently,
full of escape hatches, and not algebraic. Locus makes a *real* one the
centerpiece: a coherent thing those systems gesture at and never deliver.

**Two orthogonal axes — Locus commits to both:**

- **Algebraic effects + handlers** — the *mechanism* (Eff; OCaml 5).
  `perform op` / `handle`, where a handler receives the continuation and
  may resume it. One construct subsumes exceptions, state, async,
  generators, and nondeterminism.
- **Row-polymorphic effect typing** — the *static discipline* (Koka).
  Effects are rows of labels with effect *variables*, so higher-order code
  (`map`, `fold`) stays polymorphic over its argument's effect. It
  compiles via evidence passing (Xie–Leijen) — the same mechanism that
  buys the zero-cost story above.

**No prior art gives both in a systems language.** OCaml 5 ships handlers
with *no* static effect tracking — deliberately, to dodge the inference
cost. Koka has both axes but is not systems-flavored. Locus's slot is
exactly "both axes, in a systems language with a real backend, made the
centerpiece" — the manifesto's defensible-gap claim, restated for effects.

**Three accreted effect pragmas collapse into constraints on one row:**

- exception-listing annotations → an exception label in the row; an
  exception is just *a handler that never resumes*.
- IO purity → the empty row `<>` — and *fine-grained*, so not one
  monolithic `IO` but `console`/`fs`/`net`/… as separate labels.
- GC-safety annotations → a `gc` label; a gc-safe function is one whose row
  *excludes* `gc`, and a `nogc` region is a row constraint. This is the one
  label that touches the inherited GC directly: `gc` means "touches the
  managed heap."

Conventional effect annotations already grope toward user-defined effect
labels — they just never make them algebraic, never make them
row-polymorphic, and leave checking opt-in.
**Locus makes purity the default and effects explicit.**
That inversion — opt *out* of purity rather than opt *in* to checking — is
what turns the discipline from advisory to load-bearing: a `∅`-row proc
*provably* throws nothing, does no IO, and never touches the heap.

And it lands in this portfolio's own backyard: resumable handlers are a
strict generalization of **Common Lisp's condition/restart system** —
handlers that run before the stack unwinds and may resume the signaling
point — already lived experience in NCL/SFCL. "Exceptions that can resume"
is not exotic here; it is that, typed and unified with IO and GC-safety.

The cost, kept honest: this leans hard on Koka-style row inference staying
tractable. The standing hedge is **stages explicit, effects inferred** —
the solver only ever chases the monadic row, never the comonadic stage
grade.

## The honest hard core — research, not grind

These are the places where getting it wrong drops us straight back into
spec-by-implementation, the one outcome this project has sworn off.

- **The distributive law is the entire soundness story, and it must be
  nailed in the formal calculus before a line of compiler is written.**
  The crux is `□(A ! E)` versus `(□A) ! E′`: when you `perform` an effect
  under a quotation, does it happen *now* (at generation) or *later* (when
  the generated code runs)? That is the Template Haskell footgun — "is
  this exception thrown at compile time or run time?" — and in Locus it
  must be a *typed, total, mechanical* decision, not something discovered
  by reading the compiler. The grade is precisely what disambiguates: a
  generation-stage effect lives *outside* the `□`, an object-code effect
  lives *inside* it, and the distributive law says which boundary
  crossings are legal. Specify that judgment crisply and the headline
  footgun becomes a typechecker message. Fudge it and we have rebuilt the
  accretion we set out to escape.

- **Two dimensions of polymorphism threaten inference.** Koka's row
  inference is already delicate; adding a stage dimension plus a
  distributive law risks pushing principal-type inference out of reach.
  The pragmatic, correct compromise is **Zig-flavored: stages explicit,
  effects inferred.** Make stage transitions syntactically visible
  (`quote`/`splice` are never silently inserted) so the comonadic grade is
  mostly *checked* rather than *solved*, and let the monadic effect row be
  inferred the way Koka does. That keeps inference tractable and — bonus —
  keeps metaprogramming legible: the reader sees where stages change,
  itself a designed-ness virtue.

- **Hygiene is enforced by scope-set discipline.** Open-code staging
  (generating code with free variables captured at splice sites — what a
  real macro system wants, versus closed `□`-only code) must rule out both
  capture and *scope extrusion* (a let-inserted binding hoisted past a
  generated binder it needs). In the single-stage calculus (§3.0),
  **scope-set discipline plays the role environment classifiers
  (Taha–Nielsen) play in multi-stage systems**, and let-insertion targets
  are explicit lexical loci — *the `let` lands where the handler sits* — so
  both capture and extrusion are **static errors, not generation-time
  surprises**. This was the genuinely unexplored corner; the single-stage
  commitment is what turned it into a scoping discipline rather than a
  research risk.

- **Codegen for handlers over LLVM is real engineering — and Locus commits
  to full multi-shot.** Multi-shot delimited continuations are what
  nondeterminism, backtracking, and sampling *require*; restricting to
  one-shot (the OCaml 5 line) would amputate exactly the effects the
  centerpiece claims. The cost is tamed by **grading the compilation**, not
  by restricting the language — it is pay-as-you-go:
    - a **stage-resolved** handler (evidence is a stage-0 value) is
      partial-evaluated away — *no continuation captured*, the zero-cost
      case;
    - a **tail-resumptive** runtime handler lowers via evidence passing to
      a plain call — *no reification*;
    - a **general / multi-shot** handler reifies the continuation as a
      *heap object the GC owns*.
  The asymmetry with OCaml is the GC: multi-shot is costly because each
  resume must *copy* the captured continuation (a consumed segment can't be
  reused), which is a memory-management problem a tracing collector solves.
  So continuations are reified as **heap objects, not copied raw stack** —
  favouring **selective CPS** (CPS only code whose row carries an
  `ω`-multiplicity effect; leave `∅`-row and tail-resumptive code in direct
  style) over coroutine-frame cloning. That choice is *reinforced by the
  inherited collector*: **NewGC** scans each object by a precise
  pointer-cell range, which fits a heap-closure/CPS continuation exactly and
  fits a raw saved frame poorly. The one concrete dependency: a reified
  continuation is full of live roots, so a *moving* collector must find and
  relocate them — which means emitting LLVM `gc.statepoint` and feeding
  NewGC precise roots (the path it is architected for, shared with
  NewOpenDylan's statepoint work). That binding is Locus's named GC work
  item, not a hand-wave. Resource safety under multi-shot — does a
  `with-file` reopen if the continuation resumes twice? — is handled in the
  calculus by a **continuation-multiplicity grade** (`calculus.md` §1.3),
  not at codegen.

## The shape of Locus, stated tightly

A **graded modal core** with a comonadic **stage grade** and a monadic
**effect row** — a real algebraic, row-polymorphic effect system
(exceptions, IO purity, and GC-safety unified as one checked, default-pure
discipline) made the centerpiece; a **distributive law** as its beating
heart; **splices implemented as effect handlers**; **`static`/`const`/`macro`/CTFE unified
as staged evaluation**; **evidence-passing lowering** so statically-
resolved effects are free; **environment classifiers carrying hygiene**
through the stage grade; and **explicit stages / inferred effects** to
keep the typechecker honest.

## Where the design budget goes

The backend is inherited from my other projects, and so is the a collector — **NewGC** 
So essentially the entire design budget
concentrates on **one formal artifact: the distributive law and its
metatheory.** Get that calculus right on paper — prove **subject
reduction** and the **stage-ordering property** — and the implementation
is downhill. (LOL)

**No spec-by-implementation. The calculus is the gate.**

## References — the pieces exist; the integration does not

- **Graded monads / effect grading:** Katsumata, *Parametric effect
  monads and semantics of effect systems*. Koka — Leijen, *Type-directed
  compilation of row-typed algebraic effects*.
- **Algebraic effects & handlers (the mechanism):** Plotkin & Pretnar,
  *Handlers of algebraic effects*; Bauer & Pretnar, *Eff* / *Programming
  with algebraic effects and handlers*.
- **Evidence passing (zero-cost effects):** Xie & Leijen, *Generalized
  Evidence Passing for Effect Handlers* / *Effect Handlers in Haskell,
  Evidently*.
- **Modal staging `○`/`□`:** Davies & Pfenning, *A modal analysis of
  staged computation*.
- **Coeffects / graded comonads:** Petricek, Orchard & Mycroft,
  *Coeffects: a calculus of context-dependent computation*.
- **Effects-and-coeffects via grading + distributive law:** Gaboardi,
  Katsumata, Orchard et al.; **Granule** (Orchard, Liepelt, Eades) as the
  closest shipped vehicle.
- **Let-insertion / `genlet` via delimited control:** Kiselyov,
  *Reconciling Abstraction with High Performance: A MetaOCaml approach*.
- **Generation-as-monad:** Template Haskell's `Q` (Sheard & Peyton Jones).
- **Environment classifiers (open-code hygiene):** Taha & Nielsen,
  *Environment classifiers*.
- **One-shot / tail-resumptive handlers in a systems runtime:** OCaml 5
  effect handlers (Sivaramakrishnan et al.).
- **Resumable handlers ≈ conditions/restarts:** the Common Lisp condition
  system (Pitman, *Condition Handling in the Lisp Language Family*).
