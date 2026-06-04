//! Linux x86-64 Layer-0 asm seed.
//!
//! The CPU-level MASM surface is shared with the Windows runtime, but the call
//! boundary is Linux SysV: integer arguments use `rdi`, `rsi`, `rdx`, `rcx`,
//! `r8`, `r9`, and every XMM register is caller-saved. The sidecar registers
//! Rust dev-twin addresses for ORC today, while this MASM text keeps the native
//! asm body ready for the ELF/AOT path.

use inkwell::module::Module;
use locus_asm::Assembler;

pub const RUNTIME_MASM: &str = "\
.intel_syntax noprefix\n\
.text\n\
\n\
; A1 bring-up: prove the Locus -> Layer-0 asm pipe (returns 42).\n\
.globl locus_asm_hello\n\
locus_asm_hello:\n\
    mov rax, 42\n\
    ret\n\
\n\
; Linux SysV ABI: integer args in rdi, rsi, rdx, rcx, r8, r9; result in rax.\n\
\n\
; rotl64(x: rdi, k: rsi) -> rax = x rotated left by (k mod 64).\n\
.globl locus_asm_rotl64\n\
locus_asm_rotl64:\n\
    mov rax, rdi\n\
    mov rcx, rsi\n\
    rol rax, cl\n\
    ret\n\
\n\
; rotr64(x: rdi, k: rsi) -> rax = x rotated right by (k mod 64).\n\
.globl locus_asm_rotr64\n\
locus_asm_rotr64:\n\
    mov rax, rdi\n\
    mov rcx, rsi\n\
    ror rax, cl\n\
    ret\n\
\n\
; popcount64(x: rdi) -> rax = number of set bits in x.\n\
.globl locus_asm_popcount64\n\
locus_asm_popcount64:\n\
    popcnt rax, rdi\n\
    ret\n\
\n\
; bswap64(x: rdi) -> rax = x with its 8 bytes reversed.\n\
.globl locus_asm_bswap64\n\
locus_asm_bswap64:\n\
    mov rax, rdi\n\
    bswap rax\n\
    ret\n\
\n\
; mandel(cx: xmm0, cy: xmm1, max: rdi) -> rax = escape iteration count.\n\
.globl locus_asm_mandel\n\
locus_asm_mandel:\n\
    mov r10, 0x4010000000000000\n\
    movq xmm6, r10\n\
    xorpd xmm2, xmm2\n\
    xorpd xmm3, xmm3\n\
    xor rax, rax\n\
Lmandel_loop:\n\
    cmp rax, rdi\n\
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
    ret\n";

pub fn assemble_runtime() -> Result<String, String> {
    let mut asm = Assembler::new();
    asm.assemble("linux-runtime.masm", RUNTIME_MASM)
        .map_err(|e| format!("assembling linux runtime .masm: {e}"))
}

pub fn embed_runtime_asm(module: &Module) -> Result<(), String> {
    let mc = assemble_runtime()?;
    module.set_inline_assembly(&mc);
    Ok(())
}

pub fn runtime_symbols() -> Vec<(&'static str, u64)> {
    vec![
        ("locus_asm_hello", locus_asm_hello as usize as u64),
        ("locus_asm_rotl64", locus_asm_rotl64 as usize as u64),
        ("locus_asm_rotr64", locus_asm_rotr64 as usize as u64),
        ("locus_asm_popcount64", locus_asm_popcount64 as usize as u64),
        ("locus_asm_bswap64", locus_asm_bswap64 as usize as u64),
        ("locus_asm_mandel", locus_asm_mandel as usize as u64),
    ]
}

#[no_mangle]
pub extern "C" fn locus_asm_hello(_: i64) -> i64 {
    42
}

#[no_mangle]
pub extern "C" fn locus_asm_rotl64(x: i64, k: i64) -> i64 {
    (x as u64).rotate_left((k as u32) & 63) as i64
}

#[no_mangle]
pub extern "C" fn locus_asm_rotr64(x: i64, k: i64) -> i64 {
    (x as u64).rotate_right((k as u32) & 63) as i64
}

#[no_mangle]
pub extern "C" fn locus_asm_popcount64(x: i64) -> i64 {
    (x as u64).count_ones() as i64
}

#[no_mangle]
pub extern "C" fn locus_asm_bswap64(x: i64) -> i64 {
    (x as u64).swap_bytes() as i64
}

#[no_mangle]
pub extern "C" fn locus_asm_mandel(cx: f64, cy: f64, max: i64) -> i64 {
    let mut zx = 0.0;
    let mut zy = 0.0;
    let mut n = 0;
    while n < max {
        let zx2 = zx * zx;
        let zy2 = zy * zy;
        if zx2 + zy2 > 4.0 {
            break;
        }
        let next_zy = 2.0 * zx * zy + cy;
        let next_zx = zx2 - zy2 + cx;
        zx = next_zx;
        zy = next_zy;
        n += 1;
    }
    n
}
