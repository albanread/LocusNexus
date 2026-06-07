//! Evidence-passing & the zero-cost witness — **`calculus.md` §5**.
//!
//! The metatheory *proved* the graded zero-cost theorem (§5.2): a
//! statically-resolved, tail-resumptive (or abort), generation-safe effect is
//! **fully eliminated at runtime**; the only residual is the continuation the
//! handler's resumption shape demands. This pass is the **executable witness**
//! of that proof — it runs the theorem on real IR.
//!
//! It threads an **evidence vector** over the ANF IR (§5.1):
//!
//! ```text
//! ⟦ handle e with H ⟧  =  ⟦e⟧  under  ev[ op ↦ H.op | op ∈ H ]
//! ⟦ perform op w ⟧     =  ev(op) w
//! ```
//!
//! At each `perform op`, look up `ev(op)`:
//!   * **resolved** — a handler is in scope ⇒ the op *leaves the residual row*
//!     (§5.1). Its cost is set by the handler's **resumption shape** (§1.3):
//!     abort / tail-resumptive ⇒ inlines away; one-shot ⇒ a cheap stack
//!     continuation; multi-shot ⇒ a reified GC continuation.
//!   * **residual** — nothing intercepts it, so the op stays in the row. A
//!     **native** (`World`) op then lowers to its **prelowered runtime
//!     function** (the JIT calls it — the runtime is its default handler); a
//!     **user** op is genuinely **unhandled** and escapes to the caller.
//!
//! Full **elimination** — the front-end's zero-cost *guarantee* — needs **two**
//! conditions, and both must hold:
//!
//!   * **in force at compile time** — the handler is at the **generation
//!     stage** (`stage >= 1`), so its evidence is a generation-stage value and
//!     staged β inlines it away *before lowering* (§5.2). A runtime (stage-0)
//!     handler is only **dispatch-free**: LLVM may still inline it at `-O2`,
//!     but the front end does not *promise* it — so we do not call it
//!     eliminated. Zero-cost is **earned by staging** the handler.
//!   * **static extent** — no **λ boundary** is crossed between the `perform`
//!     and its handler. A `perform` inside a closure is resolved by whatever
//!     handler is dynamic when the closure is *applied* (dispatch-free, not
//!     eliminated), so crossing a `lam` demotes the evidence to dynamic.
//!
//! Abort / tail-resumptive **with both** ⇒ *eliminated*. Miss either ⇒ a
//! dispatch-free direct call. One-shot ⇒ a one-shot continuation; multi-shot
//! ⇒ a reified one (the shape dominates, independent of stage).

use crate::ir::{Atom, Comp, Ir};
use crate::syntax::{Label, Row};

/// A handler clause's **resumption shape** ≈ its multiplicity grade `m` (§1.3),
/// read off syntactically from how often (and where) `resume` is used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// `resume` unused — the handler aborts (`m = 0`).
    Abort,
    /// `resume` used once, in tail position (`m ≤ 1`, tail-resumptive).
    Tail,
    /// `resume` used once, not in tail position (`m = 1`).
    OneShot,
    /// `resume` used more than once (`m = ω`).
    Multi,
}

impl Shape {
    fn label(self) -> &'static str {
        match self {
            Shape::Abort => "abort (m=0)",
            Shape::Tail => "tail-resumptive (m\u{2264}1)",
            Shape::OneShot => "one-shot (m=1)",
            Shape::Multi => "multi-shot (m=\u{3c9})",
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Shape::Abort => "abort",
            Shape::Tail => "tail",
            Shape::OneShot => "one-shot",
            Shape::Multi => "multi",
        }
    }
}

/// The runtime cost that survives evidence resolution (§5.2 table).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cost {
    /// Statically resolved + abort/tail ⇒ inlines to a `let`/return — **gone**.
    Eliminated,
    /// Dynamically resolved + abort/tail ⇒ a dispatch-free evidence call.
    Direct,
    /// One-shot, non-tail ⇒ a one-shot stack continuation (no heap).
    OneShot,
    /// Multi-shot ⇒ a reified, GC-owned continuation.
    Reified,
}

impl Cost {
    fn label(self) -> &'static str {
        match self {
            Cost::Eliminated => "eliminated \u{2014} zero runtime cost",
            Cost::Direct => "dispatch-free direct call (runtime evidence)",
            Cost::OneShot => "one-shot continuation (no heap)",
            Cost::Reified => "reified GC-owned continuation",
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Cost::Eliminated => "eliminated",
            Cost::Direct => "direct",
            Cost::OneShot => "one-shot",
            Cost::Reified => "reified",
        }
    }
}

/// How a single `perform` site resolves.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Resolution {
    Resolved {
        /// Handler in force at compile time (generation stage)?
        compile_time: bool,
        /// Reached without crossing a λ boundary?
        static_extent: bool,
        shape: Shape,
        cost: Cost,
    },
    /// Residual **native** effect — nothing intercepts it, so the compiler
    /// emits a direct call to its **prelowered runtime function** (the JIT
    /// links it). Still interceptable by an enclosing handler upstream.
    Runtime,
    /// Residual **non-native** effect — no handler and no runtime default, so
    /// it escapes: the caller must handle it.
    Unhandled,
}

/// One `perform` site and its resolution.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Site {
    pub op: Label,
    pub resolution: Resolution,
}

/// The evidence pass's verdict for a program.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Report {
    pub sites: Vec<Site>,
    /// The ops with no handler — what remains in the runtime row. (This must
    /// agree with sema's root row: a cross-check that the pass is faithful.)
    pub residual: Row,
}

/// One entry of the threaded evidence vector.
#[derive(Clone)]
struct Ev {
    op: Label,
    shape: Shape,
    /// Handler in force at compile time (installed at the generation stage)?
    compile_time: bool,
    /// Still within the handler's *static extent* (no λ crossed since install)?
    static_extent: bool,
}

/// Run the evidence pass over an ANF IR block.
pub fn analyze(ir: &Ir) -> Report {
    let mut sites = Vec::new();
    walk(ir, &[], &mut sites);
    let mut residual = Row::pure();
    for s in &sites {
        if matches!(s.resolution, Resolution::Runtime | Resolution::Unhandled) {
            residual = residual.union(&Row::single(s.op.clone()));
        }
    }
    Report { sites, residual }
}

fn walk(ir: &Ir, ev: &[Ev], sites: &mut Vec<Site>) {
    match ir {
        Ir::Block { binds, comp, .. } => {
            for bind in binds {
                walk_comp(&bind.comp, ev, sites);
            }
            walk_comp(comp, ev, sites);
        }
        Ir::Let { comp, rest, .. } => {
            walk_comp(comp, ev, sites);
            walk(rest, ev, sites);
        }
        Ir::Ret { comp, .. } => walk_comp(comp, ev, sites),
    }
}

fn walk_comp(c: &Comp, ev: &[Ev], sites: &mut Vec<Site>) {
    match c {
        // Atoms, applications, primitive ops, and extern refs introduce no new
        // perform site. (An extern's `winapi` effect is latent on its type,
        // surfaced by the *row*, not as a perform — see the type, not here.)
        Comp::Atom(_)
        | Comp::Brk
        | Comp::App { .. }
        | Comp::Call { .. }
        | Comp::Bin(_, _, _)
        | Comp::FloatBin(_, _, _)
        | Comp::Cast(_, _)
        // Tag/untag are pure repr-poly shifts; to_ptr/from_ptr are pure runtime
        // handle<->word conversions — no perform site.
        | Comp::Tag(_)
        | Comp::Untag(_)
        | Comp::ToPtr(_)
        | Comp::FromPtr(_)
        | Comp::FloatMathUnary { .. }
        | Comp::FloatMathBinary { .. }
        | Comp::FloatMathTernary { .. }
        | Comp::MaskReduce { .. }
        | Comp::Extern(_, _)
        | Comp::Foreign(_, _, _)
        | Comp::Splice(_)
        | Comp::Genlet(_)
        // The `mem` primitives are like an extern: their effect is latent on the
        // binding's row, not a perform site.
        | Comp::Peek(_, _)
        | Comp::Poke(_, _, _)
        | Comp::Fill(_, _, _)
        | Comp::Copy(_, _, _)
        | Comp::Tuple(_)
        | Comp::ArrayLit { .. }
        | Comp::VectorLit { .. }
        | Comp::VectorSplat { .. }
        | Comp::VectorBin { .. }
        | Comp::VectorCompare { .. }
        | Comp::VectorSelect { .. }
        | Comp::VectorExtract { .. }
        | Comp::Proj { .. }
        | Comp::Len(_)
        | Comp::ArrayGet { .. }
        | Comp::ArraySet { .. }
        // Packed array vector load/store: a managed-array read/write whose `gc`
        // effect rides the binding's row, like `ArrayGet`/`ArraySet` — no perform.
        | Comp::VectorLoad { .. }
        | Comp::VectorStore { .. }
        // Mutable-local stack slots (`let mut` / `:=`) are pure stack
        // alloc/load/store — no perform site.
        | Comp::SlotInit(_, _)
        | Comp::SlotLoad(_)
        | Comp::SlotStore(_, _)
        // Heap `Ref` cell ops (`ref`/`!`/`:=`): alloc + scalar get/set, like a
        // tuple build / `Proj`. Their `gc`/`st` effects ride the binding's row,
        // not a perform site — no handler dispatch here.
        | Comp::RefNew(_, _)
        | Comp::RefGet(_, _)
        | Comp::RefSet(_, _, _) => {}

        Comp::Perform(op, _) => {
            let resolution = match ev.iter().rev().find(|e| e.op == *op) {
                Some(e) => Resolution::Resolved {
                    compile_time: e.compile_time,
                    static_extent: e.static_extent,
                    shape: e.shape,
                    cost: cost_of(e.shape, e.compile_time, e.static_extent),
                },
                // Unintercepted: a native op falls through to the runtime; a
                // user op has no default and escapes.
                None if op.is_native() => Resolution::Runtime,
                None => Resolution::Unhandled,
            };
            sites.push(Site { op: op.clone(), resolution });
        }

        // Crossing into a closure: the enclosing evidence is now *dynamic*
        // (its `compile_time` standing is unchanged — only the extent is lost).
        Comp::Lam { body, .. } => {
            let demoted: Vec<Ev> = ev
                .iter()
                .map(|e| Ev { static_extent: false, ..e.clone() })
                .collect();
            walk(body, &demoted, sites);
        }

        // Both branches see the same handlers (no λ boundary).
        Comp::If(_, then, els) => {
            walk(then, ev, sites);
            walk(els, ev, sites);
        }

        Comp::Loop {
            cond,
            steps,
            result,
            ..
        } => {
            walk(cond, ev, sites);
            for step in steps {
                walk(step, ev, sites);
            }
            walk(result, ev, sites);
        }

        // Staging blocks pass the evidence through unchanged (no λ boundary).
        Comp::Quote(body) | Comp::Letloc(body) => walk(body, ev, sites),

        Comp::Handle { stage, scrutinee, handler } => {
            // The clause and return bodies run under the *outer* handlers.
            for op in &handler.ops {
                walk(&op.body, ev, sites);
            }
            walk(&handler.ret.body, ev, sites);
            // The scrutinee sees this handler installed (static extent), in
            // force at compile time exactly when it sits at the generation stage.
            let mut ev2 = ev.to_vec();
            for op in &handler.ops {
                ev2.push(Ev {
                    op: op.op.clone(),
                    shape: clause_shape(&op.resume, &op.body),
                    compile_time: *stage >= 1,
                    static_extent: true,
                });
            }
            walk(scrutinee, &ev2, sites);
        }
    }
}

fn cost_of(shape: Shape, compile_time: bool, static_extent: bool) -> Cost {
    match shape {
        // The front-end zero-cost *guarantee*: erased only when the handler is
        // in force at compile time AND statically reached. Otherwise the
        // dispatch is still gone, but the call/handler runs at runtime.
        Shape::Abort | Shape::Tail => {
            if compile_time && static_extent {
                Cost::Eliminated
            } else {
                Cost::Direct
            }
        }
        Shape::OneShot => Cost::OneShot,
        Shape::Multi => Cost::Reified,
    }
}

/// Classify a clause by how its `resume` binder is used in `body` — its
/// resumption shape (≈ multiplicity `m`). Public so codegen can lower a handler
/// by the same classification the evidence pass reports.
pub fn clause_shape(resume: &str, body: &Ir) -> Shape {
    match count_var(resume, body) {
        0 => Shape::Abort,
        1 if is_tail_resume(resume, body) => Shape::Tail,
        1 => Shape::OneShot,
        _ => Shape::Multi,
    }
}

/// Count uses of variable `name` across a block (conservative: a shadowing
/// inner binder of the same name only ever *raises* the count — safe).
pub(crate) fn count_var(name: &str, ir: &Ir) -> usize {
    match ir {
        Ir::Block { binds, comp, .. } => {
            binds
                .iter()
                .map(|bind| count_var_comp(name, &bind.comp))
                .sum::<usize>()
                + count_var_comp(name, comp)
        }
        Ir::Let { comp, rest, .. } => count_var_comp(name, comp) + count_var(name, rest),
        Ir::Ret { comp, .. } => count_var_comp(name, comp),
    }
}

pub(crate) fn count_var_comp(name: &str, c: &Comp) -> usize {
    let is = |a: &Atom| usize::from(matches!(a, Atom::Var(x) if x == name));
    match c {
        Comp::Atom(a) => is(a),
        Comp::Brk => 0,
        Comp::App { fun, arg, .. } => is(fun) + is(arg),
        Comp::Call { fun, args, .. } => is(fun) + args.iter().map(|(a, _)| is(a)).sum::<usize>(),
        Comp::Extern(_, _) => 0,
        Comp::Foreign(_, args, _) => args.iter().map(is).sum(),
        Comp::Bin(_, a, b) | Comp::FloatBin(_, a, b) => is(a) + is(b),
        Comp::Cast(_, a) => is(a),
        Comp::Tag(a) | Comp::Untag(a) | Comp::ToPtr(a) | Comp::FromPtr(a) => is(a),
        Comp::FloatMathUnary { value, .. } => is(value),
        Comp::FloatMathBinary { lhs, rhs, .. } => is(lhs) + is(rhs),
        Comp::FloatMathTernary { a, b, c, .. } => is(a) + is(b) + is(c),
        Comp::MaskReduce { mask, .. } => is(mask),
        // Only one branch runs per path → the max, not the sum.
        Comp::If(c, t, e) => is(c) + count_var(name, t).max(count_var(name, e)),
        Comp::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            vars.iter().map(|v| is(&v.init)).sum::<usize>()
                + count_var(name, cond)
                + steps
                    .iter()
                    .map(|step| count_var(name, step))
                    .sum::<usize>()
                + count_var(name, result)
        }
        Comp::Perform(_, a) => is(a),
        Comp::Splice(a) | Comp::Genlet(a) => is(a),
        Comp::Peek(_, a) => is(a),
        Comp::Poke(_, a, b) => is(a) + is(b),
        Comp::Fill(a, b, c) | Comp::Copy(a, b, c) => is(a) + is(b) + is(c),
        Comp::Tuple(fields) => fields.iter().map(|(a, _)| is(a)).sum(),
        Comp::ArrayLit { elems, .. } => elems.iter().map(is).sum(),
        Comp::VectorLit { elems, .. } => elems.iter().map(is).sum(),
        Comp::VectorSplat { value, .. } => is(value),
        Comp::VectorLoad { arr, idx, .. } => is(arr) + is(idx),
        Comp::VectorStore {
            arr, idx, value, ..
        } => is(arr) + is(idx) + is(value),
        Comp::VectorBin { lhs, rhs, .. } => is(lhs) + is(rhs),
        Comp::VectorCompare { lhs, rhs, .. } => is(lhs) + is(rhs),
        Comp::VectorSelect {
            mask,
            then_value,
            else_value,
            ..
        } => is(mask) + is(then_value) + is(else_value),
        Comp::VectorExtract { vector, .. } => is(vector),
        Comp::Proj { tup, .. } => is(tup),
        Comp::Len(a) => is(a),
        Comp::ArrayGet { arr, idx, .. } => is(arr) + is(idx),
        Comp::ArraySet { arr, idx, val, .. } => is(arr) + is(idx) + is(val),
        // The stored atom is a normal operand; the slot name is a mutable-local
        // binder in its own namespace (never captured), not an SSA reference.
        Comp::SlotInit(_, init) => is(init),
        Comp::SlotLoad(_) => 0,
        Comp::SlotStore(_, val) => is(val),
        // Heap `Ref` cell ops — count the handle/value operand atoms.
        Comp::RefNew(init, _) => is(init),
        Comp::RefGet(r, _) => is(r),
        Comp::RefSet(r, val, _) => is(r) + is(val),
        Comp::Lam { body, .. } => count_var(name, body),
        Comp::Quote(b) | Comp::Letloc(b) => count_var(name, b),
        Comp::Handle {
            scrutinee, handler, ..
        } => {
            count_var(name, scrutinee)
                + handler
                    .ops
                    .iter()
                    .map(|op| count_var(name, &op.body))
                    .sum::<usize>()
                + count_var(name, &handler.ret.body)
        }
    }
}

/// Is the block's tail computation exactly `resume <atom>`?
fn is_tail_resume(name: &str, body: &Ir) -> bool {
    fn tail(ir: &Ir) -> &Comp {
        match ir {
            Ir::Block { comp, .. } => comp,
            Ir::Let { rest, .. } => tail(rest),
            Ir::Ret { comp, .. } => comp,
        }
    }
    matches!(tail(body), Comp::App { fun: Atom::Var(x), .. } if x == name)
}

// ── Rendering ────────────────────────────────────────────────────────────

/// Site tallies, by resolution.
#[derive(Default)]
struct Counts {
    eliminated: usize,
    direct: usize,
    one_shot: usize,
    reified: usize,
    runtime: usize,
    unhandled: usize,
}

impl Report {
    fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for s in &self.sites {
            match &s.resolution {
                Resolution::Resolved { cost, .. } => match cost {
                    Cost::Eliminated => c.eliminated += 1,
                    Cost::Direct => c.direct += 1,
                    Cost::OneShot => c.one_shot += 1,
                    Cost::Reified => c.reified += 1,
                },
                Resolution::Runtime => c.runtime += 1,
                Resolution::Unhandled => c.unhandled += 1,
            }
        }
        c
    }

    /// One-line summary (`--brief`).
    pub fn brief(&self) -> String {
        let c = self.counts();
        format!(
            "{}/{} eliminated; {} \u{2192} runtime; {} unhandled; residual row {}",
            c.eliminated,
            self.sites.len(),
            c.runtime,
            c.unhandled,
            self.residual
        )
    }

    /// Per-site report (`calculus.md` §5).
    pub fn to_text(&self) -> String {
        let mut s = String::from(
            "evidence pass \u{2014} calculus \u{a7}5 (evidence-passing & zero-cost)\n",
        );
        if self.sites.is_empty() {
            s.push_str("  (no effect operations)\n");
        }
        for site in &self.sites {
            match &site.resolution {
                Resolution::Resolved { compile_time, static_extent, shape, cost } => {
                    let when = if *compile_time { "compile-time" } else { "runtime" };
                    let scope = if *static_extent { "static" } else { "dynamic" };
                    s.push_str(&format!(
                        "  perform {:<8} resolved {when}/{scope}   {:<22} \u{21d2} {}\n",
                        site.op.to_string(),
                        shape.label(),
                        cost.label()
                    ));
                }
                Resolution::Runtime => s.push_str(&format!(
                    "  perform {:<8} residual (native)   \u{21d2} prelowered runtime fn \u{2014} the JIT calls it\n",
                    site.op.to_string()
                )),
                Resolution::Unhandled => s.push_str(&format!(
                    "  perform {:<8} residual (user)     \u{21d2} unhandled \u{2014} escapes to the caller\n",
                    site.op.to_string()
                )),
            }
        }
        s.push_str(&format!("  residual row: {}\n", self.residual));
        s.push_str(&format!("  summary: {}\n", self.brief()));
        s
    }

    /// Machine-readable (schema `locus-evidence/1`).
    pub fn to_json(&self) -> String {
        let esc = crate::diag::esc;
        let sites: Vec<String> = self
            .sites
            .iter()
            .map(|site| match &site.resolution {
                Resolution::Resolved { compile_time, static_extent, shape, cost } => format!(
                    "{{\"op\":\"{}\",\"resolution\":\"resolved\",\"inForce\":\"{}\",\"scope\":\"{}\",\"shape\":\"{}\",\"cost\":\"{}\"}}",
                    esc(&site.op.to_string()),
                    if *compile_time { "compile-time" } else { "runtime" },
                    if *static_extent { "static" } else { "dynamic" },
                    shape.tag(),
                    cost.tag()
                ),
                Resolution::Runtime => {
                    format!("{{\"op\":\"{}\",\"resolution\":\"runtime\"}}", esc(&site.op.to_string()))
                }
                Resolution::Unhandled => {
                    format!("{{\"op\":\"{}\",\"resolution\":\"unhandled\"}}", esc(&site.op.to_string()))
                }
            })
            .collect();
        let residual: Vec<String> = self
            .residual
            .labels()
            .map(|l| format!("\"{}\"", esc(&l.to_string())))
            .collect();
        let c = self.counts();
        format!(
            "{{\"schema\":\"locus-evidence/1\",\"ok\":true,\"residual\":[{}],\"counts\":{{\"eliminated\":{},\"direct\":{},\"oneShot\":{},\"reified\":{},\"runtime\":{},\"unhandled\":{}}},\"sites\":[{}]}}",
            residual.join(","),
            c.eliminated, c.direct, c.one_shot, c.reified, c.runtime, c.unhandled,
            sites.join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{elaborate, lower, parse, Ctx, Sig};

    fn report(src: &str, stage: crate::Stage) -> Report {
        let term = parse(src).unwrap();
        let tree = elaborate(&Sig::new(), &Ctx::new(), stage, &term).unwrap();
        analyze(&lower(&tree))
    }
    fn user(s: &str) -> Label {
        Label::User(s.to_string())
    }

    const HANDLE_ASK: &str = "handle perform ask () with { ask(x) => resume x ; return(y) => y }";

    #[test]
    fn a_runtime_tail_handler_is_dispatch_free_not_eliminated() {
        // Stage 0: the handler is a *runtime* value, so the front end does not
        // promise erasure — only dispatch-freedom. Zero-cost is earned by staging.
        let r = report(HANDLE_ASK, 0);
        assert!(
            r.residual.is_pure(),
            "ask handled \u{21d2} residual row empty"
        );
        assert_eq!(r.sites.len(), 1);
        match &r.sites[0].resolution {
            Resolution::Resolved {
                compile_time,
                static_extent,
                shape,
                cost,
            } => {
                assert!(
                    !*compile_time,
                    "stage 0 \u{21d2} not in force at compile time"
                );
                assert!(*static_extent);
                assert_eq!(*shape, Shape::Tail);
                assert_eq!(*cost, Cost::Direct);
            }
            _ => panic!("ask should resolve"),
        }
    }

    #[test]
    fn a_staged_tail_handler_is_eliminated() {
        // Stage 1: the handler is in force at compile time ⇒ inlined away
        // before lowering ⇒ genuinely zero-cost (the front-end guarantee).
        let r = report(HANDLE_ASK, 1);
        match &r.sites[0].resolution {
            Resolution::Resolved {
                compile_time, cost, ..
            } => {
                assert!(*compile_time, "stage 1 \u{21d2} in force at compile time");
                assert_eq!(*cost, Cost::Eliminated);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn a_user_op_with_no_handler_is_unhandled() {
        // `log` is a User effect — no runtime default ⇒ it escapes.
        let r = report("perform log ()", 0);
        assert_eq!(r.residual, Row::single(user("log")));
        assert!(matches!(r.sites[0].resolution, Resolution::Unhandled));
    }

    #[test]
    fn a_native_op_with_no_handler_falls_through_to_the_runtime() {
        // `console` is a World (native) effect ⇒ its default is the prelowered
        // runtime function; residual, but NOT unhandled.
        let r = report(r#"perform console "hi""#, 0);
        assert_eq!(r.residual, Row::single(Label::World("console".into())));
        assert!(matches!(r.sites[0].resolution, Resolution::Runtime));
    }

    #[test]
    fn a_handler_intercepts_a_native_op_before_the_runtime() {
        // Interception lives in the language: a user handler overrides the
        // runtime default, so console resolves to the handler, not the runtime.
        let r = report(
            r#"handle perform console "hi" with { console(s) => () ; return(y) => y }"#,
            0,
        );
        assert!(
            r.residual.is_pure(),
            "intercepted \u{21d2} never reaches the runtime"
        );
        assert!(matches!(r.sites[0].resolution, Resolution::Resolved { .. }));
    }

    #[test]
    fn multi_shot_resume_is_reified_regardless_of_stage() {
        // `resume (resume x)` uses resume twice ⇒ m = ω. Staging removes the
        // dispatch but the continuation is still reified — even at compile time.
        let r = report(
            "handle perform ask () with { ask(x) => resume (resume x) ; return(y) => y }",
            1,
        );
        let site = &r.sites[0];
        assert_eq!(site.op, user("ask"));
        match &site.resolution {
            Resolution::Resolved { shape, cost, .. } => {
                assert_eq!(*shape, Shape::Multi);
                assert_eq!(
                    *cost,
                    Cost::Reified,
                    "multi-shot keeps a reified continuation"
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn a_perform_behind_a_lambda_is_not_eliminated_even_when_staged() {
        // Stage 1 (compile-time handler) BUT the op fires inside a closure, so
        // it is resolved dynamically when the closure is applied: the second
        // condition (static extent) fails ⇒ dispatch-free, not eliminated.
        let r = report(
            "handle (fn u: Unit => perform ask ()) () with { ask(x) => resume x ; return(y) => y }",
            1,
        );
        assert!(
            r.residual.is_pure(),
            "still handled (dynamically) \u{21d2} row empty"
        );
        let site = r
            .sites
            .iter()
            .find(|s| s.op == user("ask"))
            .expect("ask site");
        match &site.resolution {
            Resolution::Resolved {
                compile_time,
                static_extent,
                cost,
                ..
            } => {
                assert!(*compile_time, "the handler IS at compile time");
                assert!(!*static_extent, "but behind a \u{3bb} \u{21d2} dynamic");
                assert_eq!(
                    *cost,
                    Cost::Direct,
                    "so still dispatch-free, not eliminated"
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn a_staged_abort_handler_is_eliminated() {
        // Abort (resume unused) is zero-cost too — when in force at compile time.
        let r = report(
            "handle perform ask () with { ask(x) => () ; return(y) => y }",
            1,
        );
        match &r.sites[0].resolution {
            Resolution::Resolved { shape, cost, .. } => {
                assert_eq!(*shape, Shape::Abort);
                assert_eq!(*cost, Cost::Eliminated);
            }
            _ => panic!(),
        }
    }
}
