//! Support layer for Rust **service plugins** — the host side of wrapping a Rust
//! crate (serde_json, rusqlite, quick-xml, …) as a sealed Locus service.
//!
//! Design: `locusnexus docs/design/rust-service-plugins.md`. A plugin is a crate
//! that depends on this one and provides: an `extern "C"` shim using [`Registry`]
//! + the marshalling helpers here, a boundary `.locus` that `mints` a raw effect,
//! a service `.locus` that `seals` it, and a [`ServicePlugin`] descriptor the
//! worker collects through the central grant list.
//!
//! ABI conventions (the C-ABI carries `i64`/`f64`/`Ptr` only):
//!   * rich values are **host-owned** in a [`Registry<T>`], shown to Locus as an
//!     opaque `i64` handle (`>0`; `0` = "no value" — the service decides whether
//!     that means `None` or `Err`);
//!   * strings cross **in** as a Locus-marshalled NUL-terminated UTF-8 pointer
//!     ([`cstr_in`]) and **out** as a fresh Locus `String` handle ([`string_out`]);
//!   * fallible ops set [`set_last_error`]; the service reads it to build `Result`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Mutex;

/// A handle registry: the host owns each `T`, Locus holds an opaque `i64` handle
/// (`> 0`). Construct as a `static` with [`Registry::new`] (const). Handles are
/// scalars, so they are GC-blind — they satisfy the boundary's GC rule for free.
pub struct Registry<T> {
    inner: Mutex<Option<Inner<T>>>,
}

struct Inner<T> {
    next: i64,
    map: HashMap<i64, T>,
}

impl<T> Registry<T> {
    /// A fresh, empty registry. `const` so it can back a `static`.
    pub const fn new() -> Self {
        Registry { inner: Mutex::new(None) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Inner<T>>> {
        // A plugin op must not panic while holding the lock; recover poisoning
        // defensively rather than cascade.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Store `value`, returning a fresh handle (`> 0`, monotonic).
    pub fn insert(&self, value: T) -> i64 {
        let mut g = self.lock();
        let inner = g.get_or_insert_with(|| Inner {
            next: 1,
            map: HashMap::new(),
        });
        let h = inner.next;
        inner.next += 1;
        inner.map.insert(h, value);
        h
    }

    /// Borrow the `T` for `handle` and run `f`. `None` if the handle is unknown.
    pub fn with<R>(&self, handle: i64, f: impl FnOnce(&T) -> R) -> Option<R> {
        let g = self.lock();
        g.as_ref().and_then(|i| i.map.get(&handle)).map(f)
    }

    /// Mutably borrow the `T` for `handle` and run `f`.
    pub fn with_mut<R>(&self, handle: i64, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mut g = self.lock();
        g.as_mut().and_then(|i| i.map.get_mut(&handle)).map(f)
    }

    /// Drop the `T` for `handle`. Idempotent — double-free-safe. Returns whether
    /// a value was present.
    pub fn remove(&self, handle: i64) -> bool {
        let mut g = self.lock();
        g.as_mut()
            .map(|i| i.map.remove(&handle).is_some())
            .unwrap_or(false)
    }

    /// Number of live handles (diagnostics / leak checks).
    pub fn len(&self) -> usize {
        self.lock().as_ref().map(|i| i.map.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ── per-thread error cell (errno-style; worker calls are synchronous) ──────────

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Record the error message for the current op. The shim returns `0` and the
/// **service** reads it (via the plugin's `*.LastError` extern) to build `Err`.
pub fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(msg.into()));
}

/// The last error message on this thread, if any (does not clear it).
pub fn last_error() -> Option<String> {
    LAST_ERROR.with(|e| e.borrow().clone())
}

/// Take (and clear) the last error message on this thread.
pub fn take_last_error() -> Option<String> {
    LAST_ERROR.with(|e| e.borrow_mut().take())
}

// ── string marshalling (centralized; plugins never hand-roll UTF-8) ────────────

extern "C" {
    // Provided by the runtime (`locus-rt`), linked into the worker.
    fn locus_string_from_utf8(ptr: *const u8, len: i64) -> i64;
}

/// Build a Locus `String` from UTF-8 `bytes`; returns its handle. The outbound
/// direction (Rust → Locus), e.g. a text column read from a database.
pub fn string_out(bytes: &[u8]) -> i64 {
    unsafe { locus_string_from_utf8(bytes.as_ptr(), bytes.len() as i64) }
}

/// View a Locus-marshalled NUL-terminated UTF-8 pointer (from the runtime's
/// `locus_string_to_cstr`, passed by the service) as `&str`. `""` if null or not
/// valid UTF-8. The inbound direction (Locus → Rust), e.g. a SQL query string.
///
/// # Safety
/// `ptr` must be a valid NUL-terminated buffer for the duration of the call (the
/// service frees it after the extern returns).
pub unsafe fn cstr_in<'a>(ptr: i64) -> &'a str {
    if ptr == 0 {
        return "";
    }
    CStr::from_ptr(ptr as *const std::os::raw::c_char)
        .to_str()
        .unwrap_or("")
}

// ── the plugin descriptor (collected by the central grant list) ────────────────

/// `(layer, name, source)` of a `.locus` module — same shape as the compiler's
/// `stdlib::ModuleSource`, so the worker can graft a plugin's modules alongside
/// the stdlib. `layer`: 0 = boundary, 1 = services.
pub type ModuleSource = (u8, &'static str, &'static str);

/// One registered service plugin. The worker reads the grant list
/// (`service_plugins()`), grafts every plugin's [`modules`](ServicePlugin::modules),
/// and feeds every plugin's [`symbols`](ServicePlugin::symbols) to the JIT.
pub struct ServicePlugin {
    /// The capability effect(s) this plugin grants, as they appear in rows
    /// (`["json"]`, `["fs_read", "fs_write"]`). Documentation/audit — the labels
    /// themselves are `Label::World` and need no registration.
    pub effects: &'static [&'static str],
    /// The plugin's boundary + service `.locus` modules (`include_str!`'d).
    pub modules: Vec<ModuleSource>,
    /// The shim's C-ABI symbols: the `extern "name"` each boundary `extern`
    /// resolves to, paired with the function address (`f as usize as u64`).
    pub symbols: Vec<(&'static str, u64)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_insert_borrow_remove() {
        static R: Registry<String> = Registry::new();
        assert!(R.is_empty());
        let h = R.insert("hello".into());
        assert!(h > 0);
        assert_eq!(R.with(h, |s| s.len()), Some(5));
        assert_eq!(R.with(9999, |s| s.len()), None); // unknown handle
        assert_eq!(R.len(), 1);
        assert!(R.remove(h));
        assert!(!R.remove(h)); // idempotent / double-free-safe
        assert!(R.is_empty());
    }

    #[test]
    fn handles_are_distinct_and_monotonic() {
        static R: Registry<i32> = Registry::new();
        let a = R.insert(1);
        let b = R.insert(2);
        assert_ne!(a, b);
        assert!(b > a);
        R.with_mut(a, |v| *v += 40);
        assert_eq!(R.with(a, |v| *v), Some(41));
    }

    #[test]
    fn last_error_is_per_thread_and_takeable() {
        set_last_error("boom");
        assert_eq!(last_error().as_deref(), Some("boom"));
        assert_eq!(take_last_error().as_deref(), Some("boom"));
        assert_eq!(last_error(), None);
    }
}
