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
    // parse + graft the stdlib prelude AND the service plugins — the same module
    // set `analyze`/`run`/`effects` use, so a plugin-backed program (e.g. one
    // using `db_open_memory`) resolves here too and the run path agrees with the
    // Analyze report. A parse error is the CLI's structured diagnostic.
    let modules = locus_llvm::plugins::plugin_grafted_modules();
    let term = match locus::program_with_stdlib(&source, &modules).map(|(t, _)| t) {
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
        // The service plugins' C-ABI symbols (e.g. the sqlite shim), so a grafted
        // boundary's externs resolve in the JIT — mirroring the CLI `run` path.
        .chain(locus_llvm::plugins::plugin_symbols())
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
    let modules = locus_llvm::plugins::plugin_grafted_modules();
    let Ok((term, _)) = locus::program_with_stdlib(&source, &modules) else {
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
    let modules = locus_llvm::plugins::plugin_grafted_modules();
    let term = match locus::program_with_stdlib(source, &modules).map(|(t, _)| t) {
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

/// The analysis result types are the **shared** ones from [`locus::analysis`] —
/// the single source of truth behind the CLI `effects` manifest, the MCP report,
/// and this report pane — so layer/effect/data attribution is computed in one
/// place. Re-exported here for the report renderer.
pub use locus::analysis::{CallEdge, DataRow, EffectInfo, FnInfo};

/// The full static analysis of a buffer: parse/type **diagnostics** (if it does
/// not elaborate), the program's root **effect manifest** (each effect tagged
/// with the layer it enters at), the per-function **table** ([`FnInfo`]), the
/// **call graph** ([`CallEdge`]s among those functions), and the **data-access
/// tree** ([`DataRow`]s). Built from a single elaboration — no JIT, no run.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub effects: Vec<EffectInfo>,
    pub functions: Vec<FnInfo>,
    pub calls: Vec<CallEdge>,
    pub data_access: Vec<DataRow>,
}

/// Statically analyze `source`: elaborate it once (no JIT) and report its
/// diagnostics, effect manifest, and the table of named functions it uses with
/// the module / layer / arguments / effect of each — so the origin of every
/// power in the manifest is visible. Runs on the big-stack pipeline worker.
pub fn analyze(source: &str) -> Analysis {
    run_on_pipeline_stack(source.to_string(), analyze_pipeline)
}

fn analyze_pipeline(source: String) -> Analysis {
    // parse + graft + resolve + elaborate (the front half of `eval_pipeline`).
    // Graft the stdlib **and the service plugins** — the same module set `run`
    // and `effects` use — so plugin surfaces like `db_open_memory` resolve and
    // their sealed effects (e.g. `{ sqlite }`) show up in the report.
    let modules = locus_llvm::plugins::plugin_grafted_modules();
    let (term, user_modules) = match locus::program_with_stdlib(&source, &modules) {
        Ok(pair) => pair,
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

    // The shared analysis engine (the single source of truth behind the CLI /
    // MCP reports too): the layer-tagged effect manifest, the per-function
    // origin table, the call graph, and the data-access tree. It reads layers
    // from the grafted module declarations (stdlib + plugins + the user's own).
    let mut decls = locus_llvm::plugins::plugin_grafted_module_decls();
    decls.extend(user_modules);
    let report = locus::analysis::analyze(&tree, &decls, &source);
    Analysis {
        diagnostics: Vec::new(),
        effects: report.effects,
        functions: report.functions,
        calls: report.calls,
        data_access: report.data_access,
    }
}

impl Analysis {
    fn from_diags(diagnostics: Vec<Diagnostic>) -> Self {
        Analysis {
            diagnostics,
            ..Default::default()
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
    // Same plugin-grafted set as analyze/eval, so the live editor checker does
    // not squiggle a valid plugin-backed program (e.g. `db_open_memory`) that
    // F6/Analyze accepts.
    let modules = locus_llvm::plugins::plugin_grafted_modules();
    let term = match locus::program_with_stdlib(&source, &modules).map(|(t, _)| t) {
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
    fn compiler_views_run_back_to_back() {
        // The bug: running one compiler-explorer view (ANF/ASM/LLVM) breaks the
        // next — a process-global consumed on first use. Reproduce by running
        // several in sequence and asserting EACH lowers.
        // (The reported "ANF breaks ASM" was a UI bug — the result pane re-
        // lowering its own dump, fixed in fedit. The pipeline itself is fine:
        // run several views back-to-back, after a JIT eval, and assert each.)
        let mut s = LocusSession::new();
        let _ = s.eval("let x = 40 in x + 2");
        let src = "let p = open_window \"t\" in \
                   let _ = gfx_begin p in \
                   let _ = fill_rect 1.0 1.0 9.0 9.0 0.2 0.4 0.7 1.0 in \
                   let _ = gfx_submit () in 0";
        for (i, view) in [
            CompilerView::Anf,
            CompilerView::Asm,
            CompilerView::Anf,
            CompilerView::Llvm,
            CompilerView::Asm,
        ]
        .into_iter()
        .enumerate()
        {
            let v = compiler_view(src, view);
            assert!(
                v.diagnostics.is_empty(),
                "view #{i} ({}) did not lower: {:?}",
                view.title(),
                v.diagnostics
            );
        }
    }

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
        // `console_writeln` runs over raw Win32, but the Console service **seals**
        // winapi: the app's manifest is `{ gc }` (allocating the string), and the
        // raw winapi power is confined to the boundary — it does not surface here.
        let out = s.eval(r#"console_writeln "hi""#);
        assert!(
            out.result.is_some(),
            "console_writeln should run: {:?}",
            out.diagnostics
        );
        assert!(
            out.effects.iter().any(|e| e == "gc"),
            "building the string allocates (gc): {:?}",
            out.effects
        );
        assert!(
            !out.effects.iter().any(|e| e == "winapi"),
            "winapi is sealed at the Console service — confined, not in the app manifest: {:?}",
            out.effects
        );
    }

    #[test]
    fn eval_runs_an_in_memory_db_program_end_to_end() {
        // The IDE eval path now grafts the service plugins (like analyze) AND
        // resolves their C-ABI symbols in the JIT, so a plugin-backed program
        // runs — not just analyzes. Opens an in-memory SQLite db, inserts, reads
        // it back. Guards the eval/analyze consistency the review flagged.
        let mut s = LocusSession::new();
        let out = s.eval(
            r#"
let c = db_open_memory () in
let _ = db_exec c "CREATE TABLE t (x INTEGER)" in
let _ = db_exec c "INSERT INTO t VALUES (42)" in
let q = db_prepare c "SELECT x FROM t" in
let rs = db_run_query q in
let v = db_get_int rs 0 0 in
let _ = db_free rs in
let _ = db_finalize q in
let _ = db_close c in
v
"#,
        );
        assert_eq!(
            out.result,
            Some(42),
            "the in-memory db program runs and reads back the row: {:?}",
            out.diagnostics
        );
        assert!(
            out.effects.iter().any(|e| e == "sqlite"),
            "the run manifest names the sqlite data effect: {:?}",
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
