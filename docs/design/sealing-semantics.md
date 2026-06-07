# Sealing semantics — making the capability model real (design)

*2026-06-07. Design-first response to [`sealing-enforcement-gap.md`](sealing-enforcement-gap.md):
the privacy claim in [`capabilities.md`](../capabilities.md) is currently
unenforced. This doc specifies the **target semantics** — to be agreed before
implementation. Decision taken: a seal **relabels** (encapsulates the lower power
and presents a capability), not merely hides names.*

## North star — the app must NOT inherit from below the services layer

The diagrams show `boundary → services → app` as a *stack of wrappers*, and the
intuition they encode is the spec: **a layer sees only the layer directly below's
exposed surface, never inherits names from further down.** The app talks to
services; it must not inherit the boundary's names *at all*. The current graft
violates exactly this — it flattens every layer's bindings into one scope, so the
app inherits the boundary. That inheritance **is** the defect. The semantics below
restore the diagram: each seal is an opaque floor, not a window.

## 0. The two properties a seal must deliver

For "a layer seals a power" to mean anything, crossing the seal upward (service →
app) must enforce **both**:

1. **Name-privacy.** The sealed boundary's names are *not utterable* above the
   sealing service. App code cannot write `win_cred_read`.
2. **Relabel (capability encapsulation).** A caller of a sealed service's function
   carries the service's **capability effect** (`console`), not the raw boundary
   effect (`winapi`). The row distinguishes "console output" from "credential read".

Today neither holds: the graft leaks all names, and `handle` propagates the
handler body's raw effect. Below, each property gets a mechanism.

## 1. Layers and the trust boundary (unchanged, restated)

`boundary` (L0) < `services` (L1) < `app` (L2). **Boundary + services are the
trusted base** (built by the worker/platform team); the **app is untrusted**
(deployment) — the same split the plugin model relies on. So: a *service* may
introduce a capability label and seal a lower power (it is trusted to abstract
correctly); the *app* may do neither. Enforcement targets the app crossing a seal.

## 2. Name-privacy — `exposing` becomes a real barrier

**Rule.** A module `M`'s bindings split into its **`exposing` set** (public) and the
rest (**private**). Above `M`, only the public names are in scope. Private bindings
are visible only to `M`'s own definitions.

**Layer-aware (the seal is the barrier, not each module).** A *boundary* exposes the
raw functions its sealing service needs — to that service only. A service that
`seals (winapi)` **consumes** those names and does **not** re-export them; only the
service's own `exposing` set passes further up. So `win_*` are visible to `Console`,
and above `Console` only `console_writeln` &c. exist.

**Graft desugaring (the implementation shape).** Grafting `M` around inner code must
bind, for the inner scope, *only* `M`'s exposed names. Two viable encodings:
- **Projection:** evaluate `M`'s items in a private scope and rebind only the
  exposed names outward — `let (e1..en) = (private M-scope yielding e1..en) in inner`.
- **Rename-to-unutterable:** α-rename `M`'s private bindings to names containing a
  control char user source cannot lex (the trick `$cbw…` already uses). `M`'s own
  code refers to the renamed names; inner code that writes `win_cred_read` finds it
  unbound.

Either makes `exposing` load-bearing. The current `graft_in` (peels *all* items)
is replaced/wrapped accordingly. **Consequence:** boundary modules must actually
list what their services consume in `exposing (…)` (today `winapi` lists `()`,
which only "works" because nothing is enforced).

## 3. Relabel — a seal encapsulates the lower power and presents a capability

**The gap.** `handle e with H` discharges the handled ops but unions the handler
*body's* row, so `console_writeln`'s row becomes `{winapi}`. `seal L { e }` only
*removes* `L` (`b.row.without([L])`), adding nothing.

**The rule.** A service declares the capability it offers and the powers it seals:

```
module Console at services
  seals    (winapi, mem)        -- powers consumed internally, encapsulated
  provides (console)            -- the capability presented to callers   ← new clause
  exposing (console_writeln, …)
```

The exposed functions' rows are rewritten at the service boundary:

> **row seen by a caller of an exposed fn  =  (its body row)  −  sealed(L…)  +  provided(C)**

So `console_writeln : String -> Unit ! {console}`: its body performs `{winapi}`,
the seal removes `winapi` (and `mem`), and `provides (console)` adds `{console}`.
A caller carries `{console}`. `win_cred_read` is unreachable (name-privacy, §2), so
there is no path by which a console user acquires `{winapi}` or reads credentials.

**Why this is sound (and not just hiding a fault).**
- The sealed `L` must be **discharged inside the service** — handled, or native and
  bottoming out in a runtime call — exactly the existing `seal`-expression
  condition (`SealUnhandled`, D-S3). Sealing an *unhandled user effect* stays an
  error: you cannot hide a fault, only encapsulate a power you actually serviced.
- `provides (C)` introduces a **capability label**, which is **not a mint** (no
  `extern`, no raw memory) — so services may do it; the app may not (it is not a
  sealing service). The *power* (`winapi`) is still minted only at the boundary.
- The relabel is a **trusted-base abstraction**: the platform team authors `Console`
  and is trusted to seal honestly. The app cannot relabel (no `provides` in app
  code) and cannot reach the raw power (name-privacy). So `{console}` faithfully
  means "did console things, nothing more."

**Granularity falls out.** One boundary (`winapi`) is fanned into many capabilities
by many services — `Console provides (console)`, a `Files` service
`provides (fs)`, a `Credentials` service `provides (cred)` — each sealing the same
`winapi`. An app's row now names exactly the *capabilities* it uses, never the raw
boundary. This is the fine-grained transparency the model promises.

## 4. The combined module rule

For `module M at services seals (S) provides (C) exposing (E)`:
1. **Authorize:** `M` may name the sealed powers `S` (and call the boundary names
   exposed to it). *(today)*
2. **No-escape:** no binding in `E` may have a *type* mentioning a label in `S`
   (RN-E0403). *(today)*
3. **Discharge:** every label in `S` must be discharged within `M` (handled or
   native) — else `SealUnhandled`. *(extends the `seal`-expr rule to the clause)*
4. **Relabel:** each `e ∈ E` presents `row(e) − S + C` to callers. *(new)*
5. **Privacy:** only `E` is in scope above `M`; `S`'s names and `M`'s privates are
   not. *(new — §2)*

A boundary `module B at boundary mints (P) exposing (raw…)` exposes `raw…` to the
*services* that seal `P`; those names do not reach the app (a sealing service
absorbs them per (5)).

## 5. Migration impact

- **`provides (C)` clause:** new syntax + parse + a capability-label registry.
- **Boundary `exposing`:** boundaries must list the raw names their services use.
- **Service declarations:** add `provides (C)`; the `effect *_op` decls may fold
  into the capability or remain as the internal op vocabulary.
- **Graft:** `graft_in` enforces §2; the seal clause performs §4.3–4.4.
- **Stdlib sweep:** `console`, `crt`, `time`, `docsfs`, `locusenv`, the IDE
  services, and the SQLite plugin (`sqlite`/`sqlite_fs`/`cred_access` already read
  as capabilities — they would become *provided* capabilities sealing their
  `*_access` boundaries). The full test suite is the safety net; every service must
  still type-check and every demo's manifest must now read in capabilities.
- **`capabilities.md`:** updated to match (and, until this lands, annotated that
  privacy/relabel are not yet enforced).

## 6. Worked examples

```
-- console: one boundary power, presented as a clean capability
Winapi   at boundary mints (winapi) exposing (win_write_console, win_read_line, …)
Console  at services seals (winapi, mem) provides (console)
         exposing (console_writeln, console_read_line, …)
-- app:  console_writeln "hi"   :  Unit ! { console }          (NOT winapi)
--       win_cred_read "…"      :  unbound variable            (name-privacy)

-- sqlite: the boundary power, fanned into mem/fs/cred capabilities
SqliteFfi  at boundary mints (sqlite_access) exposing (raw_open, raw_exec, …)
SqliteMem  at services seals (sqlite_access) provides (sqlite)      …
SqliteFs   at services seals (sqlite_access) provides (sqlite, sqlite_fs) …
VaultAccess at boundary mints (cred_access)  exposing (…)
Credentials at services seals (cred_access)  provides (cred)        …
-- app via Db:  db_open_memory ()  : … ! { sqlite }
--              db_open_file p     : … ! { sqlite, sqlite_fs }
--              db_open_profile n  : … ! { cred, sqlite, sqlite_fs }
```

## 7. Decisions (settled 2026-06-07)

- **Q-A — Capability identity → SHARED, coherence-by-trust.** `provides (C)` names
  a *shared* capability; several services may provide the same `C` (e.g. two file
  backends both `provides (fs)`, two SQLite backends both `provides (sqlite)`).
  Coherence is by trust: only the trusted base (boundary+services) may `provides`,
  so a shared capability name means what the platform team says it means.
- **Q-D — `provides` without `seals` → YES.** A pure-Locus service may `provides`
  a capability layered over *other capabilities* (service-over-service). `seals`
  lists whatever it encapsulates — boundary powers or lower capabilities alike.
- **Q-F — Backward compatibility → NONE. The leak is a defect; we do not stay
  compatible with it.** There is **no** "`seals` without `provides` keeps the old
  propagate-the-effect behavior" fallback. Enforcement (name-privacy §2 + relabel
  §3) is the *only* behavior. Consequence: the graft change and the **full stdlib
  migration land together** — every service declares its `exposing`/`seals`/
  `provides` correctly, and the suite + every demo manifest is the gate. Nothing
  ships in the leaky middle state.
- **Q-B — Relabel vs op vocabulary → keep ops internal** (proposed default).
  `provides (C)` is purely the caller-facing relabel; existing `effect *_op` decls
  stay as the internal mechanism. (Revise if it complicates the migration.)
- **Q-C — Multi-power seal → one capability → YES.** `seals (winapi, mem) provides
  (console)` encapsulates both and presents `console`.
- **Q-E — Visibility vs inclusion → inclusion unchanged.** The graft *trigger*
  still fires on exposed names; only *scope* tightens. An app naming a now-hidden
  private gets `unbound variable`.

These settle the *shape*. But a safety review (§8) found the design, as stated, is
**not yet sound** — it needs the revisions below before implementation.

## 8. Safety-review revisions (REQUIRED before implementation)

An adversarial review against the confinement guarantees found three blockers and
several unspecified interactions. The design is amended as follows; until these are
incorporated, sealing is "better than today," not sound confinement.

### 8.1 Reframe: `mint_gate` is the floor; sealing is granularity + hygiene
Confinement of a **raw power** does **not** rest on name-privacy. The JIT resolves
*any* process symbol by name (`LLVMOrcCreateDynamicLibrarySearchGeneratorForProcess`);
the only thing stopping app code from emitting an `extern "CredReadW"` is
`mint_gate` rejecting `extern`/`peek`/`poke` in app **source**. So:
- **The floor is `mint_gate`** + the invariant *"no path produces an `Extern`/
  `ExternAsm`/raw-mem node after the gate"* (audit staging/macros against it).
- **Name-privacy delivers *granularity and hygiene*** (the app names capabilities,
  not raw powers; the boundary surface isn't ambiently in scope), **not** raw-power
  confinement. `capabilities.md` and this doc must say this explicitly.

### 8.2 A `provides`-gate, manifest-authorized like `boundary` (BLOCKER fix)
`provides` introduces trust, so it must be **authorized**, not self-declarable.
Today `at boundary` is manifest-gated (RN-E0404) but `at services` is **not** — so
an app module could write `at services provides (cred)` and forge a capability.
**Fix:** a module carrying `provides` (or `seals`) must be in a manifest-authorized
trusted-base set (extend `locus.toml` to a trusted-base list covering services).
Unauthorized `provides`/`seals` is a hard error, exactly as an unauthorized mint is.

### 8.3 Relabel honesty must be CHECKED, not trusted (BLOCKER fix)
The discharge guard (`SealUnhandled`) short-circuits on `is_native`, so a service can
`seals (winapi)` while its body still performs an **undischarged** `winapi`, and the
seal silently strips it — hiding a fault. **Fixes:**
- Remove the `is_native` short-circuit for the *clause*: a sealed label appearing in
  a body must be **actually handled** (a `handle` arm) or reach a verified native
  runtime edge within the module, before the relabel applies.
- **Denylist of never-sealable labels:** `gc`, `exn`, and any divergence/fault
  `World` label may never be sealed/relabeled (they are the caller's consent/fault
  signals). Sealing one is a hard error (today `is_native` *silently allows* it —
  the wrong direction).
- **Declared subsumption (anti-laundering):** `provides (C) seals (S…)` must declare
  `C ⊒ S…` in a capability lattice, so the relabel is a *checked* abstraction, not an
  arbitrary rename. `LogService provides (log) seals (cred)` is rejected unless
  `log ⊒ cred` is a declared, sanctioned edge. This bounds the service-over-service
  (Q-D) laundering risk to *declared* edges.

### 8.4 Directed, per-consumer visibility — the graft must be restructured (BLOCKER)
The current graft concatenates all modules into **one shared inner scope**, so a
boundary's `exposing` set is visible identically to *every* consumer and the app.
"Each seal an opaque floor" requires **directed** visibility: `winapi` exposes
`CredReadW` to `Credentials` but **not** to `Time` or the app. With five services
over one `winapi`, an all-or-nothing single list either over-shares among the
trusted base (no least-privilege) or forces per-service boundary splitting.
**Fix (to design, then implement):** replace the flat sibling-graft with **nested
projection** — each consumer module is elaborated inside a scope containing *only*
the exposed surface of the dependencies it names, via value projection (`let (e1..)
= (private dep scope) in consumer`), **not** α-rename-by-control-char (forgeable).
Resolve the topology: **per-consumer `exposing`** (a boundary may expose different
sets to different sealing services) is the target. This is the core engineering and
must be specified as a tested transform with adversarial cases (§8.6).

### 8.5 Relabel attaches to the value's TYPE, and respects staging
- **Type, not name binding.** The relabel (`−S+C`) must be a property of the
  *value's type* so it flows through unification and the trait row-binding fix
  (`bind_method_use_rows` unifies against `body.ty`); a name-binding-only relabel is
  bypassed by trait dispatch and re-leaks `{winapi}`.
- **Staging.** A sealed object-effect inside a returned `Code[T ! {S}]` is pulled
  back into the caller's row at `splice`. The no-escape/relabel must apply to object
  rows **inside `Code` return types**, or staged actions launder the seal.
- **`mem` structural escape.** Give `mem` a datum-carries-liability rule like `gc`:
  a value backed by sealed unmanaged memory may not escape a `seals (mem)` with a
  clean row (today only `gc` has this).

### 8.6 Required adversarial tests (gate the implementation)
Trigger-pull-then-name-a-private (must be `unbound`); app shadows a service export;
two services export the same name; app name collides with a hidden private; staged
`Code[…{S}]` re-leak at `splice`; trait-method whose instance body calls a sealed
service (row must show `C`, never `S`); `seals (gc)`/`seals (exn)` (must be rejected);
unauthorized `at services provides (…)` (must be RN-E04xx).

### 8.7 Honest ledger (put in `capabilities.md`)
- **Structural (real):** no app mint (`mint_gate`); no sealed label in an exposed
  binding's *type* (RN-E0403); `gc` (and, post-8.5, `mem`) datum no-escape.
- **Trust-based (must be *authorized + checked*, per 8.2/8.3):** that `provides` is
  the trusted base; that a seal discharged what it relabels; that `C ⊒ S`.
- **Scope limits (state, don't hide):** shared capabilities erase *provider
  identity* (no horizontal peer-isolation, per `capabilities.md`); a capability's
  meaning is "the granularity the trusted base elected" — `nothing hidden` holds at
  *that* granularity, not at raw-power granularity. Retract the unqualified claim.

Implementation does **not** proceed until §8.2, §8.3, and §8.4 are specified to the
tested-transform level. See §5 for the (coordinated, no-compat) migration once they
are.
