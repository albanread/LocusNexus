//! The assembler.
//!
//! Top-down: text source -> tokens -> expanded tokens (macros done) ->
//! a string of LLVM-MC-flavored Intel-syntax asm -> [`crate::Jit`] input.
//!
//! This file is the module root. Layering:
//!
//! * [`source`] — `SourceMap` keyed by `FileId`, owns the text of every
//!   file that participated in this assembly (including everything an
//!   `@include` pulled in).
//! * [`span`] — `(file, line, col, len)`, attached to every token and
//!   every diagnostic.
//! * [`token`] — `Token` and its `kind` enum.
//! * [`error`] — `AsmError` family with source-mapped messages.
//! * [`lex`] — text -> `Vec<Token>`, one file at a time.
//!
//! Phases above lex (parser, expander, scope tracker, Rust-macro
//! dispatch, emitter) plug into this same `Token` stream once they exist.

pub mod emit;
pub mod error;
pub mod expand;
pub mod expr;
pub mod lex;
pub mod macros;
pub mod source;
pub mod span;
pub mod token;

pub use emit::{emit, EmitError, EmitErrorKind};
pub use error::{AsmError, LexError, LexErrorKind};
pub use expand::{Assembler, DefineValue, ExpandError, ExpandErrorKind, ExternDecl, RustMacroCtx};
pub use expr::{eval as eval_expr, EvalContext, EvalError, EvalErrorKind, SimpleCtx};
pub use lex::lex;
pub use source::{FileId, SourceFile, SourceMap};
pub use span::Span;
pub use token::{NumberLit, Punct, StringLit, Token, TokenKind};
