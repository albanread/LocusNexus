//! Capability gating — the `RN-E04xx` "the world / capabilities" family
//! ([`sealing-plan.md`] S2/S4, [`capabilities.md`]).
//!
//! The model rests on one split (see [`crate::syntax::Layer`]):
//!
//! - **Mint** — conjure a raw capability from the outside world (`extern`,
//!   `extern asm`, a foreign-module bind). The *only* privileged act; it is where
//!   authority enters the system, so it is **`boundary`-only and manifest-gated**.
//!   This module's [`mint_gate`] enforces that.
//! - **Seal** — assert a label does not escape an export edge. **Not** privileged:
//!   every layer may seal what it consumes (the region form is already enforced as
//!   `RN-E0403`; the module `seals (…)` clause is S4).
//!
//! Separating the two is what lets a *user* module create a feature (`effect …`)
//! and seal it above the services without holding any mint power.

use crate::check::TypeErr;
use crate::sema::{seal_escape, Node, Typed};
use crate::stdlib::{bound_names, first_mint};
use crate::syntax::{ModuleDecl, Term, Type};
use std::collections::{HashMap, HashSet};

/// A capability-gate violation (the mint-gate half; the seal half reuses
/// [`crate::check::TypeErr::SealLeak`] = `RN-E0403`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CapError {
    /// `RN-E0402` — a mint (`extern` / raw memory `peek`/`poke`/`fill`/`copy` /
    /// the coming `extern asm` / foreign-bind) appears outside an authorized
    /// `boundary` module: in app code (`module` is `None`, the program entry) or
    /// in a non-boundary module. `what` describes the mint.
    MintOutsideBoundary {
        module: Option<String>,
        what: String,
    },
    /// `RN-E0404` — a module declares `at boundary` (claiming mint authority) but
    /// is not authorized by the project manifest (`locus.toml [boundary]`).
    UnauthorizedBoundary { module: String },
}

impl CapError {
    /// The stable `RN-Exxxx` diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            CapError::MintOutsideBoundary { .. } => "RN-E0402",
            CapError::UnauthorizedBoundary { .. } => "RN-E0404",
        }
    }

    /// The catalog slug.
    pub fn slug(&self) -> &'static str {
        match self {
            CapError::MintOutsideBoundary { .. } => "cap.mint-outside-boundary",
            CapError::UnauthorizedBoundary { .. } => "cap.unauthorized-boundary",
        }
    }

    /// The law this enforces (spec-citing, design §8).
    pub fn spec(&self) -> &'static str {
        "capabilities (mint rule) / sealing-plan §S2"
    }
}

impl std::fmt::Display for CapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapError::MintOutsideBoundary { module: None, what } => write!(
                f,
                "{what} reaches the raw boundary in app code — minting (the FFI / raw-memory / \
                 asm edge) is allowed only inside a manifested `at boundary` module; reach the \
                 boundary through a sealed service effect instead"
            ),
            CapError::MintOutsideBoundary {
                module: Some(m),
                what,
            } => write!(
                f,
                "{what} reaches the raw boundary in module `{m}`, which is not `at boundary` — \
                 only boundary modules may mint; move it into one"
            ),
            CapError::UnauthorizedBoundary { module } => write!(
                f,
                "module `{module}` declares `at boundary` (mint authority) but is not authorized \
                 by the project manifest — list it under `locus.toml [boundary] modules` to \
                 designate it part of the trusted base"
            ),
        }
    }
}

/// The **mint-gate** ([`sealing-plan.md`] S2): `extern` / `extern asm` /
/// foreign-bind may appear **only** inside a `boundary` module the manifest
/// authorizes. Checked structurally, before elaboration.
///
/// - `entry` — the program entry (app code); any mint here is `RN-E0402`.
/// - `modules` — the user-declared modules (the bundled stdlib is trusted by
///   construction and not passed here).
/// - `authorized` — the names of `boundary` modules the manifest blesses
///   (`locus.toml [boundary] modules`). An `at boundary` module **not** in this
///   set is `RN-E0404`, whether or not it actually mints — the `at boundary`
///   claim *is* the privilege.
///
/// Returns the first violation found, or `Ok(())`.
pub fn mint_gate(
    entry: &Term,
    modules: &[ModuleDecl],
    authorized: &HashSet<String>,
) -> Result<(), CapError> {
    if let Some(what) = first_mint(entry) {
        return Err(CapError::MintOutsideBoundary { module: None, what });
    }
    for m in modules {
        if m.layer.can_mint() {
            if !authorized.contains(&m.name) {
                return Err(CapError::UnauthorizedBoundary {
                    module: m.name.clone(),
                });
            }
            // An authorized boundary module: minting is its job. OK.
        } else if let Some(what) = first_mint(&m.body) {
            return Err(CapError::MintOutsideBoundary {
                module: Some(m.name.clone()),
                what,
            });
        }
    }
    Ok(())
}

/// Collect the **type of every top-level binding** in an elaborated program —
/// the `let` / `let rec` bindings of the grafted module chain, *including* those
/// inside a service module's `handle … with { … }` wrap (the console seal). Local
/// bindings inside a value (a lambda body) are not module exports and are skipped.
fn binding_types(t: &Typed, out: &mut HashMap<String, Type>) {
    match &t.node {
        Node::Let { name, bound, body } => {
            // First (outermost) binding of a name wins — modules graft outermost,
            // so a module's own export is found before any inner shadow.
            out.entry(name.clone()).or_insert_with(|| bound.ty.clone());
            binding_types(body, out);
        }
        Node::Handle { scrutinee, .. } => binding_types(scrutinee, out),
        // A `let mut` binds a *local* mutable cell, not a module export — do not
        // register its name, but recurse into the body where the real top-level
        // bindings of the grafted chain live (a non-exporting node, like a lambda).
        Node::LetMut { body, .. } => binding_types(body, out),
        _ => {}
    }
}

/// Enforce every module's **`seals (…)` clause** ([`sealing-plan.md`] S4): no
/// binding a module *exposes* may carry a sealed label anywhere in its type. This
/// is the *seal* half of the mint/seal split — the kernel/service export boundary
/// and the user's "create a feature and seal it" edge, both checked by the one
/// [`seal_escape`] predicate the region `seal` already uses.
///
/// Runs over the **elaborated** program `typed` (every binding has its type) and
/// the `modules` that were grafted into it (the included stdlib modules plus the
/// user modules). A module with no `seals` is skipped; an exposed name not present
/// in `typed` (its module was not grafted) is skipped. Returns the first leak as
/// `RN-E0403`.
pub fn check_module_seals(modules: &[ModuleDecl], typed: &Typed) -> Result<(), TypeErr> {
    let mut types = HashMap::new();
    binding_types(typed, &mut types);
    for m in modules {
        if m.seals.is_empty() {
            continue;
        }
        // The names this module exposes — its `exposing (…)` list, or every name
        // it binds when the clause is omitted.
        let exposed: Vec<String> = match &m.exposing {
            Some(names) => names.clone(),
            None => bound_names(&m.body).into_iter().collect(),
        };
        for name in &exposed {
            let Some(ty) = types.get(name) else {
                continue; // not grafted into this program — nothing to check
            };
            for label in &m.seals {
                if let Some(escaping) = seal_escape(label, ty) {
                    return Err(TypeErr::ModuleSealLeak {
                        module: m.name.clone(),
                        binding: name.clone(),
                        label: label.clone(),
                        ty: escaping,
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_program;
    use std::collections::HashSet;

    fn gate(src: &str, authorized: &[&str]) -> Result<(), CapError> {
        let prog = parse_program(src).expect("test source parses");
        let auth: HashSet<String> = authorized.iter().map(|s| s.to_string()).collect();
        mint_gate(&prog.entry, &prog.modules, &auth)
    }

    #[test]
    fn pure_app_code_passes() {
        assert!(gate("let x = 1 in x", &[]).is_ok());
    }

    #[test]
    fn an_app_level_extern_is_a_mint_outside_boundary() {
        let err = gate(r#"let h = extern "GetStdHandle" : U32 -> Ptr in h 0"#, &[])
            .expect_err("app code may not mint");
        assert_eq!(err.code(), "RN-E0402");
        assert!(matches!(
            err,
            CapError::MintOutsideBoundary { module: None, .. }
        ));
    }

    #[test]
    fn an_extern_in_a_non_boundary_module_is_rejected() {
        let err = gate(
            r#"module Sneaky at services = let h = extern "WriteFile" : Ptr -> I32 in () 0"#,
            &[],
        )
        .expect_err("a services module may not mint");
        assert_eq!(err.code(), "RN-E0402");
        assert!(matches!(
            err,
            CapError::MintOutsideBoundary { module: Some(ref m), .. } if m == "Sneaky"
        ));
    }

    #[test]
    fn raw_memory_in_a_non_boundary_module_is_a_mint() {
        // Raw `poke`/`peek` is a boundary power, so a `services` module using it
        // directly is `RN-E0402` — even though it names no `extern`.
        let err = gate(
            "module Sneaky at services = let f = fn a: Int => poke8 a 65 in () f 1024",
            &[],
        )
        .expect_err("raw memory is boundary-only");
        assert_eq!(err.code(), "RN-E0402");
        assert!(matches!(
            err,
            CapError::MintOutsideBoundary { module: Some(ref m), .. } if m == "Sneaky"
        ));
    }

    #[test]
    fn the_array_accessor_is_not_a_mint() {
        // `a[i] <- v` desugars to memory, but it is the safe bounds-checked
        // surface — app/services code uses it freely.
        assert!(gate(
            "module Svc at services = let f = fn a: Array[Int] => (let _ = a[0] <- 7 in a[0]) in () \
             f ([1, 2])",
            &[],
        )
        .is_ok());
    }

    #[test]
    fn an_unauthorized_boundary_module_is_rejected() {
        let err = gate(
            r#"module Mine at boundary = let h = extern "Foo" : U32 -> Ptr in () 0"#,
            &[],
        )
        .expect_err("an unmanifested boundary module is rejected");
        assert_eq!(err.code(), "RN-E0404");
        assert!(matches!(
            err,
            CapError::UnauthorizedBoundary { ref module } if module == "Mine"
        ));
    }

    #[test]
    fn an_authorized_boundary_module_may_mint() {
        assert!(gate(
            r#"module Mine at boundary = let h = extern "Foo" : U32 -> Ptr in () 0"#,
            &["Mine"],
        )
        .is_ok());
    }

    #[test]
    fn even_a_non_minting_boundary_module_needs_authorization() {
        // The `at boundary` claim is itself the privilege.
        let err = gate("module Empty at boundary = () 0", &[])
            .expect_err("a boundary claim needs manifest backing");
        assert_eq!(err.code(), "RN-E0404");
    }

    // ── the seal half: the module `seals (…)` clause (S4) ───────────────

    fn seals_check(src: &str) -> Result<(), TypeErr> {
        let (term, user_mods) =
            crate::stdlib::program_with_modules(src).expect("test source parses");
        let tree =
            crate::sema::elaborate(&crate::prelude::sig(), &crate::check::Ctx::new(), 0, &term)
                .expect("test source elaborates");
        let mut all = crate::stdlib::stdlib_module_decls();
        all.extend(user_mods);
        check_module_seals(&all, &tree)
    }

    #[test]
    fn the_real_console_seal_holds() {
        // Console `seals (winapi)`, and its exported `writeln` carries `console`,
        // not `winapi` — the kernel export boundary is honoured.
        seals_check(r#"writeln "hi""#).expect("console's winapi seal holds");
    }

    #[test]
    fn a_module_exposing_the_sealed_label_is_rejected() {
        // `Bad` seals `mem` but exposes `leak`, whose type `Int -> Unit ! {mem}`
        // still carries it — the seal does not hold.
        let err = seals_check(
            "module Bad at app seals (mem) = \
               let leak = fn a: Int => poke8 a 65 in () \
             leak 1024",
        )
        .expect_err("a binding carrying the sealed label leaks");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::ModuleSealLeak {
                ref module, ref binding, label: crate::syntax::Label::World(ref n), ..
            } if module == "Bad" && binding == "leak" && n == "mem"
        ));
    }

    #[test]
    fn a_user_module_sealing_a_label_its_exports_do_not_carry_passes() {
        // `Ok` seals `mem` but exposes only a pure binding — no leak.
        seals_check(
            "module Ok at app seals (mem) = \
               let safe = fn x: Int => x + 1 in () \
             safe 5",
        )
        .expect("nothing exposed carries the sealed label");
    }

    // ── asm is gated by the SAME two seals as every mint (MASM port A3) ──────
    //
    // `extern asm` is a mint (it conjures Layer-0 machine code), so it is bound by
    // both halves of the mint/seal split, exactly like `extern` (winapi/crt):
    //   1. the mint-gate — raw asm is `boundary`-only (RN-E0402), and
    //   2. the seal-leak check — a raw `{asm}` arrow cannot be laundered through a
    //      `seals (asm)` clause (RN-E0403); the seal forces the Console-style
    //      effect discharge, it does NOT silently strip the label.

    #[test]
    fn app_level_extern_asm_is_a_mint_outside_boundary() {
        // The owner's guarantee: app code cannot reach the raw asm edge. Calling an
        // `extern asm` directly in the program entry is RN-E0402, just like a raw
        // `extern` — the `asm` capability is minted only inside an authorized
        // boundary module.
        let err = gate(
            r#"let go = extern asm "locus_asm_hello" : Int -> Int in go 0"#,
            &[],
        )
        .expect_err("app code may not mint asm");
        assert_eq!(err.code(), "RN-E0402");
        assert!(matches!(
            err,
            CapError::MintOutsideBoundary { module: None, .. }
        ));
    }

    #[test]
    fn a_raw_asm_wrapper_cannot_be_laundered_through_a_seal() {
        // A boundary module mints `asm` (its job) and tries to EXPOSE a wrapper that
        // still carries `{asm}` while sealing it. The seal is a *leak check*, not a
        // strip: a pure `rotl : Int -> Int -> Int ! {asm}` arrow cannot be hidden
        // this way (RN-E0403). To expose asm to the app you must discharge it behind
        // an effect (the Console/winapi pattern), so the exported binding carries the
        // service effect, never the raw boundary label. This is the design, not a
        // gap — it is why the journal's first A3 sketch (`seals (asm) exposing
        // (rotl)` over a raw wrapper) is intentionally rejected.
        let err = seals_check(
            r#"module Bits at boundary mints (asm) seals (asm) =
                 let rotl = extern asm "locus_asm_rotl64" : Int -> Int -> Int in ()
               rotl 1 4"#,
        )
        .expect_err("a raw asm wrapper carries {asm} and cannot be sealed away");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::ModuleSealLeak {
                ref module, label: crate::syntax::Label::World(ref n), ..
            } if module == "Bits" && n == "asm"
        ));
    }

    // ── adversarial: depth, deep-escape, and manifest membership ─────────

    #[test]
    fn the_mint_gate_reaches_a_nested_extern_in_app_code() {
        // first_extern walks the whole term: a mint buried inside a lambda body
        // is still RN-E0402 — you can't hide a raw power in a closure.
        let err = gate(
            r#"let f = fn x: Int => (let h = extern "WriteFile" : Ptr -> I32 in 0) in f 0"#,
            &[],
        )
        .expect_err("a nested extern in app code is still a mint");
        assert_eq!(err.code(), "RN-E0402");
        assert!(matches!(err, CapError::MintOutsideBoundary { module: None, .. }));
    }

    #[test]
    fn a_module_seal_catches_a_label_hidden_in_a_returned_closure() {
        // The runST-style deep no-escape: `mem` rides out in the LATENT row of a
        // returned closure (not the top row) — the seal must still catch it.
        let err = seals_check(
            "module Bad at app seals (mem) = \
               let leak = fn x: Int => fn y: Int => poke8 x y in () \
             leak 1 2",
        )
        .expect_err("a sealed label hidden in a returned closure leaks");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::ModuleSealLeak {
                ref module, ref binding, label: crate::syntax::Label::World(ref n), ..
            } if module == "Bad" && binding == "leak" && n == "mem"
        ));
    }

    #[test]
    fn a_module_seal_catches_a_label_hidden_in_a_tuple() {
        // Deep escape through DATA: the sealed label sits inside a tuple element's
        // function type. seal_escape recurses structurally, so this is caught too.
        let err = seals_check(
            "module Bad at app seals (mem) = \
               let leak = fn x: Int => (fn y: Int => poke8 x y, 0) in () \
             leak 1",
        )
        .expect_err("a sealed label hidden in a tuple leaks");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::ModuleSealLeak {
                ref module, ref binding, label: crate::syntax::Label::World(ref n), ..
            } if module == "Bad" && binding == "leak" && n == "mem"
        ));
    }

    #[test]
    fn an_unauthorized_boundary_is_rejected_against_a_nonempty_manifest() {
        // The check is name-membership, not just "is the authorized set empty":
        // a boundary module absent from a populated manifest is still RN-E0404.
        let err = gate(
            r#"module Mine at boundary = let h = extern "Foo" : U32 -> Ptr in () 0"#,
            &["Other"],
        )
        .expect_err("an unmanifested boundary module is rejected even with a non-empty manifest");
        assert_eq!(err.code(), "RN-E0404");
    }
}
