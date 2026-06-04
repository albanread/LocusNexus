//! The runtime prelude — **native effects backed by prelowered runtime
//! functions**.
//!
//! A native effect (e.g. `console`) is an ordinary, fully **interceptable**
//! effect operation: a user `handle … with { console(s) => … }` overrides it,
//! and interception lives entirely in the language — *outside* the runtime.
//! What makes it *native* is its **default** handler: a **prelowered** Rust
//! function the JIT already knows how to call (compiled machine code, not Locus
//! source awaiting lowering).
//!
//! The compiler resolves every `perform`:
//!   * an enclosing handler intercepts it ⇒ route there (a front-end decision,
//!     possibly eliminated to zero cost — `calculus.md` §5.2); else
//!   * nothing intercepts ⇒ emit a direct call to the prelowered runtime
//!     function ("the JIT knows how to call it if the compiler allows it").
//!
//! So `World` labels are the native IO surface — they have a runtime default;
//! `User` effects do not — left unhandled, they *escape* to the caller.

use crate::check::Sig;
use crate::syntax::{Label, OpSig, Type};

/// The operation names the runtime provides a **prelowered default** for —
/// the IO surface. (Canonicalised to `World` labels by [`op_label`].)
pub const NATIVE_OPS: &[&str] = &[
    "console",
    "console_float",
    "read_line",
    "fs",
    "net",
    "clock",
];

/// The canonical [`Label`] for an operation name parsed from source: a
/// **native** (`World`) label for the runtime-backed ops, else a plain `User`
/// label. This is the one place "is this name native?" is decided.
pub fn op_label(name: &str) -> Label {
    if NATIVE_OPS.contains(&name) {
        Label::World(name.to_string())
    } else {
        Label::User(name.to_string())
    }
}

/// The default operation signatures `Σ` — the native ops' parameter/result
/// types, i.e. the ABIs of the prelowered runtime functions. Loaded by the CLI
/// so a program can call the natives without declaring them.
///
/// (`writeln`/`print` are intended as stdlib sugar over `perform console`;
/// that surface belongs in the in-language library, not hard-coded here.)
pub fn sig() -> Sig {
    Sig::from([
        // console : String => Unit   — write a line to the terminal.
        (
            Label::World("console".into()),
            OpSig {
                param: Type::Str,
                result: Type::Unit,
            },
        ),
        (
            Label::World("console_float".into()),
            OpSig {
                param: Type::Float,
                result: Type::Unit,
            },
        ),
        // read_line : Unit => String — read a line from the terminal.
        (
            Label::World("read_line".into()),
            OpSig {
                param: Type::Unit,
                result: Type::Str,
            },
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_names_become_world_labels() {
        assert_eq!(op_label("console"), Label::World("console".into()));
        assert!(op_label("console").is_native());
        assert_eq!(
            op_label("console_float"),
            Label::World("console_float".into())
        );
        assert!(op_label("console_float").is_native());
    }

    #[test]
    fn unknown_names_are_user_effects() {
        assert_eq!(op_label("ask"), Label::User("ask".into()));
        assert!(!op_label("ask").is_native());
    }

    #[test]
    fn the_sig_types_console_as_string_to_unit() {
        let s = sig();
        let op = s.get(&Label::World("console".into())).unwrap();
        assert_eq!(op.param, Type::Str);
        assert_eq!(op.result, Type::Unit);

        let op = s.get(&Label::World("console_float".into())).unwrap();
        assert_eq!(op.param, Type::Float);
        assert_eq!(op.result, Type::Unit);
    }
}
