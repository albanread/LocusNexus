//! The runtime's JIT symbol table. The actual `extern "C"` shims (the `gc`
//! effect's handler) live in the `locus-rt` crate, which is also built as a
//! `staticlib` for the AOT linker. Depending on it as an `rlib` links those
//! shims into `locusc`, so the JIT can resolve their addresses here.

pub use locus_rt::runtime_symbols;
