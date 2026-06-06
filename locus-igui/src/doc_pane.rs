//! doc_pane — a markdown + Mermaid **report** pane.
//!
//! This is the IDE's general report surface: hand it markdown (with
//! optional ```mermaid fenced diagrams) and it renders a scrollable,
//! formatted document. The IDE uses it for *reports* — e.g. the
//! effect/capability report after an eval (what powers a program named,
//! drawn as a flow diagram). It is **not** the Help/manual pane; that is
//! `help_pane`'s specialised job. Reports → here; help docs → there.
//!
//! Structurally a read-only MDI child like `log_view`: a process-wide
//! `Mutex<DocState>` holds the parsed document (set from any thread via
//! [`set_report`]); the UI thread re-lays-it-out per paint at the current
//! width and draws it through the vendored `docpane` core (`parser` →
//! `layout` → `render::draw_document`, Mermaid via `selkie`).
//!
//! Rendering uses `docpane`'s own Direct2D / DirectWrite factories
//! ([`docpane::render::init`]), and the pane's `ID2D1HwndRenderTarget` is
//! created from that same factory so Mermaid path geometry (built on
//! `docpane`'s factory) composites into our target without a cross-factory
//! fault.

#![cfg(windows)]

use std::sync::Mutex;

use docpane::layout::{layout, Layout};
use docpane::parser::{parse, Block};
use docpane::render;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DefMDIChildProcW, GetClientRect, GetWindowLongPtrW, IsWindow, LoadCursorW,
    PostMessageW, RegisterClassExW, SendMessageW, SetWindowLongPtrW, CW_USEDEFAULT, GWLP_USERDATA,
    IDC_ARROW, MDICREATESTRUCTW, WHEEL_DELTA, WM_COMMAND, WM_DPICHANGED_AFTERPARENT,
    WM_LBUTTONDOWN, WM_MDIACTIVATE, WM_MDICREATE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_SETFOCUS, WM_SIZE, WNDCLASSEXW, WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// WM_COMMAND id for the "View > Report" menu entry. 0x3005 — past
/// crash_view (0x3003) and repl_pane (0x3004, retired but its id is kept
/// distinct to avoid a dispatch collision).
pub const MENU_CMD_ID: u16 = 0x3005;

const DOC_CLASS: PCWSTR = w!("WF64.iGui.DocPane");
const DOC_TITLE: PCWSTR = w!("\u{2042} report");

/// Left/right/top margin (DIPs) the document content is inset by inside
/// the pane. `docpane` bakes the `x_base` into its draw commands, so we
/// lay out at `x_base = MARGIN` and content width `client - 2*MARGIN`.
const MARGIN: f32 = 16.0;

/// HWND of the singleton report MDI child, when one is open.
static DOC_HWND: Mutex<Option<isize>> = Mutex::new(None);

// ─── Process-wide document state ────────────────────────────────────

struct DocState {
    /// The current report, pre-parsed into `docpane` blocks. Re-laid-out
    /// per paint at the live width (layout is width-dependent; parse is
    /// not, so we parse once here and only re-flow on resize/paint).
    blocks: Vec<Block>,
}

/// Process-wide singleton. `None` until the first [`set_report`] (or the
/// first paint, which falls back to the welcome report).
static DOC: Mutex<Option<DocState>> = Mutex::new(None);

fn with_doc<R>(f: impl FnOnce(&mut DocState) -> R) -> R {
    let mut guard = DOC.lock().expect("DOC poisoned");
    let state = guard.get_or_insert_with(|| DocState {
        blocks: parse(WELCOME_REPORT),
    });
    f(state)
}

/// Replace the report shown in the pane. Safe from any thread (the IDE
/// worker calls this after an eval). Parses the markdown now; the open
/// pane (if any) re-lays-it-out and repaints. A no-op visually when the
/// pane is closed — the content is remembered and shown when next opened.
pub fn set_report(markdown: &str) {
    let blocks = parse(markdown);
    with_doc(|state| state.blocks = blocks);
    request_repaint();
}

/// Set a report **and** bring the pane up. Posts the open command to the
/// frame so the (GUI-thread-only) window creation happens on the right
/// thread; safe to call from the IDE worker. Uses the published frame
/// HWND, so it works without the caller threading one through.
pub fn show_report(markdown: &str) {
    set_report(markdown);
    if let Some(&hwnd_isize) = super::cp_exports::FRAME_HWND.get() {
        let hwnd = HWND(hwnd_isize as *mut _);
        let _ = unsafe {
            PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(MENU_CMD_ID as usize), LPARAM(0))
        };
    }
}

fn request_repaint() {
    let raw = match DOC_HWND.lock() {
        Ok(g) => *g,
        Err(_) => return,
    };
    if let Some(r) = raw {
        let hwnd = HWND(r as *mut _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        }
    }
}

// ─── Window class registration & open ───────────────────────────────

pub fn register_class() -> Result<(), super::IGuiError> {
    // Bring up docpane's Direct2D / DirectWrite factories once. Idempotent;
    // first caller wins. Done here (startup) so the first paint/measure is
    // ready without a lazy check on the hot path.
    if let Err(e) = render::init() {
        return Err(super::IGuiError::D2D(format!("docpane::render::init: {e}")));
    }
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| super::IGuiError::Win32(format!("GetModuleHandleW (doc_pane): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| super::IGuiError::Win32(format!("LoadCursorW (doc_pane): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(doc_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: unsafe { super::window::app_icon() },
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: DOC_CLASS,
        hIconSm: unsafe { super::window::app_icon() },
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

/// Open the report view (or activate it if already open). UI thread.
pub fn open(_frame: HWND, mdi_client: HWND) {
    if let Some(raw) = *DOC_HWND.lock().expect("DOC_HWND poisoned") {
        let hwnd = HWND(raw as *mut _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            unsafe {
                SendMessageW(
                    mdi_client,
                    WM_MDIACTIVATE,
                    Some(WPARAM(hwnd.0 as usize)),
                    Some(LPARAM(0)),
                )
            };
            let _ = unsafe { BringWindowToTop(hwnd) };
            return;
        }
    }

    let h_instance = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => windows::Win32::Foundation::HANDLE(h.0),
        Err(e) => {
            eprintln!("[doc_pane] GetModuleHandleW: {e}");
            return;
        }
    };

    // Default position: a wide column on the right (reports read like a
    // page; give them room). The user can drag/resize after.
    let mut client_rect = RECT::default();
    let _ = unsafe { GetClientRect(mdi_client, &mut client_rect) };
    let w_full = (client_rect.right - client_rect.left).max(400);
    let h_full = (client_rect.bottom - client_rect.top).max(200);
    let width = (w_full * 5 / 10).max(420);
    let x = (w_full - width).max(0);

    let create = MDICREATESTRUCTW {
        szClass: DOC_CLASS,
        szTitle: DOC_TITLE,
        hOwner: h_instance,
        x,
        y: 0,
        cx: width,
        cy: h_full,
        style: WS_VISIBLE | WS_OVERLAPPEDWINDOW,
        lParam: LPARAM(0),
    };
    let result = unsafe {
        SendMessageW(
            mdi_client,
            WM_MDICREATE,
            Some(WPARAM(0)),
            Some(LPARAM(&create as *const _ as isize)),
        )
    };
    if result.0 == 0 {
        eprintln!("[doc_pane] WM_MDICREATE returned 0");
        let _ = CW_USEDEFAULT;
    }
}

// ─── Per-window state ───────────────────────────────────────────────

struct DocWindowState {
    hwnd: HWND,
    target: Option<ID2D1HwndRenderTarget>,
    /// Vertical scroll, in DIPs (0 = top of the document).
    scroll_y: f32,
    /// Total document height in DIPs from the last layout — clamps scroll.
    content_h: f32,
    client_w: u32,
    client_h: u32,
    dpi: u32,
}

impl DocWindowState {
    fn new(hwnd: HWND) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let dpi = if dpi == 0 { 96 } else { dpi };
        Self {
            hwnd,
            target: None,
            scroll_y: 0.0,
            content_h: 0.0,
            client_w: 0,
            client_h: 0,
            dpi,
        }
    }

    /// 96-per-DIP scale: device-pixels × `dip_scale` = DIPs.
    fn dip_scale(&self) -> f32 {
        if self.dpi == 0 {
            1.0
        } else {
            96.0 / (self.dpi as f32)
        }
    }

    fn invalidate(&self) {
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    fn ensure_target(&mut self, w: u32, h: u32) {
        if let Some(target) = self.target.as_ref() {
            let cur = unsafe { target.GetPixelSize() };
            if cur.width != w || cur.height != h {
                let _ = unsafe { target.Resize(&D2D_SIZE_U { width: w, height: h }) };
            }
            return;
        }
        // Create the target from docpane's factory so Mermaid geometry
        // (built on that same factory) composites without a cross-factory
        // fault. ID2D1Factory1 derefs to ID2D1Factory for this call.
        let factory = render::factory();
        let dpi = self.dpi as f32;
        let target = unsafe {
            factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: dpi,
                    dpiY: dpi,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd: self.hwnd,
                    pixelSize: D2D_SIZE_U { width: w, height: h },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )
        };
        match target {
            Ok(t) => self.target = Some(t),
            Err(e) => eprintln!("[doc_pane] CreateHwndRenderTarget: {e}"),
        }
    }

    fn paint(&mut self) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut rect) }.is_err() {
            return;
        }
        let w = (rect.right - rect.left) as u32;
        let h = (rect.bottom - rect.top) as u32;
        if w == 0 || h == 0 {
            return;
        }
        self.client_w = w;
        self.client_h = h;
        self.ensure_target(w, h);
        let Some(target) = self.target.clone() else { return };

        let scale = self.dip_scale();
        let w_dip = (w as f32) * scale;
        let h_dip = (h as f32) * scale;
        let content_w = (w_dip - 2.0 * MARGIN).max(1.0);

        // Lay out the current report at the live width, under the lock,
        // then release before drawing (Direct2D calls can be slow).
        let ly: Layout = with_doc(|state| {
            layout(&state.blocks, MARGIN, content_w, MARGIN, render::measure_text)
        });
        self.content_h = ly.total_h;
        // Clamp scroll against the freshly-measured height.
        let max_scroll = (ly.total_h - h_dip).max(0.0);
        if self.scroll_y > max_scroll {
            self.scroll_y = max_scroll;
        }

        unsafe { target.BeginDraw() };
        unsafe { target.Clear(Some(&docpane::theme::hex(docpane::theme::BG))) };
        // `draw_document` issues only content draws; chrome (clear) is ours.
        // ID2D1HwndRenderTarget casts to the base ID2D1RenderTarget the
        // renderer takes.
        let rt: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget = &target;
        let _ = unsafe { render::draw_document(rt, &ly, self.scroll_y, h_dip) };
        let _ = unsafe { target.EndDraw(None, None) };
    }

    fn wheel(&mut self, raw_delta: i32) {
        if WHEEL_DELTA == 0 {
            return;
        }
        // One notch ≈ 3 lines ≈ 60 DIPs. Wheel up scrolls toward the top.
        let dips = (raw_delta as f32 / WHEEL_DELTA as f32) * 60.0;
        let h_dip = (self.client_h as f32) * self.dip_scale();
        let max_scroll = (self.content_h - h_dip).max(0.0);
        self.scroll_y = (self.scroll_y - dips).clamp(0.0, max_scroll);
        self.invalidate();
    }

    fn set_dpi(&mut self, dpi: u32) {
        if dpi == 0 || dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        self.target = None;
        self.invalidate();
    }
}

// ─── Win32 plumbing ─────────────────────────────────────────────────

unsafe extern "system" fn doc_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let state = Box::new(DocWindowState::new(hwnd));
        let raw = Box::into_raw(state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        if let Ok(mut slot) = DOC_HWND.lock() {
            *slot = Some(hwnd.0 as isize);
        }
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut DocWindowState;
    if state_ptr.is_null() {
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };

    match msg {
        WM_PAINT => {
            let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
            let _ = unsafe { windows::Win32::Graphics::Gdi::BeginPaint(hwnd, &mut ps) };
            state.paint();
            let _ = unsafe { windows::Win32::Graphics::Gdi::EndPaint(hwnd, &ps) };
            LRESULT(0)
        }
        WM_SIZE => {
            let w = (lparam.0 & 0xFFFF) as u32;
            let h = ((lparam.0 >> 16) & 0xFFFF) as u32;
            state.client_w = w;
            state.client_h = h;
            if let Some(target) = state.target.as_ref() {
                let _ = unsafe { target.Resize(&D2D_SIZE_U { width: w, height: h }) };
            }
            state.invalidate();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_LBUTTONDOWN => {
            let _ = unsafe { SetFocus(Some(hwnd)) };
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let raw = ((wparam.0 >> 16) & 0xFFFF) as i16;
            state.wheel(raw as i32);
            LRESULT(0)
        }
        WM_SETFOCUS | WM_MDIACTIVATE => {
            state.invalidate();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DPICHANGED_AFTERPARENT => {
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            if dpi != 0 {
                state.set_dpi(dpi);
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_NCDESTROY => {
            let _ = unsafe { Box::from_raw(state_ptr) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            if let Ok(mut slot) = DOC_HWND.lock() {
                if matches!(*slot, Some(h) if h == hwnd.0 as isize) {
                    *slot = None;
                }
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The default report shown before the IDE pushes one — a short tour of
/// what this pane is, including a Mermaid diagram so the renderer's whole
/// surface (headings, lists, code, tables, diagrams) is exercised the
/// moment the pane opens.
const WELCOME_REPORT: &str = r#"# Reports

This is the **report pane**. The IDE renders reports here as Markdown with
Mermaid diagrams — for example, an *effect / capability report* after you
run a buffer (`F5`), showing the powers a program named and how they reach
the world.

## What a report can hold

- Headings, **bold**, *italic*, and `inline code`
- Bullet and numbered lists
- Fenced code blocks and tables
- Mermaid diagrams

## The Locus capability flow

```mermaid
flowchart LR
  P[Locus program] --> S[Sealed services]
  S --> B[Boundary / minted labels]
  B --> W[World: OS / IDE]
```

Run a program and its effect report appears here.
"#;
