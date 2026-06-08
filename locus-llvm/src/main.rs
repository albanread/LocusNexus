//! `locusc` — the Locus **compiler driver**.
//!
//! The std-only `locus` CLI does the front-end views (check / sema / ir /
//! evidence); this is the LLVM-backed half:
//!
//! - `locusc run FILE`            — JIT and execute (side effects happen; the
//!                                 program's `i64` becomes the exit code).
//! - `locusc build FILE [-o EXE]` — AOT-compile to a standalone `.exe`.
//! - `locusc asm FILE [-o OUT.s]` — dump the generated **x86-64 assembly** (the
//!                                 systems-level view of the high-level program).
//!
//! All run the same front end (`locus::parse` → `elaborate` → `lower`) to the
//! ANF IR, then hand it to the backend.

use std::collections::HashSet;
#[cfg(feature = "mcp")]
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process;
#[cfg(feature = "mcp")]
use std::process::{Command, Stdio};
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};

const USAGE: &str = "\
locusc — the Locus compiler (JIT + AOT)

USAGE:
  locusc run     FILE            JIT-compile and run FILE
  locusc build   FILE [-o EXE]   compile FILE to a standalone .exe
                   [--always-gc] link the collector even if FILE doesn't allocate
  locusc asm     FILE [-o OUT.s] dump the generated x86-64 assembly
  locusc effects FILE [--json]   print FILE's effect manifest (what it touches);
                                 --json emits a stable, diffable manifest for CI
  locusc help [TOPIC] [--human]   discover language syntax, operation, and services
  locusc help search QUERY [--human]
                                search syntax/services/examples/reminders
  locusc help service NAME [--human]
                                show a service/module/function help card
  locusc help services [--human] list published services
  locusc republish [DIR]         write the embedded stdlib to DIR for review
  locusc mcp                     serve the agent-facing MCP protocol over stdio
  locusc mcp-call COMMAND ...    call the MCP server once and print JSON
  locusc --help

OPTIONS:
  --trace-stack-usage            print compiler tree-depth / spine metrics to stderr
  --brk-enable                   allow the `brk` debug-crash expression (off by
                                 default; a `brk` traps to exercise a crash handler)

`run`'s exit code is the program's i64 result; effects print as they execute.
`asm` writes to stdout unless `-o` is given — the same code the .exe contains.

FOR AGENTS:
  locusc help agent               start here (JSON by default)
  locusc help search \"loop string\"
  locusc help service Agent
  locusc help remind loops --human
  locusc mcp-call run --source \"let x = 40 in x + 2\"

Minting (`extern`, raw memory) is a `boundary`-only capability: app code may not
name it — every command rejects an app-level mint (`RN-E0402`). The platform team
mints in `boundary` modules (baked in) and authorizes user ones via `locus.toml
[boundary]`; the app team's compiler is locked.
";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `--brk-enable` is a global gate (it changes what the parser accepts), so
    // handle it here for every subcommand and strip it before dispatch — the
    // per-command flag parsers never see it. Off by default: without it, a
    // program's `brk` is a parse error, so a deliberate crash can't ship.
    if let Some(i) = args.iter().position(|a| a == "--brk-enable") {
        args.remove(i);
        locus::set_brk_enabled(true);
    }
    let code = std::thread::Builder::new()
        .name("locusc-main".into())
        .stack_size(locus::PIPELINE_STACK_BYTES)
        .spawn(move || match dispatch(&args) {
            Ok(c) => c,
            Err(msg) => {
                eprintln!("locusc: {msg}");
                2
            }
        })
        .expect("spawn locusc worker")
        .join()
        .unwrap_or_else(|_| {
            eprintln!("locusc: internal error (worker panicked)");
            101
        });
    process::exit(code);
}

fn dispatch(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("asm") => cmd_asm(&args[1..]),
        Some("effects") => cmd_effects(&args[1..]),
        Some("help") => cmd_help(&args[1..]),
        Some("republish") => cmd_republish(&args[1..]),
        Some("mcp") => cmd_mcp(&args[1..]),
        Some("mcp-call") => cmd_mcp_call(&args[1..]),
        Some("worker") => cmd_worker(&args[1..]),
        None | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            Ok(0)
        }
        Some(other) => Err(format!(
            "unknown command `{other}` (try `run`, `build`, `asm`, `effects`, `help`, `republish`, `mcp`, `mcp-call`, `--help`)"
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
                return Err("usage: locusc help search QUERY [--human]".into());
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
                .ok_or("usage: locusc help topic TOPIC [--human]")?;
            print_help_card(id, human)
        }
        "service" => {
            let name = words
                .get(1)
                .ok_or("usage: locusc help service NAME [--human]")?;
            let Some(card) = locus::help::service(name) else {
                return Err(format!(
                    "unknown service `{name}` (try `locusc help search {name}`)"
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
                .ok_or("usage: locusc help remind TOPIC [--human]")?;
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
            "unknown help topic `{id}` (try `locusc help search {id}`)"
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

#[cfg(feature = "mcp")]
fn cmd_worker(args: &[String]) -> Result<i32, String> {
    if !args.is_empty() {
        return Err("usage: locusc worker".into());
    }
    locus_llvm::mcp::worker_blocking_stdio()
}

#[cfg(not(feature = "mcp"))]
fn cmd_worker(_args: &[String]) -> Result<i32, String> {
    Err("`locusc worker` is available when locus-llvm is built with `--features mcp`".into())
}

#[cfg(feature = "mcp")]
fn cmd_mcp(args: &[String]) -> Result<i32, String> {
    if !args.is_empty() {
        return Err("usage: locusc mcp".into());
    }
    locus_llvm::mcp::serve_blocking_stdio()
}

#[cfg(not(feature = "mcp"))]
fn cmd_mcp(_args: &[String]) -> Result<i32, String> {
    Err("`locusc mcp` is available when locus-llvm is built with `--features mcp`".into())
}

const MCP_CALL_USAGE: &str = "\
usage:
  locusc mcp-call tools
  locusc mcp-call call TOOL [JSON_ARGS]
  locusc mcp-call help_overview
  locusc mcp-call help_search QUERY [LIMIT]
  locusc mcp-call help_topic ID
  locusc mcp-call help_service NAME
  locusc mcp-call help_remind TOPIC
  locusc mcp-call check (--file PATH | --source SOURCE)
  locusc mcp-call run (--file PATH | --source SOURCE)
  locusc mcp-call ir (--file PATH | --source SOURCE)
  locusc mcp-call effects (--file PATH | --source SOURCE)";

#[cfg(feature = "mcp")]
fn cmd_mcp_call(args: &[String]) -> Result<i32, String> {
    use serde_json::{json, Value};

    let (tool, arguments, list_tools) = mcp_call_target(args)?;
    let mut child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg("mcp")
        .current_dir(std::env::current_dir().map_err(|e| e.to_string())?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("starting MCP server: {e}"))?;

    let mut stdin = child.stdin.take().ok_or("MCP server stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("MCP server stdout unavailable")?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut next_id = 1_i64;

    let init = json!({
        "jsonrpc": "2.0",
        "id": next_id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "locusc-mcp-call", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    mcp_send(&mut stdin, &init)?;
    let _ = mcp_recv_id(&mut reader, next_id)?;
    next_id += 1;
    mcp_send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    )?;

    let request = if list_tools {
        json!({"jsonrpc": "2.0", "id": next_id, "method": "tools/list", "params": {}})
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": next_id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        })
    };
    mcp_send(&mut stdin, &request)?;
    let result = mcp_recv_id(&mut reader, next_id);
    drop(stdin);
    drop(reader);
    let _ = wait_or_terminate_child(&mut child, Duration::from_secs(2));
    let result = result?;

    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
    );
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(if is_error { 1 } else { 0 })
}

#[cfg(feature = "mcp")]
fn wait_or_terminate_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return child.wait().map_err(|e| e.to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(feature = "mcp"))]
fn cmd_mcp_call(_args: &[String]) -> Result<i32, String> {
    Err("`locusc mcp-call` is available when locus-llvm is built with `--features mcp`".into())
}

#[cfg(feature = "mcp")]
fn mcp_call_target(args: &[String]) -> Result<(String, serde_json::Value, bool), String> {
    use serde_json::json;

    let Some(cmd) = args.first().map(String::as_str) else {
        return Err(MCP_CALL_USAGE.into());
    };
    let rest = &args[1..];
    match cmd {
        "tools" => Ok((String::new(), json!({}), true)),
        "call" => {
            let name = rest
                .first()
                .ok_or_else(|| format!("call requires TOOL [JSON_ARGS]\n{MCP_CALL_USAGE}"))?;
            let raw = rest.get(1).map(String::as_str).unwrap_or("{}");
            let arguments = serde_json::from_str(raw)
                .map_err(|e| format!("invalid JSON_ARGS for `{name}`: {e}"))?;
            Ok((mcp_tool_alias(name), arguments, false))
        }
        "help_overview" => Ok(("help_overview".into(), json!({}), false)),
        "help_search" => {
            let query = rest
                .first()
                .ok_or_else(|| format!("help_search requires QUERY [LIMIT]\n{MCP_CALL_USAGE}"))?;
            let limit = rest.get(1).map(|n| {
                n.parse::<usize>()
                    .map_err(|e| format!("invalid help_search LIMIT `{n}`: {e}"))
            });
            let mut arguments = json!({ "query": query });
            if let Some(limit) = limit {
                arguments["limit"] = json!(limit?);
            }
            Ok(("help_search".into(), arguments, false))
        }
        "help_topic" => {
            let id = only_arg(cmd, rest)?;
            Ok(("help_topic".into(), json!({ "id": id }), false))
        }
        "help_service" => {
            let name = only_arg(cmd, rest)?;
            Ok(("help_service".into(), json!({ "name": name }), false))
        }
        "help_remind" => {
            let topic = only_arg(cmd, rest)?;
            Ok(("help_remind".into(), json!({ "topic": topic }), false))
        }
        "check" | "run" | "effects" | "ir" | "asm" => Ok((
            mcp_tool_alias(cmd),
            mcp_source_args(rest).map_err(|e| format!("{e}\n{MCP_CALL_USAGE}"))?,
            false,
        )),
        other => Err(format!(
            "unknown mcp-call command `{other}`\n{MCP_CALL_USAGE}"
        )),
    }
}

#[cfg(feature = "mcp")]
fn only_arg<'a>(cmd: &str, args: &'a [String]) -> Result<&'a str, String> {
    if args.len() == 1 {
        Ok(&args[0])
    } else {
        Err(format!(
            "{cmd} requires exactly one argument\n{MCP_CALL_USAGE}"
        ))
    }
}

#[cfg(feature = "mcp")]
fn mcp_source_args(args: &[String]) -> Result<serde_json::Value, String> {
    use serde_json::json;

    if args.len() != 2 {
        return Err("expected --file PATH or --source SOURCE".into());
    }
    match args[0].as_str() {
        "--file" => Ok(json!({ "file": args[1] })),
        "--source" => Ok(json!({ "source": args[1] })),
        other => Err(format!("expected --file or --source, got `{other}`")),
    }
}

#[cfg(feature = "mcp")]
fn mcp_tool_alias(name: &str) -> String {
    match name {
        "ir" => "emit_ir",
        "asm" => "emit_asm",
        other => other,
    }
    .to_string()
}

#[cfg(feature = "mcp")]
fn mcp_send(stdin: &mut process::ChildStdin, value: &serde_json::Value) -> Result<(), String> {
    let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    writeln!(stdin, "{line}").map_err(|e| format!("writing MCP request: {e}"))?;
    stdin
        .flush()
        .map_err(|e| format!("flushing MCP request: {e}"))
}

#[cfg(feature = "mcp")]
fn mcp_recv_id(reader: &mut impl BufRead, id: i64) -> Result<serde_json::Value, String> {
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("reading MCP response: {e}"))?;
        if n == 0 {
            return Err("MCP server closed stdout".into());
        }
        let value: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| format!("invalid MCP JSON: {e}: {line}"))?;
        if value.get("id").and_then(serde_json::Value::as_i64) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!(
                "MCP error: {}",
                serde_json::to_string_pretty(error).unwrap_or_else(|_| error.to_string())
            ));
        }
        return value
            .get("result")
            .cloned()
            .ok_or_else(|| format!("MCP response missing result: {value}"));
    }
}

/// Discover and read the project's **boundary manifest** — the `locus.toml`
/// `[boundary] modules = […]` trust root (S2b). Walks up from the source file's
/// directory to the filesystem root; returns the authorized boundary-module names
/// (empty if no `locus.toml` / no `[boundary]` section is found, i.e. the secure
/// default — no *user* module may mint).
fn read_boundary_manifest(source_file: &Path) -> HashSet<String> {
    let mut dir = source_file.parent();
    while let Some(d) = dir {
        let candidate = d.join("locus.toml");
        if candidate.is_file() {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                return parse_boundary_modules(&text);
            }
            break;
        }
        dir = d.parent();
    }
    HashSet::new()
}

/// Extract the names from `[boundary] modules = ["A", "B"]` — a minimal hand-parse
/// (the project keeps a TOML dependency out for this one tiny key). Reads the
/// `[boundary]` table up to the next `[table]`, finds the `modules` array, and
/// collects its quoted strings (the array may span lines).
fn parse_boundary_modules(toml: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(start) = toml.find("[boundary]") else {
        return out;
    };
    let after = &toml[start + "[boundary]".len()..];
    let section = match after.find("\n[") {
        Some(i) => &after[..i],
        None => after,
    };
    let Some(m) = section.find("modules") else {
        return out;
    };
    let rest = &section[m..];
    let Some(lb) = rest.find('[') else {
        return out;
    };
    let Some(rb_off) = rest[lb..].find(']') else {
        return out;
    };
    let array = &rest[lb + 1..lb + rb_off];
    let mut cur = String::new();
    let mut in_str = false;
    for ch in array.chars() {
        match ch {
            '"' if in_str => {
                out.insert(std::mem::take(&mut cur));
                in_str = false;
            }
            '"' => in_str = true,
            _ if in_str => cur.push(ch),
            _ => {}
        }
    }
    out
}

/// The mint-gate (S2): `extern` / `extern asm` / foreign-bind may appear only in
/// an authorized `at boundary` module. App code (the entry) and user modules are
/// checked; the bundled stdlib boundary modules are trusted by construction.
/// `authorized` is the project's `locus.toml [boundary]` set (S2b) — empty by
/// default, so an un-manifested `at boundary` claim is rejected (`RN-E0404`).
fn guard_layer2(src: &str, authorized: &HashSet<String>) -> Result<(), String> {
    // The **mint-gate**, enforced on *every* CLI entry point (`run`/`build`/`asm`/
    // `effects`). Minting (`extern`, the raw memory primitives, and the coming
    // `extern asm` / foreign-bind) is `boundary`-only and manifest-authorized; a
    // violation **blocks**. The platform team mints in boundary modules (baked into
    // this binary) and authorizes any user boundary module via `locus.toml
    // [boundary]`; app code that reaches the raw boundary is rejected outright
    // (`RN-E0402` / `RN-E0404`). This is the "hand the app team a locked compiler"
    // guarantee — see `docs/user-guide-mint-and-seal.md`. (Legitimate FFI is done
    // in a boundary module, or via the library API the test harness uses.)
    let prog = locus::parse_program(src).map_err(|e| e.msg)?;
    locus::mint_gate(&prog.entry, &prog.modules, authorized)
        .map_err(|e| format!("[{}] {e}", e.code()))
}

/// Front end: source → ANF IR + the demanded Win32 DLLs. `program` grafts the
/// prelude (e.g. `console_writeln`); the resolver fills bare `extern "Sym"` from the
/// Win32 oracle and collects the DLLs the AOT linker will need.
///
/// The compile-time **service plugins** — graft module set + C-ABI symbols —
/// now live in the library ([`locus_llvm::plugins`]) so the IDE's analysis path
/// grafts the same set; the driver re-uses them here.
use locus_llvm::plugins::{plugin_grafted_modules, plugin_symbols};

fn to_ir(
    src: &str,
    trace_stack: bool,
) -> Result<(locus::Ir, locus_llvm::winapi_resolve::Demanded), String> {
    if trace_stack {
        trace_stack_header("locusc");
        if let Ok(program) = locus::parse_program(src) {
            trace_shape("user source", locus::program_source_shape(&program));
        }
    }
    let modules = plugin_grafted_modules();
    let (term, user_modules) =
        locus::program_with_stdlib(src, &modules).map_err(|e| e.msg)?;
    if trace_stack {
        trace_shape("stdlib-grafted term", locus::term_shape(&term));
    }
    let (term, demanded) = locus_llvm::winapi_resolve::resolve(term)?;
    if trace_stack {
        trace_shape("winapi-resolved term", locus::term_shape(&term));
    }
    let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
        .map_err(|e| e.to_string())?;
    if trace_stack {
        trace_shape("typed tree", locus::typed_shape(&tree));
    }
    // Enforce each module's `seals (…)` clause over the elaborated exports (S4):
    // no exposed binding may carry a sealed label. Covers the included stdlib
    // services (e.g. Console seals winapi) and the user modules.
    let mut all_modules = locus::stdlib_module_decls_from(&modules);
    all_modules.extend(user_modules);
    locus::check_module_seals(&all_modules, &tree).map_err(|e| e.to_string())?;
    // Run the generators (staging) at compile time, leaving residual object code.
    let tree = locus::stage_reduce(&tree)?;
    if trace_stack {
        trace_shape("stage-reduced tree", locus::typed_shape(&tree));
    }
    if tree.has_unknown_layout() {
        return Err(locus::TypeErr::RepresentationPolymorphicLayout.to_string());
    }
    let ir = locus::lower(&tree);
    if trace_stack {
        trace_shape("anf ir", locus::ir_shape(&ir));
    }
    Ok((ir, demanded))
}

fn trace_stack_header(tool: &str) {
    let bytes = locus::PIPELINE_STACK_BYTES;
    eprintln!(
        "{tool} stack trace: configured pipeline stack = {} bytes ({} MiB)",
        bytes,
        bytes / (1024 * 1024)
    );
}

fn trace_shape(label: &str, shape: locus::ShapeMetrics) {
    eprintln!(
        "  {label:<21} nodes={:<6} max_depth={:<5} binding_spine={:<5} app_spine={:<5} type_depth={}",
        shape.nodes,
        shape.max_depth,
        shape.max_binding_spine,
        shape.max_app_spine,
        shape.max_type_depth
    );
}

fn read(file: &str) -> Result<String, String> {
    std::fs::read_to_string(file).map_err(|e| format!("reading `{file}`: {e}"))
}

fn cmd_run(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut trace_stack = false;
    for a in args {
        match a.as_str() {
            "--trace-stack-usage" => trace_stack = true,
            other => file = Some(other.to_string()),
        }
    }
    let file = file.ok_or("usage: locusc run [--trace-stack-usage] FILE")?;
    let src = read(&file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(&file)))?;
    let (ir, apis) = to_ir(&src, trace_stack)?;
    // The program runs here — its effects execute and its i64 is the exit code.
    // Inject the plugins' C-ABI symbols so a grafted boundary's `extern`s resolve.
    let result = locus_llvm::jit_run_i64_with_symbols(&ir, &apis, &plugin_symbols())?;
    Ok(result as i32)
}

fn cmd_build(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut always_gc = false;
    let mut trace_stack = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("`-o` needs a path")?.clone()),
            "--always-gc" => always_gc = true,
            "--trace-stack-usage" => trace_stack = true,
            other => file = Some(other.to_string()),
        }
    }
    let file =
        file.ok_or("usage: locusc build [--trace-stack-usage] FILE [-o EXE] [--always-gc]")?;
    let src = read(&file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(&file)))?;
    let (ir, apis) = to_ir(&src, trace_stack)?;
    let exe = out
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&file).with_extension("exe"));
    // Each demanded DLL → its import lib, for the linker (kernel32 is free).
    let import_libs = locus_llvm::winapi_resolve::import_libs(&apis);
    locus_llvm::build_exe(&ir, &exe, &import_libs, always_gc)?;
    println!("built {}", exe.display());
    Ok(0)
}

/// `asm FILE [-o OUT.s]` — dump the host x86-64 assembly the backend generates
/// for the program (the same code the `.exe` carries). To stdout by default, or
/// to a file with `-o`. No JIT, no link — just front end → IR → `TargetMachine`.
fn cmd_asm(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut optimize = false;
    let mut trace_stack = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("`-o` needs a path")?.clone()),
            // The optimized view (what the `.exe` runs) vs the default lowering view.
            "-O2" | "--opt" => optimize = true,
            "--trace-stack-usage" => trace_stack = true,
            other => file = Some(other.to_string()),
        }
    }
    let file = file.ok_or("usage: locusc asm [--trace-stack-usage] FILE [-O2] [-o OUT.s]")?;
    let src = read(&file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(&file)))?;
    let (ir, _apis) = to_ir(&src, trace_stack)?;
    let asm = if optimize {
        locus_llvm::emit_asm_opt(&ir)?
    } else {
        locus_llvm::emit_asm(&ir)?
    };
    match out {
        Some(path) => {
            std::fs::write(&path, &asm).map_err(|e| format!("writing `{path}`: {e}"))?;
            println!("wrote {path}");
        }
        None => print!("{asm}"),
    }
    Ok(0)
}

/// `effects FILE [--json]` — print FILE's effect MANIFEST: its type and the
/// effects it performs, grouped by category and glossed. This is the "review
/// everything" surface — the transparency that makes layer-2 work by interns /
/// sub-agents auditable. A change that grows the row (a stray `{winapi}`, a new
/// `{net}`) shows up here; `--json` emits a stable, sorted manifest you can commit
/// as a golden file and **diff** in CI, so the review signal is the *delta*.
/// Permissive (no capability guard): inspection must work on any code, layer 0
/// included.
fn cmd_effects(args: &[String]) -> Result<i32, String> {
    let mut file: Option<&str> = None;
    let mut json = false;
    let mut trace_stack = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            "--trace-stack-usage" => trace_stack = true,
            other => file = Some(other),
        }
    }
    let file = file.ok_or("usage: locusc effects [--trace-stack-usage] FILE [--json]")?;
    let src = read(file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(file)))?;
    if trace_stack {
        trace_stack_header("locusc");
        if let Ok(program) = locus::parse_program(&src) {
            trace_shape("user source", locus::program_source_shape(&program));
        }
    }
    // Graft the stdlib **and the service plugins** (same module set as `run`),
    // so `effects` can resolve plugin surfaces like `sql_open` and report their
    // sealed effect (e.g. `{ sqlite }`) in the manifest.
    let modules = plugin_grafted_modules();
    let (term, _user_modules) = locus::program_with_stdlib(&src, &modules).map_err(|e| e.msg)?;
    if trace_stack {
        trace_shape("stdlib-grafted term", locus::term_shape(&term));
    }
    let (term, _apis) = locus_llvm::winapi_resolve::resolve(term)?;
    if trace_stack {
        trace_shape("winapi-resolved term", locus::term_shape(&term));
    }
    let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
        .map_err(|e| e.to_string())?;
    if trace_stack {
        trace_shape("typed tree", locus::typed_shape(&tree));
    }
    // `labels()` walks a BTreeSet, so the order is sorted and stable — diffable.
    let labels: Vec<&locus::Label> = tree.row.labels().collect();
    let ty = tree.ty.to_string();
    // The module declarations the layer attribution reads from (same grafted set
    // we just elaborated), so each effect can show the layer it enters at.
    let decls = locus::stdlib_module_decls_from(&modules);
    if json {
        print_effects_json(file, &ty, &labels, &decls);
    } else {
        print_effects_human(file, &ty, &labels, &decls);
    }
    Ok(0)
}

/// Group the labels into the non-empty categories, in `CATEGORY_ORDER`.
fn grouped<'a>(labels: &[&'a locus::Label]) -> Vec<(&'static str, Vec<&'a locus::Label>)> {
    let mut out = Vec::new();
    for cat in locus::analysis::category_order() {
        let cat = cat.as_str();
        let in_cat: Vec<&locus::Label> = labels
            .iter()
            .copied()
            .filter(|l| locus::analysis::category(l) == cat)
            .collect();
        if !in_cat.is_empty() {
            out.push((cat, in_cat));
        }
    }
    out
}

/// The layer tag shown beside an effect: `L0` boundary / `L1` services / `L2`
/// app, or `L·` for a cross-cutting effect that is not layer-confined.
fn layer_tag(l: &locus::Label, decls: &[locus::ModuleDecl]) -> &'static str {
    match locus::analysis::effect_layer_in(l, decls) {
        Some(0) => "L0",
        Some(1) => "L1",
        Some(2) => "L2",
        _ => "L\u{b7}",
    }
}

/// The human manifest: type, a one-line summary (explicit set when small, a
/// category roll-up when wide), then the legend grouped by category. Each legend
/// line is prefixed with the layer the effect enters at (`L0`/`L1`/`L2`/`L·`).
fn print_effects_human(file: &str, ty: &str, labels: &[&locus::Label], decls: &[locus::ModuleDecl]) {
    println!("{file}");
    println!("  type    : {ty}");
    if labels.is_empty() {
        println!("  effects : {{}}  — pure (touches nothing outside itself)");
        return;
    }
    let cats = grouped(labels);
    if labels.len() <= 8 {
        let names: Vec<String> = labels.iter().map(|l| format!("{l}")).collect();
        println!("  effects : {{ {} }}", names.join(", "));
    } else {
        let roll: Vec<String> = cats
            .iter()
            .map(|(c, ls)| format!("{c} {}", ls.len()))
            .collect();
        println!(
            "  effects : {} in {} categories  ({})",
            labels.len(),
            cats.len(),
            roll.join(", ")
        );
    }
    for (cat, ls) in &cats {
        println!();
        println!("  {cat} ({})", ls.len());
        for l in ls {
            println!(
                "    {}  {:<10} {}",
                layer_tag(l, decls),
                format!("{l}"),
                locus::analysis::describe(l)
            );
        }
    }
}

/// The machine manifest: stable, sorted JSON for `git diff` / CI policy. Labels
/// are the diff signal (glosses are derived, so they're left out to keep diffs
/// quiet). No serde dependency — the shape is small and fixed.
fn print_effects_json(file: &str, ty: &str, labels: &[&locus::Label], decls: &[locus::ModuleDecl]) {
    let cats = grouped(labels);
    println!("{{");
    println!("  \"file\": \"{}\",", json_escape(file));
    println!("  \"type\": \"{}\",", json_escape(ty));
    println!("  \"pure\": {},", labels.is_empty());
    println!("  \"effect_count\": {},", labels.len());
    let counts: Vec<String> = cats
        .iter()
        .map(|(c, ls)| format!("\"{c}\": {}", ls.len()))
        .collect();
    println!("  \"categories\": {{ {} }},", counts.join(", "));
    println!("  \"effects\": [");
    for (i, l) in labels.iter().enumerate() {
        let comma = if i + 1 < labels.len() { "," } else { "" };
        // `layer` is the layer the effect enters at (0/1/2), or `null` if it is
        // cross-cutting (not layer-confined).
        let layer = match locus::analysis::effect_layer_in(l, decls) {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        println!(
            "    {{ \"label\": \"{}\", \"category\": \"{}\", \"layer\": {layer} }}{comma}",
            json_escape(&format!("{l}")),
            locus::analysis::category(l)
        );
    }
    println!("  ]");
    println!("}}");
}

/// Minimal JSON string escaping (no dependency for a fixed, small shape).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `republish [DIR]` — write the compiler's embedded platform to DIR (default
/// `./stdlib-republished`) for review: the stdlib modules (layers 0/1) **and** the
/// effect catalog (the review taxonomy). The binary is the authoritative copy;
/// this emits exactly those bytes (write-out only — the compiler never reads them
/// *back* from disk, so editing the output is inert), plus a MANIFEST with each
/// file's size and FNV-1a so anyone can verify the platform that RUNS equals the
/// one they READ.
fn cmd_republish(args: &[String]) -> Result<i32, String> {
    let dir: PathBuf = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| "stdlib-republished".into());
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating `{}`: {e}", dir.display()))?;
    let mut manifest = String::from(
        "# Locus platform - republished from the compiler's embedded (authoritative) copy.\n\
         # Compare the FNV-1a below against a copy to verify it matches this compiler.\n\
         #\n# layer  module       bytes    fnv1a-64            file\n",
    );
    for (layer, name, src) in locus::stdlib_modules() {
        let fname = format!("{layer}_{name}.locus");
        let path = dir.join(&fname);
        std::fs::write(&path, src).map_err(|e| format!("writing `{}`: {e}", path.display()))?;
        manifest.push_str(&format!(
            "  {layer:<5}  {name:<11}  {:<7}  {:#018x}  {fname}\n",
            src.len(),
            fnv1a(src)
        ));
        println!("republished {}", path.display());
    }
    // The effect catalog — review config, not a code layer, so it gets the `cfg`
    // pseudo-layer in the manifest.
    let cat_path = dir.join("effects.catalog");
    std::fs::write(&cat_path, locus::analysis::EFFECT_CATALOG_SRC)
        .map_err(|e| format!("writing `{}`: {e}", cat_path.display()))?;
    manifest.push_str(&format!(
        "  {:<5}  {:<11}  {:<7}  {:#018x}  effects.catalog\n",
        "cfg",
        "effects",
        locus::analysis::EFFECT_CATALOG_SRC.len(),
        fnv1a(locus::analysis::EFFECT_CATALOG_SRC)
    ));
    println!("republished {}", cat_path.display());
    let mpath = dir.join("MANIFEST.txt");
    std::fs::write(&mpath, &manifest).map_err(|e| format!("writing `{}`: {e}", mpath.display()))?;
    println!("wrote {}", mpath.display());
    Ok(0)
}

/// FNV-1a (64-bit) — a small, dependency-free content hash, so a republished
/// module can be checked byte-for-byte against the compiler's embedded copy.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use locus::Label;

    /// The embedded catalog drives the mapping — the JSON `category` is a stable
    /// contract a CI policy keys on, so the shipped catalog must classify every
    /// label kind into a category it also lists in `order`.
    #[test]
    fn catalog_classifies_every_label_kind() {
        // winapi is the raw FFI edge — its own `boundary` bucket, NOT lumped in
        // with the abstract IO capabilities (`console` / `fs` / `net`).
        assert_eq!(locus::analysis::category(&Label::World("winapi".into())), "boundary");
        assert_eq!(locus::analysis::category(&Label::World("console".into())), "world");
        assert_eq!(locus::analysis::category(&Label::World("fs".into())), "world");
        assert_eq!(locus::analysis::category(&Label::World("mem".into())), "memory");
        assert_eq!(locus::analysis::category(&Label::Gc), "memory");
        assert_eq!(locus::analysis::category(&Label::Exn("Overflow".into())), "control");
        assert_eq!(locus::analysis::category(&Label::User("Db".into())), "user");
        assert_eq!(locus::analysis::category(&Label::Insert), "staging");
        // every category produced is one the catalog also orders (no orphan bucket)
        let order = locus::analysis::category_order();
        for l in [
            Label::World("winapi".into()),
            Label::Gc,
            Label::User("x".into()),
            Label::Insert,
        ] {
            assert!(
                order.iter().any(|c| c == locus::analysis::category(&l)),
                "{l} fell outside the roll-up order"
            );
        }
    }

    /// A label not named explicitly falls back to its KIND — so a brand-new world
    /// label or user effect is still categorised (never silently dropped).
    #[test]
    fn unlisted_labels_fall_back_to_their_kind() {
        assert_eq!(locus::analysis::category(&Label::World("bluetooth".into())), "world"); // unlisted world
        assert_eq!(locus::analysis::category(&Label::User("Telemetry".into())), "user"); // unlisted user
                                                                        // and the gloss comes from the kind fallback, not empty
        assert!(!locus::analysis::describe(&Label::User("Telemetry".into())).is_empty());
    }

    /// The hand-rolled JSON escaper covers the cases that would break a parser;
    /// UTF-8 (the em-dash glosses) passes through untouched.
    #[test]
    fn json_escape_handles_specials() {
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(json_escape("tab\tnl\n"), "tab\\tnl\\n");
        assert_eq!(json_escape("plain — unicode"), "plain — unicode");
    }

    #[test]
    fn boundary_manifest_parses_the_module_list() {
        let m = super::parse_boundary_modules(
            "[package]\nname = \"x\"\n\n[boundary]\nmodules = [\"MyFfi\", \"Gpu\"]\n",
        );
        assert!(m.contains("MyFfi") && m.contains("Gpu") && m.len() == 2);
    }

    #[test]
    fn boundary_manifest_tolerates_a_multiline_array_and_no_section() {
        let multiline =
            super::parse_boundary_modules("[boundary]\nmodules = [\n  \"A\",\n  \"B\",\n]\n");
        assert!(multiline.contains("A") && multiline.contains("B"));
        // No `[boundary]` table ⇒ empty (the secure default: nothing authorized).
        assert!(super::parse_boundary_modules("[package]\nname = \"x\"\n").is_empty());
        // A `[boundary]` after the section is not mistaken for its contents.
        let scoped =
            super::parse_boundary_modules("[boundary]\nmodules = [\"A\"]\n[other]\nx = [\"Z\"]\n");
        assert!(scoped.contains("A") && !scoped.contains("Z") && scoped.len() == 1);
    }
}
