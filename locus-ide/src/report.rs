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

use crate::session::{
    analyze, compiler_view, Analysis, CompilerView, DataRow, EffectInfo, FnInfo,
};
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
    effects_manifest(&mut md, &a.effects);
    functions_section(&mut md, &a.functions);
    calls_section(&mut md, &a);
    data_access_section(&mut md, &a.data_access);
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

    let any_recursion = a.functions.iter().any(|f| f.recursive);
    if a.calls.is_empty() && !any_recursion {
        return;
    }
    // name -> (module, layer-name, layer-rank) for every table function.
    let info: BTreeMap<&str, (&str, &str, Option<u8>)> = a
        .functions
        .iter()
        .map(|f| (f.name.as_str(), (f.module.as_str(), f.layer.as_str(), f.layer_rank)))
        .collect();

    // The node a function maps to: an app/user (or builtin) function keeps its
    // own node; a boundary/services function collapses to its module node, so
    // the many stdlib/plugin leaves converge on one node per service. Returns
    // `(node_id, label, layer_rank)`.
    let node_of = |name: &str| -> (String, String, Option<u8>) {
        match info.get(name) {
            Some((module, layer, rank)) if matches!(rank, Some(0) | Some(1)) => {
                (format!("mod_{}", sanitize(module)), format!("{module} ({layer})"), *rank)
            }
            Some((_, _, rank)) => (format!("fn_{}", sanitize(name)), name.to_string(), *rank),
            None => (format!("fn_{}", sanitize(name)), name.to_string(), None),
        }
    };

    let mut nodes: BTreeMap<String, (String, Option<u8>, bool)> = BTreeMap::new(); // id -> (label, rank, is_module)
    // (caller_id, callee_id) -> (union of crossing effects, crosses a layer?)
    let mut edges: BTreeMap<(String, String), (BTreeSet<String>, bool)> = BTreeMap::new();
    for e in &a.calls {
        let (cid, clabel, crank) = node_of(&e.caller);
        let (eid, elabel, erank) = node_of(&e.callee);
        nodes.insert(cid.clone(), (clabel, crank, cid.starts_with("mod_")));
        nodes.insert(eid.clone(), (elabel, erank, eid.starts_with("mod_")));
        if cid != eid {
            let entry = edges.entry((cid, eid)).or_default();
            for fx in &e.effects {
                entry.0.insert(fx.clone());
            }
            entry.1 |= e.crosses_layer;
        }
    }
    // Recursion: a self-loop on each recursive function that has its own node
    // (a collapsed module node hides intra-module recursion, so skip those).
    for f in &a.functions {
        if f.recursive {
            let (id, label, rank) = node_of(&f.name);
            if !id.starts_with("mod_") {
                nodes.insert(id.clone(), (label, rank, false));
                edges.entry((id.clone(), id)).or_default();
            }
        }
    }
    if edges.is_empty() {
        return;
    }

    md.push_str("\n## Call graph\n\n");
    md.push_str(
        "*Your functions are individual nodes (coloured by layer: boundary red, \
         services amber, app blue); boundary/services calls collapse to one node \
         per service module. An edge is labelled with the powers that flow across \
         it; a **bold** `==>` edge crosses a layer; `↻` marks recursion.*\n\n",
    );
    md.push_str("```mermaid\nflowchart LR\n");
    for (id, (label, rank, is_module)) in &nodes {
        if *is_module {
            let _ = writeln!(md, "  {id}([\"{label}\"])");
        } else {
            let _ = writeln!(md, "  {id}[\"{label}\"]");
        }
        // docpane reads node colour from a `%% @node` annotation (not `style`).
        let (fill, stroke) = layer_color(*rank);
        let _ = writeln!(md, "  %% @node {id} fill=\"{fill}\" stroke=\"{stroke}\"");
    }
    for ((c, e), (fx, crosses)) in &edges {
        if c == e {
            let _ = writeln!(md, "  {c} -->|↻| {e}");
        } else {
            let arrow = if *crosses { "==>" } else { "-->" };
            if fx.is_empty() {
                let _ = writeln!(md, "  {c} {arrow} {e}");
            } else {
                // Quote the edge label: an effect like `exn[Overflow]` contains a
                // `[` that, unquoted, breaks the whole Mermaid flowchart parse.
                let label = fx.iter().cloned().collect::<Vec<_>>().join(", ");
                let _ = writeln!(md, "  {c} {arrow}|\"{label}\"| {e}");
            }
        }
    }
    md.push_str("```\n");
}

/// Make an identifier safe as a Mermaid node-id suffix (`[A-Za-z0-9_]`).
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// The per-function origin table, **grouped by privilege layer**: for each named
/// function the program uses, where it is defined (module), its argument types,
/// and the effect it carries — so the manifest's powers can be traced to the
/// functions that bring them in, and you see at a glance which layer each lives
/// at. The rows arrive pre-sorted by layer rank, so a subheading is emitted each
/// time the layer changes. A `↻` marks a self-recursive function.
fn functions_section(md: &mut String, fns: &[FnInfo]) {
    md.push_str("\n## Functions — where the effects originate\n\n");
    if fns.is_empty() {
        md.push_str("_No named library functions referenced._\n");
        return;
    }
    md.push_str(
        "*Grouped by privilege layer — boundary (0) mints raw powers, services (1) \
         seal and re-export them, app (2) is your own code.*\n",
    );
    let mut current: Option<Option<u8>> = None;
    for f in fns {
        if current != Some(f.layer_rank) {
            current = Some(f.layer_rank);
            let heading = match f.layer_rank {
                Some(r) => format!("Layer {r} — {}", f.layer),
                None => "Builtins".to_string(),
            };
            let _ = write!(
                md,
                "\n### {heading}\n\n| Function | Module | Arguments | Effect |\n|---|---|---|---|\n"
            );
        }
        let name = if f.recursive {
            format!("`{}` ↻", f.name)
        } else {
            format!("`{}`", f.name)
        };
        let args = if f.args.is_empty() {
            "()".to_string()
        } else {
            f.args.join(", ")
        }
        .replace('|', "\\|");
        let eff = if f.effects.is_empty() {
            "\u{2014}".to_string()
        } else {
            f.effects.join(", ")
        };
        let _ = writeln!(md, "| {name} | {} | {args} | {eff} |", f.module);
    }
}

/// The colour (docpane `%% @node` fill / stroke) for a layer rank — boundary
/// (raw power) reads red, services amber, app blue, a builtin/unknown gray. The
/// docpane reads colour from this annotation, not a Mermaid `style` statement.
fn layer_color(rank: Option<u8>) -> (&'static str, &'static str) {
    match rank {
        Some(0) => ("#5C1010", "#E85C5C"),
        Some(1) => ("#5C3D10", "#E8A33D"),
        Some(2) => ("#103D5C", "#3DA3E8"),
        _ => ("#333333", "#888888"),
    }
}

/// `"0 · boundary"` etc. for a table cell; bare layer name when the rank is
/// unknown (a builtin).
fn layer_tag(rank: Option<u8>, name: &str) -> String {
    match rank {
        Some(r) => format!("{r} · {name}"),
        None => name.to_string(),
    }
}

fn layer_name(rank: u8) -> &'static str {
    match rank {
        0 => "boundary",
        1 => "services",
        2 => "app",
        _ => "?",
    }
}

/// The **effect manifest** (the powers the program names), each tagged with the
/// **layer it enters at** (0 boundary · 1 services · 2 app) and a gloss, plus the
/// Mermaid confinement flow with nodes coloured by that layer.
fn effects_manifest(md: &mut String, effects: &[EffectInfo]) {
    if effects.is_empty() {
        md.push_str(
            "## Effects\n\nNone — this program is **pure** (`{}`). It touches no \
             world capability.\n",
        );
        return;
    }
    md.push_str("## Effects (capabilities named)\n\n");
    md.push_str(
        "*Each power is tagged with the layer it enters the program at — \
         0 boundary · 1 services · 2 app.*\n\n",
    );
    for e in effects {
        let layer = match e.layer {
            Some(r) => format!("layer {r} ({})", layer_name(r)),
            None => "cross-cutting".to_string(),
        };
        let data = if e.is_data { " · **data**" } else { "" };
        let _ = writeln!(
            md,
            "- `{}` — {} · {}{}  \n  _{}_",
            e.label, layer, e.category, data, e.gloss
        );
    }
    md.push_str("\n## Confinement\n\n```mermaid\nflowchart LR\n");
    md.push_str("  prog[\"program\"]\n");
    for (i, e) in effects.iter().enumerate() {
        let _ = writeln!(md, "  prog --> e{i}([\"{}\"])", e.label);
        let (fill, stroke) = layer_color(e.layer);
        let _ = writeln!(md, "  %% @node e{i} fill=\"{fill}\" stroke=\"{stroke}\"");
    }
    md.push_str("```\n");
}

/// The **data-access tree**: which functions touch a data store (a SQL DB, the
/// credential vault, the filesystem, raw memory) or mutable state, and the
/// boundary provider behind each effect. Omitted when the program touches no
/// data. `gc` (allocation) is excluded as ubiquitous.
fn data_access_section(md: &mut String, rows: &[DataRow]) {
    use std::collections::BTreeSet;
    if rows.is_empty() {
        return;
    }
    md.push_str("\n## Data access\n\n");
    md.push_str(
        "*Which functions read or write a data store — a SQL database, the \
         credential vault, the filesystem, raw memory — or mutable state, and the \
         boundary provider behind each. Allocation (`gc`) is excluded as \
         ubiquitous.*\n\n",
    );
    md.push_str("| Function | Layer | Data effect | Provider |\n|---|---|---|---|\n");
    for r in rows {
        for (eff, prov) in &r.providers {
            let _ = writeln!(
                md,
                "| `{}` | {} | `{}` | {} |",
                r.function,
                layer_tag(r.layer_rank, &r.layer),
                eff,
                prov
            );
        }
    }
    // The tree: function (its layer colour) → data effect (gray) → boundary
    // provider (red). Each node emitted once.
    md.push_str("\n```mermaid\nflowchart LR\n");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    for r in rows {
        let fnid = format!("fn_{}", sanitize(&r.function));
        if seen.insert(fnid.clone()) {
            let _ = writeln!(md, "  {fnid}[\"{}\"]", r.function);
            let (fill, stroke) = layer_color(r.layer_rank);
            let _ = writeln!(md, "  %% @node {fnid} fill=\"{fill}\" stroke=\"{stroke}\"");
        }
        for (eff, prov) in &r.providers {
            let effid = format!("data_{}", sanitize(eff));
            if seen.insert(effid.clone()) {
                let _ = writeln!(md, "  {effid}[\"{eff}\"]");
                let _ = writeln!(md, "  %% @node {effid} fill=\"#2B2B2B\" stroke=\"#9090A0\"");
            }
            edges.insert((fnid.clone(), effid.clone()));
            if prov != "\u{2014}" {
                let provid = format!("prov_{}", sanitize(prov));
                if seen.insert(provid.clone()) {
                    let _ = writeln!(md, "  {provid}([\"{prov}\"])");
                    let (fill, stroke) = layer_color(Some(0));
                    let _ = writeln!(md, "  %% @node {provid} fill=\"{fill}\" stroke=\"{stroke}\"");
                }
                edges.insert((effid, provid));
            }
        }
    }
    for (a, b) in &edges {
        let _ = writeln!(md, "  {a} --> {b}");
    }
    md.push_str("```\n");
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
        // `console_writeln` elaborates with the `gc` power; `winapi` is sealed at
        // the Console service, so the app manifest names `gc` (the raw winapi is
        // confined to the boundary, not surfaced here). Reported without running.
        let md = analyze_report(r#"console_writeln "hi""#);
        assert!(md.contains("Static"), "analysis is marked static: {md}");
        assert!(!md.contains("Result:"), "nothing ran, so no result: {md}");
        assert!(md.contains("- `gc`"), "the gc power is named: {md}");
        assert!(
            md.contains("layer 0 (boundary)"),
            "each manifest effect is tagged with the layer it enters at: {md}"
        );
        // winapi is confined to the boundary — it must not surface as an app power.
        assert!(
            !md.contains("- `winapi`"),
            "the raw winapi power is sealed, not named in the manifest: {md}"
        );
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
        // The table groups by layer; the app layer is a subheading, the helper a
        // row under it attributed to "(this program)".
        assert!(
            md.contains("### Layer 2 — app"),
            "an app-layer group is present: {md}"
        );
        assert!(
            md.lines()
                .any(|l| l.starts_with("| `helper`") && l.contains("(this program)")),
            "the nested helper is attributed to the program: {md}"
        );
        // No function row may have a blank (em-dash) module. Rows are
        // `| `name` | Module | args | eff |`, so the module is cell index 2.
        for l in md.lines().filter(|l| l.starts_with("| `")) {
            let cells: Vec<&str> = l.split('|').map(|c| c.trim()).collect();
            assert_ne!(cells.get(2), Some(&"\u{2014}"), "module is attributed: {l}");
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
        // `a` (app) -> Graphics (services) is a layer-crossing edge (`==>`) and
        // carries the graphics power that flows across it.
        assert!(
            md.lines()
                .any(|l| l.contains("fn_a") && l.contains("mod_Graphics") && l.contains("graphics")),
            "the user fn -> service edge carries the graphics effect: {md}"
        );
        assert!(
            md.contains("==>"),
            "an app -> services call is marked as crossing a layer: {md}"
        );
        // No individual fill_rect/gfx_* nodes leak into the graph.
        assert!(!md.contains("fn_fill_rect"), "stdlib leaves are collapsed: {md}");
        // The service node is tinted amber (services layer) via a docpane @node.
        assert!(
            md.contains("%% @node mod_Graphics fill=\"#5C3D10\""),
            "service node carries the amber (layer-1) fill annotation: {md}"
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
        // The table groups by layer; fill_rect lives under the services group,
        // attributed to the Graphics module, carrying the `graphics` effect.
        assert!(
            md.contains("### Layer 1 — services"),
            "a services-layer group is present: {md}"
        );
        assert!(
            md.lines().any(|l| l.starts_with("| `fill_rect`") && l.contains("Graphics")),
            "fill_rect is attributed to the Graphics module: {md}"
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

    #[test]
    fn analyze_report_data_access_tree_traces_a_db_program_to_its_provider() {
        // A program that opens an in-memory SQLite db and runs a statement: the
        // service plugins are grafted into analysis, so the `sqlite` data effect
        // resolves, appears in the manifest tagged as data, and the data-access
        // tree lists the db functions that touch it.
        let src = r#"
let c = db_open_memory () in
let _ = db_exec c "CREATE TABLE t (x INTEGER)" in
db_close c
"#;
        let md = analyze_report(src);
        assert!(md.contains("## Data access"), "a data-access section is drawn: {md}");
        assert!(
            md.lines().any(|l| l.contains("`db_exec`") && l.contains("sqlite")),
            "db_exec is shown touching the sqlite data effect: {md}"
        );
        // `sqlite` is named in the manifest, tagged as a data effect.
        assert!(
            md.lines().any(|l| l.contains("- `sqlite`") && l.contains("**data**")),
            "sqlite is named in the manifest as a data effect: {md}"
        );
    }

    #[test]
    fn analyze_report_data_access_tree_includes_filesystem_writes() {
        // DocsFs is a controlled-filesystem store; its docsfs_* effects are data
        // access and must surface in the tree (a previous version missed them
        // because the data set only listed raw `fs`, not the sealed service ops).
        let src = r#"
let _ = docs_write_text "notes.txt" "hello" in
docs_read_text "notes.txt"
"#;
        let md = analyze_report(src);
        assert!(md.contains("## Data access"), "filesystem access is a data section: {md}");
        assert!(
            md.lines()
                .any(|l| l.contains("`docs_write_text`") && l.contains("docsfs_write")),
            "docs_write_text shows its filesystem data effect: {md}"
        );
    }

    #[test]
    fn analyze_report_does_not_fabricate_a_data_row_for_a_mock_service() {
        // The stdlib `Db` mock performs NO data-store I/O — its op is sealed, so
        // the root manifest is just gc. The data-access tree must NOT invent a
        // `db` effect from the module name (the friendly-collapse hazard), and
        // must agree with the manifest that no data store is touched.
        let src = r#"if db_mock_connect "test.api.key" then 1 else 0"#;
        let md = analyze_report(src);
        assert!(
            !md.contains("## Data access"),
            "no data-access section for a non-data mock service: {md}"
        );
        assert!(
            !md.contains("`db`"),
            "no fabricated `db` data effect from the module name: {md}"
        );
    }
}
