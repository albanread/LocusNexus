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
use crate::sema::{seal_escape, Node, Typed, TypedBlockItem};
use crate::stdlib::{bound_names, first_mint};
use crate::syntax::{Label, ModuleDecl, Term, Type};
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
    /// `RN-E0405` — a **level-visibility OUT-OF-LAYER** violation (D1,
    /// `level-enforcement.md` §1, Sprint 2/3). A module / the entry at layer `at`
    /// references the name `name`, but `name` is bound only at layer(s) the use
    /// site **cannot reach by layer**: resolution sees a name only at the **same**
    /// layer or **one below**, so an app naming a boundary binding (two-down) or
    /// any upward reference fails here. This is the symbol-visibility barrier that
    /// confines raw powers — the leak closed in Sprint 2 (an app naming a
    /// boundary-only `win_cred_read` is rejected here, not silently grafted).
    ///
    /// Distinct from [`CapError::LevelNotExposed`] (RN-E0406): there the name *is*
    /// bound at a reachable layer (`D`/`D-1`) but is private (not in `exposing`).
    /// Here no reachable layer binds it at all — a strictly geometric failure.
    ///
    /// `at` is the use-site layer rank (0 boundary / 1 services / 2 app);
    /// `defined_at` is the rank of the nearest layer that *does* bind `name`
    /// (for the diagnostic), or `None` when the only bindings are upward / fully
    /// out of reach.
    LevelOutOfLayer {
        name: String,
        at: u8,
        defined_at: Option<u8>,
    },
    /// `RN-E0406` — a **level-visibility NOT-EXPOSED** violation (D1, the privacy
    /// half). The name `name` **is** bound at a layer the use site `at` can reach
    /// (`D` or `D-1`), but that binding is **private** — it is not in its module's
    /// `exposing (…)` list. Cross-module resolution requires `exposed ∧ level ∈
    /// {D, D-1}`; the layer is fine, the privacy is the failure. Split out from
    /// RN-E0405 (Sprint 3) so each distinct check carries its own code: out-of-layer
    /// is geometric, not-exposed is an access-control decision the author made.
    ///
    /// `at` is the use-site layer rank; `defined_at` is the reachable layer that
    /// binds (but does not expose) `name`.
    LevelNotExposed {
        name: String,
        at: u8,
        defined_at: u8,
    },
    /// `RN-E0407` — a **non-sealable effect** was named in a `seals (…)` clause
    /// (or a `seal L { … }` region, `level-enforcement.md` §2.3 / `sealing-
    /// semantics.md` §8.3, the **inverted denylist**). `gc`, `exn`, and `Insert`
    /// are the caller's consent / fault / generativity signals and may **never** be
    /// sealed away: no handler discharges allocation, and a sealed-undischarged
    /// `exn` would *hide a fault* rather than encapsulate a serviced power. Sealing
    /// one is a hard error — the safety review found `is_native` silently *allowed*
    /// it, the wrong direction. (`st` stays sealable, routed through the existing
    /// `seal_escape` cell-escape check; native `World` powers — `winapi`/`mem`/
    /// `libc`/`crt`/`asm` — stay strippable.)
    NonSealableEffect {
        module: Option<String>,
        label: String,
    },
}

/// The **inverted denylist** ([`level-enforcement.md`] §2.3, [`sealing-
/// semantics.md`] §8.3): may `label` *never* be sealed? `gc`, `exn` (any
/// `exn[X]`, or a bare `exn` that surfaced as a `User` label), and the generative
/// `Insert` are the caller's consent / fault / generativity signals — sealing
/// them hides a fault or breaks let-insertion, so it is a hard `RN-E0407`.
///
/// Everything else stays sealable: native `World` powers (`winapi`/`mem`/`libc`/
/// `crt`/`asm`) are *stripped* subtract-only at the module-seal edge, and `st` is
/// routed through the existing [`seal_escape`] cell-escape check (so an *unhandled*
/// `st`/user effect still fails `SealUnhandled`, never silently strips).
pub fn is_never_sealable(label: &Label) -> bool {
    use crate::syntax::Label::*;
    match label {
        Gc | Exn(_) | Insert => true,
        // A bare `seals (exn)` / `seals (insert)` parses through `op_label`, which
        // makes them `User("exn")` / `User("insert")` (not the dedicated enum
        // variants) — catch those textual spellings too, so the denylist cannot be
        // dodged by writing the name without the `[X]` / capitalization.
        User(n) => n == "exn" || n == "insert",
        _ => false,
    }
}

/// The never-sealable denylist for the **region** form `seal L { e }` / `nogc`.
/// Like [`is_never_sealable`] *minus* `gc`: a region `seal gc` (≡ `nogc`) is the
/// sound `runST` discipline — it strips `gc` from the row **and** enforces the
/// gc-datum no-escape check ([`seal_escape`]), so allocation liability cannot be
/// laundered out of the region. (The unsound case is a *module* `seals (gc)`,
/// whose strip propagates to callers with no datum check — that stays RN-E0407 via
/// [`is_never_sealable`].) `exn`/`Insert` have no sound region form (a live `exn`
/// is already `SealUnhandled`; sealing a discharged one hides nothing but is
/// pointless and we reject it to keep the rule crisp).
pub fn is_never_region_sealable(label: &Label) -> bool {
    use crate::syntax::Label::*;
    match label {
        Exn(_) | Insert => true,
        User(n) => n == "exn" || n == "insert",
        _ => false,
    }
}

/// The surface name of a layer rank, for diagnostics (boundary=0/services=1/app=2).
fn layer_name(rank: u8) -> &'static str {
    match rank {
        0 => "boundary",
        1 => "services",
        2 => "app",
        _ => "?",
    }
}

impl CapError {
    /// The stable `RN-Exxxx` diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            CapError::MintOutsideBoundary { .. } => "RN-E0402",
            CapError::UnauthorizedBoundary { .. } => "RN-E0404",
            CapError::LevelOutOfLayer { .. } => "RN-E0405",
            CapError::LevelNotExposed { .. } => "RN-E0406",
            CapError::NonSealableEffect { .. } => "RN-E0407",
        }
    }

    /// The catalog slug.
    pub fn slug(&self) -> &'static str {
        match self {
            CapError::MintOutsideBoundary { .. } => "cap.mint-outside-boundary",
            CapError::UnauthorizedBoundary { .. } => "cap.unauthorized-boundary",
            CapError::LevelOutOfLayer { .. } => "capability.level-out-of-layer",
            CapError::LevelNotExposed { .. } => "capability.level-not-exposed",
            CapError::NonSealableEffect { .. } => "capability.non-sealable-effect",
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
            CapError::LevelOutOfLayer {
                name,
                at,
                defined_at,
            } => match defined_at {
                Some(d) => write!(
                    f,
                    "`{name}` is defined at layer {d} ({}) and is out of reach from layer {at} \
                     ({}): a layer resolves a name only at its own layer or one below. Reach it \
                     through a sealed service that exposes a capability for it, not by naming the \
                     raw binding.",
                    layer_name(*d),
                    layer_name(*at),
                ),
                None => write!(
                    f,
                    "`{name}` is out of reach from layer {at} ({}): a layer resolves a name only \
                     at its own layer or one below. Reach it through a sealed service that exposes \
                     a capability for it, not by naming the raw binding.",
                    layer_name(*at),
                ),
            },
            CapError::LevelNotExposed {
                name,
                at,
                defined_at,
            } => write!(
                f,
                "`{name}` is defined at layer {defined_at} ({}) — within reach of layer {at} \
                 ({}) — but is **private**: its module does not list it in `exposing (…)`. A \
                 cross-module reference resolves only an exposed name; expose it from its module, \
                 or reach it through a capability the module does expose.",
                layer_name(*defined_at),
                layer_name(*at),
            ),
            CapError::NonSealableEffect { module: None, label } => write!(
                f,
                "`{label}` may not be sealed: `gc`, `exn`, and `Insert` are the caller's \
                 consent / fault / generativity signals — no handler discharges allocation, and a \
                 sealed-undischarged `exn` would hide a fault rather than encapsulate a serviced \
                 power. Remove it from the `seal`."
            ),
            CapError::NonSealableEffect {
                module: Some(m),
                label,
            } => write!(
                f,
                "module `{m}` seals `{label}`, which may never be sealed: `gc`, `exn`, and \
                 `Insert` are the caller's consent / fault / generativity signals — no handler \
                 discharges allocation, and a sealed-undischarged `exn` would hide a fault rather \
                 than encapsulate a serviced power. Drop it from the module's `seals (…)` clause."
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
        Node::Block { items, body } => {
            for item in items {
                if let TypedBlockItem::Let { name, bound } = item {
                    out.entry(name.clone()).or_insert_with(|| bound.ty.clone());
                }
            }
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
    fn a_staged_extern_cannot_smuggle_past_the_gate() {
        // The mint-gate floor invariant (the one the safety review would not sign
        // off without): `first_mint` recurses into staging bodies, so an `extern`
        // hidden inside a `quote` in app code is still a mint and is rejected. There
        // is no combinator that synthesizes an `Extern` node from non-`extern`
        // source, so scanning source pre-stage is sufficient.
        let err = gate(
            r#"let sneaky = quote(extern "CredReadW" : Int -> Int) in 0"#,
            &[],
        )
        .expect_err("a staged/quoted extern in app code is still a mint");
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
        let src = src.to_string();
        std::thread::Builder::new()
            .name("capability-seals-check".into())
            .stack_size(crate::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let (term, user_mods) =
                    crate::stdlib::program_with_modules(&src).expect("test source parses");
                // Elaboration may itself reject a leak: the module-seal STRIP (D2)
                // wraps the grafted module in a `Term::Seal`, whose post-zonk
                // no-escape obligation (`check_seal_obligations`) catches a sealed
                // label escaping through the *result type* — also RN-E0403. Surface
                // that as the seal-check result rather than panicking, so a leak
                // caught at elaboration and a leak caught by the export-edge
                // `check_module_seals` are both observable as RN-E0403.
                let tree = match crate::sema::elaborate(
                    &crate::prelude::sig(),
                    &crate::check::Ctx::new(),
                    0,
                    &term,
                ) {
                    Ok(tree) => tree,
                    Err(e) => return Err(e),
                };
                let mut all = crate::stdlib::stdlib_module_decls();
                all.extend(user_mods);
                check_module_seals(&all, &tree)
            })
            .expect("spawn capability seal worker")
            .join()
            .expect("capability seal worker panicked")
    }

    #[test]
    fn the_real_console_seal_holds() {
        // Console `seals (winapi)`, and its exported `console_writeln` carries `console`,
        // not `winapi` — the kernel export boundary is honoured.
        seals_check(r#"console_writeln "hi""#).expect("console's winapi seal holds");
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
        // A boundary module mints `asm` (its job) and EXPOSES the raw `rotl` one
        // layer down to a services module; that service tries to re-export a
        // wrapper that still carries `{asm}` while sealing it. The seal is a *leak
        // check*, not a strip: a pure `rotl_svc : Int -> Int -> Int ! {asm}` arrow
        // cannot be hidden this way (RN-E0403). To expose asm to the app you must
        // discharge it behind an effect (the Console/winapi pattern), so the
        // exported binding carries the service effect, never the raw boundary
        // label. This is the design, not a gap — it is why the journal's first A3
        // sketch (`seals (asm) exposing (rotl)` over a raw wrapper) is rejected.
        //
        // Structured to PASS the level check (D1, RN-E0405) so the seal-leak
        // check (RN-E0403) is what fires: `rotl` is boundary-exposed → visible to
        // the one-down `Asm` service, whose `rotl_svc` is visible to the app
        // entry. The leak is the *type-level* `{asm}` escape, not a visibility one.
        let err = seals_check(
            r#"module Bits at boundary mints (asm) exposing (rotl) =
                 let rotl = extern asm "locus_asm_rotl64" : Int -> Int -> Int in ()
               module Asm at services seals (asm) exposing (rotl_svc) =
                 let rotl_svc = fn a: Int => fn b: Int => rotl a b in ()
               rotl_svc 1 4"#,
        )
        .expect_err("a raw asm wrapper carries {asm} and cannot be sealed away");
        assert_eq!(err.code(), "RN-E0403");
        assert!(matches!(
            err,
            TypeErr::ModuleSealLeak {
                ref module, label: crate::syntax::Label::World(ref n), ..
            } if module == "Asm" && n == "asm"
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
        assert!(matches!(
            err,
            CapError::MintOutsideBoundary { module: None, .. }
        ));
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
        // function type. seal_escape recurses structurally, so this is caught — now
        // by the module-seal STRIP itself (Sprint 3): the entry `leak 1` returns
        // the mem-carrying tuple as the *program result*, so the `Term::Seal(mem)`
        // wrapping the grafted module fires its post-zonk no-escape obligation
        // (`SealLeak`) at elaboration. Same law (RN-E0403, the sealed power may not
        // escape through a value), caught one phase earlier than the export-edge
        // `ModuleSealLeak` — both are the seal refusing to launder `mem`.
        let err = seals_check(
            "module Bad at app seals (mem) = \
               let leak = fn x: Int => (fn y: Int => poke8 x y, 0) in () \
             leak 1",
        )
        .expect_err("a sealed label hidden in a tuple leaks");
        assert_eq!(err.code(), "RN-E0403");
        assert!(
            matches!(
                err,
                TypeErr::SealLeak { label: crate::syntax::Label::World(ref n), .. }
                    | TypeErr::ModuleSealLeak { label: crate::syntax::Label::World(ref n), .. }
                if n == "mem"
            ),
            "the seal must refuse to launder mem through the tuple: {err:?}"
        );
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
