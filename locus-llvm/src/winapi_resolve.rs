//! The Win32 **oracle resolver** — the FFI glue between the language
//! ([`locus`]) and the ABI metadata ([`locus_winapi`]).
//!
//! A bare `extern "Sym"` (no signature) is filled from the oracle: the symbol's
//! Win32 ABI types are mapped onto Locus's value-world widths (`I32`/`U32`/`Ptr`)
//! and assembled into the FFI arrow. The pass also returns the demanded
//! `{symbol → dll}` map — the AOT linker turns the DLLs into import libs, and the
//! JIT `LoadLibrary`s each DLL + `GetProcAddress`es each symbol (its hand-built
//! import table). Run *before* elaboration; sema then injects `winapi` and
//! computes the boundary ABI.

use std::collections::BTreeMap;

use locus::{Handler, OpClause, Return, Row, Term, Type};
use locus_winapi::{FunctionInfo, TypeRef};

/// `symbol → dll` for every Win32 function a program demands.
pub type Demanded = BTreeMap<String, String>;

/// One Win32 ABI type → a Locus value-world leaf (the width carried at the FFI
/// boundary). 32-bit ints stay `I32`/`U32`; everything pointer-sized (handles,
/// strings, real pointers, and 64-bit ints) becomes `Ptr`.
fn leaf(t: &TypeRef) -> Type {
    match t {
        TypeRef::I8 | TypeRef::I16 | TypeRef::I32 | TypeRef::Bool32 => Type::I32,
        TypeRef::U8 | TypeRef::U16 | TypeRef::U32 => Type::U32,
        TypeRef::I64
        | TypeRef::U64
        | TypeRef::Pointer { .. }
        | TypeRef::Handle
        | TypeRef::NarrowString
        | TypeRef::WideString => Type::Ptr,
        TypeRef::Enum { base } | TypeRef::Alias { base, .. } => leaf(base),
        // A `void` return becomes `Unit` (the i64 left in RAX is ignored).
        TypeRef::Void => Type::Unit,
    }
}

/// CRT math symbols the app reaches via the `math.*` service layer, backed by the
/// UCRT (`ucrtbase.dll`). These are NOT Win32 — the oracle is a winmd projection
/// with no C-runtime symbols — so the resolver records the DLL directly. The
/// `crt.locus` layer-0 module writes their explicit FP signatures; here we only
/// need the symbol → DLL crosswalk (for the JIT's LoadLibrary/GetProcAddress and
/// the AOT import lib). Kept broad/future-proof; an unused entry is harmless.
fn crt_math_dll(sym: &str) -> Option<&'static str> {
    matches!(
        sym,
        "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "sinh"
            | "cosh"
            | "tanh"
            | "exp"
            | "exp2"
            | "log"
            | "log10"
            | "log2"
            | "ceil"
            | "floor"
            | "trunc"
            | "round"
            | "fabs"
            | "fmod"
            | "hypot"
            | "pow"
            | "sqrt"
            | "cbrt"
    )
    .then_some("ucrtbase.dll")
}

/// The Locus FFI arrow for a Win32 function: `p1 -> … -> pN -> ret`, or
/// `Unit -> ret` for a nullary call. Arrows are pure here — sema injects the
/// `winapi` effect on the innermost one.
fn extern_type(f: &FunctionInfo) -> Type {
    let ret = leaf(&f.return_type);
    if f.params.is_empty() {
        return Type::Fun(Box::new(Type::Unit), Box::new(ret), Row::pure());
    }
    f.params.iter().rev().fold(ret, |acc, p| {
        Type::Fun(Box::new(leaf(&p.type_ref)), Box::new(acc), Row::pure())
    })
}

/// Resolve every bare `extern` in `term` against the oracle, returning the filled
/// term plus the demanded `{symbol → dll}` map. An *explicit* extern keeps its
/// written type but still contributes its DLL if the oracle knows the symbol.
pub fn resolve(term: Term) -> Result<(Term, Demanded), String> {
    let mut demanded = Demanded::new();
    let t = walk(term, &mut demanded)?;
    Ok((t, demanded))
}

fn bx(t: Term) -> Box<Term> {
    Box::new(t)
}

fn walk(t: Term, d: &mut Demanded) -> Result<Term, String> {
    use Term::*;
    Ok(match t {
        Extern(sym, None, mint) => {
            if crt_math_dll(&sym).is_some() {
                // A CRT math symbol needs an FP signature the oracle can't supply;
                // `crt.locus` writes one explicitly, so a *bare* CRT extern is a
                // clear error rather than a confusing "unknown Win32 symbol".
                return Err(format!(
                    "`extern \"{sym}\"` is a CRT math symbol — write an explicit \
                     signature (e.g. `: Float -> Float`); the oracle has no CRT types"
                ));
            }
            let f = locus_winapi::find_function_any_dll(&sym)
                .ok_or_else(|| format!("unknown Win32 symbol `{sym}` — not in the oracle"))?;
            d.insert(sym.clone(), f.dll.clone());
            Extern(sym, Some(extern_type(f)), mint)
        }
        Extern(sym, Some(ty), mint) => {
            // CRT math (ucrtbase.dll) takes priority over the Win32 oracle so a
            // name collision can't mis-route; the explicit signature carries the ABI.
            if let Some(dll) = crt_math_dll(&sym) {
                d.insert(sym.clone(), dll.to_string());
            } else if let Some(f) = locus_winapi::find_function_any_dll(&sym) {
                d.insert(sym.clone(), f.dll.clone());
            }
            Extern(sym, Some(ty), mint)
        }
        // A Layer-0 asm symbol is embedded from a `.masm` unit, not resolved from
        // a DLL — pass it through untouched (no oracle lookup, no demanded DLL).
        ExternAsm(sym, ty) => ExternAsm(sym, ty),
        Let(n, e, b) => Let(n, bx(walk(*e, d)?), bx(walk(*b, d)?)),
        LetRec(n, ty, e, b) => LetRec(n, ty, bx(walk(*e, d)?), bx(walk(*b, d)?)),
        Lam(p, ty, b) => Lam(p, ty, bx(walk(*b, d)?)),
        App(f, a) => App(bx(walk(*f, d)?), bx(walk(*a, d)?)),
        Bin(op, a, b) => Bin(op, bx(walk(*a, d)?), bx(walk(*b, d)?)),
        If(c, a, b) => If(bx(walk(*c, d)?), bx(walk(*a, d)?), bx(walk(*b, d)?)),
        Perform(l, a) => Perform(l, bx(walk(*a, d)?)),
        Quote(a) => Quote(bx(walk(*a, d)?)),
        Splice(a) => Splice(bx(walk(*a, d)?)),
        Genlet(a) => Genlet(bx(walk(*a, d)?)),
        Letloc(a) => Letloc(bx(walk(*a, d)?)),
        Handle(e, h) => {
            let Handler { ops, ret } = *h;
            let ops = ops
                .into_iter()
                .map(|c| {
                    Ok::<_, String>(OpClause {
                        body: bx(walk(*c.body, d)?),
                        ..c
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let ret = Return {
                body: bx(walk(*ret.body, d)?),
                ..ret
            };
            Handle(bx(walk(*e, d)?), Box::new(Handler { ops, ret }))
        }
        Effect { name, ops, body } => Effect {
            name,
            ops,
            body: bx(walk(*body, d)?),
        },
        Peek(w, a) => Peek(w, bx(walk(*a, d)?)),
        Poke(w, a, v) => Poke(w, bx(walk(*a, d)?), bx(walk(*v, d)?)),
        Fill(a, b, c) => Fill(bx(walk(*a, d)?), bx(walk(*b, d)?), bx(walk(*c, d)?)),
        Copy(a, b, c) => Copy(bx(walk(*a, d)?), bx(walk(*b, d)?), bx(walk(*c, d)?)),
        Index(a, i) => Index(bx(walk(*a, d)?), bx(walk(*i, d)?)),
        IndexSet(a, i, v) => IndexSet(bx(walk(*a, d)?), bx(walk(*i, d)?), bx(walk(*v, d)?)),
        Tuple(es) => Tuple(
            es.into_iter()
                .map(|e| walk(e, d))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        LetTuple(names, e, body) => LetTuple(names, bx(walk(*e, d)?), bx(walk(*body, d)?)),
        Record(fields) => Record(
            fields
                .into_iter()
                .map(|(n, e)| Ok::<_, String>((n, walk(e, d)?)))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Field(r, name) => Field(bx(walk(*r, d)?), name),
        // Leaves: Var, Int, Bool, Unit, Str.
        leaf => leaf,
    })
}

/// The MSVC import libs the AOT linker needs — one per demanded DLL (deduped).
pub fn import_libs(demanded: &Demanded) -> Vec<String> {
    demanded
        .values()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|d| locus_winapi::import_lib_for_dll(d))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `extern "GetStdHandle"` resolves to a one-argument FFI arrow
    /// (a `U32` parameter, a pointer-sized result) and demands kernel32.
    #[test]
    fn resolves_a_bare_extern() {
        let term = locus::parse(r#"extern "GetStdHandle""#).unwrap();
        let (resolved, demanded) = resolve(term).unwrap();
        assert_eq!(
            demanded.get("GetStdHandle").map(String::as_str),
            Some("kernel32.dll")
        );
        let Term::Extern(_, Some(Type::Fun(dom, cod, _)), _) = resolved else {
            panic!("expected a filled extern arrow, got {resolved:?}");
        };
        assert_eq!(*dom, Type::U32, "STD handle id is a DWORD");
        assert!(matches!(*cod, Type::Ptr), "returns a HANDLE");
    }

    #[test]
    fn an_unknown_symbol_is_an_error() {
        let term = locus::parse(r#"extern "NoSuchWin32Fn""#).unwrap();
        assert!(resolve(term).is_err());
    }

    #[test]
    fn explicit_crt_math_extern_demands_ucrtbase() {
        let term =
            locus::parse(r#"let p = extern "pow" : Float -> Float -> Float in p 2.0 3.0"#).unwrap();
        let (_t, demanded) = resolve(term).unwrap();
        assert_eq!(
            demanded.get("pow").map(String::as_str),
            Some("ucrtbase.dll"),
            "an explicit CRT math extern demands the UCRT, not the Win32 oracle"
        );
    }

    #[test]
    fn a_bare_crt_math_extern_is_rejected() {
        // CRT math needs an FP signature the Win32 oracle cannot supply, so a bare
        // `extern "pow"` is a clear error (crt.locus writes explicit signatures).
        assert!(resolve(locus::parse(r#"extern "pow""#).unwrap()).is_err());
    }
}
