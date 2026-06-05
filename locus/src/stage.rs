//! Staging reduction — run the generators at compile time (`calculus.md` §3, the
//! comonadic half).
//!
//! A `splice` `${C}` embeds the code its generator `C` produces. **Generation is
//! a compile-time computation** — `C` lives at the generation stage (`s ≥ 1`,
//! "code that produces stage-0 code"), so we run it *now*: [`stage_reduce`]
//! executes each generator and replaces its splice with the residual stage-0
//! code. After this pass there are **no `Quote` / `Splice` / `Genlet` / `Letloc`
//! nodes** — codegen sees ordinary object code. This is where staging earns its
//! keep: the generation stage vanishes at compile time, leaving only the
//! specialized code.
//!
//! The generator interpreter ([`Staging::gen_code`]) carries an environment of
//! static bindings. A generator value is a **`Static`** number (a compile-time
//! `Int`/`Bool`), a piece of residual **`Code`**, or a generation-stage
//! **`Closure`** (a code-builder). The headline constructs:
//!   * a `Static` value referenced inside a `quote` body is a **cross-stage
//!     constant** — lifted into the generated code as a literal;
//!   * a `Code` value referenced by `${…}` inside a `quote` is spliced in;
//!   * `quote`/(recursive) functions build code; a static `if` selects it;
//!   * **`genlet(c)`** (δ's *generative* direction) hoists `c` to a shared `let`
//!     at the enclosing **locus** (the `splice`, or an explicit `letloc`) and
//!     returns a reference — so code used many times is **computed once**.
//!
//! Not yet: cross-stage *string* constants, and effects/`mem` inside a generator
//! (those raise a clear error). Hoist scope-safety (`RN-E0331`) is assumed, not
//! checked — an out-of-scope hoist surfaces as an unbound-variable error in
//! codegen rather than a miscompile.

use std::collections::HashMap;

use crate::sema::{Node, Typed, TypedBlockItem, TypedHandler, TypedOpClause, TypedReturn};
use crate::syntax::{BinOp, Row, Type};

type Env = HashMap<String, GenVal>;

/// A value produced by running a generator.
#[derive(Clone)]
enum GenVal {
    Static(i64),
    StaticFloat(u64),
    Code(Typed),
    /// A static sum/constructor value built at the generation stage (`Some(5)`,
    /// `Cons(h, t)`): the constructor `tag` plus its field values. Lets a
    /// generator **inspect** a constructor with `match` to choose what code to emit.
    Sum {
        tag: i64,
        args: Vec<GenVal>,
    },
    /// A static tuple built at the generation stage — destructured positionally by
    /// a generation-stage `let (a, b) = …`.
    Tuple(Vec<GenVal>),
    /// A static record built at the generation stage — projected by a
    /// generation-stage `.field`.
    Record(Vec<(String, GenVal)>),
    /// A generation-stage function. `self_name` lets it recurse (`let rec`).
    Closure {
        param: String,
        body: Box<Typed>,
        env: Env,
        self_name: Option<String>,
    },
}

/// Hoisted bindings accumulated by `genlet` within one locus, emitted there.
type Insertions = Vec<(String, Typed)>;

/// Reduce all staging in `t`: run the generators, hoist the `genlet`s, and
/// replace each splice with its residual code. A structural copy for programs
/// without staging.
pub fn stage_reduce(t: &Typed) -> Result<Typed, String> {
    run_reduce(t, DEFAULT_GEN_FUEL)
}

/// Run the staging reduction on a dedicated **large-stack** worker thread. The
/// generation evaluator (`gen_code`/`apply`) is a native tree-walker — it recurses
/// one Rust frame per generation step, and Locus's guaranteed TCO (a separate,
/// runtime feature) does not apply to it — so a deep (but finite, within
/// [`DEFAULT_GEN_FUEL`]) recursive generator needs far more stack than the default.
/// The budget is set well below this stack's frame capacity, so a *runaway*
/// generator hits the clean `RN-E0332` diagnostic instead of overflowing the stack.
/// (Same big-stack-worker pattern the elaborator/lowerer use.)
fn run_reduce(t: &Typed, budget: u64) -> Result<Typed, String> {
    let t = t.clone();
    std::thread::Builder::new()
        .name("locus-stage".into())
        .stack_size(STAGE_STACK_BYTES)
        .spawn(move || Staging::new(budget).reduce(&t, &Env::new()))
        .expect("spawn staging worker")
        .join()
        .expect("staging worker panicked")
}

/// The staging worker's stack. Generously sized so deep (finite) generation has
/// room; paired with [`DEFAULT_GEN_FUEL`] so the budget trips first on a runaway.
const STAGE_STACK_BYTES: usize = 256 * 1024 * 1024;

/// The default **expansion budget** (`RN-E0332`, forbidden-semantics F14): the
/// number of generation-stage function applications a single `stage_reduce` may
/// perform before it is declared non-terminating. Applications are the only
/// unbounded driver of code generation (a recursive generator calls itself
/// through `apply`), so bounding them bounds the whole expansion. Kept comfortably
/// below the [`STAGE_STACK_BYTES`] worker's depth capacity so a runaway trips the
/// clean diagnostic, not a stack overflow — yet far above any realistic macro.
const DEFAULT_GEN_FUEL: u64 = 30_000;

/// The reducer's state: a counter for fresh `genlet` binding names, plus the
/// remaining + original [`DEFAULT_GEN_FUEL`] expansion budget.
struct Staging {
    next: u32,
    /// Remaining generation-stage applications before `RN-E0332`.
    fuel: u64,
    /// The original budget, for the diagnostic message.
    budget: u64,
}

impl Staging {
    fn new(budget: u64) -> Self {
        Staging {
            next: 0,
            fuel: budget,
            budget,
        }
    }

    /// Charge one generation step (a `gen_code` application). Returns the
    /// `RN-E0332` diagnostic when the expansion budget is exhausted — the
    /// macro-termination guard.
    fn charge_step(&mut self) -> Result<(), String> {
        self.fuel = self.fuel.checked_sub(1).ok_or_else(|| {
            format!(
                "RN-E0332 macro.expansion-limit: code generation exceeded its budget of \
                 {} generation steps — the generator is likely non-terminating \
                 (forbidden-semantics F14); give the recursion a reachable base case",
                self.budget
            )
        })?;
        Ok(())
    }
}

impl Staging {
    fn fresh(&mut self) -> String {
        let n = self.next;
        self.next += 1;
        format!("__g{n}")
    }

    /// Reduce a **stage-0 (object)** expression, substituting cross-stage
    /// constants from `env` (the enclosing generator's static bindings).
    fn reduce(&mut self, t: &Typed, env: &Env) -> Result<Typed, String> {
        let rb = |node: Node| Typed {
            ty: t.ty.clone(),
            row: t.row.clone(),
            stage: t.stage,
            layout_known: t.layout_known,
            node,
        };
        Ok(match &t.node {
            // `${ C }` — the locus. Run the generator, collecting its `genlet`
            // hoists, then wrap the residual in those shared `let`s.
            Node::Splice(c) => {
                let mut ins = Insertions::new();
                let residual = as_code(self.gen_code(c, env, &mut ins)?, &t.ty)?;
                return Ok(wrap_insertions(residual, ins));
            }
            Node::Quote(_) => {
                return Err(
                    "staging: a `quote` with no enclosing `splice` is not runnable yet".into(),
                )
            }
            Node::Genlet(_) | Node::Letloc(_) => {
                return Err(
                    "staging: `genlet` / `letloc` appear at the generation stage, inside \
                            a `splice` — they cannot run as object code"
                        .into(),
                )
            }

            // A variable: a generator-bound **static** value is a cross-stage
            // constant, lifted into the code as a literal.
            Node::Var(x) => match env.get(x) {
                Some(GenVal::Static(n)) => {
                    // A residual type variable must have been zonked away before
                    // staging (D6/D7); reaching here with one means we cannot tell
                    // a `Bool` constant from an `Int` one — refuse rather than
                    // silently pick `Int`.
                    if matches!(t.ty, Type::Var(_)) {
                        return Err(format!(
                            "staging: cross-stage constant `{x}` has an un-zonked type \
                             variable — type inference left it ambiguous"
                        ));
                    }
                    if t.ty == Type::Bool {
                        rb(Node::Bool(*n != 0))
                    } else {
                        rb(Node::Int(*n))
                    }
                }
                Some(GenVal::StaticFloat(bits)) => {
                    if t.ty == Type::Float {
                        rb(Node::Float(*bits))
                    } else {
                        return Err(format!(
                            "staging: cross-stage constant `{x}` is a Float, but the residual \
                             site expects `{}`",
                            t.ty
                        ));
                    }
                }
                Some(_) => {
                    return Err(format!(
                        "staging: `{x}` is a code value or function used where a \
                                        plain value is expected — splice it (`${{{x}}}`) instead"
                    ))
                }
                None => t.clone(),
            },

            Node::Int(_)
            | Node::Float(_)
            | Node::Bool(_)
            | Node::Unit
            | Node::Str(_)
            | Node::Extern(..) => t.clone(),

            Node::Lam {
                param,
                param_ty,
                body,
            } => rb(Node::Lam {
                param: param.clone(),
                param_ty: param_ty.clone(),
                body: Box::new(self.reduce(body, &shadow(env, param))?),
            }),
            Node::Let { name, bound, body } => rb(Node::Let {
                name: name.clone(),
                bound: Box::new(self.reduce(bound, env)?),
                body: Box::new(self.reduce(body, &shadow(env, name))?),
            }),
            Node::Block { items, body } => {
                let mut out = Vec::with_capacity(items.len());
                let mut inner_env = env.clone();
                for item in items {
                    match item {
                        TypedBlockItem::Let { name, bound } => {
                            out.push(TypedBlockItem::Let {
                                name: name.clone(),
                                bound: self.reduce(bound, &inner_env)?,
                            });
                            inner_env = shadow(&inner_env, name);
                        }
                        TypedBlockItem::LetMut { name, bound } => {
                            out.push(TypedBlockItem::LetMut {
                                name: name.clone(),
                                bound: self.reduce(bound, &inner_env)?,
                            });
                            inner_env = shadow(&inner_env, name);
                        }
                        TypedBlockItem::LetTuple {
                            names,
                            bound,
                            fields_layout_known,
                        } => {
                            out.push(TypedBlockItem::LetTuple {
                                names: names.clone(),
                                bound: self.reduce(bound, &inner_env)?,
                                fields_layout_known: *fields_layout_known,
                            });
                            for name in names {
                                inner_env = shadow(&inner_env, name);
                            }
                        }
                    }
                }
                rb(Node::Block {
                    items: out,
                    body: Box::new(self.reduce(body, &inner_env)?),
                })
            }
            // A mutable local residualizes structurally, like `Let`: reduce the
            // initializer, shadow the name (a mutable local is never a static
            // cross-stage constant), reduce the body, rebuild the node.
            Node::LetMut { name, bound, body } => rb(Node::LetMut {
                name: name.clone(),
                bound: Box::new(self.reduce(bound, env)?),
                body: Box::new(self.reduce(body, &shadow(env, name))?),
            }),
            // An assignment residualizes structurally: reduce the value, rebuild.
            Node::Assign { name, value } => rb(Node::Assign {
                name: name.clone(),
                value: Box::new(self.reduce(value, env)?),
            }),
            // `Ref` operators residualize structurally (the heap cell is an object-
            // stage runtime value, like `let mut`): reduce the sub-expressions,
            // rebuild the node, preserving its scalar `cell_layout`.
            Node::RefNew { value } => rb(Node::RefNew {
                value: Box::new(self.reduce(value, env)?),
            }),
            Node::Deref { cell } => rb(Node::Deref {
                cell: Box::new(self.reduce(cell, env)?),
            }),
            Node::RefAssign { target, value } => rb(Node::RefAssign {
                target: Box::new(self.reduce(target, env)?),
                value: Box::new(self.reduce(value, env)?),
            }),
            Node::Handle { scrutinee, handler } => {
                let ops = handler
                    .ops
                    .iter()
                    .map(|c| {
                        let inner = shadow(&shadow(env, &c.arg), &c.resume);
                        Ok::<_, String>(TypedOpClause {
                            op: c.op.clone(),
                            arg: c.arg.clone(),
                            arg_ty: c.arg_ty.clone(),
                            arg_layout: c.arg_layout,
                            resume: c.resume.clone(),
                            resume_ty: c.resume_ty.clone(),
                            resume_layout: c.resume_layout,
                            body: Box::new(self.reduce(&c.body, &inner)?),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let ret = TypedReturn {
                    var: handler.ret.var.clone(),
                    var_ty: handler.ret.var_ty.clone(),
                    var_layout: handler.ret.var_layout,
                    body_ty: handler.ret.body_ty.clone(),
                    body: Box::new(self.reduce(&handler.ret.body, &shadow(env, &handler.ret.var))?),
                };
                rb(Node::Handle {
                    scrutinee: Box::new(self.reduce(scrutinee, env)?),
                    handler: TypedHandler { ops, ret },
                })
            }

            Node::Bin(op, a, b) => rb(Node::Bin(
                *op,
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
            )),
            Node::Cast(op, a) => rb(Node::Cast(*op, Box::new(self.reduce(a, env)?))),
            Node::Coerce {
                kind,
                slot,
                value,
                inner,
            } => rb(Node::Coerce {
                kind: *kind,
                slot: slot.clone(),
                value: value.clone(),
                inner: Box::new(self.reduce(inner, env)?),
            }),
            Node::FloatMathUnary(op, a) => {
                rb(Node::FloatMathUnary(*op, Box::new(self.reduce(a, env)?)))
            }
            Node::FloatMathBinary(op, a, b) => rb(Node::FloatMathBinary(
                *op,
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
            )),
            Node::FloatMathTernary(op, a, b, c) => rb(Node::FloatMathTernary(
                *op,
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
                Box::new(self.reduce(c, env)?),
            )),
            Node::MaskReduce(op, a) => rb(Node::MaskReduce(*op, Box::new(self.reduce(a, env)?))),
            Node::VectorSelect {
                mask,
                then_value,
                else_value,
            } => rb(Node::VectorSelect {
                mask: Box::new(self.reduce(mask, env)?),
                then_value: Box::new(self.reduce(then_value, env)?),
                else_value: Box::new(self.reduce(else_value, env)?),
            }),
            Node::If(c, th, el) => rb(Node::If(
                Box::new(self.reduce(c, env)?),
                Box::new(self.reduce(th, env)?),
                Box::new(self.reduce(el, env)?),
            )),
            Node::Loop {
                vars,
                cond,
                steps,
                result,
            } => {
                let mut reduced_vars = Vec::with_capacity(vars.len());
                let mut inner_env = env.clone();
                for (name, ty, layout, init) in vars {
                    reduced_vars.push((name.clone(), ty.clone(), *layout, self.reduce(init, env)?));
                    inner_env = shadow(&inner_env, name);
                }
                let reduced_steps = steps
                    .iter()
                    .map(|step| self.reduce(step, &inner_env))
                    .collect::<Result<Vec<_>, _>>()?;
                rb(Node::Loop {
                    vars: reduced_vars,
                    cond: Box::new(self.reduce(cond, &inner_env)?),
                    steps: reduced_steps,
                    result: Box::new(self.reduce(result, &inner_env)?),
                })
            }
            Node::App { fun, arg } => rb(Node::App {
                fun: Box::new(self.reduce(fun, env)?),
                arg: Box::new(self.reduce(arg, env)?),
            }),
            Node::Perform { label, arg } => rb(Node::Perform {
                label: label.clone(),
                arg: Box::new(self.reduce(arg, env)?),
            }),
            Node::Peek(w, a) => rb(Node::Peek(*w, Box::new(self.reduce(a, env)?))),
            Node::Poke(w, a, b) => rb(Node::Poke(
                *w,
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
            )),
            Node::Fill(a, b, c) => rb(Node::Fill(
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
                Box::new(self.reduce(c, env)?),
            )),
            Node::Copy(a, b, c) => rb(Node::Copy(
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
                Box::new(self.reduce(c, env)?),
            )),
            Node::Index(w, a, b) => rb(Node::Index(
                *w,
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
            )),
            Node::IndexSet(w, a, b, c) => rb(Node::IndexSet(
                *w,
                Box::new(self.reduce(a, env)?),
                Box::new(self.reduce(b, env)?),
                Box::new(self.reduce(c, env)?),
            )),
            Node::Tuple(es) => {
                let mut out = Vec::with_capacity(es.len());
                for e in es {
                    out.push(self.reduce(e, env)?);
                }
                rb(Node::Tuple(out))
            }
            Node::LetTuple(names, e, body) => {
                let inner = names.iter().fold(env.clone(), |acc, n| shadow(&acc, n));
                rb(Node::LetTuple(
                    names.clone(),
                    Box::new(self.reduce(e, env)?),
                    Box::new(self.reduce(body, &inner)?),
                ))
            }
            Node::Record(fs) => {
                let mut out = Vec::with_capacity(fs.len());
                for (name, v) in fs {
                    out.push((name.clone(), self.reduce(v, env)?));
                }
                rb(Node::Record(out))
            }
            Node::Field(r, name) => rb(Node::Field(Box::new(self.reduce(r, env)?), name.clone())),
            Node::VectorLit { shape, elems } => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.reduce(e, env)?);
                }
                rb(Node::VectorLit {
                    shape: *shape,
                    elems: out,
                })
            }
            Node::VectorSplat { shape, value } => rb(Node::VectorSplat {
                shape: *shape,
                value: Box::new(self.reduce(value, env)?),
            }),
            Node::VectorLoad { shape, arr, idx } => rb(Node::VectorLoad {
                shape: *shape,
                arr: Box::new(self.reduce(arr, env)?),
                idx: Box::new(self.reduce(idx, env)?),
            }),
            Node::VectorStore {
                shape,
                arr,
                idx,
                value,
            } => rb(Node::VectorStore {
                shape: *shape,
                arr: Box::new(self.reduce(arr, env)?),
                idx: Box::new(self.reduce(idx, env)?),
                value: Box::new(self.reduce(value, env)?),
            }),
            Node::VectorExtract { vector, lane } => rb(Node::VectorExtract {
                vector: Box::new(self.reduce(vector, env)?),
                lane: *lane,
            }),
            Node::ArrayLit { elems, elem_layout } => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.reduce(e, env)?);
                }
                rb(Node::ArrayLit {
                    elems: out,
                    elem_layout: *elem_layout,
                })
            }
            Node::Len(a) => rb(Node::Len(Box::new(self.reduce(a, env)?))),
            Node::ArrayGet {
                arr,
                idx,
                elem_layout,
            } => rb(Node::ArrayGet {
                arr: Box::new(self.reduce(arr, env)?),
                idx: Box::new(self.reduce(idx, env)?),
                elem_layout: *elem_layout,
            }),
            Node::ArraySet {
                arr,
                idx,
                val,
                elem_layout,
            } => rb(Node::ArraySet {
                arr: Box::new(self.reduce(arr, env)?),
                idx: Box::new(self.reduce(idx, env)?),
                val: Box::new(self.reduce(val, env)?),
                elem_layout: *elem_layout,
            }),
            Node::Construct { tag, args } => {
                let mut out = Vec::with_capacity(args.len());
                for (a, layout, slot) in args {
                    out.push((self.reduce(a, env)?, *layout, slot.clone()));
                }
                rb(Node::Construct {
                    tag: *tag,
                    args: out,
                })
            }
            Node::Match { scrutinee, arms } => {
                let s = self.reduce(scrutinee, env)?;
                let mut out = Vec::with_capacity(arms.len());
                for arm in arms {
                    let inner = arm
                        .binds
                        .iter()
                        .fold(env.clone(), |acc, (n, _, _, _)| shadow(&acc, n));
                    out.push(crate::sema::MatchArmT {
                        tag: arm.tag,
                        binds: arm.binds.clone(),
                        body: self.reduce(&arm.body, &inner)?,
                    });
                }
                rb(Node::Match {
                    scrutinee: Box::new(s),
                    arms: out,
                })
            }
        })
    }

    /// Run a generator (a generation-stage expression) at compile time. `ins`
    /// collects this locus's `genlet` hoists.
    fn gen_code(&mut self, c: &Typed, env: &Env, ins: &mut Insertions) -> Result<GenVal, String> {
        Ok(match &c.node {
            Node::Int(n) => GenVal::Static(*n),
            Node::Float(bits) => GenVal::StaticFloat(*bits),
            Node::Bool(b) => GenVal::Static(i64::from(*b)),
            Node::Var(x) => env
                .get(x)
                .cloned()
                .ok_or_else(|| format!("staging: unbound generator variable `{x}`"))?,
            Node::Bin(op, a, b) => {
                if c.ty == Type::Float || a.ty == Type::Float || b.ty == Type::Float {
                    return Err(
                        "staging: float generator arithmetic is parsed and typechecked, but \
                         compile-time float evaluation is FPWork S3"
                            .into(),
                    );
                }
                let x = as_static(self.gen_code(a, env, ins)?)?;
                let y = as_static(self.gen_code(b, env, ins)?)?;
                GenVal::Static(eval_binop(*op, x, y)?)
            }
            Node::Cast(op, _) => {
                return Err(format!(
                    "staging: `{}` is parsed and typechecked, but compile-time numeric conversion is FPWork S3",
                    op.symbol()
                ))
            }
            Node::If(cond, t, e) => {
                if as_static(self.gen_code(cond, env, ins)?)? != 0 {
                    self.gen_code(t, env, ins)?
                } else {
                    self.gen_code(e, env, ins)?
                }
            }
            Node::Let { name, bound, body } => {
                let mut v = self.gen_code(bound, env, ins)?;
                if let GenVal::Closure { self_name, .. } = &mut v {
                    *self_name = Some(name.clone());
                }
                let mut env2 = env.clone();
                env2.insert(name.clone(), v);
                self.gen_code(body, &env2, ins)?
            }
            Node::Block { items, body } => {
                let mut env2 = env.clone();
                for item in items {
                    match item {
                        TypedBlockItem::Let { name, bound } => {
                            let mut v = self.gen_code(bound, &env2, ins)?;
                            if let GenVal::Closure { self_name, .. } = &mut v {
                                *self_name = Some(name.clone());
                            }
                            env2.insert(name.clone(), v);
                        }
                        TypedBlockItem::LetTuple { names, bound, .. } => {
                            let GenVal::Tuple(vals) = self.gen_code(bound, &env2, ins)? else {
                                return Err(
                                    "staging: a generation-stage block tuple binding needs a \
                                     statically-known tuple"
                                        .into(),
                                );
                            };
                            for (name, v) in names.iter().zip(vals.into_iter()) {
                                env2.insert(name.clone(), v);
                            }
                        }
                        TypedBlockItem::LetMut { .. } => {
                            return Err(
                                "staging: mutable cells are not available at the generation stage"
                                    .into(),
                            )
                        }
                    }
                }
                self.gen_code(body, &env2, ins)?
            }
            Node::Lam { param, body, .. } => GenVal::Closure {
                param: param.clone(),
                body: body.clone(),
                env: env.clone(),
                self_name: None,
            },
            Node::App { fun, arg } => {
                let f = self.gen_code(fun, env, ins)?;
                let a = self.gen_code(arg, env, ins)?;
                self.apply(f, a, ins)?
            }
            // `quote(E)` — residualize the stage-0 body (its own nested splices
            // form their own loci, so they manage their own insertions).
            Node::Quote(body) => GenVal::Code(self.reduce(body, env)?),
            // `genlet(c)` — hoist `c` to a shared `let` at this locus, return a
            // reference. δ's generative direction: the `Insert` distributes out.
            Node::Genlet(inner) => {
                let elem_ty = match &inner.ty {
                    Type::Code(t, _) => (**t).clone(),
                    other => other.clone(),
                };
                let code = as_code(self.gen_code(inner, env, ins)?, &elem_ty)?;
                let name = self.fresh();
                let reference = Typed {
                    ty: code.ty.clone(),
                    row: Row::pure(),
                    stage: 0,
                    layout_known: code.layout_known && code.ty.has_known_storage_layout(),
                    node: Node::Var(name.clone()),
                };
                ins.push((name, code));
                GenVal::Code(reference)
            }
            // `letloc { e }` — an explicit, *inner* locus: hoists inside it land
            // here, not at the enclosing splice.
            Node::Letloc(inner) => {
                let elem_ty = match &inner.ty {
                    Type::Code(t, _) => (**t).clone(),
                    other => other.clone(),
                };
                let mut inner_ins = Insertions::new();
                let body = as_code(self.gen_code(inner, env, &mut inner_ins)?, &elem_ty)?;
                GenVal::Code(wrap_insertions(body, inner_ins))
            }
            // A constructor applied at the generation stage builds a *static* sum
            // value (`Some(5)`) the generator can later `match` on to pick code.
            Node::Construct { tag, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for (a, _, _) in args {
                    vals.push(self.gen_code(a, env, ins)?);
                }
                GenVal::Sum {
                    tag: *tag,
                    args: vals,
                }
            }
            // A generation-stage `match`: the scrutinee must reduce to a static
            // constructor (`GenVal::Sum`), whose tag selects the arm; the arm's
            // binders take the constructor's fields positionally. A *runtime*
            // scrutinee belongs inside a `quote`, where `reduce` residualizes the
            // match into the generated code instead.
            Node::Match { scrutinee, arms } => {
                let GenVal::Sum { tag, args } = self.gen_code(scrutinee, env, ins)? else {
                    return Err(
                        "staging: a generation-stage `match` needs a statically-known \
                         constructor — `quote` the match to emit a runtime one"
                            .into(),
                    );
                };
                let arm = arms
                    .iter()
                    .find(|a| a.tag == Some(tag))
                    .or_else(|| arms.iter().find(|a| a.tag.is_none()))
                    .ok_or_else(|| {
                        format!("staging: no `match` arm covers constructor #{tag}")
                    })?;
                let mut env2 = env.clone();
                for ((name, _, _, _), v) in arm.binds.iter().zip(args.into_iter()) {
                    env2.insert(name.clone(), v);
                }
                self.gen_code(&arm.body, &env2, ins)?
            }
            // Static aggregates at the generation stage: build them, and
            // destructure/project them so a generator can carry structured
            // compile-time data.
            Node::Tuple(es) => {
                let mut vals = Vec::with_capacity(es.len());
                for e in es {
                    vals.push(self.gen_code(e, env, ins)?);
                }
                GenVal::Tuple(vals)
            }
            Node::LetTuple(names, e, body) => {
                let GenVal::Tuple(vals) = self.gen_code(e, env, ins)? else {
                    return Err(
                        "staging: a generation-stage `let (…) = e` needs a statically-known \
                         tuple — `quote` it to destructure at runtime"
                            .into(),
                    );
                };
                let mut env2 = env.clone();
                for (name, v) in names.iter().zip(vals.into_iter()) {
                    env2.insert(name.clone(), v);
                }
                self.gen_code(body, &env2, ins)?
            }
            Node::Record(fs) => {
                let mut vals = Vec::with_capacity(fs.len());
                for (name, v) in fs {
                    vals.push((name.clone(), self.gen_code(v, env, ins)?));
                }
                GenVal::Record(vals)
            }
            Node::Field(r, name) => {
                let GenVal::Record(fields) = self.gen_code(r, env, ins)? else {
                    return Err(
                        "staging: a generation-stage `.field` needs a statically-known record \
                         — `quote` it to project at runtime"
                            .into(),
                    );
                };
                fields
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v)
                    .ok_or_else(|| format!("staging: record has no field `{name}` at generation"))?
            }
            // Mutation at the GENERATION stage is out of scope for v1: the
            // generator has no mutable store, and a `let mut` / `:=` / a `Ref`
            // operator (`ref`/`!`/`:=`) belongs at the object stage (inside a
            // `quote`, where `reduce` residualizes it).
            Node::LetMut { .. }
            | Node::Assign { .. }
            | Node::RefNew { .. }
            | Node::Deref { .. }
            | Node::RefAssign { .. } => {
                return Err(
                    "staging: mutable cells are not available at the generation stage".into(),
                )
            }
            _ => {
                return Err(
                    "staging: this generator is not reducible yet — `let`, `if`, \
                            variables, static arithmetic, `match`/constructors, tuples/records, \
                            `quote`, (recursive) functions, and `genlet` / `letloc` are supported"
                        .into(),
                )
            }
        })
    }

    /// Apply a generation-stage closure `f` to `a`. A `self` name (`let rec`) is
    /// re-bound so the function can call itself.
    fn apply(&mut self, f: GenVal, a: GenVal, ins: &mut Insertions) -> Result<GenVal, String> {
        // Charge the expansion budget — a recursive generator drives itself
        // through here, so this is where a non-terminating one is caught (F14).
        self.charge_step()?;
        let GenVal::Closure {
            param,
            body,
            env,
            self_name,
        } = f
        else {
            return Err("staging: applied a non-function value in a generator".into());
        };
        let mut call_env = env.clone();
        call_env.insert(param.clone(), a);
        if let Some(name) = &self_name {
            call_env.insert(
                name.clone(),
                GenVal::Closure {
                    param: param.clone(),
                    body: body.clone(),
                    env: env.clone(),
                    self_name: self_name.clone(),
                },
            );
        }
        self.gen_code(&body, &call_env, ins)
    }
}

/// `let n1 = c1 in let n2 = c2 in … residual` — the hoisted `genlet` bindings.
fn wrap_insertions(residual: Typed, ins: Insertions) -> Typed {
    ins.into_iter()
        .rev()
        .fold(residual, |body, (name, code)| Typed {
            ty: body.ty.clone(),
            row: code.row.union(&body.row),
            stage: 0,
            layout_known: code.layout_known && body.layout_known,
            node: Node::Let {
                name,
                bound: Box::new(code),
                body: Box::new(body),
            },
        })
}

fn shadow(env: &Env, name: &str) -> Env {
    let mut e = env.clone();
    e.remove(name);
    e
}

fn as_static(g: GenVal) -> Result<i64, String> {
    match g {
        GenVal::Static(n) => Ok(n),
        GenVal::StaticFloat(_) => {
            Err("staging: expected a static Int/Bool value here, but got a Float".into())
        }
        _ => Err("staging: expected a static value here, but got code or a function".into()),
    }
}

fn as_code(g: GenVal, ty: &Type) -> Result<Typed, String> {
    Ok(match g {
        GenVal::Code(t) => t,
        GenVal::Static(n) => {
            // Same D6/D7 defence as the cross-stage-constant site: a residual
            // `Var` here would silently residualize an `Int`; refuse instead.
            if matches!(ty, Type::Var(_)) {
                return Err("staging: a static value has an un-zonked type variable — \
                            cannot tell a `Bool` literal from an `Int` one"
                    .into());
            }
            let node = if *ty == Type::Bool {
                Node::Bool(n != 0)
            } else {
                Node::Int(n)
            };
            Typed {
                ty: ty.clone(),
                row: Row::pure(),
                stage: 0,
                layout_known: ty.has_known_storage_layout(),
                node,
            }
        }
        GenVal::StaticFloat(bits) => {
            if *ty != Type::Float {
                return Err(format!(
                    "staging: a static Float value cannot be spliced as `{ty}`"
                ));
            }
            Typed {
                ty: ty.clone(),
                row: Row::pure(),
                stage: 0,
                layout_known: ty.has_known_storage_layout(),
                node: Node::Float(bits),
            }
        }
        GenVal::Sum { .. } => {
            return Err(
                "staging: a generator produced a constructor value, not code — `quote` it \
                 (or `match` on it) to produce code"
                    .into(),
            )
        }
        GenVal::Tuple(_) | GenVal::Record(_) => {
            return Err(
                "staging: a generator produced an aggregate value, not code — `quote` it \
                 (or destructure/project it) to produce code"
                    .into(),
            )
        }
        GenVal::Closure { .. } => {
            return Err(
                "staging: a generator produced a function, not code — cannot splice it".into(),
            )
        }
    })
}

fn eval_binop(op: BinOp, x: i64, y: i64) -> Result<i64, String> {
    Ok(match op {
        BinOp::Add | BinOp::AddWrap => x.wrapping_add(y),
        BinOp::Sub | BinOp::SubWrap => x.wrapping_sub(y),
        BinOp::Mul | BinOp::MulWrap => x.wrapping_mul(y),
        BinOp::Div | BinOp::Mod => {
            return Err(
                "staging: integer division/remainder is parsed and typechecked, but \
                 compile-time division is FPWork S3"
                    .into(),
            )
        }
        BinOp::AddChecked => x
            .checked_add(y)
            .ok_or_else(|| "staging: checked integer overflow in `+?`".to_string())?,
        BinOp::SubChecked => x
            .checked_sub(y)
            .ok_or_else(|| "staging: checked integer overflow in `-?`".to_string())?,
        BinOp::MulChecked => x
            .checked_mul(y)
            .ok_or_else(|| "staging: checked integer overflow in `*?`".to_string())?,
        BinOp::And => x & y,
        BinOp::Or => x | y,
        BinOp::Xor => x ^ y,
        BinOp::Shl => x << y,
        BinOp::Shr => x >> y,
        BinOp::Eq => i64::from(x == y),
        BinOp::Ne => i64::from(x != y),
        BinOp::Lt => i64::from(x < y),
        BinOp::Le => i64::from(x <= y),
        BinOp::Gt => i64::from(x > y),
        BinOp::Ge => i64::from(x >= y),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{Ctx, Sig};
    use crate::{elaborate, parse};

    fn reduce_src(src: &str) -> Result<Typed, String> {
        let term = parse(src).map_err(|e| e.msg)?;
        let tree = elaborate(&Sig::new(), &Ctx::new(), 0, &term).map_err(|e| e.to_string())?;
        stage_reduce(&tree)
    }

    /// Reduce with a small explicit expansion budget, to test the `RN-E0332`
    /// guard deterministically without waiting on the full default budget. Runs on
    /// the same large-stack worker as `stage_reduce`, so the budget — not the
    /// stack — is what trips on a non-terminating generator.
    fn reduce_src_with_fuel(src: &str, budget: u64) -> Result<Typed, String> {
        let term = parse(src).map_err(|e| e.msg)?;
        let tree = elaborate(&Sig::new(), &Ctx::new(), 0, &term).map_err(|e| e.to_string())?;
        super::run_reduce(&tree, budget)
    }

    #[test]
    fn non_staging_passes_through() {
        let r = reduce_src("1 + 2").unwrap();
        assert!(matches!(r.node, Node::Bin(BinOp::Add, ..)));
        assert_eq!(r.ty, Type::Int);
    }

    #[test]
    fn splice_quote_cancels() {
        assert!(matches!(
            reduce_src("${ quote(7) }").unwrap().node,
            Node::Int(7)
        ));
    }

    #[test]
    fn a_static_if_selects_a_branch() {
        assert!(matches!(
            reduce_src("${ if 1 < 2 then quote(10) else quote(20) }")
                .unwrap()
                .node,
            Node::Int(10)
        ));
        assert!(matches!(
            reduce_src("${ if 2 < 1 then quote(10) else quote(20) }")
                .unwrap()
                .node,
            Node::Int(20)
        ));
    }

    #[test]
    fn the_residual_keeps_runtime_code() {
        assert!(matches!(
            reduce_src("${ quote(2 + 3) }").unwrap().node,
            Node::Bin(BinOp::Add, ..)
        ));
    }

    #[test]
    fn a_cross_stage_constant_becomes_a_literal() {
        assert!(matches!(
            reduce_src("${ let m = 7 in quote(m) }").unwrap().node,
            Node::Int(7)
        ));
    }

    #[test]
    fn a_float_cross_stage_constant_becomes_a_literal() {
        match reduce_src("${ let f = 1.5 in quote(f) }").unwrap().node {
            Node::Float(bits) => assert_eq!(bits, 1.5f64.to_bits()),
            other => panic!("expected a float literal, got {other:?}"),
        }
    }

    #[test]
    fn staged_float_arithmetic_reports_the_fpwork_gap() {
        let err = reduce_src("${ let f = 1.0 + 2.0 in quote(f) }")
            .expect_err("float generator arithmetic is not implemented yet");
        assert!(err.contains("float generator arithmetic"), "{err}");
        assert!(err.contains("FPWork S3"), "{err}");
    }

    #[test]
    fn staged_explicit_wrapping_arithmetic_wraps() {
        match reduce_src("${ let m = 9223372036854775807 +% 1 in quote(m) }")
            .unwrap()
            .node
        {
            Node::Int(n) => assert_eq!(n, i64::MIN),
            other => panic!("expected wrapped literal, got {other:?}"),
        }
    }

    #[test]
    fn staged_checked_arithmetic_reports_overflow() {
        let err = reduce_src("${ let m = 9223372036854775807 +? 1 in quote(m) }")
            .expect_err("checked overflow should fail during generation");
        assert!(err.contains("checked integer overflow in `+?`"), "{err}");
    }

    #[test]
    fn a_generator_let_parameterizes_the_code() {
        match reduce_src("${ let n = 3 in if n < 5 then quote(n * 10) else quote(0) }")
            .unwrap()
            .node
        {
            Node::Bin(BinOp::Mul, a, b) => {
                assert!(matches!(a.node, Node::Int(3)));
                assert!(matches!(b.node, Node::Int(10)));
            }
            other => panic!("expected `3 * 10`, got {other:?}"),
        }
    }

    #[test]
    fn a_code_valued_variable_is_spliced() {
        match reduce_src("${ let c = quote(5) in quote(${c} + 1) }")
            .unwrap()
            .node
        {
            Node::Bin(BinOp::Add, a, b) => {
                assert!(matches!(a.node, Node::Int(5)));
                assert!(matches!(b.node, Node::Int(1)));
            }
            other => panic!("expected `5 + 1`, got {other:?}"),
        }
    }

    #[test]
    fn a_recursive_generator_bottoms_out() {
        // A `let rec` generator that recurses with a static base case runs to that
        // base at generation time: f 3 → f 2 → f 1 → f 0 → quote(0) ⇒ Int(0).
        let r = reduce_src(
            "${ let rec f : Int -> Code[Int] = fn n: Int => \
                  if n < 1 then quote(0) else f (n - 1) in f 3 }",
        )
        .expect("a bottoming-out recursive generator reduces");
        assert!(matches!(r.node, Node::Int(0)), "got {:?}", r.node);
    }

    #[test]
    fn a_nonterminating_generator_hits_the_expansion_limit() {
        // No reachable base case — `f` recurses forever at generation time. The
        // expansion budget (RN-E0332, F14) stops it instead of hanging the compiler.
        let err = reduce_src_with_fuel(
            "${ let rec f : Int -> Code[Int] = fn n: Int => f (n + 1) in f 0 }",
            500,
        )
        .expect_err("a non-terminating generator must be caught, not hang");
        assert!(err.contains("RN-E0332"), "got {err}");
        assert!(err.contains("non-terminating"), "got {err}");
    }

    #[test]
    fn a_deep_but_finite_generator_stays_within_budget() {
        // The guard must NOT fire on a legitimately deep-but-terminating generator:
        // 50 recursive steps with a generous budget still reduces to the base.
        let r = reduce_src_with_fuel(
            "${ let rec f : Int -> Code[Int] = fn n: Int => \
                  if n < 1 then quote(0) else f (n - 1) in f 50 }",
            10_000,
        )
        .expect("a deep but finite generator stays within budget");
        assert!(matches!(r.node, Node::Int(0)), "got {:?}", r.node);
    }

    #[test]
    fn a_generator_matches_a_static_constructor_to_pick_code() {
        // The generator inspects a statically-known `So(5)` and emits the code
        // from the matching arm: `match So(5) with So(x) => quote(x*10) | …`
        // ⇒ residual `5 * 10`. (`reduce_src` doesn't graft the stdlib, so the test
        // declares its own sum type.)
        match reduce_src(
            "type Opt = Non | So(Int) in \
             ${ let o = So(5) in match o with | So(x) => quote(x * 10) | Non => quote(0) }",
        )
        .expect("a gen-stage match on a static So reduces")
        .node
        {
            Node::Bin(BinOp::Mul, a, b) => {
                assert!(matches!(a.node, Node::Int(5)));
                assert!(matches!(b.node, Node::Int(10)));
            }
            other => panic!("expected `5 * 10`, got {other:?}"),
        }
        // The other arm is selected for the other constructor.
        assert!(matches!(
            reduce_src(
                "type Opt = Non | So(Int) in \
                 ${ let o = Non in match o with | So(x) => quote(x) | Non => quote(0) }"
            )
            .expect("a gen-stage match on a static Non reduces")
            .node,
            Node::Int(0)
        ));
    }

    #[test]
    fn a_generator_destructures_a_static_tuple() {
        // `let (a, b) = (3, 4)` at the generation stage binds a=3, b=4; the static
        // `if a < b` then selects `quote(a)` ⇒ residual `Int(3)`.
        assert!(matches!(
            reduce_src("${ let (a, b) = (3, 4) in if a < b then quote(a) else quote(b) }")
                .expect("a gen-stage tuple destructure reduces")
                .node,
            Node::Int(3)
        ));
    }

    #[test]
    fn a_generator_projects_a_static_record_field() {
        // `{ x = 7, y = 9 }.x` projected at the generation stage is the static 7,
        // spliced as `Int(7)`.
        assert!(matches!(
            reduce_src("${ let r = { x = 7, y = 9 } in let v = r.x in quote(v) }")
                .expect("a gen-stage record projection reduces")
                .node,
            Node::Int(7)
        ));
    }

    #[test]
    fn genlet_hoists_a_shared_binding() {
        // `${ genlet(quote(5)) }` hoists `5` to a fresh `let` at the splice and
        // returns a reference: `let __g0 = 5 in __g0`.
        match reduce_src("${ genlet(quote(5)) }").unwrap().node {
            Node::Let { bound, body, .. } => {
                assert!(matches!(bound.node, Node::Int(5)), "binding is 5");
                assert!(matches!(body.node, Node::Var(_)), "body is a reference");
            }
            other => panic!("expected a hoisted let, got {other:?}"),
        }
    }
}
