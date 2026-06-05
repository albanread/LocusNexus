//! The **module interface** (`.locusi`) — the contract a client type-checks and
//! links against, without ever reading the producer's source
//! ([`../../docs/separate-compilation.md`] §2, §6; the sprint plan
//! [`../../docs/separate-compilation-sprints.md`] **Sprint 1**).
//!
//! Sprint 1 is the **interface format + emit** only: a [`ModuleInterface`] data
//! structure, a **textual** serializer + parser (O-S1 — lean, auditable: the
//! transparency thesis applies to interfaces too), and a round-trippable build.
//! Cross-module *consumption* (type-check against an imported interface, Sprint
//! 2) and codegen/link (Sprint 3) are **not** built here.
//!
//! What the interface records (§2): the module's `name`/`layer`/`mints`/`seals`
//! (5), its **exported value signatures** with the inferred **effect row** + any
//! trait/kind constraints (1, a [`crate::check::Scheme`]), its **exported type
//! definitions + layouts** (2), and an **interface hash + an ABI/representation
//! version** (7). Trait instances (3) and macros (4) are the named follow-ons
//! (§4b/§4c) — out of v1.
//!
//! "**Exported**" = the module's `exposing (…)` list, or *all* top-level bindings
//! (and type declarations) when `exposing` is omitted (the `bound_names` rule the
//! seal check already uses, [`crate::capability::check_module_seals`]).
//!
//! Diagnostics (`RN-E06xx`, §8) are Sprint 2 — none are emitted here.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::check::{Binding, Ctx, Scheme};
use crate::sema::{elaborate, Node, Typed, TypedBlockItem};
use crate::syntax::{Label, Layer, ModuleDecl, Term, Type, ValueLayout};

/// The **ABI / representation version** (§6): the uniform-cell + handle
/// conventions and the layout rules carry a version. An interface (or object)
/// built under a different version is rejected (`RN-E0603`, Sprint 2) rather than
/// mis-linked. Bump this whenever the cross-module representation changes.
pub const ABI_VERSION: u32 = 1;

/// The textual-format tag — the first thing a `.locusi` file declares, so a
/// reader (and a future tool) can recognise it. Versioned independently of the
/// ABI: this is the *grammar* version, `ABI_VERSION` is the *representation*.
pub const LOCUSI_FORMAT: &str = "locusi/1";

/// An exported **value signature** (§2.1): a binding name paired with its full
/// type — including the inferred **effect row** on each arrow, and (when the
/// scheme is qualified) its trait/kind constraints. Modelled as a
/// [`Scheme`] so Sprint 2 can layer cross-module qualified-type resolution on
/// without re-touching the representation; today's grafted-then-zonked build
/// yields a ground (monomorphic) scheme — D6 defaults any residual variable — so
/// `scheme.ty_vars`/`row_vars`/`constraints` are empty for the v1 surface, and
/// the *row* is what carries the cross-boundary information (§4a transparency).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ValExport {
    pub name: String,
    pub scheme: Scheme,
}

/// An exported **type definition + layout** (§2.2): enough for a client to
/// construct / match / project. A sum records its constructors (each a tag + its
/// field types); a record records its fields. The **layout** (pointer-vs-scalar
/// cell counts) is part of the contract — a client lays the value out the same
/// way across the link.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypeExport {
    pub name: String,
    /// The declared type parameters, in order (`[]` for a monomorphic sum).
    pub params: Vec<String>,
    pub def: TypeDefKind,
    /// The value's storage layout when held in one managed field — the
    /// representation half of the contract (§2.2 "pointers-first layout").
    pub layout: ValueLayout,
}

/// A type export's shape — the two product/sum forms the surface declares.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeDefKind {
    /// `type Name[..] = C1(T..) | C2 | …` — a sum: each variant is `(tag, ctor,
    /// field types)`. The tag is the runtime discriminant (declaration order).
    Sum(Vec<SumVariant>),
    /// `type Name[..] = { f1: T1, f2: T2, … }` — a record alias (named-field
    /// product). Fields kept in declaration order.
    Record(Vec<(String, Type)>),
}

/// One constructor of an exported sum type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SumVariant {
    pub tag: i64,
    pub ctor: String,
    pub fields: Vec<Type>,
}

/// A **module interface** — the producer's exports, type-complete (§2). The only
/// artifact a client reads to type-check + link against the module.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModuleInterface {
    pub name: String,
    pub layer: Layer,
    /// The raw labels this module **mints** (§5) — `boundary`-only; the linker
    /// trusts a recorded mint exactly as the source-level mint-gate did.
    pub mints: Vec<Label>,
    /// The raw labels this module **seals** at its export edge (§5).
    pub seals: Vec<Label>,
    /// Exported value signatures (1), sorted by name (a canonical, stable order).
    pub vals: Vec<ValExport>,
    /// Exported type definitions + layouts (2), in declaration order.
    pub types: Vec<TypeExport>,
    /// The ABI/representation version this interface was built under (§6).
    pub abi_version: u32,
}

impl ModuleInterface {
    /// The **interface hash** (§6): a stable hash of the *exported contract* —
    /// the name, layer, the (sorted) mints/seals, every exported value signature
    /// (`name : type`, sorted), and every exported type definition + layout. It
    /// does **not** cover the ABI version (that is a separate skew check) nor any
    /// formatting incidental (whitespace, the format tag): two builds of the same
    /// contract hash identically, and a changed signature changes the hash. A
    /// client records this to detect a stale interface (`RN-E0600`, Sprint 2).
    ///
    /// FNV-1a over a canonical byte image of the contract. Self-contained (no
    /// external crate — the core stays zero-dependency) and order-stable: the
    /// `vals` are sorted by name and the mints/seals are emitted through a
    /// `BTreeSet`, so the image is independent of collection/iteration order.
    pub fn hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.str("name").str(&self.name);
        h.str("layer").str(self.layer.name());
        h.str("mints");
        for l in self.mints.iter().collect::<BTreeSet<_>>() {
            h.str(&l.to_string());
        }
        h.str("seals");
        for l in self.seals.iter().collect::<BTreeSet<_>>() {
            h.str(&l.to_string());
        }
        h.str("vals");
        // `vals` is kept sorted by name on construction; hash through it as-is.
        for v in &self.vals {
            h.str(&v.name).str(&scheme_text(&v.scheme));
        }
        h.str("types");
        for t in &self.types {
            h.str(&t.name);
            for p in &t.params {
                h.str(p);
            }
            match &t.def {
                TypeDefKind::Sum(vs) => {
                    h.str("sum");
                    for v in vs {
                        h.u64(v.tag as u64).str(&v.ctor);
                        for f in &v.fields {
                            h.str(&f.to_string());
                        }
                    }
                }
                TypeDefKind::Record(fs) => {
                    h.str("record");
                    for (n, ty) in fs {
                        h.str(n).str(&ty.to_string());
                    }
                }
            }
            h.layout(t.layout);
        }
        h.finish()
    }
}

/// The **`RN-E06xx` "module / link" family** (`separate-compilation.md` §8). The
/// consume-path codes are **wired in Sprint 2** ([`ConsumeError`]); the rest stay
/// reserved for their follow-on. The catalog (`error-catalog.md`) lands them when
/// the lead journals the sprint.
///
/// - `RN-E0600` `module.stale-interface` — **wired (Sprint 2):** a client-load
///   hash mismatch ([`LoadedInterface::load`] → [`ConsumeError::StaleInterface`]);
///   also the nearest code for a malformed `.locusi`.
/// - `RN-E0601` `module.missing-export` — **wired (Sprint 2):** importing a name
///   the interface doesn't export ([`resolve_imports`] →
///   [`ConsumeError::MissingExport`]).
/// - `RN-E0602` `module.macro-load` — an imported macro's stage-1 body can't be
///   loaded/run (the §4c follow-on, out of v1).
/// - `RN-E0603` `module.abi-version` — **wired (Sprint 2):** an interface built
///   under an incompatible [`ABI_VERSION`], checked at load
///   ([`LoadedInterface::accept`] → [`ConsumeError::AbiVersion`]).
/// - `RN-E0604` `module.import-cycle` — **partially wired (Sprint 2):** the trivial
///   self-import is rejected ([`ConsumeError::ImportCycle`]); the transitive
///   import-graph cycle is **Sprint 2b** (needs a name-keyed interface set + DFS the
///   single-client driver does not thread yet).
///
/// **Sprint 2b (deferred, with notes in the code):**
/// - **Sealed-label-crossing** (`RN-E0403 cap.seal-leak`, §5): a producer interface
///   whose `seals (L)` leaks `L` through an exported signature. The check is
///   "checkable from the interface alone" — but doing it *right* means re-running
///   the module seal-leak analysis ([`crate::capability`]) over the loaded
///   interface's `vals`, and the lead's instruction is explicit: do **not**
///   half-implement sealing. Deferred whole to Sprint 2b.
/// - **Transitive import cycles** (`RN-E0604`): as above.
/// - **Polymorphic cross-module imports** (the generalized-scheme capture): see
///   [`check_client_against`] — mono schemes ship today; `Poly` capture is 2b.
mod reserved_diagnostics {}

// ── building an interface from a compiled module ─────────────────────────

/// What can go wrong building an interface from a module's source.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IfaceError {
    /// The module body did not type-check (elaboration failed). Carries the
    /// underlying type error's `RN-Exxxx` code + message for the driver to print.
    Elaborate(String),
    /// A **higher-order export** — an exposed function whose *uncurried body* is
    /// itself a function (a closure-returning export: more arrows in its type than
    /// lambdas in its definition, e.g. `mk : Int -> Int -> Int = fn a => helper a`).
    /// Separate compilation v1 ships **only first-order** function exports across
    /// the link; emitting a flat symbol here would be an **arity miscompile** — the
    /// producer would emit `@mk(i64)` returning a closure handle while the client,
    /// reading the identical `Int -> Int -> Int` type, calls `@mk(i64, i64)` and
    /// treats the closure handle as the final scalar. The interface type cannot
    /// distinguish the two shapes, so the producer refuses cleanly rather than
    /// mis-link (`separate-compilation.md` §1; the named closure-crossing deferral).
    HigherOrderExport { name: String },
}

impl std::fmt::Display for IfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IfaceError::Elaborate(m) => write!(f, "module does not type-check: {m}"),
            IfaceError::HigherOrderExport { name } => write!(
                f,
                "separate compilation v1: higher-order export `{name}` (a function returning a \
                 function) is not yet supported — only first-order function exports cross the link"
            ),
        }
    }
}

/// Build the [`ModuleInterface`] for one compiled module.
///
/// `module` is the user's `module … = …` declaration; `grafted` is the *whole*
/// program the stdlib graft produced for it (so the module's body type-checks
/// against the stdlib names it uses) — typically `stdlib::program(&module_src)`.
/// The interface records **only the module's own exported bindings/types**, never
/// the grafted stdlib: exports are filtered to the module's `exposing` list (or
/// its own `bound_names` when `exposing` is omitted).
///
/// Collection: elaborate `grafted` (→ a zonked [`Typed`] tree), walk it for each
/// top-level binding's type ([`binding_types`]-style), keep the ones the module
/// exports; pull `layer`/`mints`/`seals` from the `ModuleDecl`; read the module's
/// own `type` declarations (+ their layouts) straight off the `ModuleDecl.body`
/// (type decls erase during elaboration, so the source body is the model).
pub fn interface_of(module: &ModuleDecl, grafted: &Term) -> Result<ModuleInterface, IfaceError> {
    let tree = elaborate(
        &crate::prelude::sig(),
        &crate::check::Ctx::new(),
        0,
        grafted,
    )
    .map_err(|e| IfaceError::Elaborate(format!("{} {}", e.code(), e)))?;

    // Every top-level binding's type, across the grafted chain.
    let mut all_types = std::collections::HashMap::new();
    binding_types(&tree, &mut all_types);

    // The module's own type declarations (name → its def + params), read from the
    // source body — `type` erases during elaboration.
    let own_types = collect_type_defs(&module.body);

    // What this module exports: its `exposing (…)` list, or all the names it binds.
    let exposed: BTreeSet<String> = match &module.exposing {
        Some(names) => names.iter().cloned().collect(),
        None => crate::stdlib::bound_names(&module.body)
            .into_iter()
            .collect(),
    };

    // Value exports: an exposed name that is a top-level *value* binding (present
    // in `all_types`). A name that is a type/ctor export is handled below; a name
    // the module does not actually bind is silently skipped (Sprint 2's
    // `RN-E0601` is the place to reject a bogus `exposing` entry).
    let mut vals: Vec<ValExport> = exposed
        .iter()
        .filter_map(|name| {
            all_types.get(name).map(|ty| ValExport {
                name: name.clone(),
                scheme: Scheme::mono(ty.clone()),
            })
        })
        .collect();
    vals.sort_by(|a, b| a.name.cmp(&b.name));

    // Type exports: an exposed type name (the `type` declaration's own name). A
    // sum also exposes its constructors — but those are *values* in the export
    // list; the type export carries the constructor set, so we do not double-list
    // them as vals (a ctor's name maps to a function type in `all_types`, but it
    // is recorded structurally on the sum). Keep declaration order.
    let mut types: Vec<TypeExport> = Vec::new();
    for (name, params, kind) in &own_types {
        if exposed.contains(name) {
            types.push(TypeExport {
                name: name.clone(),
                params: params.clone(),
                def: kind.clone(),
                layout: type_layout(kind),
            });
        }
    }

    // A constructor of an exported type is part of the *type* export, not a
    // standalone val — drop it from `vals` to avoid redundant (and
    // layout-irrelevant) function signatures for `Cons`/`Nil`/etc.
    let ctor_names: BTreeSet<String> = types
        .iter()
        .flat_map(|t| match &t.def {
            TypeDefKind::Sum(vs) => vs.iter().map(|v| v.ctor.clone()).collect::<Vec<_>>(),
            TypeDefKind::Record(_) => Vec::new(),
        })
        .collect();
    vals.retain(|v| !ctor_names.contains(&v.name));

    Ok(ModuleInterface {
        name: module.name.clone(),
        layer: module.layer,
        mints: module.mints.clone(),
        seals: module.seals.clone(),
        vals,
        types,
        abi_version: ABI_VERSION,
    })
}

// ── cross-module codegen: the mangling + the producer's exported functions ──

/// The **stable cross-module symbol** for a module's exported value (§2.2, the
/// uniform-ABI link). The producer emits its exported function under this name
/// and the client declares the same name external, so the linker resolves them
/// (`separate-compilation-sprints.md` Sprint 3). Dots in the module path become
/// underscores; the `locus__<Module>__<name>` shape is unambiguous and a legal C
/// identifier. **The one fn shared by the producer emit and the client extern
/// decl** — change it here and both sides move together.
pub fn mangle_export(module: &str, name: &str) -> String {
    format!("locus__{}__{}", module.replace('.', "_"), name)
}

/// One **exported first-order function** of a producer module, ready for the
/// backend to emit as a flat, externally-visible uniform-ABI symbol (Sprint 3).
/// `params` is the uncurried parameter list (name + type + layout, outermost
/// first); `body` is the innermost lambda body (a `Typed` the backend lowers
/// directly, with the params already in scope as ordinary `Var`s). `mangled` is
/// [`mangle_export`] applied to the module + `name`, so it matches the client's
/// external declaration.
#[derive(Clone, Debug)]
pub struct ExportedFn {
    pub name: String,
    pub mangled: String,
    pub params: Vec<(String, Type, ValueLayout)>,
    pub ret_ty: Type,
    pub body: Typed,
}

impl ExportedFn {
    /// The function's arity — the uncurried parameter count, hence the number of
    /// `i64` parameters of its flat ABI symbol.
    pub fn arity(&self) -> usize {
        self.params.len()
    }

    /// The native [`crate::syntax::ExternAbi`] a **client** declares this export
    /// under: the uniform all-`W64` (i64) ABI of the right arity, no width
    /// conversion (every cell is an i64 handle/scalar). Shared shape with the
    /// producer's flat symbol — the client's `Foreign` call and the producer's
    /// definition agree by construction.
    pub fn client_abi(&self) -> crate::syntax::ExternAbi {
        uniform_abi(self.arity())
    }
}

/// The uniform cross-module [`crate::syntax::ExternAbi`] for an `arity`-ary
/// first-order export: `arity` `W64` parameters returning `W64`. Every value
/// crosses the link as a plain i64 (a handle or a scalar) — the lower-once,
/// called-directly ABI (`separate-compilation.md` §1). A nullary export is the
/// degenerate empty-params case the spine collector treats as a `f ()` call.
fn uniform_abi(arity: usize) -> crate::syntax::ExternAbi {
    crate::syntax::ExternAbi {
        params: vec![crate::syntax::Width::W64; arity],
        ret: crate::syntax::Width::W64,
    }
}

/// Collect the producer module's **exported first-order functions** as
/// [`ExportedFn`]s — the v1 cross-module codegen surface (Sprint 3). Each is an
/// exposed top-level binding whose elaborated value is a (possibly curried)
/// lambda chain, uncurried into a flat parameter list + an innermost body the
/// backend lowers to one externally-visible `i64 @<mangled>(i64, …)` symbol.
///
/// `module`/`grafted` are exactly [`interface_of`]'s inputs (the user's
/// declaration + the stdlib graft that lets its body type-check). We elaborate
/// the graft, find each exposed binding's `bound` `Typed`, and keep the ones that
/// are lambdas — a **value** export (no lambda) is deferred (it needs producer
/// module-init + a global; `separate-compilation-sprints.md` "Deferred"). The
/// producer source is the only thing read; the client never sees any of this.
pub fn exported_functions(
    module: &ModuleDecl,
    grafted: &Term,
) -> Result<Vec<ExportedFn>, IfaceError> {
    let tree = elaborate(
        &crate::prelude::sig(),
        &crate::check::Ctx::new(),
        0,
        grafted,
    )
    .map_err(|e| IfaceError::Elaborate(format!("{} {}", e.code(), e)))?;

    // The bound value (a `Typed`) of every top-level binding, first-wins (the
    // module grafts outermost, so its own export is found before any stdlib one).
    let mut bounds: std::collections::HashMap<String, Typed> = std::collections::HashMap::new();
    binding_bounds(&tree, &mut bounds);

    // What this module exports (same rule as `interface_of`): its `exposing (…)`
    // list, or every name it binds.
    let exposed: BTreeSet<String> = match &module.exposing {
        Some(names) => names.iter().cloned().collect(),
        None => crate::stdlib::bound_names(&module.body)
            .into_iter()
            .collect(),
    };
    // Constructors of the module's own sum types are part of the *type* export,
    // not standalone function exports — skip them (mirrors `interface_of`).
    let ctor_names: BTreeSet<String> = collect_type_defs(&module.body)
        .iter()
        .flat_map(|(_, _, kind)| match kind {
            TypeDefKind::Sum(vs) => vs.iter().map(|v| v.ctor.clone()).collect::<Vec<_>>(),
            TypeDefKind::Record(_) => Vec::new(),
        })
        .collect();

    let mut out = Vec::new();
    // Sorted for a deterministic emission order (the object's symbol order).
    for name in &exposed {
        if ctor_names.contains(name) {
            continue;
        }
        let Some(bound) = bounds.get(name) else {
            continue; // a type/ctor name, or a name the module does not bind
        };
        // Uncurry the lambda chain: peel `fn p => fn q => … body` into the flat
        // parameter list + the innermost body. A non-lambda export is a *value*
        // export (deferred) — skipped here, not an error (its signature still
        // ships in the interface; only its codegen waits for module-init). A
        // higher-order export (uncurried body is itself a function) is **refused**
        // (`IfaceError::HigherOrderExport`) rather than emitted at a mismatched
        // arity — see the variant's doc.
        if let Some(export) = uncurry_export(module, name, bound)? {
            out.push(export);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Peel an elaborated binding's lambda chain into a flat [`ExportedFn`]. Each
/// `Node::Lam` contributes one uniform-ABI parameter; the chain bottoms out at
/// the first non-lambda node, which becomes the body. Three outcomes:
///
/// - `Ok(None)` — **a value export** (no lambdas): deferred (it needs producer
///   module-init + a global). Its signature still ships in the interface and the
///   client also omits it from the call table, so the skip is safe.
/// - `Ok(Some(_))` — **a first-order function export**: the syntactic lambda depth
///   equals the type's arrow arity, so the uncurried body is a non-function
///   (ground/handle) value — the flat `i64 @<mangled>(i64, …)` symbol is sound.
/// - `Err(HigherOrderExport)` — the uncurried body's type **is** a [`Type::Fun`]:
///   the export returns a closure (its type has more arrows than the definition has
///   lambdas, e.g. `mk : Int -> Int -> Int = fn a => helper a`). Emitting a flat
///   symbol here would be an **arity miscompile** vs. the client's arrow-derived
///   arity — refuse cleanly (see [`IfaceError::HigherOrderExport`]).
fn uncurry_export(
    module: &ModuleDecl,
    name: &str,
    bound: &Typed,
) -> Result<Option<ExportedFn>, IfaceError> {
    let mut params = Vec::new();
    let mut cur = bound;
    while let Node::Lam {
        param,
        param_ty,
        body,
    } = &cur.node
    {
        let layout = param_ty.storage_layout();
        params.push((param.clone(), param_ty.clone(), layout));
        cur = body;
    }
    if params.is_empty() {
        return Ok(None); // not a function — a value export, deferred
    }
    // **The first-order guard.** The uncurried body is what the flat symbol
    // returns; if it is itself a function, the producer's lambda-depth arity
    // (`params.len()`) is *less* than the client's arrow-depth arity, and emitting
    // the symbol would silently mis-link. Refuse it instead of miscompiling.
    if matches!(cur.ty, Type::Fun(..)) {
        return Err(IfaceError::HigherOrderExport {
            name: name.to_string(),
        });
    }
    Ok(Some(ExportedFn {
        name: name.to_string(),
        mangled: mangle_export(&module.name, name),
        params,
        ret_ty: cur.ty.clone(),
        body: cur.clone(),
    }))
}

/// The **bound value** of every top-level binding (a sibling of [`binding_types`]
/// that keeps the whole `Typed`, not just its type) — what the producer's
/// function emit lowers. First binding of a name wins (outermost graft first).
fn binding_bounds(t: &Typed, out: &mut std::collections::HashMap<String, Typed>) {
    match &t.node {
        Node::Let { name, bound, body } => {
            out.entry(name.clone()).or_insert_with(|| (**bound).clone());
            binding_bounds(body, out);
        }
        Node::Block { items, body } => {
            for item in items {
                if let TypedBlockItem::Let { name, bound } = item {
                    out.entry(name.clone()).or_insert_with(|| bound.clone());
                }
            }
            binding_bounds(body, out);
        }
        Node::LetTuple(_, _, body) => binding_bounds(body, out),
        Node::Handle { scrutinee, .. } => binding_bounds(scrutinee, out),
        Node::LetMut { body, .. } => binding_bounds(body, out),
        _ => {}
    }
}

/// Collect the **type of every top-level binding** in an elaborated program —
/// the `let`/`let rec` bindings of the grafted module chain (and inside a
/// service module's `handle … with { … }` wrap). Mirrors the private
/// `capability::binding_types`; duplicated here (small, and the interface builder
/// is a distinct concern) so this module stays self-contained. First binding of
/// a name wins (a module grafts outermost, so its own export is found first).
fn binding_types(t: &Typed, out: &mut std::collections::HashMap<String, Type>) {
    match &t.node {
        Node::Let { name, bound, body } => {
            out.entry(name.clone()).or_insert_with(|| bound.ty.clone());
            binding_types(body, out);
        }
        Node::Block { items, body } => {
            for item in items {
                if let TypedBlockItem::Let { name, bound } = item {
                    out.entry(name.clone()).or_insert_with(|| bound.ty.clone());
                }
            }
            binding_types(body, out);
        }
        Node::LetTuple(_, _, body) => binding_types(body, out),
        Node::Handle { scrutinee, .. } => binding_types(scrutinee, out),
        Node::LetMut { body, .. } => binding_types(body, out),
        _ => {}
    }
}

/// Walk a module body's `let`/`type` chain, collecting each `type` declaration's
/// `(name, params, kind)`. A `type … = { f: T, … } in …` with a single record
/// variant whose ctor matches the type name is recorded as a [`TypeDefKind::Record`];
/// everything else is a [`TypeDefKind::Sum`]. (The surface has no record-`type`
/// syntax yet — sums are the v1 case — but the structure is ready for it.)
fn collect_type_defs(body: &Term) -> Vec<(String, Vec<String>, TypeDefKind)> {
    let mut out = Vec::new();
    let mut cur = body;
    loop {
        match cur {
            Term::Let(_, _, b) => cur = b,
            Term::LetRec(_, _, _, b) => cur = b,
            Term::LetTuple(_, _, b) => cur = b,
            Term::LetMut(_, _, b) => cur = b,
            Term::Block(items, b) => {
                for item in items {
                    if let crate::syntax::BlockItem::TypeDef {
                        name,
                        params,
                        variants,
                        ..
                    } = item
                    {
                        let variants: Vec<SumVariant> = variants
                            .iter()
                            .enumerate()
                            .map(|(tag, (ctor, fields))| SumVariant {
                                tag: tag as i64,
                                ctor: ctor.clone(),
                                fields: fields.clone(),
                            })
                            .collect();
                        out.push((name.clone(), params.clone(), TypeDefKind::Sum(variants)));
                    }
                }
                cur = b;
            }
            Term::Trait { body: b, .. } | Term::Instance { body: b, .. } => cur = b,
            Term::Effect { body: b, .. } => cur = b,
            Term::Handle(scrutinee, _) => cur = scrutinee,
            Term::TypeDef {
                name,
                params,
                variants,
                body: b,
                ..
            } => {
                let variants: Vec<SumVariant> = variants
                    .iter()
                    .enumerate()
                    .map(|(tag, (ctor, fields))| SumVariant {
                        tag: tag as i64,
                        ctor: ctor.clone(),
                        fields: fields.clone(),
                    })
                    .collect();
                out.push((name.clone(), params.clone(), TypeDefKind::Sum(variants)));
                cur = b;
            }
            _ => break,
        }
    }
    out
}

/// The storage layout of a value of an exported type, held in one managed field
/// (§2.2). A named sum / record is a single traced pointer cell (a handle); a
/// record *alias* aggregates its field layouts. This is the representation half
/// of the type contract a client lays out the same.
fn type_layout(kind: &TypeDefKind) -> ValueLayout {
    match kind {
        // A sum value is a handle to a tagged GC object — one traced pointer cell.
        TypeDefKind::Sum(_) => ValueLayout::pointer_cell(),
        TypeDefKind::Record(fields) => {
            Type::aggregate_storage_layout(fields.iter().map(|(_, t)| t))
        }
    }
}

// ── the textual `.locusi` format: serialize ↔ parse ──────────────────────

/// Serialize a [`ModuleInterface`] to its textual `.locusi` form (O-S1 — stable
/// + auditable). Grammar (one declaration per line; sections in a fixed order):
///
/// ```text
/// locusi/1
/// module <name> at <layer>
/// abi-version <n>
/// hash <hex>
/// mints (<l>, …)            -- omitted when empty
/// seals (<l>, …)            -- omitted when empty
/// type <Name>[<p>, …] = <C>(<T>, …) | <C> | …   -- one per exported type
/// val <name> : <type-with-row>                  -- one per exported value, sorted
/// ```
///
/// The `<type-with-row>` text reuses [`Type`]'s `Display` (the same pretty-printer
/// the CLI/diagnostics use), and the parser reads it back through the existing
/// type parser — so the textual type language is exactly the source one.
pub fn serialize(iface: &ModuleInterface) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{LOCUSI_FORMAT}");
    let _ = writeln!(s, "module {} at {}", iface.name, iface.layer.name());
    let _ = writeln!(s, "abi-version {}", iface.abi_version);
    let _ = writeln!(s, "hash {:016x}", iface.hash());
    if !iface.mints.is_empty() {
        let _ = writeln!(s, "mints ({})", labels_text(&iface.mints));
    }
    if !iface.seals.is_empty() {
        let _ = writeln!(s, "seals ({})", labels_text(&iface.seals));
    }
    for t in &iface.types {
        let _ = writeln!(s, "{}", type_decl_text(t));
    }
    for v in &iface.vals {
        let _ = writeln!(s, "val {} : {}", v.name, scheme_text(&v.scheme));
    }
    s
}

/// `l1, l2, …` for a label list (mints/seals), in the order written.
fn labels_text(labels: &[Label]) -> String {
    labels
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `type Name[p, …] = C1(T, …) | C2 | …` for one exported type. (A record alias
/// renders `= { f: T, … }`.) Reuses [`Type`]'s `Display` for every field type.
fn type_decl_text(t: &TypeExport) -> String {
    let mut s = String::from("type ");
    s.push_str(&t.name);
    if !t.params.is_empty() {
        s.push('[');
        s.push_str(&t.params.join(", "));
        s.push(']');
    }
    s.push_str(" = ");
    match &t.def {
        TypeDefKind::Sum(vs) => {
            for (i, v) in vs.iter().enumerate() {
                if i > 0 {
                    s.push_str(" | ");
                }
                s.push_str(&v.ctor);
                if !v.fields.is_empty() {
                    s.push('(');
                    for (j, f) in v.fields.iter().enumerate() {
                        if j > 0 {
                            s.push_str(", ");
                        }
                        let _ = write!(s, "{f}");
                    }
                    s.push(')');
                }
            }
        }
        TypeDefKind::Record(fs) => {
            s.push('{');
            for (i, (n, ty)) in fs.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                let _ = write!(s, " {n}: {ty}");
            }
            s.push_str(" }");
        }
    }
    s
}

/// The textual form of a scheme's type — what follows `val name :`. With
/// constraints present (Sprint 2 surface) it renders `C1 t1, … => <type>`; a
/// plain scheme is just the type. (Quantifiers are implicit, recovered by the
/// reader; the v1 build emits ground schemes anyway.)
fn scheme_text(scheme: &Scheme) -> String {
    if scheme.constraints.is_empty() {
        format!("{}", scheme.ty)
    } else {
        let cs = scheme
            .constraints
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{cs} => {}", scheme.ty)
    }
}

/// A `.locusi` parse failure: a line number + message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseIfaceError {
    pub line: usize,
    pub msg: String,
}

impl std::fmt::Display for ParseIfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

/// Parse a textual `.locusi` back into a [`ModuleInterface`]. The inverse of
/// [`serialize`]: a round-trip (`build → serialize → parse`) yields an equal
/// structure. Type text is read through the existing source type parser
/// ([`crate::parse`]'s type grammar, reached via [`parse_type_text`]).
pub fn parse(src: &str) -> Result<ModuleInterface, ParseIfaceError> {
    let mut name: Option<String> = None;
    let mut layer = Layer::App;
    let mut abi_version = ABI_VERSION;
    let mut mints = Vec::new();
    let mut seals = Vec::new();
    let mut vals = Vec::new();
    let mut types = Vec::new();
    // The hash line is read for validation but recomputed from the parsed
    // contract — a round-trip recomputes the same value, and a hand-edited
    // mismatch is detectable by comparing (Sprint 2's `RN-E0600` lives there).
    let mut declared_hash: Option<u64> = None;

    for (i, raw) in src.lines().enumerate() {
        let lineno = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let err = |msg: String| ParseIfaceError { line: lineno, msg };

        if line == LOCUSI_FORMAT {
            continue;
        }
        if let Some(rest) = line.strip_prefix("module ") {
            // `module <name> at <layer>`
            let (n, l) = rest
                .split_once(" at ")
                .ok_or_else(|| err("expected `module <name> at <layer>`".into()))?;
            name = Some(n.trim().to_string());
            layer = Layer::from_name(l.trim())
                .ok_or_else(|| err(format!("unknown layer `{}`", l.trim())))?;
        } else if let Some(rest) = line.strip_prefix("abi-version ") {
            abi_version = rest
                .trim()
                .parse()
                .map_err(|_| err(format!("bad abi-version `{}`", rest.trim())))?;
        } else if let Some(rest) = line.strip_prefix("hash ") {
            declared_hash = Some(
                u64::from_str_radix(rest.trim(), 16)
                    .map_err(|_| err(format!("bad hash `{}`", rest.trim())))?,
            );
        } else if let Some(rest) = line.strip_prefix("mints ") {
            mints = parse_labels(rest.trim()).map_err(&err)?;
        } else if let Some(rest) = line.strip_prefix("seals ") {
            seals = parse_labels(rest.trim()).map_err(&err)?;
        } else if let Some(rest) = line.strip_prefix("type ") {
            types.push(parse_type_decl(rest).map_err(&err)?);
        } else if let Some(rest) = line.strip_prefix("val ") {
            let (n, ty_text) = rest
                .split_once(" : ")
                .ok_or_else(|| err("expected `val <name> : <type>`".into()))?;
            let scheme = parse_scheme_text(ty_text.trim()).map_err(&err)?;
            vals.push(ValExport {
                name: n.trim().to_string(),
                scheme,
            });
        } else {
            return Err(err(format!("unrecognised interface line `{line}`")));
        }
    }

    let name = name.ok_or(ParseIfaceError {
        line: 0,
        msg: "interface has no `module` header".into(),
    })?;
    vals.sort_by(|a, b| a.name.cmp(&b.name));
    let iface = ModuleInterface {
        name,
        layer,
        mints,
        seals,
        vals,
        types,
        abi_version,
    };
    // A declared hash that disagrees with the recomputed one is a corrupted /
    // hand-edited interface. Sprint 1 surfaces it as a parse error; Sprint 2
    // maps the stale-link case to `RN-E0600`.
    if let Some(h) = declared_hash {
        if h != iface.hash() {
            return Err(ParseIfaceError {
                line: 0,
                msg: format!(
                    "interface hash mismatch: declared {h:016x}, recomputed {:016x}",
                    iface.hash()
                ),
            });
        }
    }
    Ok(iface)
}

/// `(l1, l2, …)` → the label list, using the same `row_label` mapping the source
/// parser uses for a row label (so `gc`/`mem`/`winapi`/`crt`/`asm`/a user op all
/// round-trip). The parens are required.
fn parse_labels(text: &str) -> Result<Vec<Label>, String> {
    let inner = text
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| format!("expected `(l, …)`, found `{text}`"))?;
    let inner = inner.trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|t| parse_label_text(t.trim()))
        .collect()
}

/// One effect label from its textual form — the inverse of [`Label`]'s `Display`.
/// Reuses the source parser's label reader so the spelling is identical.
fn parse_label_text(text: &str) -> Result<Label, String> {
    crate::parse::label_from_text(text).map_err(|e| e.msg)
}

/// Parse `Name[p, …] = …` (a `type` declaration's text, after the `type ` prefix).
fn parse_type_decl(rest: &str) -> Result<TypeExport, String> {
    let (head, rhs) = rest
        .split_once('=')
        .ok_or_else(|| "expected `=` in a type declaration".to_string())?;
    let head = head.trim();
    let (name, params) = match head.split_once('[') {
        Some((n, ps)) => {
            let ps = ps
                .strip_suffix(']')
                .ok_or_else(|| format!("unterminated type params in `{head}`"))?;
            let params = ps
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            (n.trim().to_string(), params)
        }
        None => (head.to_string(), Vec::new()),
    };
    let rhs = rhs.trim();
    let def = if rhs.starts_with('{') {
        // A record alias `{ f: T, … }` — parse via the type grammar then unwrap.
        match parse_type_text(rhs)? {
            Type::Record(fs) => TypeDefKind::Record(fs),
            other => return Err(format!("expected a record after `=`, found `{other}`")),
        }
    } else {
        let mut variants = Vec::new();
        for (tag, alt) in rhs.split('|').enumerate() {
            variants.push(parse_variant(alt.trim(), tag as i64)?);
        }
        TypeDefKind::Sum(variants)
    };
    let layout = type_layout(&def);
    Ok(TypeExport {
        name,
        params,
        def,
        layout,
    })
}

/// One sum variant `C` or `C(T, …)`.
fn parse_variant(text: &str, tag: i64) -> Result<SumVariant, String> {
    match text.split_once('(') {
        Some((ctor, args)) => {
            let args = args
                .strip_suffix(')')
                .ok_or_else(|| format!("unterminated constructor fields in `{text}`"))?;
            let fields = split_top_level(args)
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .map(|s| parse_type_text(s.trim()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SumVariant {
                tag,
                ctor: ctor.trim().to_string(),
                fields,
            })
        }
        None => Ok(SumVariant {
            tag,
            ctor: text.trim().to_string(),
            fields: Vec::new(),
        }),
    }
}

/// Split a constructor's field list on **top-level** commas — commas inside a
/// nested `[...]`/`(...)`/`{...}` (e.g. `Pair[Int, Bool]`) do not separate fields.
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(s[start..].to_string());
    parts
}

/// Parse a value signature's text — an optional `C1 t1, … =>` constraint context
/// followed by the type. The inverse of [`scheme_text`]. Quantifiers stay
/// implicit (the v1 surface emits ground schemes); the constraints, when present,
/// are recovered so a future qualified export round-trips.
fn parse_scheme_text(text: &str) -> Result<Scheme, String> {
    if let Some((ctx, ty_text)) = split_constraint_arrow(text) {
        let constraints = ctx
            .split(',')
            .map(|c| parse_constraint_text(c.trim()))
            .collect::<Result<Vec<_>, _>>()?;
        let ty = parse_type_text(ty_text.trim())?;
        Ok(Scheme {
            ty_vars: Vec::new(),
            row_vars: Vec::new(),
            constraints,
            ty,
        })
    } else {
        Ok(Scheme::mono(parse_type_text(text)?))
    }
}

/// Split `C̄ => τ` at the **constraint** `=>`, distinguishing it from a value-type
/// `=>` (there is none — Locus types use `->`, not `=>`), so the first top-level
/// `=>` is the constraint separator. Returns `None` for an unqualified type.
fn split_constraint_arrow(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match bytes[i] {
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => depth -= 1,
            b'=' if depth == 0 && bytes[i + 1] == b'>' => {
                return Some((&text[..i], &text[i + 2..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// One constraint `Trait τ` (the inverse of [`crate::syntax::Constraint`]'s
/// `Display`): the first whitespace-separated token is the trait name, the rest
/// is the constrained type.
fn parse_constraint_text(text: &str) -> Result<crate::syntax::Constraint, String> {
    let (trait_name, ty_text) = text
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("expected `Trait <type>` in constraint `{text}`"))?;
    Ok(crate::syntax::Constraint {
        trait_name: trait_name.trim().to_string(),
        ty: parse_type_text(ty_text.trim())?,
    })
}

/// Read one [`Type`] (possibly with effect rows on its arrows) from text, through
/// the source type grammar. The single bridge the `.locusi` parser uses for every
/// type it reads — keeping the interface type language identical to the source.
fn parse_type_text(text: &str) -> Result<Type, String> {
    crate::parse::type_from_text(text).map_err(|e| e.msg)
}

// ── consuming an interface: cross-module type-check (Sprint 2) ───────────

/// A loaded, **validated** interface — the result of taking a `.locusi` (or an
/// in-memory [`ModuleInterface`]) across the *client-load boundary* and checking
/// the two cheap things a client must trust before it consumes the contract: the
/// ABI/representation version (`RN-E0603`) and — for a textual interface — the
/// declared-vs-recomputed hash (`RN-E0600`). Holding the validated interface in a
/// distinct type makes "this was checked at load" a type-level fact the consume
/// path relies on (`separate-compilation.md` §3, §6, §8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoadedInterface {
    iface: ModuleInterface,
}

impl LoadedInterface {
    /// Take an **in-memory** [`ModuleInterface`] across the client-load boundary,
    /// running the ABI-version check (`RN-E0603`). (An in-memory interface — built
    /// by [`interface_of`] — has no declared hash to re-validate; the textual path
    /// [`load`](LoadedInterface::load) is where `RN-E0600` lives.) The gate's tests
    /// load interfaces this way so they do not depend on a file layout.
    pub fn accept(iface: ModuleInterface) -> Result<LoadedInterface, ConsumeError> {
        if iface.abi_version != ABI_VERSION {
            return Err(ConsumeError::AbiVersion {
                module: iface.name,
                found: iface.abi_version,
                expected: ABI_VERSION,
            });
        }
        Ok(LoadedInterface { iface })
    }

    /// Load a **textual** `.locusi` across the client-load boundary: parse it
    /// ([`parse`], which already detects a declared-vs-recomputed hash mismatch and
    /// an unparseable body), then run the ABI-version check. A hash mismatch maps to
    /// `RN-E0600` (`module.stale-interface`) — a client built against an interface
    /// that no longer matches the producer (a corrupted / hand-edited / out-of-date
    /// `.locusi`); any other parse failure is reported verbatim.
    pub fn load(src: &str) -> Result<LoadedInterface, ConsumeError> {
        let iface = parse(src).map_err(|e| {
            // `parse` reports the hash mismatch on line 0 with a recognisable
            // message; surface that as the stale-interface code, everything else as
            // a generic malformed-interface load error.
            if e.line == 0 && e.msg.starts_with("interface hash mismatch") {
                ConsumeError::StaleInterface { detail: e.msg }
            } else {
                ConsumeError::Malformed {
                    detail: e.to_string(),
                }
            }
        })?;
        LoadedInterface::accept(iface)
    }

    /// The validated interface (read-only — it was checked at construction).
    pub fn interface(&self) -> &ModuleInterface {
        &self.iface
    }
}

/// What can go wrong **consuming** an interface (the `RN-E06xx` "module / link"
/// family, `separate-compilation.md` §8). A sibling of [`crate::check::TypeErr`]
/// with the same `code`/`slug`/`spec`/`hint` shape, so the driver renders a
/// consume diagnostic exactly like a type diagnostic. (A type error *inside* the
/// client body — once it is checked against the interfaces — is the ordinary
/// [`TypeErr`](crate::check::TypeErr), wrapped by [`ConsumeError::Body`].)
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConsumeError {
    /// **`RN-E0601` `module.missing-export`.** The client imports / uses a name
    /// that the named module's interface does **not** export.
    MissingExport { module: String, name: String },
    /// **`RN-E0601` `module.missing-export`.** The client `import X`s a module for
    /// which no interface was loaded — from the client's view, a module that exports
    /// nothing it can use. (The same code as a missing name: the cure is the same —
    /// supply / fix the producer's interface.)
    UnknownModule { module: String },
    /// **`RN-E0603` `module.abi-version`.** An interface built under an
    /// incompatible [`ABI_VERSION`] — rejected at load rather than mis-linked.
    AbiVersion {
        module: String,
        found: u32,
        expected: u32,
    },
    /// **`RN-E0600` `module.stale-interface`.** A textual interface whose declared
    /// hash disagrees with the contract it describes (corrupted / hand-edited /
    /// out-of-date) — surfaced at the client-load boundary.
    StaleInterface { detail: String },
    /// **`RN-E0604` `module.import-cycle`.** A cross-module import cycle (imports
    /// must be acyclic). *Sprint 2b* — see [`check_client_against`]; only the
    /// trivial self-import is detected in Sprint 2.
    ImportCycle { module: String },
    /// A malformed interface that failed to parse for a reason other than a hash
    /// mismatch (no dedicated `RN-E06xx`; reuses the stale-interface slug as the
    /// nearest "this interface cannot be trusted" code).
    Malformed { detail: String },
    /// The client body did not type-check against the interfaces — the ordinary
    /// [`TypeErr`](crate::check::TypeErr), carried with its own `RN-Exxxx` code +
    /// message so the driver prints the real type error (an `Unbound` here means a
    /// name neither imported nor stdlib — the front line before `RN-E0601`).
    Body { code: String, message: String },
}

impl ConsumeError {
    /// The stable `RN-Exxxx` code (`separate-compilation.md` §8). For a wrapped
    /// body error this is the underlying type error's own code.
    pub fn code(&self) -> &str {
        match self {
            ConsumeError::MissingExport { .. } | ConsumeError::UnknownModule { .. } => "RN-E0601",
            ConsumeError::AbiVersion { .. } => "RN-E0603",
            ConsumeError::StaleInterface { .. } | ConsumeError::Malformed { .. } => "RN-E0600",
            ConsumeError::ImportCycle { .. } => "RN-E0604",
            ConsumeError::Body { code, .. } => code,
        }
    }

    /// The catalog **slug** (`module.*`), paired with [`code`](ConsumeError::code).
    pub fn slug(&self) -> &'static str {
        match self {
            ConsumeError::MissingExport { .. } | ConsumeError::UnknownModule { .. } => {
                "module.missing-export"
            }
            ConsumeError::AbiVersion { .. } => "module.abi-version",
            ConsumeError::StaleInterface { .. } | ConsumeError::Malformed { .. } => {
                "module.stale-interface"
            }
            ConsumeError::ImportCycle { .. } => "module.import-cycle",
            ConsumeError::Body { .. } => "type.error",
        }
    }

    /// The design section that defines this rule (spec-citing, design §8).
    pub fn spec(&self) -> &'static str {
        match self {
            ConsumeError::Body { .. } => "separate-compilation §4a (the client body)",
            _ => "separate-compilation §8 (module / link diagnostics)",
        }
    }

    /// A suggested next step, when there is an obvious one.
    pub fn hint(&self) -> Option<String> {
        match self {
            ConsumeError::MissingExport { module, name } => Some(format!(
                "module `{module}` does not export `{name}` — check the spelling, or add `{name}` \
                 to `{module}`'s `exposing (…)` and rebuild its interface"
            )),
            ConsumeError::UnknownModule { module } => Some(format!(
                "no interface loaded for `{module}` — pass its `.locusi` (e.g. `--iface \
                 {module}.locusi`) so the client can be checked against it"
            )),
            ConsumeError::AbiVersion {
                module, expected, ..
            } => Some(format!(
                "rebuild `{module}` with this compiler — its interface targets ABI version \
                 {expected}"
            )),
            ConsumeError::StaleInterface { .. } => Some(
                "the interface no longer matches the producer it was built from — rebuild the \
                 producer and recompile against the fresh `.locusi`"
                    .into(),
            ),
            ConsumeError::ImportCycle { module } => Some(format!(
                "`{module}` (transitively) imports itself — imports must be acyclic; break the \
                 cycle"
            )),
            ConsumeError::Malformed { .. } => Some(
                "the `.locusi` is corrupted — regenerate it with `locus emit-interface`".into(),
            ),
            ConsumeError::Body { .. } => None,
        }
    }
}

impl std::fmt::Display for ConsumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsumeError::MissingExport { module, name } => {
                write!(f, "module `{module}` does not export `{name}`")
            }
            ConsumeError::UnknownModule { module } => {
                write!(f, "no interface loaded for imported module `{module}`")
            }
            ConsumeError::AbiVersion {
                module,
                found,
                expected,
            } => write!(
                f,
                "interface for `{module}` was built under ABI version {found}, but this compiler \
                 is ABI version {expected}"
            ),
            ConsumeError::StaleInterface { detail } => write!(f, "stale interface: {detail}"),
            ConsumeError::ImportCycle { module } => {
                write!(f, "import cycle through module `{module}`")
            }
            ConsumeError::Malformed { detail } => write!(f, "malformed interface: {detail}"),
            ConsumeError::Body { code, message } => write!(f, "{code} {message}"),
        }
    }
}

/// One **import request**: a client `import <module> (names…)`. Today's surface
/// `import X` carries no name list, so the driver requests *every* export of the
/// resolved interface (`names = None`); the testable API also accepts an explicit
/// `Some(names)` to exercise the `RN-E0601` missing-export check directly. A
/// requested name that the named interface does not export is the missing-export
/// case (`separate-compilation.md` §8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Import {
    pub module: String,
    /// `None` = "all the module's exports" (the `import X` surface); `Some(list)` =
    /// an explicit name list to check against the interface (each must be exported).
    pub names: Option<Vec<String>>,
}

impl Import {
    /// `import <module>` — pull in everything the module exports (the surface form).
    pub fn all(module: impl Into<String>) -> Import {
        Import {
            module: module.into(),
            names: None,
        }
    }
}

/// **Resolve a client's imports against the loaded interfaces**, returning the
/// values + types it brings into scope. Fires `RN-E0601` for a requested name the
/// named interface does not export, and (the trivial Sprint-2 case) `RN-E0604` for
/// a self-import. The producer **source is never read** — only its interface
/// (`separate-compilation.md` §3).
///
/// "Brings into scope": the union of the requested [`ValExport`]s (or all of them
/// when `names = None`) and **all** the [`TypeExport`]s of each imported module —
/// a type export's constructors are needed to construct/match even when only a
/// function is named, so types come in wholesale with their module.
fn resolve_imports<'a>(
    imports: &[Import],
    ifaces: &'a [LoadedInterface],
    client_module: Option<&str>,
) -> Result<(Vec<&'a ValExport>, Vec<&'a TypeExport>), ConsumeError> {
    let mut vals: Vec<&ValExport> = Vec::new();
    let mut types: Vec<&TypeExport> = Vec::new();
    for imp in imports {
        // Sprint 2: only the trivial self-cycle is checked (a module importing
        // itself). The transitive import-graph cycle check is Sprint 2b — see the
        // module-level note — because it needs the whole interface set keyed by
        // name + a DFS the single-client driver does not yet thread.
        if Some(imp.module.as_str()) == client_module {
            return Err(ConsumeError::ImportCycle {
                module: imp.module.clone(),
            });
        }
        let loaded = ifaces
            .iter()
            .find(|l| l.iface.name == imp.module)
            .ok_or_else(|| ConsumeError::UnknownModule {
                // An `import X` with no interface for `X`: nothing to check against.
                module: imp.module.clone(),
            })?;
        let iface = &loaded.iface;
        // Names this module actually exports: its value names + its type names +
        // every constructor of an exported sum.
        let exports_name = |n: &str| -> bool {
            iface.vals.iter().any(|v| v.name == n)
                || iface.types.iter().any(|t| {
                    t.name == n
                        || matches!(&t.def, TypeDefKind::Sum(vs) if vs.iter().any(|v| v.ctor == n))
                })
        };
        match &imp.names {
            Some(names) => {
                for n in names {
                    if !exports_name(n) {
                        return Err(ConsumeError::MissingExport {
                            module: imp.module.clone(),
                            name: n.clone(),
                        });
                    }
                }
                // Only the named *values* are seeded (a type/ctor name pulls in its
                // whole type below); a named function comes in by itself.
                for n in names {
                    if let Some(v) = iface.vals.iter().find(|v| &v.name == n) {
                        vals.push(v);
                    }
                }
            }
            None => vals.extend(iface.vals.iter()),
        }
        // Types come in wholesale with their module — a client needs the
        // constructors to build/match even when it named only a function.
        types.extend(iface.types.iter());
    }
    Ok((vals, types))
}

/// Reconstruct a [`Term::TypeDef`] **chain** from imported [`TypeExport`]s, wrapped
/// around `inner` (the client entry). Each sum export becomes the `type Name[..] =
/// C(..) | …` the stdlib graft would have produced — putting the producer's type
/// name + constructors in scope so the client can construct / match, with the
/// layout re-derived locally (Sprint 1 verified `Type::Named → pointer_cell`, so it
/// matches the producer's). Record exports are not a v1 surface (no record-`type`),
/// so only sums are reconstructed; a record export would need the surface to grow
/// first. The declaring `module` is stamped so the orphan check stays well-formed.
fn graft_imported_types(types: &[&TypeExport], module: Option<&str>, inner: Term) -> Term {
    let mut result = inner;
    // Innermost-out, so the first type ends up outermost (declaration order is not
    // load-bearing here — type names are mutually visible across the chain).
    for t in types.iter().rev() {
        if let TypeDefKind::Sum(vs) = &t.def {
            let variants: Vec<(String, Vec<Type>)> = vs
                .iter()
                .map(|v| (v.ctor.clone(), v.fields.clone()))
                .collect();
            result = Term::TypeDef {
                name: t.name.clone(),
                params: t.params.clone(),
                variants,
                module: module.map(|s| s.to_string()),
                body: Box::new(result),
            };
        }
    }
    result
}

/// Seed a [`Ctx`] (Γ) with one **bodyless** [`Binding::Poly`] per imported value,
/// at stage 0. The producer's published scheme — *including its effect row* — is
/// now an in-scope name with no body. When the client calls it, the row on the
/// scheme's arrow propagates into the client's row through the ordinary (app) rule
/// (`ft.row ∪ at.row ∪ latent`, [`crate::sema`]) — **transparency falls out of
/// inference**, with no special-casing (`separate-compilation.md` §4a). A mono
/// scheme and a `Poly` scheme are seeded identically (`Binding::Poly` handles
/// both); polymorphic cross-module imports are *desirable, not blocking* — see the
/// generalized-scheme note on [`check_client_against`].
fn seed_value_ctx(vals: &[&ValExport]) -> Ctx {
    let mut ctx = Ctx::new();
    for v in vals {
        ctx.insert(v.name.clone(), (Binding::Poly(v.scheme.clone()), 0));
    }
    ctx
}

/// **The consume path** — type-check a client **against interfaces only**, never
/// the producer source (`separate-compilation.md` §3). Deterministic, no file IO:
/// the gate calls this directly with in-memory interfaces (built via
/// [`interface_of`]).
///
/// `client_src` is the client *expression* (the program entry — a bare expression,
/// or one wrapping `import` lines the parser already strips into
/// [`ProgramSource::imports`](crate::syntax::ProgramSource)); `imports` says which
/// modules + names it pulls in; `ifaces` are the loaded (ABI/hash-validated)
/// producer interfaces. Returns the elaborated [`Typed`] tree — whose **root row
/// carries every effect the producer functions publish** (§4a transparency).
///
/// How it works (reusing the existing machinery — no new `Term`/`Node` variants):
/// 1. **Resolve** the imports → the in-scope values + types ([`resolve_imports`];
///    `RN-E0601` on a missing export).
/// 2. **Graft** a [`Term::TypeDef`] chain for the imported types around the client
///    entry ([`graft_imported_types`]) — their layouts re-derive locally.
/// 3. **Seed** a [`Ctx`] with a bodyless [`Binding::Poly`] per imported value
///    ([`seed_value_ctx`]).
/// 4. **Elaborate** the grafted client in that seeded context — ordinary inference;
///    the producer's row reaches the client's row through (app).
///
/// **Generalized-scheme outcome (the IMPORTANT note).** The interface today carries
/// each export as a `Scheme::mono` (D6 defaults any residual `a`→Int *before*
/// `interface_of` reads the post-zonk binding type), so a genuinely generic export
/// crosses **monomorphized**. Capturing the *generalized* `Poly` scheme needs sema
/// to record the `let`-binding scheme on its `Node::Let` (it is computed but kept
/// only in the body's context, not the node) — deep elaboration surgery — so per
/// the sprint ruling it is **deferred to Sprint 2b ("polymorphic cross-module
/// imports")**. The consume mechanism is scheme-agnostic: [`seed_value_ctx`] seeds
/// whatever scheme the interface carries, so when 2b makes `interface_of` publish a
/// `Poly`, this path consumes it unchanged.
pub fn check_client_against(
    client_src: &str,
    imports: &[Import],
    ifaces: &[LoadedInterface],
) -> Result<Typed, ConsumeError> {
    check_client_against_with_imports(client_src, imports, ifaces).map(|(typed, _)| typed)
}

/// One **imported value** the client may call across the link (Sprint 3 codegen):
/// the producer's value `name`, the [`mangle_export`] symbol the producer emitted
/// it under, and the export's arity (the flat uniform-ABI parameter count). The
/// backend seeds these into the IR lowering's extern map so a fully-applied call
/// to `name` collapses to one direct external call to `symbol` — exactly the
/// `Comp::Foreign` path an `extern` already uses, *without* the capability mint.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImportedSymbol {
    pub name: String,
    pub symbol: String,
    pub arity: usize,
}

/// **The consume path, with the cross-module call table** — like
/// [`check_client_against`], but also returns the [`ImportedSymbol`]s the client
/// brings in (Sprint 3). The type-check is identical (the imports are seeded as
/// bodyless schemes); the extra table tells the *backend* how to turn a call to an
/// imported name into a direct external call to the producer's mangled symbol. The
/// arity is read from the imported value's published arrow type (the number of
/// `->`s a first-order function export carries) — exactly the producer's flat
/// symbol arity, so the client `Foreign` and the producer definition agree.
///
/// Only **function** values get an [`ImportedSymbol`]: a non-arrow (value) import
/// is the deferred value-export case (no flat symbol to call), so it is omitted
/// from the table — its signature still type-checks, only its cross-module *call*
/// waits for the value-export follow-on.
pub fn check_client_against_with_imports(
    client_src: &str,
    imports: &[Import],
    ifaces: &[LoadedInterface],
) -> Result<(Typed, Vec<ImportedSymbol>), ConsumeError> {
    let prog = crate::parse::parse_program(client_src).map_err(|e| ConsumeError::Malformed {
        detail: format!("client: {}", e.msg),
    })?;
    let client_module = prog.modules.first().map(|m| m.name.clone());
    let (vals, types) = resolve_imports(imports, ifaces, client_module.as_deref())?;

    // Build the cross-module call table from the resolved value imports: a
    // function export (its scheme's type is an arrow) becomes an `ImportedSymbol`
    // under its producer's mangled name. We need each value's *home module* to
    // mangle it; recover it by finding the interface that exports the name.
    let mut symbols = Vec::new();
    for v in &vals {
        let arity = arrow_arity(&v.scheme.ty);
        if arity == 0 {
            continue; // a value import — the deferred case; no flat symbol
        }
        // The owning module is the loaded interface that exports this value.
        if let Some(home) = ifaces
            .iter()
            .find(|l| l.interface().vals.iter().any(|e| e.name == v.name))
        {
            symbols.push(ImportedSymbol {
                name: v.name.clone(),
                symbol: mangle_export(&home.interface().name, &v.name),
                arity,
            });
        }
    }

    let grafted = graft_imported_types(&types, client_module.as_deref(), prog.entry);
    let ctx = seed_value_ctx(&vals);
    let typed =
        elaborate(&crate::prelude::sig(), &ctx, 0, &grafted).map_err(|e| ConsumeError::Body {
            code: e.code().to_string(),
            message: e.to_string(),
        })?;
    Ok((typed, symbols))
}

/// The arity of a function type — the number of `->`s along its spine (a nullary
/// `Unit -> T` counts as 1, matching the flat-ABI parameter count). A non-arrow
/// type is arity 0 (a value, not a function).
fn arrow_arity(ty: &Type) -> usize {
    let mut n = 0;
    let mut t = ty;
    while let Type::Fun(_, b, _) = t {
        n += 1;
        t = b;
    }
    n
}

// ── FNV-1a (64-bit) — a self-contained, stable string/byte hash ──────────

/// FNV-1a, 64-bit. Tiny and dependency-free (the core crate is zero-dep by
/// design); deterministic across runs and platforms, which is what the interface
/// hash needs (a `DefaultHasher` is explicitly *not* guaranteed stable).
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }
    fn str(&mut self, s: &str) -> &mut Fnv {
        for b in s.as_bytes() {
            self.byte(*b);
        }
        // A separator so `ab`+`c` and `a`+`bc` hash differently.
        self.byte(0xff);
        self
    }
    fn u64(&mut self, n: u64) -> &mut Fnv {
        for b in n.to_le_bytes() {
            self.byte(b);
        }
        self
    }
    fn layout(&mut self, l: ValueLayout) -> &mut Fnv {
        self.u64(l.pointer_cells as u64)
            .u64(l.scalar_cells as u64)
            .u64(l.byte_width as u64)
            .u64(l.align as u64)
            .byte(l.known as u8);
        self.byte(l.word as u8);
        self
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{Label, Layer};

    /// A sample library module: a `Box[a]` sum, a `mapBox` carrying an effect row
    /// (`{gc}` — it allocates), and an `unbox`. Exposes the type + two functions.
    const BOX_LIB: &str = r#"module Data.Box at services exposing (Box, unbox, mapBox) =
        type Box[a] = Box(a) in
        let unbox = fn b: Box[Int] => match b with | Box(x) => x in
        let mapBox = fn b: Box[Int] => Box(b) in
        ()
        ()"#;

    fn build(src: &str) -> ModuleInterface {
        let prog = crate::parse::parse_program(src).expect("sample parses");
        assert_eq!(prog.modules.len(), 1, "sample is a single module");
        let module = prog.modules[0].clone();
        let grafted = crate::stdlib::program(src).expect("sample grafts + parses");
        // Run on a generous stack — elaboration recurses over the graft.
        std::thread::Builder::new()
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(move || interface_of(&module, &grafted).expect("sample builds an interface"))
            .expect("spawn")
            .join()
            .expect("interface worker")
    }

    #[test]
    fn round_trip_yields_an_equal_structure() {
        let iface = build(BOX_LIB);
        let text = serialize(&iface);
        assert!(!text.is_empty(), "serialized interface is non-empty");
        let parsed = parse(&text).expect("the serialized interface re-parses");
        assert_eq!(parsed, iface, "build → serialize → parse round-trips");
        // Same exports, layer/seals, hash all preserved.
        assert_eq!(parsed.hash(), iface.hash());
        assert_eq!(parsed.layer, Layer::Services);
        assert_eq!(parsed.name, "Data.Box");
    }

    #[test]
    fn the_effect_row_survives_the_round_trip() {
        let iface = build(BOX_LIB);
        // `mapBox` allocates a `Box`, so its type carries a `{gc}` row.
        let map_box = iface
            .vals
            .iter()
            .find(|v| v.name == "mapBox")
            .expect("mapBox is exported");
        let text = format!("{}", map_box.scheme.ty);
        assert!(
            text.contains("{gc}"),
            "mapBox publishes its {{gc}} row: {text}"
        );
        // And it is exactly preserved through serialize → parse.
        let parsed = parse(&serialize(&iface)).expect("re-parses");
        let parsed_map = parsed.vals.iter().find(|v| v.name == "mapBox").unwrap();
        assert_eq!(parsed_map.scheme.ty, map_box.scheme.ty);
    }

    #[test]
    fn the_box_type_export_carries_its_constructor() {
        let iface = build(BOX_LIB);
        let box_ty = iface
            .types
            .iter()
            .find(|t| t.name == "Box")
            .expect("Box type is exported");
        assert_eq!(box_ty.params, vec!["a".to_string()]);
        match &box_ty.def {
            TypeDefKind::Sum(vs) => {
                assert_eq!(vs.len(), 1);
                assert_eq!(vs[0].ctor, "Box");
                assert_eq!(vs[0].tag, 0);
                assert_eq!(vs[0].fields.len(), 1, "Box(a) has one field");
            }
            other => panic!("expected a sum, got {other:?}"),
        }
        // A sum is a single traced pointer cell (a handle).
        assert!(box_ty.layout.is_single_pointer_cell());
        // The constructor is NOT also listed as a bare val.
        assert!(
            !iface.vals.iter().any(|v| v.name == "Box"),
            "the `Box` ctor is part of the type export, not a standalone val"
        );
    }

    #[test]
    fn the_hash_is_stable_across_two_identical_builds() {
        let a = build(BOX_LIB);
        let b = build(BOX_LIB);
        assert_eq!(a.hash(), b.hash(), "identical input ⇒ identical hash");
    }

    #[test]
    fn the_hash_changes_when_an_exported_signature_changes() {
        let base = build(BOX_LIB);
        // A variant whose `unbox` returns `Box[Int]` instead of `Int` — a
        // different exported signature, hence a different contract hash.
        let changed_src = r#"module Data.Box at services exposing (Box, unbox, mapBox) =
            type Box[a] = Box(a) in
            let unbox = fn b: Box[Int] => b in
            let mapBox = fn b: Box[Int] => Box(b) in
            ()
            ()"#;
        let changed = build(changed_src);
        assert_ne!(
            base.hash(),
            changed.hash(),
            "a changed exported signature changes the hash"
        );
    }

    #[test]
    fn exposing_omitted_exports_every_top_level_binding() {
        // No `exposing` ⇒ all top-level bindings (and the type) are exported.
        let src = r#"module M at app =
            type Pair2 = Mk(Int, Int) in
            let fst2 = fn p: Pair2 => match p with | Mk(a, b) => a in
            ()
            ()"#;
        let iface = build(src);
        assert!(iface.vals.iter().any(|v| v.name == "fst2"));
        assert!(iface.types.iter().any(|t| t.name == "Pair2"));
    }

    #[test]
    fn seals_and_mints_round_trip() {
        // A boundary module that mints + seals — the §5 labels are recorded and
        // re-read. (It exposes only a pure binding, so no seal-leak concern here;
        // this exercises the interface header, not the seal check.)
        let iface = ModuleInterface {
            name: "Kernel.Bits".into(),
            layer: Layer::Boundary,
            mints: vec![Label::World("asm".into())],
            seals: vec![Label::World("asm".into())],
            vals: vec![ValExport {
                name: "pure_helper".into(),
                scheme: Scheme::mono(Type::Fun(
                    Box::new(Type::Int),
                    Box::new(Type::Int),
                    crate::syntax::Row::pure(),
                )),
            }],
            types: Vec::new(),
            abi_version: ABI_VERSION,
        };
        let parsed = parse(&serialize(&iface)).expect("re-parses");
        assert_eq!(parsed, iface);
        assert_eq!(parsed.mints, vec![Label::World("asm".into())]);
        assert_eq!(parsed.seals, vec![Label::World("asm".into())]);
    }

    // ── Sprint 2: consume — cross-module type-check ──────────────────────

    /// Run `f` on the large pipeline stack (consume elaborates the grafted
    /// client) and return its result.
    fn on_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        std::thread::Builder::new()
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(f)
            .expect("spawn")
            .join()
            .expect("consume worker")
    }

    #[test]
    fn a_client_type_checks_against_an_interface_without_the_producer_source() {
        // Producer: build `Data.Box`'s interface in memory (no source crosses to
        // the client). `mapBox : Box[Int] -> Box[Int] ! {gc}` (it allocates).
        let iface = build(BOX_LIB);
        let loaded = LoadedInterface::accept(iface).expect("ABI ok");
        // Client: imports `Data.Box`, constructs a `Box` and maps it — calling the
        // bodyless producer function. Its source never mentions the producer body.
        let client = r#"import Data.Box
            mapBox(Box(1))"#;
        let typed =
            on_stack(move || check_client_against(client, &[Import::all("Data.Box")], &[loaded]))
                .expect("client type-checks against the interface alone");
        // The result is a `Box[..]`, and — the §4a gate — the producer's `{gc}` row
        // surfaces in the client's inferred row. The client never performed `gc`
        // itself; it inherited it through the call, from the interface.
        let row = format!("{}", typed.row);
        assert!(
            row.contains("gc"),
            "the producer's {{gc}} row must propagate into the client's row (§4a), got `{row}`"
        );
    }

    #[test]
    fn rn_e0601_fires_for_a_name_the_interface_does_not_export() {
        let iface = build(BOX_LIB);
        let loaded = LoadedInterface::accept(iface).expect("ABI ok");
        // `Data.Box` exposes (Box, unbox, mapBox) — not `repackage`.
        let imp = Import {
            module: "Data.Box".into(),
            names: Some(vec!["repackage".into()]),
        };
        let err = on_stack(move || check_client_against("()", &[imp], &[loaded]))
            .expect_err("a missing import must be rejected");
        assert_eq!(err.code(), "RN-E0601");
        assert!(matches!(
            err,
            ConsumeError::MissingExport { ref module, ref name }
                if module == "Data.Box" && name == "repackage"
        ));
    }

    #[test]
    fn rn_e0603_fires_for_a_bad_abi_version() {
        let mut iface = build(BOX_LIB);
        iface.abi_version = ABI_VERSION + 1; // ABI skew
        let err = LoadedInterface::accept(iface).expect_err("an ABI-skewed interface is rejected");
        assert_eq!(err.code(), "RN-E0603");
        assert!(
            matches!(err, ConsumeError::AbiVersion { found, expected, .. }
            if found == ABI_VERSION + 1 && expected == ABI_VERSION)
        );
    }

    #[test]
    fn rn_e0600_fires_on_a_hash_mismatched_interface() {
        // A hand-edited textual interface: corrupt the declared hash line. `load`
        // re-reads it, recomputes the contract hash, and rejects the mismatch.
        let text = serialize(&build(BOX_LIB));
        let corrupted = text
            .lines()
            .map(|l| {
                if l.starts_with("hash ") {
                    "hash 0000000000000000"
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let err = LoadedInterface::load(&corrupted).expect_err("a stale interface is rejected");
        assert_eq!(err.code(), "RN-E0600");
        assert!(matches!(err, ConsumeError::StaleInterface { .. }));
    }

    #[test]
    fn a_textual_interface_round_trips_through_the_load_boundary() {
        // The happy path of `load`: serialize → load → the same contract, ABI ok.
        let iface = build(BOX_LIB);
        let text = serialize(&iface);
        let loaded = LoadedInterface::load(&text).expect("a fresh interface loads");
        assert_eq!(loaded.interface(), &iface);
    }

    #[test]
    fn the_consume_path_seeds_whatever_scheme_the_interface_carries() {
        // The generalized-scheme note in code form: a hand-built interface carrying
        // an effectful function is consumed and its row reaches the client — the
        // mechanism is scheme-agnostic (it would seed a `Poly` identically). A `{fs}`
        // producer effect (a `World` label) surfaces in the client's row.
        let fs = Label::World("fs".into());
        let iface = ModuleInterface {
            name: "Data.Io".into(),
            layer: Layer::Services,
            mints: Vec::new(),
            seals: Vec::new(),
            vals: vec![ValExport {
                name: "readAll".into(),
                scheme: Scheme::mono(Type::Fun(
                    Box::new(Type::Str),
                    Box::new(Type::Str),
                    crate::syntax::Row::single(fs.clone()),
                )),
            }],
            types: Vec::new(),
            abi_version: ABI_VERSION,
        };
        let loaded = LoadedInterface::accept(iface).expect("ABI ok");
        let client = r#"import Data.Io
            readAll("path")"#;
        let typed =
            on_stack(move || check_client_against(client, &[Import::all("Data.Io")], &[loaded]))
                .expect("client type-checks");
        assert_eq!(typed.ty, Type::Str);
        let row = format!("{}", typed.row);
        assert!(
            row.contains("fs"),
            "the producer's {{fs}} row propagates (§4a), got `{row}`"
        );
    }

    #[test]
    fn a_self_import_is_the_trivial_cycle_case() {
        // Sprint 2's only cycle check: a module that imports itself.
        let iface = build(BOX_LIB);
        let loaded = LoadedInterface::accept(iface).expect("ABI ok");
        let client = r#"module Data.Box at services =
            ()
            import Data.Box
            ()"#;
        let err =
            on_stack(move || check_client_against(client, &[Import::all("Data.Box")], &[loaded]))
                .expect_err("a self-import is a cycle");
        assert_eq!(err.code(), "RN-E0604");
    }

    // ── Sprint 3: the cross-module codegen seam (mangling + import table) ────

    /// A producer source for the link gate — a first-order `add3` exporting the
    /// flat uniform-ABI symbol the client calls. Built the same way `interface_of`
    /// is (graft, elaborate, uncurry).
    const MATH_LIB: &str = "module Data.Math at services exposing (add3) =\n\
        let add3 = fn a: Int => fn b: Int => a + b in\n\
        ()\n\
        ()";

    fn build_exports(src: &str) -> Vec<ExportedFn> {
        let src = src.to_string();
        std::thread::Builder::new()
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let prog = crate::parse::parse_program(&src).expect("parses");
                let module = prog.modules[0].clone();
                let grafted = crate::stdlib::program(&src).expect("grafts");
                exported_functions(&module, &grafted).expect("exports")
            })
            .expect("spawn")
            .join()
            .expect("export worker")
    }

    #[test]
    fn mangle_export_is_a_stable_legal_identifier() {
        // The one mangling both sides share: dots → underscores, `locus__M__name`.
        assert_eq!(mangle_export("Data.Math", "add3"), "locus__Data_Math__add3");
        assert_eq!(mangle_export("M", "f"), "locus__M__f");
    }

    #[test]
    fn the_producer_uncurries_an_exported_function_to_a_flat_symbol() {
        // `add3 : Int -> Int -> Int = fn a => fn b => a + b` becomes ONE flat,
        // binary export under the mangled symbol — the producer side of the ABI.
        let exports = build_exports(MATH_LIB);
        assert_eq!(exports.len(), 1, "one exported function");
        let add3 = &exports[0];
        assert_eq!(add3.name, "add3");
        assert_eq!(add3.mangled, mangle_export("Data.Math", "add3"));
        assert_eq!(add3.arity(), 2, "uncurried to two i64 params");
        // The body is the innermost `a + b` (the lambda chain was peeled off).
        assert!(matches!(
            add3.body.node,
            Node::Bin(crate::syntax::BinOp::Add, _, _)
        ));
        // The client declares it under the uniform all-i64 ABI of that arity.
        let abi = add3.client_abi();
        assert_eq!(abi.params, vec![crate::syntax::Width::W64; 2]);
        assert_eq!(abi.ret, crate::syntax::Width::W64);
    }

    #[test]
    fn a_higher_order_export_is_cleanly_refused() {
        // `mk : Int -> Int -> Int` but its body is ONE lambda returning the closure
        // `add a` — the syntactic lambda depth (1) is less than the type's arrow
        // arity (2). Emitting a flat symbol would mis-link (producer `@mk(i64)` →
        // closure handle vs. client `@mk(i64, i64)` → scalar), so the producer must
        // refuse it rather than miscompile.
        const HO_LIB: &str = "module M at services exposing (mk) =\n\
            let add = fn a: Int => fn b: Int => a + b in\n\
            let mk = fn a: Int => add a in\n\
            ()\n\
            ()";
        let src = HO_LIB.to_string();
        let err = std::thread::Builder::new()
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let prog = crate::parse::parse_program(&src).expect("parses");
                let module = prog.modules[0].clone();
                let grafted = crate::stdlib::program(&src).expect("grafts");
                exported_functions(&module, &grafted)
            })
            .expect("spawn")
            .join()
            .expect("export worker")
            .expect_err("a higher-order export must be refused, not emitted");
        // The exact deferral, made loud: the `HigherOrderExport` variant naming `mk`.
        assert!(
            matches!(err, IfaceError::HigherOrderExport { ref name } if name == "mk"),
            "expected HigherOrderExport {{ name: \"mk\" }}, got {err:?}"
        );
        // And it carries the honest message (no flat symbol was emitted).
        let msg = err.to_string();
        assert!(
            msg.contains("higher-order export `mk`"),
            "message names `mk`: {msg}"
        );

        // The first-order `add3` (the gate's producer) is unaffected — still emits
        // exactly one flat, binary symbol.
        let exports = build_exports(MATH_LIB);
        assert_eq!(exports.len(), 1, "add3 still exports one flat symbol");
        assert_eq!(exports[0].arity(), 2, "add3 is still binary");
    }

    #[test]
    fn the_consume_path_returns_the_cross_module_call_table() {
        // The client side of the seam: importing `add3` yields an `ImportedSymbol`
        // naming the SAME mangled symbol the producer emitted, at the right arity —
        // so the backend turns `add3 40 2` into one direct external call.
        let iface = build(MATH_LIB);
        let loaded = LoadedInterface::accept(iface).expect("ABI ok");
        let (typed, imports) = on_stack(move || {
            check_client_against_with_imports(
                "import Data.Math\nadd3 40 2",
                &[Import::all("Data.Math")],
                &[loaded],
            )
        })
        .expect("client type-checks");
        assert_eq!(typed.ty, Type::Int, "add3 40 2 : Int");
        assert_eq!(imports.len(), 1, "one imported callable symbol");
        assert_eq!(
            imports[0],
            ImportedSymbol {
                name: "add3".into(),
                symbol: mangle_export("Data.Math", "add3"),
                arity: 2,
            }
        );
    }
}
