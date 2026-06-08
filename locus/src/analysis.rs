//! Shared effect/capability **analysis** — the single source of truth behind
//! the CLI `effects` manifest, the MCP `PipelineReport`, and (richest) the IDE
//! `Locus → Analyze` report pane.
//!
//! Given an elaborated [`Typed`] tree plus the grafted [`ModuleDecl`]s, it
//! produces a [`Report`]:
//!
//! - **effects** — the program's root effect manifest, each label tagged with
//!   the **layer it enters at** (0 boundary / 1 services / 2 app) and a catalog
//!   gloss. The layer is read from each module's `mints` clause where possible
//!   (authoritative), falling back to the label's kind.
//! - **functions** — the per-function origin table: where each referenced
//!   function is defined (module + layer), its argument types, the effects it
//!   carries, and whether it is self-recursive.
//! - **calls** — the call graph among those functions, each edge tagged with the
//!   callee's effects and whether the call **crosses a layer** (an app→services
//!   or services→boundary hop — the confinement-interesting ones).
//! - **data_access** — which functions touch which **data effects** (a SQL
//!   store, a credential vault, raw memory, a mutable cell), and the boundary
//!   provider behind each. The "internal data access tree".
//!
//! It is platform- and plugin-agnostic: it only needs the elaborated tree and
//! the module declarations, which the caller assembles (stdlib + any service
//! plugins + the user's own modules).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

use crate::sema::{Node, Typed, TypedBlockItem};
use crate::syntax::{Label, Layer, ModuleDecl, Term, Type};

// ── effect catalog (category + gloss; shipped data, see `effects.catalog`) ────

/// The embedded effect-catalog source — data the compiler ships and `republish`
/// emits. Public so the driver can write it back out byte-identically.
pub const EFFECT_CATALOG_SRC: &str = include_str!("effects.catalog");

/// The effect catalog: the category roll-up order, and per-label / per-kind
/// category + gloss. Data, not hardcoded logic — parsed from `effects.catalog`.
struct Catalog {
    order: Vec<String>,
    by_label: HashMap<String, (String, String)>,
    by_kind: HashMap<String, (String, String)>,
}

/// Parse the embedded catalog once. Lenient: blank / `#` / malformed lines are
/// skipped, so a missing entry degrades to the default rather than failing.
fn load_catalog() -> Catalog {
    let mut order = Vec::new();
    let mut by_label = HashMap::new();
    let mut by_kind = HashMap::new();
    for line in EFFECT_CATALOG_SRC.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tok: Vec<&str> = line.split_whitespace().collect();
        if tok[0] == "order" {
            order = tok[1..].iter().map(|s| s.to_string()).collect();
            continue;
        }
        if tok.len() < 3 {
            continue;
        }
        let entry = (tok[1].to_string(), tok[2..].join(" "));
        if let Some(kind) = tok[0].strip_prefix("kind:") {
            by_kind.insert(kind.to_string(), entry);
        } else {
            by_label.insert(tok[0].to_string(), entry);
        }
    }
    Catalog {
        order,
        by_label,
        by_kind,
    }
}

fn catalog() -> &'static Catalog {
    static C: OnceLock<Catalog> = OnceLock::new();
    C.get_or_init(load_catalog)
}

/// A label's KIND — the fallback key when it is not named explicitly.
fn label_kind(l: &Label) -> &'static str {
    match l {
        Label::World(_) => "world",
        Label::User(_) => "user",
        Label::Exn(_) => "exn",
        Label::Gc => "gc",
        Label::St => "state",
        Label::Insert => "staging",
    }
}

/// `(category, gloss)` for a label: explicit entry wins, else the kind fallback,
/// else the ultimate default. Strings live in the `'static` catalog.
fn lookup(l: &Label) -> (&'static str, &'static str) {
    let cat = catalog();
    let name = format!("{l}");
    if let Some((c, g)) = cat.by_label.get(&name) {
        return (c.as_str(), g.as_str());
    }
    if let Some((c, g)) = cat.by_kind.get(label_kind(l)) {
        return (c.as_str(), g.as_str());
    }
    ("user", "effect")
}

/// Which bucket an effect label rolls up into (from the catalog).
pub fn category(l: &Label) -> &'static str {
    lookup(l).0
}

/// A one-line gloss for an effect label (from the catalog).
pub fn describe(l: &Label) -> &'static str {
    lookup(l).1
}

/// The catalog's category display order (for grouped manifests).
pub fn category_order() -> &'static [String] {
    &catalog().order
}

// ── layer attribution ────────────────────────────────────────────────────────

/// `effect-label → minting (rank, provider-module)`, read from every module's
/// `mints` clause, keeping the **lowest rank** (most-privileged minting layer).
/// A power minted by more than one module is attributed to its most-privileged
/// minter, never to whichever module merely grafts first — so the report can
/// never under-state a power's privilege via declaration order.
fn mint_info(decls: &[ModuleDecl]) -> HashMap<String, (u8, String)> {
    let mut m: HashMap<String, (u8, String)> = HashMap::new();
    for d in decls {
        let rank = d.layer.rank();
        for l in &d.mints {
            m.entry(l.to_string())
                .and_modify(|(r, p)| {
                    if rank < *r {
                        *r = rank;
                        *p = d.name.clone();
                    }
                })
                .or_insert((rank, d.name.clone()));
        }
    }
    m
}

/// `effect-label → minting layer rank` (lowest rank wins). See [`mint_info`].
fn mint_layer_map(decls: &[ModuleDecl]) -> HashMap<String, u8> {
    mint_info(decls).into_iter().map(|(k, (r, _))| (k, r)).collect()
}

/// `effect-label → minting module name` — the boundary provider behind a power,
/// taken from the most-privileged (lowest-rank) minter. See [`mint_info`].
fn mint_provider_map(decls: &[ModuleDecl]) -> HashMap<String, String> {
    mint_info(decls).into_iter().map(|(k, (_, p))| (k, p)).collect()
}

/// The layer an effect *enters the program at*: 0 boundary / 1 services / 2 app,
/// or `None` for a cross-cutting effect not layer-confined (`st` mutation, `exn`
/// control, `Insert` staging).
///
/// `mints` (built from the module declarations) is **authoritative**. For a label
/// no module mints: a `World` power or `gc` is a **boundary** primitive — the
/// runtime mints it natively, with no services seal between it and the world (so
/// `console`/`fs`/`net`/`clock`, the native ops, are boundary, not services); any
/// other `User` effect defaults to **app**, its least-privilege-implying layer.
/// We never *infer* a lower (more-privileged) layer from a name shape — only an
/// explicit mint can place a power below app — so the report cannot under-state a
/// power's privilege.
pub fn effect_layer(l: &Label, mints: &HashMap<String, u8>) -> Option<u8> {
    if let Some(&r) = mints.get(&l.to_string()) {
        return Some(r);
    }
    match l {
        Label::World(_) | Label::Gc => Some(0),
        Label::User(_) => Some(2),
        Label::St | Label::Exn(_) | Label::Insert => None,
    }
}

/// [`effect_layer`] for a single label given the module declarations (builds the
/// mint map internally — convenient for callers that classify labels one by one,
/// e.g. the CLI / MCP manifest printers).
pub fn effect_layer_in(l: &Label, decls: &[ModuleDecl]) -> Option<u8> {
    effect_layer(l, &mint_layer_map(decls))
}

/// Map every module-exposed function name to its `(module, layer)`, straight
/// from the declarations (no elaboration). Uses the `exposing` list (the public
/// surface — a module body is often a `handle … with`, not a plain `let` spine),
/// falling back to the body's top-level `let` names when `exposing = None`.
pub fn module_layer_map(decls: &[ModuleDecl]) -> HashMap<String, (String, Layer)> {
    let mut map: HashMap<String, (String, Layer)> = HashMap::new();
    // When a name is bound at more than one layer, keep the most-privileged
    // (lowest-rank) one — never under-state a function's privilege by graft order.
    let record = |map: &mut HashMap<String, (String, Layer)>, name: &str, m: &ModuleDecl| {
        map.entry(name.to_string())
            .and_modify(|e| {
                if m.layer.rank() < e.1.rank() {
                    *e = (m.name.clone(), m.layer);
                }
            })
            .or_insert((m.name.clone(), m.layer));
    };
    for m in decls {
        match &m.exposing {
            Some(names) => {
                for n in names {
                    record(&mut map, n, m);
                }
            }
            None => {
                let mut cur = &m.body;
                while let Term::Let(name, _, body) = cur {
                    record(&mut map, name, m);
                    cur = body;
                }
            }
        }
    }
    map
}

// ── data effects (the "internal data access" subset) ──────────────────────────

/// Effect-label names that denote touching a **data store** (persistent or
/// external) or mutable state — across `World` and `User` kinds. Only labels a
/// real module actually mints/introduces are listed (no speculative entries: a
/// dead matcher like a bare `db` would collide with a same-named module under
/// the friendly collapse and fabricate rows). Distinct from `gc` (ubiquitous
/// allocation) and control/staging effects.
const DATA_WORLD: &[&str] = &[
    // raw / native stores (World powers)
    "mem", "fs", "net",
    // sealed plugin store effects (User labels minted at a boundary)
    "sqlite", "sqlite_fs", "cred_access",
    // the DocsFs controlled-filesystem service (User labels)
    "docsfs_read", "docsfs_write", "docsfs_append",
];

/// Does this label denote **data access** — reading/writing a store or a mutable
/// cell? `st` (Ref mutation) and the [`DATA_WORLD`] stores. The store labels can
/// arrive as either a `World` power (raw memory / filesystem) or a `User` effect
/// (a plugin's sealed `sqlite` / `cred_access`), so match on the name for both.
/// `gc` is intentionally excluded (allocation is ubiquitous, not a data signal).
pub fn is_data_effect(l: &Label) -> bool {
    match l {
        Label::St => true,
        Label::World(n) | Label::User(n) => DATA_WORLD.contains(&n.as_str()),
        _ => false,
    }
}

/// String form of [`is_data_effect`], for the already-stringified function rows.
pub fn is_data_effect_str(s: &str) -> bool {
    s == "st" || DATA_WORLD.contains(&s)
}

// ── report types ──────────────────────────────────────────────────────────────

/// One effect in the program's root manifest, with its provenance.
#[derive(Debug, Clone)]
pub struct EffectInfo {
    pub label: String,
    /// The layer it enters at (0/1/2), or `None` if cross-cutting.
    pub layer: Option<u8>,
    pub category: String,
    pub gloss: String,
    pub is_data: bool,
}

/// One row of the per-function origin table.
#[derive(Debug, Clone)]
pub struct FnInfo {
    pub module: String,
    pub name: String,
    /// Layer name (`boundary`/`services`/`app`/`—`).
    pub layer: String,
    /// Layer rank (0/1/2), or `None` for a builtin with no module binding.
    pub layer_rank: Option<u8>,
    pub args: Vec<String>,
    pub effects: Vec<String>,
    /// Whether the function references its own name (self-recursive).
    pub recursive: bool,
}

/// One edge of the call graph.
#[derive(Debug, Clone)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
    /// The callee's effects (the powers that flow across this call).
    pub effects: Vec<String>,
    /// Whether caller and callee sit at different layers.
    pub crosses_layer: bool,
}

/// One row of the data-access tree: a function and the data effects it performs,
/// with the boundary provider behind each.
#[derive(Debug, Clone)]
pub struct DataRow {
    pub function: String,
    pub layer: String,
    pub layer_rank: Option<u8>,
    pub effects: Vec<String>,
    /// `(data-effect, provider-module)` — the boundary that mints each effect, or
    /// `—` for an ambient effect (`st`) with no single provider.
    pub providers: Vec<(String, String)>,
}

/// The full structured analysis of an elaborated program.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub effects: Vec<EffectInfo>,
    pub functions: Vec<FnInfo>,
    pub calls: Vec<CallEdge>,
    pub data_access: Vec<DataRow>,
}

/// Classify the root effect manifest of `tree` (its `row`), tagging each label
/// with its layer + catalog gloss. The lightweight path for callers (a run
/// report, the CLI manifest) that only need the manifest, not the full table.
pub fn classify_effects(tree: &Typed, decls: &[ModuleDecl]) -> Vec<EffectInfo> {
    let mints = mint_layer_map(decls);
    tree.row
        .labels()
        .map(|l| EffectInfo {
            label: l.to_string(),
            layer: effect_layer(l, &mints),
            category: category(l).to_string(),
            gloss: describe(l).to_string(),
            is_data: is_data_effect(l),
        })
        .collect()
}

/// Sort key for an optional layer rank: `None` (builtin/unknown) sorts last.
fn rank_key(r: Option<u8>) -> u8 {
    r.unwrap_or(3)
}

/// The full analysis: the manifest, the per-function table, the call graph, and
/// the data-access tree. `source` is the user's text (a conservative token scan
/// scopes the table to functions the program actually names and attributes the
/// user's own bindings); `decls` are the grafted module declarations (stdlib +
/// plugins + the user's modules) the layers are read from.
pub fn analyze(tree: &Typed, decls: &[ModuleDecl], source: &str) -> Report {
    let mints = mint_layer_map(decls);
    let providers = mint_provider_map(decls);
    let modmap = module_layer_map(decls);

    // The program's root effect manifest, each label with its provenance.
    let effects: Vec<EffectInfo> = tree
        .row
        .labels()
        .map(|l| EffectInfo {
            label: l.to_string(),
            layer: effect_layer(l, &mints),
            category: category(l).to_string(),
            gloss: describe(l).to_string(),
            is_data: is_data_effect(l),
        })
        .collect();

    let referenced = referenced_idents(source);
    let user_names = user_bound_names(source);

    // One whole-tree pass: a binding's body (`defs`) and a `Var`'s use-site node
    // (`uses`, the fallback for a function seen only at a call). First-wins.
    let mut defs: HashMap<String, &Typed> = HashMap::new();
    let mut uses: HashMap<String, &Typed> = HashMap::new();
    walk(tree, &mut |n| match &n.node {
        Node::Var(name) => {
            uses.entry(name.clone()).or_insert(n);
        }
        Node::Let { name, bound, .. } => {
            defs.entry(name.clone()).or_insert(bound.as_ref());
        }
        Node::Block { items, .. } => {
            for it in items {
                if let TypedBlockItem::Let { name, bound } = it {
                    defs.entry(name.clone()).or_insert(bound);
                }
            }
        }
        _ => {}
    });
    let ty_of = |name: &str| -> Option<&Type> {
        defs.get(name)
            .map(|b| &b.ty)
            .or_else(|| uses.get(name).map(|v| &v.ty))
    };

    let mut functions: Vec<FnInfo> = Vec::new();
    // The data-access tree is classified from the **raw** decoded labels (`fx`),
    // BEFORE friendly_effects collapses a service's `*_op` to its module name —
    // otherwise a module named e.g. `Db` would collapse to the string "db" and a
    // bare-name data set would fabricate a data row it never performs.
    let mut raw_data: HashMap<String, Vec<String>> = HashMap::new();
    for name in &referenced {
        let Some(ty) = ty_of(name) else { continue };
        let Some((args, fx)) = decode_fun(ty) else {
            continue;
        };
        let (module, layer, layer_rank) = attribute(name, &user_names, &modmap);
        let de: Vec<String> = fx.iter().filter(|e| is_data_effect_str(e)).cloned().collect();
        if !de.is_empty() {
            raw_data.insert(name.clone(), de);
        }
        let effects = friendly_effects(&fx, &module);
        functions.push(FnInfo {
            module,
            name: name.clone(),
            layer,
            layer_rank,
            args,
            effects,
            recursive: false,
        });
    }
    functions.sort_by(|a, b| {
        rank_key(a.layer_rank)
            .cmp(&rank_key(b.layer_rank))
            .then(a.module.cmp(&b.module))
            .then(a.name.cmp(&b.name))
    });

    // Call graph: for each table function with a known body, the table functions
    // it references (its callees), plus whether it references its own name (self-
    // recursion — kept, not dropped, so a recursive loop is visible).
    let names: HashSet<String> = functions.iter().map(|r| r.name.clone()).collect();
    let mut recursive: HashSet<String> = HashSet::new();
    let mut edge_set: BTreeSet<(String, String)> = BTreeSet::new();
    for r in &functions {
        let Some(body) = defs.get(r.name.as_str()) else {
            continue;
        };
        let mut callees: BTreeSet<String> = BTreeSet::new();
        walk(body, &mut |n| {
            if let Node::Var(g) = &n.node {
                if g == &r.name {
                    recursive.insert(g.clone());
                } else if names.contains(g.as_str()) {
                    callees.insert(g.clone());
                }
            }
        });
        for g in callees {
            edge_set.insert((r.name.clone(), g));
        }
    }
    for f in &mut functions {
        if recursive.contains(&f.name) {
            f.recursive = true;
        }
    }

    // Now that the recursion flags are set, snapshot the per-function effects and
    // ranks (owned, so the edge map does not borrow `functions`) and tag edges.
    let fx_of: HashMap<String, Vec<String>> = functions
        .iter()
        .map(|f| (f.name.clone(), f.effects.clone()))
        .collect();
    let rank_of: HashMap<String, Option<u8>> = functions
        .iter()
        .map(|f| (f.name.clone(), f.layer_rank))
        .collect();
    let calls: Vec<CallEdge> = edge_set
        .into_iter()
        .map(|(caller, callee)| {
            let effects = fx_of.get(&callee).cloned().unwrap_or_default();
            let crosses = match (
                rank_of.get(&caller).copied().flatten(),
                rank_of.get(&callee).copied().flatten(),
            ) {
                (Some(a), Some(b)) => a != b,
                _ => false,
            };
            CallEdge {
                caller,
                callee,
                effects,
                crosses_layer: crosses,
            }
        })
        .collect();

    // Data-access tree: every table function that performs a data effect (from
    // the raw labels collected above), with the boundary provider behind each.
    let mut data_access: Vec<DataRow> = Vec::new();
    for f in &functions {
        let Some(de) = raw_data.get(&f.name).cloned() else {
            continue;
        };
        let provs: Vec<(String, String)> = de
            .iter()
            .map(|e| {
                (
                    e.clone(),
                    providers.get(e).cloned().unwrap_or_else(|| "\u{2014}".into()),
                )
            })
            .collect();
        data_access.push(DataRow {
            function: f.name.clone(),
            layer: f.layer.clone(),
            layer_rank: f.layer_rank,
            effects: de,
            providers: provs,
        });
    }

    Report {
        effects,
        functions,
        calls,
        data_access,
    }
}

// ── internal helpers (moved here from the IDE so there is one implementation) ──

/// Maximal `[A-Za-z_][A-Za-z0-9_]*` words in the source. Conservative: a word is
/// only ever shown if it is *also* a real binding, so over-inclusion is harmless.
fn referenced_idents(src: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    let mut cur = String::new();
    for ch in src.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            set.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        set.insert(cur);
    }
    set
}

/// The names the *user's* source binds with `let`, at any depth — so the table
/// attributes them to "(this program)". A token scan (not a parse): the word
/// following each `let` (skipping `mut`/`rec`). Conservative; consulted only for
/// names that are also real bindings, so a `let` in a comment/string is harmless.
fn user_bound_names(src: &str) -> HashSet<String> {
    let mut s = HashSet::new();
    let mut expect = false;
    for word in src.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.is_empty() {
            continue;
        }
        if expect {
            if word == "mut" || word == "rec" {
                continue;
            }
            s.insert(word.to_string());
            expect = false;
        } else if word == "let" {
            expect = true;
        }
    }
    s
}

/// Pre-order visit of every node in the typed tree, calling `f` on each.
fn walk<'a>(t: &'a Typed, f: &mut impl FnMut(&'a Typed)) {
    f(t);
    match &t.node {
        Node::Var(_) | Node::Int(_) | Node::Float(_) | Node::Bool(_) | Node::Unit | Node::Brk
        | Node::Str(_) | Node::Extern(..) => {}
        Node::Bin(_, a, b) => {
            walk(a, f);
            walk(b, f);
        }
        Node::Cast(_, a)
        | Node::MaskReduce(_, a)
        | Node::FloatMathUnary(_, a)
        | Node::Quote(a)
        | Node::Splice(a)
        | Node::Genlet(a)
        | Node::Letloc(a)
        | Node::Peek(_, a)
        | Node::Len(a) => walk(a, f),
        Node::Coerce { inner, .. } => walk(inner, f),
        Node::FloatMathBinary(_, a, b) | Node::Poke(_, a, b) | Node::Index(_, a, b) => {
            walk(a, f);
            walk(b, f);
        }
        Node::FloatMathTernary(_, a, b, c)
        | Node::If(a, b, c)
        | Node::Fill(a, b, c)
        | Node::Copy(a, b, c)
        | Node::IndexSet(_, a, b, c) => {
            walk(a, f);
            walk(b, f);
            walk(c, f);
        }
        Node::VectorSelect {
            mask,
            then_value,
            else_value,
        } => {
            walk(mask, f);
            walk(then_value, f);
            walk(else_value, f);
        }
        Node::VectorLit { elems, .. } | Node::Tuple(elems) | Node::ArrayLit { elems, .. } => {
            for e in elems {
                walk(e, f);
            }
        }
        Node::VectorSplat { value, .. } | Node::Assign { value, .. } | Node::RefNew { value } => {
            walk(value, f)
        }
        Node::VectorLoad { arr, idx, .. } | Node::ArrayGet { arr, idx, .. } => {
            walk(arr, f);
            walk(idx, f);
        }
        Node::VectorStore {
            arr, idx, value, ..
        }
        | Node::ArraySet {
            arr,
            idx,
            val: value,
            ..
        } => {
            walk(arr, f);
            walk(idx, f);
            walk(value, f);
        }
        Node::VectorExtract { vector, .. } => walk(vector, f),
        Node::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            for (_, _, _, init) in vars {
                walk(init, f);
            }
            walk(cond, f);
            for s in steps {
                walk(s, f);
            }
            walk(result, f);
        }
        Node::Lam { body, .. } => walk(body, f),
        Node::App { fun, arg } => {
            walk(fun, f);
            walk(arg, f);
        }
        Node::Let { bound, body, .. } => {
            walk(bound, f);
            walk(body, f);
        }
        Node::Block { items, body } => {
            for it in items {
                match it {
                    TypedBlockItem::Let { bound, .. }
                    | TypedBlockItem::LetMut { bound, .. }
                    | TypedBlockItem::LetTuple { bound, .. } => walk(bound, f),
                }
            }
            walk(body, f);
        }
        Node::LetMut { bound, body, .. } | Node::LetTuple(_, bound, body) => {
            walk(bound, f);
            walk(body, f);
        }
        Node::Deref { cell } => walk(cell, f),
        Node::RefAssign { target, value } => {
            walk(target, f);
            walk(value, f);
        }
        Node::Perform { arg, .. } => walk(arg, f),
        Node::Handle { scrutinee, handler } => {
            walk(scrutinee, f);
            for op in &handler.ops {
                walk(&op.body, f);
            }
            walk(&handler.ret.body, f);
        }
        Node::Record(fields) => {
            for (_, e) in fields {
                walk(e, f);
            }
        }
        Node::Field(a, _) => walk(a, f),
        Node::Construct { args, .. } => {
            for (e, _, _) in args {
                walk(e, f);
            }
        }
        Node::Match { scrutinee, arms } => {
            walk(scrutinee, f);
            for arm in arms {
                walk(&arm.body, f);
            }
        }
    }
}

/// Decode a (curried) function type into `(arg-type strings, effect labels)` —
/// the union of the latent rows along the arrow chain (the powers performed when
/// it is applied). `None` for a non-function type.
fn decode_fun(ty: &Type) -> Option<(Vec<String>, Vec<String>)> {
    let mut args = Vec::new();
    let mut effs: BTreeSet<String> = BTreeSet::new();
    let mut cur = ty;
    let mut is_fun = false;
    while let Type::Fun(dom, cod, row) = cur {
        is_fun = true;
        args.push(dom.to_string());
        for l in row.labels() {
            effs.insert(l.to_string());
        }
        cur = cod.as_ref();
    }
    is_fun.then(|| (args, effs.into_iter().collect()))
}

/// Friendly-up a function's effect labels: a handler-based service module gives
/// its functions an internal `*_op` label; collapse those to the module's
/// service name (`console_writeln_op → console`). Other labels pass through.
/// Only applied to functions attributed to a real module. Deduped.
fn friendly_effects(effs: &[String], module: &str) -> Vec<String> {
    let mappable = module != "(this program)" && module != "\u{2014}";
    let svc = module.rsplit('.').next().unwrap_or(module).to_lowercase();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for e in effs {
        if mappable && e.ends_with("_op") {
            out.insert(svc.clone());
        } else {
            out.insert(e.clone());
        }
    }
    out.into_iter().collect()
}

/// Where a referenced function is defined: the user's own program (`app`), a
/// module (+ its layer), or `—` for a builtin with no module binding. Returns
/// `(module, layer-name, layer-rank)`.
fn attribute(
    name: &str,
    user: &HashSet<String>,
    modmap: &HashMap<String, (String, Layer)>,
) -> (String, String, Option<u8>) {
    if user.contains(name) {
        return ("(this program)".to_string(), "app".to_string(), Some(2));
    }
    if let Some((m, l)) = modmap.get(name) {
        return (m.clone(), l.name().to_string(), Some(l.rank()));
    }
    ("\u{2014}".to_string(), "\u{2014}".to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(name: &str, layer: Layer, mints: Vec<Label>, exposing: Vec<&str>) -> ModuleDecl {
        ModuleDecl {
            name: name.to_string(),
            layer,
            mints,
            seals: Vec::new(),
            exposing: Some(exposing.into_iter().map(|s| s.to_string()).collect()),
            body: Term::Unit,
        }
    }

    #[test]
    fn catalog_categorizes_known_and_kind_fallback_labels() {
        assert_eq!(category(&Label::World("winapi".into())), "boundary");
        assert_eq!(category(&Label::World("fs".into())), "world");
        assert_eq!(category(&Label::Gc), "memory");
        // an unlisted user op falls back by kind to `user`.
        assert_eq!(category(&Label::User("Telemetry_op".into())), "user");
        assert!(!describe(&Label::Gc).is_empty());
        assert!(!category_order().is_empty());
    }

    #[test]
    fn effect_layer_reads_mints_then_kind_fallback() {
        let decls = vec![decl(
            "SqliteFfi",
            Layer::Boundary,
            vec![Label::World("sqlite".into())],
            vec!["raw_open"],
        )];
        let mints = mint_layer_map(&decls);
        // authoritative: minted at the boundary module.
        assert_eq!(effect_layer(&Label::World("sqlite".into()), &mints), Some(0));
        // A World power or gc is a boundary primitive — including the native ops
        // (console/fs/net/clock), which must NEVER be reported as services: that
        // would under-state a raw, unmediated power as a sealed capability.
        assert_eq!(effect_layer(&Label::World("winapi".into()), &mints), Some(0));
        assert_eq!(effect_layer(&Label::World("fs".into()), &mints), Some(0));
        assert_eq!(effect_layer(&Label::World("console".into()), &mints), Some(0));
        assert_eq!(effect_layer(&Label::World("net".into()), &mints), Some(0));
        assert_eq!(effect_layer(&Label::Gc, &mints), Some(0));
        // A user effect no module mints defaults to app — never inferred to be a
        // (more-privileged) services effect from a `_op` name shape.
        assert_eq!(effect_layer(&Label::User("db_op".into()), &mints), Some(2));
        assert_eq!(effect_layer(&Label::User("MyApp".into()), &mints), Some(2));
        // cross-cutting effects are not layer-confined.
        assert_eq!(effect_layer(&Label::St, &mints), None);
        assert_eq!(effect_layer(&Label::Insert, &mints), None);
    }

    #[test]
    fn maps_keep_the_most_privileged_minter_not_declaration_order() {
        // A power minted at BOTH services and boundary, with the services module
        // declared FIRST. The report must attribute it to boundary (rank 0), not
        // to whichever grafts first — else it under-states the power's privilege.
        let raw = Label::World("myraw".into());
        let decls = vec![
            decl("Helpers", Layer::Services, vec![raw.clone()], vec!["h"]),
            decl("Edge", Layer::Boundary, vec![raw.clone()], vec!["e"]),
        ];
        assert_eq!(mint_layer_map(&decls).get("myraw"), Some(&0u8));
        assert_eq!(
            mint_provider_map(&decls).get("myraw"),
            Some(&"Edge".to_string())
        );
        // Same for a name exposed at two layers: the boundary binding wins.
        let shared = vec![
            decl("HiSvc", Layer::Services, vec![], vec!["shared"]),
            decl("LoEdge", Layer::Boundary, vec![], vec!["shared"]),
        ];
        assert_eq!(
            module_layer_map(&shared).get("shared").map(|(_, l)| l.rank()),
            Some(0)
        );
    }

    #[test]
    fn data_effects_cover_stores_and_state_but_not_gc() {
        assert!(is_data_effect(&Label::World("mem".into())));
        // plugin store effects arrive as `User` labels — match them too.
        assert!(is_data_effect(&Label::User("sqlite".into())));
        assert!(is_data_effect(&Label::User("cred_access".into())));
        assert!(is_data_effect(&Label::St));
        assert!(!is_data_effect(&Label::Gc));
        assert!(!is_data_effect(&Label::World("console".into())));
        assert!(!is_data_effect(&Label::User("console_writeln_op".into())));
        assert!(is_data_effect_str("sqlite_fs"));
        assert!(!is_data_effect_str("gc"));
    }

    #[test]
    fn module_layer_map_keys_exposed_names_to_their_layer() {
        let decls = vec![
            decl(
                "SqliteFfi",
                Layer::Boundary,
                vec![Label::World("sqlite".into())],
                vec!["raw_open"],
            ),
            decl("Database", Layer::Services, vec![], vec!["db_open_memory"]),
        ];
        let map = module_layer_map(&decls);
        assert_eq!(map.get("raw_open").map(|(_, l)| l.rank()), Some(0));
        assert_eq!(map.get("db_open_memory").map(|(_, l)| l.rank()), Some(1));
    }
}
