//! The Locus standard library — **in-language** definitions (console IO, numeric
//! helpers, …) that the compiler grafts into a program before elaboration. The
//! "runtime" is therefore readable Locus, not a hidden native blob.
//!
//! # Where it lives — LAYERS
//!
//! Each module is a `.locus` file under `stdlib/`, bundled with `include_str!` and
//! listed in [`MODULES`] with a **layer**. Layers form the capability stack
//!
//! ```text
//!   world  ⊃  winapi  ⊃  console  ⊃  …  ⊃  app
//!  (layer)     (0)        (1)            (user)
//! ```
//!
//! A LOWER layer is grafted FURTHER OUT — closest to the `world` (the runtime that
//! ultimately handles `winapi`). So `winapi` (the raw Win32 imports, the only ones
//! that touch the boundary) sits outermost; `console` builds on it; the app is
//! innermost. The invariant: a module may use names only from its own layer or a
//! lower one. A module is a chain of `let` / `let rec` / `type` declarations ending
//! in a `()` placeholder body.
//!
//! # How it loads
//!
//! [`program`] is the parse entry point both front ends use (in place of
//! [`crate::parse`]). It parses the user source on its own — so a user parse error
//! keeps its real span — then decides which modules to graft by a **fixpoint**: a
//! module is included if the user source, *or an already-included module*, mentions
//! (as a whole word) a name it binds — so a higher layer pulls in the lower layers
//! it depends on. Included modules are grafted by **descending layer** (lowest
//! outermost), their `()` placeholders replaced. A program that touches nothing
//! from the library is returned untouched, so dumps stay clean and codegen emits no
//! dead stdlib code.

use std::collections::HashSet;

use crate::parse::{parse_module, ParseErr};
use crate::syntax::{BlockItem, ModuleDecl, Term};

pub type ModuleSource = (u8, &'static str, &'static str);

/// `(layer, name, source)`. Lower layers graft outermost (closest to `world`); a
/// module uses names only from its own or a lower layer. Each `source` now
/// carries a `module … at <layer> [seals …] =` **header** (S1b); the `u8` layer
/// here mirrors the header and drives the graft order (kept for stability +
/// `republish`). The header is parsed off with [`parse_module`].
const WINDOWS_MODULES: &[ModuleSource] = &[
    (0, "winapi", include_str!("stdlib/winapi.locus")), // raw Win32 — closest to world
    (0, "crt", include_str!("stdlib/crt.locus")),       // raw UCRT math — boundary layer
    (0, "stringrt", include_str!("stdlib/stringrt.locus")),
    (0, "arrayrt", include_str!("stdlib/arrayrt.locus")),
    (0, "agentrt", include_str!("stdlib/agentrt.locus")),
    (1, "console", include_str!("stdlib/console.locus")), // console services over winapi
    (1, "docsfs", include_str!("stdlib/docsfs.locus")),   // Documents-only FS service
    (1, "locusenv", include_str!("stdlib/locusenv.locus")), // read-only LOCUS_* env service
    (1, "time", include_str!("stdlib/time.locus")),       // monotonic timing service
    (1, "db", include_str!("stdlib/db.locus")),           // mock DB service over WinCred
    (1, "agent", include_str!("stdlib/agent.locus")),     // MCP/agent ask/tell service
    (1, "string", include_str!("stdlib/string.locus")),   // UTF-16 string helpers
    (1, "math", include_str!("stdlib/math.locus")),       // sin/cos/pow/… over crt_*
    (1, "random", include_str!("stdlib/random.locus")),   // deterministic seed-threaded PRNG
    (1, "order", include_str!("stdlib/order.locus")), // min_by/max_by; uses num's Ordering, so grafts INNER of num (earlier = inner)
    (1, "num", include_str!("stdlib/num.locus")),     // pure Int / Ordering helpers
    (1, "bool", include_str!("stdlib/bool.locus")),   // boolean logic combinators
    (1, "array", include_str!("stdlib/array.locus")), // dense scalar array loops
    (1, "fun", include_str!("stdlib/fun.locus")),     // id, compose
    (1, "list", include_str!("stdlib/list.locus")),   // List + combinators
    (1, "option", include_str!("stdlib/option.locus")), // Option + combinators
    (1, "result", include_str!("stdlib/result.locus")), // Result + combinators
];

const LINUX_MODULES: &[ModuleSource] = &[
    (0, "libc", include_str!("stdlib/linux/libc.locus")),
    (0, "libm", include_str!("stdlib/linux/libm.locus")),
    (0, "stringrt", include_str!("stdlib/stringrt.locus")),
    (0, "arrayrt", include_str!("stdlib/arrayrt.locus")),
    (0, "agentrt", include_str!("stdlib/agentrt.locus")),
    (1, "console", include_str!("stdlib/linux/console.locus")),
    (1, "docsfs", include_str!("stdlib/linux/docsfs.locus")),
    (1, "locusenv", include_str!("stdlib/linux/locusenv.locus")),
    (1, "time", include_str!("stdlib/linux/time.locus")),
    (1, "agent", include_str!("stdlib/agent.locus")),
    (1, "string", include_str!("stdlib/string.locus")),
    (1, "math", include_str!("stdlib/linux/math.locus")),
    (1, "random", include_str!("stdlib/random.locus")),
    (1, "order", include_str!("stdlib/order.locus")),
    (1, "num", include_str!("stdlib/num.locus")),
    (1, "bool", include_str!("stdlib/bool.locus")),
    (1, "array", include_str!("stdlib/array.locus")),
    (1, "fun", include_str!("stdlib/fun.locus")),
    (1, "list", include_str!("stdlib/list.locus")),
    (1, "option", include_str!("stdlib/option.locus")),
    (1, "result", include_str!("stdlib/result.locus")),
];

/// The embedded stdlib modules `(layer, name, source)` — the authoritative copy
/// the compiler grafts. Exposed so the driver can **republish** them to disk for
/// review: the binary is the single source of truth and emits it on demand
/// (write-out only — the compiler never reads stdlib *back* from disk).
pub fn modules() -> &'static [ModuleSource] {
    WINDOWS_MODULES
}

pub fn linux_modules() -> &'static [ModuleSource] {
    LINUX_MODULES
}

/// The bundled stdlib as parsed **module declarations** — each header (layer /
/// seals / exposing) plus its body — for the per-module capability checks S2
/// (the mint-gate) and S4 (the `seals` clause) will run. Re-parses the embedded
/// sources on each call (cheap; the stdlib is small and this is not on the hot
/// path). Panics if a bundled module fails to parse — that is a compiler bug.
pub fn stdlib_module_decls() -> Vec<ModuleDecl> {
    stdlib_module_decls_from(WINDOWS_MODULES)
}

pub fn linux_stdlib_module_decls() -> Vec<ModuleDecl> {
    stdlib_module_decls_from(LINUX_MODULES)
}

pub fn stdlib_module_decls_from(modules: &[ModuleSource]) -> Vec<ModuleDecl> {
    modules
        .iter()
        .map(|(_, _, src)| parse_module(src).expect("a bundled stdlib module must parse"))
        .collect()
}

/// Parse `src`, grafting the stdlib modules it (transitively) uses around it.
/// This is the program-level parse — use it instead of [`crate::parse`].
///
/// It is **permissive** about minting: the graft itself never rejects code,
/// because exercising the raw FFI is a legitimate boundary activity (the embedded
/// stdlib, and the backend's own mechanism tests). The capability POLICY is the
/// driver's — the mint-gate ([`crate::capability::mint_gate`], over
/// [`first_mint`]) plus the manifest. Library detects; driver decides.
pub fn program(src: &str) -> Result<Term, ParseErr> {
    program_with_modules(src).map(|(term, _)| term)
}

pub fn linux_program(src: &str) -> Result<Term, ParseErr> {
    linux_program_with_modules(src).map(|(term, _)| term)
}

/// Like [`program`], but also returns the **user** module declarations parsed
/// from `src` (in declaration order), so the per-module capability checks (the
/// mint-gate S2, the `seals` clause S4) can run against them. The bundled stdlib
/// modules are available separately via [`stdlib_module_decls`].
pub fn program_with_modules(src: &str) -> Result<(Term, Vec<ModuleDecl>), ParseErr> {
    program_with_stdlib(src, WINDOWS_MODULES)
}

pub fn linux_program_with_modules(src: &str) -> Result<(Term, Vec<ModuleDecl>), ParseErr> {
    program_with_stdlib(src, LINUX_MODULES)
}

pub fn program_with_stdlib(
    src: &str,
    modules: &[ModuleSource],
) -> Result<(Term, Vec<ModuleDecl>), ParseErr> {
    // A program is now `(module | import)* entry` (S1a): user modules graft
    // *inside* the stdlib (at `app`), around the entry. A bare expression is the
    // modules-empty case, so existing programs are unaffected.
    let prog = crate::parse::parse_program(src)?;

    // Names the user binds — across the entry *and* every user module body — so
    // the stdlib never triggers on a name the user redefined.
    let mut user_bound = bound_names(&prog.entry);
    for m in &prog.modules {
        user_bound.extend(bound_names(&m.body));
    }
    // Which stdlib modules are in? Fixpoint: include a module whose names appear
    // (as a whole word) in the growing "active source" (the whole user source —
    // entry + user module bodies — plus already-included stdlib modules), so a
    // higher layer pulls in the lower layers it depends on. Trigger on real
    // identifier uses only: `code_only` blanks comments + string literals first
    // (a bare `console_writeln "pow"` must not drag in the math/crt modules).
    let active_user = code_only(src);
    let parsed_modules: Vec<(u8, ModuleDecl, &'static str)> = modules
        .iter()
        .map(|(layer, _, msrc)| {
            (
                *layer,
                parse_module(msrc).expect("a bundled stdlib module must parse"),
                *msrc,
            )
        })
        .collect();
    let mut active_modules = String::new();
    let mut included = vec![false; modules.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (i, (_, decl, msrc)) in parsed_modules.iter().enumerate() {
            if included[i] {
                continue;
            }
            let transitive_names = bound_names(&decl.body);
            let user_trigger_names = exposed_names(decl);
            let mentioned_by_included = transitive_names
                .iter()
                .any(|n| mentions_word(&active_modules, n));
            let mentioned_by_user = user_trigger_names
                .iter()
                .any(|n| !user_bound.contains(n) && mentions_word(&active_user, n));
            if mentioned_by_included || mentioned_by_user {
                included[i] = true;
                active_modules.push_str(msrc);
                changed = true;
            }
        }
    }

    // Graft, innermost-out: the entry, wrapped by the user modules (last-declared
    // innermost, so a later user module's bindings are in scope for the entry and
    // an earlier one's are in scope for the later — user-land layering by
    // declaration order), then by the stdlib (services first, the boundary
    // `winapi`/`crt` outermost — closest to the world, in scope for everyone).
    let user_modules = prog.modules.clone();
    let mut result = prog.entry;
    for m in prog.modules.into_iter().rev() {
        result = graft_in(m.body, result, Some(&m.name));
    }
    let mut order: Vec<usize> = (0..modules.len()).filter(|&i| included[i]).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(modules[i].0));
    for i in order {
        let decl = parsed_modules[i].1.clone();
        result = graft_in(decl.body, result, Some(&decl.name));
    }
    Ok((result, user_modules))
}

fn exposed_names(m: &ModuleDecl) -> HashSet<String> {
    match &m.exposing {
        Some(names) => names.iter().cloned().collect(),
        None => bound_names(&m.body),
    }
}

/// The first **mint** anywhere in `t`, if any — the capability detector, as a
/// human-readable phrase. A *mint* conjures a raw capability from outside the
/// language: `extern` (the FFI boundary) and the **raw memory** primitives
/// (`peek`/`poke`/`fill`/`copy`). These are **boundary-only** powers — the gate
/// ([`crate::capability::mint_gate`]) runs this over app / non-boundary code, and
/// a test runs it over the embedded services to assert none of them mints.
///
/// The bounds-checked **array accessor** (`a[i]`, `a[i] <- v` = `Index`/
/// `IndexSet`) is *not* a mint — it is the safe surface over memory, used
/// everywhere — so it recurses like any other container. An exhaustive walk: leaf
/// terms return `None`, so the compiler forces every future `Term` variant to be
/// classified here and the detector can't silently rot.
pub fn first_mint(t: &Term) -> Option<String> {
    use Term::*;
    let go = first_mint;
    match t {
        Extern(sym, _, _) => Some(format!("`extern {sym:?}`")),
        ExternAsm(sym, _) => Some(format!("`extern asm {sym:?}`")),
        // The raw memory primitives — a boundary power, distinct from the safe,
        // bounds-checked `a[i]` / `a[i] <- v` accessor (which recurses below).
        Peek(..) => Some("a raw memory read (`peek`)".into()),
        Poke(..) => Some("a raw memory write (`poke`)".into()),
        Fill(..) => Some("a raw memory fill (`fill`)".into()),
        Copy(..) => Some("a raw memory copy (`copy`)".into()),
        Var(_) | Int(_) | Float(_) | Bool(_) | Unit | Str(_) => None,
        Lam(_, _, a)
        | Perform(_, a)
        | Quote(a)
        | Splice(a)
        | Genlet(a)
        | Letloc(a)
        | Cast(_, a)
        | Sqrt(a)
        | Sum(a)
        | Length(a)
        | MaskReduce(_, a)
        | Len(a)
        | Seal(_, a)
        // `x := v` is not a mint — the first mint, if any, is in the value `v`.
        | Assign(_, a)
        // `ref e` (allocates — `{gc}`, a sealed capability, NOT a mint) and `!r`
        // (a heap read) are the safe `Ref` surface, like `a[i]` — recurse into the
        // sub-expression.
        | RefNew(a)
        | Deref(a)
        | Field(a, _) => go(a),
        Bin(_, a, b)
        | Dot(a, b)
        | App(a, b)
        | Let(_, a, b)
        // `let mut x = a in b` is not a mint — recurse into init and body, like `Let`.
        | LetMut(_, a, b)
        | LetRec(_, _, a, b)
        | Index(a, b)
        | LetTuple(_, a, b) => go(a).or_else(|| go(b)),
        If(a, b, c) | IndexSet(a, b, c) | Fma(a, b, c) | Select(a, b, c) => {
            go(a).or_else(|| go(b)).or_else(|| go(c))
        }
        Loop {
            vars,
            cond,
            steps,
            result,
        } => vars
            .iter()
            .find_map(|(_, init)| go(init))
            .or_else(|| go(cond))
            .or_else(|| steps.iter().find_map(go))
            .or_else(|| go(result)),
        Tuple(es) | ArrayLit(es) | Construct(_, es) | VectorLit(_, es) => es.iter().find_map(go),
        VectorSplat(_, a) => go(a),
        // A packed array vector load/store is the safe, bounds-checked managed-
        // array surface (like `a[i]` / `a[i] <- v`), NOT a mint — recurse into
        // the array, index, and (for the store) the stored vector.
        VectorLoad { arr, idx, .. } => go(arr).or_else(|| go(idx)),
        VectorStore {
            arr, idx, value, ..
        } => go(arr).or_else(|| go(idx)).or_else(|| go(value)),
        Record(fields) => fields.iter().find_map(|(_, e)| go(e)),
        Effect { body, .. } | TypeDef { body, .. } => go(body),
        Block(items, body) => items.iter().find_map(first_mint_item).or_else(|| go(body)),
        // A `trait`/`instance` declaration mints nothing itself; the first mint, if
        // any, is in an instance method body or the grafted decl body.
        Trait { body, .. } => go(body),
        Instance { methods, body, .. } => methods
            .iter()
            .find_map(|m| go(&m.body))
            .or_else(|| go(body)),
        Handle(a, h) => go(a)
            .or_else(|| h.ops.iter().find_map(|c| go(&c.body)))
            .or_else(|| go(&h.ret.body)),
        Match { scrutinee, arms } => {
            go(scrutinee).or_else(|| arms.iter().find_map(|arm| go(&arm.body)))
        }
    }
}

fn first_mint_item(item: &BlockItem) -> Option<String> {
    match item {
        BlockItem::Let(_, e)
        | BlockItem::LetRec(_, _, e)
        | BlockItem::LetMut(_, e)
        | BlockItem::LetTuple(_, e) => first_mint(e),
        BlockItem::Instance { methods, .. } => methods.iter().find_map(|m| first_mint(&m.body)),
        BlockItem::Effect { .. } | BlockItem::TypeDef { .. } | BlockItem::Trait { .. } => None,
    }
}

/// Does `src` mention `name` as a **whole identifier** (not a substring of a
/// longer word)? This keeps short stdlib names — `min`, `Lt`, `Eq` — from
/// false-triggering on words like `terMINated`. (A bare substring search was the
/// original trigger and was fine only because every prelude name was long and
/// distinctive; the library now has short ones.)
fn mentions_word(src: &str, name: &str) -> bool {
    let bytes = src.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut from = 0;
    while let Some(off) = src[from..].find(name) {
        let start = from + off;
        let end = start + name.len();
        let left_ok = start == 0 || !is_ident(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_ident(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Blank out comment text and string-literal bodies in `src` — replacing them with
/// spaces, so length and line structure are preserved — leaving only real code for
/// the stdlib-trigger scan. A name inside a `-- comment` or a `"string literal"`
/// must NOT pull in a module (`console_writeln "pow"` is a log line, not a math call).
fn code_only(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // `-- …` line comment → blank to end of line.
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        // `"…"` string literal (with `\`-escapes) → blank the whole literal.
        if bytes[i] == b'"' {
            out.push(b' ');
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push(b' ');
                    i += 1;
                }
                out.push(b' ');
                i += 1;
            }
            if i < bytes.len() {
                out.push(b' '); // the closing quote
                i += 1;
            }
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

/// The top-level names a module binds — `let`/`let rec` bindings, and `type`
/// declarations (the type name *and* each constructor). Used both as the
/// inclusion trigger and (implicitly) to document the module's surface.
pub(crate) fn bound_names(t: &Term) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut cur = t;
    loop {
        match cur {
            Term::Let(n, _, body) => {
                names.insert(n.clone());
                cur = body;
            }
            Term::Block(items, body) => {
                for item in items {
                    match item {
                        BlockItem::Let(n, _)
                        | BlockItem::LetRec(n, _, _)
                        | BlockItem::LetMut(n, _) => {
                            names.insert(n.clone());
                        }
                        BlockItem::LetTuple(tuple_names, _) => {
                            names.extend(tuple_names.iter().cloned());
                        }
                        BlockItem::TypeDef { name, variants, .. } => {
                            names.insert(name.clone());
                            for (ctor, _) in variants {
                                names.insert(ctor.clone());
                            }
                        }
                        BlockItem::Trait { methods, .. } => {
                            for m in methods {
                                names.insert(m.name.clone());
                            }
                        }
                        BlockItem::Effect { .. } | BlockItem::Instance { .. } => {}
                    }
                }
                cur = body;
            }
            Term::LetRec(n, _, _, body) => {
                names.insert(n.clone());
                cur = body;
            }
            Term::LetTuple(tuple_names, _, body) => {
                names.extend(tuple_names.iter().cloned());
                cur = body;
            }
            Term::LetMut(n, _, body) => {
                names.insert(n.clone());
                cur = body;
            }
            Term::TypeDef {
                name,
                variants,
                body,
                ..
            } => {
                names.insert(name.clone());
                for (ctor, _) in variants {
                    names.insert(ctor.clone());
                }
                cur = body;
            }
            // A `trait` declaration binds its method names (the minted generic
            // functions); an `instance` binds nothing new. Thread the body either
            // way so the chain reaches the entry.
            Term::Trait { methods, body, .. } => {
                for m in methods {
                    names.insert(m.name.clone());
                }
                cur = body;
            }
            Term::Instance { body, .. } => cur = body,
            // A module may *wrap* the app in a handler (the console seal); its
            // bindings live inside the scrutinee, so look there too.
            Term::Handle(scrutinee, _) => cur = scrutinee,
            _ => break,
        }
    }
    names
}

/// Replace a module's innermost (placeholder `()`) body with `user`, threading
/// through `let` / `let rec` / `type` declarations **and a handler's scrutinee**
/// — so a layer can *wrap* the app in `handle … with { … }`, not just prepend
/// bindings. That is how the console layer SEALS `winapi`: it grafts a handler
/// around the app whose `console` clause performs the Win32 output, so the app
/// only ever demands `{console}`.
/// Graft, threading the **declaring module name** (traits v1 orphan check R5,
/// `trait-resolution.md` §4). When a `module M = …` body is grafted, `home =
/// Some("M")`; the `trait`/`instance`/`type` declarations in that body are
/// stamped with `M` (only when their `module` is still `None`, so an
/// already-stamped inner decl keeps its own home). The stdlib graft passes the
/// stdlib module's name likewise. A bare program (no modules) grafts with `None`,
/// leaving the stamps empty so the orphan check is a no-op (nothing can be an
/// orphan without a module structure).
fn graft_in(module: Term, user: Term, home: Option<&str>) -> Term {
    let (items, tail) = peel_block_items(module, home);
    let body = match tail {
        Term::Handle(scrutinee, handler) => {
            Term::Handle(Box::new(graft_in(*scrutinee, user, home)), handler)
        }
        _ => user, // the placeholder body (`()`)
    };
    if items.is_empty() {
        body
    } else {
        Term::Block(items, Box::new(body))
    }
}

fn peel_block_items(mut term: Term, home: Option<&str>) -> (Vec<BlockItem>, Term) {
    let stamp = |m: Option<String>| m.or_else(|| home.map(|s| s.to_string()));
    let mut items = Vec::new();
    loop {
        match term {
            Term::Let(n, e, body) => {
                items.push(BlockItem::Let(n, *e));
                term = *body;
            }
            Term::LetRec(n, ty, e, body) => {
                items.push(BlockItem::LetRec(n, ty, *e));
                term = *body;
            }
            Term::LetMut(n, e, body) => {
                items.push(BlockItem::LetMut(n, *e));
                term = *body;
            }
            Term::LetTuple(names, e, body) => {
                items.push(BlockItem::LetTuple(names, *e));
                term = *body;
            }
            Term::Effect { name, ops, body } => {
                items.push(BlockItem::Effect { name, ops });
                term = *body;
            }
            Term::TypeDef {
                name,
                params,
                variants,
                module,
                body,
            } => {
                items.push(BlockItem::TypeDef {
                    name,
                    params,
                    variants,
                    module: stamp(module),
                });
                term = *body;
            }
            Term::Trait {
                name,
                param,
                supers,
                methods,
                module,
                body,
            } => {
                items.push(BlockItem::Trait {
                    name,
                    param,
                    supers,
                    methods,
                    module: stamp(module),
                });
                term = *body;
            }
            Term::Instance {
                trait_name,
                head,
                requires,
                methods,
                module,
                body,
            } => {
                items.push(BlockItem::Instance {
                    trait_name,
                    head,
                    requires,
                    methods,
                    module: stamp(module),
                });
                term = *body;
            }
            other => return (items, other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::TypeErr;
    use crate::parse::parse;
    use crate::{elaborate, Ctx, Sig};

    struct Judgment {
        ty: crate::syntax::Type,
        row: crate::syntax::Row,
    }

    fn ty_of(src: &str) -> Judgment {
        let src = src.to_string();
        std::thread::Builder::new()
            .name("stdlib-ty-of".into())
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let term = program(&src).unwrap();
                // The driver elaborates with the native-op signature (`console`, ...), so
                // the tests do too. Keep the large decorated tree on this larger stack.
                let t = elaborate(&crate::prelude::sig(), &Ctx::new(), 0, &term).unwrap();
                let crate::Typed { ty, row, .. } = t;
                Judgment { ty, row }
            })
            .expect("spawn stdlib type-check worker")
            .join()
            .expect("stdlib type-check worker panicked")
    }

    fn linux_ty_of(src: &str) -> Judgment {
        let src = src.to_string();
        std::thread::Builder::new()
            .name("linux-stdlib-ty-of".into())
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let term = linux_program(&src).unwrap();
                let t = elaborate(&crate::prelude::sig(), &Ctx::new(), 0, &term).unwrap();
                let crate::Typed { ty, row, .. } = t;
                Judgment { ty, row }
            })
            .expect("spawn linux stdlib type-check worker")
            .join()
            .expect("linux stdlib type-check worker panicked")
    }

    fn named(name: &str, args: Vec<crate::syntax::Type>) -> crate::syntax::Type {
        crate::syntax::Type::Named(name.into(), args)
    }

    /// The grafted declarations, OUTERMOST first (graft order).
    fn chain_order(t: &Term) -> Vec<String> {
        let mut v = Vec::new();
        let mut cur = t;
        loop {
            match cur {
                Term::Let(n, _, body) => {
                    v.push(n.clone());
                    cur = body;
                }
                Term::Block(items, body) => {
                    for item in items {
                        match item {
                            BlockItem::Let(n, _)
                            | BlockItem::LetRec(n, _, _)
                            | BlockItem::LetMut(n, _) => v.push(n.clone()),
                            BlockItem::LetTuple(names, _) => v.extend(names.iter().cloned()),
                            BlockItem::TypeDef { name, .. } => v.push(name.clone()),
                            BlockItem::Trait { methods, .. } => {
                                for m in methods {
                                    v.push(m.name.clone());
                                }
                            }
                            BlockItem::Effect { .. } | BlockItem::Instance { .. } => {}
                        }
                    }
                    cur = body;
                }
                Term::LetRec(n, _, _, body) => {
                    v.push(n.clone());
                    cur = body;
                }
                Term::LetTuple(names, _, body) => {
                    v.extend(names.iter().cloned());
                    cur = body;
                }
                Term::LetMut(n, _, body) => {
                    v.push(n.clone());
                    cur = body;
                }
                Term::TypeDef { name, body, .. } => {
                    v.push(name.clone());
                    cur = body;
                }
                Term::Effect { body, .. } => cur = body,
                Term::Handle(scrutinee, _) => cur = scrutinee,
                _ => break,
            }
        }
        v
    }

    #[test]
    fn stdlib_graft_compacts_long_declaration_spines() {
        let term = program("clock_millis ()").unwrap();
        let shape = crate::stackdiag::term_shape(&term);
        assert!(
            shape.max_binding_spine > 20,
            "fixture should pull a substantial stdlib spine: {shape:?}"
        );
        assert!(
            shape.max_depth < shape.max_binding_spine,
            "grafting should keep declaration count as block width, not recursive depth: {shape:?}"
        );
    }

    #[test]
    fn typed_stdlib_graft_preserves_compact_declaration_blocks() {
        let term = program("clock_millis ()").unwrap();
        let typed = elaborate(&crate::prelude::sig(), &Ctx::new(), 0, &term).unwrap();
        let shape = crate::stackdiag::typed_shape(&typed);
        assert!(
            shape.max_binding_spine > 20,
            "fixture should still include a substantial typed stdlib spine: {shape:?}"
        );
        assert!(
            shape.max_depth < shape.max_binding_spine,
            "typed stdlib declarations should remain block width, not recursive depth: {shape:?}"
        );
    }

    /// The console layer SEALS winapi: `console_writeln` performs `console`, the layer's
    /// handler discharges it via the Win32 calls. So the whole program performs
    /// `{winapi}` (handled by the world) and `console` is GONE from the row —
    /// the app demanded `{console}`, never `{winapi}`.
    #[test]
    fn console_layer_seals_winapi() {
        let t = ty_of(r#"console_writeln "hi""#);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "winapi"),
            "winapi present (the handler): {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "console"),
            "console discharged: {labels:?}"
        );
    }

    /// A first float output helper resolves through the console module. It uses a
    /// fixed runtime edge while FP extern ABI work is still open.
    #[test]
    fn float_console_helper_resolves() {
        let t = ty_of("console_write_float 1.5");
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert_eq!(t.ty, crate::syntax::Type::Unit);
        assert!(
            labels.iter().any(|l| l == "console_float"),
            "float output remains the fixed native output edge: {labels:?}"
        );
    }

    /// The winapi LAYER grafts OUTERMOST — its imports wrap the console layer, so
    /// they sit closest to the world. (And `console` pulled `winapi` in.)
    #[test]
    fn a_string_or_comment_math_name_does_not_graft_math() {
        // Short math names (`pow`, `log`, `sin`, `exp`) inside a string literal or a
        // `--` comment must NOT pull in the math/crt modules (and spuriously demand
        // ucrtbase.dll). `code_only` blanks strings + comments before the trigger.
        let order =
            chain_order(&program("console_writeln \"pow log sin\" -- exp cos tan").unwrap());
        assert!(
            !order
                .iter()
                .any(|n| n == "crt_pow" || n == "pow" || n == "ln"),
            "a string/comment math name grafted the CRT math layer: {order:?}"
        );
    }

    #[test]
    fn a_real_math_call_grafts_crt_outermost() {
        // `pow` as real code DOES graft math + crt; raw `crt_pow` (layer 0) wraps the
        // public `pow` (layer 1) — outermost, like winapi.
        let order = chain_order(&program("pow 2.0 3.0").unwrap());
        let crt = order.iter().position(|n| n == "crt_pow");
        let pub_pow = order.iter().position(|n| n == "pow");
        assert!(
            crt.is_some() && pub_pow.is_some(),
            "math layer not grafted: {order:?}"
        );
        assert!(crt < pub_pow, "raw crt_pow must wrap public pow: {order:?}");
    }

    #[test]
    fn linux_math_call_grafts_libm_not_crt() {
        let order = chain_order(&linux_program("pow 2.0 3.0").unwrap());
        let libm = order.iter().position(|n| n == "libm_pow");
        let pub_pow = order.iter().position(|n| n == "pow");
        assert!(
            libm.is_some() && pub_pow.is_some(),
            "linux math layer not grafted: {order:?}"
        );
        assert!(
            !order.iter().any(|n| n == "crt_pow"),
            "linux stdlib must not graft the Windows CRT boundary: {order:?}"
        );
        assert!(
            libm < pub_pow,
            "raw libm_pow must wrap public pow: {order:?}"
        );
    }

    #[test]
    fn winapi_layer_is_outermost() {
        let order = chain_order(&program(r#"console_writeln "x""#).unwrap());
        let winapi = order
            .iter()
            .position(|n| n == "win_GetStdHandle")
            .expect("winapi grafted");
        let console = order
            .iter()
            .position(|n| n == "console_writeln")
            .expect("console grafted");
        assert!(
            winapi < console,
            "winapi must be outermost (before console): {order:?}"
        );
    }

    #[test]
    fn linux_console_layer_uses_libc_not_winapi() {
        let order = chain_order(&linux_program(r#"console_writeln "x""#).unwrap());
        let libc = order
            .iter()
            .position(|n| n == "libc_write")
            .expect("linux libc grafted");
        assert!(
            order.iter().any(|n| n == "libc_malloc"),
            "linux console must allocate a UTF-8 byte buffer through libc: {order:?}"
        );
        let console = order
            .iter()
            .position(|n| n == "console_writeln")
            .expect("console grafted");
        assert!(
            !order.iter().any(|n| n == "getstdhandle"),
            "linux stdlib must not graft the Windows Winapi boundary: {order:?}"
        );
        assert!(
            libc < console,
            "linux libc must be outermost (before console): {order:?}"
        );
    }

    #[test]
    fn time_service_seals_windows_clock_effects() {
        let t = ty_of("let start = clock_ticks () in (elapsed_millis start) + (ticks_to_micros 0)");
        assert_eq!(t.ty, crate::syntax::Type::Int);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "winapi"),
            "Windows timing bottoms out in winapi: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "mem"),
            "Windows timing uses private boundary scratch memory: {labels:?}"
        );
        assert!(
            !labels
                .iter()
                .any(|l| l == "clock_ticks" || l == "clock_frequency"),
            "service-level timing effects should be discharged: {labels:?}"
        );
    }

    #[test]
    fn linux_time_service_seals_libc_clock_effects() {
        let t = linux_ty_of(
            "let start = clock_ticks () in (elapsed_millis start) + (clock_frequency ())",
        );
        assert_eq!(t.ty, crate::syntax::Type::Int);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "libc"),
            "Linux timing bottoms out in libc: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "mem"),
            "Linux timing uses private boundary scratch memory: {labels:?}"
        );
        assert!(
            !labels
                .iter()
                .any(|l| l == "clock_ticks" || l == "clock_frequency"),
            "service-level timing effects should be discharged: {labels:?}"
        );
    }

    #[test]
    fn time_service_grafts_winapi_outermost() {
        let order = chain_order(&program("clock_ticks ()").unwrap());
        let qpc = order
            .iter()
            .position(|n| n == "win_QueryPerformanceCounter")
            .expect("Windows performance counter boundary grafted");
        let clock_ticks = order
            .iter()
            .position(|n| n == "clock_ticks")
            .expect("public time service grafted");
        assert!(
            qpc < clock_ticks,
            "winapi timing boundary must wrap the public time service: {order:?}"
        );
    }

    #[test]
    fn linux_time_service_uses_libc_not_winapi() {
        let order = chain_order(&linux_program("clock_ticks ()").unwrap());
        let libc_clock = order
            .iter()
            .position(|n| n == "libc_clock_gettime")
            .expect("Linux clock_gettime boundary grafted");
        let clock_ticks = order
            .iter()
            .position(|n| n == "clock_ticks")
            .expect("public time service grafted");
        assert!(
            !order.iter().any(|n| n == "win_QueryPerformanceCounter"),
            "Linux stdlib must not graft the Windows clock boundary: {order:?}"
        );
        assert!(
            libc_clock < clock_ticks,
            "Linux libc timing boundary must wrap the public time service: {order:?}"
        );
    }

    #[test]
    fn agent_text_service_grafts_and_tracks_agent_effect() {
        let t = ty_of(
            r#"let answer = agent_ask_text "move?" in
               let _ = agent_tell_text answer in
               string_len answer"#,
        );
        assert_eq!(t.ty, crate::syntax::Type::Int);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "agent"),
            "Agent service must leave the host-channel capability visible: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "gc"),
            "agent_ask_text materializes a managed String: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "winapi"),
            "Agent service is runtime-hosted, not Win32-backed: {labels:?}"
        );

        let order = chain_order(&program(r#"agent_ask_text "move?""#).unwrap());
        assert!(
            order.iter().any(|n| n == "locus_agent_ask_utf8"),
            "AgentRt boundary should be pulled in: {order:?}"
        );
        assert!(
            order.iter().any(|n| n == "locus_string_from_utf8"),
            "Agent should reuse StringRt to materialize responses: {order:?}"
        );
        assert!(
            order.iter().any(|n| n == "agent_ask_text"),
            "public Agent service should be grafted: {order:?}"
        );
    }

    #[test]
    fn linux_agent_text_service_is_the_same_surface() {
        let t = linux_ty_of(r#"string_len (agent_ask_text "move?")"#);
        assert_eq!(t.ty, crate::syntax::Type::Int);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "agent"),
            "Linux Agent service should use the same host-channel label: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "gc"),
            "Linux Agent service also materializes managed strings: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "libc"),
            "Agent service should not depend on libc: {labels:?}"
        );
    }

    #[test]
    fn linux_docsfs_service_is_home_pinned_and_uses_libc() {
        let t = linux_ty_of(r#"docs_write_text "note.txt" "hello""#);
        assert_eq!(t.ty, crate::syntax::Type::Unit);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "libc"),
            "Linux DocsFs bottoms out in libc: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "mem"),
            "Linux DocsFs uses private boundary scratch memory: {labels:?}"
        );
        assert!(
            !labels
                .iter()
                .any(|l| l == "docsfs_write" || l == "docsfs_append" || l == "docsfs_read"),
            "DocsFs service effects should be discharged: {labels:?}"
        );

        let order = chain_order(&linux_program(r#"docs_exists "note.txt""#).unwrap());
        let getenv = order
            .iter()
            .position(|n| n == "libc_getenv")
            .expect("Linux DocsFs should read HOME through libc");
        let exists = order
            .iter()
            .position(|n| n == "docs_exists")
            .expect("public DocsFs service grafted");
        assert!(
            order.iter().any(|n| n == "libc_home_documents_path"),
            "DocsFs should build the pinned $HOME/Documents path: {order:?}"
        );
        assert!(
            !order.iter().any(|n| n == "win_CreateFileW"),
            "Linux DocsFs must not graft the Windows file boundary: {order:?}"
        );
        assert!(
            getenv < exists,
            "libc HOME lookup must wrap public DocsFs: {order:?}"
        );
    }

    #[test]
    fn linux_locusenv_service_uses_closed_keys_over_libc() {
        let t =
            linux_ty_of("match locus_env_get LocusHome with | None => 0 | Some(s) => string_len s");
        assert_eq!(t.ty, crate::syntax::Type::Int);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "libc"),
            "Linux LocusEnv bottoms out in libc getenv: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "locus_env_get"),
            "LocusEnv service effect should be discharged: {labels:?}"
        );

        let order = chain_order(&linux_program("locus_env_get LocusHome").unwrap());
        let getenv = order
            .iter()
            .position(|n| n == "libc_getenv")
            .expect("Linux LocusEnv should read via libc getenv");
        let service = order
            .iter()
            .position(|n| n == "locus_env_get")
            .expect("public LocusEnv service grafted");
        assert!(
            !order.iter().any(|n| n == "win_GetEnvironmentVariableW"),
            "Linux LocusEnv must not graft the Windows env boundary: {order:?}"
        );
        assert!(
            getenv < service,
            "libc getenv boundary must wrap public LocusEnv: {order:?}"
        );
    }

    #[test]
    fn db_mock_service_consumes_wincred_without_exposing_secret() {
        let t = ty_of(r#"if db_mock_connect "test.api.key" then 1 else 0"#);
        assert_eq!(t.ty, crate::syntax::Type::Int);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "winapi"),
            "Db mock bottoms out in visible WinAPI calls: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "mem"),
            "Db mock uses private boundary scratch memory: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "gc"),
            "Db mock materializes the private credential String today: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "db_mock_connect_op"),
            "Db mock service effect should be discharged: {labels:?}"
        );

        let order = chain_order(&program(r#"db_mock_health_check "test.api.key""#).unwrap());
        let cred_read = order
            .iter()
            .position(|n| n == "win_CredReadW")
            .expect("CredReadW boundary grafted");
        let service = order
            .iter()
            .position(|n| n == "db_mock_connect")
            .expect("public Db mock service grafted");
        assert!(
            order.iter().any(|n| n == "win_cred_target_name"),
            "Db mock should construct the pinned secure/credentials target: {order:?}"
        );
        assert!(
            cred_read < service,
            "CredReadW boundary must wrap public Db mock: {order:?}"
        );
    }

    #[test]
    fn wincred_secret_read_is_not_a_public_service() {
        let err = std::thread::Builder::new()
            .name("wincred-direct-absent".into())
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(|| {
                let term = program(r#"wincred_get "test.api.key""#).unwrap();
                elaborate(&crate::prelude::sig(), &Ctx::new(), 0, &term).unwrap_err()
            })
            .expect("spawn wincred direct absence worker")
            .join()
            .expect("wincred direct absence worker panicked");
        assert_eq!(err, TypeErr::Unbound("wincred_get".into()));
    }

    #[test]
    fn linux_stdlib_does_not_publish_db_mock() {
        let err = std::thread::Builder::new()
            .name("linux-db-mock-absent".into())
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(|| {
                let term = linux_program(r#"db_mock_connect "test.api.key""#).unwrap();
                elaborate(&crate::prelude::sig(), &Ctx::new(), 0, &term).unwrap_err()
            })
            .expect("spawn linux db mock absence worker")
            .join()
            .expect("linux db mock absence worker panicked");
        assert_eq!(err, TypeErr::Unbound("db_mock_connect".into()));
    }

    /// A `num` helper resolves through the library.
    #[test]
    fn num_helpers_resolve() {
        assert_eq!(ty_of("abs (0 - 5)").ty, crate::syntax::Type::Int);
        assert_eq!(ty_of("max 3 4").ty, crate::syntax::Type::Int);
    }

    /// Random is a shared layer-1 service with a deterministic seed-threaded core:
    /// scalar seed stepping is pure, while pair-returning draws expose tuple
    /// allocation as `gc`.
    #[test]
    fn random_helpers_resolve_on_windows_and_linux() {
        let scalar = ty_of("random_next_seed 12345");
        assert_eq!(scalar.ty, crate::syntax::Type::Int);
        let scalar_labels: Vec<String> = scalar.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            !scalar_labels.iter().any(|l| l == "gc"),
            "scalar seed stepping should not allocate: {scalar_labels:?}"
        );

        let roll = ty_of("let (value, seed2) = random_between 1 6 12345 in (value, seed2)");
        assert_eq!(
            roll.ty,
            crate::syntax::Type::Tuple(vec![crate::syntax::Type::Int, crate::syntax::Type::Int,])
        );
        let roll_labels: Vec<String> = roll.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            roll_labels.iter().any(|l| l == "gc"),
            "pair-returning random helpers allocate tuples: {roll_labels:?}"
        );

        assert_eq!(
            ty_of("let (ok, seed2) = random_chance 1 2 12345 in if ok then seed2 else 0").ty,
            crate::syntax::Type::Int
        );

        let linux_roll =
            linux_ty_of("let (value, seed2) = random_between 10 20 12345 in value + seed2");
        assert_eq!(linux_roll.ty, crate::syntax::Type::Int);

        let order = chain_order(&program("random_next_seed 12345").unwrap());
        assert!(
            order.iter().any(|n| n == "random_next_seed"),
            "random module should graft when used: {order:?}"
        );
        assert!(
            !order.iter().any(|n| n == "crt_pow" || n == "libm_pow"),
            "random should not pull platform math boundaries: {order:?}"
        );
    }

    /// A stdlib SUM TYPE grafts: `compare` returns `Ordering`, matched here.
    #[test]
    fn a_stdlib_sum_type_grafts() {
        let t = ty_of("match compare 3 5 with | Lt => 1 | Eq => 2 | Gt => 3");
        assert_eq!(t.ty, crate::syntax::Type::Int);
    }

    /// The array module exposes the first loop-backed dense-array helpers. They
    /// stay monomorphic so Int and Float callers land on the unboxed layouts.
    #[test]
    fn scalar_array_stdlib_helpers_graft() {
        let new_int = ty_of("array_make_int 4 9");
        assert_eq!(
            new_int.ty,
            crate::syntax::Type::Array(Box::new(crate::syntax::Type::Int))
        );
        let labels: Vec<String> = new_int.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "gc"),
            "array_make_int should expose allocation as gc: {labels:?}"
        );
        let order = chain_order(&program("array_make_int 4 9").unwrap());
        assert!(
            order.iter().any(|n| n == "locus_array_new_int"),
            "ArrayRt boundary should be pulled in: {order:?}"
        );
        assert!(
            order.iter().any(|n| n == "array_make_int"),
            "public Array service should be grafted: {order:?}"
        );
        assert_eq!(
            ty_of("array_make_int 4 9").ty,
            crate::syntax::Type::Array(Box::new(crate::syntax::Type::Int))
        );
        assert_eq!(
            ty_of("array_make_int 4 9").ty,
            crate::syntax::Type::Array(Box::new(crate::syntax::Type::Int))
        );
        assert_eq!(
            ty_of("let a = array_make 4 9 in a[1]").ty,
            crate::syntax::Type::Int
        );
        assert_eq!(
            ty_of("array_sum_int ([1, 2, 3])").ty,
            crate::syntax::Type::Int
        );
        assert_eq!(
            ty_of("array_sum_float ([1.0, 2.0, 3.0])").ty,
            crate::syntax::Type::Float
        );
        assert_eq!(
            ty_of("let a = [0, 0, 0] in array_fill_int a 7").ty,
            crate::syntax::Type::Unit
        );
        assert_eq!(
            ty_of("let a = [0.0, 0.0, 0.0] in array_fill_float a 1.5").ty,
            crate::syntax::Type::Unit
        );
        assert_eq!(
            ty_of(
                "let src = [1, 2, 3] in let dst = [0, 0, 0] in array_copy_range_int src 0 dst 0 3"
            )
            .ty,
            crate::syntax::Type::Unit
        );
        assert_eq!(
            ty_of("let src = [1.0, 2.0, 3.0] in let dst = [0.0, 0.0, 0.0] in array_copy_range_float src 0 dst 0 3").ty,
            crate::syntax::Type::Unit
        );
        // dot product is a Float; in-place scale is Unit.
        assert_eq!(
            ty_of("array_dot_float ([1.0, 2.0, 3.0]) ([4.0, 5.0, 6.0])").ty,
            crate::syntax::Type::Float
        );
        assert_eq!(
            ty_of("let a = [1.0, 2.0, 3.0] in array_scale_float a 2.0").ty,
            crate::syntax::Type::Unit
        );
    }

    /// T1 (D5) positive case: `Int` is a tag-room scalar that can inhabit a
    /// traced `Var` word cell, so the motivating generic-scalar path —
    /// `list_map` over a `List[Int]` — must keep type-checking unchanged. Paired
    /// with `check::tests::current_uniform_call_abi_rejects_id_applied_to_a_float`
    /// (the current conservative rejection), this isolates the "still works"
    /// half with no effect-row noise.
    #[test]
    fn list_map_over_int_still_typechecks_under_the_kind_rule() {
        let mapped = ty_of("list_map (Cons(1, Nil)) (fn x: Int => x)");
        assert_eq!(mapped.ty, named("List", vec![crate::syntax::Type::Int]));
    }

    /// S4: the generic stdlib grafts List and recursive combinators. This is the
    /// end-to-end regression for the S3.5 match-binder fix: `list_len` must stay
    /// polymorphic when its recursive call consumes the matched tail.
    #[test]
    fn generic_list_stdlib_grafts_and_stays_polymorphic() {
        let t = ty_of("(list_len (Cons(1, Nil)), list_len (Cons(true, Nil)))");
        assert_eq!(
            t.ty,
            crate::syntax::Type::Tuple(vec![crate::syntax::Type::Int, crate::syntax::Type::Int,])
        );

        let mapped = ty_of("list_map (Cons(true, Nil)) (fn b: Bool => b)");
        assert_eq!(mapped.ty, named("List", vec![crate::syntax::Type::Bool]));

        let effectful = ty_of(
            "effect tick : Int -> Int in list_map (Cons(1, Nil)) (fn x: Int => perform tick x)",
        );
        assert_eq!(effectful.ty, named("List", vec![crate::syntax::Type::Int]));
        let labels: Vec<String> = effectful.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "gc"),
            "list construction/match allocates: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "tick"),
            "callback effect passes through: {labels:?}"
        );

        // list_take / list_drop stay generic and return a List of the same element.
        assert_eq!(
            ty_of("list_take (Cons(1, Cons(2, Cons(3, Nil)))) 2").ty,
            named("List", vec![crate::syntax::Type::Int])
        );
        assert_eq!(
            ty_of("list_drop (Cons(true, Cons(false, Nil))) 1").ty,
            named("List", vec![crate::syntax::Type::Bool])
        );

        // list_find bridges List → Option: `option` grafts OUTER of `list`, so
        // list.locus can return Some/None. The result is Option[a].
        assert_eq!(
            ty_of("list_find (Cons(1, Cons(2, Nil))) (fn x: Int => x < 2)").ty,
            named("Option", vec![crate::syntax::Type::Int])
        );

        // option_to_result bridges Option → Result (`result` grafts OUTER of
        // `option`): Some(1) with a Bool err ⇒ Result[Int, Bool].
        assert_eq!(
            ty_of("option_to_result (Some(1)) false").ty,
            named(
                "Result",
                vec![crate::syntax::Type::Int, crate::syntax::Type::Bool]
            )
        );

        // the bool module's combinators are Bool -> … -> Bool.
        assert_eq!(ty_of("bool_not true").ty, crate::syntax::Type::Bool);
        assert_eq!(ty_of("bool_and true false").ty, crate::syntax::Type::Bool);
        assert_eq!(ty_of("bool_xor true false").ty, crate::syntax::Type::Bool);

        // order's min_by/max_by take a comparator (num's `compare` ⇒ Ordering); the
        // result is the element type. `order` grafts INNER of `num`, so Ordering is
        // in scope there.
        assert_eq!(ty_of("min_by 3 5 compare").ty, crate::syntax::Type::Int);
        assert_eq!(ty_of("max_by 3 5 compare").ty, crate::syntax::Type::Int);
    }

    /// S4: Option/Result and higher-order pure helpers are written in Locus and
    /// loaded through the same layered graft as the older monomorphic modules.
    #[test]
    fn generic_option_result_and_compose_graft() {
        let opt = ty_of("option_map (Some(true)) (fn b: Bool => b)");
        assert_eq!(opt.ty, named("Option", vec![crate::syntax::Type::Bool]));

        let opt_effectful =
            ty_of("effect tick : Int -> Int in option_map (Some(1)) (fn x: Int => perform tick x)");
        assert_eq!(
            opt_effectful.ty,
            named("Option", vec![crate::syntax::Type::Int])
        );
        let labels: Vec<String> = opt_effectful.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "tick"),
            "callback effect passes through: {labels:?}"
        );

        let res = ty_of("result_map (Ok(1)) (fn x: Int => x + 1)");
        assert_eq!(
            res.ty,
            named(
                "Result",
                vec![crate::syntax::Type::Int, crate::syntax::Type::Int],
            )
        );

        // The new parity combinators: `result_with_default` (Ok value or default)
        // and `option_is_none` (predicate pair with `option_is_some`).
        assert_eq!(
            ty_of("result_with_default (Ok(7)) 0").ty,
            crate::syntax::Type::Int,
            "result_with_default returns the Ok/default scalar"
        );
        assert_eq!(
            ty_of("option_is_none (Some(1))").ty,
            crate::syntax::Type::Bool,
            "option_is_none is a predicate"
        );

        assert_eq!(
            ty_of("compose (fn x: Int => x + 1) (fn y: Int => y * 2) 10").ty,
            crate::syntax::Type::Int,
        );

        let composed_effects = ty_of(
            "effect tick : Int -> Int in \
             effect log : Int -> Int in \
             compose (fn x: Int => perform tick x) (fn y: Int => perform log y) 10",
        );
        assert_eq!(composed_effects.ty, crate::syntax::Type::Int);
        let labels: Vec<String> = composed_effects
            .row
            .labels()
            .map(|l| format!("{l}"))
            .collect();
        assert!(
            labels.iter().any(|l| l == "tick"),
            "left callback effect is preserved: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "log"),
            "right callback effect is preserved: {labels:?}"
        );
    }

    /// S4 cont.: the new List combinators stay generic and thread the callback
    /// row exactly like `list_map`/`list_fold`.
    #[test]
    fn list_reverse_and_predicates_are_generic_and_effect_polymorphic() {
        assert_eq!(
            ty_of("list_reverse (Cons(1, Cons(2, Nil)))").ty,
            named("List", vec![crate::syntax::Type::Int])
        );
        assert_eq!(
            ty_of("list_reverse (Cons(true, Nil))").ty,
            named("List", vec![crate::syntax::Type::Bool])
        );

        assert_eq!(
            ty_of("list_filter (Cons(1, Nil)) (fn x: Int => true)").ty,
            named("List", vec![crate::syntax::Type::Int])
        );
        let filtered = ty_of(
            "effect keep : Int -> Bool in \
             list_filter (Cons(1, Nil)) (fn x: Int => perform keep x)",
        );
        let labels: Vec<String> = filtered.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "keep"),
            "predicate effect passes through filter: {labels:?}"
        );

        assert_eq!(
            ty_of("list_all (Cons(1, Nil)) (fn x: Int => true)").ty,
            crate::syntax::Type::Bool
        );
        assert_eq!(
            ty_of("list_any (Cons(1, Nil)) (fn x: Int => false)").ty,
            crate::syntax::Type::Bool
        );
    }

    /// `list_for_each` is Unit-valued and exists to RUN effects, so the
    /// callback's row is the whole point — it must survive into the result.
    #[test]
    fn list_for_each_threads_callback_effect() {
        let t = ty_of(
            "effect tick : Int -> Unit in \
             list_for_each (Cons(1, Cons(2, Nil))) (fn x: Int => perform tick x)",
        );
        assert_eq!(t.ty, crate::syntax::Type::Unit);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "gc"),
            "traversal allocates: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "tick"),
            "callback effect threads: {labels:?}"
        );
    }

    /// Monadic bind on Option and Result stays generic and short-circuits.
    #[test]
    fn option_and_result_bind_graft_generic() {
        assert_eq!(
            ty_of(
                "match option_bind (Some(1)) (fn x: Int => Some(x + 1)) \
                 with | None => 0 | Some(y) => y"
            )
            .ty,
            crate::syntax::Type::Int
        );
        assert_eq!(
            ty_of(
                "match result_bind (Ok(1)) (fn x: Int => Ok(x + 1)) \
                 with | Ok(y) => y | Err(e) => 0"
            )
            .ty,
            crate::syntax::Type::Int
        );
    }

    /// `const` and `flip` are fully parametric, pure higher-order helpers.
    #[test]
    fn fun_const_and_flip_are_parametric() {
        assert_eq!(ty_of("const 5 true").ty, crate::syntax::Type::Int);
        assert_eq!(
            ty_of("flip (fn a: Int => fn b: Bool => a) true 7").ty,
            crate::syntax::Type::Int
        );
    }

    /// A program that uses nothing from the library is left exactly as parsed —
    /// no graft, so dumps and codegen stay clean.
    #[test]
    fn a_pure_program_is_not_grafted() {
        assert_eq!(program("1 + 2").unwrap(), parse("1 + 2").unwrap());
    }

    /// Local declarations shadow stdlib triggers. A program defining its own
    /// `Some`/`None` or `List` should not pull in the generic stdlib modules.
    #[test]
    fn local_sum_names_do_not_trigger_stdlib_grafts() {
        let opt_order = chain_order(&program("type Opt = None | Some(Int) in Some(7)").unwrap());
        assert!(
            !opt_order.iter().any(|n| n == "Option" || n == "option_map"),
            "local Some/None should not graft stdlib option: {opt_order:?}"
        );

        let list_order =
            chain_order(&program("type List = Nil | Cons(Int, List) in Cons(1, Nil)").unwrap());
        assert!(
            !list_order
                .iter()
                .any(|n| matches!(n.as_str(), "list_len" | "list_map" | "list_fold")),
            "local List should not graft stdlib list: {list_order:?}"
        );
        assert_eq!(
            list_order.iter().filter(|n| n.as_str() == "List").count(),
            1,
            "only the local List declaration should be present: {list_order:?}"
        );
    }

    /// An undefined name is still an error (the library doesn't define it).
    #[test]
    fn unknown_names_still_fail_to_elaborate() {
        let term = program("nope 1").unwrap();
        assert!(elaborate(&Sig::new(), &Ctx::new(), 0, &term).is_err());
    }

    /// The mint detector finds a buried `extern` (here behind a `let`, a lambda,
    /// and a tuple, so it can't be smuggled past) and the **raw memory**
    /// primitives — but **not** the bounds-checked array accessor.
    #[test]
    fn the_mint_detector_reaches_nested_positions() {
        let shallow = parse(r#"let h = extern "GetStdHandle" : U32 -> Ptr in h 0"#).unwrap();
        assert!(first_mint(&shallow).unwrap().contains("GetStdHandle"));
        let deep =
            parse(r#"let f = fn x: Int => (x, extern "WriteFile" : Ptr -> I32) in 1"#).unwrap();
        assert!(first_mint(&deep).unwrap().contains("WriteFile"));
        // Raw memory is a mint…
        let raw = parse("let a = 1024 in poke8 a 65").unwrap();
        assert!(first_mint(&raw).unwrap().contains("poke"));
        // …but the safe array accessor and ordinary effects are not.
        assert_eq!(first_mint(&parse("let a = [1, 2] in a[0]").unwrap()), None);
        assert_eq!(first_mint(&parse(r#"console_writeln "hi""#).unwrap()), None);
    }

    /// Minting (`extern` / raw memory) is **boundary-EXCLUSIVE**: every embedded
    /// module at layer ≥ 1 (the services) reaches the world through boundary
    /// bindings, never by minting itself.
    #[test]
    fn only_boundary_modules_mint() {
        for (layer, name, src) in modules() {
            let body = parse_module(src).expect("a bundled module parses").body;
            if *layer >= 1 {
                assert_eq!(
                    first_mint(&body),
                    None,
                    "layer {layer} service `{name}` must not mint (boundary-only)"
                );
            }
        }
    }

    // ── user modules graft into the program (S1c) ───────────────────────

    #[test]
    fn a_user_module_grafts_around_the_entry() {
        // A user `module` declares bindings that the entry can use.
        let t = ty_of("module Util at app = let double = fn x: Int => x + x in () double 21");
        assert_eq!(t.ty, crate::syntax::Type::Int);
    }

    #[test]
    fn user_modules_layer_by_declaration_order() {
        // `B` (declared second, grafted inner) sees `A`'s binding (declared
        // first, grafted outer) — user-land layering by declaration order.
        let t = ty_of(
            "module A at app = let base = 100 in () \
             module B at app = let plus = fn x: Int => base + x in () \
             plus 5",
        );
        assert_eq!(t.ty, crate::syntax::Type::Int);
    }

    #[test]
    fn a_user_module_pulls_in_the_stdlib_and_the_seal_holds_through_it() {
        // A user module body naming `console_writeln` pulls in `console` — and the
        // console module's handler wraps the user module too, so `console` is
        // discharged into `winapi`: the seal holds *through* the user-module
        // layer (winapi present, console gone — just like app-level code).
        let t = ty_of(
            "module Greet at app = let hi = fn u: Unit => console_writeln \"hi\" in () \
             hi ()",
        );
        assert_eq!(t.ty, crate::syntax::Type::Unit);
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "winapi"),
            "the seal discharges console into winapi through the user module: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "console"),
            "console is sealed even when used from a user module: {labels:?}"
        );
    }

    #[test]
    fn crt_externs_mint_the_crt_label_distinct_from_winapi() {
        // The `Crt` boundary module declares `mints (crt)`, so its externs (and
        // the `math` aliases over them) carry `crt`, not the default `winapi` —
        // per-provider boundary labels via the `mints` clause.
        let t = ty_of("sin 1.0");
        let labels: Vec<String> = t.row.labels().map(|l| format!("{l}")).collect();
        assert!(
            labels.iter().any(|l| l == "crt"),
            "sin carries the crt mint label: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "winapi"),
            "crt is distinct from winapi: {labels:?}"
        );
    }

    #[test]
    fn program_with_modules_returns_the_user_modules() {
        let (_term, mods) =
            program_with_modules("module A at app = () module B at services = () 0").unwrap();
        assert_eq!(mods.len(), 2);
        assert_eq!(mods[0].name, "A");
        assert_eq!(mods[0].layer, crate::syntax::Layer::App);
        assert_eq!(mods[1].name, "B");
        assert_eq!(mods[1].layer, crate::syntax::Layer::Services);
    }
}
