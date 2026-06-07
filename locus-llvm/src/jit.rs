//! ORCv2 LLJIT execution. inkwell builds the module; **llvm-sys** drives ORC.
//!
//! **Codegen v1** uses LLJIT's *default* object-linking layer — enough to JIT
//! and run the pure fragment. NewBF's SEH-aware RTDyld memory manager
//! (`RtlAddFunctionTable` registration for `uwtable=2` unwind info) is only
//! needed once exceptions must unwind through JIT'd frames — that arrives with
//! handlers. The one non-obvious move is the **context donation** (LLVM 22
//! dropped the API to read a context back out of a `ThreadSafeContext`, so we
//! transfer ownership of the inkwell context with `…FromLLVMContext`).

use std::ffi::{CStr, CString};
use std::sync::Once;

use inkwell::context::Context;
use inkwell::targets::{InitializationConfig, Target};
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

use locus::Ir;

use crate::lower::emit_module;

/// Register the host target + asm printer once (required before LLJIT can
/// detect the host machine).
fn init_target() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        Target::initialize_native(&InitializationConfig::default())
            .expect("LLVM native target init failed");
    });
}

/// Consume an `LLVMErrorRef` into an owned message.
fn take_error(err: LLVMErrorRef) -> String {
    unsafe {
        let cmsg = LLVMGetErrorMessage(err);
        let s = CStr::from_ptr(cmsg).to_string_lossy().into_owned();
        LLVMDisposeErrorMessage(cmsg);
        s
    }
}

// kernel32's loader — always available to a Windows process. Used to build the
// JIT's own import table: load each demanded DLL and resolve each symbol's
// address (so it works even for DLLs the process hasn't loaded yet).
extern "system" {
    fn LoadLibraryA(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn GetProcAddress(
        module: *mut std::ffi::c_void,
        name: *const std::os::raw::c_char,
    ) -> *mut std::ffi::c_void;
}

/// Resolve each demanded Win32 API to its real address — `LoadLibrary` the DLL,
/// then `GetProcAddress` the function. These `(symbol, address)` pairs become
/// absolute symbols below: the JIT's hand-built import table, which — unlike the
/// process-search generator — reaches DLLs the process hasn't loaded yet (user32…).
fn resolve_win32_apis(
    apis: &crate::winapi_resolve::Demanded,
) -> Result<Vec<(String, u64)>, String> {
    let mut out = Vec::with_capacity(apis.len());
    for (sym, dll) in apis {
        let cdll = CString::new(dll.as_str()).map_err(|_| format!("bad DLL name `{dll}`"))?;
        let csym = CString::new(sym.as_str()).map_err(|_| format!("bad symbol name `{sym}`"))?;
        let handle = unsafe { LoadLibraryA(cdll.as_ptr()) };
        if handle.is_null() {
            return Err(format!(
                "LoadLibrary(\"{dll}\") failed (needed for `{sym}`)"
            ));
        }
        let addr = unsafe { GetProcAddress(handle, csym.as_ptr()) };
        if addr.is_null() {
            return Err(format!("GetProcAddress(`{sym}` in {dll}) failed"));
        }
        out.push((sym.clone(), addr as usize as u64));
    }
    Ok(out)
}

/// Register `(name, address)` pairs as **absolute symbols** in `jd`, so a JIT'd
/// `call @name` binds to that address. Used for the prelowered runtime
/// ([`crate::runtime`]) and the resolved Win32 APIs — symbols the process-search
/// generator wouldn't find (exe-internal, or not-yet-loaded DLLs).
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
        let cname = CString::new(name.as_str()).unwrap();
        // Intern transfers a +1 ref that AbsoluteSymbols takes ownership of.
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

/// Dedupe `(name, addr)` pairs keeping the **last** occurrence of each name
/// (first-seen order otherwise). Host-injected `extra` symbols are appended
/// last, so last-wins lets them override a same-named runtime/Win32 default —
/// and a duplicate name never reaches `LLVMOrcJITDylibDefine`, which rejects it.
fn dedupe_last_wins(symbols: Vec<(String, u64)>) -> Vec<(String, u64)> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(String, u64)> = Vec::with_capacity(symbols.len());
    for pair in symbols.into_iter().rev() {
        if seen.insert(pair.0.clone()) {
            out.push(pair);
        }
    }
    out.reverse();
    out
}

/// Lower `ir` to LLVM, JIT it, run `__locus_main`, and return its `i64` result.
///
/// (v1: the program must be in the pure fragment — see [`crate::lower`].)
///
/// This is the Phase-1 entry point; it delegates to
/// [`jit_run_i64_with_symbols`] with no extra symbols, so its behavior is
/// unchanged. The IDE host uses the with-symbols variant to inject the `igui_*`
/// C-ABI addresses a graphical program resolves.
pub fn jit_run_i64(ir: &Ir, apis: &crate::winapi_resolve::Demanded) -> Result<i64, String> {
    jit_run_i64_with_symbols(ir, apis, &[])
}

/// Like [`jit_run_i64`], but also registers `extra` as **additional absolute
/// symbols** in the JIT dylib — alongside the prelowered runtime
/// ([`crate::runtime::runtime_symbols`]) and the resolved Win32 apis — so a
/// JIT'd `call @name` binds to a host-provided address for each `(name, addr)`
/// in `extra`.
///
/// This is the **one engine seam** the Locus IDE needs: the host (`locus-ide`)
/// passes the `igui_*` GUI C-ABI's `(export-name, function-pointer-as-u64)`
/// table here, so a graphical Locus program's `extern "iGui.…"` externs resolve
/// to the linked shell. The `extra` symbols are defined in the *same* absolute-
/// symbol manifold as the runtime/winapi ones (reusing [`define_absolute_symbols`]).
///
/// `extra` is appended **last** and the table is deduped **last-wins**, so a host
/// symbol may deliberately *override* a same-named runtime/Win32 default. The IDE
/// uses this for more than the disjoint `iGui.*` names: it redirects
/// `WriteConsoleW`/`ReadConsoleW` to its own console-pane shims, so a program's
/// console I/O lands in the IDE pane instead of the (absent) OS console — the
/// program still resolves the same *name*; only the address differs. Passing
/// `&[]` makes this identical to the historical `jit_run_i64`.
pub fn jit_run_i64_with_symbols(
    ir: &Ir,
    apis: &crate::winapi_resolve::Demanded,
    extra: &[(String, u64)],
) -> Result<i64, String> {
    init_target();

    // Build with inkwell, then transfer ownership of the module + its context
    // to ORC (LLVM 22 context-donation dance).
    let ctx = Context::create();
    // The JIT always links the runtime (all shims are registered), and a JIT run
    // is ephemeral, so there's nothing to force — gc scopes follow the program's
    // own effect.
    let module = emit_module(&ctx, ir, false)?;
    // Optimize the IR (-O2 floor) before donating the module to ORC, so `locusc
    // run` and `locusc build` optimize identically — LLJIT's default layer does not
    // run the pipeline itself. Needs the target's triple + data layout set first so
    // the passes cost-model for the host.
    let (tm, triple) = crate::aot::host_target_machine()?;
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    crate::aot::run_opt_pipeline(&module, &tm)?;
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

    // Resolve symbols from the process's already-loaded DLLs (kernel32, … are
    // always loaded) — this is what lets JIT'd code call the Win32 API by name,
    // no per-symbol registration. Non-fatal if it fails: module-internal and
    // absolute symbols still resolve.
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

    // The JIT's import table: the prelowered runtime fns + the demanded Win32
    // APIs (LoadLibrary'd + GetProcAddress'd), all as absolute symbols.
    let mut symbols: Vec<(String, u64)> = crate::runtime::runtime_symbols()
        .into_iter()
        .map(|(n, a)| (n.to_string(), a))
        .collect();
    match resolve_win32_apis(apis) {
        Ok(win32) => symbols.extend(win32),
        Err(e) => {
            unsafe { LLVMOrcDisposeLLJIT(jit) };
            return Err(e);
        }
    }
    // Host-injected extras (the IDE's `igui_*` GUI C-ABI + its console-pane
    // `WriteConsoleW`/`ReadConsoleW` overrides). Appended last, then deduped
    // last-wins so an extra may override a same-named default. With the default
    // `&[]` this is a no-op and the table is exactly the historical one.
    symbols.extend(extra.iter().cloned());
    let symbols = dedupe_last_wins(symbols);
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
