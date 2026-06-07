# Sealing is not enforced — the privacy claim is currently false

*2026-06-07. A **critical finding**, surfaced while building the SQLite plugin.
[`capabilities.md`](../capabilities.md) states the model's core promise:*

> "A **layer** can *seal* a power so its name becomes private: code outside the
> layer cannot *utter* the name, so it cannot invoke the power or build an adapter
> to it."

**This is not enforced.** Code outside a layer **can** utter a sealed power's name
and invoke it. If the privacy claim is load-bearing for the security model — and it
is — then the model is, as stated, a lie. This document proves it, isolates the
cause, and proposes the fix.

## Proof

```locus
-- app uses ONLY a harmless console service...
let _ = console_writeln "hello" in
-- ...yet directly invokes the OS CREDENTIAL-READING power from the same boundary:
let secret = win_cred_read "Git:https://github.com" in
0
```

This **type-checks and runs**. `win_cred_read` is defined in the `winapi` boundary
(`stdlib/winapi.locus`), which declares `exposing ()` — it exposes *nothing*. The
`Console` service `seals (winapi, mem)` and is supposed to be the *only* legitimate
surface over `winapi`. Yet app code reached `win_cred_read` directly, around the
seal.

Two failures, not one:

1. **Privacy fails.** A non-exposed boundary binding is nameable by app code.
2. **Granularity fails.** Both a pure-console program and the credential-reading
   program above have the **same** effect row `{ mem, winapi, gc }` — because
   `Console` seals `winapi` but the underlying effect propagates unchanged. So the
   row cannot distinguish "writes to the console" from "reads your credentials".
   The whole point of a service layer — to present a *fine-grained* capability
   (`console`, `cred`) instead of the raw boundary (`winapi`) — is defeated.

## Root cause

`stdlib.rs::graft_in` grafts a module by peeling **all** its `let` bindings into a
`Block` that wraps the inner code:

```rust
fn graft_in(module, user, home) -> Term {
    let (items, tail) = peel_block_items(module, home); // ALL items
    Term::Block(items, Box::new(user))                  // all in scope for `user`
}
```

It never consults `exposing`. So every binding of every grafted module is in
lexical scope for every inner module, including the app. `exposing` is used **only**
for: the graft *trigger* (whether to include a module at all,
`stdlib.rs::exposed_names`), the `.locusi` interface, and the seal-no-escape check
(`capability.rs::check_module_seals`, RN-E0403 — a module may not *expose a binding
whose type mentions* a sealed label). None of these stops the app from **naming** a
non-exposed binding that is in scope.

## What *is* enforced (so it is not entirely a lie)

- **No minting (RN-E0402/E0404).** App / non-boundary code cannot `extern` or do
  raw `peek`/`poke`/`fill`/`copy`. You cannot *forge* a raw power. (`mint_gate`.)
- **Seal no-escape (RN-E0403).** A module cannot *expose* a binding whose type
  carries a sealed label. (`check_module_seals`.)
- **Effect transparency (coarse).** Whatever you call, its effect is in your row —
  but at *boundary* granularity (`winapi`), not capability granularity.
- **Soft reachability.** You cannot summon a boundary by naming its private
  functions — the graft trigger only fires on *exposed* names, so an app that
  mentions only `win_cred_read` (and no service that pulls in `winapi`) gets
  `unbound variable`. The leak requires a service the app already uses to drag the
  boundary in. But once dragged in, the *entire* boundary surface is reachable.

So: forging is prevented and effects are transparent-at-the-boundary, but the
**name-privacy** that makes sealing a real confinement mechanism is absent.

## The fix (proposed)

Enforce `exposing` as a **visibility barrier** in the graft, layer-aware so seals work:

1. **A module's inner scope sees only its `exposing` set.** Grafting `M` around
   inner code must bind, for `inner`, only `M`'s exposed names — not its private
   helpers. (Desugar to: evaluate `M`'s items in a private scope, project only the
   exposed names outward; or alpha-rename non-exposed bindings to un-utterable
   names.)
2. **`seals (L)` is a privacy barrier for `L`'s names.** A boundary exposes its raw
   functions to the *next* layer (the service that seals it); a service that
   `seals (winapi)` consumes those and does **not** re-expose them — only the
   service's own `exposing` set passes further up. So `winapi`'s `win_*` are visible
   to `Console`, but `Console`'s consumers see only `console_writeln` &c.
3. **Consequence for the stdlib:** boundary modules must actually `expose` what
   their sealing services consume (today `winapi` exposes `()`, which "works" only
   because nothing is enforced). This is a real, mechanical migration of the
   `exposing`/`seals` declarations, validated by the existing services continuing
   to type-check.
4. **Granularity falls out of (2):** once `winapi` is sealed behind `Console`, an
   app that wants console output gets a `console`-flavored capability, not raw
   `winapi` — and an app cannot reach `win_cred_read` at all unless it imports a
   service that exposes a credential capability. The row then distinguishes
   "console" from "cred".

This is foundational and high-blast-radius (it touches the graft and every stdlib
module's interface), so it warrants a careful, dedicated change with the full suite
as the safety net — not a hasty patch. But until it lands, **the security sections
of `capabilities.md` describe properties the implementation does not have**, and
should say so.

## Bearing on the database design

The same gap is why [`database-access.md`](database-access.md)'s "`Credentials`
service that seals `cred_access`" refinement could not hard-hide the raw vault
functions, and why secret confinement there rests on the **`Secret`-has-no-read-
accessor** type-level fact (which *is* real) rather than on name-privacy (which is
not). Fixing the graft makes the layered confinement real end-to-end.
