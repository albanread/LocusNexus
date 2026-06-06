//! The Locus eval session — Phase 1 **engine wiring**.
//!
//! Mirrors NewFactor's `session.rs` *seam* (an in-process evaluator the
//! IDE worker drives). [`LocusSession::eval`] now runs the real Locus
//! pipeline + JIT — the exact front-end path the `locusc run` driver
//! uses (`locus-llvm/src/main.rs::cmd_run`) — and returns the program's
//! `i64` result, its **effect manifest** (the `{…}` row the program
//! touches, as `cmd_effects` derives it), and any parse/type
//! **diagnostics**. The heavy pipeline (elaboration + staging recurse
//! deeply over the stdlib graft) runs on a dedicated
//! [`locus::PIPELINE_STACK_BYTES`] worker thread, joined per eval —
//! exactly as `cmd_run` spawns its worker — so the IDE worker thread
//! never overflows.
//!
//! Phase-1 scope: surface **result + effects + diagnostics**. Capturing
//! the program's *live* console output (`console_writeln`'s text) into a
//! pane is deferred to Phase 2, where the console becomes an IDE-world
//! service (the same boundary+seal pattern as `Graphics`/`Event`). For
//! now the program's console effect goes wherever the JIT's default
//! sends it (the OS console).

#![cfg(windows)]

use igui::Diagnostic;

/// Outcome of evaluating a Locus buffer/expression.
///
/// On success `result` is the program's `i64` and `effects` is its
/// sorted effect-label manifest (`gc`, `winapi`, `mem`, …). On a
/// parse/type failure `result` is `None`, `effects` is empty, and
/// `diagnostics` carries the structured error(s); `result` is also
/// `None` for a runtime JIT failure, with the reason in `diagnostics`.
#[derive(Debug, Clone, Default)]
pub struct EvalOutcome {
    /// The program's `i64` result (its `locusc run` exit value), or
    /// `None` if it never ran (a parse/type error, or a JIT failure).
    pub result: Option<i64>,
    /// The effect manifest — the labels in the elaborated program's
    /// root row, sorted and stable (mirrors `cmd_effects`). Empty for a
    /// pure program (`{}`) or when the program failed to elaborate.
    pub effects: Vec<String>,
    /// Parse/type diagnostics (and a single synthetic entry for a
    /// runtime JIT error). Empty on a clean run. Each carries the
    /// `RN-Exxxx` code folded into its `message`, plus a 1-based
    /// `line:col` (type errors have no span yet → `1:1`).
    pub diagnostics: Vec<Diagnostic>,
}

impl EvalOutcome {
    /// Did evaluation reach a result (no parse/type/JIT error)?
    pub fn ok(&self) -> bool {
        self.result.is_some()
    }

    /// Render the outcome as console/REPL text — the result line, the
    /// effect manifest `{…}`, and any diagnostics. This is what the
    /// IDE worker echoes into the console / REPL pane.
    pub fn to_console_text(&self) -> String {
        if let Some(v) = self.result {
            let effects = if self.effects.is_empty() {
                "{}".to_string()
            } else {
                format!("{{ {} }}", self.effects.join(", "))
            };
            format!("=> {v}    effects {effects}")
        } else if self.diagnostics.is_empty() {
            // Should not happen — a non-result outcome always carries a
            // diagnostic — but never produce an empty line.
            "error (no result)".to_string()
        } else {
            self.diagnostics
                .iter()
                .map(|d| format!("error {}:{}  {}", d.line, d.column, d.message))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// A Locus evaluation session.
///
/// Phase 1 holds no long-lived engine state: each [`eval`](Self::eval)
/// builds a fresh pipeline + a one-shot ORC JIT (matching `cmd_run`,
/// which is stateless per invocation). The struct is kept so the host's
/// `session.eval(...)` call site is stable and a future incremental /
/// persistent JIT can live behind it.
#[derive(Debug, Default)]
pub struct LocusSession {
    _private: (),
}

impl LocusSession {
    /// Boot a fresh session. Infallible — no VM to boot per session;
    /// the JIT context is created per eval.
    pub fn new() -> Self {
        LocusSession { _private: () }
    }

    /// Evaluate a buffer / REPL line through the full Locus pipeline +
    /// JIT, on a big-stack worker thread.
    ///
    /// Mirrors `locusc run` (`cmd_run`): `locus::program` (grafts the
    /// stdlib) → `winapi_resolve::resolve` (fill bare `extern`s, collect
    /// the demanded apis) → `elaborate` → `stage_reduce` → `lower` →
    /// `jit_run_i64`. The effect manifest is read from the elaborated
    /// tree's root row (`cmd_effects`). Any parse/type error short-
    /// circuits into a [`Diagnostic`]; a runtime JIT error becomes a
    /// single synthetic diagnostic.
    ///
    /// Note: this intentionally does **not** run the CLI's `guard_layer2`
    /// mint-gate — the IDE is a trusted local authoring tool, and the
    /// gate needs a `locus.toml` boundary manifest the in-memory buffer
    /// has no path for. Authoring code that mints simply runs (the
    /// effect manifest still shows `winapi`/`mem`, so the powers stay
    /// visible).
    pub fn eval(&mut self, source: &str) -> EvalOutcome {
        run_on_pipeline_stack(source.to_string(), eval_pipeline)
    }

    /// A long-running eval cannot currently be interrupted (the JIT runs
    /// to completion on its worker). No-op so the host's interrupt path
    /// compiles; a cooperative cancel lands with the Phase-2 services.
    pub fn interrupt(&self) {}

    /// The session never dies (it owns no persistent VM in Phase 1).
    pub fn is_dead(&self) -> bool {
        false
    }
}

/// Build the live editor checker: `program → elaborate`, mapping any
/// parse/type error to an [`igui::Diagnostic`]. On success returns an
/// empty `Vec` (no squiggles). Runs on a big-stack worker (elaboration
/// recurses over the stdlib graft), so it is safe to call from the GUI
/// thread the editor installs it on.
///
/// This is the front half of the eval pipeline (no winapi resolve, no
/// staging, no JIT) — the cheapest path that still surfaces real
/// `RN-Exxxx` type errors as you type, matching what `locus check` does.
pub fn check_source(source: &str) -> Vec<Diagnostic> {
    run_on_pipeline_stack(source.to_string(), check_pipeline)
}

// ── pipeline bodies (run on the big-stack worker) ──────────────────────────

/// The eval pipeline proper — mirrors `cmd_run`'s front end + the JIT
/// run, plus the effect-manifest read from `cmd_effects`. Runs entirely
/// on the worker thread `run_on_pipeline_stack` spawns.
fn eval_pipeline(source: String) -> EvalOutcome {
    // parse + graft the stdlib prelude. A parse error is the same
    // structured diagnostic the CLI renders (`Report::parse_error`).
    let term = match locus::program(&source) {
        Ok(t) => t,
        Err(e) => return EvalOutcome::from_diags(vec![diag_from_report(&locus::Report::parse_error(&e, &source))]),
    };

    // Resolve bare `extern "Sym"` from the Win32 oracle, collecting the
    // demanded apis the JIT needs. A resolution failure is a plain
    // string error (no span) — surface it at 1:1.
    let (term, apis) = match locus_llvm::winapi_resolve::resolve(term) {
        Ok(pair) => pair,
        Err(msg) => return EvalOutcome::from_diags(vec![diag_plain(&msg)]),
    };

    // Elaborate (type-check + decorate). A type error → the same
    // `Report::type_error` the CLI uses (no span yet → 1:1).
    let tree = match locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term) {
        Ok(tree) => tree,
        Err(e) => return EvalOutcome::from_diags(vec![diag_from_report(&locus::Report::type_error(&e))]),
    };

    // The effect manifest: the elaborated root row's labels, sorted +
    // stable (BTreeSet walk), exactly as `cmd_effects` reports them.
    let effects: Vec<String> = tree.row.labels().map(|l| l.to_string()).collect();

    // Stage (run compile-time generators) → lower to ANF IR. Staging is
    // a fallible native tree-walk; a failure is a plain string error.
    let tree = match locus::stage_reduce(&tree) {
        Ok(t) => t,
        Err(msg) => return EvalOutcome::from_diags(vec![diag_plain(&msg)]),
    };
    let ir = locus::lower(&tree);

    // JIT-compile and run — its side effects happen (console output goes
    // to the OS console for now; Phase 2 captures it), and its `i64` is
    // the result. A runtime error becomes one synthetic diagnostic, but
    // we keep the (already-derived) effect manifest for the pane.
    //
    // Phase 2a: route through the **with-symbols** JIT, injecting the
    // `igui_*` GUI C-ABI (`ide_symbol_table`) so a graphical buffer's
    // `extern "iGui.…"` externs (minted by the Graphics/Event boundary)
    // resolve to the linked shell. A graphical program enters *its own*
    // event loop here (`loop … next_event … draw …`) and this call blocks
    // on the big-stack worker until the program returns (`Close` → exit
    // i64) — igui's channels (events in, batches out) carry the cross-
    // thread flow; we build no loop in Rust. A plain text program is
    // unaffected (it resolves no `iGui.*` symbol, so the extra table is
    // inert), keeping the Phase-1 path working.
    match locus_llvm::jit_run_i64_with_symbols(&ir, &apis, &ide_symbols()) {
        Ok(result) => EvalOutcome {
            result: Some(result),
            effects,
            diagnostics: Vec::new(),
        },
        Err(msg) => EvalOutcome {
            result: None,
            effects,
            diagnostics: vec![diag_plain(&msg)],
        },
    }
}

/// The IDE-world symbol table mapped into the `(String, u64)` pairs the JIT
/// takes. Two groups:
///   * every `igui_*` GUI C-ABI export — lets a graphical program resolve the
///     `Graphics`/`Event` externs to the linked `igui` shell; and
///   * the console-pane `WriteConsoleW`/`ReadConsoleW` **overrides** — redirect
///     a program's console I/O to the `⁂ console` pane instead of the (absent)
///     OS console. These share names with kernel32; the JIT's last-wins dedupe
///     makes the IDE shim win (see `jit_run_i64_with_symbols`).
/// Cheap to rebuild per eval (a couple dozen entries).
fn ide_symbols() -> Vec<(String, u64)> {
    igui::cp_exports::ide_symbol_table()
        .into_iter()
        .chain(igui::console_bridge::symbol_overrides())
        .map(|(name, addr)| (name.to_string(), addr))
        .collect()
}

/// Elaborate `source` and return its **effect manifest** (the root row's
/// labels, sorted + stable) — the front half of [`eval_pipeline`] with no
/// staging and **no JIT**, so it never runs (and never blocks on a graphical
/// program's event loop). Returns an empty `Vec` on any parse/resolve/type
/// error. This is the headless probe the Phase-2a gate checks: that a
/// `Graphics`/`Event` program elaborates with `graphics`/`event` in its row.
pub fn effect_manifest(source: &str) -> Vec<String> {
    run_on_pipeline_stack(source.to_string(), manifest_pipeline)
}

fn manifest_pipeline(source: String) -> Vec<String> {
    let Ok(term) = locus::program(&source) else {
        return Vec::new();
    };
    let Ok((term, _apis)) = locus_llvm::winapi_resolve::resolve(term) else {
        return Vec::new();
    };
    match locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term) {
        Ok(tree) => tree.row.labels().map(|l| l.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

// ── compiler-exploration views: ANF IR / LLVM IR / Assembly ─────────────────

/// Which lowering stage to dump for the compiler-explorer views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilerView {
    /// The Locus **ANF IR** (`lower`'s output) — the language's own
    /// intermediate form: named lambdas, tail calls, effect rows.
    Anf,
    /// The **LLVM IR** the backend emits from the ANF (pre-optimization).
    Llvm,
    /// The host **x86-64 assembly** the backend lowers to (pre-optimization).
    Asm,
}

impl CompilerView {
    /// Section title for the rendered report.
    pub fn title(self) -> &'static str {
        match self {
            CompilerView::Anf => "ANF IR",
            CompilerView::Llvm => "LLVM IR",
            CompilerView::Asm => "Assembly (x86-64)",
        }
    }
    /// Markdown code-fence language hint.
    pub fn lang(self) -> &'static str {
        match self {
            CompilerView::Anf => "text",
            CompilerView::Llvm => "llvm",
            CompilerView::Asm => "asm",
        }
    }
    /// Decode the worker-event tag (`0`=ANF, `1`=LLVM, `2`=ASM).
    pub fn from_tag(tag: i64) -> CompilerView {
        match tag {
            1 => CompilerView::Llvm,
            2 => CompilerView::Asm,
            _ => CompilerView::Anf,
        }
    }
}

/// The output of a compiler-explorer view: either the dumped `text`, or the
/// parse/type/lowering `diagnostics` that stopped it.
#[derive(Debug, Clone, Default)]
pub struct ViewText {
    pub diagnostics: Vec<Diagnostic>,
    pub text: String,
}

/// Produce the requested compiler view for `source` — the front-end pipeline
/// (`program → resolve → elaborate → stage_reduce → lower`, the exact path the
/// JIT/AOT use), then dump the ANF IR, the LLVM IR, or the assembly. No JIT, no
/// run. Runs on the big-stack pipeline worker.
pub fn compiler_view(source: &str, view: CompilerView) -> ViewText {
    run_on_pipeline_stack((source.to_string(), view), |(source, view)| {
        view_pipeline(&source, view)
    })
}

fn view_pipeline(source: &str, view: CompilerView) -> ViewText {
    let term = match locus::program(source) {
        Ok(t) => t,
        Err(e) => {
            return ViewText::from_diags(vec![diag_from_report(&locus::Report::parse_error(
                &e, source,
            ))])
        }
    };
    let (term, apis) = match locus_llvm::winapi_resolve::resolve(term) {
        Ok(pair) => pair,
        Err(msg) => return ViewText::from_diags(vec![diag_plain(&msg)]),
    };
    let tree = match locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term) {
        Ok(t) => t,
        Err(e) => return ViewText::from_diags(vec![diag_from_report(&locus::Report::type_error(&e))]),
    };
    let tree = match locus::stage_reduce(&tree) {
        Ok(t) => t,
        Err(msg) => return ViewText::from_diags(vec![diag_plain(&msg)]),
    };
    let ir = locus::lower(&tree);
    let _ = &apis; // ANF/LLVM/ASM dumps don't need the demanded-api set.
    let text = match view {
        CompilerView::Anf => Ok(ir.to_text()),
        CompilerView::Llvm => locus_llvm::emit_llvm_ir(&ir),
        CompilerView::Asm => locus_llvm::emit_asm(&ir),
    };
    match text {
        Ok(text) => ViewText { diagnostics: Vec::new(), text },
        Err(msg) => ViewText::from_diags(vec![diag_plain(&msg)]),
    }
}

impl ViewText {
    fn from_diags(diagnostics: Vec<Diagnostic>) -> Self {
        ViewText { diagnostics, text: String::new() }
    }
}

impl FromPipelinePanic for ViewText {
    fn from_panic() -> Self {
        ViewText::from_diags(vec![diag_plain("internal error: the lowering pipeline panicked")])
    }
    fn from_spawn_error(msg: &str) -> Self {
        ViewText::from_diags(vec![diag_plain(&format!(
            "internal error: could not start the lowering worker: {msg}"
        ))])
    }
}

// ── static analysis: the per-function effect table ──────────────────────────

/// One row of the analysis function table: a named function the program
/// references, and where its powers come from. `module`/`layer` say where it
/// is defined (a stdlib service module + its privilege layer, the user's own
/// program, or `—` for a builtin); `args` are the parameter types (the curried
/// arrow's domains); `effects` are the latent effect labels the function's type
/// carries — i.e. the powers performed when it is called.
#[derive(Debug, Clone)]
pub struct FnRow {
    pub module: String,
    pub name: String,
    pub layer: String,
    pub args: Vec<String>,
    pub effects: Vec<String>,
}

/// The full static analysis of a buffer: parse/type **diagnostics** (if it does
/// not elaborate), the program's root **effect manifest**, the per-function
/// **table** ([`FnRow`]), and the **call graph** among those functions
/// (`(caller, callee)` edges). Built from a single elaboration — no JIT, no run.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub effects: Vec<String>,
    pub functions: Vec<FnRow>,
    /// Directed `caller -> callee` edges, both endpoints in `functions`,
    /// sorted and deduped. Empty when no table function calls another.
    pub calls: Vec<(String, String)>,
}

/// Statically analyze `source`: elaborate it once (no JIT) and report its
/// diagnostics, effect manifest, and the table of named functions it uses with
/// the module / layer / arguments / effect of each — so the origin of every
/// power in the manifest is visible. Runs on the big-stack pipeline worker.
pub fn analyze(source: &str) -> Analysis {
    run_on_pipeline_stack(source.to_string(), analyze_pipeline)
}

fn analyze_pipeline(source: String) -> Analysis {
    // The identifiers the user actually wrote (a conservative token scan) and
    // the names they bind at top level — used to scope the table to *this*
    // program and to attribute user-defined functions.
    let referenced = referenced_idents(&source);
    let user_names = user_bound_names(&source);

    // parse + graft + resolve + elaborate (the front half of `eval_pipeline`).
    let term = match locus::program(&source) {
        Ok(t) => t,
        Err(e) => {
            return Analysis::from_diags(vec![diag_from_report(&locus::Report::parse_error(
                &e, &source,
            ))])
        }
    };
    let (term, _apis) = match locus_llvm::winapi_resolve::resolve(term) {
        Ok(pair) => pair,
        Err(msg) => return Analysis::from_diags(vec![diag_plain(&msg)]),
    };
    let tree = match locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term) {
        Ok(t) => t,
        Err(e) => {
            return Analysis::from_diags(vec![diag_from_report(&locus::Report::type_error(&e))])
        }
    };

    let effects: Vec<String> = tree.row.labels().map(|l| l.to_string()).collect();

    // One whole-tree pass: a binding's body (`defs`, for its declared type and
    // its callees) and a `Var`'s node at a use site (`uses`, the fallback for a
    // function only seen at a call). First-wins in each.
    use locus::sema::{Node, TypedBlockItem};
    let mut defs: std::collections::HashMap<String, &locus::Typed> = std::collections::HashMap::new();
    let mut uses: std::collections::HashMap<String, &locus::Typed> = std::collections::HashMap::new();
    walk(&tree, &mut |n| match &n.node {
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
    let ty_of = |name: &str| -> Option<&locus::Type> {
        defs.get(name)
            .map(|b| &b.ty)
            .or_else(|| uses.get(name).map(|v| &v.ty))
    };
    let modmap = module_map();

    let mut functions: Vec<FnRow> = Vec::new();
    for name in &referenced {
        let Some(ty) = ty_of(name) else { continue };
        let Some((args, fx)) = decode_fun(ty) else { continue };
        let (module, layer) = attribute(name, &user_names, &modmap);
        let effects = friendly_effects(&fx, &module);
        functions.push(FnRow {
            module,
            name: name.clone(),
            layer,
            args,
            effects,
        });
    }
    functions.sort_by(|a, b| {
        layer_rank(&a.layer)
            .cmp(&layer_rank(&b.layer))
            .then(a.module.cmp(&b.module))
            .then(a.name.cmp(&b.name))
    });

    // Call graph: for each table function with a known body, the table
    // functions it references (its callees). Self-edges dropped.
    let names: std::collections::HashSet<&str> =
        functions.iter().map(|r| r.name.as_str()).collect();
    let mut edges: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for r in &functions {
        let Some(body) = defs.get(&r.name) else { continue };
        let mut callees: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        walk(body, &mut |n| {
            if let Node::Var(g) = &n.node {
                if g != &r.name && names.contains(g.as_str()) {
                    callees.insert(g.clone());
                }
            }
        });
        for g in callees {
            edges.insert((r.name.clone(), g));
        }
    }

    Analysis {
        diagnostics: Vec::new(),
        effects,
        functions,
        calls: edges.into_iter().collect(),
    }
}

/// Maximal `[A-Za-z_][A-Za-z0-9_]*` words in the source. Conservative on
/// purpose: a word is only ever shown in the table if it is *also* a real
/// function binding, so over-inclusion (a word in a comment/string) is
/// harmless — it simply will not match a binding.
fn referenced_idents(src: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
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

/// The names the *user's* source binds with `let`, at **any** depth — so the
/// table attributes them to "(this program)" rather than leaving a nested
/// helper unattributed. A lightweight token scan (not a parse): the identifier
/// following each `let` (skipping `mut`/`rec`) is a binding. Conservative and
/// only consulted for names that are *also* real function bindings, so the odd
/// `let` in a comment/string is harmless.
fn user_bound_names(src: &str) -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    let mut expect = false;
    for word in src.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.is_empty() {
            continue;
        }
        if expect {
            if word == "mut" || word == "rec" {
                continue; // still expecting the bound name
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
/// One generic walker backs both passes: collecting binding / use-site
/// function types, and collecting the callees inside a function body. The
/// stdlib graft binds the service functions (`console_writeln`, `fill_rect`,
/// …) deep inside nested spines, so a whole-tree walk is required.
fn walk<'a>(t: &'a locus::Typed, f: &mut impl FnMut(&'a locus::Typed)) {
    use locus::sema::{Node, TypedBlockItem};
    f(t);
    match &t.node {
        Node::Var(_) | Node::Int(_) | Node::Float(_) | Node::Bool(_) | Node::Unit
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
        Node::VectorSelect { mask, then_value, else_value } => {
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
        Node::VectorStore { arr, idx, value, .. } | Node::ArraySet { arr, idx, val: value, .. } => {
            walk(arr, f);
            walk(idx, f);
            walk(value, f);
        }
        Node::VectorExtract { vector, .. } => walk(vector, f),
        Node::Loop { vars, cond, steps, result } => {
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

/// Map every stdlib-module-exposed function name to its `(module, layer)`,
/// read straight from the module declarations (no elaboration). Uses the
/// module's `exposing` list — the public surface — since a module body is
/// often a `handle … with` (e.g. `Console`), not a plain `let` spine, so the
/// exposed names are the reliable source. Falls back to the body's top-level
/// `let` names when a module exposes everything (`exposing = None`).
fn module_map() -> std::collections::HashMap<String, (String, String)> {
    let mut map = std::collections::HashMap::new();
    for m in locus::stdlib_module_decls() {
        let layer = m.layer.name().to_string();
        match &m.exposing {
            Some(names) => {
                for n in names {
                    map.entry(n.clone())
                        .or_insert((m.name.clone(), layer.clone()));
                }
            }
            None => {
                let mut cur = &m.body;
                while let locus::Term::Let(name, _, body) = cur {
                    map.entry(name.clone())
                        .or_insert((m.name.clone(), layer.clone()));
                    cur = body;
                }
            }
        }
    }
    map
}

/// Decode a (curried) function type into `(arg-type strings, effect labels)`.
/// Returns `None` for a non-function type. The effect is the union of the
/// latent rows along the arrow chain — the powers performed when it is applied.
fn decode_fun(ty: &locus::Type) -> Option<(Vec<String>, Vec<String>)> {
    let mut args = Vec::new();
    let mut effs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cur = ty;
    let mut is_fun = false;
    while let locus::Type::Fun(dom, cod, row) = cur {
        is_fun = true;
        args.push(dom.to_string());
        for l in row.labels() {
            effs.insert(l.to_string());
        }
        cur = cod.as_ref();
    }
    is_fun.then(|| (args, effs.into_iter().collect()))
}

/// Friendly-up a function's effect labels for display. Handler-based service
/// modules (`Console`, `Time`, `Db`, `LocusEnv`) give their functions an
/// internal per-operation label like `console_writeln_op`; collapse any such
/// `*_op` label to the module's service name (`console`, `time`, …) — the
/// effect the function conceptually performs. Non-`_op` labels (`gc`, `mem`,
/// `graphics`, `event`, …) pass through unchanged. Only applied to functions
/// attributed to a real stdlib module (not user code / unknown). Deduped.
fn friendly_effects(effs: &[String], module: &str) -> Vec<String> {
    let mappable = module != "(this program)" && module != "\u{2014}";
    let svc = module.rsplit('.').next().unwrap_or(module).to_lowercase();
    let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for e in effs {
        if mappable && e.ends_with("_op") {
            out.insert(svc.clone());
        } else {
            out.insert(e.clone());
        }
    }
    out.into_iter().collect()
}

/// Where a referenced function is defined: the user's own program, a stdlib
/// module (+ its layer), or `—` for a builtin/primitive with no module binding.
fn attribute(
    name: &str,
    user: &std::collections::HashSet<String>,
    modmap: &std::collections::HashMap<String, (String, String)>,
) -> (String, String) {
    if user.contains(name) {
        return ("(this program)".to_string(), "app".to_string());
    }
    if let Some((m, l)) = modmap.get(name) {
        return (m.clone(), l.clone());
    }
    ("\u{2014}".to_string(), "\u{2014}".to_string())
}

/// Privilege order for sorting: boundary (most privileged) → services → app →
/// builtin/unknown. Reads top-down from where powers enter the world.
fn layer_rank(layer: &str) -> u8 {
    match layer {
        "boundary" => 0,
        "services" => 1,
        "app" => 2,
        _ => 3,
    }
}

impl Analysis {
    fn from_diags(diagnostics: Vec<Diagnostic>) -> Self {
        Analysis {
            diagnostics,
            effects: Vec::new(),
            functions: Vec::new(),
            calls: Vec::new(),
        }
    }
}

impl FromPipelinePanic for Analysis {
    fn from_panic() -> Self {
        Analysis::from_diags(vec![diag_plain("internal error: the analysis pipeline panicked")])
    }
    fn from_spawn_error(msg: &str) -> Self {
        Analysis::from_diags(vec![diag_plain(&format!(
            "internal error: could not start the analysis worker: {msg}"
        ))])
    }
}

/// The checker pipeline — `program → elaborate`, error → one
/// `Diagnostic`. Empty `Vec` on success.
fn check_pipeline(source: String) -> Vec<Diagnostic> {
    let term = match locus::program(&source) {
        Ok(t) => t,
        Err(e) => return vec![diag_from_report(&locus::Report::parse_error(&e, &source))],
    };
    match locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term) {
        Ok(_) => Vec::new(),
        Err(e) => vec![diag_from_report(&locus::Report::type_error(&e))],
    }
}

impl EvalOutcome {
    /// A failed-before-run outcome carrying only diagnostics.
    fn from_diags(diagnostics: Vec<Diagnostic>) -> Self {
        EvalOutcome {
            result: None,
            effects: Vec::new(),
            diagnostics,
        }
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Run `f(input)` on a dedicated thread sized to
/// [`locus::PIPELINE_STACK_BYTES`] and join it — the same big-stack
/// worker `cmd_run` / `locus`'s `main` spawn, so deep elaboration +
/// staging cannot overflow the caller's (worker or GUI) stack. A panic
/// in the pipeline is reported through `R`'s error channel rather than
/// unwinding into the GUI.
fn run_on_pipeline_stack<T, R>(input: T, f: fn(T) -> R) -> R
where
    T: Send + 'static,
    R: Send + 'static + FromPipelinePanic,
{
    let handle = std::thread::Builder::new()
        .name("locus-ide-eval".into())
        .stack_size(locus::PIPELINE_STACK_BYTES)
        .spawn(move || f(input));
    match handle {
        Ok(h) => h.join().unwrap_or_else(|_| R::from_panic()),
        Err(e) => R::from_spawn_error(&e.to_string()),
    }
}

/// How a result type reports a worker spawn/panic — so
/// `run_on_pipeline_stack` is generic over `eval` (→ `EvalOutcome`) and
/// the checker (→ `Vec<Diagnostic>`).
trait FromPipelinePanic {
    fn from_panic() -> Self;
    fn from_spawn_error(msg: &str) -> Self;
}

impl FromPipelinePanic for EvalOutcome {
    fn from_panic() -> Self {
        EvalOutcome::from_diags(vec![diag_plain("internal error: the Locus pipeline panicked")])
    }
    fn from_spawn_error(msg: &str) -> Self {
        EvalOutcome::from_diags(vec![diag_plain(&format!(
            "internal error: could not start the eval worker: {msg}"
        ))])
    }
}

impl FromPipelinePanic for Vec<Diagnostic> {
    fn from_panic() -> Self {
        // A checker panic must not spam squiggles; report nothing.
        Vec::new()
    }
    fn from_spawn_error(_msg: &str) -> Self {
        Vec::new()
    }
}

impl FromPipelinePanic for Vec<String> {
    fn from_panic() -> Self {
        Vec::new()
    }
    fn from_spawn_error(_msg: &str) -> Self {
        Vec::new()
    }
}

/// Map a structured [`locus::Report`] error into an [`igui::Diagnostic`].
/// The `RN-Exxxx` code is folded into the message (igui's `Diagnostic`
/// has no code field), so the editor's status bar / squiggle shows it.
/// Type errors carry no span yet (`loc: None`) → default to `1:1`.
fn diag_from_report(report: &locus::Report) -> Diagnostic {
    match report {
        locus::Report::Error {
            code, message, loc, ..
        } => {
            let (line, column) = loc.unwrap_or((1, 1));
            Diagnostic {
                line,
                column,
                message: format!("{code}: {message}"),
            }
        }
        // `parse_error` / `type_error` only ever build `Report::Error`;
        // a non-error Report has no diagnostic to show.
        _ => diag_plain("unexpected non-error report"),
    }
}

/// A diagnostic for a plain string error (no code, no span) — winapi
/// resolution, staging, and runtime JIT failures. Anchored at `1:1`.
fn diag_plain(message: &str) -> Diagnostic {
    Diagnostic {
        line: 1,
        column: 1,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the eval/check **seam** headlessly (no GUI). The
    // GUI shell itself is not unit-testable, but the pipeline wiring is.

    #[test]
    fn eval_pure_expression_returns_value_with_empty_effects() {
        let mut s = LocusSession::new();
        let out = s.eval("let x = 40 in x + 2");
        assert_eq!(out.result, Some(42), "diagnostics: {:?}", out.diagnostics);
        assert!(
            out.effects.is_empty(),
            "a pure program touches nothing: {:?}",
            out.effects
        );
        assert!(out.diagnostics.is_empty());
        assert!(out.ok());
    }

    #[test]
    fn eval_console_writeln_runs_and_reports_world_effects() {
        let mut s = LocusSession::new();
        // `console_writeln` runs over raw Win32 — its row is the
        // boundary/world/memory powers. We assert it ran (some i64) and
        // that the manifest names the winapi + gc powers.
        let out = s.eval(r#"console_writeln "hi""#);
        assert!(
            out.result.is_some(),
            "console_writeln should run: {:?}",
            out.diagnostics
        );
        assert!(
            out.effects.iter().any(|e| e == "winapi"),
            "console output crosses the winapi boundary: {:?}",
            out.effects
        );
        assert!(
            out.effects.iter().any(|e| e == "gc"),
            "building the string allocates (gc): {:?}",
            out.effects
        );
    }

    #[test]
    fn checker_flags_a_bad_application_with_its_code() {
        // `1 2` — applying a non-function. A type error (`RN-E0201`).
        let diags = check_source("1 2");
        assert!(!diags.is_empty(), "a bad program must squiggle");
        assert!(
            diags.iter().any(|d| d.message.contains("RN-E0201")),
            "the type-error code is surfaced: {diags:?}"
        );
    }

    #[test]
    fn checker_flags_a_parse_error_with_a_location() {
        // `let x = in x` — a parse error (`RN-E0001`) with a real span.
        let diags = check_source("let x = in x");
        assert!(!diags.is_empty(), "a parse error must squiggle");
        let d = &diags[0];
        assert!(
            d.message.contains("RN-E0001"),
            "the parse-error code is surfaced: {d:?}"
        );
        assert_eq!(d.line, 1, "parse errors carry a line");
        assert!(d.column > 1, "…and a column: {d:?}");
    }

    #[test]
    fn checker_passes_a_good_program() {
        assert!(
            check_source("let x = 40 in x + 2").is_empty(),
            "a well-typed program has no diagnostics"
        );
        assert!(
            check_source(r#"console_writeln "hi""#).is_empty(),
            "a well-typed effectful program has no diagnostics either"
        );
    }

    // ── Phase 2a: the Graphics/Event services elaborate in the IDE host ──────
    //
    // These run the front half only (`check_source` / `effect_manifest`) — they
    // never JIT-run, because a real graphical program would block on its event
    // loop. They prove the services type-check with the right effect labels in
    // the *host*, end to end with the engine the IDE links.

    /// A minimal interactive graphical buffer (the Othello-shaped demo) type-
    /// checks — no squiggles — through the IDE's own checker.
    #[test]
    fn graphical_demo_type_checks_in_the_host() {
        let diags = check_source(DEMO_SRC);
        assert!(
            diags.is_empty(),
            "the graphical demo must type-check (no squiggles): {diags:?}"
        );
    }

    /// …and its effect manifest carries `graphics` + `event` (the sealed IDE-
    /// world capabilities) plus `gc` — the confinement the effects pane shows.
    #[test]
    fn graphical_demo_manifest_names_graphics_and_event() {
        let effects = effect_manifest(DEMO_SRC);
        assert!(
            effects.iter().any(|e| e == "graphics"),
            "the demo draws — `graphics` in the manifest: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| e == "event"),
            "the demo reads input — `event` in the manifest: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| e == "gc"),
            "the demo allocates the event sum + title (`gc`): {effects:?}"
        );
    }

    /// A graphical buffer that imports Graphics/Event but is run OUTSIDE this
    /// IDE host still elaborates (the services are pure Locus) — only the JIT
    /// link to `igui_*` is IDE-only. The manifest is the same here; the
    /// difference is purely at run time. (Front half only — no JIT.)
    const DEMO_SRC: &str = DEMO_SOURCE;
}

/// The bundled minimal interactive demo, shared by the host tests and
/// `examples/ide_demo.locus` / `gui_runit/ide_demo.locus`. Open a pane, set a
/// redraw tick, and on each frame clear + draw a 4×4 rect grid and a filled
/// circle that jumps to the last `MouseDown(x,y)` (a click places the dot). The
/// event loop is **tail recursion** — a self-tail-call lowers to a jump, so it
/// loops forever without growing the stack (cleaner than packing the state into
/// one loop variable; a multi-var loop can't share a `let` across its steps).
/// Effects are `{graphics, event, gc, mem}` (mem from the title/byte marshal).
pub const DEMO_SOURCE: &str = r#"
-- Minimal interactive graphical Locus demo for the IDE pane.
-- Each frame: clear + a 4x4 grid of rects + a circle; a click moves the dot,
-- Close exits. Effects: {graphics, event, gc, mem}.

let pane = open_window "Locus demo" in
let _ = set_redraw_rate pane 16 in

-- Draw one frame with the dot centred at (dotx, doty).
let frame = fn dotx: Int => fn doty: Int =>
  let _ = gfx_begin pane in
  let _ = clear 0.10 0.11 0.14 1.0 in
  let _ =
    loop r = 0 while r < 4 do
      let _ =
        loop c = 0 while c < 4 do
          let x0 = toFloat (8 + c * 36) in
          let y0 = toFloat (8 + r * 36) in
          let _ = fill_rect x0 y0 (x0 + 30.0) (y0 + 30.0) 0.20 0.45 0.70 1.0 in
          c + 1
        else c
      in
      r + 1
    else r
  in
  let _ = fill_circle (toFloat dotx) (toFloat doty) 12.0 1.0 0.85 0.25 1.0 in
  gfx_submit ()
in

-- Event loop as tail recursion: draw, poll one event, recurse with the new dot.
let rec spin : Int -> Int -> Int ! {graphics, event, gc, mem} =
  fn dotx: Int => fn doty: Int =>
    let _ = frame dotx doty in
    match poll_event 16 with
    | MouseDown(x, y) => spin x y
    | Close           => 0
    | _               => spin dotx doty
in
spin 80 80
"#;
