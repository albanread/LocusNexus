//! Service plugins (compile-time, platform-team-owned). The support layer is
//! [`locus_plugin`]; each plugin crate (e.g. `locus-sqlite`) wraps a Rust crate
//! behind a sealed Locus effect.
//!
//! The driver grafts their `.locus` modules alongside the stdlib and injects
//! their C-ABI symbols into the JIT. These helpers live in the **library** (not
//! just the `locusc` binary) so every front end that needs the plugin surface —
//! the compiler driver, and the IDE's analysis / report path — grafts the same
//! module set and resolves the same plugin symbols. See
//! `docs/design/rust-service-plugins.md`.

/// Every compiled-in service plugin.
pub fn service_plugins() -> Vec<locus_plugin::ServicePlugin> {
    vec![locus_sqlite::plugin()]
}

/// Stdlib modules + every plugin's modules, for the graft. A plugin's modules
/// auto-include only when a program names one of its exposed surface (the same
/// fixpoint the stdlib uses).
pub fn plugin_grafted_modules() -> Vec<(u8, &'static str, &'static str)> {
    let mut m: Vec<(u8, &'static str, &'static str)> = locus::stdlib_modules().to_vec();
    for p in service_plugins() {
        m.extend(p.modules);
    }
    m
}

/// The parsed module **declarations** for [`plugin_grafted_modules`] — stdlib +
/// plugin headers (layer / mints / seals / exposing). The IDE analysis reads
/// these to attribute every grafted function and effect to its layer, so plugin
/// surfaces like `db_open_memory` show their `services`/`boundary` provenance.
pub fn plugin_grafted_module_decls() -> Vec<locus::ModuleDecl> {
    locus::stdlib_module_decls_from(&plugin_grafted_modules())
}

/// Every plugin's C-ABI symbols (name → address), for the JIT's absolute-symbol
/// manifold (passed to `jit_run_i64_with_symbols`).
pub fn plugin_symbols() -> Vec<(String, u64)> {
    let mut s = Vec::new();
    for p in service_plugins() {
        for (name, addr) in p.symbols {
            s.push((name.to_string(), addr));
        }
    }
    s
}
