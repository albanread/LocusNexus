# Locus — Database Access (layered design)

*The layered database-access design. Companion to
[`../capabilities.md`](../capabilities.md) (the layer/seal model) and the
service-plugin design [`rust-service-plugins.md`](rust-service-plugins.md) (how a
Rust crate becomes a sealed Locus effect). This document specializes both to one
domain: talking to databases.*

> **The one-paragraph idea.** Database access is a *stack of named effects*. At
> the bottom, a Rust crate (`rusqlite`, a MySQL driver, …) does the real work as
> a **plugin in the boundary layer**, minting a raw `*_access` effect. Above it, a
> thin **backend service** per capability (in-memory SQLite, on-disk SQLite,
> MySQL, …) seals that raw power and exposes a safe, resource-disciplined surface.
> Above *those*, a single generic **`Db`** service gives the app one backend-neutral
> vocabulary — and, because `Db` also sits above **`Credentials`**, it resolves
> secure connection strings itself, so a secret never reaches application code.
> The app names *what* it wants (a backend, a credential profile, a parameterized
> query); the type system proves *exactly* which powers that touches.

---

## 1. The layering

```
┌─────────────────────────────────────────────────────────────────────────┐
│  app                          (layer 2)   talks ONLY to Db                │
│    let c   = db_open "local.notes" in      -- a credential PROFILE name    │
│    let st  = db_prepare c "SELECT body FROM notes WHERE id = ?1" in        │
│    let _   = db_bind_int st 42 in                                          │
│    let rs  = db_run_query st in …                                          │
└───────────────────────────────┬───────────────────────────────────────────┘
                                 │  effect row proves what it touched
                                 ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Db                           (layer 1)   generic relational interface     │
│    • backend-neutral vocabulary: open / prepare / bind / run / fetch /close│
│    • dispatch is STATIC on the Conn's TYPE — Conn[b]; ops effect-polymorphic│
│    • reads only the PUBLIC backend tag from Credentials; never the secret  │
└───────────┬───────────────────────────────────────────┬───────────────────┘
            │                                             │
            ▼                                             ▼
┌─────────────────────────────────┐     ┌─────────────────────────────────────┐
│  Credentials      (layer 1)      │     │  backend services   (layer 1)        │
│   profile → ConnMeta (public) +  │     │   SqliteMem  SqliteFs  MySql  MsSql  │
│   Secret (NO read accessor;      │     │   each SEALS one *_access effect;    │
│   flows vault→driver only)       │     │   one service == one capability      │
└───────────┬─────────────────────┘     └───────────────────┬───────────────────┘
            │                                                │
            ▼                                                ▼
┌─────────────────────────────────┐     ┌─────────────────────────────────────┐
│  vault_access     (layer 0)      │     │  sqlite_access  mysql_access  …      │
│   OS secret store + serde_json   │     │   (layer 0 — the PLUGINS)            │
│   (Win cred mgr / libsecret) FFI │     │   the Rust crate doing the real work │
└─────────────────────────────────┘     └─────────────────────────────────────┘
        the boundary layer: plugins that mint raw `*_access` effects
        (every #[no_mangle] entry is catch_unwind-guarded — see §10)
```

Read it top-down as *delegation* and bottom-up as *trust*. Each arrow is a call
into the layer below; each layer is a transparent wrapper that seals the power
beneath it so the layer above cannot utter its name
([`capabilities.md`](../capabilities.md): "seal makes the name private").

---

## 2. Layer 0 — the access-effect plugins (the boundary)

**Plugins live in the boundary layer.** A plugin is a Rust crate that does the
real database work and is presented to Locus as a sealed service-plugin
([`rust-service-plugins.md`](rust-service-plugins.md)): a `#[no_mangle]` shim + a
`boundary.locus` that `mints` one **`*_access`** effect + the symbol/grant wiring.

| plugin          | Rust crate        | mints effect    | does the real work                       |
|-----------------|-------------------|-----------------|------------------------------------------|
| `sqlite_access` | `rusqlite`        | `sqlite_access` | open, exec, prepare, bind, step, fetch   |
| `mysql_access`  | `mysql`/`sqlx`    | `mysql_access`  | (future)                                 |
| `mssql_access`  | `tiberius`        | `mssql_access`  | (future)                                 |
| `vault_access`  | OS store + `serde_json` | `vault_access` | fetch a profile's secret JSON blob from a vault, parse it into a credential dictionary |

The access layer is **the only place FFI happens** and **the only place a Locus
`String` is marshalled to a C string** (then freed on the same line). Rich values
(connections, prepared statements, result sets) are **host-owned in a
`Registry<T>`**; Locus holds opaque `i64` handles (GC-blind). This is the handle
ABI from the plugin design — unchanged.

The access surface is *raw and unsafe-to-expose*: it would let any caller open an
arbitrary file, interpolate a string into SQL, or leak a connection. That is why
it is sealed: app code can never name `sqlite_access` directly.

---

## 3. Layer 1a — backend services (one service == one capability)

A backend service `seals` exactly one `*_access` effect and exposes the *safe*
surface for that capability. **Least privilege is by service: you cannot use a
power whose service you did not import, and you cannot utter the raw effect it
sealed.** Splitting one Rust crate into several services is deliberate:

| service     | seals           | carries effect           | exposes (the capability)                  | cannot do                |
|-------------|-----------------|--------------------------|-------------------------------------------|--------------------------|
| `SqliteMem` | `sqlite_access` | `{ sqlite_access }`      | `sqlitemem_open ()` → in-memory `:memory:`| **touch the filesystem** |
| `SqliteFs`  | `sqlite_access` | `{ sqlite_access, sqlite_fs }` | `sqlitefs_open path` → a database file | —                        |
| `MySql`     | `mysql_access`  | `{ mysql_access }`       | `mysql_open spec` → a server connection   | read local files         |

`SqliteMem` and `SqliteFs` are the headline example: they share one Rust crate
and one `sqlite_access` plugin, but are **two services**. A program that imports
only `SqliteMem` has no function in scope that opens a file — the in-memory
database is provably sandboxed.

> **Effect-row distinction (decision Q2 = distinct effect for disk).** `seals` is
> a *capability grant*, so `sqlite_access` propagates to both. To make the
> **manifest itself prove "this program reaches the filesystem"**, the file-open
> path also carries a distinct **`sqlite_fs`** effect: `SqliteMem` surfaces
> `{ sqlite_access }`, `SqliteFs` surfaces `{ sqlite_access, sqlite_fs }`.
>
> **Where `sqlite_fs` is minted.** Minting is `boundary`-only
> (`RN-E0402`); a *service* cannot mint. So `sqlite_fs` is minted **in the
> `sqlite_access` plugin boundary**, not in the `SqliteFs` service: the boundary
> exposes two opens — `mem_open` carrying `{ sqlite_access }` and `file_open`
> carrying `{ sqlite_access, sqlite_fs }` — and each backend service seals the one
> it needs. The capability split is real because the *boundary* draws it; the
> services just consume the right door.

A backend service also owns **resource discipline**: the idiomatic surface is
scope-based (`with_db` / `with_query` run a body and *always* release the handle —
the runtime mirror of `seal`'s no-escape). Those combinators type-check today
(`Conn -> a ! {|e}`) — see the working note in
[`rust-service-plugins.md`](rust-service-plugins.md).

---

## 4. Layer 1b — `Credentials` (a parameter dictionary, secrets confined)

`Db` sits **above** a `Credentials` service so the app can connect to a protected
database *without ever holding the secret*. App code names a **profile**
(`"prod.analytics"`); everything else is resolved inside the seal.

> **A connection is invoked by name alone.** Because the credential holds *every*
> connection parameter, naming the profile is the whole act of connecting:
> `db_open "prod.analytics"` needs no host, no port, no secret in app code. The
> name is a capability; the dictionary behind it does the rest.

**A credential is a dictionary, not a single secret.** Real connections need many
parameters — `backend`, `host`, `port`, `dbname`, `user`, `password`, `certpath`,
`sslmode`, … — and the set differs per backend. So a credential is a flexible
**key→value map**, sourced from a secret vault where it is typically stored as
**JSON**:

```json
{ "backend": "mysql", "host": "db.internal", "port": 3306,
  "dbname": "analytics", "user": "svc_ro", "password": "…", "sslmode": "require" }
```

**Resolving splits the dictionary into two typed values.** A
single opaque `Cred` with string-keyed accessors was a mistake: if any layer can
call `cred_str cred "host"`, it can call `cred_str cred "password"`, so "secrets
never reach the app" reduces to *every layer choosing not to read it* — discipline,
not structure. Instead `vault_access` partitions the parsed dictionary at the seal:

```
  app:   db_open "prod.analytics"               -- just a profile name
  Db:    (meta, secret) = cred_resolve "…"      -- vault fetch + JSON parse → two handles
         kind = meta_backend meta               -- read ONLY the public backend tag → dispatch
         conn = <kind>.open meta secret         -- driver consumes `secret` in its connect call
         return conn                            -- Conn carries `meta` for display; `secret` is gone
```

- **`ConnMeta` — public, readable.** The non-secret params (`backend`, `host`,
  `port`, `dbname`, `user`, `sslmode`). Has read accessors (`meta_str`,
  `meta_int`, `meta_backend`); freely passed around and echoable as `Conn`
  metadata for display.
- **`Secret` — opaque, with *no read accessor at all*.** The `password`, key
  material, `certpath` *contents*. There is **no `secret_str`** in the language;
  the only thing that consumes a `Secret` is a driver's `*_open meta secret` call,
  which hands it straight to the native connect inside the boundary. A `Secret`
  can be *moved*, never *read* — so "the password never becomes a readable value"
  is a type-level fact, not a convention. (Which JSON keys are secret is decided
  by `vault_access` from a per-backend manifest, not by the caller.)
- **Dispatch reads only the public tag.** `Db` calls
  `meta_backend` — a public field — to choose the driver. It never holds a
  `Secret`-reading capability because none exists; the secret-reading TCB shrinks
  to the driver's connect path. Dispatch and credentials remain "the same decision
  from two sides", but `Db` sees only the side that is safe to see.

- **`vault_access` parses the JSON** (`serde_json`) host-side into the two
  partitions. Locus gets two `i64` handles; no key or value is a Locus `String`.
  Malformed/oversized/hostile blobs are bounded and rejected at parse — see §10.

> **Vaults & JSON.** Because vault blobs are JSON, the credential layer shares
> machinery with a general `json_access` plugin (parse JSON → a value the host
> owns). v1 parses inside the `vault_access` shim with `serde_json`; if/when a
> standalone JSON service lands, `Credentials` becomes a thin consumer of it.

---

## 5. Layer 1c — `Db` (the generic relational interface)

`Db` is the one module the app imports for *operations*. It is **backend-neutral**,
and the organizing principle is:

> **The connection directs the flow — at the type level.** The app
> opens a *connection* and the connection routes every operation. The subtlety:
> if `Conn` is a bare `i64` and `db_exec` dispatches on a *runtime*
> tag, then `db_exec`'s effect row is one fixed static thing — it must either be
> the *union* of all backends (so a SQLite-only program falsely carries
> `mysql_access` + `sqlite_fs`) or an abstract `{db}` (which *erases* the very
> `sqlite_fs` distinction §3 built). Runtime dispatch on a value is exactly the
> ambient behavior the calculus forbids. **So the connection carries its backend in
> its *type*, not in a runtime integer.**

`Conn[b]` is parameterized by a backend `b`. The opens return *backend-specific*
types — `sqlitemem_open : … -> Conn[SqliteMem]`, `file_open : … -> Conn[SqliteFs]`,
`mysql_open : … -> Conn[MySql]` — and the generic ops are **effect-polymorphic over
the backend**:

```
  db_exec : Conn[b] -> Sql -> Rows ! eff(b) | e
```

where `eff(b)` is the backend's effect (`{sqlite_access}` for `SqliteMem`,
`{sqlite_access, sqlite_fs}` for `SqliteFs`, `{mysql_access}` for `MySql`). The app
writes one generic `db_exec`, dispatch is **static** (resolved from the connection's
type, the trait/row-polymorphism we already have — D6 single-param traits + `{|e}`
rows), and the effect row a program ends up with is **exactly** the union of the
backends it actually opened — no more, no less. "The connection directs the flow"
is preserved, lifted from values to types: *the type of the connection you hold
determines both which driver runs and which effect you incur.* (Decision Q1.)

The vocabulary (v1) — every op is `Conn[b]`-polymorphic unless noted:

| function                          | meaning                                                   |
|-----------------------------------|-----------------------------------------------------------|
| `db_open profile`                 | name a credential → it supplies *all* params → `Conn[b]`  |
| `db_open_memory ()`               | shortcut: in-memory SQLite db → `Conn[SqliteMem]` (no creds)|
| `db_exec conn sql`                | run a statement with no result set (DDL/DML) → rows       |
| `db_query conn sql`               | run a constant query (`sql : Sql`, literal-only) → `ResultSet`|
| `db_prepare conn sql`             | compile a parameterized statement → `Stmt[b]`             |
| `db_bind_int stmt v` / `_text` / `_blob` / `_null` | bind the next `?n` placeholder    |
| `db_run_query stmt`               | execute the prepared stmt → `ResultSet`                   |
| `db_run_exec stmt`                | execute the prepared stmt (DML) → rows                    |
| `db_reset stmt`                   | clear bindings to re-run with new values                  |
| `db_finalize stmt`                | release the prepared statement                            |
| `with_transaction conn (fn => …)` | commit on normal exit, **rollback on early exit** |
| `db_rows rs` / `db_cols rs`       | shape of a result set                                     |
| `db_is_null rs r c`               | distinguish SQL `NULL` from a real `0`/`""`               |
| `db_get_int rs r c` / `_text` / `_blob` | read a cell (no silent `Real`→`Int` coercion)       |
| `db_free rs` / `db_close conn`    | release                                                   |
| `db_error ()`                     | last error message (redacted — never echoes bound values) |

**The security spine: values only ever cross via bind, never via string-building.**
The moment a runtime value is involved the app must `db_prepare` + `db_bind_*`.
This is enforced by a **type, not a comment**: `db_query`/`db_exec`
take `sql : Sql`, and a `Sql` is constructible *only from a string literal* (a
compile-time-checked newtype). A runtime `String` — and therefore
`concat "… " name` — **cannot reach** `db_query`; the unsafe path is not merely
discouraged, it does not type-check. See §6.

**The headline surface is scope-based (decision Q3 = scope combinators).** The
front door is `with_db` / `with_query`, which open, run a body, and **always**
release the handle — the runtime mirror of `seal`'s no-escape. The flat
`db_open … db_close` ops above are the *primitives* the combinators are built
from; apps reach for the combinators so a handle can't leak on an early return:

```
with_db "local.notes" (fn conn =>          -- conn released no matter how body exits
  with_query conn "SELECT count(*) FROM notes" (fn rs =>   -- rs freed on exit
    db_get_int rs 0 0))
```

These type-check today (`Conn -> a ! {|e}`, the open-effect-row form — see
[`rust-service-plugins.md`](rust-service-plugins.md)).

---

## 6. Parameterized & prepared statements (injection safety, first-class)

A secure language should make the safe thing the *default* thing. Two related
mechanisms, both built on the access layer's `prepare`/`bind`/`step`:

- **Parameterized** — a value is bound to a `?n` placeholder, never interpolated:
  ```
  let st = db_prepare conn "SELECT body FROM notes WHERE author = ?1 AND year > ?2" in
  let _  = db_bind_text st author in     -- ?1   (author is untrusted input)
  let _  = db_bind_int  st 2020   in     -- ?2
  let rs = db_run_query st in …
  ```
  `author` is sent to the engine as a *bound value*, so `'; DROP TABLE notes; --`
  is just a string that matches no author. **Value injection is prevented by
  construction**: the SQL text and the data travel on different rails, and
  the `Sql` text rail only accepts string *literals*, so a runtime value cannot
  reach it without going through `bind`.

  **Honest scope.** This stops *value* injection (WHERE/VALUES
  positions). It does **not** make all injection "impossible": SQL identifiers —
  table/column names, `PRAGMA` args, `ATTACH DATABASE '<path>'`, `ORDER BY`
  direction — are not bindable placeholders in any engine. For those, the design
  provides a vetted quoting primitive (`db_ident`) and claims only
  **value-injection-safe by default**, not blanket immunity.

- **Prepared (reusable)** — the same compiled statement, re-bound and re-run:
  ```
  let ins = db_prepare conn "INSERT INTO notes (author, body) VALUES (?1, ?2)" in
  loop i = 0 while i < n do
    let _ = db_reset ins in
    let _ = db_bind_text ins (author_of i) in
    let _ = db_bind_text ins (body_of   i) in
    let _ = db_run_exec ins in
    i + 1
  else i
  ```
  Compiled once, executed many times — the standard performance + safety win.

**`ATTACH`/`PRAGMA` and the mem sandbox.** `db_exec` accepts
arbitrary literal SQL, and `ATTACH DATABASE '/etc/…'` / file-touching `PRAGMA`s
would let a `SqliteMem`-only program reach the filesystem *through SQL text*,
defeating §3. The `SqliteMem` connection therefore installs a rusqlite
`set_authorizer` that **denies `ATTACH` and file `PRAGMA`s** — the in-memory
sandbox is enforced at the engine, not just by which `open` you could name.

**Implementation note — what "prepared" really means.** A
`rusqlite::Statement` borrows its `Connection`, so we cannot store one behind an
`i64`. The plugin models a `Stmt` as `(conn_handle, conn_generation, sql,
Vec<Value>)` and uses `Connection::prepare_cached(sql)` at run time. Honest
caveat: `prepare_cached` is an **LRU keyed on the SQL string** (capacity set
explicitly, e.g. 32). Within the cache window it is genuinely compiled-once; a
statement evicted by `>capacity` distinct interleaved statements is re-parsed on
next use. So "prepared" means *compiled-and-cached-when-resident*, not a
permanently-held compiled object. (A held-`Statement` handle via `ouroboros` is
the upgrade path if the re-parse ever matters.) The `conn_generation` makes a
`Stmt` outliving its `Conn` **fail closed** — see §10.

---

## 7. Security properties (what the design buys — and what it doesn't)

Each property is tagged with *how* it is enforced, so the guarantee is precise.

1. **Effect transparency — structural.** Because dispatch is type-directed (§5),
   a program's row is *exactly* the union of the backends it opened:
   `{ sqlite_access }` (in-memory sandbox), `{ sqlite_access, sqlite_fs }` (touches
   disk), `{ vault_access, mysql_access }` (a remote server behind a secret). No
   over-reporting, no erasure. Auditable from the signature.
2. **Least privilege by capability — structural (naming) + engine-enforced.** You
   cannot reach a power whose service you did not import, nor utter the raw
   `*_access` it sealed. The in-memory sandbox is *additionally* enforced by the
   `set_authorizer` block on `ATTACH`/file-`PRAGMA` (§6) — not by naming alone.
3. **Value-injection-safe by default — structural.** `db_query`/`db_exec` take
   `Sql` (literal-only), so runtime values can reach the engine *only* via `bind`.
   Identifier injection is *not* covered (§6) — `db_ident` quoting is provided, the
   claim is scoped to value positions.
4. **Secret confinement — structural.** A `Secret` has no read accessor in the
   language (§4); the password can be *moved* into a driver's connect call but
   never *read* as a value, by `Db`, a backend, or the app. The secret-reading TCB
   is the driver connect path only.
5. **Resource discipline — combinator-enforced, with a caveat.** `with_db` /
   `with_query` / `with_transaction` release deterministically (`seal`'s no-escape
   at runtime). The flat `open/close` primitives remain available *underneath*, so
   a program that deliberately uses them can still leak a GC-blind handle; the
   discipline is guaranteed only for code that stays on the combinator surface.

**Threat-model dependence.** Properties 1–4 hold against *application code* in the
worker. Whether they hold against *untrusted code co-resident in the same worker
process* is a separate question, answered in §10.

---

## 8. Decisions

- **Q1 — Backend dispatch → the connection directs the flow, *at the type level*.**
  Opens return backend-specific `Conn[b]`; generic ops are effect-polymorphic
  (`db_exec : Conn[b] -> Sql -> Rows ! eff(b) | e`). Dispatch is **static** (from
  the connection's type), so the effect row is exactly the backends opened — not a
  runtime `i64` tag, which would force over-reporting or erasure. *(§5.)*
- **Q2 — fs vs mem in the row → distinct effect for disk.** `SqliteMem` surfaces
  `{ sqlite_access }`; `SqliteFs` additionally mints **`sqlite_fs`**, so the
  manifest proves filesystem access. *(§3.)*
- **Q3 — Headline surface → scope combinators.** `with_db` / `with_query`
  (auto-release) are the front door; flat `open/close` are the primitives. *(§5/§6.)*

- **Q4 — Credential shape → a parameter dictionary, split into `ConnMeta` +
  `Secret`.** A credential is a flexible key→value map sourced from a secret vault
  as JSON; naming the profile supplies *all* params, so `db_open "name"` is the
  complete act of connecting. `vault_access` parses it into a **public `ConnMeta`**
  (readable: backend/host/port/…) and an **opaque `Secret`** with *no read
  accessor* — moved into a driver's connect call, never read as a value. `Db`
  reads only the public `backend` tag to dispatch. *(§4.)*

### Design decisions
- **B** — type-directed dispatch (not runtime `i64`) so effect transparency holds.
- **C/J** — `Secret` has no read accessor; `Db` is credential-blind beyond the tag.
- **D** — `Sql` literal-only type; `set_authorizer` blocks `ATTACH`/file-`PRAGMA`.
- **A** — `sqlite_fs` minted at the **boundary** (mint is boundary-only), not a service.
- **E** — threat model stated; cross-tenant isolation via separate processes (v1).
- **H** — `catch_unwind` mandatory at every shim entry (incl. the existing plugin).
- **I** — `with_transaction`, `db_is_null`, BLOB path, no silent coercion.

---

## 9. SQLite reference: what exists, what to build

**Built and verified:**
- The `rusqlite` plugin, hardened: every shim entry `catch_unwind`-guarded;
  prepared/parameterized statements; NULL fidelity; the in-memory sandbox
  (authorizer denies `ATTACH`/file-PRAGMA). *(steps 0–1)*
- `SqliteMem` / `SqliteFs` split with the distinct `sqlite_fs` effect minted at a
  dedicated on-disk boundary. *(step 2)*
- The generic **`Database`** service: phantom-typed `Conn[b]`, capability-honest
  rows (`db_open_memory → {sqlite}`, `db_open_file → {sqlite, sqlite_fs}`),
  backend-mix rejected by the type checker, generic `db_*` ops. *(step 3)*
- The **`vault_access`** credential layer: connect by profile name
  (`db_open_profile`), credential = JSON parameter dictionary split into a public
  `ConnMeta` and an unreadable `Secret`; verified that a password never reaches
  app scope. Runtime-dispatched connections are `Conn[Dyn]` with the honest
  worst-case row `{cred_access, sqlite, sqlite_fs}`. *(step 4)*
- Demos: `examples/{sqlite_demo, sqlite_prepared, db_layer, db_credentials}.locus`.

**Future refinements:** a `Credentials` *service* that seals `cred_access`
and restricts provisioning (today the vault boundary is exposed directly); BLOB
support (needs a bytes ABI); `with_db`/`with_query`/`with_transaction` scope
combinators; the cross-DBMS generic op (awaits a backend-generic effect, i.e.
associated effects). The original flat `sql_*` surface also remains.

**To build for this design:**
0. **Harden the existing shim first:** wrap every `#[no_mangle]` entry in
   `catch_unwind` → `set_last_error` + sentinel; add `db_is_null`, blob
   bind/get, and non-coercing readers.
1. Extend the `sqlite_access` shim with `prepare`/`bind_*`/`run_*`/`reset`/
   `finalize`; model a `Stmt` as `(conn, generation, sql, Vec<Value>)` with
   `prepare_cached`. Boundary exposes `mem_open` (`{sqlite_access}`)
   and `file_open` (`{sqlite_access, sqlite_fs}`) — `sqlite_fs` minted here.
2. `SqliteMem` (`:memory:`, with the `ATTACH`/`PRAGMA` authorizer) and
   `SqliteFs` (file path) services; opens typed `Conn[SqliteMem]` / `Conn[SqliteFs]`.
3. `Db` layer (§5 vocabulary) with **type-directed** effect-polymorphic ops over
   `Conn[b]`; the `Sql` literal-only newtype + `db_ident`;
   `with_db`/`with_query`/`with_transaction` combinators (resource-discipline).
4. `vault_access` (fetch a profile blob; bounded `serde_json` parse) →
   **`ConnMeta` (readable) + `Secret` (no accessor)**; a `Credentials`
   service exposing `meta_*` only; wire `db_open profile` = resolve →
   `meta_backend` dispatch → `<driver>.open meta secret`.
5. Demos: (a) parameterized query + prepared-loop insert on an in-memory db; (b)
   `db_open "profile"` connecting by name through a credential dictionary — both
   with the effect manifest shown.

---

## 10. Threat model & runtime contract

Several claims are only meaningful relative to a stated
threat model and a hardened FFI contract. Here they are.

### 10.1 Threat model

- **In scope: a single trusted application** compiled by the worker. Against
  *application code*, properties 1–4 (§7) hold structurally. This is the primary
  deployment and what the security claims are about.
- **Out of scope (v1): mutually-untrusted code co-resident in one worker
  process.** The handle space is a shared, monotonic `i64` registry per value
  type, and handles are small integers passed to type-directed ops. A *malicious*
  module that fabricates an integer it was never given could name another
  connection's handle. We do **not** claim in-process isolation between distrusting
  tenants in v1; such tenants get **separate worker processes** (OS-level
  isolation). If in-process isolation becomes a goal, §10.3 is the upgrade.

So "provably sandboxed" means
*against the app*, with cross-tenant isolation delegated to the process boundary —
stated explicitly, not an unstated assumption.

### 10.2 The FFI contract every plugin shim must honor

- **Panics never cross the C ABI.** Unwinding out of an
  `extern "C"` fn is, on current Rust, a hard **abort** (UB on older toolchains) —
  either way it takes down the worker and every connection in it. So **every
  `#[no_mangle]` entry point wraps its body in `std::panic::catch_unwind`**, and a
  caught panic becomes `set_last_error(...)` + the error sentinel (`0` / `-1`).
  This is part of the `rust-service-plugins.md` contract and is honored by the
  SQLite reference, so a `.unwrap()` or OOM inside `query_map` never becomes a
  worker-killer.
- **Handles fail closed, never stale-resolve.** The `Registry`
  counter is **monotonic** — a freed handle id is *never* reissued — so a `Stmt`
  whose `Conn` was closed resolves to *nothing* and returns an error; it can never
  silently run against a *different*, recycled connection (the ABA risk
  does not arise). A `conn_generation` stamped on each `Stmt` makes this
  explicit and survives even if the allocator ever changes.
- **No silent data corruption.** `db_get_blob`/`db_bind_blob` carry binary as
  bytes (no UTF-8 lossy round-trip); `db_is_null` distinguishes `NULL`; cell
  readers do not coerce `Real`→`Int` silently.

### 10.3 If in-process isolation is later required

Per-capability registries (`CONNS_MEM` / `CONNS_FS` / …) plus **unforgeable
handles** — each handle a `(capability_tag, generation, index)` triple, the
generation random-seeded so a guessed integer fails to resolve. This closes
the cross-capability forge and is cheap; it is future work because v1's
threat model puts distrusting tenants in separate processes.
