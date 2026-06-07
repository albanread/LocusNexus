# Locus — Capabilities, Sealing, and the Layered Runtime

*Drafted 2026-05-31. A **working proposal** (OPEN), companion to
[`design.md`](design.md) §7 and [`../MANIFESTO.md`](../MANIFESTO.md). Prose-first
by intent: if this model only works inside the calculus, it is
spec-by-implementation (principle 4). It should read clearly without the maths —
the maths is the footnote, not the argument.*

> **Why Locus exists — the idea the rest serves.** Locus's effect system is *calculus-driven* ([`calculus.md`](calculus.md)): effects are a graded monad, so **every effect a computation can have is written in its type** — nothing hidden, ambient, or implicit, down to `{gc}` itself. That one fact is what sets Locus apart from everything else: it buys **transparency** (you read what code *does* straight from its signature) and **safety** (the effect row is a checked upper bound and capabilities seal every raw power, so the dangerous thing cannot happen silently or nonlocally). Every other feature exists to keep that guarantee total.

> **Ratified design rulings** — canonical in [`language-design.md`](language-design.md); where this document predates a ruling, the ruling governs.
> - **D1 — i62 in traced cells.** A generic `Int` is i62 *only when stored in a traced heap cell*, with a loud `panic` on overflow; concrete and stack-resident ints are full i64.
> - **D2 — No subtyping (v1).** "Is-a" is trait membership; "has-these-fields" is row polymorphism.
> - **D3 — Float-on-the-stack + use-inferred kinds.** A scalar is a raw full-width word on the stack/in registers (the GC never scans it). A type variable is `Uniform` (excludes `Wide` = Float/Float32/SIMD; tags integers to i62) *only at traced-store sites*. Generic float *functions* work at runtime; bulk float data uses `Array[Float]`; a concrete-float closure capture is a scalar (untraced) cell. Only a `Wide` value in a *traced* slot is a (loud) error → `Array`/monomorphize.
> - **D4 — Set-rows (v1).** Effect rows are unordered idempotent sets; scoped rows / `mask` deferred.
> - **D5 — JASM is Layer 0.** AOT-assembled, embedded in app storage at deploy, reached only via a sealed `asm` capability; no inline asm.
> - **D6 — Single-param traits (v1).** Associated types are the v2 path for collections; no multi-param/fundeps.

## The idea in one paragraph

Dangerous powers are **named effects**. The raw power to call the OS is the
`winapi` effect. A **layer** can *seal* a power so its name becomes private: code
outside the layer cannot *utter* the name, so it cannot invoke the power or build
an adapter to it. The runtime is a stack of layers — **kernel → os → services →
app** — each a transparent wrapper over the one below. Only the **kernel**
performs the raw powers; it seals them at its top edge and exports *abstract*
effects (`console`, `fs`, `alloc`, …) instead. The **app** is just the top of the
stack: it trades in abstract effects and literally cannot name the raw powers, so
it cannot call them. The unsafe layer is a sealed room with a service window — the
app orders `console`, the kernel does the WinAPI, the app never holds the key.

> **Enforcement status (2026-06-07 — now enforced).** The security claims below are
> **enforced**, via the two checks in [`design/level-enforcement.md`](design/level-enforcement.md)
> (the gap that prompted them is recorded in
> [`design/sealing-enforcement-gap.md`](design/sealing-enforcement-gap.md)):
> - **Rule 1 — mint.** App / non-boundary code cannot `extern`/`peek`/`poke` (`RN-E0402/E0404`); a raw power cannot be *forged*. This is the floor.
> - **Symbol visibility (D1).** A name resolves only at its own layer and one below, and only if exposed — `app` cannot name a `boundary` power (`RN-E0405` out-of-layer / `RN-E0406` not-exposed). An app that names `win_cred_read` is rejected, not silently grafted.
> - **Effect ceilings (D2).** A module's `seals (E)` *strips* E from its exported rows, so a sealed raw power does not rise above the sealing service — a console app's row is `{gc}`, not `{winapi}` (`RN-E0407` rejects sealing `gc`/`exn`/`insert`). Verified not to re-leak through trait dispatch or staging.
>
> **Honest residuals.** Confinement is *vertical-only* — all `services` share the
> boundary's single `exposing` surface (`Time` can name a power `Db` needs exposed;
> the *app* still cannot, being two layers down). Row granularity is *subtract-only*:
> a console app reads `{gc}` (raw `winapi` removed), not a positive `{console}`
> token — fine-grained capability tokens are an optional future step. `mint_gate`
> remains the raw-power floor; visibility/ceilings add confinement + transparency on
> top.

## The layers

```
app        — programs. Perform only abstract effects. Cannot name raw powers.
services   — higher-level effects (log, config, …) over os effects.
os         — console / fs / clock / alloc / … as effects, over kernel primitives.
kernel     — the only layer that MINTS capabilities (writes `extern`; binds
             foreign providers like the Rust GC). Performs their raw effects;
             seals them; exports abstract effects.
           ───────────────────────────  ← the security seals
the OS (winapi)      the GC (collect / move)      … other providers
```

Lower = more privileged. Each layer's modules import **only the layer below.**

## The two rules (that's all)

1. **Capability mints are kernel-only.** `extern "Sym" : T` conjures a `winapi`
   capability from nothing; binding a **foreign module** (the Rust GC) over FFI
   mints its privileged effect the same way. Both are *trusted boundary
   ascriptions* — you assert the declared effect matches the foreign code's real
   behaviour — and both are **kernel-only**. If any module could mint, sealing
   would be theatre.
2. **Seal raw, export abstract.** At its top edge each layer *seals* the raw
   effects it consumed from below and exports only the abstract effects it built
   on them. You may perform an effect **iff** some layer below exported it — and
   **no layer exports a raw power** (`winapi`, `gc!`); only the kernel performs
   them, by rule 1.

Everything else follows.

## The guarantee, stated app-side

> *(Enforced. `mint_gate` delivers "uninhabitable-by-forging"; the "unnameable" half
> is the D1 symbol-visibility check (`RN-E0405`/`RN-E0406`) — an app naming a raw
> power is rejected. See the status note at the top.)*

From outside the kernel a raw power is **unnameable** — its label is not in scope
— and therefore **uninhabitable**: no term can introduce it, because the only
introduction form (`perform`, or the foreign mint) is kernel-only. So app code
**cannot author a function that drives the OS or the GC** — not directly, not
indirectly; a `! {winapi}` or `! {gc!}` computation simply *has no inhabitant* out
there. App code can only call the kernel's **curated API** — the exported abstract
effects. Allocation is the boundary case, and it *confirms* the rule: `alloc` may
cause a collection, but only because the **runtime** chose a safepoint — the app
requested memory and consented (`gc`), it never *invoked* `gc!`.

## Sealed is not hidden

A seal removes *authority*, not *visibility*. App code cannot **wield** `winapi`
or `gc!`, but every line that implements them stays **readable**: the kernel
source shows exactly which calls `console` makes, the GC contract lives in the
types, the layers are a straight-line audit. The fiction is **disclosed where it
matters** — the `gc` label sits in the app's own row, so "this allocates; a
collector exists" is printed in the type, not buried. That is the difference
between this and a bundled black box (a browser engine dragged in to draw a
diagram): you get the convenient abstraction *and* an auditable, shrinkable floor
under it. Sealing keeps the transparency promise
([`../MANIFESTO.md`](../MANIFESTO.md)) intact — the app is denied the dangerous
*power*, never the *truth*.

These are two halves of **one** calculus-grounded mechanism, not two
features. The effect row is the **transparency** half — every authority a
computation exercises is written in its type ([`calculus.md`](calculus.md)),
down to `gc`. A capability seal is the **safety** half — the same row, used
as a *checked upper bound*: a sealed label is one the row may not contain
outside the kernel, so the raw power is **un-ambient** (no inhabitant can
introduce it) at the same time as it stays **legible** (the kernel source
that does wield it reads plainly). Locus does not trade visibility for
confinement the way a black box does; the row gives both at once, because
making the authority *written* is precisely what lets the checker *bound*
it.

## Why there is no grant graph to maintain

"Who may use what" is **read off the module-import graph** — which is already
acyclic — plus the per-boundary seals. The architecture makes that graph a linear
spine (kernel ▸ os ▸ services ▸ app), so capability flow is just *"you can perform
an effect if a layer beneath you exported it."* There is no separate grant object
to design, keep in sync, or get wrong. **The import structure is the grant
structure.**

(A general grant **DAG** — peers at one layer with *different* clearances — is a
strict generalization we are **not** building. The layer model gives *vertical*
confinement perfectly; *horizontal* peer-isolation it does not give, and v1 does
not need it. Named, deferred, not forgotten.)

## Why there is no WinAPI taxonomy

The app's confinement does **not** depend on enumerating Win32 into a label
ontology. It depends on a single fact: `winapi` is sealed and the app cannot name
it. *Which* specific calls `console` uses (`GetStdHandle`, `WriteConsoleW`) is
**visible in the kernel source**, reviewed there, and absent from the app's type.
The abstract effects emerge from **writing the wrappers**, not from a pre-built
taxonomy. One sealed label; the rest is ordinary code you can read.

## Security reduces to one seal *per provider*

The only containment-critical boundary is the **raw power at the kernel's top
edge** (`winapi`, `gc!`). A middle layer cannot escalate, because it cannot name
the raw power either — the worst it can do is pass through abstract effects it
legitimately holds. So the os / services / app divisions are **transparency**
boundaries (a layered, straight-line audit; readable wrappers), **not**
additional security tiers. Each provider contributes one seal; the layers above
them are for clarity.

**A third sealed provider: the assembler boundary (`asm`, D5).** Layer 0 — the
AOT-assembled foundation embedded in app storage — is reached only through a
**sealed `asm` capability** (`extern asm "sym" : T`); there is no inline `asm { … }`
in Locus source (D5). `asm` is a **sibling of `mem`/`extern`**: it mints from
nothing exactly as `extern` does (rule 1, kernel-only), and it **reuses the same
sealing plumbing** — one more raw power, sealed at the kernel's top edge,
unnameable above it. Hand-written Layer-0 code is gated on GC-safety before it may
touch the heap (it must obey the handle discipline and never leave a `Wide` value
where the collector would classify it). So the seal-per-provider count is now
three: `winapi`, `gc!`, and `asm`.

## A second provider: the GC (and foreign modules in general)

The OS is one sealed provider; the **garbage collector** (NewGC — Rust,
`E:\NewGC\`, bound via a `LocusLayout`) is the second, and it shows the pattern
is general, not winapi-specific.

**Split the GC's effect in two — allocation is not the dangerous part.**
- **`gc` — the client label (exported, ubiquitous).** "This code allocates, so a
  safepoint may occur in its extent." Most code carries it; the app *needs* it.
  It is the app's **consent to be collected.** Like every other power, it is
  **opt-in and visible**: `{gc}` is an effect, so **no `{gc}` in a function's
  row ⟺ that function needs no collector** — it allocates nothing, so nothing
  collects it, and (the GC being an AOT link-time decision) a program with no
  `{gc}` anywhere links no collector at all. You pay for the GC exactly when the
  row says you do, and you read that off the type, not off the runtime.
- **`gc!` — the privileged GC-internal capability (sealed, kernel-only).** Trigger
  a collection, **move** objects (NewGC is mark-*evacuate* — a moving collector),
  rewrite pointers, scan roots, touch the raw heap. *This* is the dangerous one,
  and the app can never name it.

So the seal is on `gc!`, not on `gc`. The kernel hosts NewGC, performs `gc!`
internally, seals it, and exports only `alloc : Size -> Ptr ! {gc}` upward. The
app allocates and consents; it cannot drive the collector.

**A foreign module is a trusted boundary + a seal.** NewGC is Rust — it is not
typed in Locus's rows, so the seal is checked at the *boundary*, not inside the
box: the kernel **ascribes** effect signatures to the FFI surface (`alloc … !
{gc}`; the collection driver `… ! {gc!}`) and *trusts* the Rust code matches them
— exactly the trust `extern` already asks for. That is why a foreign FFI surface
is a **mint** (rule 1) and stays kernel-only.

**The GC seal is two-sided — the GC-specific subtlety.** The OS boundary is mostly
outbound (Locus calls WinAPI). The GC also reaches **inward**: a moving collector
*rewrites pointers inside live Locus objects* — the single most privileged act in
the system, operating on app data — yet it is safe, because the app never
*invokes* it. Two projections, both narrow:
- **Outbound (GC → Locus):** `alloc` + the `gc` label. Safe.
- **Inbound (Locus → GC):** the `LocusLayout` (NewGC's `HeapLayout` binding) +
  precise roots via `gc.statepoint` / stack maps. This is the *only* reach the GC
  has into Locus memory — *forward these object cells, per this layout, at these
  safepoints* — and it is the **compiler's** channel, emitted by codegen, never an
  effect the app can perform. The collector's raw power is **bounded by the layout
  contract**; it cannot touch arbitrary bytes.

So the dangerous "move your objects" power is mediated entirely by the
**compiler/GC contract** (statepoints + stack maps + layout), which is the typed
interface of the inbound seal. The app reaches none of it.

**Payoff — `nogc` unifies.** In a `nogc` region the compiler emits no allocation
*and no safepoint*, so **both** projections close: the collector provably cannot
act there. The two readings of `nogc` — "allocates nothing" and "the GC can't
touch this" — are the **same fact**: sealing `gc` / `gc!` over a region.

*(v1 reality: NewGC is single-threaded STW with no safepoint/poll API yet, so the
inbound channel is currently coarse. The model needs **no new GC work** — it is
how the **already-planned** `gc.statepoint` + `LocusLayout` effort (PLAN Phase 4)
fits the security story. Correctness-before-perf: do not touch the collector for
this.)*

## How it lands in the calculus (the footnote)

- **Sealing is `runST`, generalized.** Locus already seals `st[T]` at the *escape
  boundary* (local mutation is pure unless it escapes — calculus §1.1). A seal is
  the same check applied to a `World` label: the sealed label must appear
  **nowhere** in the boundary's outward type — not at the top of the row, and not
  deep inside any returned closure or datum. That deep no-escape check is
  `runST`'s `∀s` trick. `nogc { … }` is the same shape (seal `gc` over a region),
  so this is not a new mechanism — it is an existing one named and pointed at
  `World` (and at the GC provider).
- **A seal is an explicit boundary handler.** Because effects are *inferred*, a
  wrapper that performs `winapi` *infers* `! {winapi}`; the seal cannot silently
  erase it. The kernel installs an adapter — a handler that re-abstracts
  `console` into `winapi` (and `alloc` over `gc!`) — and the checker enforces that
  the raw label does not leak past it. Explicit boundary, checked, not magic
  (principle 3).
- **Raw powers are perform-only and residual.** The evidence pass never *handles*
  `winapi`; it is the residual that becomes the syscall. `gc!` is the same: never
  handled in Locus, it bottoms out in the foreign provider. Sealing adds one word
  to that existing category: *private to the kernel module.*

## Implementation cost

≈ **one** new surface feature (the `seal` / boundary-`mask` operator, = generalized
`runST`) plus **one** rule (capability mints are kernel-only). *(This boundary
`mask`/seal is the `runST`-style no-escape check on a `World` label; it is **not**
the effect-row `mask` that reorders scoped rows — that one is deferred with scoped
rows under D4. Effect rows themselves are **unordered idempotent sets** in v1 (D4),
which is all the seal's "label appears nowhere in the outward row" check needs.)*
Everything else —
the layers, "call down only," capability flow — is **module-import discipline
Locus already has.** The GC's two-sided contract (statepoints, stack maps,
`LocusLayout`) is **already-planned Phase-4 work**, not new spend for this model.
The layering is the simplification: it turns a policy problem into a structure you
can read.

## OPEN — decided by wrapping Win32 (and binding the GC), not in the abstract

> **Status (2026-06-03) — the sealing/capability core is IMPLEMENTED.** The
> mint/seal build is shipped + tested ([`sealing-plan.md`](sealing-plan.md);
> [`sealing-solution.md`](sealing-solution.md); user tour in
> [`user-guide-mint-and-seal.md`](user-guide-mint-and-seal.md)): **O-C3** — minting
> is `boundary`-only + manifest-gated via `locus.toml [boundary]`
> (`RN-E0402`/`RN-E0404`), and the gate covers `extern`, raw memory
> (`peek`/`poke`/`fill`/`copy`), **and `extern asm`**; **O-C5** — per-provider mint
> labels via the **`mints (L)`** clause; the **`seal` surface** (region + module
> `seals (…)`) enforces no-escape (`RN-E0403`); the **`asm`** capability is a live
> mint label (`extern asm`, with the GC-blind gate `RN-E0405`). Still **[NYIMP]**:
> `gc!` as a mint label, and de-hardcoding the `row_label` set (fully
> manifest-driven recognition). **O-C2** and **O-C4** stay deferred as written below.

- **O-C1.** The exact abstract surface each layer exports (`console`/`fs`/…).
  Emerges from the wrappers; not pre-enumerated.
- **O-C2.** Whether *middle* seals (os→services→app) are **enforced** by the
  checker or **conventional** at first — the only *security*-critical seals are
  the raw powers at the kernel edge; the rest may start as discipline.
- **O-C3.** How "mints are kernel-only" is enforced — a privileged-module marker,
  a build manifest, or a compiler flag designating the kernel crate.
- **O-C4.** Horizontal/peer least-privilege within a layer — explicitly **out of
  v1** (the layer model is vertical by design); revisit only if a real
  mutually-distrustful-plugin need appears.
- **O-C5.** Names and granularity of the GC split (`gc` client label vs. `gc!`
  privileged capability), and whether future foreign providers get the two-sided
  treatment by construction.

---

*Companions: [`design.md`](design.md) §7 (runtime), [`calculus.md`](calculus.md)
§1.1 (the `st` escape boundary this generalizes), [`../MANIFESTO.md`](../MANIFESTO.md)
(transparency commitment). The GC provider: NewGC at `E:\NewGC\` (its `HeapLayout`
trait is the inbound seal's interface).*
