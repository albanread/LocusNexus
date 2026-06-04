//! # Locus — core (Phase 1)
//!
//! A from-scratch typechecker for the Locus core calculus
//! ([`../../docs/calculus.md`]). This crate is the **core checker**: a
//! direct, readable transcription of the typing rules — *simple clarity
//! over complex convenience*. Every rule cites its calculus section, so the
//! code and the spec stay one artifact (manifesto F10: behaviour is
//! derivable from the rules, not from the compiler).
//!
//! ## Slices
//!
//! - **Slice 1: the effect fragment.** Default-pure, row-inferred
//!   (`calculus.md` §1.1, §2.1).
//! - **Slice 2: handlers.** `handle e with H` *discharges* the labels it
//!   handles — effects shrink (the (op) rule, §2.1; preservation §7).
//! - **Slice 3: staging.** Stages (object=0), `Code[T]` (`□`),
//!   `quote`/`splice`, and the `O`/`G` split `δ` performs at a quote
//!   boundary (§3). Single-stage rejects nested quotes; SO-1 stops the
//!   generator reading a runtime binder.
//! - **Slice 4: let-insertion (§4.1(a)).** `genlet c` ≡ `perform Insert(c)`
//!   — a generative effect; `letloc { … }` and `splice` are its **loci**
//!   (handlers that discharge `Insert`). *Deferred:* the scope-safety check
//!   (§4.1(b), `RN-E0331`) — needs `Code` values to carry their open scope.
//! - **Slice 5: a parser.** Locus source text → the core AST ([`parse`]).
//!   A minimal *explicit*, ML-style surface (delimiters, not indentation) that
//!   maps one-to-one onto the core.
//! - **Slice 6: the `locus` CLI** (`src/main.rs`). The agent-focused command
//!   surface (design §8): `check` (parse + infer → `type ! row @ stage`) and
//!   `ast`, with `-e EXPR`, file/stdin, and clean exit codes.
//! - **Slice 7: structured diagnostics** ([`diag::Report`]). One
//!   representation, three renderings — labelled **text** (default),
//!   `--brief`, and `--json` (schema `locus-diag/1`) — so the human and the
//!   machine see the same fields (design §8.1).
//! - **Slice 8: richer diagnostics.** Source [`Span`]s (the lexer tags every
//!   token, so parse errors point at `line:col`), stable `RN-Exxxx` **codes**,
//!   **spec citations** (the calculus §), and **hints** — design §8's
//!   "spec-citing by construction."
//! - **Slice 9: sema — the authoritative typed model.** [`elaborate`]
//!   decorates *every* node with its `type ! row @ stage` ([`sema::Typed`]);
//!   `infer` is now a thin projection of it. This decorated tree — not the raw
//!   AST — is what the later phases read. Exposed as `locus sema` (text tree,
//!   or `--json` schema `locus-sema/1`).
//! - **Slice 10: IR — A-normal form.** [`lower`] turns the typed tree into ANF
//!   ([`ir::Ir`]): every intermediate is `let`-named, every operand is an atom,
//!   and each binding is tagged with the effect row it performs. This makes
//!   evaluation order — hence effect sequencing and the non-tail continuations
//!   of `calculus.md` §5.1 — explicit. Exposed as `locus ir` (`locus-ir/1`).
//! - **Slice 11: evidence-passing — the zero-cost witness.** [`analyze`]
//!   threads an evidence vector over the IR (`calculus.md` §5): each `perform`
//!   is resolved against the handlers in scope and classified by the handler's
//!   resumption [`Shape`], or left **residual**. The *executable* proof of the
//!   §5.2 zero-cost theorem. Exposed as `locus evidence` (`locus-evidence/1`).
//! - **Slice 12: zero-cost is a front-end *guarantee*.** Elimination is gated
//!   on the handler being **in force at compile time** — installed at the
//!   generation stage (`stage >= 1`) — *and* statically reached (no λ crossed).
//!   A runtime (stage-0) handler is only **dispatch-free**: the same handler
//!   reads *dispatch-free* at stage 0 but *eliminated* at stage 1. Zero-cost is
//!   **earned by staging** the handler, not left to LLVM (§5.2).
//! - **Slice 13: a `String` type + `"…"` literals** — the value `writeln`
//!   needs; threads through every phase untouched ([`Type::Str`]).
//! - **Slice 14 (this commit): the runtime prelude — native effects.** A
//!   native effect (a `World` label) is fully interceptable, but its *default*
//!   handler is a **prelowered** Rust runtime function the JIT calls
//!   ([`prelude`]). The evidence pass splits a residual op into **→ runtime**
//!   (native — the prelowered fn) vs **unhandled** (user — escapes to the
//!   caller). The CLI loads the prelude `Sig` (`console : String => Unit`, …)
//!   by default, so interception lives in the language, the runtime supplies
//!   only the default.
//!
//! The pipeline `parse → check → …` is built one honest slice at a time;
//! nothing here is scaffolded ahead of a proved rule.

pub mod capability;
pub mod check;
pub mod diag;
pub mod evidence;
pub mod iface;
pub mod ir;
pub mod parse;
pub mod prelude;
pub mod sema;
pub mod stage;
pub mod stdlib;
pub mod syntax;
pub mod tagcheck;
pub mod unify;

pub use capability::{check_module_seals, mint_gate, CapError};
pub use check::{infer, Ctx, Sig, Stage, TypeErr};
pub use diag::{Report, Span, SCHEMA};
pub use evidence::{analyze, clause_shape, Cost, Shape};
pub use iface::{
    check_client_against, check_client_against_with_imports, exported_functions, interface_of,
    mangle_export, ConsumeError, ExportedFn, Import, ImportedSymbol, LoadedInterface,
    ModuleInterface, SumVariant, TypeDefKind, TypeExport, ValExport, ABI_VERSION, LOCUSI_FORMAT,
};
pub use ir::{
    lower, lower_function_body, lower_with_imports, Atom, Comp, Ir, IrHandler, IrOpClause, LoopVar,
};
pub use parse::{parse, parse_module, parse_program, ParseErr};
pub use sema::{elaborate, Node, Typed};
pub use stage::stage_reduce;
pub use stdlib::{
    first_mint, linux_modules as linux_stdlib_modules, linux_program, linux_program_with_modules,
    linux_stdlib_module_decls, modules as stdlib_modules, program, program_with_modules,
    program_with_stdlib, stdlib_module_decls, stdlib_module_decls_from,
};
pub use syntax::{
    BinOp, CastOp, ExternAbi, FloatMathOp, Handler, Label, Layer, MaskReduceOp, MemWidth,
    ModuleDecl, OpClause, OpSig, ProgramSource, Return, Row, RowVarId, Term, TyVarId, Type,
    ValueLayout, VectorShape, Width,
};
pub use tagcheck::{check_tags, TagError};
pub use unify::{unify, unify_row, zonk, UnifStore};

/// The stack a full pipeline run (parse → **elaborate** → stage → lower) should
/// be given. Elaboration recurses deeply over the nested stdlib graft (a
/// ~10-module `let … in let … in …` chain) and staging is a native tree-walker,
/// so a large program needs far more than a default thread stack. The driver and
/// the test harnesses run the pipeline on a worker of this size; keeping it
/// generous means adding stdlib bindings (or compiling a big program) never
/// trips a stack overflow. (Matches the staging worker.)
pub const PIPELINE_STACK_BYTES: usize = 256 * 1024 * 1024;
