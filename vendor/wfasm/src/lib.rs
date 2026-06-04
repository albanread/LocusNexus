//! `locus-asm` — the MASM-style macro assembler that is Locus's **Layer-0 `asm`
//! provider** front end (D5, [`docs/jasm-boundary-layer.md`](../../docs/jasm-boundary-layer.md)).
//!
//! Vendored from `wfasm` (`E:\JASM\rust`): **only** the `asm/` Assembler — the
//! LLVM-free, text→text engine. Its entry point is
//! [`Assembler::assemble`](asm::Assembler::assemble): MASM source → a string of
//! LLVM-MC-flavored Intel-syntax asm, which Locus's own LLVM layer assembles to a
//! COFF object (AOT) or registers as absolute symbols (dev JIT). wfasm's
//! `jit`/`llvm` (a second MCJIT) and `win32` generator are deliberately **not**
//! vendored — Locus owns the LLVM layer, and `locus-winapi` is the single Win32
//! type oracle (no duplicated API data; R-JASM-5).

pub mod asm;

pub use asm::{AsmError, Assembler};
