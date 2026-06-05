//! The `locus` CLI — the agent-focused command surface (design §8), built on
//! the core checker. **Slices 6–9.**
//!
//! `check` parses + type-checks; `sema` prints the fully typed tree (every
//! node's `type ! row @ stage`); `ast` dumps the parse tree. Diagnostics are a
//! single structured [`Report`] (`diag.rs`) rendered three ways: labelled
//! **text** (default), `--brief` one-liner, or `--json` (`locus-diag/1`) — the
//! human and the machine see the same fields. `-e EXPR` checks inline;
//! otherwise a file or stdin. Exit: 0 ok, 1 parse/type error, 2 usage error.
//! No dependencies — arguments and JSON are hand-built.

use std::io::Read;
use std::process;

use locus::iface::{check_client_against, ConsumeError, Import, LoadedInterface};
use locus::{
    analyze, elaborate, iface, ir_shape, lower, parse_program, prelude, program,
    program_source_shape, term_shape, typed_shape, Ctx, Report, ShapeMetrics, Stage, TypeErr,
};

const USAGE: &str = "\
locus — the Locus core checker (Phase 1)

USAGE:
  locus check    [options] [FILE]   parse and type-check; report `type ! row @ stage`
  locus sema     [options] [FILE]   elaborate; print the fully typed tree (every node)
  locus ir       [options] [FILE]   lower to A-normal form; print the IR (effect-tagged)
  locus evidence [options] [FILE]   evidence-pass; classify each effect (zero-cost / residual)
  locus ast      [options] [FILE]   print the parsed AST
  locus help [TOPIC] [--human]      discover language syntax, operation, and services
  locus help search QUERY [--human] search syntax/services/examples/reminders
  locus help service NAME [--human] show a service/module/function help card
  locus help services [--human]     list published services
  locus emit-interface [opts] FILE  compile one module; emit its `.locusi` interface
  locus check-client  [opts] FILE   type-check a client against imported `.locusi`s
  locus --help

OPTIONS (check / ast):
  -e EXPR        check EXPR directly instead of a file
  FILE | -       read FILE, or stdin if FILE is `-` or omitted
  --stage N      check at stage N (0 = runtime [default], 1 = generation)
  --brief        one-line output (the judgment, or `error: …`)
  --json         machine-readable JSON (schema locus-diag/1)
  --trace-stack-usage
                 print compiler tree-depth / spine metrics to stderr
  (default)      structured, labelled text

FOR AGENTS:
  locus help agent                  start here (JSON by default)
  locus help search \"loop string\"
  locus help service Agent
  locus help remind loops --human

OPTIONS (emit-interface):
  FILE | -       read FILE, or stdin if FILE is `-` or omitted
  -o OUT         write the `.locusi` to OUT instead of stdout

OPTIONS (check-client):
  FILE | -       read the client FILE, or stdin if `-` or omitted
  --iface PATH   load a producer `.locusi` (repeatable); each `import X` in the
                 client resolves to the loaded interface named `X`

EXIT: 0 ok · 1 parse/type error · 2 usage error
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Run the whole command on a generously-sized stack: elaboration recurses
    // deeply over the stdlib graft and staging is a native tree-walker, so a
    // large program would overflow a default thread stack.
    let code = std::thread::Builder::new()
        .name("locus-main".into())
        .stack_size(locus::PIPELINE_STACK_BYTES)
        .spawn(move || match dispatch(&args) {
            Ok(c) => c,
            Err(msg) => {
                eprintln!("locus: {msg}\ntry `locus --help`");
                2
            }
        })
        .expect("spawn locus worker")
        .join()
        .unwrap_or_else(|_| {
            eprintln!("locus: internal error (worker panicked)");
            101
        });
    process::exit(code);
}

fn dispatch(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("check") => command(&args[1..], Mode::Check),
        Some("sema") => command(&args[1..], Mode::Sema),
        Some("ir") => command(&args[1..], Mode::Ir),
        Some("evidence") => command(&args[1..], Mode::Evidence),
        Some("ast") => command(&args[1..], Mode::Ast),
        Some("help") => cmd_help(&args[1..]),
        Some("emit-interface") => emit_interface(&args[1..]),
        Some("check-client") => check_client(&args[1..]),
        None | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            Ok(0)
        }
        Some(other) => Err(format!(
            "unknown command `{other}` (try `check`, `sema`, `ir`, `evidence`, `ast`, \
             `help`, `emit-interface`, `check-client`, `--help`)"
        )),
    }
}

fn cmd_help(args: &[String]) -> Result<i32, String> {
    let (human, words) = parse_help_args(args);
    if words.is_empty() || matches!(words[0].as_str(), "overview" | "index") {
        println!(
            "{}",
            if human {
                locus::help::overview_text()
            } else {
                locus::help::overview_json()
            }
        );
        return Ok(0);
    }

    match words[0].as_str() {
        "agent" => print_help_card("agent.start", human),
        "search" => {
            let query = words[1..].join(" ");
            if query.trim().is_empty() {
                return Err("usage: locus help search QUERY [--human]".into());
            }
            let hits = locus::help::search(&query, 8);
            println!(
                "{}",
                if human {
                    locus::help::search_text(&query, &hits)
                } else {
                    locus::help::search_json(&query, &hits)
                }
            );
            Ok(0)
        }
        "topic" => {
            let id = words
                .get(1)
                .ok_or("usage: locus help topic TOPIC [--human]")?;
            print_help_card(id, human)
        }
        "service" => {
            let name = words
                .get(1)
                .ok_or("usage: locus help service NAME [--human]")?;
            let Some(card) = locus::help::service(name) else {
                return Err(format!(
                    "unknown service `{name}` (try `locus help search {name}`)"
                ));
            };
            println!(
                "{}",
                if human {
                    locus::help::card_text(card)
                } else {
                    locus::help::card_json(card)
                }
            );
            Ok(0)
        }
        "services" => {
            println!(
                "{}",
                if human {
                    locus::help::services_text()
                } else {
                    locus::help::services_json()
                }
            );
            Ok(0)
        }
        "remind" => {
            let topic = words
                .get(1)
                .ok_or("usage: locus help remind TOPIC [--human]")?;
            println!(
                "{}",
                if human {
                    locus::help::remind_text(topic)
                } else {
                    locus::help::remind_json(topic)
                }
            );
            Ok(0)
        }
        other => {
            if let Some(card) = locus::help::find(other) {
                println!(
                    "{}",
                    if human {
                        locus::help::card_text(card)
                    } else {
                        locus::help::card_json(card)
                    }
                );
            } else {
                let query = words.join(" ");
                let hits = locus::help::search(&query, 8);
                println!(
                    "{}",
                    if human {
                        locus::help::search_text(&query, &hits)
                    } else {
                        locus::help::search_json(&query, &hits)
                    }
                );
            }
            Ok(0)
        }
    }
}

fn parse_help_args(args: &[String]) -> (bool, Vec<String>) {
    let mut human = false;
    let mut words = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--human" => human = true,
            "--json" => {}
            _ => words.push(arg.clone()),
        }
    }
    (human, words)
}

fn print_help_card(id: &str, human: bool) -> Result<i32, String> {
    let Some(card) = locus::help::find(id) else {
        return Err(format!(
            "unknown help topic `{id}` (try `locus help search {id}`)"
        ));
    };
    println!(
        "{}",
        if human {
            locus::help::card_text(card)
        } else {
            locus::help::card_json(card)
        }
    );
    Ok(0)
}

/// What to do with the parsed program.
#[derive(Clone, Copy)]
enum Mode {
    /// Type-check → `type ! row @ stage`.
    Check,
    /// Elaborate → the fully decorated typed tree.
    Sema,
    /// Elaborate, then lower → the A-normal-form IR.
    Ir,
    /// Elaborate, lower, then run the evidence pass (zero-cost classification).
    Evidence,
    /// Pretty-print the raw parse tree.
    Ast,
}

/// Output rendering, selected by flags.
#[derive(Clone, Copy)]
enum Fmt {
    Text,
    Brief,
    Json,
}

/// Render a diagnostic [`Report`] in the chosen format, print it, and return
/// the exit code it implies (0 ok, 1 error).
fn emit(r: &Report, fmt: Fmt) -> i32 {
    println!(
        "{}",
        match fmt {
            Fmt::Text => r.to_text(),
            Fmt::Brief => r.to_brief(),
            Fmt::Json => r.to_json(),
        }
    );
    if r.ok() {
        0
    } else {
        1
    }
}

fn command(args: &[String], mode: Mode) -> Result<i32, String> {
    let mut fmt = Fmt::Text;
    let mut stage: Stage = 0;
    let mut inline: Option<String> = None;
    let mut file: Option<String> = None;
    let mut trace_stack = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => fmt = Fmt::Json,
            "--brief" => fmt = Fmt::Brief,
            "--trace-stack-usage" => trace_stack = true,
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(0);
            }
            "--stage" => {
                let n = it.next().ok_or("`--stage` needs a number")?;
                stage = n.parse().map_err(|_| format!("bad stage `{n}`"))?;
            }
            "-e" => inline = Some(it.next().ok_or("`-e` needs an expression")?.clone()),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option `{other}`"));
            }
            other => file = Some(other.to_string()),
        }
    }

    let source = match inline {
        Some(s) => s,
        None => read_input(file.as_deref())?,
    };

    let source_shape = if trace_stack {
        parse_program(&source)
            .ok()
            .map(|p| program_source_shape(&p))
    } else {
        None
    };

    // A parse failure is the same diagnostic in every mode. `program` parses the
    // user source and grafts the prelude (e.g. `console_writeln`) if the program uses it.
    let term = match program(&source) {
        Ok(t) => t,
        Err(e) => return Ok(emit(&Report::parse_error(&e, &source), fmt)),
    };
    if trace_stack {
        trace_stack_header();
        if let Some(shape) = source_shape {
            trace_shape("user source", shape);
        }
        trace_shape("stdlib-grafted term", term_shape(&term));
    }

    match mode {
        Mode::Ast => Ok(emit(
            &Report::Ast {
                pretty: format!("{term:#?}"),
            },
            fmt,
        )),

        Mode::Check => {
            let r = match elaborate(&prelude::sig(), &Ctx::new(), stage, &term) {
                Ok(tree) => {
                    if trace_stack {
                        trace_shape("typed tree", typed_shape(&tree));
                    }
                    Report::Ok {
                        ty: tree.ty,
                        row: tree.row,
                        stage,
                    }
                }
                Err(e) => Report::type_error(&e),
            };
            Ok(emit(&r, fmt))
        }

        // Sema's success artifact is the typed *tree*, not a one-line
        // judgment — so it renders itself (schema `locus-sema/1` for `--json`).
        // Errors stay on the shared diagnostic path.
        Mode::Sema => match elaborate(&prelude::sig(), &Ctx::new(), stage, &term) {
            Ok(tree) => {
                if trace_stack {
                    trace_shape("typed tree", typed_shape(&tree));
                }
                println!(
                    "{}",
                    match fmt {
                        Fmt::Text => tree.to_text(),
                        Fmt::Brief => tree.judgment(),
                        Fmt::Json => tree.to_json(),
                    }
                );
                Ok(0)
            }
            Err(e) => Ok(emit(&Report::type_error(&e), fmt)),
        },

        // IR lowers the typed tree to ANF and renders it (`locus-ir/1` JSON);
        // `--brief` keeps the program's overall judgment for context.
        Mode::Ir => match elaborate(&prelude::sig(), &Ctx::new(), stage, &term) {
            Ok(tree) => {
                if trace_stack {
                    trace_shape("typed tree", typed_shape(&tree));
                }
                if tree.has_unknown_layout() {
                    return Ok(emit(
                        &Report::type_error(&TypeErr::RepresentationPolymorphicLayout),
                        fmt,
                    ));
                }
                let ir = lower(&tree);
                if trace_stack {
                    trace_shape("anf ir", ir_shape(&ir));
                }
                println!(
                    "{}",
                    match fmt {
                        Fmt::Text => ir.to_text(),
                        Fmt::Brief => tree.judgment(),
                        Fmt::Json => ir.to_json(),
                    }
                );
                Ok(0)
            }
            Err(e) => Ok(emit(&Report::type_error(&e), fmt)),
        },

        // Evidence runs the zero-cost pass over the IR (schema `locus-evidence/1`).
        Mode::Evidence => match elaborate(&prelude::sig(), &Ctx::new(), stage, &term) {
            Ok(tree) => {
                if trace_stack {
                    trace_shape("typed tree", typed_shape(&tree));
                }
                if tree.has_unknown_layout() {
                    return Ok(emit(
                        &Report::type_error(&TypeErr::RepresentationPolymorphicLayout),
                        fmt,
                    ));
                }
                let ir = lower(&tree);
                if trace_stack {
                    trace_shape("anf ir", ir_shape(&ir));
                }
                let report = analyze(&ir);
                println!(
                    "{}",
                    match fmt {
                        Fmt::Text => report.to_text(),
                        Fmt::Brief => report.brief(),
                        Fmt::Json => report.to_json(),
                    }
                );
                Ok(0)
            }
            Err(e) => Ok(emit(&Report::type_error(&e), fmt)),
        },
    }
}

/// `locus emit-interface FILE [-o OUT]` — compile **one** module and emit its
/// textual `.locusi` interface (separate-compilation v1, Sprint 1). Single-module
/// input: the file's one `module … = …` declaration is the unit; its exported
/// contract (value signatures with rows, type defs + layouts, layer/mints/seals,
/// the interface hash + ABI version) is serialized. Cross-module type-check
/// (Sprint 2) and link (Sprint 3) are not done here.
fn trace_stack_header() {
    let bytes = locus::PIPELINE_STACK_BYTES;
    eprintln!(
        "locus stack trace: configured pipeline stack = {} bytes ({} MiB)",
        bytes,
        bytes / (1024 * 1024)
    );
}

fn trace_shape(label: &str, shape: ShapeMetrics) {
    eprintln!(
        "  {label:<21} nodes={:<6} max_depth={:<5} binding_spine={:<5} app_spine={:<5} type_depth={}",
        shape.nodes,
        shape.max_depth,
        shape.max_binding_spine,
        shape.max_app_spine,
        shape.max_type_depth
    );
}

fn emit_interface(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(0);
            }
            "-o" => out = Some(it.next().ok_or("`-o` needs an output path")?.clone()),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option `{other}`"));
            }
            other => file = Some(other.to_string()),
        }
    }

    let source = read_input(file.as_deref())?;

    // Parse the program to recover the user module declaration(s)…
    let prog = match parse_program(&source) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", Report::parse_error(&e, &source).to_text());
            return Ok(1);
        }
    };
    let module = match prog.modules.as_slice() {
        [m] => m.clone(),
        [] => {
            return Err(
                "emit-interface: input declares no `module …` (single-module input expected)"
                    .into(),
            )
        }
        _ => {
            return Err(format!(
                "emit-interface: input declares {} modules; single-module input expected",
                prog.modules.len()
            ))
        }
    };

    // …and the grafted program (the module + the stdlib it uses) so the module
    // body type-checks. The interface records only the module's own exports.
    let grafted = match program(&source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{}", Report::parse_error(&e, &source).to_text());
            return Ok(1);
        }
    };

    match iface::interface_of(&module, &grafted) {
        Ok(i) => {
            let text = iface::serialize(&i);
            match out {
                Some(path) => {
                    std::fs::write(&path, &text).map_err(|e| format!("writing `{path}`: {e}"))?;
                    eprintln!("wrote {path}");
                }
                None => print!("{text}"),
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("locus emit-interface: {e}");
            Ok(1)
        }
    }
}

/// `locus check-client FILE --iface Lib.locusi …` — type-check a **client**
/// against imported `.locusi`s only, never the producer source (separate-compilation
/// v1, Sprint 2). Each `--iface PATH` loads a producer interface across the
/// client-load boundary (ABI-version + hash checks, `RN-E0603`/`RN-E0600`); each
/// `import X` in the client resolves to the loaded interface named `X` and brings
/// in its exports. The client's row carries the producers' published effect rows
/// (§4a transparency). On the consume path the core API ([`check_client_against`])
/// does the work; this verb is the file-IO shell over it. No codegen/link (Sprint 3).
fn check_client(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut iface_paths: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(0);
            }
            "--iface" => iface_paths.push(it.next().ok_or("`--iface` needs a path")?.clone()),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option `{other}`"));
            }
            other => file = Some(other.to_string()),
        }
    }

    let source = read_input(file.as_deref())?;

    // Load each producer interface across the client-load boundary (ABI/hash
    // validated). A load failure (`RN-E0603`/`RN-E0600`/malformed) is the same
    // structured consume diagnostic the body path uses.
    let mut ifaces = Vec::new();
    for path in &iface_paths {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading `{path}`: {e}"))?;
        match LoadedInterface::load(&text) {
            Ok(l) => ifaces.push(l),
            Err(e) => return Ok(emit_consume_error(&e)),
        }
    }

    // The client's `import X` lines resolve to the loaded interface named `X`; the
    // surface carries no name list, so the driver requests *every* export of each.
    let prog = match parse_program(&source) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", Report::parse_error(&e, &source).to_text());
            return Ok(1);
        }
    };
    let imports: Vec<Import> = prog.imports.iter().cloned().map(Import::all).collect();

    match check_client_against(&source, &imports, &ifaces) {
        Ok(typed) => {
            println!(
                "ok\n  type  {}\n  row   {}\n  stage {}",
                typed.ty, typed.row, 0
            );
            Ok(0)
        }
        Err(e) => Ok(emit_consume_error(&e)),
    }
}

/// Render a [`ConsumeError`] like a type diagnostic (the same labelled fields the
/// [`Report`] error path uses) and return exit code 1.
fn emit_consume_error(e: &ConsumeError) -> i32 {
    let mut s = format!("error  {} {}\n  {}", e.code(), e.slug(), e);
    s += &format!("\n  spec  {}", e.spec());
    if let Some(h) = e.hint() {
        s += &format!("\n  hint  {h}");
    }
    eprintln!("{s}");
    1
}

fn read_input(file: Option<&str>) -> Result<String, String> {
    match file {
        None | Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("reading stdin: {e}"))?;
            Ok(s)
        }
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("reading `{path}`: {e}")),
    }
}
