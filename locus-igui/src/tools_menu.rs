//! Frame-level menu bar and keyboard accelerators.
//!
//! Layout (standard Windows MDI app convention):
//!
//!   File   New, Open…, Save, Save As…, Exit
//!   Edit   Undo/Redo, Cut/Copy/Paste, Select All, word nav
//!   View   Console, Report, Log, Crash dump
//!   Locus  Break, Restart, Run Buffer, Analyze, Show ANF/LLVM/Assembly
//!   RunIt  one entry per .locus program in gui_runit/
//!
//! File commands route to the active fedit child via the
//! EDIT_CMD forwarding range (so opening from inside fedit Just
//! Works); File→New / File→Exit are frame-level.
//!
//! View commands open or focus a built-in pane (singleton per
//! pane).  Locus commands fire IGuiEvents the worker drains.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateAcceleratorTableW, CreateMenu, CreatePopupMenu, ACCEL, FCONTROL, FSHIFT,
    FVIRTKEY, HACCEL, HMENU, MF_POPUP, MF_SEPARATOR, MF_STRING,
};

use super::crash_view;
use super::doc_pane;
use super::fconsole;
use super::fedit;
use super::log_view;

/// Frame-level WM_COMMAND id for File → Exit.  Frame-handled.
pub const FILE_CMD_EXIT: u16 = 0x3050;

/// Frame-level WM_COMMAND id for the Forth-Restart menu item.
/// Living here so all menu IDs sit together.
pub const FORTH_RESTART_CMD_ID: u16 = 0x3200;

/// Frame-level WM_COMMAND id for Forth → Break (interrupt the
/// running eval at the next safepoint).  Unlike Restart this
/// doesn't tear down the session — just aborts the in-flight
/// eval via the VM's safepoint-interrupt mechanism.
pub const FORTH_INTERRUPT_CMD_ID: u16 = 0x3201;

/// Range reserved for auto-assigned RunIt menu items.
/// Up to 4096 entries before we overflow — well past any reasonable
/// directory size.
pub const RUNIT_CMD_BASE: u16 = 0x4000;
pub const RUNIT_CMD_END:  u16 = 0x4FFF;

/// Help → Documentation: open the manual in-window as a doc-pane
/// (rendered by the shared docpane core).  Frame-handled, on demand.
pub const HELP_CMD_DOCS: u16 = 0x5000;

/// Range for File → Recent Files entries (one id per remembered file).
pub const RECENT_CMD_BASE: u16 = 0x5100;
pub const RECENT_CMD_END:  u16 = 0x51FF;

/// Range for Theme menu entries (one id per `window::THEMES` preset).
pub const THEME_CMD_BASE: u16 = 0x5200;
pub const THEME_CMD_END:  u16 = 0x52FF;

// ─── Menu builders ────────────────────────────────────────────────────

/// Append items to a popup.  `id = 0` with label `"SEP"` inserts a
/// separator; everything else is a normal MF_STRING item.
fn append_items(popup: HMENU, ctx: &str, items: &[(u16, &str)]) {
    for &(id, label) in items {
        if label == "SEP" {
            let _ = unsafe { AppendMenuW(popup, MF_SEPARATOR, 0, PCWSTR::null()) };
            continue;
        }
        let mut w: Vec<u16> = label.encode_utf16().collect();
        w.push(0);
        if let Err(e) = unsafe {
            AppendMenuW(popup, MF_STRING, id as usize, PCWSTR(w.as_ptr()))
        } {
            eprintln!("[{ctx}] append {label:?}: {e}");
        }
    }
}

fn append_popup(bar: HMENU, ctx: &str, title: &str, popup: HMENU) {
    let mut t: Vec<u16> = title.encode_utf16().collect();
    t.push(0);
    if let Err(e) = unsafe {
        AppendMenuW(bar, MF_POPUP, popup.0 as usize, PCWSTR(t.as_ptr()))
    } {
        eprintln!("[{ctx}] append popup: {e}");
    }
}

/// File menu — New, Open, Recent Files ▸, Save, Save As, Exit. `recent` is the
/// `(id, file_name)` list for the Recent Files submenu (empty → a disabled
/// "(none)" placeholder).
pub fn append_file_menu(bar: HMENU, recent: &[(u16, String)]) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else {
        eprintln!("[file-menu] CreatePopupMenu failed");
        return;
    };
    append_items(popup, "file-menu", &[
        (fedit::MENU_CMD_ID,       "&New\tCtrl+N"),
        (fedit::EDIT_CMD_OPEN,     "&Open…\tCtrl+O"),
    ]);
    append_recent_submenu(popup, recent);
    append_items(popup, "file-menu", &[
        (0,                        "SEP"),
        (fedit::EDIT_CMD_SAVE,     "&Save\tCtrl+S"),
        (fedit::EDIT_CMD_SAVE_AS,  "Save &As…\tCtrl+Shift+S"),
        (0,                        "SEP"),
        (FILE_CMD_EXIT,            "E&xit\tAlt+F4"),
    ]);
    append_popup(bar, "file-menu", "&File", popup);
}

/// A "Recent &Files" submenu inside the File popup. Each entry opens the file;
/// an empty list shows a single disabled "(none)" item.
fn append_recent_submenu(file_popup: HMENU, recent: &[(u16, String)]) {
    let Ok(sub) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    if recent.is_empty() {
        // A greyed placeholder so the submenu isn't confusingly empty.
        let mut w: Vec<u16> = "(none)".encode_utf16().collect();
        w.push(0);
        let _ = unsafe {
            AppendMenuW(
                sub,
                MF_STRING | windows::Win32::UI::WindowsAndMessaging::MF_GRAYED,
                0,
                PCWSTR(w.as_ptr()),
            )
        };
    } else {
        for (id, name) in recent {
            let mut w: Vec<u16> = name.encode_utf16().collect();
            w.push(0);
            let _ = unsafe { AppendMenuW(sub, MF_STRING, *id as usize, PCWSTR(w.as_ptr())) };
        }
    }
    let mut title: Vec<u16> = "Recent &Files".encode_utf16().collect();
    title.push(0);
    let _ = unsafe {
        AppendMenuW(file_popup, MF_POPUP, sub.0 as usize, PCWSTR(title.as_ptr()))
    };
}

/// Theme menu — one entry per `window::THEMES` preset; switches the wallpaper.
/// The currently-active theme is checkmarked.
pub fn append_theme_menu(bar: HMENU) {
    use windows::Win32::UI::WindowsAndMessaging::MF_CHECKED;
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else {
        eprintln!("[theme-menu] CreatePopupMenu failed");
        return;
    };
    let active = super::prefs::theme_load();
    for (i, theme) in super::window::THEMES.iter().enumerate() {
        let mut w: Vec<u16> = theme.name.encode_utf16().collect();
        w.push(0);
        let id = THEME_CMD_BASE + i as u16;
        let flags = if i == active { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = unsafe { AppendMenuW(popup, flags, id as usize, PCWSTR(w.as_ptr())) };
    }
    append_popup(bar, "theme-menu", "&Theme", popup);
}

/// Edit menu — Undo, Redo, Cut, Copy, Paste, Select All, word nav.
pub fn append_edit_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else {
        eprintln!("[edit-menu] CreatePopupMenu failed");
        return;
    };
    append_items(popup, "edit-menu", &[
        (fedit::EDIT_CMD_UNDO,       "&Undo\tCtrl+Z"),
        (fedit::EDIT_CMD_REDO,       "&Redo\tCtrl+Y"),
        (0,                          "SEP"),
        (fedit::EDIT_CMD_CUT,        "Cu&t\tCtrl+X"),
        (fedit::EDIT_CMD_COPY,       "&Copy\tCtrl+C"),
        (fedit::EDIT_CMD_PASTE,      "&Paste\tCtrl+V"),
        (fedit::EDIT_CMD_SELECT_ALL, "Select &All\tCtrl+A"),
        (0,                          "SEP"),
        (fedit::EDIT_CMD_NEXT_WORD,  "Next &Word\tCtrl+\u{2192}"),
        (fedit::EDIT_CMD_PREV_WORD,  "Pre&v Word\tCtrl+\u{2190}"),
    ]);
    append_popup(bar, "edit-menu", "&Edit", popup);
}

/// View menu — the built-in panes.  Each entry focuses an
/// existing singleton or creates a new one.
pub fn append_view_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else {
        eprintln!("[view-menu] CreatePopupMenu failed");
        return;
    };
    append_items(popup, "view-menu", &[
        (fconsole::MENU_CMD_ID,    "&Console\tCtrl+Shift+R"),
        (doc_pane::MENU_CMD_ID,    "Repor&t\tCtrl+Shift+T"),
        // No REPL: Locus has no read-eval-print loop; the console is the
        // interactive surface. (stack_view excised for Phase 0.)
        (log_view::MENU_CMD_ID,    "&Log\tCtrl+Shift+L"),
        (crash_view::MENU_CMD_ID,  "Crash &Dump\tCtrl+Shift+X"),
    ]);
    append_popup(bar, "view-menu", "&View", popup);
}

/// RunIt menu — one entry per discovered `.locus` file in `gui_runit/`.
/// `runit` is a slice of `(menu_id, display_name)` pairs.  Silently
/// skipped when empty (no menu shown), so the bar stays clean when
/// the `gui_runit/` directory is absent.
pub fn append_runit_menu(bar: HMENU, runit: &[(u16, String)]) {
    if runit.is_empty() {
        return;
    }
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else {
        eprintln!("[runit-menu] CreatePopupMenu failed");
        return;
    };
    for (id, name) in runit {
        let mut w: Vec<u16> = name.encode_utf16().collect();
        w.push(0);
        if let Err(e) = unsafe {
            AppendMenuW(popup, MF_STRING, *id as usize, PCWSTR(w.as_ptr()))
        } {
            eprintln!("[runit-menu] append {name:?}: {e}");
        }
    }
    append_popup(bar, "runit-menu", "&RunIt", popup);
}

/// Locus menu — language-thread lifecycle and buffer evaluation.
pub fn append_forth_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else {
        eprintln!("[locus-menu] CreatePopupMenu failed");
        return;
    };
    append_items(popup, "locus-menu", &[
        (FORTH_INTERRUPT_CMD_ID,      "&Break\tCtrl+B"),
        (FORTH_RESTART_CMD_ID,        "&Restart\tCtrl+Shift+F5"),
        (0,                           "SEP"),
        (fedit::EDIT_CMD_RUN_BUFFER,  "R&un Buffer\tF5"),
        (fedit::EDIT_CMD_ANALYZE,     "&Analyze\tF6"),
        (0,                           "SEP"),
        (fedit::EDIT_CMD_ANF,         "Show A&NF IR"),
        (fedit::EDIT_CMD_LLVM,        "Show &LLVM IR"),
        (fedit::EDIT_CMD_ASM,         "Show Assemb&ly"),
    ]);
    append_popup(bar, "locus-menu", "&Locus", popup);
}

/// Help menu — Documentation (opens the manual in-window as a doc-pane).
pub fn append_help_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else {
        eprintln!("[help-menu] CreatePopupMenu failed");
        return;
    };
    append_items(popup, "help-menu", &[
        (HELP_CMD_DOCS, "&Documentation\tF1"),
    ]);
    append_popup(bar, "help-menu", "&Help", popup);
}

/// Build the default frame menu bar: File, Edit, View, Locus, [RunIt], Theme,
/// Help.  `runit` and `recent` carry `(id, display_name)` pairs (RunIt
/// programs and the recent-files list); pass empty slices to omit / empty them.
pub fn build_default_menu_bar(
    runit: &[(u16, String)],
    recent: &[(u16, String)],
) -> Option<HMENU> {
    let bar = unsafe { CreateMenu() }.ok()?;
    append_file_menu(bar, recent);
    append_edit_menu(bar);
    append_view_menu(bar);
    append_forth_menu(bar);
    append_runit_menu(bar, runit);
    append_theme_menu(bar);
    append_help_menu(bar);
    Some(bar)
}

/// Frame-level accelerator table.  Mirrors the visible menu
/// shortcuts so power-users get the same keystrokes regardless of
/// whether the menu is open.
pub fn build_accelerator_table() -> Option<HACCEL> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F5, VK_F6};
    let entries = [
        // File
        ACCEL { fVirt: FCONTROL | FVIRTKEY,          key: b'N' as u16, cmd: fedit::MENU_CMD_ID },
        ACCEL { fVirt: FCONTROL | FVIRTKEY,          key: b'O' as u16, cmd: fedit::EDIT_CMD_OPEN },
        ACCEL { fVirt: FCONTROL | FVIRTKEY,          key: b'S' as u16, cmd: fedit::EDIT_CMD_SAVE },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'S' as u16, cmd: fedit::EDIT_CMD_SAVE_AS },
        // View
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'R' as u16, cmd: fconsole::MENU_CMD_ID },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'T' as u16, cmd: doc_pane::MENU_CMD_ID },
        // stack_view (Ctrl+Shift+K) excised for Phase 0.
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'L' as u16, cmd: log_view::MENU_CMD_ID },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'X' as u16, cmd: crash_view::MENU_CMD_ID },
        // Locus
        ACCEL { fVirt: FCONTROL | FVIRTKEY,          key: b'B' as u16, cmd: FORTH_INTERRUPT_CMD_ID },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: VK_F5.0,     cmd: FORTH_RESTART_CMD_ID },
        ACCEL { fVirt: FVIRTKEY,                     key: VK_F5.0,     cmd: fedit::EDIT_CMD_RUN_BUFFER },
        ACCEL { fVirt: FVIRTKEY,                     key: VK_F6.0,     cmd: fedit::EDIT_CMD_ANALYZE },
        // Help
        ACCEL { fVirt: FVIRTKEY,                     key: 0x70_u16,    cmd: HELP_CMD_DOCS },
    ];
    unsafe { CreateAcceleratorTableW(&entries) }
        .ok()
        .filter(|h| !h.is_invalid())
}
