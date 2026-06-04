//! Build-time pipeline: SQLite (`E:\windows_api\windows_api.db`) ➜ Rust
//! structs ➜ postcard ➜ zstd-19 ➜ `data/winapi_data.bin.zst`.
//!
//! Ported from NewBF's `newbf-winapi/build.rs`. The source DB path is
//! `LOCUS_WINDOWS_API_DB`-overridable and defaults to the shared read-only
//! `E:\windows_api\windows_api.db` (the 28 MB DB is NOT vendored — only the tiny
//! projected blob ships). Constants are deferred (the DB carries enum *types*
//! but not member values).
//!
//! See `src/data_schema.rs` for the wire format. Every parameter + return value
//! must resolve into the [`TypeRef`] enum or the enclosing function is dropped.

use std::env;
use std::fs;
use std::path::PathBuf;

use rusqlite::{params, Connection, OpenFlags};

include!("src/data_schema.rs");

/// Default location of the shared Win32 ABI database.
const DEFAULT_DB: &str = r"E:\windows_api\windows_api.db";

/// Resolve a `reference`-kind type by name (the DB doesn't carry the
/// underlying integer for these typedef rows).
fn resolve_named_reference(name: &str) -> Option<TypeRef> {
    if matches!(name, "BOOL" | "BOOLEAN") {
        return Some(TypeRef::Bool32);
    }
    if matches!(
        name,
        "PSTR" | "LPSTR" | "PCSTR" | "LPCSTR" | "PCHAR" | "LPCH" | "LPCCH"
    ) {
        return Some(TypeRef::NarrowString);
    }
    if matches!(
        name,
        "PWSTR" | "LPWSTR" | "PCWSTR" | "LPCWSTR" | "PWCHAR" | "PCNZWCH"
    ) {
        return Some(TypeRef::WideString);
    }
    let alias = |base| {
        Some(TypeRef::Alias {
            name: name.into(),
            base: Box::new(base),
        })
    };
    match name {
        "DWORD" | "ULONG" | "UINT" | "UINT32" | "ULONG32" | "COLORREF" => alias(TypeRef::U32),
        "LONG" | "INT" | "INT32" | "LONG32" | "HRESULT" | "NTSTATUS" => alias(TypeRef::I32),
        "WORD" | "USHORT" | "UINT16" | "WCHAR" => alias(TypeRef::U16),
        "SHORT" | "INT16" => alias(TypeRef::I16),
        "BYTE" | "UCHAR" | "UINT8" => alias(TypeRef::U8),
        "CHAR" | "INT8" => alias(TypeRef::I8),
        "DWORDLONG" | "ULONGLONG" | "UINT64" | "ULONG64" | "DWORD64" => alias(TypeRef::U64),
        "LONGLONG" | "INT64" | "LONG64" => alias(TypeRef::I64),
        // Pointer-sized integers — 64-bit on Win64.
        "SIZE_T" | "ULONG_PTR" | "DWORD_PTR" | "UINT_PTR" | "WPARAM" => alias(TypeRef::U64),
        "SSIZE_T" | "LONG_PTR" | "INT_PTR" | "LRESULT" | "LPARAM" => alias(TypeRef::I64),
        _ => None,
    }
}

fn resolve_primitive(name: &str) -> Option<TypeRef> {
    Some(match name {
        "void" => TypeRef::Void,
        "bool" => TypeRef::Bool32,
        "u8" | "char" => TypeRef::U8,
        "i8" => TypeRef::I8,
        "u16" => TypeRef::U16,
        "i16" => TypeRef::I16,
        "u32" => TypeRef::U32,
        "i32" => TypeRef::I32,
        "u64" => TypeRef::U64,
        "i64" => TypeRef::I64,
        "usize" | "isize" => TypeRef::U64, // Win64 — pointer-sized
        _ => return None,
    })
}

/// `H`-prefixed all-alphanumeric names are opaque handles (HWND, HICON, …).
fn is_handle_name(name: &str) -> bool {
    name.starts_with('H') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Map a `types` row to a `TypeRef`, or `None` when not ABI-resolvable
/// (struct-by-value, COM interface, …) — the enclosing function is dropped.
fn classify_type(conn: &Connection, type_id: i64, depth: u32) -> Option<TypeRef> {
    if depth > 8 {
        return None;
    }
    let (kind, name, pointee, target): (String, String, Option<i64>, Option<i64>) = {
        let mut stmt = conn
            .prepare_cached(
                "SELECT kind, type_name, pointee_type_id, target_type_id \
                 FROM types WHERE type_id = ?1",
            )
            .ok()?;
        stmt.query_row(params![type_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .ok()?
    };

    match kind.as_str() {
        "primitive" => resolve_primitive(&name),
        "reference" => resolve_named_reference(&name)
            .or_else(|| is_handle_name(&name).then_some(TypeRef::Handle)),
        "pointer" => {
            let pointee_id = pointee?;
            let pointee_ref = classify_pointee(conn, pointee_id, depth + 1).map(Box::new);
            Some(TypeRef::Pointer {
                pointee_type_ref: pointee_ref,
            })
        }
        "enum" => {
            let base = target
                .and_then(|t| classify_type(conn, t, depth + 1))
                .unwrap_or(TypeRef::U32);
            Some(TypeRef::Enum {
                base: Box::new(base),
            })
        }
        // Callable types (WNDPROC, …) + delegates → opaque pointer; the
        // enclosing function (SetWindowsHookExW, …) is accepted.
        "function_pointer" | "delegate" => Some(TypeRef::Pointer {
            pointee_type_ref: None,
        }),
        // The DB inconsistently stores some pointer-sized integer/handle
        // typedefs (LPARAM, HINSTANCE, …) as `struct`. Accept by name.
        "struct" | "union" => resolve_named_reference(&name)
            .or_else(|| is_handle_name(&name).then_some(TypeRef::Handle)),
        _ => None,
    }
}

/// Pointer-to-struct collapses to an opaque pointer (`None`); the enclosing
/// function stays accepted. Pointer-to-primitive surfaces the primitive.
fn classify_pointee(conn: &Connection, type_id: i64, depth: u32) -> Option<TypeRef> {
    if depth > 8 {
        return None;
    }
    let kind: String = {
        let mut stmt = conn
            .prepare_cached("SELECT kind FROM types WHERE type_id = ?1")
            .ok()?;
        stmt.query_row(params![type_id], |r| r.get(0)).ok()?
    };
    match kind.as_str() {
        "struct" | "union" | "interface" | "delegate" | "apis-container" => None,
        _ => classify_type(conn, type_id, depth),
    }
}

fn classify_param_dir(s: Option<&str>) -> Direction {
    match s.map(|x| x.to_ascii_lowercase()).as_deref() {
        Some("in") => Direction::In,
        Some("out") => Direction::Out,
        Some("inout") => Direction::InOut,
        _ => Direction::Unknown,
    }
}

fn project_functions(conn: &Connection) -> rusqlite::Result<Vec<FunctionInfo>> {
    let mut fn_stmt = conn.prepare(
        "SELECT function_id, function_name, dll_name, callconv, return_type_id, \
                is_variadic, aw_family, set_last_error \
         FROM functions \
         WHERE dll_name IS NOT NULL \
         ORDER BY dll_name, function_name",
    )?;
    let mut param_stmt = conn.prepare_cached(
        "SELECT ordinal, param_name, type_id, direction, is_optional \
         FROM function_params WHERE function_id = ?1 ORDER BY ordinal",
    )?;

    let rows = fn_stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;

    let mut out = Vec::new();
    let (mut skipped_variadic, mut skipped_no_return, mut skipped_bad_type) =
        (0usize, 0usize, 0usize);
    for row in rows {
        let (fid, name, dll, callconv, ret_id, is_variadic, aw_family, sle) = row?;
        if is_variadic != 0 {
            skipped_variadic += 1;
            continue;
        }
        let Some(dll) = dll else { continue };
        let Some(ret_id) = ret_id else {
            skipped_no_return += 1;
            continue;
        };
        let Some(ret_ty) = classify_type(conn, ret_id, 0) else {
            skipped_bad_type += 1;
            continue;
        };

        let mut params: Vec<ParamInfo> = Vec::new();
        let mut bad = false;
        let mut p_iter = param_stmt.query(params![fid])?;
        while let Some(prow) = p_iter.next()? {
            let pname: Option<String> = prow.get(1)?;
            let ptype_id: Option<i64> = prow.get(2)?;
            let pdir: Option<String> = prow.get(3)?;
            let popt: i64 = prow.get(4)?;
            let (Some(ptype_id), false) = (ptype_id, bad) else {
                bad = true;
                break;
            };
            let Some(ptype_ref) = classify_type(conn, ptype_id, 0) else {
                bad = true;
                break;
            };
            params.push(ParamInfo {
                name: pname,
                type_ref: ptype_ref,
                direction: classify_param_dir(pdir.as_deref()),
                is_optional: popt != 0,
            });
        }
        drop(p_iter);
        if bad {
            skipped_bad_type += 1;
            continue;
        }

        let aw = aw_family
            .as_deref()
            .and_then(|s| s.bytes().next())
            .filter(|b| *b == b'A' || *b == b'W');

        out.push(FunctionInfo {
            name,
            dll: dll.to_ascii_lowercase(),
            callconv: callconv.unwrap_or_else(|| "stdcall".into()),
            return_type: ret_ty,
            params,
            aw_family: aw,
            set_last_error: sle != 0,
        });
    }

    println!(
        "cargo:warning=locus-winapi: projected {} functions (skipped variadic={skipped_variadic} \
         no_return={skipped_no_return} bad_type={skipped_bad_type})",
        out.len(),
    );
    Ok(out)
}

fn main() {
    let db_path = env::var("LOCUS_WINDOWS_API_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DB));

    // The committed Win32 snapshot lives in the crate, so a clean clone builds
    // without the external DB. `include_bytes!(env!("WINAPI_DATA_BIN"))` in
    // lib.rs reads exactly this file; we only *refresh* it from the DB when one
    // is available (a maintainer action), and otherwise build from it as-is.
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let snapshot = manifest.join("data").join("winapi_data.bin.zst");

    println!("cargo:rerun-if-env-changed=LOCUS_WINDOWS_API_DB");
    println!("cargo:rerun-if-changed={}", db_path.display());
    println!("cargo:rerun-if-changed=src/data_schema.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=WINAPI_DATA_BIN={}", snapshot.display());

    match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => {
            let functions = project_functions(&conn).expect("project functions");
            // Constants deferred: the DB carries enum *types* but not member values.
            let constants: Vec<ConstantInfo> = Vec::new();
            let mut dll_names: Vec<String> = functions.iter().map(|f| f.dll.clone()).collect();
            dll_names.sort();
            dll_names.dedup();

            let idx = WinApiIndex {
                functions,
                constants,
                dll_names,
            };
            let bytes = postcard::to_allocvec(&idx).expect("postcard serialise");
            let compressed = zstd::stream::encode_all(&*bytes, 19).expect("zstd encode");

            // Write only when the content actually changed, so an unchanged DB
            // doesn't churn the file mtime (and force needless recompiles).
            let changed = fs::read(&snapshot)
                .map(|old| old != compressed)
                .unwrap_or(true);
            if changed {
                if let Some(dir) = snapshot.parent() {
                    fs::create_dir_all(dir).expect("create data dir");
                }
                fs::write(&snapshot, &compressed).expect("write snapshot");
            }
            println!(
                "cargo:warning=locus-winapi: snapshot {} — postcard {} bytes ➜ zstd-19 {} bytes; \
                 {} functions, {} dlls",
                if changed { "refreshed" } else { "unchanged" },
                bytes.len(),
                compressed.len(),
                idx.functions.len(),
                idx.dll_names.len(),
            );
        }
        Err(e) => {
            // No DB (e.g. a clean clone): the committed snapshot must exist.
            assert!(
                snapshot.exists(),
                "locus-winapi: no Win32 ABI DB at {} ({e}) and no committed snapshot at {}. \
                 Set LOCUS_WINDOWS_API_DB to a windows_api.db to generate one.",
                db_path.display(),
                snapshot.display(),
            );
            println!(
                "cargo:warning=locus-winapi: using committed Win32 snapshot (no DB at {}).",
                db_path.display()
            );
        }
    }
}
