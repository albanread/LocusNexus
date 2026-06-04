//! Linux libc/libm symbol resolver for the Locus sidecar.
//!
//! This is deliberately small: Locus's current `crt.locus` math layer already
//! writes explicit floating-point signatures, so Linux only needs a symbol to
//! shared-object map plus a `dlopen`/`dlsym` bridge for the JIT.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};

use locus::{Handler, OpClause, Return, Row, Term, Type};

/// `symbol -> shared object` for every Linux C runtime symbol a program demands.
pub type Demanded = BTreeMap<String, String>;

const LIBC: &str = "libc.so.6";
const LIBM: &str = "libm.so.6";

/// Linux shared object that exports `sym`, for the portable C boundary Locus
/// currently knows about. Unknown explicit externs are left alone so future
/// user-owned boundary modules can still link through the process search path.
pub fn library_for_symbol(sym: &str) -> Option<&'static str> {
    if is_math_symbol(sym) {
        Some(LIBM)
    } else if is_libc_symbol(sym) {
        Some(LIBC)
    } else {
        None
    }
}

fn is_math_symbol(sym: &str) -> bool {
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
}

fn is_libc_symbol(sym: &str) -> bool {
    matches!(
        sym,
        "write"
            | "read"
            | "malloc"
            | "free"
            | "memcpy"
            | "memmove"
            | "memset"
            | "strlen"
            | "puts"
            | "putchar"
            | "printf"
    )
}

fn arrow(params: Vec<Type>, ret: Type) -> Type {
    if params.is_empty() {
        return Type::Fun(Box::new(Type::Unit), Box::new(ret), Row::pure());
    }
    params.into_iter().rev().fold(ret, |acc, p| {
        Type::Fun(Box::new(p), Box::new(acc), Row::pure())
    })
}

fn unary_float() -> Type {
    arrow(vec![Type::Float], Type::Float)
}

fn binary_float() -> Type {
    arrow(vec![Type::Float, Type::Float], Type::Float)
}

fn known_extern_type(sym: &str) -> Result<Option<Type>, String> {
    let t = match sym {
        "read" | "write" => arrow(vec![Type::I32, Type::Ptr, Type::Int], Type::Int),
        "malloc" => arrow(vec![Type::Int], Type::Ptr),
        "free" => arrow(vec![Type::Ptr], Type::Unit),
        "memcpy" | "memmove" => arrow(vec![Type::Ptr, Type::Ptr, Type::Int], Type::Ptr),
        "memset" => arrow(vec![Type::Ptr, Type::I32, Type::Int], Type::Ptr),
        "strlen" => arrow(vec![Type::Ptr], Type::Int),
        "puts" => arrow(vec![Type::Ptr], Type::I32),
        "putchar" => arrow(vec![Type::I32], Type::I32),
        "printf" => {
            return Err(
                "`extern \"printf\"` is variadic; Linux bare-extern resolution only supports \
                 fixed-signature C calls. Write an explicit fixed wrapper instead."
                    .into(),
            )
        }
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "exp"
        | "exp2" | "log" | "log10" | "log2" | "ceil" | "floor" | "trunc" | "round" | "fabs"
        | "sqrt" | "cbrt" => unary_float(),
        "atan2" | "fmod" | "hypot" | "pow" => binary_float(),
        _ => return Ok(None),
    };
    Ok(Some(t))
}

/// Resolve every known Linux C-runtime extern in `term`, returning the type-filled
/// term plus the demanded shared objects. Bare known libc/libm symbols are filled
/// from the seed ABI table here; explicit externs keep their written signature
/// but still contribute their shared-object demand.
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
            if let Some(ty) = known_extern_type(&sym)? {
                let lib = library_for_symbol(&sym)
                    .expect("every known Linux extern type has a shared object");
                d.insert(sym.clone(), lib.to_string());
                Extern(sym, Some(ty), mint)
            } else {
                return Err(format!(
                    "unknown Linux C-runtime symbol `{sym}` — not in the libc/libm seed oracle"
                ));
            }
        }
        Extern(sym, Some(ty), mint) => {
            if let Some(lib) = library_for_symbol(&sym) {
                d.insert(sym.clone(), lib.to_string());
            }
            Extern(sym, Some(ty), mint)
        }
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
        leaf => leaf,
    })
}

/// Resolve demanded libc/libm symbols to absolute addresses for ORC.
pub fn resolve_absolute_symbols(demanded: &Demanded) -> Result<Vec<(String, u64)>, String> {
    let mut handles = BTreeMap::<String, *mut libc::c_void>::new();
    let mut out = Vec::with_capacity(demanded.len());
    for (sym, lib) in demanded {
        let handle = match handles.get(lib) {
            Some(&h) => h,
            None => {
                let h = dlopen_library(lib)?;
                handles.insert(lib.clone(), h);
                h
            }
        };
        let addr = dlsym_symbol(handle, sym, lib)?;
        out.push((sym.clone(), addr as usize as u64));
    }
    Ok(out)
}

fn dlopen_library(lib: &str) -> Result<*mut libc::c_void, String> {
    let c_lib = CString::new(lib).map_err(|_| format!("bad shared-object name `{lib}`"))?;
    let handle = unsafe { libc::dlopen(c_lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        Err(format!("dlopen({lib}) failed: {}", dlerror()))
    } else {
        Ok(handle)
    }
}

fn dlsym_symbol(
    handle: *mut libc::c_void,
    sym: &str,
    lib: &str,
) -> Result<*mut libc::c_void, String> {
    let c_sym = CString::new(sym).map_err(|_| format!("bad symbol name `{sym}`"))?;
    unsafe {
        libc::dlerror();
        let addr = libc::dlsym(handle, c_sym.as_ptr());
        if addr.is_null() {
            Err(format!("dlsym({sym} in {lib}) failed: {}", dlerror()))
        } else {
            Ok(addr)
        }
    }
}

fn dlerror() -> String {
    unsafe {
        let err = libc::dlerror();
        if err.is_null() {
            "unknown dynamic-loader error".to_string()
        } else {
            CStr::from_ptr(err).to_string_lossy().into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_symbols_live_in_libm() {
        assert_eq!(library_for_symbol("pow"), Some("libm.so.6"));
        assert_eq!(library_for_symbol("sin"), Some("libm.so.6"));
    }

    #[test]
    fn c_runtime_symbols_live_in_libc() {
        assert_eq!(library_for_symbol("malloc"), Some("libc.so.6"));
        assert_eq!(library_for_symbol("write"), Some("libc.so.6"));
    }

    #[test]
    fn explicit_math_extern_demands_libm() {
        let term = locus::parse(r#"extern "pow" : Float -> Float -> Float"#).unwrap();
        let (_resolved, demanded) = resolve(term).unwrap();
        assert_eq!(demanded.get("pow").map(String::as_str), Some("libm.so.6"));
    }

    #[test]
    fn bare_libc_extern_is_filled_and_demands_libc() {
        let term = locus::parse(r#"extern "write""#).unwrap();
        let (resolved, demanded) = resolve(term).unwrap();
        assert_eq!(demanded.get("write").map(String::as_str), Some("libc.so.6"));
        let Term::Extern(_, Some(Type::Fun(fd, rest, _)), _) = resolved else {
            panic!("expected filled write extern, got {resolved:?}");
        };
        assert_eq!(*fd, Type::I32);
        let Type::Fun(buf, rest, _) = *rest else {
            panic!("write second arg missing");
        };
        assert_eq!(*buf, Type::Ptr);
        let Type::Fun(count, ret, _) = *rest else {
            panic!("write third arg missing");
        };
        assert_eq!(*count, Type::Int);
        assert_eq!(*ret, Type::Int);
    }

    #[test]
    fn bare_libm_extern_is_filled_and_demands_libm() {
        let term = locus::parse(r#"extern "pow""#).unwrap();
        let (resolved, demanded) = resolve(term).unwrap();
        assert_eq!(demanded.get("pow").map(String::as_str), Some("libm.so.6"));
        let Term::Extern(_, Some(Type::Fun(a, rest, _)), _) = resolved else {
            panic!("expected filled pow extern, got {resolved:?}");
        };
        assert_eq!(*a, Type::Float);
        let Type::Fun(b, ret, _) = *rest else {
            panic!("pow second arg missing");
        };
        assert_eq!(*b, Type::Float);
        assert_eq!(*ret, Type::Float);
    }

    #[test]
    fn bare_variadic_extern_is_rejected() {
        let err = resolve(locus::parse(r#"extern "printf""#).unwrap()).unwrap_err();
        assert!(err.contains("variadic"), "{err}");
    }

    #[test]
    fn unknown_bare_extern_is_rejected_by_the_linux_oracle() {
        let err = resolve(locus::parse(r#"extern "definitely_not_in_libc""#).unwrap()).unwrap_err();
        assert!(err.contains("unknown Linux C-runtime symbol"), "{err}");
    }
}
