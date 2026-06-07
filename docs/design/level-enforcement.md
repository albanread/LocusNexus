# Level enforcement — symbol visibility + effect ceilings (design + sprints)

*2026-06-07. Implementation design for the two SEMA checks that make the capability
model real, distilled from two adversarial verification runs (see
[`sealing-semantics.md`](sealing-semantics.md) §8 and
[`sealing-enforcement-gap.md`](sealing-enforcement-gap.md)) and the level model the
user specified. Replaces the over-engineered `provides`/relabel machinery.*

## 0. The model in one screen

Layers: **`boundary`=0, `services`=1, `app`=2**; the **world** (raw OS symbols) is
below 0, reached only by `extern` inside boundary, gated by `mint_gate`.

**Two independent dimensions. Do not conflate them.**

- **D1 — Symbol visibility (the safety).** Every binding carries its module's
  **level**. A reference in a level-`D` module resolves a name only if the name's
  home module is at level `D` or `D-1` **and** the name is `exposing`-public (or
  same module). No down-call past one level (`D-2` is unbound). **This is what
  confines powers** — *wielding requires naming*; app code that cannot name
  `win_cred_read` cannot perform it, whatever any row says.

- **D2 — Effect ceilings (the transparency).** Every effect carries a **max level
  (ceiling)**. An effect may appear in rows up to its ceiling; **above its ceiling
  it is sealed (stripped)**. Raw world effects (`winapi`, `libc`, `crt`, `asm`) have
  ceiling **0** — confined to boundary, never pollute service/app rows. Capability
  effects (`sqlite`, `sqlite_fs`, `cred`, …) and ambient runtime (`gc`, `mem`) have
  ceiling **2** — they reach the app, which is where useful granularity lives. This
  is row honesty/granularity; it carries **zero** confinement weight (D1 does that).

**Load-bearing invariant:** a row label is `BTreeSet<Label>` of inert strings —
evidence, not authority. So a *forgotten* ceiling/seal degrades transparency,
never confinement. Confinement is D1 + `mint_gate`, full stop.

## 1. D1 — symbol visibility, precisely

1. **One level down.** Level-`D` module resolves names at level `D` and `D-1` only.
   `app(2)`→{2,1}; `services(1)`→{1,0}; `boundary(0)`→{0, world}. `D-2` = unbound.
   Upward never resolves.
2. **Private except `exposing`.** A binding is private unless in its module's
   `exposing (…)`; private = invisible to every other module incl. same-level
   siblings. Omitted `exposing` = expose-nothing. Cross-module resolution requires
   **exposed ∧ level∈{D,D-1}**. A module always sees its own privates.
3. The escalation (`win_cred_read`, boundary level 0, used at app level 2) fails on
   **either** condition independently (not exposed by `winapi`; and two-down).

**Confinement is vertical-only (accepted):** all level-1 services share boundary's
single `exposing` set — `Time` can name `win_cred_read` if `Db` needs it exposed.
The app still can't (two-down). This makes the `winapi exposing` migration
security-critical (§5, H2).

## 2. D2 — effect ceilings, precisely

1. **Every effect has a ceiling level**, assigned where it is introduced:
   - Boundary-minted **world** effects (`winapi`, `libc`, `crt`, `asm`, raw `mem`-as-mint): **ceiling 0**.
   - Ambient runtime: `gc` **ceiling 2**, `mem` (the safe-surface effect) **ceiling 2**.
   - Plugin/service **capability** effects (`sqlite`, `sqlite_fs`, `cred`, …): **ceiling 2** (declared at the plugin's mint).
2. **Strip above ceiling.** A function at level `L` has row = (accumulated effects)
   with every effect `E` such that `ceiling(E) < L` removed. So `winapi` (ceiling 0)
   is absent from any level-≥1 row; `console_writeln` (level 1) reads `{gc}`, the app
   reads `{gc}`. `sqlite` (ceiling 2) survives to the app → `{sqlite, gc}`.
3. **Never-sealable / never-confinable effects.** `gc` and `exn` may not be given a
   ceiling below 2 (no handler discharges allocation; a sealed-undischarged `exn`
   hides a fault). This is the **inverted denylist** (today `is_native` wrongly lets
   them be sealed silently — `sema.rs:6202`).
4. **Honesty (keep + extend).** Stripping an effect at a level crossing requires it
   was actually discharged below (handled, or native bottoming out in a runtime
   call); a non-native `User`/`Exn` still live → `SealUnhandled`. The existing
   type-no-escape check (`seal_escape`/RN-E0403) stays.

**Granularity is per-capability and opt-in.** A capability shows in app rows iff it
is introduced as a ceiling-2 effect (the DB plugin already does this). The stdlib
`console` currently discharges its op locally, so a console app reads `{gc}` (no
`console` token). Giving `console`/`fs` app-row granularity = refactor those
services to perform a ceiling-2 capability effect — **Sprint 5, optional**.

## 3. The SEMA mechanism ("dumb as a brick", grounded)

- **`BlockItem::Scope { depth: u8, home: ModuleId, items }`** — a new, **runtime-
  transparent** block wrapper. `peel_block_items` (`stdlib.rs:~530`) wraps each
  grafted module's items in it instead of splicing them naked. Eval/IR ignore it
  (same single `Ctx`); it exists only so resolution can thread `current_depth` /
  `current_home`. This is the one genuine addition — a name-keyed side-table is
  **insufficient** because the flat graft erases the *use-site* module (a service's
  legal one-down `win_write_console` and an app's illegal two-down use are
  byte-identical terms). NOT the rejected nested per-consumer projection.
- **Pass 2a (D1)** — scope-driven name resolution in `elaborate_block`: for `Var n`
  in scope `(D, M)`, resolve `M`-own → then `D-1` parent only; require
  `home==M ∨ (exposed ∧ level∈{D,D-1})`, else **unbound variable** (clean
  diagnostic). Scope-driven (not a post-hoc audit over the last-wins `HashMap`) so
  identically-named privates at different levels don't mis-*bind*.
- **Pass 2b (D2)** — ceiling strip: when finalizing a function/module-export row
  (post-zonk), drop every `E` with `ceiling(E) < level`. Cleanest seat: extend
  `check_module_seals` (`capability.rs:169`) / the row finalization at the module
  edge. `elaborate_handle` is **unchanged** (its handler-body union is corrected one
  level up). Effect ceilings live on the `Label`/effect declaration.
- **`mint_gate` unchanged** — still the raw-power floor. Plus the **audit** that no
  pass synthesizes an `Extern`/raw-mem node *after* the gate (Sprint 0).

## 4. What is rejected (stays rejected)

No `provides` clause, no positive-capability minting at the seal, no `C ⊒ S`
subsumption lattice, no nested per-consumer graft projection, no horizontal
per-consumer least-privilege. The per-effect ceiling replaces per-module-edge
sealing as the primary mechanism (simpler; `seals` becomes redundant for the
ceiling and is repurposed/retired — Sprint 3 decides).

## 5. Known holes to close (from verification)

- **H1 (blocker)** — Pass 2a needs the use-site module → the `BlockItem::Scope`
  marker (Sprint 1).
- **H2 (serious)** — `winapi exposing ()` → curated helper list is security-critical
  (fail-empty → hand-curated, shared by all services). Must expose `win_cred_read`
  (Db needs it) but never raw `extern`/alloc bindings. **Suite denylist assertion**
  required (Sprint 2/4).
- **H3 (serious, doc)** — correct stale text: ceiling is per-effect (not "no row
  above depth S"); `mem` *is* sealed/ceiling'd; fix the `console_layer_seals_winapi`
  docstring. (Sprint 0/4.)
- **H4 (minor)** — diagnostic for a service that strips a native effect it didn't
  discharge (`console_float` is a live example). Advisory lint (Sprint 4).
- **H5 (minor)** — unify Windows (User op, discharged) vs Linux (native `console`
  World op) console discipline (Sprint 4/5).

---

# Sprint plan

Each sprint ends with the **full suite green** and named tests. Dev may use a
temporary internal gate to keep things green mid-migration; the shipped semantics
have no compat shim.

### Sprint 0 — Honesty + audit (no behavior change) — SMALL
- Correct `capabilities.md`: floor = `mint_gate`; row = transparency at boundary
  granularity; confinement = symbol visibility (not yet enforced — mark it). Fix the
  `console_layer_seals_winapi` docstring (H3).
- **Audit** the `mint_gate` floor invariant: prove no pass (staging `quote`/`splice`,
  any macro/graft step) can synthesize an `Extern`/`peek`/`poke` node *after*
  `mint_gate` runs. Write a test asserting it. (The one floor-leak the value review
  would not sign off without.)
- Exit: docs honest; floor invariant tested.

### Sprint 1 — Level tagging + the transparent scope marker — FOUNDATION
- Add `BlockItem::Scope { depth, home, items }`; make `peel_block_items` wrap each
  grafted module. Thread `current_depth`/`current_home` through `elaborate_block`.
  Eval/IR treat it transparently. **No enforcement yet.**
- Tag every value binding with `(level, home)` (today only Type/Trait/Instance carry
  home).
- Exit: suite byte-for-byte green (pure plumbing); a debug dump shows correct
  level/home per binding.

### Sprint 2 — D1 visibility enforcement (CLOSES THE ESCALATION) — CORE
- Implement Pass 2a (resolve at `D`/`D-1`, exposed-only, own-privates) as
  scope-driven resolution.
- Migrate stdlib `exposing` lists to the curated helper surface (esp. `winapi`,
  H2) + the denylist assertion test (no raw `extern`/alloc binding exposed).
- Tests: `sealing-escalation-repro.locus` now **fails** (`unbound win_cred_read`);
  app shadow/collision cases; two-down rejection; same-level-private isolation;
  five-services-over-winapi all compile.
- Exit: escalation closed; suite green.

### Sprint 3 — D2 effect ceilings — TRANSPARENCY
- Add a ceiling to effect introduction (boundary world effects → 0; gc/mem → 2;
  plugin capabilities → declared, default 2). Implement Pass 2b strip at the module
  edge. **Invert the gc/exn denylist** (`sema.rs:6202`): hard-reject sealing/ceiling
  `gc`/`exn`/`insert`; allow `st` via `seal_escape`; native `World` strips.
- Decide `seals` clause fate (retire vs keep as authorization).
- Tests: `console_writeln` app row = `{gc}` (no winapi); a DB app row = `{sqlite,…}`
  (ceiling-2 survives); `winapi` absent from every level-≥1 row; rejecting
  `seals (gc)`.
- Exit: rows honest at capability granularity; suite green.

### Sprint 4 — Hardening — POLISH
- H2 denylist test hardened; H3 doc corrections complete; H4 forgotten-native-seal
  advisory lint; H5 cross-platform console discipline unified. Full adversarial
  test suite from §8.6 of `sealing-semantics.md`.
- Update `capabilities.md` from "not yet enforced" to the now-true statements.
- Exit: model fully enforced + documented honestly; suite green.

### Sprint 5 — Capability granularity (OPTIONAL, decide later)
- Refactor stdlib services (`console`, files, …) to perform ceiling-2 capability
  effects so app rows distinguish `console` vs `fs` vs `cred`. Only if the auditor
  needs finer than "raw effects minus confined." The DB plugin already demonstrates
  the pattern.
