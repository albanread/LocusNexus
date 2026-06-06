//! `locus-ide` — the Locus IDE shell.
//!
//! Phase 1 scope: the ported `igui` MDI shell (Direct2D / DirectWrite /
//! D3D11 + Win32) with an editable buffer, now backed by the **real**
//! engine. [`LocusSession::eval`] runs the Locus pipeline + JIT
//! (`jit_run_i64`) on a big-stack worker, returning the `i64` result,
//! the effect manifest, and parse/type diagnostics; [`check_source`]
//! powers the editor's live `RN-Exxxx` squiggles (installed via
//! `igui::install_checker`). The `Event`/`Graphics` services + JIT
//! symbol registration + live console capture land in Phase 2. This
//! crate *calls into* the frozen `locus*` engine crates but does not
//! modify them.

#![cfg(windows)]

pub mod report;
pub mod session;

pub use report::{analyze_report, eval_report, lowering_report};
pub use session::{
    check_source, compiler_view, CompilerView, EvalOutcome, LocusSession, ViewText,
};
