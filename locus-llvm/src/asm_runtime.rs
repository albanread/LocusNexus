//! **Layer-0 runtime asm primitives** (ruling D5, [`docs/jasm-boundary-layer.md`]).
//!
//! Hand-written `.masm` units, assembled by the vendored [`locus_asm`] Assembler
//! (text → LLVM-MC Intel asm) and embedded as **module-level inline asm in the AOT
//! object**, so their `.globl` symbols are callable from Locus via `extern asm`.
//! The symbols co-resolve with the program's `call`s at link (`link.exe`) — no
//! engine friction (§3). AOT-only: the dev JIT (ORC) cannot see module inline asm,
//! so the dev-twin registers addresses as absolute symbols (deferred, §3/§6).
//!
//! This is the **shipped** asm floor: a program that calls a runtime asm primitive
//! gets the hand-written machine code linked into its `.exe`. Win32 from a stub
//! (sprint A4) takes its signature from `locus-winapi` (the single oracle) and its
//! address from the existing resolver — never a second copy of the API data.

use inkwell::module::Module;
use locus_asm::Assembler;

/// The bundled runtime `.masm`. It grows with the sprints (A3 regime-A primitives,
/// A5 SIMD kernels); for now it carries the A1 bring-up `hello_asm` plus a couple
/// of genuinely useful **regime-A** (collection-free, pointer-blind) bit
/// primitives demonstrated as `extern asm`.
pub const RUNTIME_MASM: &str = "\
.intel_syntax noprefix\n\
.text\n\
\n\
; A1 bring-up: prove the Locus → Layer-0 asm pipe (returns 42).\n\
.globl locus_asm_hello\n\
locus_asm_hello:\n\
    mov rax, 42\n\
    ret\n\
\n\
; ── Useful regime-A bit primitives (collection-free, pointer-blind leaves) ──\n\
; Win64 ABI: integer args in rcx, rdx, …; result in rax. These have no Locus\n\
; surface (no rotate / popcount / byteswap operator), so they are genuine\n\
; Layer-0 value, and being pure leaves they need no GC handle discipline.\n\
\n\
; rotl64(x: rcx, k: rdx) -> rax = x rotated left by (k mod 64).\n\
.globl locus_asm_rotl64\n\
locus_asm_rotl64:\n\
    mov rax, rcx\n\
    mov rcx, rdx\n\
    rol rax, cl\n\
    ret\n\
\n\
; rotr64(x: rcx, k: rdx) -> rax = x rotated right by (k mod 64).\n\
.globl locus_asm_rotr64\n\
locus_asm_rotr64:\n\
    mov rax, rcx\n\
    mov rcx, rdx\n\
    ror rax, cl\n\
    ret\n\
\n\
; popcount64(x: rcx) -> rax = number of set bits in x.\n\
.globl locus_asm_popcount64\n\
locus_asm_popcount64:\n\
    popcnt rax, rcx\n\
    ret\n\
\n\
; bswap64(x: rcx) -> rax = x with its 8 bytes reversed.\n\
.globl locus_asm_bswap64\n\
locus_asm_bswap64:\n\
    mov rax, rcx\n\
    bswap rax\n\
    ret\n\
\n\
; ── Mandelbrot escape-iteration inner loop (scalar SSE2, regime-A) ──\n\
; mandel(cx: xmm0, cy: xmm1, max: r8) -> rax = iterations until |z|^2 > 4.\n\
; z = (zx,zy) from 0; zx,zy <- zx^2-zy^2+cx, 2*zx*zy+cy. The Win64-ABI mixed\n\
; FP/int boundary the experiment exercises (two doubles + one int in). Saves the\n\
; callee-saved xmm6/xmm7 it uses.\n\
.globl locus_asm_mandel\n\
locus_asm_mandel:\n\
    sub rsp, 32\n\
    movdqu [rsp], xmm6\n\
    movdqu [rsp+16], xmm7\n\
    mov r10, 0x4010000000000000\n\
    movq xmm6, r10\n\
    xorpd xmm2, xmm2\n\
    xorpd xmm3, xmm3\n\
    xor rax, rax\n\
Lmandel_loop:\n\
    cmp rax, r8\n\
    jge Lmandel_done\n\
    movapd xmm4, xmm2\n\
    mulsd xmm4, xmm2\n\
    movapd xmm5, xmm3\n\
    mulsd xmm5, xmm3\n\
    movapd xmm7, xmm4\n\
    addsd xmm7, xmm5\n\
    ucomisd xmm7, xmm6\n\
    ja Lmandel_done\n\
    movapd xmm7, xmm2\n\
    mulsd xmm7, xmm3\n\
    addsd xmm7, xmm7\n\
    addsd xmm7, xmm1\n\
    subsd xmm4, xmm5\n\
    addsd xmm4, xmm0\n\
    movapd xmm2, xmm4\n\
    movapd xmm3, xmm7\n\
    inc rax\n\
    jmp Lmandel_loop\n\
Lmandel_done:\n\
    movdqu xmm6, [rsp]\n\
    movdqu xmm7, [rsp+16]\n\
    add rsp, 32\n\
    ret\n\
\n\
; ── A4: Win32 from a Layer-0 stub, via the SINGLE oracle ──────────────────\n\
; getstdout() -> rax = GetStdHandle(STD_OUTPUT_HANDLE). The owner's constraint:\n\
; the asm provides only the `call` site; the symbol's ABI + DLL come from\n\
; `locus-winapi` (validated by `runtime_asm_import_libs`), never a second copy.\n\
; Win64 prologue: entry rsp == 8 (mod 16), so `sub 40` (32 shadow + 8) realigns\n\
; to 0 before the call. The dummy Locus arg arrives in rcx and is overwritten.\n\
.globl locus_asm_getstdout\n\
locus_asm_getstdout:\n\
    sub rsp, 40\n\
    mov rcx, -11\n\
    call GetStdHandle\n\
    add rsp, 40\n\
    ret\n\
\n\
; ── A5 perf probe: 2-lane SIMD Mandelbrot (packed double) ─────────────────\n\
; mandel2(cx0: xmm0, cx1: xmm1, cy: xmm2, max: r9) -> rax = count0 + count1.\n\
; Two horizontal neighbours (shared cy) iterated together in packed doubles. A\n\
; per-lane escape mask (cmplepd) freezes a lane's count once |z|^2 > 4; the loop\n\
; ends when BOTH lanes have escaped or `max` is hit. This is the data-dependent\n\
; shape the LLVM autovectorizer won't touch — the honest test of whether hand\n\
; SIMD beats the scalar compiler. Saves the callee-saved xmm6/7/8 it scratches.\n\
.globl locus_asm_mandel2\n\
locus_asm_mandel2:\n\
    sub rsp, 56\n\
    movdqu [rsp], xmm6\n\
    movdqu [rsp+16], xmm7\n\
    movdqu [rsp+32], xmm8\n\
    unpcklpd xmm0, xmm1\n\
    unpcklpd xmm2, xmm2\n\
    xorpd xmm3, xmm3\n\
    xorpd xmm4, xmm4\n\
    xorpd xmm1, xmm1\n\
    mov r10, 0x4010000000000000\n\
    movq xmm5, r10\n\
    unpcklpd xmm5, xmm5\n\
    xor eax, eax\n\
Lm2_loop:\n\
    cmp rax, r9\n\
    jge Lm2_done\n\
    movapd xmm6, xmm3\n\
    mulpd xmm6, xmm6\n\
    movapd xmm7, xmm4\n\
    mulpd xmm7, xmm7\n\
    movapd xmm8, xmm6\n\
    addpd xmm8, xmm7\n\
    cmplepd xmm8, xmm5\n\
    movmskpd edx, xmm8\n\
    test edx, edx\n\
    jz Lm2_done\n\
    psubq xmm1, xmm8\n\
    movapd xmm8, xmm3\n\
    mulpd xmm8, xmm4\n\
    addpd xmm8, xmm8\n\
    addpd xmm8, xmm2\n\
    subpd xmm6, xmm7\n\
    addpd xmm6, xmm0\n\
    movapd xmm3, xmm6\n\
    movapd xmm4, xmm8\n\
    inc rax\n\
    jmp Lm2_loop\n\
Lm2_done:\n\
    movq rax, xmm1\n\
    punpckhqdq xmm1, xmm1\n\
    movq rdx, xmm1\n\
    add rax, rdx\n\
    movdqu xmm6, [rsp]\n\
    movdqu xmm7, [rsp+16]\n\
    movdqu xmm8, [rsp+32]\n\
    add rsp, 56\n\
    ret\n";

/// Assemble [`RUNTIME_MASM`] to MC asm and embed it as module-level inline asm on
/// `module`, so its `.globl` symbols are defined in the emitted COFF. AOT path
/// only. Returns the assembler error (source-mapped) on a bad `.masm`.
pub fn embed_runtime_asm(module: &Module) -> Result<(), String> {
    let mut asm = Assembler::new();
    let mc = asm
        .assemble("runtime.masm", RUNTIME_MASM)
        .map_err(|e| format!("assembling the runtime .masm: {e}"))?;
    module.set_inline_assembly(&mc);
    Ok(())
}

/// The Win32 symbols the bundled runtime `.masm` calls internally (A4). Each must
/// exist in the **single** oracle ([`locus_winapi`]) — that is where its ABI and
/// owning DLL live. The asm contributes only the `call` site; the API *data* is
/// never copied (the owner's constraint, R-JASM-5).
pub const RUNTIME_ASM_WIN32_IMPORTS: &[&str] = &["GetStdHandle"];

/// Validate every [`RUNTIME_ASM_WIN32_IMPORTS`] symbol against the oracle and
/// return the **import libs** its DLLs need on the AOT link line — so the asm's
/// `call <sym>` resolves at link exactly like a Locus `extern`'s would, through
/// the one resolution mechanism.
///
/// A symbol the oracle does not know is a **hard error**: the asm floor may not
/// invent Win32 surface, it must go through `locus-winapi`. This is the gate that
/// keeps a second copy of the API data from creeping in.
pub fn runtime_asm_import_libs() -> Result<Vec<String>, String> {
    let mut dlls = std::collections::BTreeSet::new();
    for sym in RUNTIME_ASM_WIN32_IMPORTS {
        let f = locus_winapi::find_function_any_dll(sym).ok_or_else(|| {
            format!(
                "the runtime .masm calls Win32 `{sym}`, which is not in the oracle \
                 (locus-winapi) — the asm floor must use the single API source, not its own copy"
            )
        })?;
        dlls.insert(f.dll.clone());
    }
    Ok(dlls
        .iter()
        .filter_map(|d| locus_winapi::import_lib_for_dll(d))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime asm's Win32 imports all resolve through the single oracle, and
    /// `GetStdHandle` maps to kernel32's import lib — proving the asm reaches Win32
    /// via `locus-winapi`, with no second copy of the API data (the owner's
    /// constraint). If a symbol were missing from the oracle this would error.
    #[test]
    fn runtime_asm_win32_imports_resolve_through_the_oracle() {
        let libs =
            runtime_asm_import_libs().expect("every runtime-asm Win32 import is in the oracle");
        assert!(
            libs.iter().any(|l| l.eq_ignore_ascii_case("kernel32.lib")),
            "GetStdHandle lives in kernel32; got import libs {libs:?}"
        );
    }
}
