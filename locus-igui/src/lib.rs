//! iGui — integrated Direct2D/DirectWrite MDI shell, ported for the
//! Locus IDE (`locus-ide`).
//!
//! Borrowed verbatim from NewFactor's `igui` (itself from NewCP /
//! NewCormanLisp), language-agnostic shell only. For Phase 0 of the
//! Locus IDE port the language-/Factor-specific panes are excised:
//!   - `doc_pane` / `help_pane` (pulled `docpane`/`selkie`/`pest`/
//!     `fontdue`/`pulldown-cmark`) — deferred; the Help → Documentation
//!     menu item is stubbed to a no-op.
//!   - `lisp_shims` — the Lisp host binding; the Locus host is Rust.
//!   - `stack_view` — Factor's data-stack viewer; the Locus values/
//!     effects pane is Phase 3.
//!
//! Host hooks (unchanged):
//!   - `igui::window::run(...)` — the MDI frame on a dedicated GUI thread.
//!   - `igui::install_checker(fn) + Diagnostic` — host compile-checker
//!     → inline diagnostics.

#![cfg(windows)]

pub mod batch;
pub mod channels;
mod child;
pub mod console_bridge;
pub mod cp_exports;
mod cursor;
mod d2d;
mod d3d;
mod dwrite;
mod executor;
mod font_metrics;
pub mod doc_pane;
pub mod help_pane;
pub mod log_view;
pub mod fconsole;
pub mod repl_pane;
pub mod crash_view;
pub mod crash_handler;
mod menu;
pub(crate) mod prefs;
pub(crate) mod fedit;
pub(crate) mod rope_buffer;
mod registry;
mod renderer;
mod replies;
mod tools_menu;
pub mod system_colors;
pub(crate) mod text_view;
pub mod window;

pub use fedit::{install_checker, Diagnostic};
/// Open a fast-scrolling, rope-backed **text pane** with in-memory content
/// (the compiler-view dumps — ANF IR / LLVM IR / assembly). Worker-safe; the
/// pane is created on the GUI thread. `masm` selects assembly highlighting.
pub use fedit::show_text as show_text_pane;
pub use window::run;

/// Errors surfaced from iGui startup. Phase 1 keeps this lossy on purpose;
/// every variant carries enough text to diagnose without a debugger.
#[derive(Debug)]
pub enum IGuiError {
    Win32(String),
    D3D(String),
    D2D(String),
    DWrite(String),
}

impl std::fmt::Display for IGuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IGuiError::Win32(msg) => write!(f, "iGui: Win32: {msg}"),
            IGuiError::D3D(msg) => write!(f, "iGui: D3D: {msg}"),
            IGuiError::D2D(msg) => write!(f, "iGui: D2D: {msg}"),
            IGuiError::DWrite(msg) => write!(f, "iGui: DirectWrite: {msg}"),
        }
    }
}

impl std::error::Error for IGuiError {}

/// Background colour painted into the MDI client area between
/// children.  Deep indigo — the LocusNexus ground (matches the
/// wallpaper tile so the brush seams are invisible).
pub(crate) const PHASE1_BACKGROUND: [f32; 4] = [0.071, 0.063, 0.122, 1.0];
