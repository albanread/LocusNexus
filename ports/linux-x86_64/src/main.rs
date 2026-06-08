//! Linux x86-64 sidecar driver for Locus.
//!
//! This project intentionally lives outside the root workspace while the port is
//! young. It reuses the shared front end, LLVM lowering, and runtime shims, but
//! owns the Linux-specific driver/JIT decisions locally.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::Once;

mod asm_runtime;

use inkwell::context::Context;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;
use llvm_sys::core::LLVMGetModuleContext;
use llvm_sys::error::{LLVMDisposeErrorMessage, LLVMErrorRef, LLVMGetErrorMessage};
use llvm_sys::orc2::lljit::{
    LLVMOrcCreateLLJIT, LLVMOrcCreateLLJITBuilder, LLVMOrcDisposeLLJIT,
    LLVMOrcLLJITAddLLVMIRModule, LLVMOrcLLJITGetExecutionSession, LLVMOrcLLJITGetGlobalPrefix,
    LLVMOrcLLJITGetMainJITDylib, LLVMOrcLLJITLookup, LLVMOrcLLJITRef,
};
use llvm_sys::orc2::{
    LLVMJITEvaluatedSymbol, LLVMJITSymbolFlags, LLVMJITSymbolGenericFlags, LLVMOrcAbsoluteSymbols,
    LLVMOrcCSymbolMapPair, LLVMOrcCreateDynamicLibrarySearchGeneratorForProcess,
    LLVMOrcCreateNewThreadSafeContextFromLLVMContext, LLVMOrcCreateNewThreadSafeModule,
    LLVMOrcDefinitionGeneratorRef, LLVMOrcDisposeThreadSafeContext, LLVMOrcExecutionSessionIntern,
    LLVMOrcExecutorAddress, LLVMOrcJITDylibAddGenerator, LLVMOrcJITDylibDefine, LLVMOrcJITDylibRef,
};

const USAGE: &str = "\
locusc - the Locus compiler on Linux x86-64 (JIT + AOT)

USAGE:
  locusc run     FILE            JIT-compile and run FILE
  locusc build   FILE [-o EXE]   compile FILE to an ELF executable
                   [--always-gc] link the collector even if FILE doesn't allocate
  locusc asm     FILE [-o OUT.s] dump the generated x86-64 assembly
  locusc effects FILE [--json]   print FILE's effect manifest (what it touches);
                                 --json emits a stable, diffable manifest for CI
  locusc republish [DIR]         write the embedded Linux stdlib to DIR for review
  locusc --help

OPTIONS:
  --trace-stack-usage            print compiler tree-depth / spine metrics to stderr

`run`'s exit code is the program's i64 result; effects print as they execute.
`asm` writes to stdout unless `-o` is given - the same code the executable contains.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = std::thread::Builder::new()
        .name("linux-locusc-main".into())
        .stack_size(locus::PIPELINE_STACK_BYTES)
        .spawn(move || match dispatch(&args) {
            Ok(code) => code,
            Err(msg) => {
                eprintln!("locusc: {msg}");
                2
            }
        })
        .expect("spawn linux locusc worker")
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
        Some("republish") => cmd_republish(&args[1..]),
        None | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            Ok(0)
        }
        Some(other) => Err(format!(
            "unknown command `{other}` (try `run`, `build`, `asm`, `effects`, `republish`, or `--help`)"
        )),
    }
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
    let compiled = compile_file(&file, trace_stack)?;
    let result = jit_run_i64(&compiled.ir, &compiled.demanded)?;
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
    let output = out
        .map(PathBuf::from)
        .unwrap_or_else(|| default_linux_exe_path(&file));
    let compiled = compile_file(&file, trace_stack)?;
    build_elf_executable(&compiled.ir, &compiled.demanded, &output, always_gc)?;
    println!("built {}", output.display());
    Ok(0)
}

fn cmd_asm(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut trace_stack = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("`-o` needs a path")?.clone()),
            "--trace-stack-usage" => trace_stack = true,
            other => file = Some(other.to_string()),
        }
    }
    let file = file.ok_or("usage: locusc asm [--trace-stack-usage] FILE [-o OUT.s]")?;
    let compiled = compile_file(&file, trace_stack)?;
    let asm = emit_asm_text(&compiled.ir)?;
    match out {
        Some(path) => {
            std::fs::write(&path, &asm).map_err(|e| format!("writing `{path}`: {e}"))?;
            println!("wrote {path}");
        }
        None => print!("{asm}"),
    }
    Ok(0)
}

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
    let src = read_source(file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(file)))?;
    if trace_stack {
        trace_stack_header("linux-locusc");
        if let Ok(program) = locus::parse_program(&src) {
            trace_shape("user source", locus::program_source_shape(&program));
        }
    }
    let term = locus::linux_program(&src).map_err(|e| e.msg)?;
    if trace_stack {
        trace_shape("stdlib-grafted term", locus::term_shape(&term));
    }
    let (term, _demanded) = locus_libc::resolve(term)?;
    if trace_stack {
        trace_shape("libc-resolved term", locus::term_shape(&term));
    }
    let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
        .map_err(|e| e.to_string())?;
    if trace_stack {
        trace_shape("typed tree", locus::typed_shape(&tree));
    }
    let labels: Vec<&locus::Label> = tree.row.labels().collect();
    let ty = tree.ty.to_string();
    // The module declarations the layer attribution reads from (the Linux stdlib
    // set `linux_program` grafts), so each effect can show the layer it enters at.
    let decls = locus::linux_stdlib_module_decls();
    if json {
        print_effects_json(file, &ty, &labels, &decls);
    } else {
        print_effects_human(file, &ty, &labels, &decls);
    }
    Ok(0)
}

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

fn print_effects_human(file: &str, ty: &str, labels: &[&locus::Label], decls: &[locus::ModuleDecl]) {
    println!("{file}");
    println!("  type    : {ty}");
    if labels.is_empty() {
        println!("  effects : {{}}  - pure (touches nothing outside itself)");
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

fn cmd_republish(args: &[String]) -> Result<i32, String> {
    let dir: PathBuf = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| "stdlib-republished".into());
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating `{}`: {e}", dir.display()))?;
    let mut manifest = String::from(
        "# Locus Linux platform - republished from the compiler's embedded copy.\n\
         # layer  module       bytes    fnv1a-64            file\n",
    );
    for (layer, name, src) in locus::linux_stdlib_modules() {
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

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn compile_file(file: &str, trace_stack: bool) -> Result<CompiledProgram, String> {
    let src = read_source(file)?;
    compile_source_on_pipeline(&src, Path::new(file), trace_stack)
}

fn read_source(file: &str) -> Result<String, String> {
    std::fs::read_to_string(file).map_err(|e| format!("reading `{file}`: {e}"))
}

fn default_linux_exe_path(file: &str) -> PathBuf {
    let p = PathBuf::from(file);
    match (p.parent(), p.file_stem()) {
        (Some(parent), Some(stem)) if !parent.as_os_str().is_empty() => parent.join(stem),
        (_, Some(stem)) => PathBuf::from(stem),
        _ => p,
    }
}

struct CompiledProgram {
    ir: locus::Ir,
    demanded: locus_libc::Demanded,
}

fn run_source_on_pipeline(src: &str, source_file: &Path) -> Result<i64, String> {
    let compiled = compile_source_on_pipeline(src, source_file, false)?;
    jit_run_i64(&compiled.ir, &compiled.demanded)
}

fn compile_source_on_pipeline(
    src: &str,
    source_file: &Path,
    trace_stack: bool,
) -> Result<CompiledProgram, String> {
    let src = src.to_string();
    let source_file = source_file.to_path_buf();
    std::thread::Builder::new()
        .name("locus-linux-pipeline".into())
        .stack_size(locus::PIPELINE_STACK_BYTES)
        .spawn(move || {
            if trace_stack {
                trace_stack_header("linux-locusc");
                if let Ok(program) = locus::parse_program(&src) {
                    trace_shape("user source", locus::program_source_shape(&program));
                }
            }
            let (term, user_modules) =
                locus::linux_program_with_modules(&src).map_err(|e| e.msg)?;
            if trace_stack {
                trace_shape("stdlib-grafted term", locus::term_shape(&term));
            }
            let (term, demanded) = locus_libc::resolve(term)?;
            if trace_stack {
                trace_shape("libc-resolved term", locus::term_shape(&term));
            }
            guard_layer2(&src, &read_boundary_manifest(&source_file))?;
            let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
                .map_err(|e| e.to_string())?;
            if trace_stack {
                trace_shape("typed tree", locus::typed_shape(&tree));
            }
            let mut all_modules = locus::linux_stdlib_module_decls();
            all_modules.extend(user_modules);
            locus::check_module_seals(&all_modules, &tree).map_err(|e| e.to_string())?;
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
            Ok(CompiledProgram { ir, demanded })
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "locus-linux pipeline worker panicked".to_string())?
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

fn read_boundary_manifest(source_file: &Path) -> std::collections::HashSet<String> {
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
    std::collections::HashSet::new()
}

fn parse_boundary_modules(toml: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
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

fn guard_layer2(src: &str, authorized: &std::collections::HashSet<String>) -> Result<(), String> {
    let prog = locus::parse_program(src).map_err(|e| e.msg)?;
    locus::mint_gate(&prog.entry, &prog.modules, authorized)
        .map_err(|e| format!("[{}] {e}", e.code()))
}

fn init_target() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        Target::initialize_native(&InitializationConfig::default())
            .expect("LLVM native target init failed");
    });
}

fn host_target_machine() -> Result<(TargetMachine, TargetTriple), String> {
    init_target();
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).map_err(|e| e.to_string())?;
    let tm = target
        .create_target_machine(
            &triple,
            "generic",
            "",
            OptimizationLevel::Default,
            RelocMode::Default,
            CodeModel::Default,
        )
        .ok_or_else(|| "failed to create host target machine".to_string())?;
    Ok((tm, triple))
}

fn take_error(err: LLVMErrorRef) -> String {
    unsafe {
        let cmsg = LLVMGetErrorMessage(err);
        let s = CStr::from_ptr(cmsg).to_string_lossy().into_owned();
        LLVMDisposeErrorMessage(cmsg);
        s
    }
}

unsafe fn define_absolute_symbols(
    jit: LLVMOrcLLJITRef,
    jd: LLVMOrcJITDylibRef,
    symbols: &[(String, u64)],
) -> Result<(), String> {
    if symbols.is_empty() {
        return Ok(());
    }
    let es = LLVMOrcLLJITGetExecutionSession(jit);
    let mut pairs: Vec<LLVMOrcCSymbolMapPair> = Vec::new();
    for (name, addr) in symbols {
        let cname = CString::new(name.as_str()).map_err(|_| format!("bad symbol name `{name}`"))?;
        let interned = LLVMOrcExecutionSessionIntern(es, cname.as_ptr());
        let flags = LLVMJITSymbolFlags {
            GenericFlags: LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsExported as u8
                | LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsCallable as u8,
            TargetFlags: 0,
        };
        pairs.push(LLVMOrcCSymbolMapPair {
            Name: interned,
            Sym: LLVMJITEvaluatedSymbol {
                Address: *addr,
                Flags: flags,
            },
        });
    }
    let mu = LLVMOrcAbsoluteSymbols(pairs.as_mut_ptr(), pairs.len());
    let err = LLVMOrcJITDylibDefine(jd, mu);
    if !err.is_null() {
        return Err(format!(
            "defining absolute symbols failed: {}",
            take_error(err)
        ));
    }
    Ok(())
}

fn jit_run_i64(ir: &locus::Ir, demanded: &locus_libc::Demanded) -> Result<i64, String> {
    init_target();

    let ctx = Context::create();
    let module = locus_llvm::lower::emit_module(&ctx, ir, false)?;
    let mod_raw = module.as_mut_ptr();
    let ctx_raw = unsafe { LLVMGetModuleContext(mod_raw) };
    std::mem::forget(module);
    std::mem::forget(ctx);

    let builder = unsafe { LLVMOrcCreateLLJITBuilder() };
    let mut jit: LLVMOrcLLJITRef = std::ptr::null_mut();
    let err = unsafe { LLVMOrcCreateLLJIT(&mut jit, builder) };
    if !err.is_null() {
        return Err(format!("LLJIT creation failed: {}", take_error(err)));
    }

    let jd = unsafe { LLVMOrcLLJITGetMainJITDylib(jit) };

    unsafe {
        let prefix = LLVMOrcLLJITGetGlobalPrefix(jit);
        let mut generator: LLVMOrcDefinitionGeneratorRef = std::ptr::null_mut();
        let gerr = LLVMOrcCreateDynamicLibrarySearchGeneratorForProcess(
            &mut generator,
            prefix,
            None,
            std::ptr::null_mut(),
        );
        if gerr.is_null() {
            LLVMOrcJITDylibAddGenerator(jd, generator);
        } else {
            let _ = take_error(gerr);
        }
    }

    let mut symbols: Vec<(String, u64)> = locus_rt::runtime_symbols()
        .into_iter()
        .map(|(name, addr)| (name.to_string(), addr))
        .collect();
    symbols.extend(
        asm_runtime::runtime_symbols()
            .into_iter()
            .map(|(name, addr)| (name.to_string(), addr)),
    );
    symbols.extend(locus_libc::resolve_absolute_symbols(demanded)?);
    if let Err(e) = unsafe { define_absolute_symbols(jit, jd, &symbols) } {
        unsafe { LLVMOrcDisposeLLJIT(jit) };
        return Err(e);
    }

    let tsc = unsafe { LLVMOrcCreateNewThreadSafeContextFromLLVMContext(ctx_raw) };
    let tsm = unsafe { LLVMOrcCreateNewThreadSafeModule(mod_raw, tsc) };
    unsafe { LLVMOrcDisposeThreadSafeContext(tsc) };

    let err = unsafe { LLVMOrcLLJITAddLLVMIRModule(jit, jd, tsm) };
    if !err.is_null() {
        let msg = take_error(err);
        unsafe { LLVMOrcDisposeLLJIT(jit) };
        return Err(format!("adding module failed: {msg}"));
    }

    let cname = CString::new("__locus_main").unwrap();
    let mut addr: LLVMOrcExecutorAddress = 0;
    let err = unsafe { LLVMOrcLLJITLookup(jit, &mut addr, cname.as_ptr()) };
    if !err.is_null() {
        let msg = take_error(err);
        unsafe { LLVMOrcDisposeLLJIT(jit) };
        return Err(format!("lookup of `__locus_main` failed: {msg}"));
    }
    if addr == 0 {
        unsafe { LLVMOrcDisposeLLJIT(jit) };
        return Err("`__locus_main` resolved to a null address".into());
    }

    let main: extern "C" fn() -> i64 = unsafe { std::mem::transmute(addr as usize) };
    let result = main();
    unsafe { LLVMOrcDisposeLLJIT(jit) };
    Ok(result)
}

fn emit_asm_text(ir: &locus::Ir) -> Result<String, String> {
    let ctx = Context::create();
    let module = locus_llvm::lower::emit_module(&ctx, ir, false)?;
    let (tm, triple) = host_target_machine()?;
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    asm_runtime::embed_runtime_asm(&module)?;
    let buf = tm
        .write_to_memory_buffer(&module, FileType::Assembly)
        .map_err(|e| e.to_string())?;
    std::str::from_utf8(buf.as_slice())
        .map(str::to_string)
        .map_err(|e| format!("LLVM emitted non-UTF8 assembly: {e}"))
}

fn emit_elf_object(ir: &locus::Ir, path: &Path, always_gc: bool) -> Result<(), String> {
    let ctx = Context::create();
    let module = locus_llvm::lower::emit_module(&ctx, ir, always_gc)?;
    let (tm, triple) = host_target_machine()?;
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    asm_runtime::embed_runtime_asm(&module)?;
    let buf = tm
        .write_to_memory_buffer(&module, FileType::Object)
        .map_err(|e| e.to_string())?;
    std::fs::write(path, buf.as_slice()).map_err(|e| format!("writing {}: {e}", path.display()))
}

const ELF_C_MAIN: &str = "\
#include <stdint.h>\n\
\n\
extern int64_t __locus_main(void);\n\
\n\
int main(void) { return (int)__locus_main(); }\n";

const ELF_C_STUB_RUNTIME: &str = "\
#include <stdint.h>\n\
#include <stdio.h>\n\
#include <stdlib.h>\n\
\n\
void* locus_alloc(int64_t bytes) {\n\
  size_t n = bytes <= 0 ? 8u : (size_t)bytes;\n\
  n = (n + 7u) & ~(size_t)7u;\n\
  return malloc(n);\n\
}\n\
\n\
void locus_write_float(int64_t bits) {\n\
  union { uint64_t u; double d; } v;\n\
  v.u = (uint64_t)bits;\n\
  printf(\"%.17g\\n\", v.d);\n\
}\n\
\n\
double locus_fp64_add(double a, double b) { return a + b; }\n\
double locus_fp64_add_i64(double a, int64_t b) { return a + (double)b; }\n\
float locus_fp32_id(float x) { return x; }\n\
\n";

fn compile_c_runtime(obj: &Path, link_real_runtime: bool) -> Result<(), String> {
    let src = obj.with_extension("c");
    let source = if link_real_runtime {
        ELF_C_MAIN.to_string()
    } else {
        format!("{ELF_C_STUB_RUNTIME}{ELF_C_MAIN}")
    };
    std::fs::write(&src, source).map_err(|e| format!("writing runtime source: {e}"))?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let out = Command::new(&cc)
        .arg("-c")
        .arg(&src)
        .arg("-o")
        .arg(obj)
        .output()
        .map_err(|e| format!("failed to invoke `{cc}`: {e}"))?;
    let _ = std::fs::remove_file(&src);
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{cc}` failed compiling runtime ({}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn build_elf_executable(
    ir: &locus::Ir,
    demanded: &locus_libc::Demanded,
    output: &Path,
    always_gc: bool,
) -> Result<(), String> {
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("locus");
    let dir = output.parent().filter(|p| !p.as_os_str().is_empty());
    let join = |name: String| match dir {
        Some(d) => d.join(name),
        None => PathBuf::from(name),
    };
    let program_obj = join(format!("{stem}.program.o"));
    let runtime_obj = join(format!("{stem}.runtime.o"));
    let runtime_staticlib = locate_runtime_staticlib();
    if always_gc && runtime_staticlib.is_none() {
        return Err(
            "`--always-gc` needs `liblocus_rt.a`; build `locus-rt` or set LOCUS_RT_STATICLIB"
                .into(),
        );
    }

    emit_elf_object(ir, &program_obj, always_gc)?;
    compile_c_runtime(&runtime_obj, runtime_staticlib.is_some())?;
    let link = link_elf_executable(
        &[&program_obj, &runtime_obj],
        output,
        demanded,
        runtime_staticlib.as_deref(),
    );

    let _ = std::fs::remove_file(&program_obj);
    let _ = std::fs::remove_file(&runtime_obj);
    link
}

fn link_elf_executable(
    objects: &[&Path],
    output: &Path,
    demanded: &locus_libc::Demanded,
    runtime_staticlib: Option<&Path>,
) -> Result<(), String> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let mut cmd = Command::new(&cc);
    for obj in objects {
        cmd.arg(obj);
    }
    cmd.arg("-no-pie").arg("-o").arg(output);
    if let Some(runtime) = runtime_staticlib {
        cmd.arg(runtime);
    }
    for flag in linux_link_flags(demanded) {
        cmd.arg(flag);
    }
    if runtime_staticlib.is_some() {
        for flag in ["-lpthread", "-ldl", "-lm"] {
            cmd.arg(flag);
        }
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to invoke `{cc}`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{cc}` failed linking executable ({}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn linux_link_flags(demanded: &locus_libc::Demanded) -> Vec<String> {
    let mut flags = std::collections::BTreeSet::new();
    for lib in demanded.values() {
        match lib.as_str() {
            "libc.so.6" => {}
            "libm.so.6" => {
                flags.insert("-lm".to_string());
            }
            other => {
                flags.insert(format!("-Wl,-l:{other}"));
            }
        }
    }
    flags.into_iter().collect()
}

fn locate_runtime_staticlib() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("LOCUS_RT_STATICLIB").map(PathBuf::from) {
        if path.exists() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for cand in [
        dir.join("liblocus_rt.a"),
        dir.join("..").join("liblocus_rt.a"),
        dir.join("..").join("..").join("liblocus_rt.a"),
    ] {
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Result<i64, String> {
        run_source_on_pipeline(src, Path::new("test.locus"))
    }

    fn run_with_boundary_manifest(src: &str, modules: &[&str]) -> Result<i64, String> {
        let (root, source_file) = temp_project(src, modules)?;
        let result = run_source_on_pipeline(src, &source_file);
        let _ = std::fs::remove_dir_all(root);
        result
    }

    fn temp_project(src: &str, modules: &[&str]) -> Result<(PathBuf, PathBuf), String> {
        let unique = format!(
            "locus-linux-sidecar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let quoted = modules
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            root.join("locus.toml"),
            format!("[boundary]\nmodules = [{quoted}]\n"),
        )
        .map_err(|e| e.to_string())?;
        let source_file = root.join("main.locus");
        std::fs::write(&source_file, src).map_err(|e| e.to_string())?;
        Ok((root, source_file))
    }

    #[test]
    fn cli_usage_tracks_the_compiler_surface() {
        for cmd in ["run", "build", "asm", "effects", "republish"] {
            assert!(USAGE.contains(cmd), "usage missing `{cmd}`");
        }
        assert!(
            !USAGE.contains("emit-obj"),
            "Linux agent CLI should match locusc's public command surface"
        );
    }

    #[test]
    fn pure_integer_program_runs() {
        assert_eq!(run("let x = 40 in x + 2").unwrap(), 42);
    }

    #[test]
    fn gc_program_runs() {
        assert_eq!(run("let a = [1, 2, 39] in a[0] + a[1] + a[2]").unwrap(), 42);
    }

    #[test]
    fn math_program_runs_through_libm() {
        assert_eq!(f64::from_bits(run("pow 2.0 3.0").unwrap() as u64), 8.0);
        assert_eq!(f64::from_bits(run("sin 0.0").unwrap() as u64), 0.0);
    }

    #[test]
    fn console_program_runs_through_linux_libc() {
        assert_eq!(run(r#"writeln "hello from linux stdlib""#).unwrap(), 0);
        assert_eq!(
            run("writeln \"Hello, \u{4e16}\u{754c} \u{1f389}\"").unwrap(),
            0
        );
    }

    #[test]
    fn bare_libc_externs_materialize_in_boundary_module() {
        let src = r#"
module C at boundary mints (libc) =
  let malloc = extern "malloc" in
  let write = extern "write" in
  ()

let buf = malloc 1 in
let _ = buf[0] <- 42 in
let n = write 1 buf 1 in
n + 41
"#;
        assert_eq!(run_with_boundary_manifest(src, &["C"]).unwrap(), 42);
    }

    #[test]
    fn bare_libm_extern_materializes_in_boundary_module() {
        let src = r#"
module M at boundary mints (libm) =
  let pow = extern "pow" in
  ()

pow 2.0 3.0
"#;
        assert_eq!(
            f64::from_bits(run_with_boundary_manifest(src, &["M"]).unwrap() as u64),
            8.0
        );
    }

    #[test]
    fn asm_command_writes_x86_64_assembly() {
        let (root, source_file) = temp_project("42", &[]).unwrap();
        let asm = root.join("out.s");
        let args = vec![
            source_file.display().to_string(),
            "-o".to_string(),
            asm.display().to_string(),
        ];
        assert_eq!(cmd_asm(&args).unwrap(), 0);
        let text = std::fs::read_to_string(&asm).unwrap();
        let _ = std::fs::remove_dir_all(root);
        assert!(text.contains("__locus_main"));
    }

    #[test]
    fn effects_command_accepts_linux_bare_externs() {
        let src = r#"
module C at boundary mints (libc) =
  let write = extern "write" in
  ()

write 1 "x" 1
"#;
        let (root, source_file) = temp_project(src, &["C"]).unwrap();
        let args = vec![source_file.display().to_string(), "--json".to_string()];
        assert_eq!(cmd_effects(&args).unwrap(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn republish_command_writes_linux_stdlib_and_catalog() {
        let (root, _source_file) = temp_project("0", &[]).unwrap();
        let out = root.join("platform");
        let args = vec![out.display().to_string()];
        assert_eq!(cmd_republish(&args).unwrap(), 0);
        assert!(out.join("0_libc.locus").is_file());
        assert!(out.join("0_libm.locus").is_file());
        assert!(out.join("effects.catalog").is_file());
        assert!(out.join("MANIFEST.txt").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn linux_runtime_masm_is_sysv_shaped() {
        let mc = asm_runtime::assemble_runtime().unwrap();
        assert!(mc.contains(".globl locus_asm_rotl64"));
        assert!(asm_runtime::RUNTIME_MASM.contains("mov rax, rdi"));
        assert!(asm_runtime::RUNTIME_MASM.contains("mov rcx, rsi"));
        assert!(asm_runtime::RUNTIME_MASM.contains("cmp rax, rdi"));
        assert!(!asm_runtime::RUNTIME_MASM.contains("GetStdHandle"));
    }

    #[test]
    fn asm_program_runs_from_manifested_boundary_module() {
        let src = r#"
module Bits at boundary mints (asm) =
  let rotl = extern asm "locus_asm_rotl64" : Int -> Int -> Int in
  let popcount = extern asm "locus_asm_popcount64" : Int -> Int in
  let bswap = extern asm "locus_asm_bswap64" : Int -> Int in
  ()

(rotl 1 4) + (popcount 255) + (bswap (bswap 18))
"#;
        assert_eq!(run_with_boundary_manifest(src, &["Bits"]).unwrap(), 42);
    }

    #[test]
    fn asm_mandel_dev_twin_uses_sysv_mixed_args() {
        let src = r#"
module Fractal at boundary mints (asm) =
  let mandel = extern asm "locus_asm_mandel" : Float -> Float -> Int -> Int in
  ()

(mandel 0.0 0.0 100) + (mandel 2.0 2.0 100)
"#;
        assert_eq!(run_with_boundary_manifest(src, &["Fractal"]).unwrap(), 101);
    }

    #[test]
    fn aot_elf_embeds_masm_and_links() {
        let src = r#"
module Bits at boundary mints (asm) =
  let rotl = extern asm "locus_asm_rotl64" : Int -> Int -> Int in
  let popcount = extern asm "locus_asm_popcount64" : Int -> Int in
  let bswap = extern asm "locus_asm_bswap64" : Int -> Int in
  ()

(rotl 1 4) + (popcount 255) + (bswap (bswap 18))
"#;
        let (root, source_file) = temp_project(src, &["Bits"]).unwrap();
        let compiled = compile_source_on_pipeline(src, &source_file, false).unwrap();
        let exe = root.join("bits");
        build_elf_executable(&compiled.ir, &compiled.demanded, &exe, false).unwrap();
        let status = Command::new(&exe).status().expect("run ELF executable");
        let _ = std::fs::remove_dir_all(root);
        assert_eq!(status.code(), Some(42));
    }

    #[test]
    fn aot_elf_links_real_gc_runtime_when_available() {
        if locate_runtime_staticlib().is_none() {
            return;
        }
        let src = "let a = [1, 2, 39] in a[0] + a[1] + a[2]";
        let (root, source_file) = temp_project(src, &[]).unwrap();
        let compiled = compile_source_on_pipeline(src, &source_file, false).unwrap();
        let exe = root.join("gc-array");
        build_elf_executable(&compiled.ir, &compiled.demanded, &exe, false).unwrap();
        let status = Command::new(&exe).status().expect("run GC ELF executable");
        let _ = std::fs::remove_dir_all(root);
        assert_eq!(status.code(), Some(42));
    }
}
