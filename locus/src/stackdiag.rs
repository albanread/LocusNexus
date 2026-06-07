//! Stack-pressure diagnostics for compiler pipeline inputs.
//!
//! These counters describe the *shape* of the compiler's recursive work:
//! total nodes, maximum tree depth, long binding spines, application spines, and
//! nested type depth. They intentionally use explicit worklists so tracing stack
//! pressure does not itself consume the recursive stack we are investigating.

use crate::ir::{Comp, Ir};
use crate::sema::{Node, Typed};
use crate::syntax::{BlockItem, Constraint, ProgramSource, Term, Type};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShapeMetrics {
    pub nodes: usize,
    pub max_depth: usize,
    pub max_binding_spine: usize,
    pub max_app_spine: usize,
    pub max_type_depth: usize,
}

impl ShapeMetrics {
    fn observe_node(&mut self, depth: usize) {
        self.nodes += 1;
        self.max_depth = self.max_depth.max(depth);
    }

    fn observe_binding_spine(&mut self, spine: usize) {
        self.max_binding_spine = self.max_binding_spine.max(spine);
    }

    fn observe_app_spine(&mut self, spine: usize) {
        self.max_app_spine = self.max_app_spine.max(spine);
    }

    fn observe_type(&mut self, ty: &Type) {
        let mut stack = vec![(ty, 1usize)];
        while let Some((ty, depth)) = stack.pop() {
            self.max_type_depth = self.max_type_depth.max(depth);
            let next = depth + 1;
            match ty {
                Type::Vector(_, inner) | Type::Array(inner) => stack.push((inner, next)),
                Type::Fun(a, b, _) => {
                    stack.push((a, next));
                    stack.push((b, next));
                }
                Type::Code(inner, _) => stack.push((inner, next)),
                Type::Tuple(items) => {
                    for item in items {
                        stack.push((item, next));
                    }
                }
                Type::Record(fields) => {
                    for (_, item) in fields {
                        stack.push((item, next));
                    }
                }
                Type::Named(_, args) => {
                    for arg in args {
                        stack.push((arg, next));
                    }
                }
                Type::Var(_)
                | Type::Int
                | Type::Float
                | Type::Float32
                | Type::Mask(_)
                | Type::Bool
                | Type::Unit
                | Type::Str
                | Type::I32
                | Type::U32
                | Type::Ptr => {}
            }
        }
    }
}

/// Shape of a parsed user source before stdlib grafting.
pub fn program_source_shape(program: &ProgramSource) -> ShapeMetrics {
    let mut out = ShapeMetrics::default();
    observe_term_into(&mut out, &program.entry);
    for module in &program.modules {
        observe_term_into(&mut out, &module.body);
    }
    out
}

/// Shape of a core term, typically after stdlib grafting.
pub fn term_shape(term: &Term) -> ShapeMetrics {
    let mut out = ShapeMetrics::default();
    observe_term_into(&mut out, term);
    out
}

/// Shape of the elaborated typed tree.
pub fn typed_shape(tree: &Typed) -> ShapeMetrics {
    let mut out = ShapeMetrics::default();
    observe_typed_into(&mut out, tree);
    out
}

/// Shape of the current ANF IR.
pub fn ir_shape(ir: &Ir) -> ShapeMetrics {
    enum Work<'a> {
        Ir(&'a Ir, usize, usize),
        Comp(&'a Comp, usize),
    }

    let mut out = ShapeMetrics::default();
    let mut stack = vec![Work::Ir(ir, 1, 0)];
    while let Some(work) = stack.pop() {
        match work {
            Work::Ir(ir, depth, binding_spine) => {
                out.observe_node(depth);
                match ir {
                    Ir::Block { binds, comp, .. } => {
                        for (i, bind) in binds.iter().enumerate() {
                            out.observe_type(&bind.ty);
                            out.observe_binding_spine(binding_spine + i + 1);
                            stack.push(Work::Comp(&bind.comp, depth + 1));
                        }
                        stack.push(Work::Comp(comp, depth + 1));
                    }
                    Ir::Let { ty, comp, rest, .. } => {
                        out.observe_type(ty);
                        let spine = binding_spine + 1;
                        out.observe_binding_spine(spine);
                        stack.push(Work::Ir(rest, depth + 1, spine));
                        stack.push(Work::Comp(comp, depth + 1));
                    }
                    Ir::Ret { comp, .. } => stack.push(Work::Comp(comp, depth + 1)),
                }
            }
            Work::Comp(comp, depth) => {
                out.observe_node(depth);
                match comp {
                    Comp::App { arg_ty, ret_ty, .. } => {
                        out.observe_type(arg_ty);
                        out.observe_type(ret_ty);
                    }
                    Comp::Call {
                        args,
                        fun_ty,
                        ret_ty,
                        ..
                    } => {
                        out.observe_type(fun_ty);
                        out.observe_type(ret_ty);
                        for (_, ty) in args {
                            out.observe_type(ty);
                        }
                    }
                    Comp::FloatMathUnary { ty, .. }
                    | Comp::FloatMathBinary { ty, .. }
                    | Comp::FloatMathTernary { ty, .. } => out.observe_type(ty),
                    Comp::VectorLit { elem_ty, .. }
                    | Comp::VectorSplat { elem_ty, .. }
                    | Comp::VectorLoad { elem_ty, .. }
                    | Comp::VectorStore { elem_ty, .. }
                    | Comp::VectorBin { elem_ty, .. }
                    | Comp::VectorCompare { elem_ty, .. } => out.observe_type(elem_ty),
                    Comp::Proj { ty, .. } => out.observe_type(ty),
                    Comp::Lam {
                        param_ty,
                        ret_ty,
                        body,
                        ..
                    } => {
                        if let Some(param_ty) = param_ty {
                            out.observe_type(param_ty);
                        }
                        out.observe_type(ret_ty);
                        stack.push(Work::Ir(body, depth + 1, 0));
                    }
                    Comp::If(_, then_ir, else_ir) => {
                        stack.push(Work::Ir(else_ir, depth + 1, 0));
                        stack.push(Work::Ir(then_ir, depth + 1, 0));
                    }
                    Comp::Loop {
                        vars,
                        cond,
                        steps,
                        result,
                    } => {
                        for var in vars {
                            out.observe_type(&var.ty);
                        }
                        stack.push(Work::Ir(result, depth + 1, 0));
                        for step in steps {
                            stack.push(Work::Ir(step, depth + 1, 0));
                        }
                        stack.push(Work::Ir(cond, depth + 1, 0));
                    }
                    Comp::Handle {
                        scrutinee, handler, ..
                    } => {
                        stack.push(Work::Ir(scrutinee, depth + 1, 0));
                        for op in &handler.ops {
                            out.observe_type(&op.arg_ty);
                            out.observe_type(&op.resume_ty);
                            stack.push(Work::Ir(&op.body, depth + 1, 0));
                        }
                        out.observe_type(&handler.ret.var_ty);
                        out.observe_type(&handler.ret.body_ty);
                        stack.push(Work::Ir(&handler.ret.body, depth + 1, 0));
                    }
                    Comp::Quote(body) | Comp::Letloc(body) => {
                        stack.push(Work::Ir(body, depth + 1, 0));
                    }
                    Comp::Atom(_)
                    | Comp::Brk
                    | Comp::Extern(..)
                    | Comp::Foreign(..)
                    | Comp::Bin(..)
                    | Comp::FloatBin(..)
                    | Comp::Cast(..)
                    | Comp::Tag(_)
                    | Comp::Untag(_)
                    | Comp::ToPtr(_)
                    | Comp::FromPtr(_)
                    | Comp::VectorSelect { .. }
                    | Comp::MaskReduce { .. }
                    | Comp::VectorExtract { .. }
                    | Comp::Perform(..)
                    | Comp::Splice(_)
                    | Comp::Genlet(_)
                    | Comp::Peek(..)
                    | Comp::Poke(..)
                    | Comp::Fill(..)
                    | Comp::Copy(..)
                    | Comp::Tuple(_)
                    | Comp::ArrayLit { .. }
                    | Comp::Len(_)
                    | Comp::ArrayGet { elem_ty: _, .. }
                    | Comp::ArraySet { .. }
                    | Comp::SlotInit(..)
                    | Comp::SlotLoad(_)
                    | Comp::SlotStore(..)
                    | Comp::RefNew(..)
                    | Comp::RefGet(..)
                    | Comp::RefSet(..) => {}
                }
            }
        }
    }
    out
}

fn observe_constraints(out: &mut ShapeMetrics, constraints: &[Constraint]) {
    for constraint in constraints {
        out.observe_type(&constraint.ty);
    }
}

fn observe_block_item<'a>(
    out: &mut ShapeMetrics,
    stack: &mut Vec<(&'a Term, usize, usize, usize)>,
    item: &'a BlockItem,
    depth: usize,
) {
    out.observe_node(depth);
    let child = depth + 1;
    match item {
        BlockItem::Let(_, bound) | BlockItem::LetMut(_, bound) | BlockItem::LetTuple(_, bound) => {
            stack.push((bound, child, 0, 0));
        }
        BlockItem::LetRec(_, ty, bound) => {
            out.observe_type(ty);
            stack.push((bound, child, 0, 0));
        }
        BlockItem::Effect { ops, .. } => {
            for op in ops {
                out.observe_type(&op.param);
                out.observe_type(&op.result);
            }
        }
        BlockItem::TypeDef { variants, .. } => {
            for (_, fields) in variants {
                for field in fields {
                    out.observe_type(field);
                }
            }
        }
        BlockItem::Trait {
            supers, methods, ..
        } => {
            observe_constraints(out, supers);
            for method in methods {
                out.observe_type(&method.sig);
            }
        }
        BlockItem::Instance {
            head,
            requires,
            methods,
            ..
        } => {
            out.observe_type(head);
            observe_constraints(out, requires);
            for method in methods {
                stack.push((&method.body, child, 0, 0));
            }
        }
        BlockItem::Scope { .. } => {
            unreachable!("Scope items are flattened by the Term::Block arm before observation")
        }
    }
}

/// Splice the runtime-transparent `BlockItem::Scope` graft markers into their leaf
/// items, so the shape diagnostic measures the *flattened* structure elaboration
/// actually sees (Scope adds no node/level — `elaborate_block` flattens it).
fn flatten_scope_items<'a>(items: &'a [BlockItem], out: &mut Vec<&'a BlockItem>) {
    for it in items {
        match it {
            BlockItem::Scope { items, .. } => flatten_scope_items(items, out),
            other => out.push(other),
        }
    }
}

fn observe_term_into(out: &mut ShapeMetrics, root: &Term) {
    let mut stack = vec![(root, 1usize, 0usize, 0usize)];
    while let Some((term, depth, binding_spine, app_spine)) = stack.pop() {
        out.observe_node(depth);
        let child = depth + 1;
        match term {
            Term::Var(_)
            | Term::Int(_)
            | Term::Float(_)
            | Term::Bool(_)
            | Term::Unit
            | Term::Brk
            | Term::Str(_) => {}
            Term::Extern(_, ty, _) => {
                if let Some(ty) = ty {
                    out.observe_type(ty);
                }
            }
            Term::ExternAsm(_, ty) => out.observe_type(ty),
            Term::Bin(_, a, b)
            | Term::Dot(a, b)
            | Term::App(a, b)
            | Term::Poke(_, a, b)
            | Term::Index(a, b) => {
                if matches!(term, Term::App(..)) {
                    let spine = app_spine + 1;
                    out.observe_app_spine(spine);
                    stack.push((b, child, 0, 0));
                    stack.push((a, child, 0, spine));
                } else {
                    stack.push((b, child, 0, 0));
                    stack.push((a, child, 0, 0));
                }
            }
            Term::Cast(_, a)
            | Term::Sqrt(a)
            | Term::Sum(a)
            | Term::Length(a)
            | Term::MaskReduce(_, a)
            | Term::VectorSplat(_, a)
            | Term::Assign(_, a)
            | Term::RefNew(a)
            | Term::Deref(a)
            | Term::Perform(_, a)
            | Term::Seal(_, a)
            | Term::Quote(a)
            | Term::Splice(a)
            | Term::Genlet(a)
            | Term::Letloc(a)
            | Term::Peek(_, a)
            | Term::Field(a, _)
            | Term::Len(a) => stack.push((a, child, 0, 0)),
            Term::Fma(a, b, c)
            | Term::Select(a, b, c)
            | Term::Fill(a, b, c)
            | Term::Copy(a, b, c)
            | Term::IndexSet(a, b, c) => {
                stack.push((c, child, 0, 0));
                stack.push((b, child, 0, 0));
                stack.push((a, child, 0, 0));
            }
            Term::VectorLit(_, elems) | Term::Tuple(elems) | Term::ArrayLit(elems) => {
                for elem in elems {
                    stack.push((elem, child, 0, 0));
                }
            }
            Term::VectorLoad { arr, idx, .. } => {
                stack.push((idx, child, 0, 0));
                stack.push((arr, child, 0, 0));
            }
            Term::VectorStore {
                arr, idx, value, ..
            } => {
                stack.push((value, child, 0, 0));
                stack.push((idx, child, 0, 0));
                stack.push((arr, child, 0, 0));
            }
            Term::If(c, t, e) => {
                stack.push((e, child, 0, 0));
                stack.push((t, child, 0, 0));
                stack.push((c, child, 0, 0));
            }
            Term::Loop {
                vars,
                cond,
                steps,
                result,
            } => {
                stack.push((result, child, 0, 0));
                for step in steps {
                    stack.push((step, child, 0, 0));
                }
                stack.push((cond, child, 0, 0));
                for (_, init) in vars {
                    stack.push((init, child, 0, 0));
                }
            }
            Term::Lam(_, ty, body) => {
                if let Some(ty) = ty {
                    out.observe_type(ty);
                }
                stack.push((body, child, 0, 0));
            }
            Term::Let(_, bound, body) | Term::LetMut(_, bound, body) => {
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
                stack.push((bound, child, 0, 0));
            }
            Term::LetRec(_, ty, bound, body) => {
                out.observe_type(ty);
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
                stack.push((bound, child, 0, 0));
            }
            Term::Block(items, body) => {
                let mut flat: Vec<&BlockItem> = Vec::new();
                flatten_scope_items(items, &mut flat);
                let spine = binding_spine + flat.len();
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
                for item in flat {
                    observe_block_item(out, &mut stack, item, child);
                }
            }
            Term::Handle(scrutinee, handler) => {
                stack.push((scrutinee, child, 0, 0));
                for op in &handler.ops {
                    stack.push((&op.body, child, 0, 0));
                }
                stack.push((&handler.ret.body, child, 0, 0));
            }
            Term::Effect { ops, body, .. } => {
                for op in ops {
                    out.observe_type(&op.param);
                    out.observe_type(&op.result);
                }
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
            }
            Term::Trait {
                supers,
                methods,
                body,
                ..
            } => {
                observe_constraints(out, supers);
                for method in methods {
                    out.observe_type(&method.sig);
                }
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
            }
            Term::Instance {
                head,
                requires,
                methods,
                body,
                ..
            } => {
                out.observe_type(head);
                observe_constraints(out, requires);
                for method in methods {
                    stack.push((&method.body, child, 0, 0));
                }
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
            }
            Term::Record(fields) => {
                for (_, value) in fields {
                    stack.push((value, child, 0, 0));
                }
            }
            Term::LetTuple(_, bound, body) => {
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
                stack.push((bound, child, 0, 0));
            }
            Term::TypeDef { variants, body, .. } => {
                for (_, fields) in variants {
                    for field in fields {
                        out.observe_type(field);
                    }
                }
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
            }
            Term::Construct(_, args) => {
                for arg in args {
                    stack.push((arg, child, 0, 0));
                }
            }
            Term::Match { scrutinee, arms } => {
                for arm in arms {
                    stack.push((&arm.body, child, 0, 0));
                }
                stack.push((scrutinee, child, 0, 0));
            }
        }
    }
}

fn observe_typed_into(out: &mut ShapeMetrics, root: &Typed) {
    let mut stack = vec![(root, 1usize, 0usize, 0usize)];
    while let Some((typed, depth, binding_spine, app_spine)) = stack.pop() {
        out.observe_node(depth);
        out.observe_type(&typed.ty);
        let child = depth + 1;
        match &typed.node {
            Node::Var(_)
            | Node::Int(_)
            | Node::Float(_)
            | Node::Bool(_)
            | Node::Unit
            | Node::Brk
            | Node::Str(_)
            | Node::Extern(..) => {}
            Node::Bin(_, a, b)
            | Node::FloatMathBinary(_, a, b)
            | Node::Index(_, a, b)
            | Node::Poke(_, a, b) => {
                stack.push((b, child, 0, 0));
                stack.push((a, child, 0, 0));
            }
            Node::App { fun, arg } => {
                let spine = app_spine + 1;
                out.observe_app_spine(spine);
                stack.push((arg, child, 0, 0));
                stack.push((fun, child, 0, spine));
            }
            Node::If(a, b, c)
            | Node::Fill(a, b, c)
            | Node::Copy(a, b, c)
            | Node::FloatMathTernary(_, a, b, c)
            | Node::IndexSet(_, a, b, c) => {
                stack.push((c, child, 0, 0));
                stack.push((b, child, 0, 0));
                stack.push((a, child, 0, 0));
            }
            Node::Loop {
                vars,
                cond,
                steps,
                result,
            } => {
                stack.push((result, child, 0, 0));
                for step in steps {
                    stack.push((step, child, 0, 0));
                }
                stack.push((cond, child, 0, 0));
                for (_, ty, _, init) in vars {
                    out.observe_type(ty);
                    stack.push((init, child, 0, 0));
                }
            }
            Node::VectorSelect {
                mask,
                then_value,
                else_value,
            } => {
                stack.push((else_value, child, 0, 0));
                stack.push((then_value, child, 0, 0));
                stack.push((mask, child, 0, 0));
            }
            Node::Lam { param_ty, body, .. } => {
                out.observe_type(param_ty);
                stack.push((body, child, 0, 0));
            }
            Node::Perform { arg, .. }
            | Node::Quote(arg)
            | Node::Splice(arg)
            | Node::Genlet(arg)
            | Node::Letloc(arg)
            | Node::Cast(_, arg)
            | Node::FloatMathUnary(_, arg)
            | Node::MaskReduce(_, arg)
            | Node::Peek(_, arg)
            | Node::Len(arg)
            | Node::Field(arg, _) => stack.push((arg, child, 0, 0)),
            Node::Coerce {
                slot, value, inner, ..
            } => {
                out.observe_type(slot);
                out.observe_type(value);
                stack.push((inner, child, 0, 0));
            }
            Node::VectorSplat { value, .. } | Node::VectorExtract { vector: value, .. } => {
                stack.push((value, child, 0, 0));
            }
            Node::VectorLit { elems, .. } | Node::Tuple(elems) | Node::ArrayLit { elems, .. } => {
                for elem in elems {
                    stack.push((elem, child, 0, 0));
                }
            }
            Node::VectorLoad { arr, idx, .. } | Node::ArrayGet { arr, idx, .. } => {
                stack.push((idx, child, 0, 0));
                stack.push((arr, child, 0, 0));
            }
            Node::VectorStore {
                arr, idx, value, ..
            } => {
                stack.push((value, child, 0, 0));
                stack.push((idx, child, 0, 0));
                stack.push((arr, child, 0, 0));
            }
            Node::ArraySet { arr, idx, val, .. } => {
                stack.push((val, child, 0, 0));
                stack.push((idx, child, 0, 0));
                stack.push((arr, child, 0, 0));
            }
            Node::Let { bound, body, .. } | Node::LetMut { bound, body, .. } => {
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
                stack.push((bound, child, 0, 0));
            }
            Node::Block { items, body } => {
                let spine = binding_spine + items.len();
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
                for item in items {
                    match item {
                        crate::sema::TypedBlockItem::Let { bound, .. }
                        | crate::sema::TypedBlockItem::LetMut { bound, .. }
                        | crate::sema::TypedBlockItem::LetTuple { bound, .. } => {
                            stack.push((bound, child, 0, 0));
                        }
                    }
                }
            }
            Node::LetTuple(_, bound, body) => {
                let spine = binding_spine + 1;
                out.observe_binding_spine(spine);
                stack.push((body, child, spine, 0));
                stack.push((bound, child, 0, 0));
            }
            Node::Assign { value, .. } | Node::RefNew { value } => {
                stack.push((value, child, 0, 0));
            }
            Node::Deref { cell } => stack.push((cell, child, 0, 0)),
            Node::RefAssign { target, value } => {
                stack.push((value, child, 0, 0));
                stack.push((target, child, 0, 0));
            }
            Node::Record(fields) => {
                for (_, value) in fields {
                    stack.push((value, child, 0, 0));
                }
            }
            Node::Construct { args, .. } => {
                for (arg, _, ty) in args {
                    out.observe_type(ty);
                    stack.push((arg, child, 0, 0));
                }
            }
            Node::Match { scrutinee, arms } => {
                for arm in arms {
                    for (_, _, _, ty) in &arm.binds {
                        out.observe_type(ty);
                    }
                    stack.push((&arm.body, child, 0, 0));
                }
                stack.push((scrutinee, child, 0, 0));
            }
            Node::Handle { scrutinee, handler } => {
                stack.push((scrutinee, child, 0, 0));
                for op in &handler.ops {
                    out.observe_type(&op.arg_ty);
                    out.observe_type(&op.resume_ty);
                    stack.push((&op.body, child, 0, 0));
                }
                out.observe_type(&handler.ret.var_ty);
                out.observe_type(&handler.ret.body_ty);
                stack.push((&handler.ret.body, child, 0, 0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{Row, Term};

    #[test]
    fn term_shape_tracks_long_binding_and_app_spines() {
        let term = Term::Let(
            "a".into(),
            Box::new(Term::Int(1)),
            Box::new(Term::Let(
                "b".into(),
                Box::new(Term::Int(2)),
                Box::new(Term::App(
                    Box::new(Term::App(
                        Box::new(Term::Var("f".into())),
                        Box::new(Term::Var("x".into())),
                    )),
                    Box::new(Term::Var("y".into())),
                )),
            )),
        );

        let shape = term_shape(&term);
        assert_eq!(shape.max_binding_spine, 2);
        assert_eq!(shape.max_app_spine, 2);
        assert!(shape.max_depth >= 3);
    }

    #[test]
    fn term_shape_treats_block_width_as_binding_spine_not_depth() {
        let term = Term::Block(
            vec![
                BlockItem::Let("a".into(), Term::Int(1)),
                BlockItem::Let("b".into(), Term::Int(2)),
                BlockItem::Let("c".into(), Term::Int(3)),
            ],
            Box::new(Term::Var("c".into())),
        );

        let shape = term_shape(&term);
        assert_eq!(shape.max_binding_spine, 3);
        assert!(shape.max_depth <= 3, "{shape:?}");
    }

    #[test]
    fn ir_shape_treats_flat_anf_blocks_as_spine_not_depth() {
        let term = crate::stdlib::program("clock_millis ()").expect("stdlib program parses");
        let typed = crate::elaborate(&crate::Sig::new(), &crate::Ctx::new(), 0, &term)
            .expect("stdlib program type-checks");
        let reduced = crate::stage_reduce(&typed).expect("stage reduction succeeds");
        let ir = crate::lower(&reduced);
        let shape = ir_shape(&ir);

        assert!(shape.max_binding_spine > 20, "{shape:?}");
        assert!(shape.max_depth < shape.max_binding_spine, "{shape:?}");
    }

    #[test]
    fn type_shape_counts_nested_arrows() {
        let mut shape = ShapeMetrics::default();
        let ty = Type::Fun(
            Box::new(Type::Int),
            Box::new(Type::Fun(
                Box::new(Type::Bool),
                Box::new(Type::Int),
                Row::pure(),
            )),
            Row::pure(),
        );
        shape.observe_type(&ty);
        assert_eq!(shape.max_type_depth, 3);
    }
}
