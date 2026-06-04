# Articles

Longer-form writing about Locus — the ideas, and the language in action. Each
article is reproducible against the code in this repo.

**The Phantom diptych** — Locus's two halves, told as one idea: a zero-cost
abstraction is a *phantom* that haunts the compile (the types, the rows, the
stages) and is gone from the runtime. One ghost, two masks.

- [**The Phantom of the Handler**](the-phantom-of-the-handler.md) — the monad
  (effects). Handlers that do their work and fold to a single `mov`; multi-shot
  continuations as heap closures; mutable `State` from a pure handler, no cell.
- [**The Phantom of the Stage**](the-phantom-of-the-stage.md) — the comonad
  (staging). Code that writes code at compile time and vanishes: a static choice
  folds to a constant, a compile-time value is baked in, and `power` — a recursive
  code-builder — specializes `yⁿ` to straight-line `imul`s with no recursion left.
  Plus δ: staged code can be effectful, and the row stays honest.


- [**The `mem` effect in action**](the-mem-effect-in-action.md) — a real
  UTF-16 → UTF-8 transcoder written in Locus using only the `mem` capability (no
  `WideCharToMultiByte`, no runtime help), read from its type (`Int ! {mem,
  winapi}`) all the way down to the x86-64 it compiles to. The thesis: Locus is
  *both* a very-high-level language (effects, handlers, staging) *and* a systems
  language (raw `peek`/`poke`/`[i]`/FFI), and the effect system is the honest seam
  between them — low-level power that's typed and tracked, not an `unsafe`
  carve-out.

- [**Safety through transparency: a lead, not a leash**](safety-through-transparency.md)
  — why Locus tracking every effect in the type is a *lead*, not a leash: not a
  sandbox (real ones still belong at other levels), but two ordinary strengths for a
  team and its fallible collaborators — human and AI. **Auditability:** review the
  row, not every line; a change that grows the footprint shows up in the diff.
  **Mistake prevention:** a guardrail a team builds into its own compiler — `extern`
  is layer-0-only, a *dev* build warns and a *prod* build blocks. Reproducible with
  `locusc effects` and `locusc republish`.
