//! AOT — compile a program to a standalone **`.exe`** (the shipping half of
//! "JIT *and* AOT").
//!
//! The same [`emit_module`](crate::lower) the JIT uses is run through an LLVM
//! `TargetMachine` to a native object (COFF on Windows), then MSVC `link.exe`
//! links it with a tiny C runtime and the CRT into an executable. The C runtime
//! supplies the CRT entry (`main`, which calls the program's `__locus_main`) and
//! the closure allocator `locus_alloc` — the same one the JIT binds as an
//! absolute symbol, resolved here at **link** time. (Output is not here: it is
//! the Locus `console_writeln` prelude over Win32.) Model: NewBF's `aot.rs`.

use std::path::Path;
use std::sync::Once;

use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;

/// The IR optimization pipeline run over every **executing** artifact (the AOT
/// object and the JIT module) before codegen. `-O2` is the *floor*, not the
/// advantage (`docs/sofar_nofurther.md`: the real headroom is the facts the effect
/// rows carry, which LLVM abandons) — but without it the emitted IR reaches the
/// backend unoptimized and the numbers are artificially bad. The `asm` *dump*
/// (`emit_asm`) deliberately stays unoptimized: it is the honest view of what the
/// lowering itself produces.
///
/// **Soundness under a moving GC:** the handle discipline makes this safe *by
/// construction* — GC pointers live as opaque `i64` handles; a raw pointer is
/// materialized (`locus_gc_get_ptr`) only transiently and is never held across an
/// allocation in the IR, and the runtime shims are *external* calls (opaque, never
/// inlined or reordered across each other), so no pass can float a raw pointer
/// across a collection. The full GC test suite is the standing proof.
pub(crate) const OPT_PIPELINE: &str = "default<O2>";

/// Run [`OPT_PIPELINE`] over `module` in place. Shared by [`emit_object`] (AOT) and
/// the JIT path so `locusc run` and `locusc build` optimize identically.
pub(crate) fn run_opt_pipeline(module: &Module, tm: &TargetMachine) -> Result<(), String> {
    module
        .run_passes(OPT_PIPELINE, tm, PassBuilderOptions::create())
        .map_err(|e| format!("LLVM optimization pipeline (`{OPT_PIPELINE}`) failed: {e}"))
}

use locus::Ir;

use crate::lower::emit_module;

fn init_native_target() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        Target::initialize_native(&InitializationConfig::default())
            .expect("LLVM native target init failed");
    });
}

/// A `TargetMachine` for the host, plus its triple. `pub(crate)` so the JIT path
/// can build one to drive the shared optimization pipeline ([`run_opt_pipeline`]).
pub(crate) fn host_target_machine() -> Result<(TargetMachine, TargetTriple), String> {
    init_native_target();
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

/// Lower `ir` and write a native object file (the program — `__locus_main` plus
/// its calls to runtime symbols) to `path`.
pub fn emit_object(ir: &Ir, path: &Path, always_gc: bool) -> Result<(), String> {
    let ctx = Context::create();
    let module = emit_module(&ctx, ir, always_gc)?;
    let (tm, triple) = host_target_machine()?;
    // The object's triple + layout must match the target machine or the linker
    // and LLVM disagree about the codegen.
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    // Embed the Layer-0 runtime asm primitives (D5): their `.globl` symbols land
    // in this COFF and co-resolve with any `extern asm` call at link. AOT-only —
    // the JIT/ORC path can't see module inline asm (jasm-boundary-layer.md §3).
    crate::asm_runtime::embed_runtime_asm(&module)?;
    // Optimize the IR (-O2 floor) before codegen — the shipped `.exe` runs the
    // optimized module. (The embedded Layer-0 `.masm` is module-level inline asm,
    // untouched by the IR passes.)
    run_opt_pipeline(&module, &tm)?;
    let buf = tm
        .write_to_memory_buffer(&module, FileType::Object)
        .map_err(|e| e.to_string())?;
    std::fs::write(path, buf.as_slice()).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Lower `ir` and emit the host **x86-64 assembly** for the program
/// (`__locus_main` and its lifted lambdas / handler blocks) as text. The
/// systems-level view: exactly the machine code the high-level program — effects,
/// handlers, the `mem` accessor — compiled to. Same module the JIT and AOT use,
/// run through the `TargetMachine` with `FileType::Assembly` instead of `Object`.
pub fn emit_asm(ir: &Ir) -> Result<String, String> {
    emit_asm_inner(ir, false)
}

/// Like [`emit_asm`], but runs the **`-O2` IR pipeline** ([`run_opt_pipeline`]) first —
/// the assembly the shipped `.exe` actually carries. Plain [`emit_asm`] is the honest
/// *lowering* view (what the front end emits); this is the *optimized* view (what runs).
/// Useful for perf inspection — e.g. confirming a `let mut` slot is promoted to a
/// register (mem2reg), with no memory traffic.
pub fn emit_asm_opt(ir: &Ir) -> Result<String, String> {
    emit_asm_inner(ir, true)
}

/// Lower `ir` and emit the **LLVM IR** (textual `.ll`) for the program — the
/// mid-level view between the ANF IR and the host assembly: the same module the
/// JIT and AOT build, printed. The honest *lowering* view (what the front end
/// emits, pre-optimization).
pub fn emit_llvm_ir(ir: &Ir) -> Result<String, String> {
    emit_llvm_ir_inner(ir, false)
}

/// Like [`emit_llvm_ir`], but runs the **`-O2` IR pipeline** first — the LLVM IR
/// after optimization, closer to what the assembly is generated from.
pub fn emit_llvm_ir_opt(ir: &Ir) -> Result<String, String> {
    emit_llvm_ir_inner(ir, true)
}

fn emit_llvm_ir_inner(ir: &Ir, optimize: bool) -> Result<String, String> {
    let ctx = Context::create();
    let module = emit_module(&ctx, ir, false)?;
    let (tm, triple) = host_target_machine()?;
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    if optimize {
        run_opt_pipeline(&module, &tm)?;
    }
    Ok(module.print_to_string().to_string())
}

fn emit_asm_inner(ir: &Ir, optimize: bool) -> Result<String, String> {
    let ctx = Context::create();
    // The asm dump neither links nor runs, so the gc policy is irrelevant here.
    let module = emit_module(&ctx, ir, false)?;
    let (tm, triple) = host_target_machine()?;
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    if optimize {
        run_opt_pipeline(&module, &tm)?;
    }
    let buf = tm
        .write_to_memory_buffer(&module, FileType::Assembly)
        .map_err(|e| e.to_string())?;
    String::from_utf8(buf.as_slice().to_vec())
        .map_err(|e| format!("assembly was not valid UTF-8: {e}"))
}

/// The C runtime, linked alongside the program. It provides the CRT entry
/// `main` (which runs the program and uses its `i64` result as the exit code)
/// and the closure allocator `locus_alloc`. Output is no longer here — it is the
/// Locus `console_writeln` prelude (readable over Win32). No headers — `malloc` is
/// declared directly, so `cl.exe` needs no `%INCLUDE%`.
// The CRT entry: runs the program and uses its `i64` result as the exit code.
const CRT_MAIN: &str = "\
extern long long __locus_main(void);\n\
int main(void) { return (int)__locus_main(); }\n";

// `locus_alloc` (closures) as plain C — used ONLY when the real runtime staticlib
// isn't linked. An allocating program links `locus_rt.lib`, which provides
// `locus_alloc` itself, so this would collide; hence it's conditional.
const C_ALLOC: &str = "\
extern void* malloc(unsigned long long);\n\
extern int printf(const char*, ...);\n\
void* locus_alloc(long long n) { return malloc((unsigned long long)n); }\n\
void locus_write_float(long long bits) {\n\
  union { unsigned long long u; double d; } v;\n\
  v.u = (unsigned long long)bits;\n\
  printf(\"%.17g\\n\", v.d);\n\
}\n\
double locus_fp64_add(double a, double b) { return a + b; }\n\
double locus_fp64_add_i64(double a, long long b) { return a + (double)b; }\n\
float locus_fp32_id(float x) { return x; }\n";

/// Compile the C runtime to `obj`. When `link_real_runtime` is set the managed
/// heap (incl. `locus_alloc`) comes from `locus_rt.lib`, so only the CRT entry is
/// compiled here; otherwise a plain C `locus_alloc` is included for closures.
fn compile_runtime(obj: &Path, link_real_runtime: bool) -> Result<(), String> {
    let src = obj.with_extension("c");
    let source = if link_real_runtime {
        CRT_MAIN.to_string()
    } else {
        format!("{C_ALLOC}{CRT_MAIN}")
    };
    std::fs::write(&src, source).map_err(|e| format!("writing runtime source: {e}"))?;
    let mut cmd = cc::windows_registry::find("x86_64-pc-windows-msvc", "cl.exe")
        .ok_or_else(|| "could not locate MSVC cl.exe (install VS Build Tools)".to_string())?;
    cmd.arg("/nologo")
        .arg("/c")
        .arg(format!("/Fo{}", obj.display()))
        .arg(&src);
    let out = cmd
        .output()
        .map_err(|e| format!("failed to invoke cl.exe: {e}"))?;
    let _ = std::fs::remove_file(&src);
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "cl.exe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Link `objects` into a console `.exe` at `output`, driving MSVC `link.exe`.
/// `cc::windows_registry::find` populates `%LIB%`, so the CRT import libs below
/// resolve without a Developer Command Prompt.
fn link_executable(objects: &[&Path], output: &Path, import_libs: &[String]) -> Result<(), String> {
    let mut cmd = cc::windows_registry::find("x86_64-pc-windows-msvc", "link.exe")
        .ok_or_else(|| "could not locate MSVC link.exe (install VS Build Tools)".to_string())?;
    for obj in objects {
        cmd.arg(obj);
    }
    cmd.arg(format!("/OUT:{}", output.display()));
    cmd.arg("/SUBSYSTEM:CONSOLE");
    cmd.arg("/ENTRY:mainCRTStartup");
    cmd.arg("/MACHINE:X64");
    cmd.arg("/STACK:16777216");
    cmd.arg("/NXCOMPAT");
    cmd.arg("/DYNAMICBASE");
    for lib in [
        "kernel32.lib",
        "msvcrt.lib",
        "ucrt.lib",
        "vcruntime.lib",
        "legacy_stdio_definitions.lib",
    ] {
        cmd.arg(lib);
    }
    // Import libs for the DLLs the program's externs demand (e.g. `user32.lib`
    // for MessageBoxW). kernel32 is already above; any duplicate is harmless.
    for lib in import_libs {
        cmd.arg(lib);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to invoke link.exe: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "link.exe failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Compile `ir` to a standalone `.exe` at `exe`: emit the program object,
/// compile the C runtime, and link them with the CRT plus the demanded Win32
/// `import_libs` (the resolver derives these from the program's externs' DLLs).
pub fn build_exe(
    ir: &Ir,
    exe: &Path,
    import_libs: &[String],
    always_gc: bool,
) -> Result<(), String> {
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("locus");
    let dir = exe.parent().filter(|p| !p.as_os_str().is_empty());
    let join = |name: String| match dir {
        Some(d) => d.join(name),
        None => std::path::PathBuf::from(name),
    };
    let program_obj = join(format!("{stem}.program.obj"));
    let runtime_obj = join(format!("{stem}.runtime.obj"));

    // A program that performs the `gc` effect links the real managed-heap
    // runtime (the same collector the JIT uses); a gc-free program stays thin —
    // unless `--always-gc` forces the collector on regardless.
    let needs_gc = always_gc || crate::lower::block_performs_gc(ir);

    emit_object(ir, &program_obj, always_gc)?;
    compile_runtime(&runtime_obj, needs_gc)?;

    let mut libs = import_libs.to_vec();
    // The embedded runtime `.masm` may itself `call` a Win32 symbol (A4); its
    // import libs come from the SAME oracle the program's externs use, so the
    // asm's call resolves at link with no second copy of the API data.
    libs.extend(crate::asm_runtime::runtime_asm_import_libs()?);
    if needs_gc {
        libs.push(locate_runtime_staticlib()?.to_string_lossy().into_owned());
        // The Rust staticlib pulls in std; these are the system import libs its
        // std + the windows crate (VirtualAlloc &c.) resolve against.
        for l in [
            "ws2_32.lib",
            "userenv.lib",
            "advapi32.lib",
            "bcrypt.lib",
            "ntdll.lib",
        ] {
            libs.push(l.to_string());
        }
    }
    let link = link_executable(&[&program_obj, &runtime_obj], exe, &libs);

    let _ = std::fs::remove_file(&program_obj);
    let _ = std::fs::remove_file(&runtime_obj);
    link
}

/// Emit a **library object** — the producer's exported first-order functions as
/// flat, externally-visible uniform-ABI symbols (`docs/separate-compilation.md`
/// §3; the cross-module codegen ABI). NO `__locus_main` (a library is not a
/// program) and NO closure env (a module binding is closed). The client object
/// declares the same mangled symbols external and calls them directly; the linker
/// resolves them ([`link_program`]). The Layer-0 runtime asm is **not** embedded
/// here — that lives in the program object (`emit_object`) so its `.globl`s are
/// defined exactly once across the linked objects.
pub fn emit_library_object(
    exports: &[crate::lower::LibExport],
    path: &Path,
    always_gc: bool,
) -> Result<(), String> {
    let ctx = Context::create();
    let module = crate::lower::emit_library_module(&ctx, exports, always_gc)?;
    let (tm, triple) = host_target_machine()?;
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    let buf = tm
        .write_to_memory_buffer(&module, FileType::Object)
        .map_err(|e| e.to_string())?;
    std::fs::write(path, buf.as_slice()).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// **Link an already-emitted set of program objects** into a `.exe` — the
/// multi-object AOT path (`docs/separate-compilation-sprints.md` Sprint 3). The
/// caller has emitted each object (the client `App.o` via [`emit_object`], a
/// producer `Lib.o` via [`emit_library_object`]); this compiles the shared C
/// runtime, then links **all** of `objects` + that runtime with the CRT, the
/// demanded Win32 `import_libs`, and — when `needs_gc` — the single shared
/// `locus_rt.lib` collector (one GC across both units). `needs_gc` is the caller's
/// whole-program decision (true if *any* object allocates, or `--always-gc`); the
/// runtime stub is compiled accordingly so `locus_alloc` does not collide with the
/// staticlib's.
pub fn link_program(
    objects: &[&Path],
    exe: &Path,
    import_libs: &[String],
    needs_gc: bool,
) -> Result<(), String> {
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("locus");
    let dir = exe.parent().filter(|p| !p.as_os_str().is_empty());
    let runtime_obj = match dir {
        Some(d) => d.join(format!("{stem}.runtime.obj")),
        None => std::path::PathBuf::from(format!("{stem}.runtime.obj")),
    };
    compile_runtime(&runtime_obj, needs_gc)?;

    let mut libs = import_libs.to_vec();
    // The program object's embedded `.masm` may call a Win32 symbol (A4); its
    // import libs come from the same oracle the externs use.
    libs.extend(crate::asm_runtime::runtime_asm_import_libs()?);
    if needs_gc {
        libs.push(locate_runtime_staticlib()?.to_string_lossy().into_owned());
        for l in [
            "ws2_32.lib",
            "userenv.lib",
            "advapi32.lib",
            "bcrypt.lib",
            "ntdll.lib",
        ] {
            libs.push(l.to_string());
        }
    }
    let mut all: Vec<&Path> = objects.to_vec();
    all.push(&runtime_obj);
    let link = link_executable(&all, exe, &libs);
    let _ = std::fs::remove_file(&runtime_obj);
    link
}

/// Locate `locus_rt.lib` (the managed-heap runtime staticlib) next to the running
/// compiler — `target/<profile>/` for `locusc`, or one level up when invoked from
/// a test harness in `target/<profile>/deps/`.
fn locate_runtime_staticlib() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent().ok_or("compiler has no parent directory")?;
    for cand in [
        dir.join("locus_rt.lib"),
        dir.join("..").join("locus_rt.lib"),
    ] {
        if cand.exists() {
            // Guard the **stale staticlib** footgun. `locus-rt` is
            // `crate-type = ["staticlib","rlib"]`, but a *transitive dependency*
            // build (what `cargo test`/`cargo build -p locus-llvm` does) only
            // refreshes the **rlib** — only an explicit `cargo build -p locus-rt`
            // regenerates the **`.lib`**. So a `.lib` predating a runtime change
            // links against an old symbol set and the AOT link dies with a cryptic
            // `LNK2019` on whatever shim was added since (e.g. a new
            // `locus_string_from_utf16`). In a source tree, turn that into a clear,
            // actionable error instead of an afternoon of phantom-chasing.
            if let Some(stale) = runtime_staticlib_stale(&cand) {
                return Err(stale);
            }
            return Ok(cand);
        }
    }
    Err(
        "locus_rt.lib not found next to the compiler — build it with \
         `cargo build -p locus-rt`"
            .to_string(),
    )
}

/// `Some(message)` if `lib` is **older than the `locus-rt` sources** (so it would
/// miss runtime symbols added since it was built); else `None`. Only fires in a
/// source tree — a shipped compiler has no `locus-rt/src` to compare against, so
/// the guard no-ops. Conservative by design: it flags any source-newer-than-`.lib`
/// (even a comment edit) — the fix is one cheap `cargo build -p locus-rt`, which
/// beats a silent `LNK2019`.
fn runtime_staticlib_stale(lib: &Path) -> Option<String> {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("locus-rt")
        .join("src");
    if !src_dir.exists() {
        return None; // shipped compiler — nothing to compare against
    }
    let lib_mtime = lib.metadata().and_then(|m| m.modified()).ok()?;
    let newest_src = newest_rs_mtime(&src_dir)?;
    (newest_src > lib_mtime).then(|| {
        format!(
            "locus_rt.lib at {} is STALE — newer `locus-rt/src` exists. `cargo test` \
             refreshes locus-rt's rlib but NOT its staticlib (`.lib`), so the AOT link \
             would miss a runtime symbol added since the `.lib` was built (a cryptic \
             LNK2019). Rebuild it: `cargo build -p locus-rt`.",
            lib.display()
        )
    })
}

/// The newest modification time among `*.rs` files under `dir` (recursively), or
/// `None` if there are none / it can't be read.
fn newest_rs_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let t = if path.is_dir() {
            newest_rs_mtime(&path)
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            entry.metadata().and_then(|m| m.modified()).ok()
        } else {
            None
        };
        if let Some(t) = t {
            newest = Some(newest.map_or(t, |n| n.max(t)));
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir_of(src: &str) -> Ir {
        let src = src.to_string();
        std::thread::Builder::new()
            .name("aot-ir-of".into())
            .stack_size(locus::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let term = locus::program(&src).unwrap(); // grafts the prelude (e.g. `console_writeln`)
                let tree =
                    locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term).unwrap();
                locus::lower(&locus::stage_reduce(&tree).unwrap())
            })
            .expect("spawn AOT IR worker")
            .join()
            .expect("AOT IR worker panicked")
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn an_int_program_becomes_the_exit_code() {
        let exe = std::env::temp_dir().join(format!("locus_aot_int_{}.exe", std::process::id()));
        build_exe(&ir_of("42"), &exe, &[], false).expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "the program's i64 is the exit code"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn a_console_writeln_program_prints_from_the_shipped_exe() {
        // Output is the Locus `console_writeln` prelude over Win32 — no native console.
        let exe = std::env::temp_dir().join(format!("locus_aot_wln_{}.exe", std::process::id()));
        build_exe(
            &ir_of(r#"console_writeln "from the exe""#),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let out = std::process::Command::new(&exe).output().expect("run exe");
        assert!(out.status.success(), "exe ran");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("from the exe"),
            "the shipped exe should print; got {stdout:?}"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn an_allocating_program_builds_and_runs_with_the_real_gc() {
        // A tuple program built to a STANDALONE .exe, linked against the actual
        // collector (locus_rt.lib) — no stub. That it computes the right exit
        // code proves the managed heap (alloc + the field stores + the scope's
        // enter/leave_with) really ran in a shipped binary. Requires the runtime
        // staticlib: `cargo build -p locus-rt`.
        let exe = std::env::temp_dir().join(format!("locus_aot_gc_{}.exe", std::process::id()));
        build_exe(&ir_of("let (a, b) = (40, 2) in a + b"), &exe, &[], false).expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "tuple program ran through the real heap"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn a_scalar_ref_counter_exe_runs_under_o2() {
        // THE Sprint-1 AOT gate: the scalar `Ref[Int]` counter built to a STANDALONE
        // `.exe`, optimized at the `-O2` floor, linked against the real collector
        // (locus_rt.lib). `ref e` allocates a one-field heap cell, `r := !r + 41`
        // reads + writes it through the handle, `!r` reads it back — the exit code
        // 42 proves the managed-heap alloc + set_scalar/get_scalar round-trip
        // survives the shipped, optimized binary. (Seed `ref 1` for the honest
        // 1 + 41 = 42; the plan's literal `ref 0 ⇒ 42` is an off-by-one.) Requires
        // `cargo build -p locus-rt`.
        let exe = std::env::temp_dir().join(format!("locus_aot_ref_{}.exe", std::process::id()));
        build_exe(
            &ir_of("let r = ref 1 in let _ = (r := !r + 41) in !r"),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "the scalar Ref counter ran through the real heap under -O2"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn a_nested_tuple_exe_traces_through_the_real_heap() {
        // Nested data in a shipped exe: the inner tuple is a traced pointer cell
        // of the outer, so this only yields the right answer if the real
        // collector's set_ptr/get_ptr and the handle indirection all work AOT.
        let exe = std::env::temp_dir().join(format!("locus_aot_nest_{}.exe", std::process::id()));
        build_exe(
            &ir_of("let (p, c) = ((30, 5), 7) in let (a, b) = p in a + b + c"),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "nested tuple traced correctly in the exe"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn a_vector_field_in_an_object_round_trips_in_the_exe() {
        // SIMD Sprint 1, end to end under `-O2`: a `Quad[Float32]` (2 scalar
        // cells) stored in a tuple BETWEEN two scalar fields, then projected,
        // proves the multi-cell scalar store/load + cumulative offset survive the
        // shipped, optimized binary (the `set_scalar`/`get_scalar` pairs are
        // opaque, so the optimizer cannot fold the round-trip away).
        let exe = std::env::temp_dir().join(format!("locus_aot_vecfld_{}.exe", std::process::id()));
        build_exe(
            &ir_of(
                "let t = (40, Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0), 3) in \
                 let (a, q, b) = t in \
                 a + round (fromFloat32 (q.x) + fromFloat32 (q.w)) - b",
            ),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        // 40 + round(1.0 + 4.0) - 3 = 40 + 5 - 3 = 42.
        assert_eq!(
            status.code(),
            Some(42),
            "multi-cell vector field round-trips through the real heap under -O2"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn a_vector_array_round_trips_in_the_exe() {
        // An `Array[Quad[Float32]]` built, element-written, read back, and lane-
        // reduced — the array multi-cell payload (element `i` at `i*elem_bytes`)
        // works in the shipped exe with the real collector and `-O2`.
        let exe = std::env::temp_dir().join(format!("locus_aot_vecarr_{}.exe", std::process::id()));
        build_exe(
            &ir_of(
                "let a = [splatQuad (toFloat32 0.0), splatQuad (toFloat32 0.0)] in \
                 let _ = a[1] <- Quad(toFloat32 10.0, toFloat32 12.0, toFloat32 0.0, toFloat32 20.0) in \
                 let q = a[1] in \
                 round (fromFloat32 (q.x) + fromFloat32 (q.y) + fromFloat32 (q.w))",
            ),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "Array[Quad[Float32]] round-trips through the real heap under -O2"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn a_vector_array_load_add_store_kernel_runs_in_the_exe() {
        // SIMD Sprint 2, end to end under `-O2`: the kernel primitive in a shipped
        // binary. Two `Array[Float32]` inputs, a `loop` strided by 4 that
        // `loadQuad`s a chunk from each, packed-adds, and `storeQuad`s into an
        // output array; then the output elements are read and reduced to the exit
        // code. Length 8 = 2*4 lanes (an exact multiple — tail handling is out of
        // scope). The packed load/op/store survive optimization (the GC payload
        // accessors are opaque, so the round-trip cannot fold away), so a correct
        // exit code proves a real bounds-checked SIMD load/add/store ran AOT.
        let exe =
            std::env::temp_dir().join(format!("locus_aot_veckern_{}.exe", std::process::id()));
        build_exe(
            &ir_of(
                "let a = [toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0, \
                          toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0] in \
                 let b = [toFloat32 10.0, toFloat32 20.0, toFloat32 30.0, toFloat32 40.0, \
                          toFloat32 50.0, toFloat32 60.0, toFloat32 70.0, toFloat32 80.0] in \
                 let out = [toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, \
                            toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0] in \
                 let _ = (loop i = 0, acc = 0 while i < len out \
                          do i + 4, \
                             (let _ = storeQuad(out, i, loadQuad(a, i) + loadQuad(b, i)) in acc) \
                          else acc) in \
                 round (fromFloat32 (out[0])) - 11 + round (fromFloat32 (out[7])) - 88 + 42",
            ),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        // out[0]=11, out[7]=88 → (11-11) + (88-88) + 42 = 42.
        assert_eq!(
            status.code(),
            Some(42),
            "the SIMD load/add/store kernel computes the elementwise result AOT under -O2"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn a_function_returning_a_quad_has_its_result_used_in_the_exe() {
        // SIMD Sprint 3, Part B, end to end under `-O2`: a named `fn (…) => <Quad>`
        // returns a vector across the closure ABI, the caller `storeQuad`s that result
        // into an output array, then reads the lanes back and reduces to the exit code.
        // The packed store round-trips through the opaque GC payload accessors, so the
        // optimizer cannot fold it away — a correct exit code proves the vector result
        // crossed the ABI and landed in memory in a shipped binary.
        let exe = std::env::temp_dir().join(format!("locus_aot_vecret_{}.exe", std::process::id()));
        build_exe(
            &ir_of(
                "let make = fn n: Int => \
                     Quad(toFloat32 10.0, toFloat32 11.0, toFloat32 12.0, toFloat32 13.0) in \
                 let out = [toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0] in \
                 let _ = storeQuad(out, 0, make 0) in \
                 round (fromFloat32 (out[0])) + round (fromFloat32 (out[3])) + 19",
            ),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        // make ⇒ (10,11,12,13); out[0]=10, out[3]=13 → 10 + 13 + 19 = 42.
        assert_eq!(
            status.code(),
            Some(42),
            "a Quad function result is stored and read back correctly AOT under -O2"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn always_gc_links_the_runtime_for_a_nonallocating_program() {
        // `--always-gc` (the last arg) forces the real collector to be linked
        // even though `42` allocates nothing. The exe is fat but behaves
        // identically — proving the override reaches the link without breaking a
        // program that has no handles to manage.
        let exe = std::env::temp_dir().join(format!("locus_aot_always_{}.exe", std::process::id()));
        build_exe(&ir_of("42"), &exe, &[], true).expect("build with --always-gc");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "forced-gc exe still runs correctly"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn calls_an_embedded_layer0_asm_symbol() {
        // The Locus → Layer-0 asm pipe (D5): an `extern asm` call resolves to a
        // hand-written `.masm` primitive (`locus_asm_hello: mov rax,42; ret`),
        // assembled by the vendored JASM Assembler (`locus-asm`) and embedded in
        // this program's COFF as module-level inline asm. The .exe returning 42
        // proves assemble → embed → link → call works end to end. (build_exe
        // bypasses the CLI mint-gate, which `guard_layer2` enforces separately.)
        let exe = std::env::temp_dir().join(format!("locus_aot_asm_{}.exe", std::process::id()));
        build_exe(
            &ir_of("let go = extern asm \"locus_asm_hello\" : Int -> Int in go 0"),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(status.code(), Some(42), "the embedded Layer-0 asm ran");
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn calls_useful_layer0_bit_primitives() {
        // Useful regime-A asm: rotate-left, popcount, and a byteswap round-trip —
        // bit ops Locus has no operator for, so they are genuine Layer-0 value.
        // `rotl 1 4` = 16, `popcount 255` = 8, `bswap (bswap 18)` = 18 → 42, only
        // if each resolves to its hand-written `.masm` with the correct Win64 ABI.
        let exe = std::env::temp_dir().join(format!("locus_aot_bits_{}.exe", std::process::id()));
        build_exe(
            &ir_of(
                "let rotl = extern asm \"locus_asm_rotl64\" : Int -> Int -> Int in \
                 let popcount = extern asm \"locus_asm_popcount64\" : Int -> Int in \
                 let bswap = extern asm \"locus_asm_bswap64\" : Int -> Int in \
                 (rotl 1 4) + (popcount 255) + (bswap (bswap 18))",
            ),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "rotl(16) + popcount(8) + bswap-roundtrip(18)"
        );
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn the_asm_mandel_kernel_is_correct() {
        // (0,0) is inside the set → runs to `max`; (2,2) escapes at iteration 1.
        // This also validates the mixed FP/int extern-asm Win64 ABI: two doubles
        // (cx, cy) in xmm0/xmm1, the int `max` in r8, result in rax.
        let chk = |src: &str, want: i32| {
            let exe = std::env::temp_dir()
                .join(format!("locus_mandel_{want}_{}.exe", std::process::id()));
            build_exe(&ir_of(src), &exe, &[], false).expect("build exe");
            let code = std::process::Command::new(&exe).status().unwrap().code();
            let _ = std::fs::remove_file(&exe);
            assert_eq!(code, Some(want), "mandel src: {src}");
        };
        let m = "let m = extern asm \"locus_asm_mandel\" : Float -> Float -> Int -> Int in ";
        chk(&format!("{m} m 0.0 0.0 100"), 100);
        chk(&format!("{m} m 2.0 2.0 100"), 1);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn the_simd_mandel2_kernel_matches_the_scalar_counts() {
        // The 2-lane packed-double kernel (A5 perf probe) must return the SUM of
        // the two lanes' scalar escape counts — proving the per-lane mask freezes
        // each lane independently. cx0,cx1 share cy (horizontal neighbours):
        //   (0,0)+(0,0) → 100+100 = 200   both interior, run to max
        //   (2,2)+(2,2) →   1+  1 =   2    both escape immediately
        //   (0,0)+(2,0) → 100+  2 = 102    one interior, one escapes at iter 2
        let chk = |src: &str, want: i32| {
            let exe = std::env::temp_dir()
                .join(format!("locus_mandel2_{want}_{}.exe", std::process::id()));
            build_exe(&ir_of(src), &exe, &[], false).expect("build exe");
            let code = std::process::Command::new(&exe).status().unwrap().code();
            let _ = std::fs::remove_file(&exe);
            assert_eq!(code, Some(want), "mandel2 src: {src}");
        };
        let m =
            "let m = extern asm \"locus_asm_mandel2\" : Float -> Float -> Float -> Int -> Int in ";
        chk(&format!("{m} m 0.0 0.0 0.0 100"), 200);
        chk(&format!("{m} m 2.0 2.0 2.0 100"), 2);
        chk(&format!("{m} m 0.0 2.0 0.0 100"), 102);
    }

    /// **A5 perf measurement (honest, not propaganda).** Time the 2-lane SIMD
    /// kernel against the scalar kernel over the *identical* points — both reached
    /// as `extern asm`, so this isolates 2-lane-packed vs 1-lane, with the scalar
    /// asm standing in for "the good compiler" (the earlier experiment showed
    /// scalar asm ties LLVM, so the ratio transfers). Points are interior
    /// neighbours on the real axis (cx ∈ [-0.5, -0.4], cy = 0) → both lanes run to
    /// `max`, the balanced case where SIMD *can* win. `cx` drifts per rep to defeat
    /// hoisting; `acc` is returned so nothing is dead-code-eliminated. `#[ignore]`d
    /// (it runs for seconds); invoke with `--ignored --nocapture`.
    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    #[ignore]
    fn bench_simd_vs_scalar_mandel_throughput() {
        let reps = "1000000";
        let simd_src = format!(
            "let m2 = extern asm \"locus_asm_mandel2\" : Float -> Float -> Float -> Int -> Int in \
             loop i = 0, cx = (0.0 - 0.5), acc = 0 \
             while i < {reps} \
             do (i + 1), (cx + 0.0000001), (acc + (m2 cx (cx + 0.01) 0.0 500)) \
             else acc"
        );
        let scalar_src = format!(
            "let m = extern asm \"locus_asm_mandel\" : Float -> Float -> Int -> Int in \
             loop i = 0, cx = (0.0 - 0.5), acc = 0 \
             while i < {reps} \
             do (i + 1), (cx + 0.0000001), (acc + (m cx 0.0 500) + (m (cx + 0.01) 0.0 500)) \
             else acc"
        );

        let build = |tag: &str, src: &str| {
            let exe =
                std::env::temp_dir().join(format!("locus_bench_{tag}_{}.exe", std::process::id()));
            build_exe(&ir_of(src), &exe, &[], false).expect("build bench exe");
            exe
        };
        let best_ms = |exe: &std::path::Path| {
            let mut best = u128::MAX;
            for _ in 0..4 {
                let t = std::time::Instant::now();
                let st = std::process::Command::new(exe).status().expect("run bench");
                let ms = t.elapsed().as_millis();
                assert!(st.code().is_some(), "bench exe ran");
                best = best.min(ms);
            }
            best
        };

        let simd_exe = build("simd", &simd_src);
        let scalar_exe = build("scalar", &scalar_src);
        let simd_ms = best_ms(&simd_exe);
        let scalar_ms = best_ms(&scalar_exe);
        let _ = std::fs::remove_file(&simd_exe);
        let _ = std::fs::remove_file(&scalar_exe);

        println!(
            "\n=== A5 SIMD perf probe ({reps} reps × 2 interior points × 500 max-iters) ===\n\
             scalar asm (1 lane, 2 calls/rep): {scalar_ms} ms (best of 4)\n\
             SIMD   asm (2 lanes, 1 call/rep): {simd_ms} ms (best of 4)\n\
             speedup: {:.2}x\n",
            scalar_ms as f64 / simd_ms as f64
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn an_asm_stub_calls_win32_through_the_oracle() {
        // A4 (the owner's constraint): a Layer-0 `.masm` stub invokes Win32
        // (`GetStdHandle(STD_OUTPUT_HANDLE)`) and must return EXACTLY what the
        // language's own FFI returns for the same call — because both reach Win32
        // through the single `locus-winapi` oracle + the one resolver, never a
        // second copy of the API data. The program subtracts the two; a 0 exit code
        // proves the asm stub and the Locus `extern` agree on the live handle. (The
        // asm's `call GetStdHandle` resolves at link via the oracle-derived
        // kernel32 import lib wired into `build_exe`.)
        let exe = std::env::temp_dir().join(format!("locus_aot_win32_{}.exe", std::process::id()));
        build_exe(
            &ir_of(
                "let from_asm = extern asm \"locus_asm_getstdout\" : Int -> Int in \
                 let from_lang = extern \"GetStdHandle\" : Int -> Int in \
                 (from_asm 0) - (from_lang (0 - 11))",
            ),
            &exe,
            &[],
            false,
        )
        .expect("build exe");
        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(0),
            "the asm stub's Win32 handle equals the language's FFI handle"
        );
        let _ = std::fs::remove_file(&exe);
    }

    // ── Sprint 3: separate compilation — link two modules into one .exe ──────

    /// Build a producer module's `(interface, exported flat functions)` from its
    /// source — the producer side of the link. Reuses the normal stdlib graft so
    /// the body type-checks; `exported_functions` uncurries each exposed function
    /// into a flat uniform-ABI [`crate::lower::LibExport`] the backend emits.
    fn producer_exports(src: &str) -> (locus::ModuleInterface, Vec<crate::lower::LibExport>) {
        let src = src.to_string();
        std::thread::Builder::new()
            .name("sepcomp-producer".into())
            .stack_size(locus::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let prog = locus::parse_program(&src).expect("producer parses");
                let module = prog.modules[0].clone();
                let grafted = locus::program(&src).expect("producer grafts + parses");
                let iface = locus::interface_of(&module, &grafted).expect("producer interface");
                let fns = locus::exported_functions(&module, &grafted).expect("producer exports");
                // Lower each exported function's body to a flat-ABI LibExport. No
                // `__locus_main`, no closure env — just the uncurried params + body.
                let exports = fns
                    .into_iter()
                    .map(|f| {
                        let body = locus::stage_reduce(&f.body).expect("stage-reduce export body");
                        crate::lower::LibExport {
                            symbol: f.mangled.clone(),
                            params: f.params.clone(),
                            ret_ty: f.ret_ty.clone(),
                            body: locus::lower_function_body(&body, &[]),
                        }
                    })
                    .collect();
                (iface, exports)
            })
            .expect("spawn producer worker")
            .join()
            .expect("producer worker panicked")
    }

    /// Build the client object's IR **against the producer interface only** — the
    /// client never sees the producer body. `check_client_against_with_imports`
    /// type-checks the client and returns the cross-module call table; the IR is
    /// lowered with that table seeded, so the call to the imported function
    /// collapses to one `Comp::Foreign` direct external call to the mangled symbol.
    fn client_ir(client_src: &str, iface: locus::ModuleInterface) -> Ir {
        let client_src = client_src.to_string();
        std::thread::Builder::new()
            .name("sepcomp-client".into())
            .stack_size(locus::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let loaded = locus::LoadedInterface::accept(iface).expect("ABI ok");
                let module_name = loaded.interface().name.clone();
                let (typed, imports) = locus::check_client_against_with_imports(
                    &client_src,
                    &[locus::Import::all(module_name)],
                    &[loaded],
                )
                .expect("client type-checks against the interface alone");
                // Seed the cross-module externs: name → (mangled symbol, uniform ABI).
                let extern_table: Vec<(String, String, locus::ExternAbi)> = imports
                    .iter()
                    .map(|i| {
                        (
                            i.name.clone(),
                            i.symbol.clone(),
                            locus::ExternAbi {
                                params: vec![locus::Width::W64; i.arity],
                                ret: locus::Width::W64,
                            },
                        )
                    })
                    .collect();
                let typed = locus::stage_reduce(&typed).expect("stage-reduce client");
                locus::lower_with_imports(&typed, &extern_table)
            })
            .expect("spawn client worker")
            .join()
            .expect("client worker panicked")
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn separately_compiled_lib_and_app_link_and_run() {
        // THE Sprint-3 gate. A producer `Data.Math` exports a first-order
        // `add3 : Int -> Int -> Int = fn a => fn b => a + b`; a client `App`
        // imports it and computes `add3 40 2`. The two are compiled to SEPARATE
        // objects on disk (separate `emit` calls, the client built ONLY from its
        // own tree + the producer's interface — never the producer body), then
        // linked into one `.exe` that must exit 42.
        let producer_src = "module Data.Math at services exposing (add3) =\n\
                            let add3 = fn a: Int => fn b: Int => a + b in\n\
                            ()\n\
                            ()";
        let (iface, exports) = producer_exports(producer_src);
        // The producer exported exactly the flat symbol the mangling defines.
        assert_eq!(exports.len(), 1, "one exported function");
        assert_eq!(exports[0].symbol, locus::mangle_export("Data.Math", "add3"));
        assert_eq!(exports[0].params.len(), 2, "add3 is binary (uncurried)");

        // The client is built from its OWN source + the producer INTERFACE only —
        // this is genuine separate compilation, not a graft of both modules. The
        // client source never mentions `add3`'s body (`a + b`); it only calls it.
        let client_src = "import Data.Math\nadd3 40 2";
        assert!(
            !client_src.contains("a + b") && !client_src.contains("fn a"),
            "the client source must NOT contain the producer body — separate compilation"
        );
        let app_ir = client_ir(client_src, iface);
        // Non-fakery proof at the IR level: the client lowered the imported call to
        // ONE direct external `Comp::Foreign` to the mangled symbol — it did not
        // inline or graft the producer body (there is no `a + b` add in this IR).
        assert!(
            ir_calls_foreign(&app_ir, &locus::mangle_export("Data.Math", "add3")),
            "client IR must call the producer symbol as one Foreign external call:\n{app_ir:?}"
        );

        // Emit two SEPARATE objects on disk.
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let lib_o = dir.join(format!("locus_sepcomp_lib_{pid}.obj"));
        let app_o = dir.join(format!("locus_sepcomp_app_{pid}.obj"));
        let exe = dir.join(format!("locus_sepcomp_{pid}.exe"));
        emit_library_object(&exports, &lib_o, false).expect("emit Data.Math.o");
        emit_object(&app_ir, &app_o, false).expect("emit App.o");
        assert!(lib_o.exists() && app_o.exists(), "two objects on disk");

        // Link App.o + Lib.o + the shared C runtime → an exe. Neither side
        // allocates, so no GC runtime is needed.
        let needs_gc = crate::lower::block_performs_gc(&app_ir)
            || exports
                .iter()
                .any(|e| crate::lower::block_performs_gc(&e.body));
        link_program(&[&app_o, &lib_o], &exe, &[], needs_gc).expect("link 2-module program");

        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "the separately-compiled 2-module program computes add3 40 2 = 42"
        );
        let _ = std::fs::remove_file(&lib_o);
        let _ = std::fs::remove_file(&app_o);
        let _ = std::fs::remove_file(&exe);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn separately_compiled_lib_allocates_over_the_shared_gc() {
        // The optional GC variant (Sprint 3): a producer function whose body
        // ALLOCATES (it builds a tuple on the managed heap, then projects it),
        // returning a scalar. Compiled separately, linked with the SAME
        // `locus_rt.lib` collector the client links, and run — proving the
        // producer's body uses the one shared GC across the link. Requires the
        // runtime staticlib (`cargo build -p locus-rt`); the existing gc exe tests
        // share that prerequisite.
        let producer_src = "module Data.Math at services exposing (sumPair) =\n\
                            let sumPair = fn a: Int => fn b: Int => \
                              (let p = (a, b) in let (x, y) = p in x + y) in\n\
                            ()\n\
                            ()";
        let (iface, exports) = producer_exports(producer_src);
        assert_eq!(exports.len(), 1);
        // The producer body performs `gc` — it allocates the pair.
        assert!(
            crate::lower::block_performs_gc(&exports[0].body),
            "the producer body should allocate (perform gc)"
        );

        let client_src = "import Data.Math\nsumPair 40 2";
        let app_ir = client_ir(client_src, iface);
        assert!(ir_calls_foreign(
            &app_ir,
            &locus::mangle_export("Data.Math", "sumPair")
        ));

        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let lib_o = dir.join(format!("locus_sepcomp_gc_lib_{pid}.obj"));
        let app_o = dir.join(format!("locus_sepcomp_gc_app_{pid}.obj"));
        let exe = dir.join(format!("locus_sepcomp_gc_{pid}.exe"));
        // The producer object allocates, so emit it with the managed-heap path on.
        emit_library_object(&exports, &lib_o, false).expect("emit Data.Math.o (gc)");
        emit_object(&app_ir, &app_o, false).expect("emit App.o (gc)");

        // Whole-program GC decision: the producer allocates, so the shared
        // collector is linked even though the client itself does not allocate.
        let needs_gc = crate::lower::block_performs_gc(&app_ir)
            || exports
                .iter()
                .any(|e| crate::lower::block_performs_gc(&e.body));
        assert!(needs_gc, "the linked program needs the shared GC");
        link_program(&[&app_o, &lib_o], &exe, &[], needs_gc).expect("link 2-module gc program");

        let status = std::process::Command::new(&exe).status().expect("run exe");
        assert_eq!(
            status.code(),
            Some(42),
            "the producer allocated a pair over the shared GC and returned 42"
        );
        let _ = std::fs::remove_file(&lib_o);
        let _ = std::fs::remove_file(&app_o);
        let _ = std::fs::remove_file(&exe);
    }

    /// Does the IR contain a `Comp::Foreign` call to `sym`? — the proof the client
    /// lowered an imported call to a direct external call, not an inlined body.
    fn ir_calls_foreign(ir: &Ir, sym: &str) -> bool {
        use locus::{Comp, Ir as I};
        fn comp_has(comp: &Comp, sym: &str) -> bool {
            match comp {
                Comp::Foreign(s, _, _) => s == sym,
                Comp::If(_, then, els) => ir_has(then, sym) || ir_has(els, sym),
                Comp::Loop {
                    cond,
                    steps,
                    result,
                    ..
                } => {
                    ir_has(cond, sym) || steps.iter().any(|s| ir_has(s, sym)) || ir_has(result, sym)
                }
                _ => false,
            }
        }
        fn ir_has(ir: &I, sym: &str) -> bool {
            match ir {
                I::Block { binds, comp, .. } => {
                    binds.iter().any(|b| comp_has(&b.comp, sym)) || comp_has(comp, sym)
                }
                I::Let { comp, rest, .. } => comp_has(comp, sym) || ir_has(rest, sym),
                I::Ret { comp, .. } => comp_has(comp, sym),
            }
        }
        ir_has(ir, sym)
    }
}
