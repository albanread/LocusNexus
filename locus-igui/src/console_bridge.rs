//! console_bridge — route a JIT'd Locus program's console I/O to the IDE
//! console pane (the `⁂ console` fconsole) instead of the (absent) OS console.
//!
//! A Locus program's console output funnels through `WriteConsoleW` and its
//! input through `ReadConsoleW` (see `stdlib/winapi.locus`: `win_write_console`
//! / `win_read_line`). `locus-ide` is a GUI app with no console, so that I/O
//! would otherwise vanish. The IDE injects the two shims below as **absolute
//! symbol overrides** through the JIT's `extra` table (deduped last-wins over
//! the kernel32 addresses — see `jit_run_i64_with_symbols`), so:
//!
//!   * output → streamed line-by-line into the console pane (`fconsole::append`)
//!   * input  ← the line the user submits at the console pane's prompt
//!
//! This is the console analogue of the `Graphics`/`Event` IDE-world services:
//! the IDE is the running program's *world*, and the console is part of it. The
//! program resolves the same *names* (`WriteConsoleW`/`ReadConsoleW`); only the
//! address differs, so its effect row is unchanged — `winapi` still shows.

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::{Condvar, Mutex};

use super::fconsole;

// ── output: stream WriteConsoleW chunks into the pane, line by line ──────────

/// Accumulates output until a newline, then flushes the completed line to the
/// pane. `WriteConsoleW` arrives in arbitrary chunks — `console_writeln` emits
/// the text, then CR and LF as two separate one-char writes — so we buffer and
/// split on `\n`, dropping the `\r`.
static OUT_LINEBUF: Mutex<String> = Mutex::new(String::new());

/// Feed a chunk of decoded output text through the line buffer, appending each
/// completed line to the console pane. Lines are collected under the lock and
/// emitted after releasing it (`fconsole::append` posts cross-thread).
fn out_push(s: &str) {
    let mut completed: Vec<String> = Vec::new();
    {
        let mut buf = OUT_LINEBUF.lock().expect("OUT_LINEBUF poisoned");
        for ch in s.chars() {
            match ch {
                '\n' => completed.push(std::mem::take(&mut *buf)),
                '\r' => {} // the partner of console_writeln's LF — drop it
                c => buf.push(c),
            }
        }
    }
    for line in completed {
        fconsole::append(&line);
    }
}

/// Flush any buffered partial line (text with no trailing newline) to the pane.
/// Called before blocking for input so a prompt like `Enter name: ` is visible
/// even though the program hasn't written a newline after it.
fn out_flush_partial() {
    let line = {
        let mut buf = OUT_LINEBUF.lock().expect("OUT_LINEBUF poisoned");
        if buf.is_empty() {
            return;
        }
        std::mem::take(&mut *buf)
    };
    fconsole::append(&line);
}

/// Append a complete console line — the entry point for `perform console`,
/// which the runtime routes here through the sink the IDE installs
/// (`locus_rt::set_console_sink`). Goes through the same line buffer as
/// `WriteConsoleW` output so the native effect and the `console_writeln` service
/// interleave in the right order.
pub fn write_line(s: &str) {
    out_push(s);
    out_push("\n");
}

/// `locus_write_float` shim (runtime ABI). `console_write_float` lowers to a
/// direct `call @locus_write_float`, whose default runtime impl is a `println!`
/// to the process stdout — invisible in a GUI app. Override it to stream the
/// value (newline-terminated, matching `println!`) into the console pane, so
/// float output is uniform with the rest of the console surface.
pub extern "C" fn igui_console_write_float(bits: i64) {
    out_push(&format!("{}\n", f64::from_bits(bits as u64)));
}

/// `WriteConsoleW` shim (kernel32 ABI). Streams the UTF-16 buffer into the
/// console pane and always reports success, so the Locus boundary's `WriteFile`
/// fallback never fires. The console handle is irrelevant here and ignored.
pub extern "system" fn igui_console_write_w(
    _handle: *mut c_void,
    buffer: *const u16,
    to_write: u32,
    written: *mut u32,
    _reserved: *mut c_void,
) -> i32 {
    if !buffer.is_null() && to_write > 0 {
        let units = unsafe { std::slice::from_raw_parts(buffer, to_write as usize) };
        out_push(&String::from_utf16_lossy(units));
    }
    if !written.is_null() {
        unsafe { *written = to_write };
    }
    1 // TRUE
}

// ── input: block the program on ReadConsoleW until the pane delivers a line ──

struct InputState {
    /// A JIT'd program is blocked in `ReadConsoleW`, waiting for a line.
    waiting: bool,
    /// The line delivered by `deliver_input` / `cancel_input`, taken by the
    /// blocked reader. `None` until one is delivered.
    pending: Option<String>,
}

static INPUT: Mutex<InputState> = Mutex::new(InputState {
    waiting: false,
    pending: None,
});
static INPUT_CV: Condvar = Condvar::new();

/// True iff a JIT'd program is currently blocked waiting for a console line.
/// fconsole consults this to decide whether the program is interactive.
pub fn is_waiting() -> bool {
    INPUT.lock().map(|s| s.waiting).unwrap_or(false)
}

/// Deliver a line to a program blocked in `ReadConsoleW`. Returns `true` if a
/// program was waiting (the line was consumed by it); `false` if none was — in
/// which case the caller should handle the line normally (start a fresh eval).
/// The trailing newline the program expects is added by the read shim.
pub fn deliver_input(line: &str) -> bool {
    let mut st = INPUT.lock().expect("INPUT poisoned");
    if !st.waiting {
        return false;
    }
    st.pending = Some(line.to_string());
    st.waiting = false;
    INPUT_CV.notify_all();
    true
}

/// Unblock any waiting reader with EOF (an empty line). Called when the console
/// pane / frame closes so a program stuck on input doesn't hang the eval worker.
pub fn cancel_input() {
    let mut st = INPUT.lock().expect("INPUT poisoned");
    if st.waiting {
        st.pending = Some(String::new());
        st.waiting = false;
        INPUT_CV.notify_all();
    }
}

/// Block the calling (eval-worker) thread until a line is delivered to the pane.
fn block_for_line() -> String {
    out_flush_partial(); // make any pending prompt visible first
    let mut st = INPUT.lock().expect("INPUT poisoned");
    st.pending = None;
    st.waiting = true;
    loop {
        if let Some(line) = st.pending.take() {
            st.waiting = false;
            return line;
        }
        st = INPUT_CV.wait(st).expect("INPUT_CV poisoned");
    }
}

/// `ReadConsoleW` shim (kernel32 ABI). Blocks until the user submits a line at
/// the console pane, then returns it as UTF-16 terminated with CR LF — exactly
/// what a real console returns, which `win_read_line` then strips back off.
pub extern "system" fn igui_console_read_w(
    _handle: *mut c_void,
    buffer: *mut u16,
    to_read: u32,
    read: *mut u32,
    _input_control: *mut c_void,
) -> i32 {
    let line = block_for_line();
    let mut units: Vec<u16> = line.encode_utf16().collect();
    units.push(13); // CR
    units.push(10); // LF
    let n = units.len().min(to_read as usize);
    if !buffer.is_null() && n > 0 {
        unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), buffer, n) };
    }
    if !read.is_null() {
        unsafe { *read = n as u32 };
    }
    1 // TRUE
}

// ── the symbol overrides the IDE injects ─────────────────────────────────────

/// The `(name, fn-ptr-as-u64)` console overrides the IDE adds to the JIT's
/// `extra` table. Deduped last-wins over the winapi-resolved kernel32 addresses,
/// so a program's `WriteConsoleW`/`ReadConsoleW` bind to these pane shims.
pub fn symbol_overrides() -> Vec<(&'static str, u64)> {
    vec![
        ("WriteConsoleW", igui_console_write_w as usize as u64),
        ("ReadConsoleW", igui_console_read_w as usize as u64),
        ("locus_write_float", igui_console_write_float as usize as u64),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pane sink (`fconsole::append`) can't run headless, but the input
    // rendezvous is pure state and is unit-testable.

    #[test]
    fn deliver_returns_false_when_nobody_waits() {
        // No program blocked → the line is not consumed (caller evals it).
        assert!(!deliver_input("ignored"));
        assert!(!is_waiting());
    }

    #[test]
    fn a_blocked_reader_receives_the_delivered_line() {
        use std::thread;
        // Spawn a "program" that blocks for a line.
        let reader = thread::spawn(|| block_for_line());
        // Wait until it has registered as waiting (bounded spin).
        let mut spins = 0;
        while !is_waiting() && spins < 100_000 {
            std::hint::spin_loop();
            spins += 1;
        }
        assert!(is_waiting(), "reader should register as waiting");
        assert!(deliver_input("hello"), "a waiting reader consumes the line");
        assert_eq!(reader.join().unwrap(), "hello");
        assert!(!is_waiting(), "no longer waiting after delivery");
    }
}
