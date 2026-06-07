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
use rusqlite::{types::ValueRef, Connection};

/// Open database connections (handle → `Connection`).
static CONNS: Registry<Connection> = Registry::new();

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
    let path = unsafe { cstr_in(path) };
    match Connection::open(path) {
        Ok(c) => CONNS.insert(c),
        Err(e) => {
            set_last_error(format!("sqlite.Open: {e}"));
            0
        }
    }
}

/// `sqlite.Close(conn)` — close a connection (idempotent).
#[no_mangle]
pub extern "C" fn sqlite_close(conn: i64) {
    CONNS.remove(conn);
}

/// `sqlite.Exec(conn, sql_cstr) -> rows` — run a statement with no result set
/// (DDL/DML). Returns rows-affected, or `-1` + last-error on failure.
#[no_mangle]
pub extern "C" fn sqlite_exec(conn: i64, sql: i64) -> i64 {
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
}

/// `sqlite.Query(conn, sql_cstr) -> resultset` — run a query and materialize all
/// rows into a host-owned result set; returns its handle (`0` + last-error on fail).
#[no_mangle]
pub extern "C" fn sqlite_query(conn: i64, sql: i64) -> i64 {
    let sql = unsafe { cstr_in(sql) };
    let built = CONNS.with(conn, |c| -> rusqlite::Result<ResultSet> {
        let mut stmt = c.prepare(sql)?;
        let ncols = stmt.column_count() as i64;
        let rows = stmt
            .query_map([], |row| {
                let mut cells = Vec::with_capacity(ncols as usize);
                for i in 0..ncols as usize {
                    cells.push(match row.get_ref(i)? {
                        ValueRef::Null => Cell::Null,
                        ValueRef::Integer(n) => Cell::Int(n),
                        ValueRef::Real(f) => Cell::Real(f),
                        ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
                        ValueRef::Blob(b) => Cell::Text(format!("<blob {} bytes>", b.len())),
                    });
                }
                Ok(cells)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ResultSet { ncols, rows })
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
}

/// `sqlite.Rows(resultset) -> n`
#[no_mangle]
pub extern "C" fn sqlite_rows(rs: i64) -> i64 {
    RESULTS.with(rs, |r| r.rows.len() as i64).unwrap_or(0)
}

/// `sqlite.Cols(resultset) -> n`
#[no_mangle]
pub extern "C" fn sqlite_cols(rs: i64) -> i64 {
    RESULTS.with(rs, |r| r.ncols).unwrap_or(0)
}

/// `sqlite.Int(resultset, row, col) -> value` — the cell as an integer
/// (`Real` is truncated; non-numeric → 0).
#[no_mangle]
pub extern "C" fn sqlite_int(rs: i64, row: i64, col: i64) -> i64 {
    RESULTS
        .with(rs, |r| {
            match r.rows.get(row as usize).and_then(|c| c.get(col as usize)) {
                Some(Cell::Int(n)) => *n,
                Some(Cell::Real(f)) => *f as i64,
                _ => 0,
            }
        })
        .unwrap_or(0)
}

/// `sqlite.Text(resultset, row, col) -> String` — the cell as text (numbers
/// stringified; null/missing → empty).
#[no_mangle]
pub extern "C" fn sqlite_text(rs: i64, row: i64, col: i64) -> i64 {
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
}

/// `sqlite.Free(resultset)` — release a result set (idempotent).
#[no_mangle]
pub extern "C" fn sqlite_free(rs: i64) {
    RESULTS.remove(rs);
}

/// `sqlite.LastError() -> String` — take the last error message (empty if none).
#[no_mangle]
pub extern "C" fn sqlite_last_error() -> i64 {
    string_out(take_last_error().unwrap_or_default().as_bytes())
}

/// The plugin descriptor — collected by the worker's grant list.
pub fn plugin() -> ServicePlugin {
    ServicePlugin {
        effects: &["sqlite"],
        modules: vec![
            (0u8, "sqlite_ffi", include_str!("../locus/sqlite_boundary.locus")),
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
        ],
    }
}
