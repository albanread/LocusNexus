//! Report rendering — an [`EvalOutcome`] → Markdown (with a Mermaid
//! diagram) for the IDE's `doc_pane`.
//!
//! Pure string-building, no GUI: the host pushes the result into
//! `igui::doc_pane::set_report`, which renders it through the `docpane`
//! core. Kept here (not in `main.rs`) so it is unit-testable headlessly.
//!
//! The report surfaces the run's result, its **effect manifest** (the
//! powers the program named), and a Mermaid flow showing those powers
//! reaching the world — confinement made visible, the IDE's thesis drawn.

#![cfg(windows)]

use std::fmt::Write as _;

use crate::session::{analyze, compiler_view, Analysis, CompilerView, FnRow};
use crate::EvalOutcome;

/// Render the Markdown effect/capability report for an eval outcome
/// (a *run* — the program executed; this surfaces its result too).
pub fn eval_report(outcome: &EvalOutcome) -> String {
    let mut md = String::new();
    md.push_str("# Run report\n\n");

    if let Some(v) = outcome.result {
        let _ = writeln!(md, "**Result:** `{v}`\n");
        effects_section(&mut md, &outcome.effects);
    } else if outcome.diagnostics.is_empty() {
        md.push_str("The program did not produce a result.\n");
    } else {
        diagnostics_section(&mut md, &outcome.diagnostics);
    }
    md
}

/// Render the Markdown **analysis** report for a buffer — elaborate-only,
/// no run. Surfaces the effect manifest (the powers the program names) and
/// the confinement diagram, or the parse/type diagnostics if it does not
/// elaborate. The headless counterpart to [`eval_report`]: no result line,
/// because nothing executed.
pub fn analyze_report(source: &str) -> String {
    let mut md = String::new();
    md.push_str("# Analysis\n\n*Static — the program was elaborated, not run.*\n\n");

    let a = analyze(source);
    if !a.diagnostics.is_empty() {
        diagnostics_section(&mut md, &a.diagnostics);
        return md;
    }
    effects_section(&mut md, &a.effects);
    functions_section(&mut md, &a.functions);
    calls_section(&mut md, &a);
    md
}

/// Render a compiler-explorer view (ANF IR / LLVM IR / Assembly) for `source`
/// as a Markdown report: a titled, fenced code block with the dump — or the
/// diagnostics if the program does not lower. Drives the Locus → Show … menu.
pub fn lowering_report(source: &str, view: CompilerView) -> String {
    let out = compiler_view(source, view);
    let mut md = String::new();
    let _ = writeln!(md, "# {}\n", view.title());
    if !out.diagnostics.is_empty() {
        md.push_str("*The program did not lower:*\n\n");
        diagnostics_section(&mut md, &out.diagnostics);
        return md;
    }
    md.push_str(
        "*Static — the front end + lowering, no run. This is exactly what the \
         JIT/AOT build from.*\n\n",
    );
    let _ = writeln!(md, "```{}", view.lang());
    md.push_str(&out.text);
    if !out.text.ends_with('\n') {
        md.push('\n');
    }
    md.push_str("```\n");
    md
}

/// The call graph among the table's functions, as a Mermaid flowchart
/// (`caller --> callee`).
///
/// **Reduction for large programs:** the user's own (app-layer) functions are
/// individual nodes, but every stdlib function is collapsed into a single node
/// per *module* (a stadium-shaped `Module (layer)` node). So the many calls
/// into `String`/`Graphics`/… converge on one node each instead of cluttering
/// the graph with service leaves — you see your program's structure and which
/// services it reaches. Node ids are prefixed so a name that collides with a
/// Mermaid keyword can't break the diagram. Omitted when there are no edges.
fn calls_section(md: &mut String, a: &Analysis) {
    use std::collections::{BTreeMap, BTreeSet};

    if a.calls.is_empty() {
        return;
    }
    // name -> (module, layer) for every table function.
    let info: BTreeMap<&str, (&str, &str)> = a
        .functions
        .iter()
        .map(|f| (f.name.as_str(), (f.module.as_str(), f.layer.as_str())))
        .collect();

    // The node a function maps to: an app/user function keeps its own node;
    // any other (stdlib) function collapses to its module node. Returns
    // `(node_id, label)`.
    let node_of = |name: &str| -> (String, String) {
        match info.get(name) {
            Some((module, layer)) if *layer != "app" => {
                let id = format!("mod_{}", sanitize(module));
                (id, format!("{module} ({layer})"))
            }
            Some(_) => (format!("fn_{}", sanitize(name)), name.to_string()),
            // Not in the table (shouldn't happen for edge endpoints) — own node.
            None => (format!("fn_{}", sanitize(name)), name.to_string()),
        }
    };

    let mut nodes: BTreeMap<String, (String, bool)> = BTreeMap::new(); // id -> (label, is_module)
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    for (caller, callee) in &a.calls {
        let (cid, clabel) = node_of(caller);
        let (eid, elabel) = node_of(callee);
        nodes.insert(cid.clone(), (clabel, cid.starts_with("mod_")));
        nodes.insert(eid.clone(), (elabel, eid.starts_with("mod_")));
        if cid != eid {
            edges.insert((cid, eid));
        }
    }
    if edges.is_empty() {
        return;
    }

    md.push_str("\n## Call graph\n\n");
    md.push_str("*Your functions are individual nodes; stdlib calls collapse to one node per service module.*\n\n");
    md.push_str("```mermaid\nflowchart LR\n");
    for (id, (label, is_module)) in &nodes {
        if *is_module {
            // Stadium shape + amber fill marks a collapsed service module.
            let _ = writeln!(md, "  {id}([\"{label}\"])");
            // docpane reads node colour from a `%% @node` annotation (not the
            // `style` statement): dark-amber fill + bright-amber stroke, which
            // reads against the fixed light node text.
            let _ = writeln!(md, "  %% @node {id} fill=\"#5C3D10\" stroke=\"#E8A33D\"");
        } else {
            let _ = writeln!(md, "  {id}[\"{label}\"]");
        }
    }
    for (c, e) in &edges {
        let _ = writeln!(md, "  {c} --> {e}");
    }
    md.push_str("```\n");
}

/// Make an identifier safe as a Mermaid node-id suffix (`[A-Za-z0-9_]`).
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// The per-function origin table: for each named function the program uses,
/// where it is defined (module + layer), its argument types, and the effect it
/// carries — so the manifest's powers can be traced to the functions that bring
/// them in.
fn functions_section(md: &mut String, fns: &[FnRow]) {
    md.push_str("\n## Functions — where the effects originate\n\n");
    if fns.is_empty() {
        md.push_str("_No named library functions referenced._\n");
        return;
    }
    md.push_str("| Module | Function | Layer | Arguments | Effect |\n");
    md.push_str("|---|---|---|---|---|\n");
    for f in fns {
        let args = if f.args.is_empty() {
            "()".to_string()
        } else {
            f.args.join(", ")
        };
        let eff = if f.effects.is_empty() {
            "\u{2014}".to_string()
        } else {
            f.effects.join(", ")
        };
        // Guard the few characters that would break a Markdown table cell.
        let args = args.replace('|', "\\|");
        let _ = writeln!(
            md,
            "| {} | `{}` | {} | {} | {} |",
            f.module, f.name, f.layer, args, eff
        );
    }
}

/// Shared renderer: the "Effects" list + the Mermaid "Confinement" flow
/// (the program reaching the world only through the labels in its row).
fn effects_section(md: &mut String, effects: &[String]) {
    if effects.is_empty() {
        md.push_str(
            "## Effects\n\nNone — this program is **pure** (`{}`). It touches no \
             world capability.\n",
        );
        return;
    }
    md.push_str("## Effects (capabilities named)\n\n");
    for e in effects {
        let _ = writeln!(md, "- `{e}`");
    }
    md.push_str("\n## Confinement\n\n```mermaid\nflowchart LR\n");
    md.push_str("  prog[\"program\"]\n");
    for (i, e) in effects.iter().enumerate() {
        let _ = writeln!(md, "  prog --> e{i}[\"{e}\"]");
    }
    md.push_str("```\n");
}

/// Shared renderer: a fenced block of `line:col  message` diagnostics.
fn diagnostics_section(md: &mut String, diagnostics: &[igui::Diagnostic]) {
    md.push_str("## Diagnostics\n\n```\n");
    for d in diagnostics {
        let _ = writeln!(md, "{}:{}  {}", d.line, d.column, d.message);
    }
    md.push_str("```\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use igui::Diagnostic;

    #[test]
    fn pure_program_report_names_no_capability() {
        let out = EvalOutcome {
            result: Some(42),
            effects: Vec::new(),
            diagnostics: Vec::new(),
        };
        let md = eval_report(&out);
        assert!(md.contains("`42`"), "result shown: {md}");
        assert!(md.contains("pure"), "a pure program says so: {md}");
        assert!(!md.contains("```mermaid"), "no confinement diagram when pure: {md}");
    }

    #[test]
    fn effectful_program_report_lists_powers_and_draws_them() {
        let out = EvalOutcome {
            result: Some(0),
            effects: vec!["gc".into(), "graphics".into(), "event".into()],
            diagnostics: Vec::new(),
        };
        let md = eval_report(&out);
        assert!(md.contains("- `graphics`"), "each power is listed: {md}");
        assert!(md.contains("```mermaid"), "the confinement flow is drawn: {md}");
        // Every effect label reaches the world from the program node.
        assert!(md.matches("prog -->").count() == 3, "one edge per power: {md}");
        assert!(md.contains("\"event\""), "the label is a node: {md}");
    }

    #[test]
    fn analyze_report_names_powers_without_running() {
        // `console_writeln` elaborates with the winapi/gc powers; analysis
        // reports them without executing (no result line, no console I/O).
        let md = analyze_report(r#"console_writeln "hi""#);
        assert!(md.contains("Static"), "analysis is marked static: {md}");
        assert!(!md.contains("Result:"), "nothing ran, so no result: {md}");
        assert!(md.contains("- `winapi`"), "the winapi power is named: {md}");
        assert!(md.contains("```mermaid"), "confinement flow drawn: {md}");
    }

    #[test]
    fn analyze_report_pure_program_says_pure() {
        let md = analyze_report("let x = 40 in x + 2");
        assert!(md.contains("pure"), "a pure buffer analyzes as pure: {md}");
        assert!(!md.contains("```mermaid"), "no flow when pure: {md}");
    }

    #[test]
    fn analyze_report_bad_program_shows_diagnostics() {
        let md = analyze_report("1 2");
        assert!(md.contains("Diagnostics"), "a type error gets a section: {md}");
        assert!(md.contains("RN-E0201"), "the code surfaces: {md}");
    }

    #[test]
    fn analyze_report_table_attributes_a_library_function() {
        let md = analyze_report(r#"console_writeln "hi""#);
        assert!(md.contains("## Functions"), "the functions table section is present: {md}");
        assert!(
            md.contains("console_writeln"),
            "the called library function is listed in the table: {md}"
        );
        // Its origin module is named (Console), so the user sees where it lives.
        assert!(md.contains("Console"), "the function's module is attributed: {md}");
        // The internal `console_writeln_op` operation label is presented as the
        // friendly service effect `console`.
        assert!(
            md.lines().any(|l| l.contains("`console_writeln`") && l.contains("console |")),
            "the effect reads as the friendly `console`, not `*_op`: {md}"
        );
        assert!(
            !md.contains("console_writeln_op"),
            "the raw *_op label is not shown: {md}"
        );
    }

    #[test]
    fn analyze_report_attributes_nested_user_functions_to_the_program() {
        // A user helper defined *inside* another function's body must still be
        // attributed to "(this program)" / app — not left with a blank module.
        let src = r#"
let outer = fn k: Int =>
  let helper = fn z: Int => z + k in
  helper 5
in
outer 3
"#;
        let md = analyze_report(src);
        assert!(
            md.lines().any(|l| l.contains("`helper`")
                && l.contains("(this program)")
                && l.contains("app")),
            "the nested helper is attributed to the program / app layer: {md}"
        );
        // No function row may have a blank (em-dash) module or layer.
        for l in md.lines().filter(|l| l.starts_with("| ") && l.contains('`')) {
            let cells: Vec<&str> = l.split('|').map(|c| c.trim()).collect();
            assert_ne!(cells.get(1), Some(&"\u{2014}"), "module is attributed: {l}");
            assert_ne!(cells.get(3), Some(&"\u{2014}"), "layer is attributed: {l}");
        }
    }

    #[test]
    fn analyze_report_draws_the_call_graph() {
        // dbl <- quad <- main; the call graph should show quad --> dbl as a
        // Mermaid edge (prefixed node ids, bare-name labels).
        let src = r#"
let dbl = fn x: Int => x + x in
let quad = fn x: Int => dbl (dbl x) in
quad 3
"#;
        let md = analyze_report(src);
        assert!(md.contains("## Call graph"), "a call-graph section is drawn: {md}");
        assert!(
            md.contains("fn_quad --> fn_dbl"),
            "quad calls dbl is an edge: {md}"
        );
        assert!(md.contains("fn_dbl[\"dbl\"]"), "nodes are labelled by name: {md}");
    }

    #[test]
    fn analyze_report_no_call_graph_for_a_lone_function() {
        // A single function that calls nothing in the table: no graph section.
        let md = analyze_report("let id = fn x: Int => x in id 5");
        assert!(!md.contains("## Call graph"), "no graph when there are no edges: {md}");
    }

    #[test]
    fn call_graph_collapses_stdlib_calls_to_one_module_node() {
        // Two user functions both call into Graphics; the graph should show a
        // single collapsed `mod_Graphics` service node, not each draw call.
        let src = r#"
let pane = open_window "d" in
let a = fn n: Int => let _ = gfx_begin pane in fill_rect 1.0 2.0 3.0 4.0 0.2 0.4 0.7 1.0 in
let b = fn n: Int => gfx_submit () in
let _ = a 1 in b 2
"#;
        let md = analyze_report(src);
        assert!(md.contains("mod_Graphics([\"Graphics (services)\"])"), "collapsed service node: {md}");
        assert!(md.contains("fn_a --> mod_Graphics"), "user fn -> service edge: {md}");
        // No individual fill_rect/gfx_* nodes leak into the graph.
        assert!(!md.contains("fn_fill_rect"), "stdlib leaves are collapsed: {md}");
        // The service node is tinted amber via a docpane @node annotation.
        assert!(
            md.contains("%% @node mod_Graphics fill=\"#5C3D10\""),
            "service node carries the amber fill annotation: {md}"
        );
    }

    #[test]
    fn lowering_report_anf_dumps_the_ir() {
        let md = lowering_report("let dbl = fn x: Int => x + x in dbl 21", CompilerView::Anf);
        assert!(md.contains("# ANF IR"), "titled: {md}");
        assert!(md.contains("```text"), "fenced as text: {md}");
        assert!(md.contains("lam x"), "the ANF shows the lambda: {md}");
    }

    #[test]
    fn lowering_report_llvm_and_asm_emit_nonempty_dumps() {
        let llvm = lowering_report("40 + 2", CompilerView::Llvm);
        assert!(llvm.contains("# LLVM IR") && llvm.contains("```llvm"), "{llvm}");
        assert!(llvm.contains("__locus_main"), "LLVM IR has the entry fn: {llvm}");
        let asm = lowering_report("40 + 2", CompilerView::Asm);
        assert!(asm.contains("# Assembly") && asm.contains("```asm"), "{asm}");
        assert!(asm.contains("__locus_main"), "asm has the entry symbol: {asm}");
    }

    #[test]
    fn lowering_report_bad_program_shows_diagnostics() {
        let md = lowering_report("1 2", CompilerView::Anf);
        assert!(md.contains("did not lower"), "explains the failure: {md}");
        assert!(md.contains("Diagnostics") && md.contains("RN-E0201"), "{md}");
    }

    #[test]
    fn analyze_report_table_traces_graphics_effects_to_their_module() {
        // A graphical program: the table should attribute each draw call to the
        // Graphics service (services layer) and show the `graphics` effect — so
        // the `graphics` in the manifest is traceable to the functions that
        // bring it in.
        let g = r#"
let pane = open_window "demo" in
let _ = gfx_begin pane in
let _ = fill_rect 1.0 2.0 3.0 4.0 0.2 0.4 0.7 1.0 in
gfx_submit ()
"#;
        let md = analyze_report(g);
        assert!(md.contains("| Module | Function | Layer | Arguments | Effect |"), "{md}");
        // fill_rect: Graphics / services / graphics effect.
        assert!(
            md.contains("| Graphics | `fill_rect` | services |"),
            "fill_rect is attributed to Graphics/services: {md}"
        );
        assert!(
            md.lines().any(|l| l.contains("`fill_rect`") && l.contains("graphics")),
            "fill_rect carries the graphics effect: {md}"
        );
        // Argument types are shown (fill_rect takes Floats).
        assert!(
            md.lines().any(|l| l.contains("`fill_rect`") && l.contains("Float")),
            "fill_rect's argument types are shown: {md}"
        );
    }

    #[test]
    fn failed_run_report_shows_diagnostics() {
        let out = EvalOutcome {
            result: None,
            effects: Vec::new(),
            diagnostics: vec![Diagnostic {
                line: 3,
                column: 7,
                message: "RN-E0201: not a function".into(),
            }],
        };
        let md = eval_report(&out);
        assert!(md.contains("Diagnostics"), "errors get a section: {md}");
        assert!(md.contains("3:7  RN-E0201"), "the diagnostic is rendered: {md}");
        assert!(!md.contains("Result:"), "no result line on a failed run: {md}");
    }
}
