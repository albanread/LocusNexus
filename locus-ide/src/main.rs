// Release builds ship as a Windows GUI-subsystem app: no console
// window pops up when the user launches locus-ide.exe from Explorer.
// Debug builds keep the console attached so eprintln! traces show up
// when running under `cargo run`.
#![cfg_attr(
    all(windows, not(debug_assertions)),
    windows_subsystem = "windows"
)]

//! `locus-ide` — the Locus IDE host.
//!
//! Structurally mirrors NewFactor's `src/bin/newfactor_ui.rs`, but
//! swaps the in-process Factor `Session` for a stubbed
//! [`locus_ide::LocusSession`] (Phase 0). The shell (`igui`) runs the
//! Direct2D MDI frame on a dedicated GUI thread; an IDE worker thread
//! drains `IGuiEvent`s. The eval/JIT wiring, `install_checker`
//! squiggles, and the `Event`/`Graphics` services land in later phases.
//!
//! ```text
//! locus-ide.exe  (one Windows process)
//! ├── GUI thread        Direct2D MDI, Win32 message pump (igui)
//! │     ↕ IGuiEvent MPSC channel
//! └── IDE worker        receives events, drives LocusSession (stub)
//! ```

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Phase 1: install the live checker (locus parse→elaborate) so the
    // editor shows RN-Exxxx squiggles as you type.
    install_editor_checker();

    // Route a program's `perform console` output to the IDE console pane (the
    // runtime sink). `console_writeln`/`console_read_line` ride the
    // WriteConsoleW/ReadConsoleW overrides instead; together they make the whole
    // console surface land in the pane.
    locus_rt::set_console_sink(Some(console_pane_sink));

    igui::crash_handler::install();

    // The VEH above catches hardware SEH (access violations, etc.). A *Rust
    // panic* — an unwrap / poisoned-lock / `expect` on the worker thread —
    // unwinds silently instead: the worker dies, `eval` never returns, and the
    // IDE just looks frozen with no crash window. So also install a panic hook
    // that formats the panic (message, location, thread, backtrace) and surfaces
    // it in the SAME crash view (`crash_view::push` is thread-safe and posts a
    // GUI-thread flush — it was built for exactly this), then chains to the
    // default hook for stderr.
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Box<dyn Any>".to_string());
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let thread = std::thread::current();
        let tname = thread.name().unwrap_or("<unnamed>").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        igui::crash_view::push(format!(
            "kind:           Rust panic\n\
             thread:         {tname}\n\
             location:       {loc}\n\
             message:        {msg}\n\n\
             backtrace:\n{bt}"
        ));
        default_panic_hook(info);
    }));

    // LocusNexus wallpaper: a violet ⁂ (asterism — "a locus of points")
    // tiled on a deep-indigo ground.  Distinct from the Factor IDE, and
    // the brand mark of the Locus world.
    igui::window::set_frame_palette(igui::window::FramePalette {
        bg: 0x12101F, // deep indigo ground
        fg: 0x7A5AC8, // violet asterism glyph
    });

    let worker = || {
        wait_for_frame();
        retitle_frame();
        auto_open_console();
        run_ide_worker();
    };
    let exit_code = igui::run(Some(worker))?;
    std::process::exit(exit_code);
}

/// The console-line sink the runtime calls for a program's `perform console`
/// output — forwards each line into the igui console pane. A plain `fn(&str)`
/// (it coerces to the `Option<fn(&str)>` `set_console_sink` takes); the line is
/// already decoded, so no Locus-heap access happens here.
#[cfg(windows)]
fn console_pane_sink(line: &str) {
    igui::console_bridge::write_line(line);
}

/// Install the Locus parse→elaborate checker so the editor renders
/// live `RN-Exxxx` squiggles. `igui::install_checker` takes a
/// `Fn(&str) -> Vec<igui::Diagnostic>`; [`locus_ide::check_source`] runs
/// `program → elaborate` on a big-stack worker and maps any error to a
/// `Diagnostic` (code folded into the message, 1-based `line:col`). A
/// well-typed buffer yields an empty `Vec` (no squiggles).
#[cfg(windows)]
fn install_editor_checker() {
    igui::install_checker(|src: &str| locus_ide::check_source(src));
}

// ── IDE worker ────────────────────────────────────────────────────────────

/// The IDE worker loop: drain `IGuiEvent`s and drive `LocusSession`.
/// The eval arms run `LocusSession::eval` (= the Locus pipeline +
/// `jit_run_i64` on a big-stack worker) and format the result + effect
/// manifest + diagnostics into the console / REPL pane.
#[cfg(windows)]
fn run_ide_worker() {
    use igui::channels::{self, IGuiEvent};
    use igui::fconsole;
    use locus_ide::LocusSession;

    boot_banner();
    let mut session = LocusSession::new();

    loop {
        let Some(ev) = channels::next_event(200) else { continue };

        match ev {
            IGuiEvent::EvalBuffer { source } => {
                // F5 / RUN_BUFFER: run the buffer through the real
                // pipeline + JIT and echo the result + effect manifest
                // (+ any diagnostics) into the console pane.
                let outcome = session.eval(&source);
                fconsole::append(&outcome.to_console_text());
            }
            IGuiEvent::AnalyzeBuffer { source } => {
                // F6 / Locus → Analyze: elaborate the buffer WITHOUT
                // running it (no JIT, no side effects, no event loop) and
                // render an effect/capability *report* into the doc pane —
                // markdown + a Mermaid confinement diagram of the powers
                // the program names. `show_report` pops the pane up, since
                // analyzing is an explicit, report-seeking action.
                igui::doc_pane::show_report(&locus_ide::analyze_report(&source));
            }
            IGuiEvent::ShowLowering { source, kind } => {
                // Locus → Show ANF/LLVM/Assembly: a compiler-exploration view.
                // Run the front-end + lowering (no JIT) and drop the (verbose)
                // dump into a fast-scrolling, rope-backed text pane — not the
                // markdown doc pane. Assembly highlights as Masm; ANF/LLVM are
                // shown plain. Errors go to the same pane as text.
                let view = locus_ide::CompilerView::from_tag(kind);
                let lang = match view {
                    locus_ide::CompilerView::Anf => "anf",
                    locus_ide::CompilerView::Asm => "asm",
                    locus_ide::CompilerView::Llvm => "plain", // no LLVM highlighter yet
                };
                let out = locus_ide::compiler_view(&source, view);
                if out.diagnostics.is_empty() {
                    let title = format!("\u{2042} {}", view.title());
                    igui::show_text_pane(&title, &out.text, lang);
                } else {
                    let mut body = format!("{} — did not lower:\n\n", view.title());
                    for d in &out.diagnostics {
                        body.push_str(&format!("{}:{}  {}\n", d.line, d.column, d.message));
                    }
                    igui::show_text_pane(&format!("\u{2042} {} (errors)", view.title()), &body, "plain");
                }
            }
            IGuiEvent::ReplSubmit { child_id } => {
                use igui::repl_pane::{self, AppendKind};
                let Some(source) = repl_pane::pop_input(child_id) else { continue };
                let outcome = session.eval(&source);
                // An ok result is Output; a parse/type/JIT error is an
                // Error append so the REPL pane styles it distinctly.
                let kind = if outcome.ok() {
                    AppendKind::Output
                } else {
                    AppendKind::Error
                };
                repl_pane::append(child_id, outcome.to_console_text(), kind);
            }
            IGuiEvent::ForthRestart => {
                // Restart (Ctrl+Shift+F5). Break already cooperatively
                // stopped any running program (its synthetic Close made
                // eval return), so by the time we drain this the worker
                // is idle. Locus keeps no resident session to rebuild —
                // each F5 elaborates and JITs the buffer fresh — so all
                // Restart can meaningfully do is acknowledge the stop and
                // leave a clean prompt.
                fconsole::append("locus-ide: stopped \u{2014} ready.");
            }
            IGuiEvent::FrameClose => {
                fconsole::append("locus-ide: frame closing");
                return;
            }
            _ => {}
        }
    }
}

#[cfg(windows)]
fn boot_banner() {
    use igui::fconsole;
    fconsole::append("Locus IDE");
    fconsole::append("");
    fconsole::append("edit -> run -> see value/effects/errors, live. F5 runs the buffer");
    fconsole::append("(result + effect manifest here in the console); F6 analyzes it into a");
    fconsole::append("report (effects + diagram, without running); F7 checks it (squiggles).");
    fconsole::append("Console: Ctrl+Shift+R   Report: Ctrl+Shift+T   Help: F1");
    fconsole::append("");
}

// ── Startup helpers ───────────────────────────────────────────────────────

#[cfg(windows)]
fn wait_for_frame() {
    use std::time::Duration;
    for _ in 0..200 {
        if igui::cp_exports::FRAME_HWND.get().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("[locus-ide] FRAME_HWND not published after 4 s; continuing anyway");
}

/// Override iGui's default frame title with the Locus IDE's.
#[cfg(windows)]
fn retitle_frame() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::SetWindowTextW;

    let Some(&hwnd_isize) = igui::cp_exports::FRAME_HWND.get() else {
        return;
    };
    let hwnd = HWND(hwnd_isize as *mut _);
    let title: Vec<u16> = "\u{2042} Locus \u{2014} IDE"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = unsafe { SetWindowTextW(hwnd, PCWSTR(title.as_ptr())) };
}

#[cfg(windows)]
fn auto_open_console() {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_COMMAND};
    let Some(&hwnd_isize) = igui::cp_exports::FRAME_HWND.get() else {
        return;
    };
    let hwnd = HWND(hwnd_isize as *mut _);
    let cmd_id = igui::fconsole::MENU_CMD_ID;
    let _ = unsafe {
        PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(cmd_id as usize), LPARAM(0))
    };
}

#[cfg(not(windows))]
fn main() {
    eprintln!("locus-ide is Windows-only (igui depends on Direct2D / DirectWrite).");
    std::process::exit(1);
}
