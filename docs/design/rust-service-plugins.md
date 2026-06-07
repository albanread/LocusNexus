# Rust Service Plugins — a design for wrapping the world

*How a Rust crate (serde_json, sqlx, quick-xml, image, …) becomes a sealed
Locus service. A **compile-time, platform-team-owned** plugin model: the team
that builds the Locus worker grants capabilities; deployment never can. This is
the mint/seal model ([modules-and-capabilities](../guide/modules-and-capabilities.md))
extended into an organized, repeatable Rust-side contract.*

Status: **design** — proposed, not yet built. It formalizes the wiring services
already use, so adding the next one is a self-contained drop-in instead of four
hand-edits across the worker.

---

## The one idea

A **service** is a capability from the outside world — read a file, parse JSON,
query a database — named by an **effect** and reached only through a **sealed**
interface. Today each one is wired by hand across four places in the worker. A
**plugin** is those four things bundled into one self-contained, registered unit:

| Part | What it is | Where it lives today (ad hoc) |
|---|---|---|
| **Rust shim** | `extern "C"` functions wrapping the crate | `cp_exports.rs` / `locus-rt` |
| **Boundary module** | `at boundary mints (raw)` — declares the externs | `stdlib/*.locus` |
| **Service module** | `at services seals (raw)` — exposes the effect | `stdlib/*.locus` |
| **Effect label** | the capability in the row (`{json}`) | `Label::World(name)` |
| **Symbol table entry** | name → function address for the JIT | `ide_symbol_table()` |
| **Module registration** | the `include_str!` graft list | `WINDOWS_MODULES` |

A plugin packages all of these and registers through **one** descriptor, so the
worker collects symbols + modules automatically and nothing core is edited per
plugin.

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
  `include_str!`'d into the binary today. The app team gets that binary and works
  *above* the seals. To add a capability you need the worker's source and a
  rebuild — a platform-team act, reviewable like any other.
- **The registry is a grant list.** The central `service_plugins()` table is the
  human-auditable record of every capability the worker grants. One file, read
  top-to-bottom, answers "what can programs built with this worker reach?" That
  auditability is a feature; prefer it to decentralized auto-registration.

> A future *sandboxed* dynamic tier (capability-restricted, signed, behind a
> manifest grant) is possible, but it is a separate trust story. v1 is static.

---

## Anatomy of a plugin

A plugin is a **Rust crate** under `plugins/<name>/` containing the shim and its
two co-located `.locus` modules, plus one entry in the central registry.

```
plugins/
  json/
    Cargo.toml            # depends on serde_json + the plugin support crate
    src/lib.rs            # the extern "C" shim + the plugin descriptor
    locus/
      json_boundary.locus # at boundary mints (json_ffi)  — the externs
      json.locus          # at services seals (json_ffi) exposing {json}
  sql/
    …
  xml/
    …
plugins-registry/
  src/lib.rs              # service_plugins() -> &[ServicePlugin]  (the grant list)
```

### 1. The boundary module — mints the raw effect

Layer-0. Declares the `extern "C"` symbols the shim provides and **mints** a raw,
per-plugin effect (`json_ffi`) so every extern carries it. Resolves only inside
the worker (host-injected symbols), exactly like `IdeGraphics` over `iGui.*`.

```locus
module JsonFfi at boundary mints (json_ffi) exposing () =
  -- parse UTF-8 bytes → a host-owned document; returns an opaque handle (>0) or 0
  let json_parse  = extern "json.Parse"  : Ptr -> Int -> Int in
  let json_free   = extern "json.Free"   : Int -> Unit in
  -- navigate: field/index return child handles; 0 = absent
  let json_field  = extern "json.Field"  : Int -> Ptr -> Int -> Int in
  let json_index  = extern "json.Index"  : Int -> Int -> Int in
  -- project scalars out of a leaf handle
  let json_as_int = extern "json.AsInt"  : Int -> Int in
  let json_kind   = extern "json.Kind"   : Int -> Int in   -- 0=null 1=bool 2=num 3=str 4=arr 5=obj
  ()
```

### 2. The service module — seals it, exposes the capability

Layer-1. Wraps the raw handle API behind a tidy, **resource-safe** surface and
**seals** `json_ffi` so the app only ever names `{json}`. The raw FFI label never
escapes — the app cannot reach `json.Parse` directly.

```locus
module Json at services seals (json_ffi) exposing (with_json, field, index, as_int) =
  -- Scoped open: parse, run the body with the doc handle, ALWAYS free — so a
  -- handle can't leak (the lesson of the FFI-buffer leaks). Models `seal`'s
  -- "nothing escapes" as a runtime discipline.
  let with_json = fn bytes: String => fn body: Int -> a =>
    let doc = json_parse (str_ptr bytes) (str_len bytes) in
    let r   = body doc in
    let _   = json_free doc in
    r
  in
  let field  = fn doc: Int => fn name: String => json_field doc (str_ptr name) (str_len name) in
  let index  = fn doc: Int => fn i: Int => json_index doc i in
  let as_int = fn node: Int => json_as_int node in
  ()
```

App code then reads `{json}` in its row — provably confined to JSON, never the
raw FFI:

```locus
with_json input (fn doc =>
  as_int (field (field doc "player") "score"))
-- row: {json}
```

### 3. The Rust shim — the crate, behind the C-ABI

`extern "C"` functions named to match the boundary's `extern` symbols, using the
**handle ABI** (below). The crate's rich values (`serde_json::Value`) live in a
host-side registry; Locus holds only `i64` handles.

```rust
use locus_plugin::{Registry, str_in, ServicePlugin, ModuleSource};

static DOCS: Registry<serde_json::Value> = Registry::new();

#[export_name = "json.Parse"]
pub extern "C" fn json_parse(ptr: i64, len: i64) -> i64 {
    match serde_json::from_slice(str_in(ptr, len)) {
        Ok(v) => DOCS.insert(v),   // → a fresh handle (>0)
        Err(_) => 0,
    }
}

#[export_name = "json.Free"]
pub extern "C" fn json_free(h: i64) { DOCS.remove(h); }

#[export_name = "json.Field"]
pub extern "C" fn json_field(h: i64, ptr: i64, len: i64) -> i64 {
    DOCS.with(h, |v| v.get(str_in(ptr, len)).cloned().map_or(0, |c| DOCS.insert(c)))
}
// json.Index / json.AsInt / json.Kind … likewise

pub fn plugin() -> ServicePlugin {
    ServicePlugin {
        effect:   "json",
        boundary: ModuleSource(0, "json_ffi", include_str!("../locus/json_boundary.locus")),
        service:  ModuleSource(1, "json",     include_str!("../locus/json.locus")),
        symbols:  || vec![
            ("json.Parse", json_parse as usize),
            ("json.Free",  json_free  as usize),
            ("json.Field", json_field as usize),
            // …
        ],
    }
}
```

### 4. Registration — one line in the grant list

```rust
// plugins-registry/src/lib.rs
pub fn service_plugins() -> Vec<ServicePlugin> {
    vec![
        json::plugin(),
        sql::plugin(),
        xml::plugin(),
    ]
}
```

The worker consumes this once:
- **Modules:** each plugin's `boundary` + `service` are appended to the stdlib
  graft list (`WINDOWS_MODULES` becomes `WINDOWS_MODULES ++ plugin modules`), so
  the name-fixpoint auto-includes a plugin only when a program uses it.
- **Symbols (JIT):** `ide_symbol_table()` is extended with every plugin's
  `symbols()`, so `extern "json.Parse"` resolves in the in-process JIT.
- **Symbols (AOT/`locusc build`):** the plugin crates are linked into the worker,
  so the `#[export_name]` symbols co-resolve at link — no table needed.
- **Effect label:** `{json}` is a `Label::World` and needs no central edit.

Adding a plugin touches **its own crate and one registry line** — never
`cp_exports`, `stdlib.rs`, or the core compiler.

---

## The handle ABI — how rich values cross

The C-ABI carries `i64` / `f64` / `Ptr` only. The conventions:

- **Scalars** (`Int`/`Float`/`Bool`) cross inline.
- **Strings** cross as `(Ptr, Int)` = a UTF-8 view. The support crate provides
  `str_in(ptr, len) -> &[u8]` (read a Locus string) and a string-return helper
  (intern bytes back into a Locus `String` via the runtime). Plugins never
  hand-roll UTF-8 marshalling — that path is centralized (and was the source of
  the FFI-buffer leaks now fixed).
- **Rich values** (`serde_json::Value`, a `PgConnection`, a parsed XML tree) are
  **host-owned**, stored in a per-plugin `Registry<T>` (a slab behind a `Mutex`),
  and represented in Locus as an **opaque `i64` handle**. Every navigation/op
  takes the handle. This is the Canvas/window/agent pattern, formalized.
- **Bulk data** (a file's bytes, an image's pixels) use the **shared-buffer**
  pattern: the host owns a buffer, Locus gets its base address + length and reads
  it with `peek` (the `{mem}` effect — the cost model stays visible), or a handle
  + a copy-out call. One crossing, not one-per-byte.
- **Errors**: an op returns a handle, with `0` reserved for "absent/failed"; a
  fuller convention returns a tagged `(status, handle)` the **service** seals into
  a Locus `Result[E]` / `exn`. Raw error codes never reach the app.

**GC-blindness (the `RN-E0405` rule).** A boundary signature may name only
GC-blind types — scalars, `Ptr`, `Unit`, `String`. Handles are `i64`, so they are
GC-blind by construction. A plugin never takes a raw `Array`/sum/record/`Tuple`
(a movable managed datum) across the boundary; bulk Locus data goes via the
shared-buffer pattern (or pinning, when it lands).

---

## Resource discipline is part of the contract

Handles are host resources; a leaked handle is a leaked `serde_json::Value` or an
open DB connection. The model makes lifetime the **service's** job, not the app's:

- **Scope by default.** Prefer `with_<thing> : Open -> (Handle -> a) -> a` that
  opens, runs the body, and frees on the way out — so the handle cannot escape
  (the runtime mirror of `seal`'s no-escape check). Bare `open`/`close` exist for
  advanced use but are not the headline API.
- **One owner.** The `Registry<T>` is the single owner; `remove` is idempotent and
  double-free-safe.
- **Effect honesty.** The service's effect (`{json}`, `{sql}`) is what the row
  shows. A plugin that allocates on the GC heap also carries `{gc}`; one that
  pokes a shared buffer carries `{mem}` — the cost is in the type, as always.

---

## The support crate (`locus-plugin`)

A small platform-owned crate that removes the boilerplate so a plugin is mostly
"wrap the crate":

- `Registry<T>` — the handle slab (insert → handle, `with(h, f)`, `remove`).
- `str_in` / string-return — the centralized string marshalling.
- `ServicePlugin` + `ModuleSource` — the descriptor types.
- `#[locus_export]` (optional) — a thin attribute over `#[export_name]` that also
  records the `(name, address)` pair, so `symbols()` can be derived rather than
  hand-listed.

---

## How it composes with the existing model

- **Mint/seal is unchanged.** The boundary `mints` the raw FFI effect; the service
  `seals` it; the app trades in the abstract `{json}`. The mint-gate still blocks
  app-land `extern` (`RN-E0402`); plugin boundary modules are trusted because they
  are baked into the worker — same TCB argument as the stdlib boundary modules.
- **The cost model stays visible.** `{json}` / `{sql}` in a row tells you what code
  reaches, readable from the signature — the point of the effect system.
- **Auto-inclusion still applies.** A plugin's modules graft only when a program
  names them, so the JSON crate adds nothing to a program that never parses JSON.

---

## Open decisions (for the team)

1. **Symbol derivation** — hand-listed `symbols()` (explicit, auditable) vs an
   `#[locus_export]`-collected table (less boilerplate). Recommend explicit for
   v1; revisit if the list grows.
2. **Error convention** — a single `(status, handle)` standard vs per-plugin. A
   shared `Result`-encoding the support crate seals is cleaner; pin it before the
   second plugin so they don't diverge.
3. **Async/blocking** — DB and network crates are async. The worker JIT call is
   synchronous; a plugin either blocks on a runtime it owns (simple, a thread per
   call) or exposes a poll/future handle. Decide per-category; start blocking.
4. **Capability granularity** — one effect per plugin (`{json}`) vs grouped
   (`{data}`). One-per-plugin keeps the row honest; recommend it.
5. **Pinning for bulk Locus data** — until GC pinning lands, Locus→plugin bulk
   data is copy-or-shared-buffer; revisit when pin hooks exist.

---

## Worked end-to-end: the JSON plugin

The four artifacts above (`json_boundary.locus`, `json.locus`, `src/lib.rs`, the
registry line) are the *entire* plugin. A program:

```locus
-- parse an agent's reply and pull a field; row reads {json}
with_json reply (fn doc =>
  let cmd = field doc "command" in
  ...)
```

…builds with no change to the compiler, links the JSON symbols statically, and
carries `{json}` in its manifest. Swapping `serde_json` for `simd-json` is a
shim-internal change — the boundary, the service, and every program are
untouched. That is the payoff: **the world's Rust crates, organized behind sealed
Locus effects, added one self-contained plugin at a time, entirely under the
worker team's control.**
