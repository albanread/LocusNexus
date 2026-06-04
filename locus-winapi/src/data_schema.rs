// Postcard-serializable, language-agnostic Win32 ABI schema for the embedded
// metadata blob. `include!`'d by BOTH `build.rs` (writes the blob from the
// SQLite source DB) and `lib.rs` (decodes it at first access), so it declares
// no `mod` of its own and uses only outer doc comments.
//
// Wire-format stability: NONE. The blob is rebuilt from the source DB on every
// `cargo build`; downstream consumers go through the public `locus_winapi`
// query API, never the encoded layout. The model is deliberately neutral — it
// records ABI *facts*, and the Locus-type crosswalk (TypeRef → Width/Type)
// happens at consumption time in the FFI glue, not here.

use serde::{Deserialize, Serialize};

/// One Win32 API function projected from the vendored SQLite DB.
///
/// Flat — no interning — to keep the postcard schema trivial. The ~15k-entry
/// surface's redundant `dll`/`callconv` strings cost nothing after zstd-19.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionInfo {
    /// Exported name, e.g. `"MessageBoxW"`. A/W variants are separate entries.
    pub name: String,
    /// DLL providing the symbol, lower-cased with the `.dll` suffix, e.g.
    /// `"user32.dll"`.
    pub dll: String,
    /// Calling convention, e.g. `"stdcall"` — interpreted by the FFI lowering.
    pub callconv: String,
    /// Return type (`TypeRef::Void` for `void`).
    pub return_type: TypeRef,
    /// Parameters in source order.
    pub params: Vec<ParamInfo>,
    /// A/W charset marker: `Some(b'A')`/`Some(b'W')`, or `None` if the
    /// function doesn't participate in the A/W naming convention.
    pub aw_family: Option<u8>,
    /// `SetLastError` semantics — `GetLastError()` carries a code after a
    /// failed call.
    pub set_last_error: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ParamInfo {
    pub name: Option<String>,
    pub type_ref: TypeRef,
    pub direction: Direction,
    pub is_optional: bool,
}

/// Compact, ABI-resolved type reference. Pre-resolving the FFI footguns here
/// (BOOL≠bool, DWORD=u32, LPCWSTR vs char*, HANDLE, pointer-sized) is exactly
/// what lets the compiler materialize correct calls without hand bindings.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TypeRef {
    Void,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    /// 32-bit Windows `BOOL` (a typedef of `i32`, not a 1-byte bool).
    Bool32,
    /// Pointer to `pointee_type_ref`, or opaque if `None`.
    Pointer { pointee_type_ref: Option<Box<TypeRef>> },
    /// Opaque pointer-sized handle (`HANDLE`/`HWND`/`HMODULE`/…).
    Handle,
    /// Narrow-string pointer (`LPSTR`/`LPCSTR`) — distinguished for marshalling.
    NarrowString,
    /// Wide-string pointer (`LPWSTR`/`LPCWSTR`).
    WideString,
    /// Enum with representation `base`.
    Enum { base: Box<TypeRef> },
    /// Typedef of `base`, keeping the Windows name for diagnostics
    /// (e.g. `DWORD` aliasing `U32`).
    Alias { name: String, base: Box<TypeRef> },
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq)]
pub enum Direction {
    In,
    Out,
    InOut,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConstantInfo {
    pub name: String,
    pub value: i64,
    pub source_dll: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WinApiIndex {
    pub functions: Vec<FunctionInfo>,
    pub constants: Vec<ConstantInfo>,
    /// Distinct DLL names present in `functions` (deduped, sorted).
    pub dll_names: Vec<String>,
}
