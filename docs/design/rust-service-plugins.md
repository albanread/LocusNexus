# Rust Service Plugins — a design for wrapping the world

*How a Rust crate (serde_json, sqlx, quick-xml, image, …) becomes a sealed
Locus service. A **compile-time, platform-team-owned** plugin model: the team
that builds the Locus worker grants capabilities; deployment never can. This is
the mint/seal model ([modules-and-capabilities](../guide/modules-and-capabilities.md))
extended into an organized, repeatable Rust-side contract.*

Status: **design — decided.** Trust model and the design notes below are
settled; this is the spec a first plugin is built against. It formalizes the
wiring services already use, so adding the next one is a self-contained drop-in
instead of four hand-edits across the worker.

---

## The one idea

A **service** is a capability from the outside world — read a file, parse JSON,
query a database — named by an **effect** and reached only through a **sealed**
interface. Today each one is wired by hand across several places in the worker. A
**plugin** is those things bundled into one self-contained, registered crate:

| Part | What it is | Where it lives today (ad hoc) |
|---|---|---|
| **Rust shim** | `extern "C"` functions wrapping the crate | `cp_exports.rs` / `locus-rt` |
| **Boundary module** | `at boundary mints (raw)` — declares the externs | `stdlib/*.locus` |
| **Service module** | `at services seals (raw)` — exposes the effect | `stdlib/*.locus` |
| **Effect label** | the capability in the row (`{json}`) | `Label::World(name)` |
| **Symbol table entry** | name → function address for the JIT | `ide_symbol_table()` |
| **Module registration** | the `include_str!` graft list | `WINDOWS_MODULES` |

A plugin packages all of these and registers through **one** descriptor in a
central grant list, so the worker collects symbols + modules automatically and
nothing core is edited per plugin.

---

## Trust model: compile-time, not deployment

The requirement is exact: **the team that builds the Locus worker controls which
plugins exist; the team that deploys a program cannot add one.** That settles the
linking model — plugins are **statically compiled into the worker**, not loaded
at runtime.

- **Why not dynamic (`dlopen`/`.dll`/`.so`)?** A runtime plugin loader is a
  capability the *deployment* controls. Whoever drops a `.dll` next to the binary
  mints `extern` into the language — exactly the authority the mint-gate
  (`RN-E0402`) exists to deny app-land. Dynamic loading would move the floor into
  deployment's hands, breaking the asymmetric-trust split the whole model rests
  on ([safety-through-transparency](../articles/safety-through-transparency.md)).
- **Compile-time = the TCB.** The plugin set is fixed when the platform team
  builds `locusc` / the worker, just as the boundary `.locus` modules are
  `include_str!`'d into the binary today. To add a capability you need the
  worker's source and a rebuild — a platform-team act, reviewable like any other.
- **The registry is a grant list.** The central `service_plugins()` table is the
  human-auditable record of every capability the worker grants. One file, read
  top to bottom, answers "what can programs built with this worker reach?" That
  auditability is a feature; we prefer it to decentralized auto-registration.

> A future *sandboxed* dynamic tier (capability-restricted, signed, behind a
> manifest grant) is possible but is a separate trust story. v1 is static.

---

## Anatomy of a plugin

A plugin is a **Rust crate** under `plugins/<name>/` containing the shim and its
two co-located `.locus` modules, plus one entry in the central registry.

```
plugins/
  json/
    Cargo.toml             # serde_json + the locus-plugin support crate
    src/lib.rs             # the extern "C" shim + the plugin descriptor
    locus/
      json_boundary.locus  # at boundary mints (json_ffi)  — the externs
      json.locus           # at services seals (json_ffi) exposing {json}
  sql/  …
  xml/  …
plugins-registry/
  src/lib.rs               # service_plugins() -> Vec<ServicePlugin>  (the grant list)
```

### 1. Boundary module — mints the raw effect

Layer-0. Declares the `extern "C"` symbols the shim provides and **mints** a raw,
per-plugin effect (`json_ffi`) so every extern carries it. Resolves only inside
the worker (host-provided symbols), exactly like `IdeGraphics` over `iGui.*`.

```locus
module JsonFfi at boundary mints (json_ffi) exposing () =
  -- parse UTF-8 bytes → a host-owned document. >0 handle on success, 0 on error
  -- (the message is on the per-call last-error; see the error convention below).
  let json_parse  = extern "json.Parse"  : Ptr -> Int -> Int in
  let json_free   = extern "json.Free"   : Int -> Unit in
  -- navigation: 0 means "no such child" (an absence, not an error).
  let json_field  = extern "json.Field"  : Int -> Ptr -> Int -> Int in
  let json_index  = extern "json.Index"  : Int -> Int -> Int in
  let json_kind   = extern "json.Kind"   : Int -> Int in   -- 0=null 1=bool 2=num 3=str 4=arr 5=obj
  let json_as_int = extern "json.AsInt"  : Int -> Int in
  let json_err    = extern "json.LastError" : Unit -> Int in  -- a String handle, or 0
  ()
```

### 2. Service module — seals it, exposes the capability

Layer-1. Wraps the raw handle API behind a tidy, **resource-safe**,
**Result/Option-typed** surface and **seals** `json_ffi` so the app only ever
names `{json}`. The raw FFI label never escapes.

```locus
module Json at services seals (json_ffi)
  exposing (with_json, field, index, as_int) =
  -- Scoped open: parse, run the body with the doc, ALWAYS free — a handle can't
  -- leak (the runtime mirror of `seal`'s no-escape). Failure → Err with the
  -- boundary's message; success → Ok of the body's result.
  let with_json = fn bytes: String => fn body: Int -> a =>
    let doc = json_parse (str_ptr bytes) (str_len bytes) in
    if doc == 0 then Err (string_of_handle (json_err ()))
    else
      let r = body doc in
      let _ = json_free doc in
      Ok r
  in
  -- navigation is total-but-partial → Option, not Result.
  let field = fn doc: Int => fn name: String =>
    let h = json_field doc (str_ptr name) (str_len name) in
    if h == 0 then None else Some h
  in
  let index  = fn doc: Int => fn i: Int =>
    let h = json_index doc i in if h == 0 then None else Some h in
  let as_int = fn node: Int => json_as_int node in
  ()
```

App code reads `{json}` in its row — provably confined to JSON, never the raw FFI:

```locus
with_json input (fn doc =>
  match field doc "player" with
  | Some p => (match field p "score" with | Some s => as_int s | None => 0)
  | None   => 0)
-- row: {json}
```

### 3. Rust shim — the crate, behind the C-ABI

`extern "C"` functions named to match the boundary's symbols, using the **handle
ABI** (below). The crate's rich values live in a host-side registry; Locus holds
only `i64` handles. `#[locus_export]` both sets the export name and records the
`(name, address)` pair into a per-crate table, so the descriptor's `symbols()` is
derived, not hand-listed twice.

```rust
use locus_plugin::{Registry, str_in, string_out, set_last_error, last_error, exports, plugin_modules};

static DOCS: Registry<serde_json::Value> = Registry::new();

#[locus_export("json.Parse")]
extern "C" fn json_parse(ptr: i64, len: i64) -> i64 {
    match serde_json::from_slice(str_in(ptr, len)) {
        Ok(v)  => DOCS.insert(v),                       // fresh handle (>0)
        Err(e) => { set_last_error(e.to_string()); 0 }  // 0 + message
    }
}

#[locus_export("json.Free")]
extern "C" fn json_free(h: i64) { DOCS.remove(h); }     // idempotent, double-free-safe

#[locus_export("json.Field")]
extern "C" fn json_field(h: i64, ptr: i64, len: i64) -> i64 {
    DOCS.with(h, |v| v.get(str_in(ptr, len)).cloned().map_or(0, |c| DOCS.insert(c)))
}

#[locus_export("json.LastError")]
extern "C" fn json_last_error() -> i64 { last_error().map_or(0, string_out) }
// json.Index / json.AsInt / json.Kind … likewise

pub fn plugin() -> locus_plugin::ServicePlugin {
    locus_plugin::ServicePlugin {
        effects:  &["json"],
        modules:  plugin_modules!(0 => "json_ffi" : "../locus/json_boundary.locus",
                                  1 => "json"     : "../locus/json.locus"),
        symbols:  exports!(),   // the #[locus_export]s collected in this crate
    }
}
```

### 4. Registration — one line in the grant list

```rust
// plugins-registry/src/lib.rs
pub fn service_plugins() -> Vec<locus_plugin::ServicePlugin> {
    vec![
        json::plugin(),
        sql::plugin(),
        xml::plugin(),
    ]
}
```

Adding a plugin touches **its own crate and one registry line** — never
`cp_exports`, `stdlib.rs`, or the core compiler.

---

## Worker integration — how the grant list flows in

`service_plugins()` is consumed at exactly three seams, matching how symbols
resolve in each build/run path:

1. **Module graft (all paths).** Each plugin's `modules` are appended to the
   stdlib source list the grafter sees — `WINDOWS_MODULES ++ plugin modules`. The
   name-fixpoint then auto-includes a plugin **only when a program uses one of its
   exposed names**, so the JSON crate adds nothing to a program that never parses
   JSON. (This is the one place the worker reads the registry while compiling.)
2. **IDE / JIT (in-process).** The JIT links `extern "json.Parse"` by name; the
   worker passes it a symbol table. `ide_symbol_table()` is extended with every
   plugin's `symbols()`:
   `ide_symbol_table() ++ service_plugins().flat_map(|p| p.symbols)`. (For a JIT
   that already resolves against the host process's own exported symbols, this is
   belt-and-suspenders; the explicit table keeps it deterministic and lets a
   plugin expose a symbol the linker would otherwise dead-strip.)
3. **AOT / `locusc build` (link).** The plugin crates are ordinary dependencies of
   the worker, so their `#[locus_export]` (`= #[no_mangle] extern "C"`) symbols are
   in the binary and **co-resolve at link** with the emitted `call json.Parse` —
   no table needed, same as `locus_*` runtime symbols today.

So the registry is read in one compile path and one JIT path; AOT falls out of
linking the crates. None of the three needs a per-plugin edit.

---

## The handle ABI — how rich values cross

The C-ABI carries `i64` / `f64` / `Ptr` only. The conventions, pinned:

- **Scalars** (`Int`/`Float`/`Bool`) cross inline.
- **Strings** cross **in** as `(Ptr, Int)` (a UTF-8 view — `str_in(ptr,len) -> &[u8]`)
  and **out** as an `i64` String handle (`string_out(s) -> handle`, over the
  runtime's `locus_string_from_utf8`). Plugins never hand-roll UTF-8 marshalling —
  that path is centralized in `locus-plugin`, so buffer ownership is handled in
  one audited place.
- **Rich values** (`serde_json::Value`, a `PgConnection`, a parsed tree) are
  **host-owned** in a per-plugin `Registry<T>` (a slab behind a `Mutex`) and shown
  to Locus as an **opaque `i64` handle**. Every op takes the handle. The
  Canvas/agent pattern, formalized.
- **Bulk data** (a file's bytes, an image's pixels) use the **shared-buffer**
  pattern: the host owns a buffer, Locus gets a handle + its base address + length
  and reads it with `peek` (the `{mem}` effect — the cost stays in the row), freed
  on close. One crossing, not one-per-byte.
- **The result convention (pinned).** A handle return uses **`0` = "no value"**;
  the *service* assigns the meaning:
  - a **total-but-partial** op (field lookup, array index) → `0` is `None`; the
    service returns `Option[Handle]`.
  - a **fallible** op (parse, connect, query) → `0` is failure; the shim calls
    `set_last_error(msg)`, and the service reads `*_LastError()` to build
    `Result[String, T]` (or raises `exn` where that reads better). `set_last_error`
    / `last_error` are a per-thread cell in `locus-plugin` (errno-style; the worker
    calls are synchronous on one thread, so this is race-free).
  Raw error codes never reach the app; only `Option`/`Result`/`exn` do.

**GC-blindness (the `RN-E0405` rule).** A boundary signature may name only
GC-blind types — scalars, `Ptr`, `Unit`, `String`. Handles are `i64`, so they are
GC-blind by construction. A plugin never takes a raw `Array`/sum/record/`Tuple`
(a movable managed datum) across the boundary; bulk Locus data goes via the
shared-buffer pattern (or pinning, once GC pin hooks land — a future addition).

---

## Resource discipline is part of the contract

A leaked handle is a leaked `serde_json::Value` or an open DB connection. The
model makes lifetime the **service's** job, not the app's:

- **Scope by default.** The headline API is `with_<thing> : Open -> (Handle -> a) -> a`
  that opens, runs the body, and frees on the way out — so the handle cannot
  escape. Bare `open`/`close` exist for advanced use but are not the front door.
- **One owner.** The `Registry<T>` is the single owner; `remove` is idempotent and
  double-free-safe.
- **Effect honesty.** The row shows the service's effect (`{json}`, `{sql}`). A
  plugin that also allocates on the GC heap carries `{gc}`; one that pokes a shared
  buffer carries `{mem}` — the cost is in the type.

---

## Concurrency & blocking (pinned: blocking v1)

Worker→plugin calls are **synchronous on the calling thread** (the program's
worker thread). So:

- **DB / network / any async crate blocks v1.** The plugin owns its runtime
  (e.g. a `tokio` runtime it `block_on`s, or a blocking client) and a call blocks
  the program until it returns — the honest synchronous-effect semantics (a
  `query` that takes 200 ms is a 200 ms call, visible as `{sql}` in the row).
- **Registries are `Mutex`-guarded** for soundness, but contention is nil (one
  synchronous caller). A plugin op must not re-enter Locus while holding its
  registry lock.
- **Cancellation** rides the cooperative-interrupt channel later: a long blocking
  op is a place a future poll/cancel hook plugs in. Not v1.
- A future **async tier** would expose a *future-handle* + a `poll`/`await`
  primitive so a program can interleave IO with its event loop — designed when a
  plugin actually needs it.

---

## Capability granularity (pinned: one effect per capability)

One effect per plugin is the default and keeps the row honest. A plugin **may
expose more than one effect when the capability splits meaningfully** — a
filesystem plugin minting/sealing `{fs_read}` and `{fs_write}` so a reader's row
is distinguishable from a writer's (least privilege, readable in the signature).
Hence `effects: &[…]` is a list. Resist a coarse `{data}` that hides what code
touches.

---

## The support crate (`locus-plugin`)

Platform-owned; removes the boilerplate so a plugin is mostly "wrap the crate":

- `Registry<T>` — the handle slab (`insert -> handle`, `with(h, f)`, `remove`).
- `str_in` / `string_out` — the centralized string marshalling over the runtime.
- `set_last_error` / `last_error` — the per-thread error cell.
- `ServicePlugin`, the `plugin_modules!` and `exports!` macros, and
  `#[locus_export]` — the descriptor + collection (a per-crate `linkme` slice,
  still compile-time and auditable; the *grant list* stays explicit).

---

## How it composes with the existing model

- **Mint/seal is unchanged.** The boundary `mints` the raw FFI effect; the service
  `seals` it; the app trades in the abstract `{json}`. The mint-gate still blocks
  app-land `extern` (`RN-E0402`); plugin boundary modules are trusted because they
  are baked into the worker — the same TCB argument as the stdlib boundary modules.
- **The cost model stays visible.** `{json}` / `{sql}` / `{fs_write}` in a row tells
  you what code reaches, readable from the signature.
- **Auto-inclusion still applies.** A plugin's modules graft only when a program
  names them.

---

## Decisions

1. **Trust/linking** — compile-time static; no runtime loader in v1. *(settled)*
2. **Symbol derivation** — `#[locus_export]` sets the export name *and* records
   `(name, addr)` into a per-crate slice; `symbols()` is derived. The grant list
   (`service_plugins()`) stays explicit for auditability.
3. **Errors** — `0 = no value`; service assigns `Option` (partial) vs `Result`/`exn`
   (fallible) reading a per-thread `last_error`. One convention, in `locus-plugin`.
4. **Async** — blocking v1, plugin owns its runtime; future-handle tier is a future addition.
5. **Granularity** — one effect per *capability* (`effects: &[…]`), split for least
   privilege (`{fs_read}`/`{fs_write}`); no coarse umbrella effect.
6. **Bulk Locus→plugin data** — shared-buffer (`{mem}`); GC pinning arrives with the
   pin hooks.

---

## Worked end-to-end: the JSON plugin

The four artifacts above (`json_boundary.locus`, `json.locus`, `src/lib.rs`, the
registry line) are the *entire* plugin. A program:

```locus
-- parse an agent's reply and pull a field; row reads {json}
with_json reply (fn doc =>
  match field doc "command" with | Some c => ... | None => ...)
```

…builds with no change to the compiler, links the JSON symbols statically, and
carries `{json}` in its manifest. Swapping `serde_json` for `simd-json` is a
shim-internal change — the boundary, the service, and every program are
untouched. That is the payoff: **the world's Rust crates, organized behind sealed
Locus effects, added one self-contained plugin at a time, entirely under the
worker team's control.**
