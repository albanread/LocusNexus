//! locus-sqlite — a reference **service plugin**: SQLite (via `rusqlite`, bundled)
//! behind the sealed Locus `{sqlite}` effect. See the design at
//! `locusnexus docs/design/rust-service-plugins.md`.
//!
//! The shim follows the handle ABI: a `Connection` and a materialized result set
//! are host-owned in [`Registry`]s; Locus holds opaque `i64` handles. Strings
//! cross in as Locus-marshalled NUL-terminated UTF-8 pointers (the boundary calls
//! `locus_string_to_cstr`); text columns cross out as Locus `String`s
//! ([`string_out`]). A failed op sets the per-thread error; the service reads it.

use locus_plugin::{cstr_in, set_last_error, string_out, take_last_error, Registry, ServicePlugin};
use rusqlite::{params_from_iter, types::Value, types::ValueRef, Connection};
use std::panic::{catch_unwind, AssertUnwindSafe, UnwindSafe};

/// Open database connections (handle → `Connection`).
static CONNS: Registry<Connection> = Registry::new();

/// Prepared-statement registry. A `rusqlite::Statement` borrows its `Connection`
/// and cannot be stored behind an `i64`, so we model a prepared statement as its
/// owning connection handle + the SQL + accumulated bound params, and recompile
/// via `prepare_cached` at run time (the §6 model).
struct Prepared {
    conn: i64,
    sql: String,
    params: Vec<Value>,
}

static STMTS: Registry<Prepared> = Registry::new();

/// Run `f`, converting any panic into `set_last_error` + the error `sentinel`.
///
/// Panics must never unwind across the C ABI (a hard abort on current Rust — it
/// would take down the worker and every live connection). Every `#[no_mangle]`
/// entry routes its body through here. `name` is used to build the error message.
fn guard<T>(name: &str, sentinel: T, f: impl FnOnce() -> T + UnwindSafe) -> T {
    catch_unwind(f).unwrap_or_else(|_| {
        set_last_error(format!("{name}: panic"));
        sentinel
    })
}

/// Materialize all rows of a `query`'s output into an owned [`ResultSet`], using
/// the same column classification as [`sqlite_query`]. Shared by the constant- and
/// prepared-query paths.
fn materialize(stmt: &mut rusqlite::Statement, params: &[Value]) -> rusqlite::Result<ResultSet> {
    let ncols = stmt.column_count() as i64;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            let mut cells = Vec::with_capacity(ncols as usize);
            for i in 0..ncols as usize {
                cells.push(match row.get_ref(i)? {
                    ValueRef::Null => Cell::Null,
                    ValueRef::Integer(n) => Cell::Int(n),
                    ValueRef::Real(f) => Cell::Real(f),
                    ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
                    // TODO(blob): BLOB columns are stringified as a placeholder.
                    // Reading real bytes needs a bytes ABI (out of scope here).
                    ValueRef::Blob(b) => Cell::Text(format!("<blob {} bytes>", b.len())),
                });
            }
            Ok(cells)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ResultSet { ncols, rows })
}

/// One cell of a materialized result set — owned, so the set outlives the
/// statement that produced it (no `rusqlite` borrow to thread through a handle).
enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

/// A fully-read query result: all rows, eagerly materialized.
struct ResultSet {
    ncols: i64,
    rows: Vec<Vec<Cell>>,
}

static RESULTS: Registry<ResultSet> = Registry::new();

/// `sqlite.Open(path_cstr) -> conn` — open (or create) a database. `path` is a
/// marshalled C string; `":memory:"` for an in-memory db. `0` + last-error on fail.
#[no_mangle]
pub extern "C" fn sqlite_open(path: i64) -> i64 {
    guard("sqlite.Open", 0, AssertUnwindSafe(|| {
        let path = unsafe { cstr_in(path) };
        match Connection::open(path) {
            Ok(c) => CONNS.insert(c),
            Err(e) => {
                set_last_error(format!("sqlite.Open: {e}"));
                0
            }
        }
    }))
}

/// `sqlite.Close(conn)` — close a connection (idempotent).
#[no_mangle]
pub extern "C" fn sqlite_close(conn: i64) {
    guard("sqlite.Close", (), AssertUnwindSafe(|| {
        CONNS.remove(conn);
    }))
}

/// `sqlite.Exec(conn, sql_cstr) -> rows` — run a statement with no result set
/// (DDL/DML). Returns rows-affected, or `-1` + last-error on failure.
#[no_mangle]
pub extern "C" fn sqlite_exec(conn: i64, sql: i64) -> i64 {
    guard("sqlite.Exec", -1, AssertUnwindSafe(|| {
        let sql = unsafe { cstr_in(sql) };
        CONNS
            .with(conn, |c| match c.execute(sql, []) {
                Ok(n) => n as i64,
                Err(e) => {
                    set_last_error(format!("sqlite.Exec: {e}"));
                    -1
                }
            })
            .unwrap_or_else(|| {
                set_last_error("sqlite.Exec: invalid connection handle");
                -1
            })
    }))
}

/// `sqlite.Query(conn, sql_cstr) -> resultset` — run a query and materialize all
/// rows into a host-owned result set; returns its handle (`0` + last-error on fail).
#[no_mangle]
pub extern "C" fn sqlite_query(conn: i64, sql: i64) -> i64 {
    guard("sqlite.Query", 0, AssertUnwindSafe(|| {
        let sql = unsafe { cstr_in(sql) };
        let built = CONNS.with(conn, |c| -> rusqlite::Result<ResultSet> {
            let mut stmt = c.prepare(sql)?;
            materialize(&mut stmt, &[])
        });
        match built {
            Some(Ok(rs)) => RESULTS.insert(rs),
            Some(Err(e)) => {
                set_last_error(format!("sqlite.Query: {e}"));
                0
            }
            None => {
                set_last_error("sqlite.Query: invalid connection handle");
                0
            }
        }
    }))
}

/// `sqlite.Rows(resultset) -> n`
#[no_mangle]
pub extern "C" fn sqlite_rows(rs: i64) -> i64 {
    guard("sqlite.Rows", 0, AssertUnwindSafe(|| {
        RESULTS.with(rs, |r| r.rows.len() as i64).unwrap_or(0)
    }))
}

/// `sqlite.Cols(resultset) -> n`
#[no_mangle]
pub extern "C" fn sqlite_cols(rs: i64) -> i64 {
    guard("sqlite.Cols", 0, AssertUnwindSafe(|| {
        RESULTS.with(rs, |r| r.ncols).unwrap_or(0)
    }))
}

/// `sqlite.Int(resultset, row, col) -> value` — the cell as an integer
/// (`Real` is truncated; non-numeric → 0).
#[no_mangle]
pub extern "C" fn sqlite_int(rs: i64, row: i64, col: i64) -> i64 {
    guard("sqlite.Int", 0, AssertUnwindSafe(|| {
        RESULTS
            .with(rs, |r| {
                match r.rows.get(row as usize).and_then(|c| c.get(col as usize)) {
                    Some(Cell::Int(n)) => *n,
                    Some(Cell::Real(f)) => *f as i64,
                    _ => 0,
                }
            })
            .unwrap_or(0)
    }))
}

/// `sqlite.Text(resultset, row, col) -> String` — the cell as text (numbers
/// stringified; null/missing → empty).
#[no_mangle]
pub extern "C" fn sqlite_text(rs: i64, row: i64, col: i64) -> i64 {
    guard("sqlite.Text", 0, AssertUnwindSafe(|| {
        RESULTS
            .with(rs, |r| {
                let s = match r.rows.get(row as usize).and_then(|c| c.get(col as usize)) {
                    Some(Cell::Text(t)) => t.clone(),
                    Some(Cell::Int(n)) => n.to_string(),
                    Some(Cell::Real(f)) => f.to_string(),
                    _ => String::new(),
                };
                string_out(s.as_bytes())
            })
            .unwrap_or_else(|| string_out(b""))
    }))
}

/// `sqlite.Free(resultset)` — release a result set (idempotent).
#[no_mangle]
pub extern "C" fn sqlite_free(rs: i64) {
    guard("sqlite.Free", (), AssertUnwindSafe(|| {
        RESULTS.remove(rs);
    }))
}

/// `sqlite.LastError() -> String` — take the last error message (empty if none).
#[no_mangle]
pub extern "C" fn sqlite_last_error() -> i64 {
    guard("sqlite.LastError", 0, AssertUnwindSafe(|| {
        string_out(take_last_error().unwrap_or_default().as_bytes())
    }))
}

// ── new entry points ───────────────────────────────────────────────────────────

/// `sqlite.OpenFile(path_cstr) -> conn` — open (or create) a database *file*
/// (identical to [`sqlite_open`]). `0` + last-error on fail.
#[no_mangle]
pub extern "C" fn sqlite_open_file(path: i64) -> i64 {
    guard("sqlite.OpenFile", 0, AssertUnwindSafe(|| {
        let path = unsafe { cstr_in(path) };
        match Connection::open(path) {
            Ok(c) => CONNS.insert(c),
            Err(e) => {
                set_last_error(format!("sqlite.OpenFile: {e}"));
                0
            }
        }
    }))
}

/// `sqlite.OpenMemory() -> conn` — open an in-memory `:memory:` database and
/// install an authorizer that **denies `ATTACH` and file-touching `PRAGMA`s**, so
/// a `SqliteMem` program cannot reach the filesystem through SQL text (the §6
/// sandbox, enforced at the engine). `0` + last-error on fail.
#[no_mangle]
pub extern "C" fn sqlite_open_memory() -> i64 {
    guard("sqlite.OpenMemory", 0, AssertUnwindSafe(|| {
        use rusqlite::hooks::{AuthAction, Authorization};
        let conn = match Connection::open(":memory:") {
            Ok(c) => c,
            Err(e) => {
                set_last_error(format!("sqlite.OpenMemory: {e}"));
                return 0;
            }
        };
        // Deny ATTACH outright; deny PRAGMAs that can name a file on disk
        // (the others — e.g. cache_size — stay allowed). Everything else: Allow.
        conn.authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| match ctx.action {
            AuthAction::Attach { .. } => Authorization::Deny,
            AuthAction::Pragma { pragma_name, .. }
                if matches!(
                    pragma_name.to_ascii_lowercase().as_str(),
                    "temp_store_directory"
                        | "journal_mode"
                        | "wal_checkpoint"
                        | "database_list"
                        | "module_list"
                ) =>
            {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }));
        CONNS.insert(conn)
    }))
}

/// `sqlite.Prepare(conn, sql_cstr) -> stmt` — compile-check the SQL against the
/// connection (`prepare_cached`) and, on success, store a [`Prepared`] (conn + sql
/// + empty params) returning its handle. `0` + last-error on bad handle/SQL.
#[no_mangle]
pub extern "C" fn sqlite_prepare(conn: i64, sql: i64) -> i64 {
    guard("sqlite.Prepare", 0, AssertUnwindSafe(|| {
        let sql = unsafe { cstr_in(sql) }.to_owned();
        let ok = CONNS.with(conn, |c| match c.prepare_cached(&sql) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("sqlite.Prepare: {e}")),
        });
        match ok {
            Some(Ok(())) => STMTS.insert(Prepared { conn, sql, params: vec![] }),
            Some(Err(msg)) => {
                set_last_error(msg);
                0
            }
            None => {
                set_last_error("sqlite.Prepare: invalid connection handle");
                0
            }
        }
    }))
}

/// `sqlite.BindInt(stmt, val)` — push an integer onto the statement's params.
#[no_mangle]
pub extern "C" fn sqlite_bind_int(stmt: i64, val: i64) {
    guard("sqlite.BindInt", (), AssertUnwindSafe(|| {
        STMTS.with_mut(stmt, |p| p.params.push(Value::Integer(val)));
    }))
}

/// `sqlite.BindText(stmt, text_cstr)` — push a text value onto the params.
#[no_mangle]
pub extern "C" fn sqlite_bind_text(stmt: i64, text: i64) {
    guard("sqlite.BindText", (), AssertUnwindSafe(|| {
        let s = unsafe { cstr_in(text) }.to_owned();
        STMTS.with_mut(stmt, |p| p.params.push(Value::Text(s)));
    }))
}

// TODO(blob): a `sqlite_bind_blob(stmt, ptr, len)` pushing `Value::Blob(Vec<u8>)`
// goes here once the bytes ABI lands (out of scope — needs a non-cstr ABI).

/// `sqlite.BindNull(stmt)` — push a SQL NULL onto the params.
#[no_mangle]
pub extern "C" fn sqlite_bind_null(stmt: i64) {
    guard("sqlite.BindNull", (), AssertUnwindSafe(|| {
        STMTS.with_mut(stmt, |p| p.params.push(Value::Null));
    }))
}

/// `sqlite.StmtQuery(stmt) -> resultset` — recompile the statement on its owning
/// connection (`prepare_cached`), run it with the bound params, and materialize
/// all rows into a host-owned result set. `0` + last-error on any failure.
#[no_mangle]
pub extern "C" fn sqlite_stmt_query(stmt: i64) -> i64 {
    guard("sqlite.StmtQuery", 0, AssertUnwindSafe(|| {
        // Clone what we need out of the registry so we don't hold the STMTS lock
        // while taking the CONNS lock.
        let prep = STMTS.with(stmt, |p| (p.conn, p.sql.clone(), p.params.clone()));
        let (conn, sql, params) = match prep {
            Some(t) => t,
            None => {
                set_last_error("sqlite.StmtQuery: invalid statement handle");
                return 0;
            }
        };
        let built = CONNS.with(conn, |c| -> rusqlite::Result<ResultSet> {
            let mut stmt = c.prepare_cached(&sql)?;
            materialize(&mut stmt, &params)
        });
        match built {
            Some(Ok(rs)) => RESULTS.insert(rs),
            Some(Err(e)) => {
                set_last_error(format!("sqlite.StmtQuery: {e}"));
                0
            }
            None => {
                set_last_error("sqlite.StmtQuery: invalid connection handle (closed?)");
                0
            }
        }
    }))
}

/// `sqlite.StmtExec(stmt) -> rows` — recompile + execute the statement (DML) with
/// the bound params, returning rows-affected. `-1` + last-error on any failure.
#[no_mangle]
pub extern "C" fn sqlite_stmt_exec(stmt: i64) -> i64 {
    guard("sqlite.StmtExec", -1, AssertUnwindSafe(|| {
        let prep = STMTS.with(stmt, |p| (p.conn, p.sql.clone(), p.params.clone()));
        let (conn, sql, params) = match prep {
            Some(t) => t,
            None => {
                set_last_error("sqlite.StmtExec: invalid statement handle");
                return -1;
            }
        };
        let done = CONNS.with(conn, |c| -> rusqlite::Result<i64> {
            let mut stmt = c.prepare_cached(&sql)?;
            Ok(stmt.execute(params_from_iter(params.iter()))? as i64)
        });
        match done {
            Some(Ok(n)) => n,
            Some(Err(e)) => {
                set_last_error(format!("sqlite.StmtExec: {e}"));
                -1
            }
            None => {
                set_last_error("sqlite.StmtExec: invalid connection handle (closed?)");
                -1
            }
        }
    }))
}

/// `sqlite.StmtReset(stmt)` — clear the bound params so the statement can be
/// re-bound and re-run (the prepared-loop pattern).
#[no_mangle]
pub extern "C" fn sqlite_stmt_reset(stmt: i64) {
    guard("sqlite.StmtReset", (), AssertUnwindSafe(|| {
        STMTS.with_mut(stmt, |p| p.params.clear());
    }))
}

/// `sqlite.Finalize(stmt)` — release a prepared statement (idempotent).
#[no_mangle]
pub extern "C" fn sqlite_finalize(stmt: i64) {
    guard("sqlite.Finalize", (), AssertUnwindSafe(|| {
        STMTS.remove(stmt);
    }))
}

/// `sqlite.IsNull(resultset, row, col) -> 1|0` — `1` iff the cell is SQL `NULL`;
/// `0` otherwise (including out-of-range, which is treated as not-null). Lets a
/// caller distinguish a true `NULL` from a real `0`/`""` (which `sqlite.Int`/`Text`
/// cannot).
#[no_mangle]
pub extern "C" fn sqlite_is_null(rs: i64, row: i64, col: i64) -> i64 {
    guard("sqlite.IsNull", 0, AssertUnwindSafe(|| {
        RESULTS
            .with(rs, |r| {
                match r.rows.get(row as usize).and_then(|c| c.get(col as usize)) {
                    Some(Cell::Null) => 1,
                    _ => 0,
                }
            })
            .unwrap_or(0)
    }))
}

// ── credential vault (the §4 model: ConnMeta public, Secret confined) ──────────

use std::collections::HashMap;
use std::sync::Mutex;

/// The **public** half of a resolved credential — everything safe to read back.
/// `backend` is `0` = memory, `1` = file. `path` is the connection path
/// (`":memory:"` for memory, the file path for file). `fields` holds any *other*
/// non-secret string parameters, read by [`meta_str`]. (§4: "readable: backend,
/// path, and other non-secret fields".)
struct ConnMeta {
    backend: u8,
    path: String,
    fields: HashMap<String, String>,
}

/// The **confined** half of a resolved credential. Deliberately a newtype with
/// **no read accessor exported to Locus** — there is no `secret_str`. A `Secret`
/// exists only to be held and (in a future `<backend>_open(meta, secret)`) handed
/// to a driver's native connect; it can be *moved*, never *read* as a value. This
/// is the structural basis of "secrets never reach the app" (§4, §7.4).
#[allow(dead_code)] // the inner String is consumed only by a future driver connect
struct Secret(String);

/// Resolved public metadata (handle → `ConnMeta`).
static META: Registry<ConnMeta> = Registry::new();

/// Resolved confined secrets (handle → `Secret`). Parked here to demonstrate the
/// split; the handle is intentionally NOT returned to Locus in this version.
static SECRET: Registry<Secret> = Registry::new();

/// The vault's profile store: profile name → its raw JSON blob. A stand-in for a
/// real OS secret store (Win cred mgr / libsecret); populated by [`vault_register`].
/// Same poisoning-tolerant lock pattern the `Registry` uses.
static PROFILES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Reject blobs larger than this at parse — a hostile/oversized credential blob
/// must not be able to make us allocate without bound (§10 / fix K).
const MAX_BLOB: usize = 64 * 1024;

/// `vault.Register(name_cstr, json_cstr) -> 1|0` — provision the vault: store
/// `name -> json`. Validates that `json` parses as a JSON **object**; `0` +
/// last-error otherwise. (A real vault would read the OS store instead.)
#[no_mangle]
pub extern "C" fn vault_register(name: i64, json: i64) -> i64 {
    guard("vault.Register", 0, AssertUnwindSafe(|| {
        let name = unsafe { cstr_in(name) }.to_owned();
        let json = unsafe { cstr_in(json) }.to_owned();
        if json.len() > MAX_BLOB {
            set_last_error(format!(
                "vault.Register: credential blob too large ({} bytes, max {MAX_BLOB})",
                json.len()
            ));
            return 0;
        }
        match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(v) if v.is_object() => {}
            Ok(_) => {
                set_last_error("vault.Register: credential JSON is not an object");
                return 0;
            }
            Err(e) => {
                set_last_error(format!("vault.Register: invalid credential JSON: {e}"));
                return 0;
            }
        }
        let mut g = PROFILES.lock().unwrap_or_else(|p| p.into_inner());
        g.get_or_insert_with(HashMap::new).insert(name, json);
        1
    }))
}

/// `cred.Resolve(name_cstr) -> meta` — resolve a profile to a public [`ConnMeta`]
/// handle, parking any secret in [`SECRET`] (its handle is *not* returned). `0` +
/// last-error if the profile is absent, the blob is unparseable/oversized, the
/// backend is unknown, or a required `path` is missing. (§4: resolve splits the
/// dictionary; `Db` reads only the public tag.)
#[no_mangle]
pub extern "C" fn cred_resolve(name: i64) -> i64 {
    guard("cred.Resolve", 0, AssertUnwindSafe(|| {
        let name = unsafe { cstr_in(name) };
        let json = {
            let g = PROFILES.lock().unwrap_or_else(|p| p.into_inner());
            match g.as_ref().and_then(|m| m.get(name)) {
                Some(j) => j.clone(),
                None => {
                    set_last_error(format!("cred.Resolve: unknown profile '{name}'"));
                    return 0;
                }
            }
        };
        if json.len() > MAX_BLOB {
            set_last_error(format!(
                "cred.Resolve: credential blob too large ({} bytes, max {MAX_BLOB})",
                json.len()
            ));
            return 0;
        }
        let obj = match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => {
                set_last_error("cred.Resolve: credential JSON is not an object");
                return 0;
            }
            Err(e) => {
                set_last_error(format!("cred.Resolve: invalid credential JSON: {e}"));
                return 0;
            }
        };

        // backend (required, "memory" | "file").
        let backend = match obj.get("backend").and_then(|v| v.as_str()) {
            Some("memory") => 0u8,
            Some("file") => 1u8,
            _ => {
                set_last_error("cred.Resolve: unknown backend");
                return 0;
            }
        };

        // path: default ":memory:" for memory; required for file.
        let path = match obj.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_owned(),
            None if backend == 0 => ":memory:".to_owned(),
            None => {
                set_last_error("cred.Resolve: 'file' backend requires a 'path'");
                return 0;
            }
        };

        // Every OTHER string field except the secret keys → public fields.
        // The secret (password|secret) is confined into a Secret, never a field.
        let mut fields = HashMap::new();
        let mut secret: Option<String> = None;
        for (k, v) in &obj {
            match k.as_str() {
                "backend" | "path" => {}
                "password" | "secret" => {
                    if let Some(s) = v.as_str() {
                        secret = Some(s.to_owned());
                    }
                }
                _ => {
                    if let Some(s) = v.as_str() {
                        fields.insert(k.clone(), s.to_owned());
                    }
                }
            }
        }

        let meta = META.insert(ConnMeta { backend, path, fields });
        if let Some(s) = secret {
            // Parked to demonstrate the split; handle intentionally not returned.
            let _ = SECRET.insert(Secret(s));
        }
        meta
    }))
}

/// `meta.Backend(meta) -> 0|1` — the public backend code; `-1` + last-error on a
/// bad handle.
#[no_mangle]
pub extern "C" fn meta_backend(meta: i64) -> i64 {
    guard("meta.Backend", -1, AssertUnwindSafe(|| {
        META.with(meta, |m| m.backend as i64).unwrap_or_else(|| {
            set_last_error("meta.Backend: invalid meta handle");
            -1
        })
    }))
}

/// `meta.Path(meta) -> String` — the connection path, marshalled out; empty string
/// on a bad handle.
#[no_mangle]
pub extern "C" fn meta_path(meta: i64) -> i64 {
    guard("meta.Path", 0, AssertUnwindSafe(|| {
        META.with(meta, |m| string_out(m.path.as_bytes()))
            .unwrap_or_else(|| string_out(b""))
    }))
}

/// `meta.Str(meta, key_cstr) -> String` — look up an arbitrary **non-secret**
/// field by key, marshalled out; empty string if absent or on a bad handle. This
/// is the public-field reader; there is deliberately **no secret equivalent**.
#[no_mangle]
pub extern "C" fn meta_str(meta: i64, key: i64) -> i64 {
    guard("meta.Str", 0, AssertUnwindSafe(|| {
        let key = unsafe { cstr_in(key) };
        META.with(meta, |m| {
            string_out(m.fields.get(key).map(|s| s.as_bytes()).unwrap_or(b""))
        })
        .unwrap_or_else(|| string_out(b""))
    }))
}

/// The plugin descriptor — collected by the worker's grant list.
pub fn plugin() -> ServicePlugin {
    ServicePlugin {
        effects: &["sqlite", "sqlite_fs", "cred_access"],
        modules: vec![
            (0u8, "sqlite_ffi", include_str!("../locus/sqlite_boundary.locus")),
            (0u8, "sqlite_disk", include_str!("../locus/sqlite_disk.locus")),
            (0u8, "vault_access", include_str!("../locus/vault_boundary.locus")),
            // `db` (the generic interface) is grafted INNER of `sqlite` (the
            // backend service it calls), so `Sqlite`'s `sql_*` bindings are in
            // scope for `Database`. Within a layer the graft nests earlier-listed
            // modules inner, so `db` is listed before `sqlite`.
            (1u8, "db", include_str!("../locus/db.locus")),
            (1u8, "sqlite", include_str!("../locus/sqlite.locus")),
        ],
        symbols: vec![
            ("sqlite.Open", sqlite_open as *const () as u64),
            ("sqlite.Close", sqlite_close as *const () as u64),
            ("sqlite.Exec", sqlite_exec as *const () as u64),
            ("sqlite.Query", sqlite_query as *const () as u64),
            ("sqlite.Rows", sqlite_rows as *const () as u64),
            ("sqlite.Cols", sqlite_cols as *const () as u64),
            ("sqlite.Int", sqlite_int as *const () as u64),
            ("sqlite.Text", sqlite_text as *const () as u64),
            ("sqlite.Free", sqlite_free as *const () as u64),
            ("sqlite.LastError", sqlite_last_error as *const () as u64),
            ("sqlite.OpenFile", sqlite_open_file as *const () as u64),
            ("sqlite.OpenMemory", sqlite_open_memory as *const () as u64),
            ("sqlite.Prepare", sqlite_prepare as *const () as u64),
            ("sqlite.BindInt", sqlite_bind_int as *const () as u64),
            ("sqlite.BindText", sqlite_bind_text as *const () as u64),
            ("sqlite.BindNull", sqlite_bind_null as *const () as u64),
            ("sqlite.StmtQuery", sqlite_stmt_query as *const () as u64),
            ("sqlite.StmtExec", sqlite_stmt_exec as *const () as u64),
            ("sqlite.StmtReset", sqlite_stmt_reset as *const () as u64),
            ("sqlite.Finalize", sqlite_finalize as *const () as u64),
            ("sqlite.IsNull", sqlite_is_null as *const () as u64),
            ("vault.Register", vault_register as *const () as u64),
            ("cred.Resolve", cred_resolve as *const () as u64),
            ("meta.Backend", meta_backend as *const () as u64),
            ("meta.Path", meta_path as *const () as u64),
            ("meta.Str", meta_str as *const () as u64),
        ],
    }
}
