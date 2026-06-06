//! Lower the Locus **ANF IR** ([`locus::ir`]) to an LLVM module (via inkwell).
//!
//! **Codegen v2** — the pure fragment *plus the runtime call*. The program is
//! emitted as `__locus_main : () -> i64`; values are LLVM `BasicValueEnum`s
//! (`Int`/`Bool`/`Unit` as `i64`, `String` as an opaque pointer to a global
//! constant). A residual native `perform` (e.g. `console`) lowers to a `call`
//! of its prelowered runtime symbol `locus_<op>` ([`crate::runtime`]). Lambdas,
//! handlers, staging, and non-native performs are still explicit "unsupported"
//! errors — the gap stays honest.

use std::collections::{HashMap, HashSet};

use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{
    BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FloatType, IntType, VectorType,
};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FloatValue, FunctionValue, IntValue,
    PhiValue, PointerValue, VectorValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

use locus::{
    clause_shape, Atom, BinOp, CastOp, Comp, ExternAbi, FloatMathOp, Ir, IrBind, IrHandler,
    IrOpClause, Label, LoopVar, MaskReduceOp, MemWidth, Row, Shape, Type, ValueLayout, VectorShape,
    Width,
};

/// An installed handler while its scrutinee is lowered. A `perform` finds its
/// clause here. Tail-resumptive clauses inline (continuation implicit); abort
/// clauses store their value in the result slot and branch to `exit` — set only
/// when the handler has an abort clause (a pure tail-resumptive handle is
/// straight-line, no exit block).
struct Frame<'ctx> {
    clauses: Vec<IrOpClause>,
    exit: Option<(BasicBlock<'ctx>, PointerValue<'ctx>)>,
}

#[derive(Clone)]
struct EnvVal<'ctx> {
    value: BasicValueEnum<'ctx>,
    ty: Type,
    layout: ValueLayout,
}

#[derive(Clone, Copy)]
struct RawScalarArray<'ctx> {
    scalar_base: PointerValue<'ctx>,
    len: IntValue<'ctx>,
}

type ArrayIndexBound = (String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopVarStorage {
    Scalar,
    HandleRoot,
}

#[derive(Clone)]
struct CaptureInfo {
    name: String,
    ty: Type,
    layout: ValueLayout,
    gc_slot: u64,
    raw_slot: u64,
}

/// Which region a managed-object field lives in, and how many **contiguous**
/// cells of that region it spans. The traced-pointer region holds one cell per
/// handle (a `Var` word cell is also a single traced cell — `word` true); the
/// untraced scalar region holds `ceil(scalar_bytes/8)` cells, so a one-cell
/// `Int` is `Scalar { cells: 1 }` and a `Quad[Float32]` (16 B) is
/// `Scalar { cells: 2 }`. This generalizes the old single-cell `one_cell_pointer`
/// reject so a vector field can occupy multiple scalar cells (SIMD Sprint 1);
/// the per-field count is what `lower_tuple`/`lower_proj` accumulate to address
/// the right base cell.
#[derive(Clone, Copy, Debug)]
enum FieldRegion {
    /// One traced pointer cell — a GC handle, or (when `word`) a verbatim
    /// repr-poly `Var` word cell stored/read with `set_word`/`get_word`.
    Pointer { word: bool },
    /// `cells` contiguous untraced scalar cells (`cells >= 1`).
    Scalar { cells: u64 },
}

/// Classify a managed-object field's layout into its region + cell count. A
/// pointer (or word) cell is one traced cell; a scalar-only layout spans
/// `scalar_cells` (= `ceil(byte_width/8)`) contiguous untraced cells, the
/// multi-cell case a vector field needs. A mixed or unknown layout is still a
/// hard codegen error (first-class value packing / undecided repr).
fn field_region(layout: ValueLayout, context: &str) -> Result<FieldRegion, String> {
    if layout.is_single_pointer_cell() {
        Ok(FieldRegion::Pointer {
            word: layout.is_word_cell(),
        })
    } else if layout.is_scalar_only() {
        Ok(FieldRegion::Scalar {
            cells: layout.scalar_cells as u64,
        })
    } else if !layout.known {
        Err(format!("codegen: unknown storage layout for {context}"))
    } else {
        Err(format!(
            "codegen: {context} has unsupported mixed layout p{} s{} ({} bytes, align {}); \
             mixed pointer/scalar values need first-class value packing",
            layout.pointer_cells, layout.scalar_cells, layout.byte_width, layout.align
        ))
    }
}

fn capture_pointer(layout: ValueLayout, context: &str) -> Result<bool, String> {
    if layout.is_single_pointer_cell() {
        Ok(true)
    } else if layout.is_scalar_only() {
        Ok(false)
    } else if !layout.known {
        Err(format!("codegen: unknown storage layout for {context}"))
    } else {
        Err(format!(
            "codegen: {context} has unsupported mixed layout p{} s{}",
            layout.pointer_cells, layout.scalar_cells
        ))
    }
}

fn capture_cells(layout: ValueLayout, context: &str) -> Result<u64, String> {
    if layout.is_single_pointer_cell() {
        Ok(1)
    } else if layout.is_scalar_only() {
        Ok(layout.scalar_cells as u64)
    } else if !layout.known {
        Err(format!("codegen: unknown storage layout for {context}"))
    } else {
        Err(format!(
            "codegen: {context} has unsupported mixed layout p{} s{}",
            layout.pointer_cells, layout.scalar_cells
        ))
    }
}

fn loop_var_uses_root(layout: ValueLayout) -> bool {
    layout.is_single_pointer_cell() && !layout.is_word_cell()
}

fn loop_var_storage(var: &LoopVar) -> Result<LoopVarStorage, String> {
    if var.layout.is_scalar_only() {
        Ok(LoopVarStorage::Scalar)
    } else if loop_var_uses_root(var.layout) {
        Ok(LoopVarStorage::HandleRoot)
    } else if !var.layout.known {
        Err(format!(
            "codegen: loop accumulator `{}` has unknown storage layout",
            var.name
        ))
    } else if var.layout.is_word_cell() {
        Err(format!(
            "codegen: loop accumulator `{}` has repr-polymorphic word layout; \
             instantiate it before handle-root lowering",
            var.name
        ))
    } else {
        Err(format!(
            "codegen: loop accumulator `{}` has unsupported mixed layout p{} s{}",
            var.name, var.layout.pointer_cells, var.layout.scalar_cells
        ))
    }
}

enum ArrayElemLayout {
    Pointer,
    /// A scalar-payload element. `elem_cells` is `ceil(byte_width/8)`: a
    /// **sub-word** element (`byte_width` in {1,2,4}) packs several per cell and
    /// `elem_cells == 1`; a **multi-cell** element (a vector — `Quad[Float32]` is
    /// 16 B = 2 cells) occupies `elem_cells` whole contiguous cells. `data_cells`
    /// is the whole array's payload cell count. `stride` is the real element
    /// byte size, so element `i` starts at byte `i * stride` (cell `1 + i*elem_cells`
    /// for the multi-cell case).
    Scalar {
        stride: u64,
        data_cells: u64,
        elem_cells: u64,
    },
}

fn array_elem_layout(
    layout: ValueLayout,
    len: usize,
    context: &str,
) -> Result<ArrayElemLayout, String> {
    if layout.is_single_pointer_cell() {
        return Ok(ArrayElemLayout::Pointer);
    }
    if !layout.known {
        return Err(format!("codegen: unknown storage layout for {context}"));
    }
    if !layout.is_scalar_only() {
        return Err(format!(
            "codegen: {context} has unsupported mixed layout p{} s{}",
            layout.pointer_cells, layout.scalar_cells
        ));
    }
    // Accept any scalar-only, known-`byte_width` element. A sub-word width
    // (1/2/4) packs within a cell via the byte-strided runtime; a width of 8 is
    // one whole cell; a wider width (a vector, e.g. 16/32 B) must be a whole
    // number of cells so it lays out as `elem_cells` contiguous cells per
    // element (the multi-cell store/load path). A non-cell-multiple wide width
    // has no Locus type and is rejected loudly.
    let byte_width = layout.byte_width;
    if byte_width == 0 || (byte_width > 8 && byte_width % 8 != 0) {
        return Err(format!(
            "codegen: {context} uses unsupported scalar array stride {byte_width} bytes"
        ));
    }
    let elem_cells = byte_width.div_ceil(8) as u64;
    let bytes = byte_width
        .checked_mul(len)
        .ok_or_else(|| format!("codegen: {context} byte size overflow"))?;
    let data_cells = bytes.div_ceil(8) as u64;
    Ok(ArrayElemLayout::Scalar {
        stride: byte_width as u64,
        data_cells,
        elem_cells,
    })
}

/// Does this block (a function body, or the whole program) perform the `gc`
/// effect — i.e. allocate a tuple/record directly, or call something that does?
/// Only such code needs a handle scope and the managed-heap runtime; code that's
/// gc-free emits no runtime calls and links thin. Nested lambda bodies are
/// separate functions with their own scopes, so their *latent* gc (which rides on
/// the arrow, not this block's rows) is correctly not counted here.
pub(crate) fn block_performs_gc(ir: &Ir) -> bool {
    fn row_has_gc(row: &Row) -> bool {
        row.labels().any(|l| matches!(l, Label::Gc))
    }
    match ir {
        Ir::Block { binds, row, comp } => {
            binds
                .iter()
                .any(|bind| row_has_gc(&bind.row) || comp_needs_gc_runtime(&bind.comp))
                || row_has_gc(row)
                || comp_needs_gc_runtime(comp)
        }
        Ir::Let {
            row, comp, rest, ..
        } => row_has_gc(row) || comp_needs_gc_runtime(comp) || block_performs_gc(rest),
        Ir::Ret { row, comp } => row_has_gc(row) || comp_needs_gc_runtime(comp),
    }
}

fn comp_needs_gc_runtime(comp: &Comp) -> bool {
    match comp {
        Comp::If(_, then, els) => block_performs_gc(then) || block_performs_gc(els),
        Comp::Loop {
            vars,
            cond,
            steps,
            result,
            ..
        } => {
            vars.iter().any(|var| loop_var_uses_root(var.layout))
                || block_performs_gc(cond)
                || steps.iter().any(block_performs_gc)
                || block_performs_gc(result)
        }
        Comp::Quote(body) | Comp::Letloc(body) => block_performs_gc(body),
        Comp::Handle {
            scrutinee, handler, ..
        } => {
            block_performs_gc(scrutinee)
                || handler.ops.iter().any(|op| block_performs_gc(&op.body))
                || block_performs_gc(&handler.ret.body)
        }
        _ => false,
    }
}

fn block_produces_gc_handle(ir: &Ir) -> bool {
    match ir {
        Ir::Block { binds, comp, .. } => {
            binds.iter().any(|bind| comp_produces_gc_handle(&bind.comp))
                || comp_produces_gc_handle(comp)
        }
        Ir::Let { comp, rest, .. } => {
            comp_produces_gc_handle(comp) || block_produces_gc_handle(rest)
        }
        Ir::Ret { comp, .. } => comp_produces_gc_handle(comp),
    }
}

fn comp_produces_gc_handle(comp: &Comp) -> bool {
    match comp {
        Comp::Atom(Atom::Str(_)) => true,
        // `ref e` allocates a one-field heap cell and yields its **handle** — a
        // managed value to be rooted, exactly like a tuple/array allocation.
        Comp::Lam { .. } | Comp::Tuple(_) | Comp::ArrayLit { .. } | Comp::RefNew(_, _) => true,
        Comp::App { ret_ty, .. } => ret_ty.storage_layout().is_gc_reachable(),
        Comp::Proj { layout, .. } => layout.is_gc_reachable(),
        Comp::ArrayGet { elem_layout, .. } => elem_layout.is_gc_reachable(),
        Comp::If(_, then, els) => block_produces_gc_handle(then) || block_produces_gc_handle(els),
        Comp::Loop {
            cond,
            steps,
            result,
            ..
        } => {
            block_produces_gc_handle(cond)
                || steps.iter().any(block_produces_gc_handle)
                || block_produces_gc_handle(result)
        }
        Comp::Quote(body) | Comp::Letloc(body) => block_produces_gc_handle(body),
        Comp::Handle {
            scrutinee, handler, ..
        } => {
            block_produces_gc_handle(scrutinee)
                || handler
                    .ops
                    .iter()
                    .any(|op| block_produces_gc_handle(&op.body))
                || block_produces_gc_handle(&handler.ret.body)
        }
        _ => false,
    }
}

/// True when a loop block cannot run code that may move heap objects. That lets
/// codegen borrow raw object-field pointers for the duration of the loop.
fn block_preserves_raw_heap_ptrs(ir: &Ir) -> bool {
    match ir {
        Ir::Block { binds, comp, .. } => {
            binds
                .iter()
                .all(|bind| comp_preserves_raw_heap_ptrs(&bind.comp))
                && comp_preserves_raw_heap_ptrs(comp)
        }
        Ir::Let { comp, rest, .. } => {
            comp_preserves_raw_heap_ptrs(comp) && block_preserves_raw_heap_ptrs(rest)
        }
        Ir::Ret { comp, .. } => comp_preserves_raw_heap_ptrs(comp),
    }
}

fn comp_preserves_raw_heap_ptrs(comp: &Comp) -> bool {
    match comp {
        Comp::Atom(Atom::Str(_)) => false,
        Comp::Atom(_)
        | Comp::Extern(_, _)
        | Comp::Bin(_, _, _)
        | Comp::FloatBin(_, _, _)
        | Comp::Cast(_, _)
        | Comp::Tag(_)
        | Comp::Untag(_)
        // ToPtr (table read) and FromPtr (intern a handle slot) allocate no
        // objects and trigger no collection, so they cannot move/invalidate a
        // raw heap pointer — like Tag/Untag, they preserve it.
        | Comp::ToPtr(_)
        | Comp::FromPtr(_)
        | Comp::FloatMathUnary { .. }
        | Comp::FloatMathBinary { .. }
        | Comp::FloatMathTernary { .. }
        | Comp::VectorLit { .. }
        | Comp::VectorSplat { .. }
        | Comp::VectorBin { .. }
        | Comp::VectorCompare { .. }
        | Comp::VectorSelect { .. }
        | Comp::MaskReduce { .. }
        | Comp::VectorExtract { .. }
        | Comp::Peek(_, _)
        | Comp::Poke(_, _, _)
        | Comp::Fill(_, _, _)
        | Comp::Copy(_, _, _)
        | Comp::Proj { .. }
        | Comp::Len(_)
        | Comp::ArrayGet { .. }
        | Comp::ArraySet { .. }
        // A packed array vector load/store reads/writes the existing array
        // payload via its handle; it allocates nothing and cannot move a heap
        // object, so a borrowed raw pointer survives across it (like ArrayGet).
        | Comp::VectorLoad { .. }
        | Comp::VectorStore { .. }
        | Comp::Splice(_)
        | Comp::Genlet(_)
        // Mutable-local stack slots are scalar alloc/load/store — they allocate
        // no objects and trigger no collection, so a raw heap pointer survives.
        | Comp::SlotInit(_, _)
        | Comp::SlotLoad(_)
        | Comp::SlotStore(_, _)
        // A heap `Ref` read/write is a scalar get/set on an EXISTING cell handle —
        // it allocates nothing and triggers no collection, so a borrowed raw heap
        // pointer survives across it (like `Proj`/`ArrayGet`). (Only `ref` — the
        // alloc — is in the moving group below.)
        | Comp::RefGet(_, _)
        | Comp::RefSet(_, _, _) => true,
        Comp::If(_, then, els) => {
            block_preserves_raw_heap_ptrs(then) && block_preserves_raw_heap_ptrs(els)
        }
        Comp::Loop {
            cond,
            steps,
            result,
            ..
        } => {
            block_preserves_raw_heap_ptrs(cond)
                && steps.iter().all(block_preserves_raw_heap_ptrs)
                && block_preserves_raw_heap_ptrs(result)
        }
        Comp::Quote(body) | Comp::Letloc(body) => block_preserves_raw_heap_ptrs(body),
        Comp::App { .. }
        | Comp::Call { .. }
        | Comp::Foreign(_, _, _)
        | Comp::Lam { .. }
        | Comp::Perform(_, _)
        | Comp::Tuple(_)
        | Comp::ArrayLit { .. }
        // `ref e` allocates a one-field heap cell — it may trigger a collection
        // that moves objects, so a borrowed raw heap pointer does NOT survive
        // across it (like a tuple/array allocation).
        | Comp::RefNew(_, _)
        | Comp::Handle { .. } => false,
    }
}

fn collect_raw_array_uses(ir: &Ir, out: &mut Vec<String>) {
    match ir {
        Ir::Block { binds, comp, .. } => {
            for bind in binds {
                collect_raw_array_uses_comp(&bind.comp, out);
            }
            collect_raw_array_uses_comp(comp, out);
        }
        Ir::Let { comp, rest, .. } => {
            collect_raw_array_uses_comp(comp, out);
            collect_raw_array_uses(rest, out);
        }
        Ir::Ret { comp, .. } => collect_raw_array_uses_comp(comp, out),
    }
}

/// Is `elem_ty` a vector lane type that lives directly in an array's raw scalar
/// payload — so a `loadShape`/`storeShape` over it can ride the cached raw-base
/// loop fast path? Exactly the lanes `lane_byte_size`/`vector_type` accept
/// (`Float32`/`Float`): a known, untraced, fixed-width scalar element. (A traced
/// or unknown element never reaches a packed array load/store, so this is the
/// same gate the scalar `ArrayGet`/`ArraySet` borrow uses, phrased on the lane.)
fn is_vector_lane_raw_array_elem(elem_ty: &Type) -> bool {
    matches!(elem_ty, Type::Float32 | Type::Float)
}

fn collect_raw_array_uses_comp(comp: &Comp, out: &mut Vec<String>) {
    match comp {
        Comp::Len(arr) => collect_raw_array_atom(arr, out),
        Comp::ArrayGet {
            arr, elem_layout, ..
        } if elem_layout.known
            && elem_layout.is_scalar_only()
            && matches!(elem_layout.byte_width, 1 | 2 | 4 | 8) =>
        {
            collect_raw_array_atom(arr, out)
        }
        Comp::ArraySet {
            arr, elem_layout, ..
        } if elem_layout.known
            && elem_layout.is_scalar_only()
            && matches!(elem_layout.byte_width, 1 | 2 | 4 | 8) =>
        {
            collect_raw_array_atom(arr, out)
        }
        // A packed array vector load/store touches the same raw scalar payload as
        // a scalar `ArrayGet`/`ArraySet` (`base + idx*elem_bytes`), so borrowing
        // `arr`'s base once before the loop hoists the per-iteration
        // `locus_gc_scalar_fields_ptr` deref out — `vector_array_elem_ptr` then
        // reuses the cached base. Gated on a scalar lane layout; a `loadQuad`
        // allocates nothing, so the GC-free-region invariant the borrow needs
        // (`block_preserves_raw_heap_ptrs`, checked at the call site) still holds.
        // The whole-vector bounds check is emitted regardless — the borrow only
        // caches the deref, it never elides bounds.
        Comp::VectorLoad { arr, elem_ty, .. } if is_vector_lane_raw_array_elem(elem_ty) => {
            collect_raw_array_atom(arr, out)
        }
        Comp::VectorStore { arr, elem_ty, .. } if is_vector_lane_raw_array_elem(elem_ty) => {
            collect_raw_array_atom(arr, out)
        }
        Comp::If(_, then, els) => {
            collect_raw_array_uses(then, out);
            collect_raw_array_uses(els, out);
        }
        Comp::Loop {
            cond,
            steps,
            result,
            ..
        } => {
            collect_raw_array_uses(cond, out);
            for step in steps {
                collect_raw_array_uses(step, out);
            }
            collect_raw_array_uses(result, out);
        }
        Comp::Quote(body) | Comp::Letloc(body) => collect_raw_array_uses(body, out),
        Comp::Handle {
            scrutinee, handler, ..
        } => {
            collect_raw_array_uses(scrutinee, out);
            for op in &handler.ops {
                collect_raw_array_uses(&op.body, out);
            }
            collect_raw_array_uses(&handler.ret.body, out);
        }
        _ => {}
    }
}

fn collect_raw_array_atom(atom: &Atom, out: &mut Vec<String>) {
    if let Atom::Var(name) = atom {
        if !out.contains(name) {
            out.push(name.clone());
        }
    }
}

fn proved_loop_array_bounds(vars: &[LoopVar], cond: &Ir, steps: &[Ir]) -> HashSet<ArrayIndexBound> {
    let nonnegative = nonnegative_loop_vars(vars, steps);
    let mut upper_bounds = Vec::new();
    collect_len_upper_bounds(cond, &mut HashMap::new(), &mut upper_bounds);
    upper_bounds
        .into_iter()
        .filter(|(arr, idx)| {
            nonnegative.contains(idx)
                && steps
                    .iter()
                    .all(|step| !block_binds_name(step, arr) && !block_binds_name(step, idx))
        })
        .collect()
}

fn nonnegative_loop_vars(vars: &[LoopVar], steps: &[Ir]) -> HashSet<String> {
    vars.iter()
        .zip(steps.iter())
        .filter_map(|(var, step)| {
            if matches!(var.init, Atom::Int(n) if n >= 0)
                && step_preserves_nonnegative_loop_var(&var.name, step)
            {
                Some(var.name.clone())
            } else {
                None
            }
        })
        .collect()
}

fn step_preserves_nonnegative_loop_var(name: &str, step: &Ir) -> bool {
    match step {
        Ir::Block { binds, comp, .. } => {
            binds
                .iter()
                .all(|bind| bind.name != name && !comp_binds_name(&bind.comp, name))
                && match comp {
                    Comp::Atom(Atom::Var(x)) => x == name,
                    Comp::Bin(BinOp::Add, Atom::Var(x), Atom::Int(n))
                    | Comp::Bin(BinOp::Add, Atom::Int(n), Atom::Var(x)) => x == name && *n >= 0,
                    _ => false,
                }
        }
        Ir::Let {
            name: bound,
            comp,
            rest,
            ..
        } => {
            bound != name
                && !comp_binds_name(comp, name)
                && step_preserves_nonnegative_loop_var(name, rest)
        }
        Ir::Ret { comp, .. } => match comp {
            Comp::Atom(Atom::Var(x)) => x == name,
            Comp::Bin(BinOp::Add, Atom::Var(x), Atom::Int(n))
            | Comp::Bin(BinOp::Add, Atom::Int(n), Atom::Var(x)) => x == name && *n >= 0,
            _ => false,
        },
    }
}

fn collect_len_upper_bounds(
    ir: &Ir,
    lens: &mut HashMap<String, String>,
    out: &mut Vec<ArrayIndexBound>,
) {
    match ir {
        Ir::Block { binds, comp, .. } => {
            for bind in binds {
                if let Comp::Len(Atom::Var(arr)) = &bind.comp {
                    lens.insert(bind.name.clone(), arr.clone());
                }
            }
            if let Comp::Bin(BinOp::Lt, Atom::Var(idx), Atom::Var(bound)) = comp {
                if let Some(arr) = lens.get(bound) {
                    out.push((arr.clone(), idx.clone()));
                }
            }
        }
        Ir::Let {
            name, comp, rest, ..
        } => {
            if let Comp::Len(Atom::Var(arr)) = comp {
                lens.insert(name.clone(), arr.clone());
            }
            collect_len_upper_bounds(rest, lens, out);
        }
        Ir::Ret { comp, .. } => {
            if let Comp::Bin(BinOp::Lt, Atom::Var(idx), Atom::Var(bound)) = comp {
                if let Some(arr) = lens.get(bound) {
                    out.push((arr.clone(), idx.clone()));
                }
            }
        }
    }
}

/// Peel a curried lambda chain off an IR body: while the body is exactly a
/// lambda value (no prerequisite binds), collect each parameter and descend.
/// Returns the *additional* parameters past the first, the innermost body, and
/// the innermost return type (`None` iff no further lambda was peeled — i.e. the
/// enclosing lambda has arity 1). Used to build an uncurried fast entry.
#[allow(clippy::type_complexity)]
fn peel_lam_chain(ir: &Ir) -> (Vec<(String, Type, ValueLayout)>, &Ir, Option<&Type>) {
    let mut params: Vec<(String, Type, ValueLayout)> = Vec::new();
    let mut cur = ir;
    let mut inner_ret: Option<&Type> = None;
    loop {
        let comp = match cur {
            Ir::Ret { comp, .. } => comp,
            Ir::Block { binds, comp, .. } if binds.is_empty() => comp,
            _ => break,
        };
        match comp {
            Comp::Lam {
                param,
                param_ty,
                param_layout,
                ret_ty,
                body,
            } => {
                params.push((
                    param.clone(),
                    param_ty.clone().unwrap_or(Type::Int),
                    *param_layout,
                ));
                inner_ret = Some(ret_ty);
                cur = &**body;
            }
            _ => break,
        }
    }
    (params, cur, inner_ret)
}

fn block_binds_name(ir: &Ir, target: &str) -> bool {
    match ir {
        Ir::Block { binds, comp, .. } => {
            binds
                .iter()
                .any(|bind| bind.name == target || comp_binds_name(&bind.comp, target))
                || comp_binds_name(comp, target)
        }
        Ir::Let {
            name, comp, rest, ..
        } => name == target || comp_binds_name(comp, target) || block_binds_name(rest, target),
        Ir::Ret { comp, .. } => comp_binds_name(comp, target),
    }
}

fn comp_binds_name(comp: &Comp, target: &str) -> bool {
    match comp {
        Comp::If(_, then, els) => block_binds_name(then, target) || block_binds_name(els, target),
        Comp::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            vars.iter().any(|var| var.name == target)
                || block_binds_name(cond, target)
                || steps.iter().any(|step| block_binds_name(step, target))
                || block_binds_name(result, target)
        }
        Comp::Lam { param, body, .. } => param == target || block_binds_name(body, target),
        Comp::Quote(body) | Comp::Letloc(body) => block_binds_name(body, target),
        Comp::Handle {
            scrutinee, handler, ..
        } => {
            block_binds_name(scrutinee, target)
                || handler.ops.iter().any(|op| {
                    op.arg == target || op.resume == target || block_binds_name(&op.body, target)
                })
                || handler.ret.var == target
                || block_binds_name(&handler.ret.body, target)
        }
        _ => false,
    }
}

/// Emit `__locus_main : () -> i64` for the program `ir`. `always_gc` forces the
/// managed-heap path on even for a program that performs no `gc` effect (the
/// `--always-gc` override) — useful when you'd rather pay for collection than
/// leak in a long-running process.
pub fn emit_module<'ctx>(
    ctx: &'ctx Context,
    ir: &Ir,
    always_gc: bool,
) -> Result<Module<'ctx>, String> {
    let module = ctx.create_module("locus");
    let builder = ctx.create_builder();

    let i64t = ctx.i64_type();
    let func = module.add_function("__locus_main", i64t.fn_type(&[], false), None);
    let entry = ctx.append_basic_block(func, "entry");
    builder.position_at_end(entry);

    // Program-level GC decision: does the program allocate, or is it forced on?
    let uses_gc = always_gc || block_performs_gc(ir);
    let mut cg = Cg {
        ctx,
        module: &module,
        builder: &builder,
        env: HashMap::new(),
        lifted: 0,
        handlers: Vec::new(),
        resume_id: None,
        uses_gc,
        raw_scalar_arrays: HashMap::new(),
        loop_array_bounds: HashSet::new(),
        mut_slots: HashMap::new(),
    };
    // Open the program's root scope if it uses the heap — everything `main`
    // allocates is then rooted here, so a mid-program collection has precise
    // roots. A gc-free program emits no scope and links thin.
    let frame = if uses_gc { Some(cg.gc_enter()?) } else { None };
    let result = cg.lower_block(ir)?;

    // `__locus_main` returns i64: the process exit code. A program whose final
    // result is a SIMD vector has no meaningful exit-code encoding — a vector is
    // not an i64, and silently reducing it (pick a lane? sum?) would be a hidden
    // surprise, while a vector-as-exit-code encoding would be a miscompile. So
    // this is a clean, actionable diagnostic: the program must itself reduce the
    // vector to a scalar before it returns. Function/closure/library-export
    // returns DO carry typed vectors (the ABI handles those — see
    // `vector_function_abi_emits_typed_llvm_vectors`); only the top-level
    // i64-exit-code boundary requires the scalar.
    let ret =
        match result {
            BasicValueEnum::IntValue(iv) => iv,
            _ => return Err(
                "codegen: program result requires a scalar cell — a SIMD vector is not a valid process exit code. \
                 Reduce it to a scalar before the program returns: project a lane (e.g. `fromFloat32 (v.x)`), \
                 reduce a mask with `any`/`all`, sum the lanes (`sum v`), or `storeQuad`/`storePair`/`storeOct` \
                 the vector into an out-array and return a scalar"
                    .into(),
            ),
        };
    // Close the root scope (self-describing: a handle result escapes, a scalar
    // rides through). At program exit the heap is dropped anyway, but this keeps
    // the model uniform and the handle stack balanced.
    let ret = match frame {
        Some(frame) => cg.gc_leave_with(frame, ret)?,
        None => ret,
    };
    builder
        .build_return(Some(&ret))
        .map_err(|e| e.to_string())?;
    Ok(module)
}

/// One **exported function** to emit as a flat, externally-visible uniform-ABI
/// symbol (`docs/separate-compilation-sprints.md` Sprint 3). `symbol` is the
/// mangled cross-module name ([`locus::mangle_export`]); `params` are the
/// uncurried parameters (name + type + layout, outermost first), each one flat
/// `i64` parameter of the symbol; `body` is the innermost-lambda body already
/// lowered to ANF ([`locus::lower_function_body`]). A library export is **closed**
/// (a module binding captures nothing), so there is no closure env — just the
/// params bound directly as the function's arguments.
pub struct LibExport {
    pub symbol: String,
    pub params: Vec<(String, Type, ValueLayout)>,
    pub ret_ty: Type,
    pub body: Ir,
}

/// Emit a **library object module** — each [`LibExport`] becomes a flat,
/// externally-visible `i64 @<symbol>(i64, …)` function (uncurried; body lowered
/// directly, no closure env, **no `__locus_main`** — a library is not a program).
/// Reuses the same [`Cg`] body lowering + GC scope the closure path uses, driven
/// from a flat function with N `i64` params. `always_gc` (or any export body that
/// performs `gc`) opens a handle scope per function over the shared `locus_rt.lib`
/// collector — the producer's body uses the heap exactly as an in-module function
/// does (Sprint 3, "one shared GC"). The client declares the same `symbol`
/// external and calls it directly; the linker resolves them.
pub fn emit_library_module<'ctx>(
    ctx: &'ctx Context,
    exports: &[LibExport],
    always_gc: bool,
) -> Result<Module<'ctx>, String> {
    let module = ctx.create_module("locus_lib");
    let builder = ctx.create_builder();
    let i64t = ctx.i64_type();

    for export in exports {
        // The flat uniform ABI: one i64 per uncurried parameter, returning i64.
        let param_metas: Vec<BasicMetadataTypeEnum> =
            export.params.iter().map(|_| i64t.into()).collect();
        let func = module.add_function(&export.symbol, i64t.fn_type(&param_metas, false), None);
        let entry = ctx.append_basic_block(func, "entry");
        builder.position_at_end(entry);

        // This export's GC decision is per-body: it allocates (or `--always-gc`).
        let uses_gc = always_gc || block_performs_gc(&export.body);
        let mut cg = Cg {
            ctx,
            module: &module,
            builder: &builder,
            env: HashMap::new(),
            lifted: 0,
            handlers: Vec::new(),
            resume_id: None,
            uses_gc,
            raw_scalar_arrays: HashMap::new(),
            loop_array_bounds: HashSet::new(),
            mut_slots: HashMap::new(),
        };
        // Bind each parameter to its incoming i64 argument — the closure path's
        // env load, but the values arrive directly as the flat symbol's args.
        for (i, (name, ty, layout)) in export.params.iter().enumerate() {
            let arg = func
                .get_nth_param(i as u32)
                .ok_or_else(|| format!("codegen: missing parameter {i} of `{}`", export.symbol))?;
            cg.env.insert(
                name.clone(),
                EnvVal {
                    value: arg,
                    ty: ty.clone(),
                    layout: *layout,
                },
            );
        }
        // A handle scope rooting everything the body allocates (like every
        // function under GC); a gc-free body emits none and stays thin.
        let frame = if uses_gc { Some(cg.gc_enter()?) } else { None };
        let body_ret = cg.lower_block(&export.body)?;
        let ret = Cg::expect_cell(body_ret, "library export result")?;
        let ret = match frame {
            Some(frame) => cg.gc_leave_with(frame, ret)?,
            None => ret,
        };
        builder
            .build_return(Some(&ret))
            .map_err(|e| e.to_string())?;
    }
    Ok(module)
}

struct Cg<'ctx, 'a> {
    ctx: &'ctx Context,
    module: &'a Module<'ctx>,
    builder: &'a Builder<'ctx>,
    env: HashMap<String, EnvVal<'ctx>>,
    /// Counter for unique lifted-lambda function names.
    lifted: u32,
    /// Active handler frames (innermost last). A `perform` finds its handling
    /// clause here; tail-resumptive clauses inline, abort clauses jump to exit.
    handlers: Vec<Frame<'ctx>>,
    /// The `resume` binder of the clause currently being inlined, if any: a tail
    /// `resume V` is the identity continuation, so its value is just `V`.
    resume_id: Option<String>,
    /// Whether THIS PROGRAM uses the managed heap (it allocates, or `--always-gc`).
    /// Program-level: when set, closures are GC objects with typed pointer/scalar
    /// capture layout and every function opens a handle scope; when clear,
    /// closures stay `locus_alloc`'d and nothing is scoped (the thin-exe path).
    uses_gc: bool,
    /// Raw scalar-array views borrowed for a currently-lowered no-GC loop region.
    raw_scalar_arrays: HashMap<String, RawScalarArray<'ctx>>,
    /// `(array, index)` pairs proven in-bounds by the active loop guard.
    loop_array_bounds: HashSet<ArrayIndexBound>,
    /// Mutable-local stack slots in scope (`let mut`): name → its `alloca` pointer.
    /// `SlotLoad`/`SlotStore` `load`/`store` through it. Kept distinct from `env`
    /// (which holds SSA values) — a slot name is read via `SlotLoad`, never as an
    /// `Atom::Var`. The cell is function-local and never captured by a closure, so
    /// the slot needs no GC root and the pointer never crosses a function boundary.
    mut_slots: HashMap<String, PointerValue<'ctx>>,
}

impl<'ctx> Cg<'ctx, '_> {
    fn lower_binding(
        &mut self,
        name: &str,
        ty: &Type,
        layout: ValueLayout,
        comp: &Comp,
    ) -> Result<(), String> {
        // A foreign-function binding introduces no value; a call to it
        // was collected into `Comp::Foreign` during IR lowering.
        if let Comp::Extern(..) = comp {
            return Ok(());
        }
        // A recursive function binding shows up structurally: a
        // `let`-bound lambda whose body references the let name.
        if let Comp::Lam {
            param,
            param_ty,
            param_layout,
            ret_ty,
            body,
        } = comp
        {
            if free_vars(body, param).contains(name) {
                let v = self.lower_closure(
                    Some(name),
                    param,
                    param_ty.as_ref(),
                    *param_layout,
                    ret_ty,
                    body,
                )?;
                self.env.insert(
                    name.to_string(),
                    EnvVal {
                        value: v,
                        ty: ty.clone(),
                        layout,
                    },
                );
                return Ok(());
            }
        }
        let v = self.lower_comp(comp)?;
        self.env.insert(
            name.to_string(),
            EnvVal {
                value: v,
                ty: ty.clone(),
                layout,
            },
        );
        Ok(())
    }

    fn lower_block(&mut self, ir: &Ir) -> Result<BasicValueEnum<'ctx>, String> {
        match ir {
            Ir::Block { binds, comp, .. } => {
                for bind in binds {
                    self.lower_binding(&bind.name, &bind.ty, bind.layout, &bind.comp)?;
                }
                self.lower_comp(comp)
            }
            Ir::Let {
                name,
                ty,
                layout,
                comp,
                rest,
                ..
            } => {
                self.lower_binding(name, ty, *layout, comp)?;
                self.lower_block(rest)
            }
            Ir::Ret { comp, .. } => self.lower_comp(comp),
        }
    }

    fn lower_comp(&mut self, comp: &Comp) -> Result<BasicValueEnum<'ctx>, String> {
        match comp {
            Comp::Atom(a) => self.lower_atom(a),
            Comp::Bin(op, lhs, rhs) => self.lower_bin(*op, lhs, rhs),
            Comp::FloatBin(op, lhs, rhs) => self.lower_float_bin(*op, lhs, rhs),
            Comp::Cast(op, a) => self.lower_cast(*op, a),
            Comp::Tag(a) => self.lower_tag(a),
            Comp::Untag(a) => self.lower_untag(a),
            Comp::ToPtr(a) => self.lower_to_ptr(a),
            Comp::FromPtr(a) => self.lower_from_ptr(a),
            Comp::FloatMathUnary { op, ty, value } => self.lower_float_math_unary(*op, ty, value),
            Comp::FloatMathBinary { op, ty, lhs, rhs } => {
                self.lower_float_math_binary(*op, ty, lhs, rhs)
            }
            Comp::FloatMathTernary { op, ty, a, b, c } => {
                self.lower_float_math_ternary(*op, ty, a, b, c)
            }
            Comp::VectorLit {
                shape,
                elem_ty,
                elems,
            } => self.lower_vector_lit(*shape, elem_ty, elems),
            Comp::VectorSplat {
                shape,
                elem_ty,
                value,
            } => self.lower_vector_splat(*shape, elem_ty, value),
            Comp::VectorLoad {
                shape,
                elem_ty,
                arr,
                idx,
            } => self.lower_vector_load(*shape, elem_ty, arr, idx),
            Comp::VectorStore {
                shape,
                elem_ty,
                arr,
                idx,
                value,
            } => self.lower_vector_store(*shape, elem_ty, arr, idx, value),
            Comp::VectorBin {
                op,
                shape,
                elem_ty,
                lhs,
                rhs,
            } => self.lower_vector_bin(*op, *shape, elem_ty, lhs, rhs),
            Comp::VectorCompare {
                op,
                shape,
                elem_ty,
                lhs,
                rhs,
            } => self.lower_vector_compare(*op, *shape, elem_ty, lhs, rhs),
            Comp::VectorSelect {
                shape,
                elem_ty,
                mask,
                then_value,
                else_value,
            } => self.lower_vector_select(*shape, elem_ty, mask, then_value, else_value),
            Comp::MaskReduce { op, shape, mask } => self.lower_mask_reduce(*op, *shape, mask),
            Comp::VectorExtract {
                vector,
                lane,
                elem_ty,
            } => self.lower_vector_extract(vector, *lane, elem_ty),
            Comp::If(cond, then, els) => self.lower_if(cond, then, els),
            Comp::Loop {
                vars,
                cond,
                steps,
                result,
            } => self.lower_loop(vars, cond, steps, result),
            Comp::Lam {
                param,
                param_ty,
                param_layout,
                ret_ty,
                body,
            } => self.lower_closure(None, param, param_ty.as_ref(), *param_layout, ret_ty, body),
            Comp::App {
                fun,
                arg,
                arg_ty,
                ret_ty,
            } => self.lower_app(fun, arg, arg_ty, ret_ty),
            Comp::Call {
                fun,
                args,
                fun_ty,
                ret_ty,
            } => self.lower_call(fun, args, fun_ty, ret_ty),
            Comp::Foreign(sym, args, abi) => self.lower_foreign(sym, args, abi),
            Comp::Perform(label, arg) => self.lower_perform(label, arg),
            Comp::Handle {
                scrutinee, handler, ..
            } => self.lower_handle(scrutinee, handler),
            Comp::Peek(w, addr) => self.lower_peek(*w, addr),
            Comp::Poke(w, addr, val) => self.lower_poke(*w, addr, val),
            Comp::Fill(dst, byte, count) => self.lower_fill(dst, byte, count),
            Comp::Copy(dst, src, count) => self.lower_copy(dst, src, count),
            Comp::Tuple(fields) => self.lower_tuple(fields),
            Comp::ArrayLit { elems, elem_layout } => self.lower_array_lit(elems, *elem_layout),
            Comp::Proj {
                tup,
                slot,
                layout,
                ty,
            } => self.lower_proj(tup, *slot, *layout, ty),
            Comp::Len(arr) => self.lower_len(arr),
            Comp::ArrayGet {
                arr,
                idx,
                elem_layout,
                elem_ty,
            } => self.lower_array_get(arr, idx, *elem_layout, elem_ty),
            Comp::ArraySet {
                arr,
                idx,
                val,
                elem_layout,
            } => self.lower_array_set(arr, idx, val, *elem_layout),
            Comp::SlotInit(name, init) => self.lower_slot_init(name, init),
            Comp::SlotLoad(name) => self.lower_slot_load(name),
            Comp::SlotStore(name, val) => self.lower_slot_store(name, val),
            Comp::RefNew(init, layout) => self.lower_ref_new(init, *layout),
            Comp::RefGet(r, layout) => self.lower_ref_get(r, *layout),
            Comp::RefSet(r, val, layout) => self.lower_ref_set(r, val, *layout),
            other => Err(format!("codegen (v2): unsupported computation: {other:?}")),
        }
    }

    /// `&block[i]` — the i'th i64 slot of a closure block.
    fn slot(
        &self,
        block: PointerValue<'ctx>,
        i: u64,
        name: &str,
    ) -> Result<PointerValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        unsafe {
            self.builder
                .build_gep(i64t, block, &[i64t.const_int(i, false)], name)
        }
        .map_err(|e| e.to_string())
    }

    /// `&block[i]` with a dynamic i64 index.
    fn dynamic_slot(
        &self,
        block: PointerValue<'ctx>,
        i: IntValue<'ctx>,
        name: &str,
    ) -> Result<PointerValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        unsafe { self.builder.build_gep(i64t, block, &[i], name) }.map_err(|e| e.to_string())
    }

    /// Call `locus_alloc(bytes)` → a fresh 8-aligned pointer.
    fn call_alloc(&mut self, bytes: u64) -> Result<PointerValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let f = self.module.get_function("locus_alloc").unwrap_or_else(|| {
            self.module
                .add_function("locus_alloc", ptrt.fn_type(&[i64t.into()], false), None)
        });
        self.builder
            .build_call(f, &[i64t.const_int(bytes, false).into()], "alloc")
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .map(|v| v.into_pointer_value())
            .ok_or_else(|| "locus_alloc returned no value".to_string())
    }

    fn expect_cell(value: BasicValueEnum<'ctx>, context: &str) -> Result<IntValue<'ctx>, String> {
        match value {
            BasicValueEnum::IntValue(v) => Ok(v),
            _ => Err(format!(
                "codegen: {context} requires a scalar cell; SIMD vector/mask values cannot cross this boundary yet"
            )),
        }
    }

    fn abi_type(&self, ty: &Type) -> Result<BasicTypeEnum<'ctx>, String> {
        match ty {
            Type::Vector(shape, elem) => Ok(self.vector_type(*shape, elem)?.into()),
            Type::Mask(shape) => Ok(self.mask_type(*shape).into()),
            _ => Ok(self.ctx.i64_type().into()),
        }
    }

    fn abi_meta_type(&self, ty: &Type) -> Result<BasicMetadataTypeEnum<'ctx>, String> {
        Ok(self.abi_type(ty)?.into())
    }

    fn abi_is_scalar_cell(ty: &Type) -> bool {
        !matches!(ty, Type::Vector(_, _) | Type::Mask(_))
    }

    fn pack_mask_to_cell(
        &self,
        shape: VectorShape,
        value: VectorValue<'ctx>,
    ) -> Result<IntValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let mut acc = i64t.const_zero();
        for i in 0..shape.lanes() {
            let lane = self
                .builder
                .build_extract_element(value, i64t.const_int(i as u64, false), "mask.pack.lane")
                .map_err(|e| e.to_string())?
                .into_int_value();
            let bit = self
                .builder
                .build_int_z_extend(lane, i64t, "mask.pack.bit")
                .map_err(|e| e.to_string())?;
            let shifted = if i == 0 {
                bit
            } else {
                self.builder
                    .build_left_shift(bit, i64t.const_int(i as u64, false), "mask.pack.shl")
                    .map_err(|e| e.to_string())?
            };
            acc = self
                .builder
                .build_or(acc, shifted, "mask.pack.or")
                .map_err(|e| e.to_string())?;
        }
        Ok(acc)
    }

    fn unpack_mask_from_cell(
        &self,
        shape: VectorShape,
        cell: IntValue<'ctx>,
    ) -> Result<VectorValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let boolt = self.ctx.bool_type();
        let mut vec = self.mask_type(shape).get_undef();
        for i in 0..shape.lanes() {
            let bit = if i == 0 {
                cell
            } else {
                self.builder
                    .build_right_shift(
                        cell,
                        i64t.const_int(i as u64, false),
                        false,
                        "mask.unpack.shr",
                    )
                    .map_err(|e| e.to_string())?
            };
            let bit = self
                .builder
                .build_and(bit, i64t.const_int(1, false), "mask.unpack.and")
                .map_err(|e| e.to_string())?;
            let bit = self
                .builder
                .build_int_truncate(bit, boolt, "mask.unpack.bit")
                .map_err(|e| e.to_string())?;
            vec = self
                .builder
                .build_insert_element(vec, bit, i64t.const_int(i as u64, false), "mask.unpack.ins")
                .map_err(|e| e.to_string())?;
        }
        Ok(vec)
    }

    fn value_to_capture_cells(
        &self,
        value: BasicValueEnum<'ctx>,
        ty: &Type,
        layout: ValueLayout,
        context: &str,
    ) -> Result<Vec<IntValue<'ctx>>, String> {
        if matches!(ty, Type::Mask(_)) {
            let Type::Mask(shape) = ty else {
                unreachable!()
            };
            return Ok(vec![
                self.pack_mask_to_cell(*shape, value.into_vector_value())?
            ]);
        }
        match value {
            BasicValueEnum::IntValue(v) => Ok(vec![v]),
            BasicValueEnum::VectorValue(v) if layout.is_scalar_only() => {
                let cells = capture_cells(layout, context)? as usize;
                let ptr = self
                    .builder
                    .build_alloca(v.get_type(), "capture.spill")
                    .map_err(|e| e.to_string())?;
                self.builder
                    .build_store(ptr, v)
                    .map_err(|e| e.to_string())?;
                let i64t = self.ctx.i64_type();
                let mut out = Vec::with_capacity(cells);
                for i in 0..cells {
                    let slot = self.slot(ptr, i as u64, "capture.spill.slot")?;
                    let cell = self
                        .builder
                        .build_load(i64t, slot, "capture.spill.cell")
                        .map_err(|e| e.to_string())?
                        .into_int_value();
                    out.push(cell);
                }
                Ok(out)
            }
            _ => Err(format!(
                "codegen: {context} has unsupported value/type combination for closure capture"
            )),
        }
    }

    fn capture_cells_to_value(
        &self,
        ty: &Type,
        layout: ValueLayout,
        cells: &[IntValue<'ctx>],
        context: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match ty {
            Type::Mask(shape) => {
                let first = cells
                    .first()
                    .copied()
                    .ok_or_else(|| format!("codegen: missing mask capture cell for {context}"))?;
                Ok(self.unpack_mask_from_cell(*shape, first)?.into())
            }
            Type::Vector(shape, elem) => {
                let vt = self.vector_type(*shape, elem)?;
                let expected = capture_cells(layout, context)? as usize;
                if cells.len() != expected {
                    return Err(format!(
                        "codegen: {context} expected {expected} capture cells, got {}",
                        cells.len()
                    ));
                }
                let ptr = self
                    .builder
                    .build_alloca(vt, "capture.reload")
                    .map_err(|e| e.to_string())?;
                for (i, cell) in cells.iter().enumerate() {
                    let slot = self.slot(ptr, i as u64, "capture.reload.slot")?;
                    self.builder
                        .build_store(slot, *cell)
                        .map_err(|e| e.to_string())?;
                }
                Ok(self
                    .builder
                    .build_load(vt, ptr, "capture.reload.vec")
                    .map_err(|e| e.to_string())?
                    .into_vector_value()
                    .into())
            }
            _ => {
                let first = cells
                    .first()
                    .copied()
                    .ok_or_else(|| format!("codegen: missing capture cell for {context}"))?;
                Ok(first.into())
            }
        }
    }

    /// Decompose a value occupying `cells` (>= 1) **contiguous scalar cells**
    /// into that many `i64` words, in cell order. A one-cell value is its own
    /// `i64`; a multi-cell value (a SIMD vector — a `Quad[Float32]` is 2 cells)
    /// is spilled to a temp `alloca` of its own LLVM type and read back as `cells`
    /// `i64`s (the same vector↔words bridge the closure-capture spill uses; the
    /// optimizer later folds the alloca away). The caller then `set_scalar`s each
    /// word into the object's contiguous `base..base+cells` scalar region.
    fn scalar_value_to_cells(
        &self,
        value: BasicValueEnum<'ctx>,
        cells: u64,
        context: &str,
    ) -> Result<Vec<IntValue<'ctx>>, String> {
        if cells == 1 {
            return Ok(vec![Self::expect_cell(value, context)?]);
        }
        let i64t = self.ctx.i64_type();
        let ptr = self
            .builder
            .build_alloca(value.get_type(), "scalar.spill")
            .map_err(|e| e.to_string())?;
        self.builder
            .build_store(ptr, value)
            .map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(cells as usize);
        for i in 0..cells {
            let cell_ptr = self.slot(ptr, i, "scalar.spill.slot")?;
            let cell = self
                .builder
                .build_load(i64t, cell_ptr, "scalar.spill.cell")
                .map_err(|e| e.to_string())?
                .into_int_value();
            out.push(cell);
        }
        Ok(out)
    }

    /// Reassemble a value of type `ty` from its `cells` **contiguous scalar
    /// cells** (the inverse of [`scalar_value_to_cells`]). A single-cell value is
    /// returned verbatim (codegen elsewhere bitcasts an `Int`-cell `Float`); a
    /// multi-cell vector is rebuilt by storing the `i64` words back to a temp
    /// `alloca` and loading the value at its LLVM vector type — `ty` is what
    /// disambiguates `<4 x float>` from `<2 x double>` (both 2 cells).
    fn scalar_cells_to_value(
        &self,
        ty: &Type,
        cells: &[IntValue<'ctx>],
        context: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if cells.len() == 1 {
            return Ok(cells[0].into());
        }
        let (shape, elem) = match ty {
            Type::Vector(shape, elem) => (*shape, elem.as_ref()),
            other => {
                return Err(format!(
                    "codegen: {context} reassembles {} cells but `{other}` is not a multi-cell vector",
                    cells.len()
                ));
            }
        };
        let vt = self.vector_type(shape, elem)?;
        let ptr = self
            .builder
            .build_alloca(vt, "scalar.reload")
            .map_err(|e| e.to_string())?;
        for (i, cell) in cells.iter().enumerate() {
            let cell_ptr = self.slot(ptr, i as u64, "scalar.reload.slot")?;
            self.builder
                .build_store(cell_ptr, *cell)
                .map_err(|e| e.to_string())?;
        }
        Ok(self
            .builder
            .build_load(vt, ptr, "scalar.reload.vec")
            .map_err(|e| e.to_string())?)
    }

    fn capture_layout(
        &self,
        self_name: Option<&str>,
        self_ty: Option<&Type>,
        captures: &[String],
    ) -> Result<Vec<CaptureInfo>, String> {
        let mut infos = Vec::with_capacity(captures.len());
        for name in captures {
            let (ty, layout) = if self_name == Some(name.as_str()) {
                let ty = self_ty.cloned().unwrap_or_else(|| {
                    Type::Fun(Box::new(Type::Int), Box::new(Type::Int), Row::pure())
                });
                (ty, ValueLayout::pointer_cell())
            } else {
                let env = self
                    .env
                    .get(name)
                    .ok_or_else(|| format!("codegen: unbound captured variable `{name}`"))?;
                (env.ty.clone(), env.layout)
            };
            let context = format!("captured variable `{name}`");
            capture_pointer(layout, &context)?;
            capture_cells(layout, &context)?;
            infos.push(CaptureInfo {
                name: name.clone(),
                ty,
                layout,
                gc_slot: 0,
                raw_slot: 0,
            });
        }

        // scalar/raw 0 = curried fn-ptr; 1 = UNCURRIED fast-entry ptr (0 if
        // none); 2 = fast-entry arity (0 if none). A saturated call site reads
        // slot 2 to confirm the arity matches before calling slot 1 directly.
        // Captures start at 3.
        let (mut ptr_slot, mut scalar_slot, mut raw_slot) = (0u64, 3u64, 3u64);
        for info in &mut infos {
            let context = format!("captured variable `{}`", info.name);
            let cells = capture_cells(info.layout, &context)?;
            if capture_pointer(info.layout, &context)? {
                info.gc_slot = ptr_slot;
                ptr_slot += 1;
            } else {
                info.gc_slot = scalar_slot;
                scalar_slot += cells;
            }
            info.raw_slot = raw_slot;
            raw_slot += cells;
        }
        Ok(infos)
    }

    /// Bind a lifted function's captures into `self.env`, reading them from its
    /// `env` parameter (`env_param`): a GC closure HANDLE (i64) or a raw env
    /// pointer. Shared by the curried lifted entry and the uncurried fast entry
    /// so both read captures at identical slots. (Slots 0/1 are the curried and
    /// fast fn-ptrs; `capture_layout` numbers captures from 2.)
    fn load_captures_into_env(
        &mut self,
        env_param: BasicValueEnum<'ctx>,
        capture_infos: &[CaptureInfo],
    ) -> Result<(), String> {
        let i64t = self.ctx.i64_type();
        if self.uses_gc {
            let env_h = env_param.into_int_value();
            for cap in capture_infos {
                let context = format!("captured variable `{}`", cap.name);
                let cap_pointer = capture_pointer(cap.layout, &context)?;
                let loaded = if cap_pointer {
                    let shim = if cap.layout.is_word_cell() {
                        "locus_gc_get_word"
                    } else {
                        "locus_gc_get_ptr"
                    };
                    self.gc_call(shim, &[env_h, i64t.const_int(cap.gc_slot, false)], true)?
                        .expect("capture get returns a value")
                        .into()
                } else {
                    let cells = capture_cells(cap.layout, &context)?;
                    let mut values = Vec::with_capacity(cells as usize);
                    for i in 0..cells {
                        let cell = self
                            .gc_call(
                                "locus_gc_get_scalar",
                                &[env_h, i64t.const_int(cap.gc_slot + i, false)],
                                true,
                            )?
                            .expect("capture get returns a value");
                        values.push(cell);
                    }
                    self.capture_cells_to_value(&cap.ty, cap.layout, &values, &context)?
                };
                self.env.insert(
                    cap.name.clone(),
                    EnvVal {
                        value: loaded,
                        ty: cap.ty.clone(),
                        layout: cap.layout,
                    },
                );
            }
        } else {
            let env_ptr = env_param.into_pointer_value();
            for cap in capture_infos {
                let context = format!("captured variable `{}`", cap.name);
                let cells = capture_cells(cap.layout, &context)?;
                let mut values = Vec::with_capacity(cells as usize);
                for i in 0..cells {
                    let s = self.slot(env_ptr, cap.raw_slot + i, "cap.slot")?;
                    let loaded = self
                        .builder
                        .build_load(i64t, s, "cap")
                        .map_err(|e| e.to_string())?
                        .into_int_value();
                    values.push(loaded);
                }
                let loaded = self.capture_cells_to_value(&cap.ty, cap.layout, &values, &context)?;
                self.env.insert(
                    cap.name.clone(),
                    EnvVal {
                        value: loaded,
                        ty: cap.ty.clone(),
                        layout: cap.layout,
                    },
                );
            }
        }
        Ok(())
    }

    /// Generate an **uncurried fast entry** `ret (env, p1, …, pn)` for a
    /// non-recursive lambda chain of arity n ≥ 2: it binds every parameter
    /// directly and reads the chain's captures from `env` (the same closure the
    /// curried entries use — `capture_infos` is the outermost closure's layout),
    /// then runs the innermost `body`. A saturated call site invokes this with
    /// one direct call and zero per-argument closure allocation. The curried
    /// entries (`__locus_lam_*`) are still emitted for partial application; this
    /// is a parallel fast path, not a replacement.
    fn emit_fast_entry(
        &mut self,
        self_name: Option<&str>,
        params: &[(String, Type, ValueLayout)],
        capture_infos: &[CaptureInfo],
        body: &Ir,
        ret_ty: &Type,
    ) -> Result<FunctionValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let env_ty: BasicMetadataTypeEnum = if self.uses_gc {
            i64t.into()
        } else {
            ptrt.into()
        };
        let mut param_metas: Vec<BasicMetadataTypeEnum> = Vec::with_capacity(params.len() + 1);
        param_metas.push(env_ty);
        for (_, ty, _) in params {
            param_metas.push(self.abi_meta_type(ty)?);
        }
        let ret_abi = self.abi_type(ret_ty)?;
        let name = format!("__locus_fast_{}", self.lifted);
        self.lifted += 1;
        let func = self
            .module
            .add_function(&name, ret_abi.fn_type(&param_metas, false), None);

        let outer_block = self.builder.get_insert_block();
        let outer_env = std::mem::take(&mut self.env);

        let entry = self.ctx.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);

        // Parameters occupy native slots 1..=n (slot 0 is env).
        for (i, (pname, pty, playout)) in params.iter().enumerate() {
            let arg = func.get_nth_param((i + 1) as u32).unwrap();
            self.env.insert(
                pname.clone(),
                EnvVal {
                    value: arg,
                    ty: pty.clone(),
                    layout: *playout,
                },
            );
        }

        let frame = if self.uses_gc {
            Some(self.gc_enter()?)
        } else {
            None
        };

        self.load_captures_into_env(func.get_nth_param(0).unwrap(), capture_infos)?;

        match self_name {
            // Recursive (TCO-eligible) fast entry: a phi-loop so a saturated
            // self-tail-call `f a₁ … aₙ` jumps back instead of recursing —
            // constant native stack even for multi-arg functions.
            Some(sname) => {
                let loop_block = self.ctx.append_basic_block(func, "tail.loop");
                self.builder
                    .build_unconditional_branch(loop_block)
                    .map_err(|e| e.to_string())?;
                let incoming = self
                    .builder
                    .get_insert_block()
                    .ok_or_else(|| "codegen: missing fast-entry block".to_string())?;
                self.builder.position_at_end(loop_block);
                let mut phis: Vec<PhiValue<'ctx>> = Vec::with_capacity(params.len());
                for (i, (pname, pty, playout)) in params.iter().enumerate() {
                    let phi = self
                        .builder
                        .build_phi(i64t, "tail.p")
                        .map_err(|e| e.to_string())?;
                    let seed = func.get_nth_param((i + 1) as u32).unwrap().into_int_value();
                    phi.add_incoming(&[(&seed, incoming)]);
                    self.env.insert(
                        pname.clone(),
                        EnvVal {
                            value: phi.as_basic_value(),
                            ty: pty.clone(),
                            layout: *playout,
                        },
                    );
                    phis.push(phi);
                }
                self.lower_tail_return_n(body, sname, loop_block, &phis, frame)?;
            }
            None => {
                let body_ret = self.lower_block(body)?;
                let ret = self.return_with_frame(frame, body_ret)?;
                self.builder
                    .build_return(Some(&ret))
                    .map_err(|e| e.to_string())?;
            }
        }

        self.env = outer_env;
        if let Some(b) = outer_block {
            self.builder.position_at_end(b);
        }
        Ok(func)
    }

    /// A lambda → a lifted top-level function `i64 (ptr env, i64 arg)` plus a
    /// heap closure built in the enclosing scope. Under GC the closure is a
    /// handle to a pointer-first/scalar-second object; without GC it is the old
    /// raw `{ fn_ptr, captures… }` block.
    ///
    /// `self_name`, when set, is the lambda's own binding (a `let rec`): the
    /// closure is bound to it *before* the captures are read, so the
    /// self-capture slot points back at the block — that is the recursion.
    fn lower_closure(
        &mut self,
        self_name: Option<&str>,
        param: &str,
        param_ty: Option<&Type>,
        param_layout: ValueLayout,
        ret_ty: &Type,
        body: &Ir,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let param_ty = param_ty.cloned().unwrap_or(Type::Int);
        let self_fun_ty = Type::Fun(
            Box::new(param_ty.clone()),
            Box::new(ret_ty.clone()),
            Row::pure(),
        );

        let mut capture_set = free_vars(body, param);
        capture_set.extend(active_handler_free_vars(body, &self.handlers));
        capture_set.remove(param);
        let mut captures: Vec<String> = capture_set.into_iter().collect();
        captures.sort();
        let capture_infos = self.capture_layout(self_name, Some(&self_fun_ty), &captures)?;

        // Lift the body into a fresh function that loads captures from `env`.
        let name = format!("__locus_lam_{}", self.lifted);
        self.lifted += 1;
        // env is a closure HANDLE (i64) under GC, or a raw env pointer otherwise.
        let env_ty: BasicMetadataTypeEnum = if self.uses_gc {
            i64t.into()
        } else {
            ptrt.into()
        };
        let arg_ty = self.abi_meta_type(&param_ty)?;
        let ret_abi = self.abi_type(ret_ty)?;
        let func = self
            .module
            .add_function(&name, ret_abi.fn_type(&[env_ty, arg_ty], false), None);

        let outer_block = self.builder.get_insert_block();
        let outer_env = std::mem::take(&mut self.env);

        let entry = self.ctx.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        let arg = func.get_nth_param(1).unwrap();
        self.env.insert(
            param.to_string(),
            EnvVal {
                value: arg,
                ty: param_ty.clone(),
                layout: param_layout,
            },
        );

        // Per-call handle scope (under GC, every function gets one; pointer
        // captures are read INTO it so the handles `get_ptr` interns are popped on return).
        // The closure-handle env and the argument are rooted in the caller below
        // this frame, so they stay live across the call.
        let frame = if self.uses_gc {
            Some(self.gc_enter()?)
        } else {
            None
        };

        self.load_captures_into_env(func.get_nth_param(0).unwrap(), &capture_infos)?;

        if let Some(name) = self_name.filter(|_| {
            !block_performs_gc(body)
                && Self::abi_is_scalar_cell(&param_ty)
                && Self::abi_is_scalar_cell(ret_ty)
        }) {
            let loop_block = self.ctx.append_basic_block(func, "tail.loop");
            self.builder
                .build_unconditional_branch(loop_block)
                .map_err(|e| e.to_string())?;
            let incoming = self
                .builder
                .get_insert_block()
                .ok_or_else(|| "codegen: missing function entry block".to_string())?;
            self.builder.position_at_end(loop_block);
            let phi = self
                .builder
                .build_phi(i64t, "tail.arg")
                .map_err(|e| e.to_string())?;
            let initial_arg = arg.into_int_value();
            phi.add_incoming(&[(&initial_arg, incoming)]);
            self.env.insert(
                param.to_string(),
                EnvVal {
                    value: phi.as_basic_value(),
                    ty: param_ty.clone(),
                    layout: param_layout,
                },
            );
            self.lower_tail_return(body, name, loop_block, phi, frame)?;
        } else {
            let body_ret = self.lower_block(body)?;
            let ret = self.return_with_frame(frame, body_ret)?;
            self.builder
                .build_return(Some(&ret))
                .map_err(|e| e.to_string())?;
        }

        // Back in the enclosing scope: allocate, bind self (recursion), then
        // read + store the captures. `self_name` is bound to the closure value
        // BEFORE the captures, so a `let rec` self-capture points back at it.
        self.env = outer_env;
        if let Some(b) = outer_block {
            self.builder.position_at_end(b);
        }
        let fnptr_i = self
            .builder
            .build_ptr_to_int(func.as_global_value().as_pointer_value(), i64t, "fnaddr")
            .map_err(|e| e.to_string())?;

        // Uncurried fast entry for a lambda chain of arity >= 2, called from
        // saturated call sites (Comp::Call). A NON-recursive chain always gets
        // one. A RECURSIVE chain gets one only when it is tail-call-eligible
        // (every param + the result is a scalar cell, and the body performs no
        // GC), so a self-tail-call becomes a constant-stack loop — multi-arg
        // curried recursion was never tail-optimized, so this is a strict win
        // and removes a latent stack-growth case. A recursive chain that isn't
        // eligible keeps the curried path unchanged (no fast entry). The address
        // lives in slot 1, the arity in slot 2; 0/0 means "no fast entry".
        let zero = i64t.const_int(0, false);
        let (fast_ptr_i, fast_arity_i) = {
            let (extra, inner_body, inner_ret) = peel_lam_chain(body);
            match inner_ret {
                Some(inner_ret) if !extra.is_empty() => {
                    let mut all_params: Vec<(String, Type, ValueLayout)> =
                        Vec::with_capacity(extra.len() + 1);
                    all_params.push((param.to_string(), param_ty.clone(), param_layout));
                    all_params.extend(extra);
                    let recursive = self_name.is_some();
                    let tco_ok = all_params
                        .iter()
                        .all(|(_, t, _)| Self::abi_is_scalar_cell(t))
                        && Self::abi_is_scalar_cell(inner_ret)
                        && !block_performs_gc(inner_body);
                    if recursive && !tco_ok {
                        (zero, zero)
                    } else {
                        let arity = all_params.len() as u64;
                        // Recursive entries get `self_name` (→ TCO loop); non-
                        // recursive ones pass None (straight-line body).
                        let f = self.emit_fast_entry(
                            self_name,
                            &all_params,
                            &capture_infos,
                            inner_body,
                            inner_ret,
                        )?;
                        let ptr = self
                            .builder
                            .build_ptr_to_int(
                                f.as_global_value().as_pointer_value(),
                                i64t,
                                "fastaddr",
                            )
                            .map_err(|e| e.to_string())?;
                        (ptr, i64t.const_int(arity, false))
                    }
                }
                _ => (zero, zero),
            }
        };

        if self.uses_gc {
            // A GC closure is laid out like every managed object: pointer
            // captures first, then scalar cells. Scalar 0 is the fn-ptr; scalar
            // captures follow. This is lossless for full-width scalar bits.
            let mut n_ptr = 0u64;
            let mut n_scalar = 3u64; // scalars 0/1/2 = curried ptr, fast ptr, fast arity
            for cap in &capture_infos {
                let context = format!("captured variable `{}`", cap.name);
                if capture_pointer(cap.layout, &context)? {
                    n_ptr += 1;
                } else {
                    n_scalar += capture_cells(cap.layout, &context)?;
                }
            }
            let clos = self
                .gc_call(
                    "locus_gc_alloc",
                    &[
                        i64t.const_int(n_ptr, false),
                        i64t.const_int(n_scalar, false),
                    ],
                    true,
                )?
                .expect("alloc returns a handle");
            if let Some(n) = self_name {
                self.env.insert(
                    n.to_string(),
                    EnvVal {
                        value: clos.into(),
                        ty: self_fun_ty.clone(),
                        layout: ValueLayout::pointer_cell(),
                    },
                );
            }
            self.gc_call(
                "locus_gc_set_scalar",
                &[clos, i64t.const_int(0, false), fnptr_i],
                false,
            )?;
            // Scalar 1/2: the uncurried fast-entry ptr and its arity (0/0 when none).
            self.gc_call(
                "locus_gc_set_scalar",
                &[clos, i64t.const_int(1, false), fast_ptr_i],
                false,
            )?;
            self.gc_call(
                "locus_gc_set_scalar",
                &[clos, i64t.const_int(2, false), fast_arity_i],
                false,
            )?;
            for cap in &capture_infos {
                let val = self
                    .env
                    .get(&cap.name)
                    .ok_or_else(|| format!("codegen: unbound captured variable `{}`", cap.name))?
                    .value;
                let context = format!("captured variable `{}`", cap.name);
                let cap_pointer = capture_pointer(cap.layout, &context)?;
                if cap_pointer {
                    let val = Self::expect_cell(val, &context)?;
                    let shim = if cap.layout.is_word_cell() {
                        "locus_gc_set_word"
                    } else {
                        "locus_gc_set_ptr"
                    };
                    self.gc_call(
                        shim,
                        &[clos, i64t.const_int(cap.gc_slot, false), val],
                        false,
                    )?;
                } else {
                    let cells = self.value_to_capture_cells(val, &cap.ty, cap.layout, &context)?;
                    for (i, cell) in cells.into_iter().enumerate() {
                        self.gc_call(
                            "locus_gc_set_scalar",
                            &[clos, i64t.const_int(cap.gc_slot + i as u64, false), cell],
                            false,
                        )?;
                    }
                }
            }
            Ok(clos.into())
        } else {
            // Raw slots 0/1/2 (curried ptr, fast ptr, fast arity) are reserved
            // before any captures, so a closure is always at least three cells.
            let total_cells = capture_infos
                .last()
                .map(|cap| {
                    capture_cells(cap.layout, &format!("captured variable `{}`", cap.name))
                        .map(|cells| cap.raw_slot + cells)
                })
                .transpose()?
                .unwrap_or(3);
            let block = self.call_alloc(total_cells * 8)?;
            let clos = self
                .builder
                .build_ptr_to_int(block, i64t, "clos")
                .map_err(|e| e.to_string())?;
            if let Some(n) = self_name {
                self.env.insert(
                    n.to_string(),
                    EnvVal {
                        value: clos.into(),
                        ty: self_fun_ty,
                        layout: ValueLayout::scalar_cell(),
                    },
                );
            }
            let s0 = self.slot(block, 0, "fn.slot")?;
            self.builder
                .build_store(s0, fnptr_i)
                .map_err(|e| e.to_string())?;
            // Raw slots 1/2: fast-entry ptr and arity (0/0 when none).
            let s1 = self.slot(block, 1, "fast.slot")?;
            self.builder
                .build_store(s1, fast_ptr_i)
                .map_err(|e| e.to_string())?;
            let s2 = self.slot(block, 2, "fastarity.slot")?;
            self.builder
                .build_store(s2, fast_arity_i)
                .map_err(|e| e.to_string())?;
            for cap in &capture_infos {
                let val = self
                    .env
                    .get(&cap.name)
                    .ok_or_else(|| format!("codegen: unbound captured variable `{}`", cap.name))?
                    .value;
                let context = format!("captured variable `{}`", cap.name);
                let cells = self.value_to_capture_cells(val, &cap.ty, cap.layout, &context)?;
                for (i, cell) in cells.into_iter().enumerate() {
                    let s = self.slot(block, cap.raw_slot + i as u64, "cap.store")?;
                    self.builder
                        .build_store(s, cell)
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(clos.into())
        }
    }

    /// Lower a recursive function body in return-producing mode. A direct
    /// self-tail call becomes a jump to `tail.loop` with the next argument added
    /// to the phi, so simple recursive iteration consumes constant native stack.
    fn lower_tail_return(
        &mut self,
        ir: &Ir,
        self_name: &str,
        loop_block: BasicBlock<'ctx>,
        arg_phi: PhiValue<'ctx>,
        frame: Option<IntValue<'ctx>>,
    ) -> Result<(), String> {
        match ir {
            Ir::Block { binds, comp, .. } => {
                for bind in binds {
                    self.lower_binding(&bind.name, &bind.ty, bind.layout, &bind.comp)?;
                }
                self.lower_tail_comp(comp, self_name, loop_block, arg_phi, frame)
            }
            Ir::Let {
                name,
                ty,
                layout,
                comp,
                rest,
                ..
            } => {
                self.lower_binding(name, ty, *layout, comp)?;
                self.lower_tail_return(rest, self_name, loop_block, arg_phi, frame)
            }
            Ir::Ret { comp, .. } => {
                self.lower_tail_comp(comp, self_name, loop_block, arg_phi, frame)
            }
        }
    }

    fn lower_tail_comp(
        &mut self,
        comp: &Comp,
        self_name: &str,
        loop_block: BasicBlock<'ctx>,
        arg_phi: PhiValue<'ctx>,
        frame: Option<IntValue<'ctx>>,
    ) -> Result<(), String> {
        match comp {
            Comp::App {
                fun: Atom::Var(f),
                arg,
                ..
            } if f == self_name => {
                let next = Self::expect_cell(self.lower_atom(arg)?, "tail-recursive argument")?;
                let current = self
                    .builder
                    .get_insert_block()
                    .ok_or_else(|| "codegen: missing tail-recursive block".to_string())?;
                arg_phi.add_incoming(&[(&next, current)]);
                self.builder
                    .build_unconditional_branch(loop_block)
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Comp::If(cond, then, els) => {
                self.lower_tail_if(cond, then, els, self_name, loop_block, arg_phi, frame)
            }
            _ => {
                let ret = Self::expect_cell(self.lower_comp(comp)?, "function result")?;
                let ret = match frame {
                    Some(frame) => self.gc_leave_with(frame, ret)?,
                    None => ret,
                };
                self.builder
                    .build_return(Some(&ret))
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }
    }

    fn lower_tail_if(
        &mut self,
        cond: &Atom,
        then: &Ir,
        els: &Ir,
        self_name: &str,
        loop_block: BasicBlock<'ctx>,
        arg_phi: PhiValue<'ctx>,
        frame: Option<IntValue<'ctx>>,
    ) -> Result<(), String> {
        let i64t = self.ctx.i64_type();
        let cv = Self::expect_cell(self.lower_atom(cond)?, "if condition")?;
        let c = self
            .builder
            .build_int_compare(IntPredicate::NE, cv, i64t.const_zero(), "ifcond")
            .map_err(|e| e.to_string())?;
        let parent = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .ok_or_else(|| "codegen: `if` outside a function".to_string())?;
        let then_bb = self.ctx.append_basic_block(parent, "then");
        let else_bb = self.ctx.append_basic_block(parent, "else");
        self.builder
            .build_conditional_branch(c, then_bb, else_bb)
            .map_err(|e| e.to_string())?;

        let saved = self.env.clone();
        self.builder.position_at_end(then_bb);
        self.env = saved.clone();
        self.lower_tail_return(then, self_name, loop_block, arg_phi, frame)?;

        self.builder.position_at_end(else_bb);
        self.env = saved;
        self.lower_tail_return(els, self_name, loop_block, arg_phi, frame)?;
        Ok(())
    }

    /// n-ary analogue of `lower_tail_return`, for an uncurried fast entry. A
    /// saturated self-tail-call `f a₁ … aₙ` (a `Comp::Call` to `self_name`)
    /// updates every loop phi and jumps, so tail recursion runs in constant
    /// native stack — even for multi-argument functions, which the curried path
    /// never tail-optimized. Non-tail self-calls fall through to `lower_binding`
    /// /`lower_call` as ordinary direct recursive calls.
    fn lower_tail_return_n(
        &mut self,
        ir: &Ir,
        self_name: &str,
        loop_block: BasicBlock<'ctx>,
        phis: &[PhiValue<'ctx>],
        frame: Option<IntValue<'ctx>>,
    ) -> Result<(), String> {
        match ir {
            Ir::Block { binds, comp, .. } => {
                for bind in binds {
                    self.lower_binding(&bind.name, &bind.ty, bind.layout, &bind.comp)?;
                }
                self.lower_tail_comp_n(comp, self_name, loop_block, phis, frame)
            }
            Ir::Let {
                name,
                ty,
                layout,
                comp,
                rest,
                ..
            } => {
                self.lower_binding(name, ty, *layout, comp)?;
                self.lower_tail_return_n(rest, self_name, loop_block, phis, frame)
            }
            Ir::Ret { comp, .. } => self.lower_tail_comp_n(comp, self_name, loop_block, phis, frame),
        }
    }

    fn lower_tail_comp_n(
        &mut self,
        comp: &Comp,
        self_name: &str,
        loop_block: BasicBlock<'ctx>,
        phis: &[PhiValue<'ctx>],
        frame: Option<IntValue<'ctx>>,
    ) -> Result<(), String> {
        match comp {
            Comp::Call {
                fun: Atom::Var(f),
                args,
                ..
            } if f == self_name && args.len() == phis.len() => {
                // Evaluate every next-iteration argument (all ANF atoms), then
                // feed the phis and jump — a multi-arg self-tail-call as a loop.
                let mut nexts = Vec::with_capacity(args.len());
                for (a, _) in args {
                    nexts.push(Self::expect_cell(self.lower_atom(a)?, "tail-recursive argument")?);
                }
                let current = self
                    .builder
                    .get_insert_block()
                    .ok_or_else(|| "codegen: missing tail-recursive block".to_string())?;
                for (phi, next) in phis.iter().zip(nexts.iter()) {
                    phi.add_incoming(&[(next, current)]);
                }
                self.builder
                    .build_unconditional_branch(loop_block)
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Comp::If(cond, then, els) => {
                self.lower_tail_if_n(cond, then, els, self_name, loop_block, phis, frame)
            }
            _ => {
                let ret = Self::expect_cell(self.lower_comp(comp)?, "function result")?;
                let ret = match frame {
                    Some(frame) => self.gc_leave_with(frame, ret)?,
                    None => ret,
                };
                self.builder
                    .build_return(Some(&ret))
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_tail_if_n(
        &mut self,
        cond: &Atom,
        then: &Ir,
        els: &Ir,
        self_name: &str,
        loop_block: BasicBlock<'ctx>,
        phis: &[PhiValue<'ctx>],
        frame: Option<IntValue<'ctx>>,
    ) -> Result<(), String> {
        let i64t = self.ctx.i64_type();
        let cv = Self::expect_cell(self.lower_atom(cond)?, "if condition")?;
        let c = self
            .builder
            .build_int_compare(IntPredicate::NE, cv, i64t.const_zero(), "ifcond")
            .map_err(|e| e.to_string())?;
        let parent = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .ok_or_else(|| "codegen: `if` outside a function".to_string())?;
        let then_bb = self.ctx.append_basic_block(parent, "then");
        let else_bb = self.ctx.append_basic_block(parent, "else");
        self.builder
            .build_conditional_branch(c, then_bb, else_bb)
            .map_err(|e| e.to_string())?;

        let saved = self.env.clone();
        self.builder.position_at_end(then_bb);
        self.env = saved.clone();
        self.lower_tail_return_n(then, self_name, loop_block, phis, frame)?;

        self.builder.position_at_end(else_bb);
        self.env = saved;
        self.lower_tail_return_n(els, self_name, loop_block, phis, frame)?;
        Ok(())
    }

    /// A fully-applied **foreign call** → declare the symbol with its *native*
    /// signature (per the [`ExternAbi`]) and call it; ORC (JIT) / the linker
    /// (AOT) resolve it. Win64 has a *single* calling convention, so LLVM's C
    /// convention **is** the ABI. The uniform `i64` value model meets the real
    /// native classes *here*: integer args/results are narrowed or extended at
    /// the edge, while Float/Float32 cells are bitcast to native `double`/`float`
    /// so fixed-signature calls use the FP ABI lanes.
    fn lower_foreign(
        &mut self,
        sym: &str,
        args: &[Atom],
        abi: &ExternAbi,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let i32t = self.ctx.i32_type();
        let f64t = self.ctx.f64_type();
        let f32t = self.ctx.f32_type();
        let native = |w: Width| -> BasicMetadataTypeEnum<'ctx> {
            match w {
                Width::I32 | Width::U32 => i32t.into(),
                Width::W64 => i64t.into(),
                Width::F32 => f32t.into(),
                Width::F64 => f64t.into(),
            }
        };
        // Declare (or reuse) the symbol with its real native signature.
        let f = self.module.get_function(sym).unwrap_or_else(|| {
            let params: Vec<BasicMetadataTypeEnum> =
                abi.params.iter().map(|w| native(*w)).collect();
            let fnty = match abi.ret {
                Width::W64 => i64t.fn_type(&params, false),
                Width::I32 | Width::U32 => i32t.fn_type(&params, false),
                Width::F32 => f32t.fn_type(&params, false),
                Width::F64 => f64t.fn_type(&params, false),
            };
            self.module.add_function(sym, fnty, None)
        });
        // Each uniform-i64 argument, narrowed to the parameter width if needed.
        let mut argv: Vec<BasicMetadataValueEnum> = Vec::with_capacity(args.len());
        for (a, w) in args.iter().zip(&abi.params) {
            let v = Self::expect_cell(self.lower_atom(a)?, "foreign argument")?;
            let arg: BasicMetadataValueEnum = match w {
                Width::I32 | Width::U32 => self
                    .builder
                    .build_int_truncate(v, i32t, "arg.i32")
                    .map_err(|e| e.to_string())?
                    .into(),
                Width::W64 => v.into(),
                Width::F32 => self.cell_to_f32(v, "arg.f32")?.into(),
                Width::F64 => self.cell_to_f64(v, "arg.f64")?.into(),
            };
            argv.push(arg);
        }
        let ret = self
            .builder
            .build_call(f, &argv, "winapi")
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("foreign call `{sym}` returned no value"))?;
        // Convert the native return back into the uniform i64 cell model.
        match abi.ret {
            Width::I32 => {
                let ret = ret.into_int_value();
                Ok(self
                    .builder
                    .build_int_s_extend(ret, i64t, "ret.sext")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            Width::U32 => {
                let ret = ret.into_int_value();
                Ok(self
                    .builder
                    .build_int_z_extend(ret, i64t, "ret.zext")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            Width::W64 => Ok(ret.into_int_value().into()),
            Width::F32 => Ok(self
                .f32_to_cell(ret.into_float_value(), "ret.f32.cell")?
                .into()),
            Width::F64 => Ok(self
                .f64_to_cell(ret.into_float_value(), "ret.f64.cell")?
                .into()),
        }
    }

    /// `f a` → call indirectly through the closure `f`: load its fn_ptr from
    /// slot 0 and invoke `fn(env = f, arg = a)`. (Foreign calls never reach
    /// here — the IR collects their spine into `Comp::Foreign`.)
    fn lower_app(
        &mut self,
        f: &Atom,
        a: &Atom,
        arg_ty: &Type,
        ret_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // A tail-resumptive `resume V` is the identity continuation — its value
        // is just `V` (the perform site continues with it).
        if let Atom::Var(name) = f {
            if self.resume_id.as_deref() == Some(name.as_str()) {
                return self.lower_atom(a);
            }
        }
        let fv: IntValue = Self::expect_cell(self.lower_atom(f)?, "function value")?;
        let av = self.lower_atom(a)?;
        self.apply_one_val(fv, av, arg_ty, ret_ty)
    }

    /// Apply ONE argument to an already-lowered closure handle `fv`: load its
    /// curried fn-ptr from slot 0 and invoke `fn(env = fv, arg = av)`. The unit
    /// of the curried calling convention; `lower_app` is this on lowered atoms,
    /// and `lower_call`'s slow path folds it over a saturated argument list.
    fn apply_one_val(
        &mut self,
        fv: IntValue<'ctx>,
        av: BasicValueEnum<'ctx>,
        arg_ty: &Type,
        ret_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let arg_abi = self.abi_meta_type(arg_ty)?;
        let ret_abi = self.abi_type(ret_ty)?;

        if self.uses_gc {
            // `fv` is a closure HANDLE. Its fn-ptr lives in scalar field 0; the
            // handle itself is the env. The lifted function takes typed arguments.
            let fp_i = self
                .gc_call("locus_gc_get_scalar", &[fv, i64t.const_int(0, false)], true)?
                .expect("fn-ptr scalar");
            let fp = self
                .builder
                .build_int_to_ptr(fp_i, ptrt, "fp")
                .map_err(|e| e.to_string())?;
            let fnty = ret_abi.fn_type(&[i64t.into(), arg_abi], false);
            return self
                .builder
                .build_indirect_call(fnty, fp, &[fv.into(), av.into()], "call")
                .map_err(|e| e.to_string())?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| "call returned no value".to_string());
        }

        // Non-GC: `fv` is a raw closure-block pointer; load fn-ptr from slot 0.
        let env_ptr = self
            .builder
            .build_int_to_ptr(fv, ptrt, "clos")
            .map_err(|e| e.to_string())?;
        let fp_slot = self.slot(env_ptr, 0, "fp.slot")?;
        let fp_i = self
            .builder
            .build_load(i64t, fp_slot, "fp.i")
            .map_err(|e| e.to_string())?
            .into_int_value();
        let fp = self
            .builder
            .build_int_to_ptr(fp_i, ptrt, "fp")
            .map_err(|e| e.to_string())?;
        let fnty = ret_abi.fn_type(&[ptrt.into(), arg_abi], false);
        self.builder
            .build_indirect_call(fnty, fp, &[env_ptr.into(), av.into()], "call")
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| "call returned no value".to_string())
    }

    /// Read scalar slot `idx` of a closure handle `clos` as an i64 (the curried
    /// fn-ptr at 0, the fast-entry ptr at 1, the fast-entry arity at 2).
    fn read_closure_slot(
        &mut self,
        clos: IntValue<'ctx>,
        idx: u64,
    ) -> Result<IntValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        if self.uses_gc {
            Ok(self
                .gc_call("locus_gc_get_scalar", &[clos, i64t.const_int(idx, false)], true)?
                .expect("closure scalar"))
        } else {
            let ptrt = self.ctx.ptr_type(AddressSpace::default());
            let env_ptr = self
                .builder
                .build_int_to_ptr(clos, ptrt, "clos")
                .map_err(|e| e.to_string())?;
            let s = self.slot(env_ptr, idx, "clos.slot")?;
            Ok(self
                .builder
                .build_load(i64t, s, "clos.cell")
                .map_err(|e| e.to_string())?
                .into_int_value())
        }
    }

    /// A SATURATED call `f a₁ … aₙ` (n ≥ 2) to a named function. If the
    /// closure's stored fast-entry arity (slot 2) equals `n`, call its uncurried
    /// fast entry (slot 1) directly — one call, no per-argument closure
    /// allocation. Otherwise fall back to the curried apply chain. The guard is
    /// loop-invariant for a fixed callee, so LICM hoists it and a hot inner loop
    /// is left with just the direct call (which is itself then inlinable).
    fn lower_call(
        &mut self,
        fun: &Atom,
        args: &[(Atom, Type)],
        fun_ty: &Type,
        ret_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let clos = Self::expect_cell(self.lower_atom(fun)?, "function value")?;
        let argc = args.len();
        let mut arg_vals: Vec<BasicValueEnum<'ctx>> = Vec::with_capacity(argc);
        for (a, _) in args {
            arg_vals.push(self.lower_atom(a)?);
        }
        let ret_abi = self.abi_type(ret_ty)?;

        let fast_arity = self.read_closure_slot(clos, 2)?;
        let fast_ptr = self.read_closure_slot(clos, 1)?;

        let func = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .ok_or_else(|| "codegen: lower_call outside a function".to_string())?;
        let fast_bb = self.ctx.append_basic_block(func, "call.fast");
        let slow_bb = self.ctx.append_basic_block(func, "call.curried");
        let cont_bb = self.ctx.append_basic_block(func, "call.cont");

        let cond = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                fast_arity,
                i64t.const_int(argc as u64, false),
                "call.arity.ok",
            )
            .map_err(|e| e.to_string())?;
        self.builder
            .build_conditional_branch(cond, fast_bb, slow_bb)
            .map_err(|e| e.to_string())?;

        // FAST: one direct call to the uncurried entry, `fast(env, a1..an)`.
        self.builder.position_at_end(fast_bb);
        let mut metas: Vec<BasicMetadataTypeEnum> = Vec::with_capacity(argc + 1);
        metas.push(if self.uses_gc { i64t.into() } else { ptrt.into() });
        for (_, ty) in args {
            metas.push(self.abi_meta_type(ty)?);
        }
        let fnty = ret_abi.fn_type(&metas, false);
        let fp = self
            .builder
            .build_int_to_ptr(fast_ptr, ptrt, "fastfp")
            .map_err(|e| e.to_string())?;
        let env_arg: BasicMetadataValueEnum = if self.uses_gc {
            clos.into()
        } else {
            self.builder
                .build_int_to_ptr(clos, ptrt, "fastenv")
                .map_err(|e| e.to_string())?
                .into()
        };
        let mut callargs: Vec<BasicMetadataValueEnum> = Vec::with_capacity(argc + 1);
        callargs.push(env_arg);
        for v in &arg_vals {
            callargs.push((*v).into());
        }
        let fast_ret = self
            .builder
            .build_indirect_call(fnty, fp, &callargs, "fastcall")
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| "fast call returned no value".to_string())?;
        let fast_end = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(cont_bb)
            .map_err(|e| e.to_string())?;

        // SLOW: the curried apply chain, identical to nested `Comp::App`s.
        self.builder.position_at_end(slow_bb);
        let mut fv = clos;
        let mut cur_ty = fun_ty.clone();
        let mut slow_ret: Option<BasicValueEnum<'ctx>> = None;
        for (i, (_, aty)) in args.iter().enumerate() {
            let rest = match &cur_ty {
                Type::Fun(_, ret, _) => (**ret).clone(),
                _ => return Err("codegen: curried fallback head is not a function".into()),
            };
            let r = self.apply_one_val(fv, arg_vals[i], aty, &rest)?;
            if i + 1 < argc {
                fv = Self::expect_cell(r, "intermediate closure")?;
                cur_ty = rest;
            } else {
                slow_ret = Some(r);
            }
        }
        let slow_ret = slow_ret.ok_or_else(|| "codegen: empty saturated call".to_string())?;
        let slow_end = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(cont_bb)
            .map_err(|e| e.to_string())?;

        // CONT: merge the two results.
        self.builder.position_at_end(cont_bb);
        let phi = self
            .builder
            .build_phi(ret_abi, "callret")
            .map_err(|e| e.to_string())?;
        phi.add_incoming(&[(&fast_ret, fast_end), (&slow_ret, slow_end)]);
        Ok(phi.as_basic_value())
    }

    /// `if c then … else …` → a conditional branch into two blocks that merge
    /// with a phi. The condition is the uniform `i64` (Bool widened); we test
    /// `!= 0`. The env is snapshotted around each branch so branch-local
    /// bindings stay lexically scoped (and don't shadow the outer scope after).
    fn lower_if(
        &mut self,
        cond: &Atom,
        then: &Ir,
        els: &Ir,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let condv = self.lower_atom(cond)?.into_int_value();
        let zero = self.ctx.i64_type().const_int(0, false);
        let test = self
            .builder
            .build_int_compare(IntPredicate::NE, condv, zero, "if.test")
            .map_err(|e| e.to_string())?;

        let func = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .ok_or("codegen: no enclosing function for `if`")?;
        let then_bb = self.ctx.append_basic_block(func, "then");
        let else_bb = self.ctx.append_basic_block(func, "else");
        let merge_bb = self.ctx.append_basic_block(func, "merge");

        self.builder
            .build_conditional_branch(test, then_bb, else_bb)
            .map_err(|e| e.to_string())?;

        let saved = self.env.clone();

        // then
        self.builder.position_at_end(then_bb);
        let then_val = self.lower_block(then)?;
        let then_end = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| e.to_string())?;

        // else — restart from the pre-`if` env
        self.env = saved.clone();
        self.builder.position_at_end(else_bb);
        let else_val = self.lower_block(els)?;
        let else_end = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| e.to_string())?;

        // branch bindings are out of scope after the `if`
        self.env = saved;

        // merge
        self.builder.position_at_end(merge_bb);
        let phi = self
            .builder
            .build_phi(then_val.get_type(), "if.val")
            .map_err(|e| e.to_string())?;
        phi.add_incoming(&[(&then_val, then_end), (&else_val, else_end)]);
        Ok(phi.as_basic_value())
    }

    fn borrow_raw_scalar_arrays(
        &mut self,
        names: &[String],
    ) -> Result<HashMap<String, RawScalarArray<'ctx>>, String> {
        let i64t = self.ctx.i64_type();
        let mut out = HashMap::new();
        for name in names {
            if out.contains_key(name) || self.raw_scalar_arrays.contains_key(name) {
                continue;
            }
            let value = self
                .env
                .get(name)
                .ok_or_else(|| format!("codegen: unbound array `{name}` in loop"))?
                .value;
            let handle = Self::expect_cell(value, "array handle")?;
            let scalar_base = self.gc_ptr_call("locus_gc_scalar_fields_ptr", &[handle])?;
            let len = self
                .builder
                .build_load(i64t, scalar_base, &format!("{name}.len"))
                .map_err(|e| e.to_string())?
                .into_int_value();
            out.insert(name.clone(), RawScalarArray { scalar_base, len });
        }
        Ok(out)
    }

    fn lower_cached_scalar_array_slot(
        &mut self,
        cached: RawScalarArray<'ctx>,
        idx: &Atom,
        stride: u64,
        bounds_checked_by_loop: bool,
    ) -> Result<(IntValue<'ctx>, PointerValue<'ctx>), String> {
        let i64t = self.ctx.i64_type();
        let iv = Self::expect_cell(self.lower_atom(idx)?, "array index")?;
        if !bounds_checked_by_loop {
            let zero = i64t.const_zero();
            let neg = self
                .builder
                .build_int_compare(IntPredicate::SLT, iv, zero, "array.idx.neg")
                .map_err(|e| e.to_string())?;
            let past = self
                .builder
                .build_int_compare(IntPredicate::SGE, iv, cached.len, "array.idx.past")
                .map_err(|e| e.to_string())?;
            let bad = self
                .builder
                .build_or(neg, past, "array.idx.bad")
                .map_err(|e| e.to_string())?;
            self.trap_if(bad, "array.index")?;
        }

        let byte = if stride == 1 {
            iv
        } else {
            self.builder
                .build_int_mul(iv, i64t.const_int(stride, false), "array.byte")
                .map_err(|e| e.to_string())?
        };
        let cell = if stride == 8 {
            iv
        } else {
            self.builder
                .build_int_unsigned_div(byte, i64t.const_int(8, false), "array.cell")
                .map_err(|e| e.to_string())?
        };
        let slot = self
            .builder
            .build_int_add(cell, i64t.const_int(1, false), "array.slot")
            .map_err(|e| e.to_string())?;
        let ptr = self.dynamic_slot(cached.scalar_base, slot, "array.elem.ptr")?;
        Ok((byte, ptr))
    }

    fn lower_cached_scalar_array_get(
        &mut self,
        cached: RawScalarArray<'ctx>,
        idx: &Atom,
        stride: u64,
        bounds_checked_by_loop: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let (byte, ptr) =
            self.lower_cached_scalar_array_slot(cached, idx, stride, bounds_checked_by_loop)?;
        let raw = self
            .builder
            .build_load(i64t, ptr, "array.elem.raw")
            .map_err(|e| e.to_string())?
            .into_int_value();
        if stride == 8 {
            return Ok(raw.into());
        }

        let byte_in_cell = self
            .builder
            .build_int_unsigned_rem(byte, i64t.const_int(8, false), "array.byte.in.cell")
            .map_err(|e| e.to_string())?;
        let shift = self
            .builder
            .build_int_mul(byte_in_cell, i64t.const_int(8, false), "array.shift")
            .map_err(|e| e.to_string())?;
        let shifted = self
            .builder
            .build_right_shift(raw, shift, false, "array.shifted")
            .map_err(|e| e.to_string())?;
        let bits = stride * 8;
        let mask = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        Ok(self
            .builder
            .build_and(shifted, i64t.const_int(mask, false), "array.elem")
            .map_err(|e| e.to_string())?
            .into())
    }

    fn lower_cached_scalar_array_set(
        &mut self,
        cached: RawScalarArray<'ctx>,
        idx: &Atom,
        val: &Atom,
        stride: u64,
        bounds_checked_by_loop: bool,
    ) -> Result<(), String> {
        let i64t = self.ctx.i64_type();
        let vv = Self::expect_cell(self.lower_atom(val)?, "array element")?;
        let (byte, ptr) =
            self.lower_cached_scalar_array_slot(cached, idx, stride, bounds_checked_by_loop)?;
        if stride == 8 {
            self.builder
                .build_store(ptr, vv)
                .map_err(|e| e.to_string())?;
            return Ok(());
        }

        let old = self
            .builder
            .build_load(i64t, ptr, "array.store.old")
            .map_err(|e| e.to_string())?
            .into_int_value();
        let byte_in_cell = self
            .builder
            .build_int_unsigned_rem(byte, i64t.const_int(8, false), "array.store.byte.in.cell")
            .map_err(|e| e.to_string())?;
        let shift = self
            .builder
            .build_int_mul(byte_in_cell, i64t.const_int(8, false), "array.store.shift")
            .map_err(|e| e.to_string())?;
        let bits = stride * 8;
        let mask = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let shifted_mask = self
            .builder
            .build_left_shift(
                i64t.const_int(mask, false),
                shift,
                "array.store.shifted.mask",
            )
            .map_err(|e| e.to_string())?;
        let clear_mask = self
            .builder
            .build_xor(
                shifted_mask,
                i64t.const_int(u64::MAX, false),
                "array.store.clear.mask",
            )
            .map_err(|e| e.to_string())?;
        let cleared = self
            .builder
            .build_and(old, clear_mask, "array.store.cleared")
            .map_err(|e| e.to_string())?;
        let shifted_payload = self
            .builder
            .build_left_shift(vv, shift, "array.store.shifted.payload")
            .map_err(|e| e.to_string())?;
        let payload = self
            .builder
            .build_and(shifted_payload, shifted_mask, "array.store.payload")
            .map_err(|e| e.to_string())?;
        let new_value = self
            .builder
            .build_or(cleared, payload, "array.store.new")
            .map_err(|e| e.to_string())?;
        self.builder
            .build_store(ptr, new_value)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn lower_loop(
        &mut self,
        vars: &[LoopVar],
        cond: &Ir,
        steps: &[Ir],
        result: &Ir,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if vars.len() != steps.len() {
            return Err(format!(
                "codegen: loop has {} accumulator(s), but {} step expression(s)",
                vars.len(),
                steps.len()
            ));
        }
        let storages = vars
            .iter()
            .map(loop_var_storage)
            .collect::<Result<Vec<_>, _>>()?;
        let has_handle_roots = storages
            .iter()
            .any(|storage| matches!(storage, LoopVarStorage::HandleRoot));
        if has_handle_roots && !self.uses_gc {
            return Err("codegen: handle loop accumulators require the GC runtime".into());
        }

        let mut init_vals = Vec::with_capacity(vars.len());
        let mut handle_roots = Vec::with_capacity(vars.len());
        for (var, storage) in vars.iter().zip(storages.iter()) {
            let init = self.lower_atom(&var.init)?;
            match storage {
                LoopVarStorage::Scalar => {
                    init_vals.push(Some(init));
                    handle_roots.push(None);
                }
                LoopVarStorage::HandleRoot => {
                    let init = Self::expect_cell(init, "loop handle accumulator init")?;
                    let root = self.gc_root(init)?;
                    init_vals.push(None);
                    handle_roots.push(Some(root));
                }
            }
        }

        let handle_loop_names: HashSet<String> = vars
            .iter()
            .zip(storages.iter())
            .filter_map(|(var, storage)| {
                matches!(storage, LoopVarStorage::HandleRoot).then(|| var.name.clone())
            })
            .collect();
        let mut raw_array_names = if block_preserves_raw_heap_ptrs(cond)
            && steps.iter().all(block_preserves_raw_heap_ptrs)
        {
            let mut names = Vec::new();
            collect_raw_array_uses(cond, &mut names);
            for step in steps {
                collect_raw_array_uses(step, &mut names);
            }
            names
        } else {
            Vec::new()
        };
        raw_array_names.retain(|name| !handle_loop_names.contains(name));
        let borrowed_raw_arrays = self.borrow_raw_scalar_arrays(&raw_array_names)?;
        let proved_array_bounds = proved_loop_array_bounds(vars, cond, steps);

        let func = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .ok_or("codegen: no enclosing function for `loop`")?;
        let preheader = self
            .builder
            .get_insert_block()
            .ok_or("codegen: loop has no preheader block")?;
        let header_bb = self.ctx.append_basic_block(func, "loop");
        let body_bb = self.ctx.append_basic_block(func, "loop.body");
        let exit_bb = self.ctx.append_basic_block(func, "loop.exit");

        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| e.to_string())?;

        let saved_outer = self.env.clone();
        let saved_raw_arrays = self.raw_scalar_arrays.clone();
        let saved_loop_array_bounds = self.loop_array_bounds.clone();
        for (name, array) in borrowed_raw_arrays {
            self.raw_scalar_arrays.insert(name, array);
        }
        let mut loop_env = saved_outer.clone();

        self.builder.position_at_end(header_bb);
        let mut phis = Vec::with_capacity(vars.len());
        for (((var, storage), init), root) in vars
            .iter()
            .zip(storages.iter())
            .zip(init_vals.iter())
            .zip(handle_roots.iter())
        {
            match storage {
                LoopVarStorage::Scalar => {
                    let init = init.ok_or_else(|| {
                        format!(
                            "codegen: loop accumulator `{}` has no scalar init",
                            var.name
                        )
                    })?;
                    let phi = self
                        .builder
                        .build_phi(init.get_type(), &format!("loop.{}", var.name))
                        .map_err(|e| e.to_string())?;
                    phi.add_incoming(&[(&init, preheader)]);
                    loop_env.insert(
                        var.name.clone(),
                        EnvVal {
                            value: phi.as_basic_value(),
                            ty: var.ty.clone(),
                            layout: var.layout,
                        },
                    );
                    phis.push(Some(phi));
                }
                LoopVarStorage::HandleRoot => {
                    let root = root.ok_or_else(|| {
                        format!("codegen: loop accumulator `{}` has no root", var.name)
                    })?;
                    loop_env.insert(
                        var.name.clone(),
                        EnvVal {
                            value: root.into(),
                            ty: var.ty.clone(),
                            layout: var.layout,
                        },
                    );
                    phis.push(None);
                }
            }
        }

        self.env = loop_env.clone();
        let cond_val = self.lower_loop_block(cond)?;
        let cond_cell = Self::expect_cell(cond_val, "loop condition")?;
        let zero = self.ctx.i64_type().const_int(0, false);
        let test = self
            .builder
            .build_int_compare(IntPredicate::NE, cond_cell, zero, "loop.test")
            .map_err(|e| e.to_string())?;
        self.env = loop_env.clone();
        self.builder
            .build_conditional_branch(test, body_bb, exit_bb)
            .map_err(|e| e.to_string())?;

        self.builder.position_at_end(body_bb);
        let mut next_vals: Vec<Option<BasicValueEnum<'ctx>>> =
            (0..steps.len()).map(|_| None).collect();
        for bound in &proved_array_bounds {
            self.loop_array_bounds.insert(bound.clone());
        }
        if has_handle_roots {
            let needs_frame = steps
                .iter()
                .any(|step| block_performs_gc(step) || block_produces_gc_handle(step));
            let frame = if self.uses_gc && needs_frame {
                Some(self.gc_enter()?)
            } else {
                None
            };
            let mut next_handles = Vec::new();
            for (idx, (step, storage)) in steps.iter().zip(storages.iter()).enumerate() {
                self.env = loop_env.clone();
                let next = self.lower_block(step)?;
                match storage {
                    LoopVarStorage::Scalar => next_vals[idx] = Some(next),
                    LoopVarStorage::HandleRoot => {
                        let next = Self::expect_cell(next, "loop handle accumulator step")?;
                        next_handles.push((idx, next));
                    }
                }
            }
            for (idx, next) in next_handles {
                let root = handle_roots[idx].ok_or_else(|| {
                    format!(
                        "codegen: loop accumulator `{}` has no root for update",
                        vars[idx].name
                    )
                })?;
                self.gc_root_set(root, next)?;
            }
            if let Some(frame) = frame {
                self.gc_leave(frame)?;
            }
        } else {
            for (idx, step) in steps.iter().enumerate() {
                self.env = loop_env.clone();
                next_vals[idx] = Some(self.lower_loop_block(step)?);
            }
        }
        self.loop_array_bounds = saved_loop_array_bounds.clone();
        let body_end = self
            .builder
            .get_insert_block()
            .ok_or("codegen: loop body has no current block")?;
        for (idx, phi) in phis.iter().enumerate() {
            if let Some(phi) = phi {
                let next = next_vals[idx].ok_or_else(|| {
                    format!(
                        "codegen: loop accumulator `{}` has no scalar step",
                        vars[idx].name
                    )
                })?;
                phi.add_incoming(&[(&next, body_end)]);
            }
        }
        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| e.to_string())?;

        self.builder.position_at_end(exit_bb);
        self.raw_scalar_arrays = saved_raw_arrays;
        self.loop_array_bounds = saved_loop_array_bounds;
        self.env = loop_env;
        let out = self.lower_block(result)?;
        self.env = saved_outer;
        Ok(out)
    }

    fn lower_loop_block(&mut self, ir: &Ir) -> Result<BasicValueEnum<'ctx>, String> {
        let needs_frame = block_performs_gc(ir) || block_produces_gc_handle(ir);
        let frame = if self.uses_gc && needs_frame {
            Some(self.gc_enter()?)
        } else {
            None
        };
        let value = self.lower_block(ir)?;
        self.return_with_frame(frame, value)
    }

    fn trap_if(&mut self, cond: IntValue<'ctx>, label: &str) -> Result<(), String> {
        let func = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .ok_or_else(|| format!("codegen: no enclosing function for `{label}` trap"))?;
        let trap_bb = self.ctx.append_basic_block(func, &format!("{label}.trap"));
        let cont_bb = self.ctx.append_basic_block(func, &format!("{label}.cont"));
        self.builder
            .build_conditional_branch(cond, trap_bb, cont_bb)
            .map_err(|e| e.to_string())?;

        self.builder.position_at_end(trap_bb);
        let trap = self.module.get_function("llvm.trap").unwrap_or_else(|| {
            self.module
                .add_function("llvm.trap", self.ctx.void_type().fn_type(&[], false), None)
        });
        self.builder
            .build_call(trap, &[], &format!("{label}.abort"))
            .map_err(|e| e.to_string())?;
        self.builder
            .build_unreachable()
            .map_err(|e| e.to_string())?;

        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    /// **Tag** a concrete tag-room scalar into a uniform repr-poly word (D7):
    /// `value << 2` (low bits become `00`, so the collector reads the word as an
    /// inert immediate). Guarded by the **i62 overflow trap** (the `Int` decision):
    /// a value whose magnitude needs more than 62 bits aborts loudly at the tag
    /// site rather than silently truncating. The check is exact — `(v<<2)>>2 == v`
    /// (arithmetic shift) holds iff the top two bits were a sign-extension, i.e.
    /// `|v| < 2^61` — so it catches every out-of-range value, both signs.
    fn lower_tag(&mut self, a: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let v = Self::expect_cell(self.lower_atom(a)?, "tag operand")?;
        let two = i64t.const_int(2, false);
        let shifted = self
            .builder
            .build_left_shift(v, two, "tag.shl")
            .map_err(|e| e.to_string())?;
        // Round-trip through an arithmetic right shift; if it does not recover the
        // original value the scalar did not fit in i62 — trap.
        let restored = self
            .builder
            .build_right_shift(shifted, two, true, "tag.check")
            .map_err(|e| e.to_string())?;
        let overflow = self
            .builder
            .build_int_compare(IntPredicate::NE, restored, v, "tag.overflow")
            .map_err(|e| e.to_string())?;
        self.trap_if(overflow, "tag.i62")?;
        Ok(shifted.into())
    }

    /// **Untag** a uniform repr-poly word back to its concrete scalar (D7):
    /// arithmetic (sign-preserving) `value >> 2`, recovering the i62 value the tag
    /// shift stored. The inverse of [`lower_tag`](Self::lower_tag).
    fn lower_untag(&mut self, a: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let v = Self::expect_cell(self.lower_atom(a)?, "untag operand")?;
        let two = i64t.const_int(2, false);
        let restored = self
            .builder
            .build_right_shift(v, two, true, "untag.ashr")
            .map_err(|e| e.to_string())?;
        Ok(restored.into())
    }

    /// **ToPtr** (repr-poly): resolve a concrete managed handle to its traced
    /// object word (`addr|10`) via the runtime, so it can be laid into a `Var`
    /// (word) cell as a real interior pointer the collector follows and rewrites
    /// on evacuation. A shim call, NOT a shift. The stored field still routes
    /// through `set_word` — only the *value* differs (now an `addr|10` word, which
    /// `set_word`'s handle-magic assert accepts). Inverse of `lower_from_ptr`.
    fn lower_to_ptr(&mut self, a: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let v = Self::expect_cell(self.lower_atom(a)?, "to_ptr operand")?;
        let r = self
            .gc_call("locus_gc_to_ptr", &[v], true)?
            .expect("to_ptr returns a word");
        Ok(r.into())
    }

    /// **FromPtr** (repr-poly): intern an `addr|10` object word — read verbatim
    /// from a `Var` cell via `get_word` — into a fresh managed handle via the
    /// runtime, recovering a usable reference. Inverse of `lower_to_ptr`.
    fn lower_from_ptr(&mut self, a: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let v = Self::expect_cell(self.lower_atom(a)?, "from_ptr operand")?;
        let r = self
            .gc_call("locus_gc_from_ptr", &[v], true)?
            .expect("from_ptr returns a handle");
        Ok(r.into())
    }

    fn cell_to_f64(&self, value: IntValue<'ctx>, name: &str) -> Result<FloatValue<'ctx>, String> {
        Ok(self
            .builder
            .build_bit_cast(value, self.ctx.f64_type(), name)
            .map_err(|e| e.to_string())?
            .into_float_value())
    }

    fn f64_to_cell(&self, value: FloatValue<'ctx>, name: &str) -> Result<IntValue<'ctx>, String> {
        Ok(self
            .builder
            .build_bit_cast(value, self.ctx.i64_type(), name)
            .map_err(|e| e.to_string())?
            .into_int_value())
    }

    fn cell_to_f32(&self, value: IntValue<'ctx>, name: &str) -> Result<FloatValue<'ctx>, String> {
        let bits = self
            .builder
            .build_int_truncate(value, self.ctx.i32_type(), "f32.bits")
            .map_err(|e| e.to_string())?;
        Ok(self
            .builder
            .build_bit_cast(bits, self.ctx.f32_type(), name)
            .map_err(|e| e.to_string())?
            .into_float_value())
    }

    fn f32_to_cell(&self, value: FloatValue<'ctx>, name: &str) -> Result<IntValue<'ctx>, String> {
        let bits = self
            .builder
            .build_bit_cast(value, self.ctx.i32_type(), "f32.bits")
            .map_err(|e| e.to_string())?
            .into_int_value();
        self.builder
            .build_int_z_extend(bits, self.ctx.i64_type(), name)
            .map_err(|e| e.to_string())
    }

    fn vector_type(&self, shape: VectorShape, elem_ty: &Type) -> Result<VectorType<'ctx>, String> {
        let lanes = shape.lanes() as u32;
        match elem_ty {
            Type::Float32 => Ok(self.ctx.f32_type().vec_type(lanes)),
            Type::Float => Ok(self.ctx.f64_type().vec_type(lanes)),
            other => Err(format!(
                "codegen: {}[{other}] vectors are not supported yet",
                shape.name()
            )),
        }
    }

    fn mask_type(&self, shape: VectorShape) -> VectorType<'ctx> {
        self.ctx.bool_type().vec_type(shape.lanes() as u32)
    }

    fn lane_float_type(&self, elem_ty: &Type) -> Result<FloatType<'ctx>, String> {
        match elem_ty {
            Type::Float32 => Ok(self.ctx.f32_type()),
            Type::Float => Ok(self.ctx.f64_type()),
            other => Err(format!(
                "codegen: vector lane type `{other}` is unsupported"
            )),
        }
    }

    fn scalar_cell_to_lane(
        &self,
        cell: IntValue<'ctx>,
        elem_ty: &Type,
        name: &str,
    ) -> Result<FloatValue<'ctx>, String> {
        match elem_ty {
            Type::Float32 => self.cell_to_f32(cell, name),
            Type::Float => self.cell_to_f64(cell, name),
            other => Err(format!(
                "codegen: vector lane type `{other}` is unsupported"
            )),
        }
    }

    fn lane_to_scalar_cell(
        &self,
        lane: FloatValue<'ctx>,
        elem_ty: &Type,
        name: &str,
    ) -> Result<IntValue<'ctx>, String> {
        match elem_ty {
            Type::Float32 => self.f32_to_cell(lane, name),
            Type::Float => self.f64_to_cell(lane, name),
            other => Err(format!(
                "codegen: vector lane type `{other}` is unsupported"
            )),
        }
    }

    fn lower_vector_lit(
        &mut self,
        shape: VectorShape,
        elem_ty: &Type,
        elems: &[Atom],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if elems.len() != shape.lanes() {
            return Err(format!(
                "codegen: {} vector expects {} lanes, got {}",
                shape.name(),
                shape.lanes(),
                elems.len()
            ));
        }
        let vt = self.vector_type(shape, elem_ty)?;
        let mut vec = vt.get_undef();
        let i64t = self.ctx.i64_type();
        for (i, atom) in elems.iter().enumerate() {
            let cell = self.lower_atom(atom)?.into_int_value();
            let lane = self.scalar_cell_to_lane(cell, elem_ty, "vec.lane")?;
            vec = self
                .builder
                .build_insert_element(vec, lane, i64t.const_int(i as u64, false), "vec.ins")
                .map_err(|e| e.to_string())?;
        }
        Ok(vec.into())
    }

    fn lower_vector_splat(
        &mut self,
        shape: VectorShape,
        elem_ty: &Type,
        value: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let vt = self.vector_type(shape, elem_ty)?;
        let mut vec = vt.get_undef();
        let i64t = self.ctx.i64_type();
        let cell = self.lower_atom(value)?.into_int_value();
        let lane = self.scalar_cell_to_lane(cell, elem_ty, "splat.lane")?;
        for i in 0..shape.lanes() {
            vec = self
                .builder
                .build_insert_element(vec, lane, i64t.const_int(i as u64, false), "splat.ins")
                .map_err(|e| e.to_string())?;
        }
        Ok(vec.into())
    }

    /// The byte size of a single vector lane element in an array payload — the
    /// stride between adjacent loaded elements. `Float32` is 4 B, `Float` 8 B.
    /// (Vector loads only support these lane types — same as `vector_type`.)
    fn lane_byte_size(elem_ty: &Type) -> Result<u64, String> {
        match elem_ty {
            Type::Float32 => Ok(4),
            Type::Float => Ok(8),
            other => Err(format!(
                "codegen: vector lane type `{other}` has no array element size"
            )),
        }
    }

    /// **Address + bounds machinery for a packed array vector load/store** (SIMD
    /// Sprint 2). Resolves the array's raw scalar-payload base (reusing the
    /// borrowed loop pointer when the array is a cached raw scalar array, else a
    /// `locus_gc_scalar_fields_ptr` call), bounds-checks the **whole vector**
    /// (`0 <= idx && idx + lanes <= len`, the OOB trap path — a packed load must
    /// not read past the array), and returns a pointer to the element-`idx` byte
    /// address `payload + 8 + idx*elem_bytes`. Scalar slot 0 is the length, so
    /// the payload data begins at byte 8 (cell 1). The pointer is to the lane
    /// element type; the caller does one vector load/store there.
    fn vector_array_elem_ptr(
        &mut self,
        shape: VectorShape,
        elem_ty: &Type,
        arr: &Atom,
        idx: &Atom,
    ) -> Result<PointerValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let lanes = shape.lanes() as u64;
        let elem_bytes = Self::lane_byte_size(elem_ty)?;

        // The raw scalar-payload base + the logical length. Inside a no-GC loop
        // the base is already borrowed (and bounds may be partially known); else
        // fetch it for this one operation. Either way we re-check `idx + lanes`.
        let (scalar_base, len) = if let Atom::Var(name) = arr {
            if let Some(cached) = self.raw_scalar_arrays.get(name).copied() {
                (cached.scalar_base, cached.len)
            } else {
                self.scalar_payload_base_and_len(arr)?
            }
        } else {
            self.scalar_payload_base_and_len(arr)?
        };

        let iv = Self::expect_cell(self.lower_atom(idx)?, "vector array index")?;
        // Bounds: `idx < 0 || idx + lanes > len` traps. `idx + lanes` is computed
        // in i64; a `len` here is a small non-negative element count, and `idx`
        // is the loop's already-range-checked counter, so the add cannot wrap in
        // practice — but the compare is unsigned-safe via the explicit `idx < 0`.
        let neg = self
            .builder
            .build_int_compare(IntPredicate::SLT, iv, i64t.const_zero(), "vec.idx.neg")
            .map_err(|e| e.to_string())?;
        let end = self
            .builder
            .build_int_add(iv, i64t.const_int(lanes, false), "vec.idx.end")
            .map_err(|e| e.to_string())?;
        let past = self
            .builder
            .build_int_compare(IntPredicate::SGT, end, len, "vec.idx.past")
            .map_err(|e| e.to_string())?;
        let bad = self
            .builder
            .build_or(neg, past, "vec.idx.bad")
            .map_err(|e| e.to_string())?;
        self.trap_if(bad, "vector.index")?;

        // Byte address of element `idx`: payload data starts at byte 8 (scalar
        // slot 0 is the length), then `idx * elem_bytes`. GEP on an `i8` base so
        // the offset is in bytes, yielding a pointer the vector op reads/writes.
        let i8t = self.ctx.i8_type();
        let byte_off = self
            .builder
            .build_int_mul(iv, i64t.const_int(elem_bytes, false), "vec.byte")
            .map_err(|e| e.to_string())?;
        let byte_off = self
            .builder
            .build_int_add(byte_off, i64t.const_int(8, false), "vec.byte.hdr")
            .map_err(|e| e.to_string())?;
        let elem_ptr = unsafe {
            self.builder
                .build_gep(i8t, scalar_base, &[byte_off], "vec.elem.ptr")
        }
        .map_err(|e| e.to_string())?;
        Ok(elem_ptr)
    }

    /// Borrow an array's raw scalar-payload base pointer and read its logical
    /// length (scalar slot 0). The non-cached counterpart to a `raw_scalar_arrays`
    /// entry — used by the vector load/store when the array isn't already borrowed
    /// for a loop.
    fn scalar_payload_base_and_len(
        &mut self,
        arr: &Atom,
    ) -> Result<(PointerValue<'ctx>, IntValue<'ctx>), String> {
        let i64t = self.ctx.i64_type();
        let handle = Self::expect_cell(self.lower_atom(arr)?, "array handle")?;
        let scalar_base = self.gc_ptr_call("locus_gc_scalar_fields_ptr", &[handle])?;
        let len = self
            .builder
            .build_load(i64t, scalar_base, "vec.arr.len")
            .map_err(|e| e.to_string())?
            .into_int_value();
        Ok((scalar_base, len))
    }

    /// `loadShape(arr, i)` → one **packed vector load** of `shape.lanes()`
    /// contiguous array elements at element index `i` (SIMD Sprint 2). The load
    /// is **element-aligned** (`align = elem_bytes`): the payload base is only
    /// 8-byte aligned and `i` is arbitrary, so the vector may start at any element
    /// offset — correctness over peak alignment (Sprint 3 polish). It is a single
    /// `<lanes x elem>` load, not `lanes` scalar loads.
    fn lower_vector_load(
        &mut self,
        shape: VectorShape,
        elem_ty: &Type,
        arr: &Atom,
        idx: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let vt = self.vector_type(shape, elem_ty)?;
        let elem_bytes = Self::lane_byte_size(elem_ty)? as u32;
        let elem_ptr = self.vector_array_elem_ptr(shape, elem_ty, arr, idx)?;
        let loaded = self
            .builder
            .build_load(vt, elem_ptr, "vec.load")
            .map_err(|e| e.to_string())?;
        loaded
            .as_instruction_value()
            .expect("a load is an instruction")
            .set_alignment(elem_bytes)
            .map_err(|e| e.to_string())?;
        Ok(loaded)
    }

    /// `storeShape(arr, i, v)` → one **packed vector store** of `v`'s lanes to the
    /// `shape.lanes()` contiguous elements at index `i`; yields `Unit` (0).
    /// Element-aligned, like the load.
    fn lower_vector_store(
        &mut self,
        shape: VectorShape,
        elem_ty: &Type,
        arr: &Atom,
        idx: &Atom,
        value: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem_bytes = Self::lane_byte_size(elem_ty)? as u32;
        let v = self.lower_atom(value)?;
        let elem_ptr = self.vector_array_elem_ptr(shape, elem_ty, arr, idx)?;
        let store = self
            .builder
            .build_store(elem_ptr, v)
            .map_err(|e| e.to_string())?;
        store.set_alignment(elem_bytes).map_err(|e| e.to_string())?;
        Ok(self.ctx.i64_type().const_zero().into())
    }

    fn lower_vector_bin(
        &mut self,
        op: BinOp,
        _shape: VectorShape,
        elem_ty: &Type,
        lhs: &Atom,
        rhs: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if !matches!(elem_ty, Type::Float32 | Type::Float) {
            return Err(format!(
                "codegen: vector lane type `{elem_ty}` is unsupported"
            ));
        }
        let a = self.lower_atom(lhs)?.into_vector_value();
        let b = self.lower_atom(rhs)?.into_vector_value();
        let r = match op {
            BinOp::Add => self
                .builder
                .build_float_add(a, b, "vadd")
                .map_err(|e| e.to_string())?,
            BinOp::Sub => self
                .builder
                .build_float_sub(a, b, "vsub")
                .map_err(|e| e.to_string())?,
            BinOp::Mul => self
                .builder
                .build_float_mul(a, b, "vmul")
                .map_err(|e| e.to_string())?,
            BinOp::Div => self
                .builder
                .build_float_div(a, b, "vdiv")
                .map_err(|e| e.to_string())?,
            _ => {
                return Err(format!(
                    "codegen: `{}` is not a supported vector operation",
                    op.symbol()
                ));
            }
        };
        Ok(r.into())
    }

    fn lower_vector_compare(
        &mut self,
        op: BinOp,
        shape: VectorShape,
        elem_ty: &Type,
        lhs: &Atom,
        rhs: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if !matches!(elem_ty, Type::Float32 | Type::Float) {
            return Err(format!(
                "codegen: vector lane type `{elem_ty}` is unsupported"
            ));
        }
        let _ = self.mask_type(shape);
        let a = self.lower_atom(lhs)?.into_vector_value();
        let b = self.lower_atom(rhs)?.into_vector_value();
        let pred = match op {
            BinOp::Eq => FloatPredicate::OEQ,
            BinOp::Ne => FloatPredicate::ONE,
            BinOp::Lt => FloatPredicate::OLT,
            BinOp::Le => FloatPredicate::OLE,
            BinOp::Gt => FloatPredicate::OGT,
            BinOp::Ge => FloatPredicate::OGE,
            _ => {
                return Err(format!(
                    "codegen: `{}` is not a supported vector comparison",
                    op.symbol()
                ));
            }
        };
        Ok(self
            .builder
            .build_float_compare(pred, a, b, "vcmp")
            .map_err(|e| e.to_string())?
            .into())
    }

    fn lower_vector_select(
        &mut self,
        shape: VectorShape,
        elem_ty: &Type,
        mask: &Atom,
        then_value: &Atom,
        else_value: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let _ = self.vector_type(shape, elem_ty)?;
        let m = self.lower_atom(mask)?.into_vector_value();
        let t = self.lower_atom(then_value)?.into_vector_value();
        let e = self.lower_atom(else_value)?.into_vector_value();
        self.builder
            .build_select(m, t, e, "vselect")
            .map_err(|e| e.to_string())
    }

    fn lower_mask_reduce(
        &mut self,
        op: MaskReduceOp,
        shape: VectorShape,
        mask: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let v = self.lower_atom(mask)?.into_vector_value();
        let i64t = self.ctx.i64_type();
        let mut lanes = Vec::with_capacity(shape.lanes());
        for i in 0..shape.lanes() {
            let idx = i64t.const_int(i as u64, false);
            let lane = self
                .builder
                .build_extract_element(v, idx, "mask.lane")
                .map_err(|e| e.to_string())?
                .into_int_value();
            lanes.push(lane);
        }
        let mut lanes = lanes.into_iter();
        let mut acc = lanes
            .next()
            .ok_or_else(|| "codegen: empty SIMD mask shape".to_string())?;
        for lane in lanes {
            acc = match op {
                MaskReduceOp::Any => self
                    .builder
                    .build_or(acc, lane, "mask.any")
                    .map_err(|e| e.to_string())?,
                MaskReduceOp::All => self
                    .builder
                    .build_and(acc, lane, "mask.all")
                    .map_err(|e| e.to_string())?,
            };
        }
        Ok(self
            .builder
            .build_int_z_extend(acc, i64t, "mask.bool")
            .map_err(|e| e.to_string())?
            .into())
    }

    fn lower_vector_extract(
        &mut self,
        vector: &Atom,
        lane: usize,
        elem_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let v = self.lower_atom(vector)?.into_vector_value();
        let idx = self.ctx.i64_type().const_int(lane as u64, false);
        let lane_value = self
            .builder
            .build_extract_element(v, idx, "vec.extract")
            .map_err(|e| e.to_string())?
            .into_float_value();
        Ok(self
            .lane_to_scalar_cell(lane_value, elem_ty, "vec.extract.cell")?
            .into())
    }

    fn float_intrinsic_suffix(&self, ty: &Type) -> Result<String, String> {
        match ty {
            Type::Float32 => Ok("f32".into()),
            Type::Float => Ok("f64".into()),
            Type::Vector(shape, elem) => match &**elem {
                Type::Float32 => Ok(format!("v{}f32", shape.lanes())),
                Type::Float => Ok(format!("v{}f64", shape.lanes())),
                other => Err(format!(
                    "codegen: vector lane type `{other}` is unsupported"
                )),
            },
            other => Err(format!("codegen: `{other}` is not a floating math type")),
        }
    }

    fn lower_float_math_unary(
        &mut self,
        op: FloatMathOp,
        ty: &Type,
        value: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match op {
            FloatMathOp::Sqrt => match ty {
                Type::Float => {
                    let cell = self.lower_atom(value)?.into_int_value();
                    let f = self.cell_to_f64(cell, "sqrt.in")?;
                    let r = self.call_f64_unary_intrinsic("llvm.sqrt.f64", f, "sqrt")?;
                    Ok(self.f64_to_cell(r, "sqrt.bits")?.into())
                }
                Type::Float32 => {
                    let cell = self.lower_atom(value)?.into_int_value();
                    let f = self.cell_to_f32(cell, "sqrt.f32.in")?;
                    let r = self.call_f32_unary_intrinsic("llvm.sqrt.f32", f, "sqrt.f32")?;
                    Ok(self.f32_to_cell(r, "sqrt.f32.bits")?.into())
                }
                Type::Vector(shape, elem) => {
                    let v = self.lower_atom(value)?.into_vector_value();
                    let r = self.call_vector_unary_intrinsic("llvm.sqrt", *shape, elem, v)?;
                    Ok(r.into())
                }
                other => Err(format!(
                    "codegen: `{}` does not support `{other}`",
                    op.symbol()
                )),
            },
            FloatMathOp::Sum => match ty {
                Type::Vector(shape, elem) => {
                    let v = self.lower_atom(value)?.into_vector_value();
                    let r = self.call_vector_reduce_fadd(*shape, elem, v, "sum")?;
                    Ok(self.lane_to_scalar_cell(r, elem, "sum.bits")?.into())
                }
                other => Err(format!(
                    "codegen: `{}` does not support `{other}`",
                    op.symbol()
                )),
            },
            FloatMathOp::Length => match ty {
                Type::Vector(shape, elem) => {
                    let v = self.lower_atom(value)?.into_vector_value();
                    let squared = self
                        .builder
                        .build_float_mul(v, v, "length.square")
                        .map_err(|e| e.to_string())?;
                    let sum = self.call_vector_reduce_fadd(*shape, elem, squared, "length.sum")?;
                    let r = match &**elem {
                        Type::Float32 => {
                            self.call_f32_unary_intrinsic("llvm.sqrt.f32", sum, "length.sqrt")?
                        }
                        Type::Float => {
                            self.call_f64_unary_intrinsic("llvm.sqrt.f64", sum, "length.sqrt")?
                        }
                        other => {
                            return Err(format!(
                                "codegen: vector lane type `{other}` is unsupported"
                            ));
                        }
                    };
                    Ok(self.lane_to_scalar_cell(r, elem, "length.bits")?.into())
                }
                other => Err(format!(
                    "codegen: `{}` does not support `{other}`",
                    op.symbol()
                )),
            },
            other => Err(format!(
                "codegen: `{}` is not unary floating math",
                other.symbol()
            )),
        }
    }

    fn lower_float_math_binary(
        &mut self,
        op: FloatMathOp,
        ty: &Type,
        lhs: &Atom,
        rhs: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match op {
            FloatMathOp::Dot => match ty {
                Type::Vector(shape, elem) => {
                    let a = self.lower_atom(lhs)?.into_vector_value();
                    let b = self.lower_atom(rhs)?.into_vector_value();
                    let product = self
                        .builder
                        .build_float_mul(a, b, "dot.mul")
                        .map_err(|e| e.to_string())?;
                    let r = self.call_vector_reduce_fadd(*shape, elem, product, "dot")?;
                    Ok(self.lane_to_scalar_cell(r, elem, "dot.bits")?.into())
                }
                other => Err(format!(
                    "codegen: `{}` does not support `{other}`",
                    op.symbol()
                )),
            },
            other => Err(format!(
                "codegen: `{}` is not binary floating math",
                other.symbol()
            )),
        }
    }

    fn lower_float_math_ternary(
        &mut self,
        op: FloatMathOp,
        ty: &Type,
        a: &Atom,
        b: &Atom,
        c: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match op {
            FloatMathOp::Fma => match ty {
                Type::Float => {
                    let ac = self.lower_atom(a)?.into_int_value();
                    let bc = self.lower_atom(b)?.into_int_value();
                    let cc = self.lower_atom(c)?.into_int_value();
                    let av = self.cell_to_f64(ac, "fma.a")?;
                    let bv = self.cell_to_f64(bc, "fma.b")?;
                    let cv = self.cell_to_f64(cc, "fma.c")?;
                    let r = self.call_f64_ternary_intrinsic("llvm.fma.f64", av, bv, cv, "fma")?;
                    Ok(self.f64_to_cell(r, "fma.bits")?.into())
                }
                Type::Float32 => {
                    let ac = self.lower_atom(a)?.into_int_value();
                    let bc = self.lower_atom(b)?.into_int_value();
                    let cc = self.lower_atom(c)?.into_int_value();
                    let av = self.cell_to_f32(ac, "fma.f32.a")?;
                    let bv = self.cell_to_f32(bc, "fma.f32.b")?;
                    let cv = self.cell_to_f32(cc, "fma.f32.c")?;
                    let r =
                        self.call_f32_ternary_intrinsic("llvm.fma.f32", av, bv, cv, "fma.f32")?;
                    Ok(self.f32_to_cell(r, "fma.f32.bits")?.into())
                }
                Type::Vector(shape, elem) => {
                    let av = self.lower_atom(a)?.into_vector_value();
                    let bv = self.lower_atom(b)?.into_vector_value();
                    let cv = self.lower_atom(c)?.into_vector_value();
                    let r =
                        self.call_vector_ternary_intrinsic("llvm.fma", *shape, elem, av, bv, cv)?;
                    Ok(r.into())
                }
                other => Err(format!(
                    "codegen: `{}` does not support `{other}`",
                    op.symbol()
                )),
            },
            other => Err(format!(
                "codegen: `{}` is not ternary floating math",
                other.symbol()
            )),
        }
    }

    fn call_f64_unary_intrinsic(
        &self,
        intrinsic: &str,
        arg: FloatValue<'ctx>,
        name: &str,
    ) -> Result<FloatValue<'ctx>, String> {
        let f64t = self.ctx.f64_type();
        let f = self.module.get_function(intrinsic).unwrap_or_else(|| {
            self.module
                .add_function(intrinsic, f64t.fn_type(&[f64t.into()], false), None)
        });
        Ok(self
            .builder
            .build_call(f, &[arg.into()], name)
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_float_value())
    }

    fn call_f32_unary_intrinsic(
        &self,
        intrinsic: &str,
        arg: FloatValue<'ctx>,
        name: &str,
    ) -> Result<FloatValue<'ctx>, String> {
        let f32t = self.ctx.f32_type();
        let f = self.module.get_function(intrinsic).unwrap_or_else(|| {
            self.module
                .add_function(intrinsic, f32t.fn_type(&[f32t.into()], false), None)
        });
        Ok(self
            .builder
            .build_call(f, &[arg.into()], name)
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_float_value())
    }

    fn call_f64_ternary_intrinsic(
        &self,
        intrinsic: &str,
        a: FloatValue<'ctx>,
        b: FloatValue<'ctx>,
        c: FloatValue<'ctx>,
        name: &str,
    ) -> Result<FloatValue<'ctx>, String> {
        let f64t = self.ctx.f64_type();
        let f = self.module.get_function(intrinsic).unwrap_or_else(|| {
            self.module.add_function(
                intrinsic,
                f64t.fn_type(&[f64t.into(), f64t.into(), f64t.into()], false),
                None,
            )
        });
        Ok(self
            .builder
            .build_call(f, &[a.into(), b.into(), c.into()], name)
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_float_value())
    }

    fn call_f32_ternary_intrinsic(
        &self,
        intrinsic: &str,
        a: FloatValue<'ctx>,
        b: FloatValue<'ctx>,
        c: FloatValue<'ctx>,
        name: &str,
    ) -> Result<FloatValue<'ctx>, String> {
        let f32t = self.ctx.f32_type();
        let f = self.module.get_function(intrinsic).unwrap_or_else(|| {
            self.module.add_function(
                intrinsic,
                f32t.fn_type(&[f32t.into(), f32t.into(), f32t.into()], false),
                None,
            )
        });
        Ok(self
            .builder
            .build_call(f, &[a.into(), b.into(), c.into()], name)
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_float_value())
    }

    fn call_vector_unary_intrinsic(
        &self,
        base: &str,
        shape: VectorShape,
        elem_ty: &Type,
        arg: inkwell::values::VectorValue<'ctx>,
    ) -> Result<inkwell::values::VectorValue<'ctx>, String> {
        let ty = Type::Vector(shape, Box::new(elem_ty.clone()));
        let suffix = self.float_intrinsic_suffix(&ty)?;
        let intrinsic = format!("{base}.{suffix}");
        let vt = self.vector_type(shape, elem_ty)?;
        let f = self.module.get_function(&intrinsic).unwrap_or_else(|| {
            self.module
                .add_function(&intrinsic, vt.fn_type(&[vt.into()], false), None)
        });
        Ok(self
            .builder
            .build_call(f, &[arg.into()], "vec.unary")
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_vector_value())
    }

    fn call_vector_reduce_fadd(
        &self,
        shape: VectorShape,
        elem_ty: &Type,
        arg: inkwell::values::VectorValue<'ctx>,
        name: &str,
    ) -> Result<FloatValue<'ctx>, String> {
        let ty = Type::Vector(shape, Box::new(elem_ty.clone()));
        let suffix = self.float_intrinsic_suffix(&ty)?;
        let intrinsic = format!("llvm.vector.reduce.fadd.{suffix}");
        let ft = self.lane_float_type(elem_ty)?;
        let vt = self.vector_type(shape, elem_ty)?;
        let f = self.module.get_function(&intrinsic).unwrap_or_else(|| {
            self.module
                .add_function(&intrinsic, ft.fn_type(&[ft.into(), vt.into()], false), None)
        });
        let start = ft.const_float(0.0);
        Ok(self
            .builder
            .build_call(f, &[start.into(), arg.into()], name)
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_float_value())
    }

    fn call_vector_ternary_intrinsic(
        &self,
        base: &str,
        shape: VectorShape,
        elem_ty: &Type,
        a: inkwell::values::VectorValue<'ctx>,
        b: inkwell::values::VectorValue<'ctx>,
        c: inkwell::values::VectorValue<'ctx>,
    ) -> Result<inkwell::values::VectorValue<'ctx>, String> {
        let ty = Type::Vector(shape, Box::new(elem_ty.clone()));
        let suffix = self.float_intrinsic_suffix(&ty)?;
        let intrinsic = format!("{base}.{suffix}");
        let vt = self.vector_type(shape, elem_ty)?;
        let f = self.module.get_function(&intrinsic).unwrap_or_else(|| {
            self.module.add_function(
                &intrinsic,
                vt.fn_type(&[vt.into(), vt.into(), vt.into()], false),
                None,
            )
        });
        Ok(self
            .builder
            .build_call(f, &[a.into(), b.into(), c.into()], "vec.ternary")
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_vector_value())
    }

    fn lower_float_bin(
        &mut self,
        op: BinOp,
        lhs: &Atom,
        rhs: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let lhs_cell = self.lower_atom(lhs)?.into_int_value();
        let rhs_cell = self.lower_atom(rhs)?.into_int_value();
        let a = self.cell_to_f64(lhs_cell, "lhs.f64")?;
        let c = self.cell_to_f64(rhs_cell, "rhs.f64")?;
        let b = self.builder;
        let i64t = self.ctx.i64_type();
        match op {
            BinOp::Add => {
                let r = b.build_float_add(a, c, "fadd").map_err(|e| e.to_string())?;
                Ok(self.f64_to_cell(r, "fadd.bits")?.into())
            }
            BinOp::Sub => {
                let r = b.build_float_sub(a, c, "fsub").map_err(|e| e.to_string())?;
                Ok(self.f64_to_cell(r, "fsub.bits")?.into())
            }
            BinOp::Mul => {
                let r = b.build_float_mul(a, c, "fmul").map_err(|e| e.to_string())?;
                Ok(self.f64_to_cell(r, "fmul.bits")?.into())
            }
            BinOp::Div => {
                let r = b.build_float_div(a, c, "fdiv").map_err(|e| e.to_string())?;
                Ok(self.f64_to_cell(r, "fdiv.bits")?.into())
            }
            BinOp::Eq => {
                let cmp = b
                    .build_float_compare(FloatPredicate::OEQ, a, c, "feq")
                    .map_err(|e| e.to_string())?;
                Ok(b.build_int_z_extend(cmp, i64t, "feqw")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            BinOp::Ne => {
                let cmp = b
                    .build_float_compare(FloatPredicate::ONE, a, c, "fne")
                    .map_err(|e| e.to_string())?;
                Ok(b.build_int_z_extend(cmp, i64t, "fnew")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            BinOp::Lt => {
                let cmp = b
                    .build_float_compare(FloatPredicate::OLT, a, c, "flt")
                    .map_err(|e| e.to_string())?;
                Ok(b.build_int_z_extend(cmp, i64t, "fltw")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            BinOp::Le => {
                let cmp = b
                    .build_float_compare(FloatPredicate::OLE, a, c, "fle")
                    .map_err(|e| e.to_string())?;
                Ok(b.build_int_z_extend(cmp, i64t, "flew")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            BinOp::Gt => {
                let cmp = b
                    .build_float_compare(FloatPredicate::OGT, a, c, "fgt")
                    .map_err(|e| e.to_string())?;
                Ok(b.build_int_z_extend(cmp, i64t, "fgtw")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            BinOp::Ge => {
                let cmp = b
                    .build_float_compare(FloatPredicate::OGE, a, c, "fge")
                    .map_err(|e| e.to_string())?;
                Ok(b.build_int_z_extend(cmp, i64t, "fgew")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            _ => Err(format!(
                "codegen: `{}` is not a Float operation",
                op.symbol()
            )),
        }
    }

    fn lower_cast(&mut self, op: CastOp, atom: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let value = self.lower_atom(atom)?.into_int_value();
        let i64t = self.ctx.i64_type();
        match op {
            CastOp::ToFloat => {
                let f = self
                    .builder
                    .build_signed_int_to_float(value, self.ctx.f64_type(), "sitofp")
                    .map_err(|e| e.to_string())?;
                Ok(self.f64_to_cell(f, "sitofp.bits")?.into())
            }
            CastOp::Floor => {
                let f = self.cell_to_f64(value, "floor.in")?;
                let rounded = self.call_f64_unary_intrinsic("llvm.floor.f64", f, "floor")?;
                Ok(self
                    .builder
                    .build_float_to_signed_int(rounded, i64t, "floor.i64")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            CastOp::Round => {
                let f = self.cell_to_f64(value, "round.in")?;
                let rounded = self.call_f64_unary_intrinsic("llvm.round.f64", f, "round")?;
                Ok(self
                    .builder
                    .build_float_to_signed_int(rounded, i64t, "round.i64")
                    .map_err(|e| e.to_string())?
                    .into())
            }
            CastOp::ToFloat32 => {
                let f = self.cell_to_f64(value, "tof32.in")?;
                let narrowed = self
                    .builder
                    .build_float_trunc(f, self.ctx.f32_type(), "fptrunc")
                    .map_err(|e| e.to_string())?;
                Ok(self.f32_to_cell(narrowed, "f32.cell")?.into())
            }
            CastOp::FromFloat32 => {
                let f = self.cell_to_f32(value, "fromf32.in")?;
                let widened = self
                    .builder
                    .build_float_ext(f, self.ctx.f64_type(), "fpext")
                    .map_err(|e| e.to_string())?;
                Ok(self.f64_to_cell(widened, "f64.cell")?.into())
            }
        }
    }

    /// A primitive integer op. Operands are `i64`; comparisons produce `i1`,
    /// zero-extended to `i64` to keep the uniform value model.

    fn lower_bin(
        &mut self,
        op: BinOp,
        lhs: &Atom,
        rhs: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let a = self.lower_atom(lhs)?.into_int_value();
        let c = self.lower_atom(rhs)?.into_int_value();
        let b = self.builder;
        let i64t = self.ctx.i64_type();
        let r = match op {
            BinOp::Add | BinOp::AddWrap => {
                b.build_int_add(a, c, "add").map_err(|e| e.to_string())?
            }
            BinOp::Sub | BinOp::SubWrap => {
                b.build_int_sub(a, c, "sub").map_err(|e| e.to_string())?
            }
            BinOp::Mul | BinOp::MulWrap => {
                b.build_int_mul(a, c, "mul").map_err(|e| e.to_string())?
            }
            BinOp::Div => {
                return self.lower_int_div(a, c);
            }
            BinOp::Mod => {
                return self.lower_int_rem(a, c);
            }
            BinOp::AddChecked | BinOp::SubChecked | BinOp::MulChecked => {
                return self.lower_checked_int(op, a, c);
            }
            BinOp::And => b.build_and(a, c, "and").map_err(|e| e.to_string())?,
            BinOp::Or => b.build_or(a, c, "or").map_err(|e| e.to_string())?,
            BinOp::Xor => b.build_xor(a, c, "xor").map_err(|e| e.to_string())?,
            BinOp::Shl => b.build_left_shift(a, c, "shl").map_err(|e| e.to_string())?,
            // arithmetic (sign-preserving) right shift — values are signed i64.
            BinOp::Shr => b
                .build_right_shift(a, c, true, "shr")
                .map_err(|e| e.to_string())?,
            BinOp::Eq => {
                let cmp = b
                    .build_int_compare(IntPredicate::EQ, a, c, "eq")
                    .map_err(|e| e.to_string())?;
                b.build_int_z_extend(cmp, i64t, "eqw")
                    .map_err(|e| e.to_string())?
            }
            BinOp::Ne => {
                let cmp = b
                    .build_int_compare(IntPredicate::NE, a, c, "ne")
                    .map_err(|e| e.to_string())?;
                b.build_int_z_extend(cmp, i64t, "new")
                    .map_err(|e| e.to_string())?
            }
            BinOp::Lt => {
                let cmp = b
                    .build_int_compare(IntPredicate::SLT, a, c, "lt")
                    .map_err(|e| e.to_string())?;
                b.build_int_z_extend(cmp, i64t, "ltw")
                    .map_err(|e| e.to_string())?
            }
            BinOp::Le => {
                let cmp = b
                    .build_int_compare(IntPredicate::SLE, a, c, "le")
                    .map_err(|e| e.to_string())?;
                b.build_int_z_extend(cmp, i64t, "lew")
                    .map_err(|e| e.to_string())?
            }
            BinOp::Gt => {
                let cmp = b
                    .build_int_compare(IntPredicate::SGT, a, c, "gt")
                    .map_err(|e| e.to_string())?;
                b.build_int_z_extend(cmp, i64t, "gtw")
                    .map_err(|e| e.to_string())?
            }
            BinOp::Ge => {
                let cmp = b
                    .build_int_compare(IntPredicate::SGE, a, c, "ge")
                    .map_err(|e| e.to_string())?;
                b.build_int_z_extend(cmp, i64t, "gew")
                    .map_err(|e| e.to_string())?
            }
        };
        Ok(r.into())
    }

    fn lower_int_div(
        &mut self,
        lhs: IntValue<'ctx>,
        rhs: IntValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let b = self.builder;
        let zero = i64t.const_zero();
        let minus_one = i64t.const_int((-1_i64) as u64, true);
        let min = i64t.const_int(i64::MIN as u64, true);

        let div_by_zero = b
            .build_int_compare(IntPredicate::EQ, rhs, zero, "div.zero")
            .map_err(|e| e.to_string())?;
        let lhs_min = b
            .build_int_compare(IntPredicate::EQ, lhs, min, "div.lhs_min")
            .map_err(|e| e.to_string())?;
        let rhs_minus_one = b
            .build_int_compare(IntPredicate::EQ, rhs, minus_one, "div.rhs_minus_one")
            .map_err(|e| e.to_string())?;
        let overflow = b
            .build_and(lhs_min, rhs_minus_one, "div.overflow")
            .map_err(|e| e.to_string())?;
        let invalid = b
            .build_or(div_by_zero, overflow, "div.invalid")
            .map_err(|e| e.to_string())?;
        self.trap_if(invalid, "div")?;

        let r = b
            .build_int_signed_div(lhs, rhs, "sdiv")
            .map_err(|e| e.to_string())?;
        Ok(r.into())
    }

    fn lower_int_rem(
        &mut self,
        lhs: IntValue<'ctx>,
        rhs: IntValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let b = self.builder;
        let zero = i64t.const_zero();
        let minus_one = i64t.const_int((-1_i64) as u64, true);
        let min = i64t.const_int(i64::MIN as u64, true);

        let rem_by_zero = b
            .build_int_compare(IntPredicate::EQ, rhs, zero, "rem.zero")
            .map_err(|e| e.to_string())?;
        let lhs_min = b
            .build_int_compare(IntPredicate::EQ, lhs, min, "rem.lhs_min")
            .map_err(|e| e.to_string())?;
        let rhs_minus_one = b
            .build_int_compare(IntPredicate::EQ, rhs, minus_one, "rem.rhs_minus_one")
            .map_err(|e| e.to_string())?;
        let overflow = b
            .build_and(lhs_min, rhs_minus_one, "rem.overflow")
            .map_err(|e| e.to_string())?;
        let invalid = b
            .build_or(rem_by_zero, overflow, "rem.invalid")
            .map_err(|e| e.to_string())?;
        self.trap_if(invalid, "rem")?;

        let r = b
            .build_int_signed_rem(lhs, rhs, "srem")
            .map_err(|e| e.to_string())?;
        Ok(r.into())
    }

    fn lower_checked_int(
        &mut self,
        op: BinOp,
        lhs: IntValue<'ctx>,
        rhs: IntValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let boolt = self.ctx.bool_type();
        let intrinsic = match op {
            BinOp::AddChecked => "llvm.sadd.with.overflow.i64",
            BinOp::SubChecked => "llvm.ssub.with.overflow.i64",
            BinOp::MulChecked => "llvm.smul.with.overflow.i64",
            _ => {
                return Err(format!(
                    "codegen: `{}` is not checked arithmetic",
                    op.symbol()
                ))
            }
        };
        let pair_t = self.ctx.struct_type(&[i64t.into(), boolt.into()], false);
        let f = self.module.get_function(intrinsic).unwrap_or_else(|| {
            self.module.add_function(
                intrinsic,
                pair_t.fn_type(&[i64t.into(), i64t.into()], false),
                None,
            )
        });
        let pair = self
            .builder
            .build_call(f, &[lhs.into(), rhs.into()], "checked")
            .map_err(|e| e.to_string())?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{intrinsic} returned no value"))?
            .into_struct_value();
        let value = self
            .builder
            .build_extract_value(pair, 0, "checked.value")
            .map_err(|e| e.to_string())?
            .into_int_value();
        let overflow = self
            .builder
            .build_extract_value(pair, 1, "checked.overflow")
            .map_err(|e| e.to_string())?
            .into_int_value();

        self.trap_if(overflow, "overflow")?;
        Ok(value.into())
    }

    // ── the `mem` capability: raw load/store + bulk fill/copy ────────────────
    // Addresses are the uniform i64 (`inttoptr` to an opaque `ptr`); accesses use
    // the type's natural alignment (callers width-align — `peek16` at even
    // offsets, `peek8` anywhere). Each yields a value (`peek`) or `Unit` (i64 0).

    /// The LLVM integer type for a memory access width.
    fn mem_int_type(&self, w: MemWidth) -> IntType<'ctx> {
        match w {
            MemWidth::W8 => self.ctx.i8_type(),
            MemWidth::W16 => self.ctx.i16_type(),
            MemWidth::W32 => self.ctx.i32_type(),
            MemWidth::W64 => self.ctx.i64_type(),
        }
    }

    /// `peekW addr` → load `W` bits, **zero-extended** to the uniform i64.
    fn lower_peek(&mut self, w: MemWidth, addr: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let a = self.lower_atom(addr)?.into_int_value();
        let ptr = self
            .builder
            .build_int_to_ptr(a, ptrt, "peek.ptr")
            .map_err(|e| e.to_string())?;
        let it = self.mem_int_type(w);
        let loaded = self
            .builder
            .build_load(it, ptr, "peek.val")
            .map_err(|e| e.to_string())?
            .into_int_value();
        let out = if matches!(w, MemWidth::W64) {
            loaded
        } else {
            self.builder
                .build_int_z_extend(loaded, i64t, "peek.zext")
                .map_err(|e| e.to_string())?
        };
        Ok(out.into())
    }

    /// `pokeW addr val` → store the low `W` bits of `val`; yields `Unit`.
    fn lower_poke(
        &mut self,
        w: MemWidth,
        addr: &Atom,
        val: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let a = self.lower_atom(addr)?.into_int_value();
        let v = self.lower_atom(val)?.into_int_value();
        let ptr = self
            .builder
            .build_int_to_ptr(a, ptrt, "poke.ptr")
            .map_err(|e| e.to_string())?;
        let it = self.mem_int_type(w);
        let narrowed = if matches!(w, MemWidth::W64) {
            v
        } else {
            self.builder
                .build_int_truncate(v, it, "poke.val")
                .map_err(|e| e.to_string())?
        };
        self.builder
            .build_store(ptr, narrowed)
            .map_err(|e| e.to_string())?;
        Ok(self.ctx.i64_type().const_zero().into())
    }

    /// `fill dst byte count` → memset `count` bytes to the low byte of `byte`.
    fn lower_fill(
        &mut self,
        dst: &Atom,
        byte: &Atom,
        count: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i8t = self.ctx.i8_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let d = self.lower_atom(dst)?.into_int_value();
        let b = self.lower_atom(byte)?.into_int_value();
        let n = self.lower_atom(count)?.into_int_value();
        let dptr = self
            .builder
            .build_int_to_ptr(d, ptrt, "fill.ptr")
            .map_err(|e| e.to_string())?;
        let bv = self
            .builder
            .build_int_truncate(b, i8t, "fill.byte")
            .map_err(|e| e.to_string())?;
        self.builder
            .build_memset(dptr, 1, bv, n)
            .map_err(|e| e.to_string())?;
        Ok(self.ctx.i64_type().const_zero().into())
    }

    /// `copy dst src count` → memmove `count` bytes (overlap-safe).
    fn lower_copy(
        &mut self,
        dst: &Atom,
        src: &Atom,
        count: &Atom,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let d = self.lower_atom(dst)?.into_int_value();
        let s = self.lower_atom(src)?.into_int_value();
        let n = self.lower_atom(count)?.into_int_value();
        let dptr = self
            .builder
            .build_int_to_ptr(d, ptrt, "copy.dst")
            .map_err(|e| e.to_string())?;
        let sptr = self
            .builder
            .build_int_to_ptr(s, ptrt, "copy.src")
            .map_err(|e| e.to_string())?;
        self.builder
            .build_memmove(dptr, 1, sptr, 1, n)
            .map_err(|e| e.to_string())?;
        Ok(self.ctx.i64_type().const_zero().into())
    }

    /// Declare (once) and call a managed-heap runtime shim. All arguments and
    /// the result are `i64` (handles and scalars share the uniform model).
    /// Returns the result for the `returns` shims, `None` for the `void` ones.
    fn gc_call(
        &mut self,
        name: &str,
        args: &[IntValue<'ctx>],
        returns: bool,
    ) -> Result<Option<IntValue<'ctx>>, String> {
        let i64t = self.ctx.i64_type();
        let metas: Vec<BasicMetadataTypeEnum> = args.iter().map(|_| i64t.into()).collect();
        let fnty = if returns {
            i64t.fn_type(&metas, false)
        } else {
            self.ctx.void_type().fn_type(&metas, false)
        };
        let f = self
            .module
            .get_function(name)
            .unwrap_or_else(|| self.module.add_function(name, fnty, None));
        let argvals: Vec<BasicMetadataValueEnum> = args.iter().map(|a| (*a).into()).collect();
        let call = self
            .builder
            .build_call(f, &argvals, name)
            .map_err(|e| e.to_string())?;
        if returns {
            Ok(Some(
                call.try_as_basic_value()
                    .basic()
                    .ok_or_else(|| format!("{name} returned no value"))?
                    .into_int_value(),
            ))
        } else {
            Ok(None)
        }
    }

    /// Declare and call a managed-heap runtime shim returning a raw pointer.
    fn gc_ptr_call(
        &mut self,
        name: &str,
        args: &[IntValue<'ctx>],
    ) -> Result<PointerValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let ptrt = self.ctx.ptr_type(AddressSpace::default());
        let metas: Vec<BasicMetadataTypeEnum> = args.iter().map(|_| i64t.into()).collect();
        let fnty = ptrt.fn_type(&metas, false);
        let f = self
            .module
            .get_function(name)
            .unwrap_or_else(|| self.module.add_function(name, fnty, None));
        let argvals: Vec<BasicMetadataValueEnum> = args.iter().map(|a| (*a).into()).collect();
        let call = self
            .builder
            .build_call(f, &argvals, name)
            .map_err(|e| e.to_string())?;
        Ok(call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("{name} returned no value"))?
            .into_pointer_value())
    }

    /// Open a handle scope at a function's entry — every object the function
    /// allocates is rooted here until it `leave`s. Returns the frame marker.
    fn gc_enter(&mut self) -> Result<IntValue<'ctx>, String> {
        Ok(self
            .gc_call("locus_gc_enter", &[], true)?
            .expect("enter returns a frame"))
    }

    /// Close a scope at a function's return, handing `result` back to the caller.
    /// `result` is **self-describing**: the runtime escapes it if it carries the
    /// handle magic, or passes it through unchanged if it's a scalar — so this is
    /// emitted uniformly without the front end tracking which functions return a
    /// reference.
    fn gc_leave_with(
        &mut self,
        frame: IntValue<'ctx>,
        result: IntValue<'ctx>,
    ) -> Result<IntValue<'ctx>, String> {
        Ok(self
            .gc_call("locus_gc_leave_with", &[frame, result], true)?
            .expect("leave_with returns a value"))
    }

    fn gc_leave(&mut self, frame: IntValue<'ctx>) -> Result<(), String> {
        self.gc_call("locus_gc_leave", &[frame], false).map(|_| ())
    }

    fn gc_root(&mut self, value: IntValue<'ctx>) -> Result<IntValue<'ctx>, String> {
        Ok(self
            .gc_call("locus_gc_root", &[value], true)?
            .expect("root returns a handle"))
    }

    fn gc_root_set(
        &mut self,
        root: IntValue<'ctx>,
        value: IntValue<'ctx>,
    ) -> Result<IntValue<'ctx>, String> {
        Ok(self
            .gc_call("locus_gc_root_set", &[root, value], true)?
            .expect("root_set returns a handle"))
    }

    fn return_with_frame(
        &mut self,
        frame: Option<IntValue<'ctx>>,
        value: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match (frame, value) {
            (Some(frame), BasicValueEnum::IntValue(value)) => {
                Ok(self.gc_leave_with(frame, value)?.into())
            }
            (Some(frame), value) => {
                self.gc_leave(frame)?;
                Ok(value)
            }
            (None, value) => Ok(value),
        }
    }

    /// `(a1, …, an)` → a **managed** heap object (performs the `gc` effect). The
    /// pointer (GC-traced) fields are laid out first, then the opaque scalars;
    /// the value is the object's **handle** (a stable table index as i64), not a
    /// raw address — so the collector may relocate it freely. Field atoms are
    /// ANF-trivial (no allocation), so nothing can collect between the alloc and
    /// the field stores; the object is fully built before any later allocation.
    fn lower_tuple(
        &mut self,
        fields: &[(Atom, ValueLayout)],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let (mut n_ptr, mut n_scalar) = (0u64, 0u64);

        // Evaluate every field value first (trivial — no allocation). A pointer
        // field is one traced cell; a scalar field spans `cells` (>= 1)
        // contiguous untraced cells, the multi-cell case a vector field needs
        // (`Quad[Float32]` = 2 cells). A **word cell** (a `Type::Var` field, D4)
        // is a single *pointer*-region cell stored verbatim (`set_word`), so we
        // carry the word flag per field. The scalar field value is kept as a raw
        // `BasicValueEnum` (a vector is not an `i64`) and decomposed below.
        let mut vals: Vec<(BasicValueEnum<'ctx>, FieldRegion)> = Vec::with_capacity(fields.len());
        for (a, layout) in fields {
            let region = field_region(*layout, "managed object field")?;
            match region {
                FieldRegion::Pointer { .. } => n_ptr += 1,
                FieldRegion::Scalar { cells } => n_scalar += cells,
            }
            let value = self.lower_atom(a)?;
            vals.push((value, region));
        }

        let obj = self
            .gc_call(
                "locus_gc_alloc",
                &[
                    i64t.const_int(n_ptr, false),
                    i64t.const_int(n_scalar, false),
                ],
                true,
            )?
            .expect("alloc returns a handle");

        // The pointer and scalar regions are numbered independently; `si`
        // accumulates the **real per-field cell counts**, so a field after a
        // multi-cell vector lands at the right base scalar cell.
        let (mut pi, mut si) = (0u64, 0u64);
        for (v, region) in vals {
            match region {
                FieldRegion::Pointer { word } => {
                    // A word cell is in the pointer region but stored verbatim; a
                    // real pointer cell resolves its handle through `set_ptr`.
                    let v = Self::expect_cell(v, "managed object field")?;
                    let shim = if word {
                        "locus_gc_set_word"
                    } else {
                        "locus_gc_set_ptr"
                    };
                    self.gc_call(shim, &[obj, i64t.const_int(pi, false), v], false)?;
                    pi += 1;
                }
                FieldRegion::Scalar { cells } => {
                    // Decompose into `cells` words and store each into the field's
                    // contiguous `si..si+cells` scalar cells (opaque `set_scalar`
                    // calls, so `-O2` cannot drop them).
                    let words = self.scalar_value_to_cells(v, cells, "managed object field")?;
                    for (k, word) in words.into_iter().enumerate() {
                        self.gc_call(
                            "locus_gc_set_scalar",
                            &[obj, i64t.const_int(si + k as u64, false), word],
                            false,
                        )?;
                    }
                    si += cells;
                }
            }
        }
        Ok(obj.into())
    }

    /// `t.i` → read field `slot` of object handle `t`. A pointer field comes back
    /// as a fresh handle; a single scalar field as its value; a **multi-cell**
    /// scalar field (a vector) is read as `cells` contiguous `get_scalar`s and
    /// reassembled into its LLVM vector (`ty` picks the vector type).
    fn lower_proj(
        &mut self,
        tup: &Atom,
        slot: usize,
        layout: ValueLayout,
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let tv = Self::expect_cell(self.lower_atom(tup)?, "managed object handle")?;
        match field_region(layout, "projected field")? {
            // A **word cell** (a `Type::Var` field) is read verbatim with
            // `get_word`: it sits in the pointer region but holds a raw repr-poly
            // word, so interning it as a handle (`get_ptr`) would mis-handle a
            // tagged scalar. The reader (passthrough store, or an `Untag` on the
            // load side) interprets the word. A real pointer cell interns a handle.
            FieldRegion::Pointer { word } => {
                let shim = if word {
                    "locus_gc_get_word"
                } else {
                    "locus_gc_get_ptr"
                };
                let r = self
                    .gc_call(shim, &[tv, i64t.const_int(slot as u64, false)], true)?
                    .expect("get returns a value");
                Ok(r.into())
            }
            FieldRegion::Scalar { cells } => {
                // Read the field's contiguous `slot..slot+cells` scalar cells
                // (opaque `get_scalar`s, so `-O2` keeps them) and reassemble the
                // value — a single cell passes through, a vector is rebuilt.
                let mut words = Vec::with_capacity(cells as usize);
                for k in 0..cells {
                    let word = self
                        .gc_call(
                            "locus_gc_get_scalar",
                            &[tv, i64t.const_int(slot as u64 + k, false)],
                            true,
                        )?
                        .expect("get returns a value");
                    words.push(word);
                }
                self.scalar_cells_to_value(ty, &words, "projected field")
            }
        }
    }

    /// `[a1, ..., an]` -> a managed array. Pointer arrays use traced pointer
    /// slots plus scalar slot 0 for length. Scalar arrays use scalar slot 0 for
    /// length and a contiguous untraced byte payload starting at scalar slot 1.
    fn lower_array_lit(
        &mut self,
        elems: &[Atom],
        elem_layout: ValueLayout,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let len = elems.len();
        match array_elem_layout(elem_layout, len, "array element")? {
            ArrayElemLayout::Pointer => {
                let vals: Vec<_> = elems
                    .iter()
                    .map(|a| {
                        let value = self.lower_atom(a)?;
                        Self::expect_cell(value, "array element")
                    })
                    .collect::<Result<_, _>>()?;
                let arr = self
                    .gc_call(
                        "locus_gc_alloc",
                        &[i64t.const_int(len as u64, false), i64t.const_int(1, false)],
                        true,
                    )?
                    .expect("array alloc returns a handle");
                self.gc_call(
                    "locus_gc_set_scalar",
                    &[arr, i64t.const_zero(), i64t.const_int(len as u64, false)],
                    false,
                )?;
                for (i, v) in vals.into_iter().enumerate() {
                    self.gc_call(
                        "locus_gc_set_ptr",
                        &[arr, i64t.const_int(i as u64, false), v],
                        false,
                    )?;
                }
                Ok(arr.into())
            }
            ArrayElemLayout::Scalar {
                stride,
                data_cells,
                elem_cells,
            } => {
                // Evaluate elements first (ANF-trivial). A sub-word/whole-cell
                // element is one `i64`; a multi-cell vector element is decomposed
                // into `elem_cells` words below.
                let vals: Vec<_> = elems
                    .iter()
                    .map(|a| self.lower_atom(a))
                    .collect::<Result<_, _>>()?;
                let arr = self
                    .gc_call(
                        "locus_gc_alloc",
                        &[i64t.const_zero(), i64t.const_int(1 + data_cells, false)],
                        true,
                    )?
                    .expect("array alloc returns a handle");
                self.gc_call(
                    "locus_gc_set_scalar",
                    &[arr, i64t.const_zero(), i64t.const_int(len as u64, false)],
                    false,
                )?;
                for (i, v) in vals.into_iter().enumerate() {
                    self.array_store_scalar_elem(
                        arr,
                        i64t.const_int(i as u64, false),
                        v,
                        stride,
                        elem_cells,
                    )?;
                }
                Ok(arr.into())
            }
        }
    }

    /// Store one scalar-payload array element `value` at element index `iv`. A
    /// single-cell element (sub-word packed, or one whole cell) goes through the
    /// byte-strided shim; a **multi-cell** element (a vector) is decomposed into
    /// `elem_cells` words, each stored into its contiguous cell with the
    /// cell-granular shim. All are opaque runtime calls, so `-O2` keeps them.
    fn array_store_scalar_elem(
        &mut self,
        arr: IntValue<'ctx>,
        iv: IntValue<'ctx>,
        value: BasicValueEnum<'ctx>,
        stride: u64,
        elem_cells: u64,
    ) -> Result<(), String> {
        let i64t = self.ctx.i64_type();
        if elem_cells <= 1 {
            let v = Self::expect_cell(value, "array element")?;
            self.gc_call(
                "locus_gc_array_set_scalar_bytes",
                &[arr, iv, i64t.const_int(stride, false), v],
                false,
            )?;
        } else {
            let words = self.scalar_value_to_cells(value, elem_cells, "array element")?;
            for (w, word) in words.into_iter().enumerate() {
                self.gc_call(
                    "locus_gc_array_set_scalar_cell",
                    &[
                        arr,
                        iv,
                        i64t.const_int(elem_cells, false),
                        i64t.const_int(w as u64, false),
                        word,
                    ],
                    false,
                )?;
            }
        }
        Ok(())
    }

    /// `len a` -> the array's logical element count.
    fn lower_len(&mut self, arr: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        if let Atom::Var(name) = arr {
            if let Some(cached) = self.raw_scalar_arrays.get(name).copied() {
                return Ok(cached.len.into());
            }
        }
        let av = Self::expect_cell(self.lower_atom(arr)?, "array handle")?;
        let r = self
            .gc_call("locus_gc_len", &[av], true)?
            .expect("len returns a value");
        Ok(r.into())
    }

    /// `a[i]` on an array → a bounds-checked element read (a pointer element
    /// comes back as a fresh handle, a single scalar as its value, a multi-cell
    /// vector reassembled from its contiguous cells via `elem_ty`).
    fn lower_array_get(
        &mut self,
        arr: &Atom,
        idx: &Atom,
        elem_layout: ValueLayout,
        elem_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let av = Self::expect_cell(self.lower_atom(arr)?, "array handle")?;
        let iv = Self::expect_cell(self.lower_atom(idx)?, "array index")?;
        match array_elem_layout(elem_layout, 0, "array element")? {
            ArrayElemLayout::Pointer => {
                let r = self
                    .gc_call("locus_gc_array_get_ptr", &[av, iv], true)?
                    .expect("array get returns a value");
                Ok(r.into())
            }
            ArrayElemLayout::Scalar {
                stride, elem_cells, ..
            } => {
                // Multi-cell (vector) elements aren't on the cached raw fast path
                // yet (`collect_raw_array_uses` gates it on byte_width ∈
                // {1,2,4,8}); read each contiguous cell and reassemble.
                if elem_cells > 1 {
                    let mut words = Vec::with_capacity(elem_cells as usize);
                    for w in 0..elem_cells {
                        let word = self
                            .gc_call(
                                "locus_gc_array_get_scalar_cell",
                                &[
                                    av,
                                    iv,
                                    i64t.const_int(elem_cells, false),
                                    i64t.const_int(w, false),
                                ],
                                true,
                            )?
                            .expect("array get returns a value");
                        words.push(word);
                    }
                    return self.scalar_cells_to_value(elem_ty, &words, "array element");
                }
                if let Atom::Var(name) = arr {
                    if let Some(cached) = self.raw_scalar_arrays.get(name).copied() {
                        let bounds_checked_by_loop = if let Atom::Var(idx_name) = idx {
                            self.loop_array_bounds
                                .contains(&(name.clone(), idx_name.clone()))
                        } else {
                            false
                        };
                        return self.lower_cached_scalar_array_get(
                            cached,
                            idx,
                            stride,
                            bounds_checked_by_loop,
                        );
                    }
                }
                let r = self
                    .gc_call(
                        "locus_gc_array_get_scalar_bytes",
                        &[av, iv, i64t.const_int(stride, false)],
                        true,
                    )?
                    .expect("array get returns a value");
                Ok(r.into())
            }
        }
    }

    /// `a[i] <- v` on an array → a bounds-checked element write; yields `Unit` (0).
    fn lower_array_set(
        &mut self,
        arr: &Atom,
        idx: &Atom,
        val: &Atom,
        elem_layout: ValueLayout,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        match array_elem_layout(elem_layout, 0, "array element")? {
            ArrayElemLayout::Pointer => {
                let av = Self::expect_cell(self.lower_atom(arr)?, "array handle")?;
                let iv = Self::expect_cell(self.lower_atom(idx)?, "array index")?;
                let vv = Self::expect_cell(self.lower_atom(val)?, "array element")?;
                self.gc_call("locus_gc_array_set_ptr", &[av, iv, vv], false)?;
            }
            ArrayElemLayout::Scalar {
                stride, elem_cells, ..
            } => {
                // Multi-cell (vector) elements bypass the cached raw fast path
                // (Sprint 3); decompose and store each contiguous cell.
                if elem_cells > 1 {
                    let av = Self::expect_cell(self.lower_atom(arr)?, "array handle")?;
                    let iv = Self::expect_cell(self.lower_atom(idx)?, "array index")?;
                    let vv = self.lower_atom(val)?;
                    self.array_store_scalar_elem(av, iv, vv, stride, elem_cells)?;
                    return Ok(i64t.const_zero().into());
                }
                if let Atom::Var(name) = arr {
                    if let Some(cached) = self.raw_scalar_arrays.get(name).copied() {
                        let bounds_checked_by_loop = if let Atom::Var(idx_name) = idx {
                            self.loop_array_bounds
                                .contains(&(name.clone(), idx_name.clone()))
                        } else {
                            false
                        };
                        self.lower_cached_scalar_array_set(
                            cached,
                            idx,
                            val,
                            stride,
                            bounds_checked_by_loop,
                        )?;
                        return Ok(i64t.const_zero().into());
                    }
                }
                let av = Self::expect_cell(self.lower_atom(arr)?, "array handle")?;
                let iv = Self::expect_cell(self.lower_atom(idx)?, "array index")?;
                let vv = Self::expect_cell(self.lower_atom(val)?, "array element")?;
                self.gc_call(
                    "locus_gc_array_set_scalar_bytes",
                    &[av, iv, i64t.const_int(stride, false), vv],
                    false,
                )?;
            }
        }
        Ok(self.ctx.i64_type().const_zero().into())
    }

    fn lower_atom(&mut self, atom: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        Ok(match atom {
            Atom::Int(n) => i64t.const_int(*n as u64, true).into(),
            Atom::Float(bits) => i64t.const_int(*bits, false).into(),
            Atom::Bool(b) => i64t.const_int(u64::from(*b), false).into(),
            Atom::Unit => i64t.const_int(0, false).into(),
            // Locus strings are WIDE (UTF-16) internally — Windows-native,
            // `LPCWSTR`-shaped. A literal becomes a null-terminated UTF-16 global;
            // its pointer rides the uniform i64 model through closures. The
            // boundaries that dereference it (`perform console`, foreign `…W`
            // calls) `inttoptr` it back.
            Atom::Str(s) => {
                let units: Vec<_> = s.encode_utf16().map(|u| Atom::Int(i64::from(u))).collect();
                self.lower_array_lit(&units, ValueLayout::scalar_bytes(2, 2))?
            }
            Atom::Var(x) => {
                self.env
                    .get(x)
                    .ok_or_else(|| format!("codegen: unbound variable `{x}`"))?
                    .value
            }
        })
    }

    /// Build an `alloca` in the **function entry block** (rather than at the
    /// current insertion point), so a `let mut` reached inside a loop body still
    /// gets one stable stack slot instead of one per iteration, and `mem2reg`
    /// can promote it. Leaves the builder at its original position.
    fn entry_alloca(&self, name: &str) -> Result<PointerValue<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let cur = self
            .builder
            .get_insert_block()
            .ok_or("codegen: no current block for a mutable-local slot")?;
        let entry = cur
            .get_parent()
            .ok_or("codegen: no enclosing function for a mutable-local slot")?
            .get_first_basic_block()
            .ok_or("codegen: enclosing function has no entry block")?;
        // Insert the alloca before the entry block's terminator if it has one,
        // otherwise at its end; then restore the builder to where we were.
        match entry.get_first_instruction() {
            Some(first) => self.builder.position_before(&first),
            None => self.builder.position_at_end(entry),
        }
        let slot = self
            .builder
            .build_alloca(i64t, &format!("mut.{name}"))
            .map_err(|e| e.to_string())?;
        self.builder.position_at_end(cur);
        Ok(slot)
    }

    /// `let mut name = init` — allocate a scalar stack slot and store the initial
    /// value. Every supported scalar (`Int`/`Float`/`Bool`) rides the uniform
    /// `i64` word model, so one `i64` cell holds any of them. Yields `Unit`.
    fn lower_slot_init(&mut self, name: &str, init: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let v = Self::expect_cell(self.lower_atom(init)?, "mutable-local initializer")?;
        let slot = self.entry_alloca(name)?;
        self.builder
            .build_store(slot, v)
            .map_err(|e| e.to_string())?;
        self.mut_slots.insert(name.to_string(), slot);
        Ok(self.ctx.i64_type().const_zero().into())
    }

    /// A read of a mutable local — `load` its current value from the slot.
    fn lower_slot_load(&mut self, name: &str) -> Result<BasicValueEnum<'ctx>, String> {
        let slot = *self
            .mut_slots
            .get(name)
            .ok_or_else(|| format!("codegen: read of mutable local `{name}` with no live slot"))?;
        self.builder
            .build_load(self.ctx.i64_type(), slot, &format!("{name}.load"))
            .map_err(|e| e.to_string())
    }

    /// `name := val` — store the new value into the mutable local's slot. Yields
    /// `Unit`.
    fn lower_slot_store(&mut self, name: &str, val: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let v = Self::expect_cell(self.lower_atom(val)?, "mutable-local assignment")?;
        let slot = *self.mut_slots.get(name).ok_or_else(|| {
            format!("codegen: assignment to mutable local `{name}` with no live slot")
        })?;
        self.builder
            .build_store(slot, v)
            .map_err(|e| e.to_string())?;
        Ok(self.ctx.i64_type().const_zero().into())
    }

    /// `ref e` — allocate a one-field mutable heap cell (`Ref[T]`,
    /// `docs/mutability.md` §1.1) holding `e`, returning its **handle**. A `Ref` is
    /// a one-field heap object, so this is the single-field [`lower_tuple`] path
    /// specialised: `locus_gc_alloc` the content region, then a `set_scalar` of the
    /// init word into slot 0. The content cell is a **scalar** (sema's v1 gate
    /// rejects a pointer-typed `Ref`), so a multi-cell scalar value (a vector) is
    /// decomposed by `scalar_value_to_cells` exactly as a vector tuple field is.
    fn lower_ref_new(
        &mut self,
        init: &Atom,
        layout: ValueLayout,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let value = self.lower_atom(init)?;
        // Scalar content cell only this sprint (the pointer-cell case + write
        // barrier is Sprint 3). `field_region` would also classify a pointer cell,
        // but sema guarantees scalar here — assert it loudly rather than silently
        // mis-lower if that ever changes.
        let cells =
            match field_region(layout, "ref cell")? {
                FieldRegion::Scalar { cells } => cells,
                FieldRegion::Pointer { .. } => return Err(
                    "codegen: a pointer-typed `Ref` cell needs the GC write barrier (Sprint 3); \
                     sema should have rejected it (RN-E0247)"
                        .into(),
                ),
            };
        let obj = self
            .gc_call(
                "locus_gc_alloc",
                &[i64t.const_zero(), i64t.const_int(cells, false)],
                true,
            )?
            .expect("ref alloc returns a handle");
        let words = self.scalar_value_to_cells(value, cells, "ref cell")?;
        for (k, word) in words.into_iter().enumerate() {
            self.gc_call(
                "locus_gc_set_scalar",
                &[obj, i64t.const_int(k as u64, false), word],
                false,
            )?;
        }
        Ok(obj.into())
    }

    /// `!r` — read the heap cell `r : Ref[T]`. Resolve the handle, then read its
    /// `cells` contiguous scalar cells (`get_scalar`) from slot 0 and reassemble
    /// the value — exactly the scalar-field side of [`lower_proj`] at slot 0.
    fn lower_ref_get(
        &mut self,
        r: &Atom,
        layout: ValueLayout,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let rv = Self::expect_cell(self.lower_atom(r)?, "ref handle")?;
        let cells = match field_region(layout, "ref cell")? {
            FieldRegion::Scalar { cells } => cells,
            FieldRegion::Pointer { .. } => {
                return Err("codegen: pointer-typed `Ref` read is Sprint 3 (RN-E0247)".into())
            }
        };
        // The opaque `get_scalar` calls keep `-O2` from folding the round-trip away.
        let mut words = Vec::with_capacity(cells as usize);
        for k in 0..cells {
            let word = self
                .gc_call("locus_gc_get_scalar", &[rv, i64t.const_int(k, false)], true)?
                .expect("get returns a value");
            words.push(word);
        }
        // The content cell holds the scalar verbatim (a `Float`'s bits ride an
        // `i64` cell, bitcast elsewhere). `scalar_cells_to_value` passes a single
        // cell through and rebuilds a multi-cell vector if it ever were one.
        self.scalar_cells_to_value(&Type::Int, &words, "ref cell")
    }

    /// `r := v` — write `v` into the heap cell `r : Ref[T]`, in place. Resolve the
    /// handle, decompose `v` into its scalar cells, and `set_scalar` them from slot
    /// 0 — the scalar-field side of [`lower_tuple`]'s store at slot 0. Yields
    /// `Unit`. **No write barrier:** a scalar content cell never holds a pointer,
    /// so this write can never create an old→young pointer (the barrier for a
    /// pointer-typed `Ref` is Sprint 3, attached exactly here at the `set_ptr` path).
    fn lower_ref_set(
        &mut self,
        r: &Atom,
        val: &Atom,
        layout: ValueLayout,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64t = self.ctx.i64_type();
        let rv = Self::expect_cell(self.lower_atom(r)?, "ref handle")?;
        let value = self.lower_atom(val)?;
        let cells = match field_region(layout, "ref cell")? {
            FieldRegion::Scalar { cells } => cells,
            FieldRegion::Pointer { .. } => {
                return Err(
                    "codegen: pointer-typed `Ref` write needs the barrier (Sprint 3)".into(),
                )
            }
        };
        let words = self.scalar_value_to_cells(value, cells, "ref cell")?;
        for (k, word) in words.into_iter().enumerate() {
            self.gc_call(
                "locus_gc_set_scalar",
                &[rv, i64t.const_int(k as u64, false), word],
                false,
            )?;
        }
        Ok(i64t.const_zero().into())
    }

    /// `perform op arg` → route to the innermost in-scope handler clause.
    ///
    /// **Tail-resumptive**: the continuation is implicit, so inline the clause —
    /// `resume` is the identity continuation (`resume V` = V), its body's value
    /// IS the perform's value, and lowering continues. **Abort**: the clause
    /// never resumes, so run it, store its value in the handle's result slot,
    /// branch to the handle exit (skipping the rest of the scrutinee and the
    /// `return` clause), and continue in a fresh — now dead — block.
    fn lower_perform(&mut self, label: &Label, arg: &Atom) -> Result<BasicValueEnum<'ctx>, String> {
        let found = self.handlers.iter().rev().find_map(|frame| {
            frame
                .clauses
                .iter()
                .find(|c| &c.op == label)
                .map(|c| (c.clone(), frame.exit))
        });
        let Some((c, exit)) = found else {
            if matches!(label, Label::World(name) if name == "console_float") {
                let bits = self.lower_atom(arg)?.into_int_value();
                self.gc_call("locus_write_float", &[bits], false)?;
                return Ok(self.ctx.i64_type().const_zero().into());
            }
            return Err(format!(
                "codegen: unhandled effect `{label}` — no handler in scope, and there is no \
                 native runtime (output is the `console_writeln` prelude)"
            ));
        };
        let argv = self.lower_atom(arg)?;
        let saved_env = self.env.clone();
        self.env.insert(
            c.arg.clone(),
            EnvVal {
                value: argv,
                ty: c.arg_ty,
                layout: c.arg_layout,
            },
        );
        match clause_shape(&c.resume, &c.body) {
            Shape::Tail => {
                let saved = self.resume_id.replace(c.resume.clone());
                let v = self.lower_block(&c.body);
                self.resume_id = saved;
                self.env = saved_env;
                v
            }
            Shape::Abort => {
                let (exit_bb, slot) = exit.expect("abort clause installed without an exit block");
                let v = self.lower_block(&c.body)?;
                let v = Self::expect_cell(v, "abort handler result")?;
                self.builder
                    .build_store(slot, v)
                    .map_err(|e| e.to_string())?;
                self.builder
                    .build_unconditional_branch(exit_bb)
                    .map_err(|e| e.to_string())?;
                let func = self
                    .builder
                    .get_insert_block()
                    .and_then(|b| b.get_parent())
                    .ok_or("codegen: no enclosing function for an abort")?;
                let dead = self.ctx.append_basic_block(func, "after.abort");
                self.builder.position_at_end(dead);
                self.env = saved_env;
                Ok(self.ctx.i64_type().const_int(0, false).into())
            }
            other => Err(format!(
                "codegen: handler for `{label}` is {other:?} — only tail / abort lower yet"
            )),
        }
    }

    /// `handle <scrutinee> with { op(x) => … ; return(y) => … }`.
    ///
    /// Lowers **tail-resumptive** and **abort** clauses (one-/multi-shot, which
    /// need a reified continuation, are a later slice). A pure tail-resumptive
    /// handle is straight-line — the clauses inline at their perform sites and
    /// the scrutinee's result flows into `return`. A handle with any abort clause
    /// gets a **result slot + exit block**: aborts and the normal `return` store
    /// the result and branch to exit, which loads it.
    fn lower_handle(
        &mut self,
        scrutinee: &Ir,
        handler: &IrHandler,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let mut has_abort = false;
        for op in &handler.ops {
            match clause_shape(&op.resume, &op.body) {
                Shape::Tail => {}
                Shape::Abort => has_abort = true,
                // One-shot / multi-shot: `resume` is a *reified* continuation,
                // callable any number of times. A separate lowering path.
                Shape::OneShot | Shape::Multi => {
                    return self.lower_handle_reified(scrutinee, handler);
                }
            }
        }

        if !has_abort {
            self.handlers.push(Frame {
                clauses: handler.ops.clone(),
                exit: None,
            });
            let result = self.lower_block(scrutinee);
            self.handlers.pop();
            let result = result?;
            self.env.insert(
                handler.ret.var.clone(),
                EnvVal {
                    value: result,
                    ty: handler.ret.var_ty.clone(),
                    layout: handler.ret.var_layout,
                },
            );
            return self.lower_block(&handler.ret.body);
        }

        let func = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .ok_or("codegen: no enclosing function for a handler")?;
        let i64t = self.ctx.i64_type();
        let slot = self
            .builder
            .build_alloca(i64t, "handle.slot")
            .map_err(|e| e.to_string())?;
        let exit_bb = self.ctx.append_basic_block(func, "handle.exit");

        self.handlers.push(Frame {
            clauses: handler.ops.clone(),
            exit: Some((exit_bb, slot)),
        });
        let result = self.lower_block(scrutinee);
        self.handlers.pop();
        let result = result?;

        // Normal completion → the return clause → store + branch to exit.
        self.env.insert(
            handler.ret.var.clone(),
            EnvVal {
                value: result,
                ty: handler.ret.var_ty.clone(),
                layout: handler.ret.var_layout,
            },
        );
        let ret_val = Self::expect_cell(self.lower_block(&handler.ret.body)?, "handler result")?;
        self.builder
            .build_store(slot, ret_val)
            .map_err(|e| e.to_string())?;
        self.builder
            .build_unconditional_branch(exit_bb)
            .map_err(|e| e.to_string())?;

        self.builder.position_at_end(exit_bb);
        self.builder
            .build_load(i64t, slot, "handle.result")
            .map_err(|e| e.to_string())
    }

    /// A handler with a **reified** clause (one-shot or multi-shot) — `resume` is
    /// a real captured continuation, callable zero, one, or many times.
    ///
    /// **Selective CPS:** [`cps_transform`] rewrites the scrutinee into ordinary
    /// IR (lets + lambdas + calls), capturing each syntactic `perform` of this
    /// handler's ops as a **continuation lambda** — so `let x = perform op a in
    /// rest` threads `λx. <rest>` as `resume` into the clause. Each continuation
    /// becomes a closure, so the existing codegen handles it: a one-shot resume
    /// runs it once, a multi-shot resume runs it N times. `resume_id` is cleared
    /// so every `resume v` is a genuine call, not the tail identity shortcut.
    fn lower_handle_reified(
        &mut self,
        scrutinee: &Ir,
        handler: &IrHandler,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let transformed = cps_transform(scrutinee, handler)?;
        let saved = self.resume_id.take();
        let result = self.lower_block(&transformed);
        self.resume_id = saved;
        result
    }
}

/// Rewrite a reified handler's scrutinee `block` into ordinary IR, capturing each
/// **syntactic** `perform` of the handler's ops as an explicit continuation.
///
/// - `let x = perform op a in rest`  ⇒  `let <arg> = a in let resume = (λx. T[rest]) in <clause>`
/// - `perform op a` in tail position ⇒  same, but the continuation is the **return
///    clause** `λ ret.var. ret.body` (nothing follows the perform).
/// - a pure tail `v`                 ⇒  `let ret.var = v in <return body>` (apply return).
/// - a pure binding                  ⇒  kept; transform the rest.
///
/// Each continuation is a `λ` → a closure, so calling it twice (multi-shot) just
/// runs it twice. Branches, nested handlers, and performs of *other* effects in
/// the scrutinee are a later slice (the continuation would have to split across
/// them) — they raise a clear error.
fn cps_transform(block: &Ir, handler: &IrHandler) -> Result<Ir, String> {
    match block {
        Ir::Block { binds, row, comp } => {
            let nested = let_chain_from_parts(binds, row, comp);
            cps_transform(&nested, handler)
        }
        Ir::Ret { comp, row } => {
            if let Comp::Perform(op, arg) = comp {
                if handler_handles(handler, op) {
                    // nothing follows: the continuation is the return clause.
                    let cont = Comp::Lam {
                        param: handler.ret.var.clone(),
                        param_ty: Some(handler.ret.var_ty.clone()),
                        param_layout: handler.ret.var_layout,
                        ret_ty: handler.ret.body_ty.clone(),
                        body: handler.ret.body.clone(),
                    };
                    return Ok(perform_cps(
                        clause_of(handler, op),
                        arg.clone(),
                        cont,
                        row.clone(),
                    ));
                }
            }
            cps_guard(comp, handler)?;
            // a pure final value (or an outer-effect perform): apply the return
            // clause — `let ret.var = comp in ret.body`.
            Ok(Ir::Let {
                name: handler.ret.var.clone(),
                ty: handler.ret.var_ty.clone(),
                layout: handler.ret.var_layout,
                row: row.clone(),
                comp: comp.clone(),
                rest: handler.ret.body.clone(),
            })
        }
        Ir::Let {
            name,
            ty,
            layout,
            row,
            comp,
            rest,
        } => {
            if let Comp::Perform(op, arg) = comp {
                if handler_handles(handler, op) {
                    // the continuation is `λ name. <transformed rest>`.
                    let cont = Comp::Lam {
                        param: name.clone(),
                        param_ty: Some(ty.clone()),
                        param_layout: *layout,
                        ret_ty: handler.ret.body_ty.clone(),
                        body: Box::new(cps_transform(rest, handler)?),
                    };
                    return Ok(perform_cps(
                        clause_of(handler, op),
                        arg.clone(),
                        cont,
                        row.clone(),
                    ));
                }
            }
            cps_guard(comp, handler)?;
            // a pure binding (or an outer-effect perform, which routes to an outer
            // handler): keep it, transform the rest. An outer effect runs once,
            // *before* this handler's continuation — exactly the deep-handler order.
            Ok(Ir::Let {
                name: name.clone(),
                ty: ty.clone(),
                layout: *layout,
                row: row.clone(),
                comp: comp.clone(),
                rest: Box::new(cps_transform(rest, handler)?),
            })
        }
    }
}

/// Convert a flat ANF block back to the legacy let-chain spelling used by
/// the selective-CPS transformer.
fn let_chain_from_parts(binds: &[IrBind], row: &Row, comp: &Comp) -> Ir {
    let mut ir = Ir::Ret {
        row: row.clone(),
        comp: comp.clone(),
    };
    for bind in binds.iter().rev() {
        ir = Ir::Let {
            name: bind.name.clone(),
            ty: bind.ty.clone(),
            layout: bind.layout,
            row: bind.row.clone(),
            comp: bind.comp.clone(),
            rest: Box::new(ir),
        };
    }
    ir
}

/// `let <clause.arg> = arg in let <clause.resume> = cont in <clause.body>` — bind
/// the operation's argument and the continuation, then run the clause.
fn perform_cps(clause: &IrOpClause, arg: Atom, cont: Comp, cont_row: Row) -> Ir {
    Ir::Let {
        name: clause.arg.clone(),
        ty: clause.arg_ty.clone(),
        layout: clause.arg_layout,
        row: Row::pure(),
        comp: Comp::Atom(arg),
        rest: Box::new(Ir::Let {
            name: clause.resume.clone(),
            ty: clause.resume_ty.clone(),
            layout: clause.resume_layout,
            row: cont_row,
            comp: cont,
            rest: clause.body.clone(),
        }),
    }
}

/// Reject a computation that performs one of the handler's ops where this slice
/// can't capture the continuation syntactically — inside a branch, a nested
/// handler, or a closure. (A perform of *another* effect is fine: it routes to an
/// outer handler.) Sound by construction — anything not captured is refused.
fn cps_guard(comp: &Comp, handler: &IrHandler) -> Result<(), String> {
    if comp_performs(comp, handler) {
        return Err(
            "codegen: this one-/multi-shot handler performs its effect inside a branch, a nested \
             handler, or a function — capturing a continuation across those is a later slice"
                .to_string(),
        );
    }
    Ok(())
}

fn handler_handles(handler: &IrHandler, op: &Label) -> bool {
    handler.ops.iter().any(|c| &c.op == op)
}

fn clause_of<'h>(handler: &'h IrHandler, op: &Label) -> &'h IrOpClause {
    handler
        .ops
        .iter()
        .find(|c| &c.op == op)
        .expect("clause exists (checked by handler_handles)")
}

/// Does `comp` syntactically perform one of `handler`'s ops *inside a nested
/// block* (branch / closure / nested handler) — the positions selective CPS does
/// not yet split a continuation across?
fn comp_performs(comp: &Comp, handler: &IrHandler) -> bool {
    match comp {
        Comp::If(_, t, e) => block_performs(t, handler) || block_performs(e, handler),
        Comp::Loop {
            cond,
            steps,
            result,
            ..
        } => {
            block_performs(cond, handler)
                || steps.iter().any(|step| block_performs(step, handler))
                || block_performs(result, handler)
        }
        Comp::Lam { body, .. } | Comp::Quote(body) | Comp::Letloc(body) => {
            block_performs(body, handler)
        }
        Comp::Handle {
            scrutinee,
            handler: h,
            ..
        } => {
            block_performs(scrutinee, handler)
                || h.ops.iter().any(|c| block_performs(&c.body, handler))
                || block_performs(&h.ret.body, handler)
        }
        // A top-level `Perform` is handled by `cps_transform` itself; every other
        // computation is a leaf with no embedded block.
        _ => false,
    }
}

fn block_performs(block: &Ir, handler: &IrHandler) -> bool {
    match block {
        Ir::Block { binds, comp, .. } => {
            binds.iter().any(|bind| {
                matches!(&bind.comp, Comp::Perform(op, _) if handler_handles(handler, op))
                    || comp_performs(&bind.comp, handler)
            }) || matches!(comp, Comp::Perform(op, _) if handler_handles(handler, op))
                || comp_performs(comp, handler)
        }
        Ir::Let { comp, rest, .. } => {
            matches!(comp, Comp::Perform(op, _) if handler_handles(handler, op))
                || comp_performs(comp, handler)
                || block_performs(rest, handler)
        }
        Ir::Ret { comp, .. } => {
            matches!(comp, Comp::Perform(op, _) if handler_handles(handler, op))
                || comp_performs(comp, handler)
        }
    }
}

fn active_handler_free_vars<'ctx>(body: &Ir, handlers: &[Frame<'ctx>]) -> HashSet<String> {
    let mut free = HashSet::new();
    let mut shadowed = HashSet::new();

    // A lambda body that says only `perform op x` can still lower to a handler
    // clause that mentions helpers from the handler's lexical scope. Those
    // helpers must become closure captures before the lifted function is built.
    for frame in handlers.iter().rev() {
        for clause in &frame.clauses {
            if !shadowed.insert(clause.op.clone()) {
                continue;
            }
            if block_performs_label(body, &clause.op) {
                free.extend(op_clause_free_vars(clause));
            }
        }
    }

    free
}

fn op_clause_free_vars(clause: &IrOpClause) -> HashSet<String> {
    let mut bound = vec![clause.arg.clone(), clause.resume.clone()];
    let mut free = HashSet::new();
    fv_ir(&clause.body, &mut bound, &mut free);
    free
}

fn block_performs_label(block: &Ir, label: &Label) -> bool {
    match block {
        Ir::Block { binds, comp, .. } => {
            binds
                .iter()
                .any(|bind| comp_performs_label(&bind.comp, label))
                || comp_performs_label(comp, label)
        }
        Ir::Let { comp, rest, .. } => {
            comp_performs_label(comp, label) || block_performs_label(rest, label)
        }
        Ir::Ret { comp, .. } => comp_performs_label(comp, label),
    }
}

fn comp_performs_label(comp: &Comp, label: &Label) -> bool {
    match comp {
        Comp::Perform(op, _) => op == label,
        Comp::If(_, then, els) => {
            block_performs_label(then, label) || block_performs_label(els, label)
        }
        Comp::Loop {
            cond,
            steps,
            result,
            ..
        } => {
            block_performs_label(cond, label)
                || steps.iter().any(|step| block_performs_label(step, label))
                || block_performs_label(result, label)
        }
        Comp::Lam { body, .. } | Comp::Quote(body) | Comp::Letloc(body) => {
            block_performs_label(body, label)
        }
        Comp::Handle {
            scrutinee, handler, ..
        } => {
            block_performs_label(scrutinee, label)
                || handler
                    .ops
                    .iter()
                    .any(|clause| block_performs_label(&clause.body, label))
                || block_performs_label(&handler.ret.body, label)
        }
        _ => false,
    }
}

// ── free-variable analysis (what a lambda must capture) ──────────────────

/// The free variables of a lambda body, excluding its parameter.
fn free_vars(body: &Ir, param: &str) -> HashSet<String> {
    let mut free = HashSet::new();
    let mut bound = vec![param.to_string()];
    fv_ir(body, &mut bound, &mut free);
    free
}

fn fv_ir(ir: &Ir, bound: &mut Vec<String>, free: &mut HashSet<String>) {
    match ir {
        Ir::Block { binds, comp, .. } => {
            for bind in binds {
                fv_comp(&bind.comp, bound, free);
                bound.push(bind.name.clone());
            }
            fv_comp(comp, bound, free);
            for _ in binds {
                bound.pop();
            }
        }
        Ir::Let {
            name, comp, rest, ..
        } => {
            fv_comp(comp, bound, free);
            bound.push(name.clone());
            fv_ir(rest, bound, free);
            bound.pop();
        }
        Ir::Ret { comp, .. } => fv_comp(comp, bound, free),
    }
}

fn fv_comp(comp: &Comp, bound: &mut Vec<String>, free: &mut HashSet<String>) {
    match comp {
        Comp::Atom(a) => fv_atom(a, bound, free),
        Comp::Extern(_, _) => {}
        Comp::Foreign(_, args, _) => {
            for a in args {
                fv_atom(a, bound, free);
            }
        }
        Comp::App { fun, arg, .. } => {
            fv_atom(fun, bound, free);
            fv_atom(arg, bound, free);
        }
        Comp::Call { fun, args, .. } => {
            fv_atom(fun, bound, free);
            for (a, _) in args {
                fv_atom(a, bound, free);
            }
        }
        Comp::Bin(_, a, b) | Comp::FloatBin(_, a, b) => {
            fv_atom(a, bound, free);
            fv_atom(b, bound, free);
        }
        Comp::VectorLit { elems, .. } => {
            for a in elems {
                fv_atom(a, bound, free);
            }
        }
        Comp::VectorSplat { value, .. } => fv_atom(value, bound, free),
        Comp::VectorBin { lhs, rhs, .. } => {
            fv_atom(lhs, bound, free);
            fv_atom(rhs, bound, free);
        }
        Comp::VectorCompare { lhs, rhs, .. } => {
            fv_atom(lhs, bound, free);
            fv_atom(rhs, bound, free);
        }
        Comp::VectorSelect {
            mask,
            then_value,
            else_value,
            ..
        } => {
            fv_atom(mask, bound, free);
            fv_atom(then_value, bound, free);
            fv_atom(else_value, bound, free);
        }
        Comp::MaskReduce { mask, .. } => fv_atom(mask, bound, free),
        Comp::VectorExtract { vector, .. } => fv_atom(vector, bound, free),
        Comp::Cast(_, a) => fv_atom(a, bound, free),
        Comp::Tag(a) | Comp::Untag(a) | Comp::ToPtr(a) | Comp::FromPtr(a) => {
            fv_atom(a, bound, free)
        }
        Comp::FloatMathUnary { value, .. } => fv_atom(value, bound, free),
        Comp::FloatMathBinary { lhs, rhs, .. } => {
            fv_atom(lhs, bound, free);
            fv_atom(rhs, bound, free);
        }
        Comp::FloatMathTernary { a, b, c, .. } => {
            fv_atom(a, bound, free);
            fv_atom(b, bound, free);
            fv_atom(c, bound, free);
        }
        Comp::If(c, t, e) => {
            fv_atom(c, bound, free);
            fv_ir(t, bound, free);
            fv_ir(e, bound, free);
        }
        Comp::Loop {
            vars,
            cond,
            steps,
            result,
        } => {
            for var in vars {
                fv_atom(&var.init, bound, free);
            }
            for var in vars {
                bound.push(var.name.clone());
            }
            fv_ir(cond, bound, free);
            for step in steps {
                fv_ir(step, bound, free);
            }
            fv_ir(result, bound, free);
            for _ in vars {
                bound.pop();
            }
        }
        Comp::Lam { param, body, .. } => {
            bound.push(param.clone());
            fv_ir(body, bound, free);
            bound.pop();
        }
        Comp::Perform(_, a) | Comp::Splice(a) | Comp::Genlet(a) => fv_atom(a, bound, free),
        Comp::Peek(_, a) => fv_atom(a, bound, free),
        Comp::Poke(_, a, b) => {
            fv_atom(a, bound, free);
            fv_atom(b, bound, free);
        }
        Comp::Fill(a, b, c) | Comp::Copy(a, b, c) => {
            fv_atom(a, bound, free);
            fv_atom(b, bound, free);
            fv_atom(c, bound, free);
        }
        Comp::Tuple(fields) => {
            for (a, _) in fields {
                fv_atom(a, bound, free);
            }
        }
        Comp::ArrayLit { elems, .. } => {
            for a in elems {
                fv_atom(a, bound, free);
            }
        }
        Comp::Proj { tup, .. } => fv_atom(tup, bound, free),
        Comp::Len(a) => fv_atom(a, bound, free),
        Comp::ArrayGet { arr, idx, .. } => {
            fv_atom(arr, bound, free);
            fv_atom(idx, bound, free);
        }
        Comp::ArraySet { arr, idx, val, .. } => {
            fv_atom(arr, bound, free);
            fv_atom(idx, bound, free);
            fv_atom(val, bound, free);
        }
        Comp::VectorLoad { arr, idx, .. } => {
            fv_atom(arr, bound, free);
            fv_atom(idx, bound, free);
        }
        Comp::VectorStore {
            arr, idx, value, ..
        } => {
            fv_atom(arr, bound, free);
            fv_atom(idx, bound, free);
            fv_atom(value, bound, free);
        }
        // Mutable-local slots: the stored atom is an ordinary operand. The slot
        // name lives in its own (function-local, never-captured) namespace, not
        // among the SSA names this scan tracks, so it is not a free variable.
        Comp::SlotInit(_, init) => fv_atom(init, bound, free),
        Comp::SlotLoad(_) => {}
        Comp::SlotStore(_, val) => fv_atom(val, bound, free),
        // Heap `Ref` cell ops — the handle/value atoms are ordinary operands a
        // closure may capture (a `Ref` is a first-class value), so scan them.
        Comp::RefNew(init, _) => fv_atom(init, bound, free),
        Comp::RefGet(r, _) => fv_atom(r, bound, free),
        Comp::RefSet(r, val, _) => {
            fv_atom(r, bound, free);
            fv_atom(val, bound, free);
        }
        Comp::Quote(b) | Comp::Letloc(b) => fv_ir(b, bound, free),
        Comp::Handle {
            scrutinee, handler, ..
        } => {
            fv_ir(scrutinee, bound, free);
            for op in &handler.ops {
                bound.push(op.arg.clone());
                bound.push(op.resume.clone());
                fv_ir(&op.body, bound, free);
                bound.pop();
                bound.pop();
            }
            bound.push(handler.ret.var.clone());
            fv_ir(&handler.ret.body, bound, free);
            bound.pop();
        }
    }
}

fn fv_atom(a: &Atom, bound: &[String], free: &mut HashSet<String>) {
    if let Atom::Var(x) = a {
        if !bound.contains(x) {
            free.insert(x.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_region_counts_multi_cell_scalar_fields() {
        // A `Quad[Float32]` field (16 B) is now ACCEPTED as 2 contiguous scalar
        // cells — the SIMD Sprint 1 generalization of the old single-cell reject.
        let quad = ValueLayout::scalar_bytes(16, 16);
        match field_region(quad, "quad field").expect("multi-cell scalar accepted") {
            FieldRegion::Scalar { cells } => assert_eq!(cells, 2),
            FieldRegion::Pointer { .. } => panic!("a vector field is a scalar region"),
        }

        // A single scalar is 1 cell; a handle is the pointer region.
        match field_region(ValueLayout::scalar_cell(), "int field").unwrap() {
            FieldRegion::Scalar { cells } => assert_eq!(cells, 1),
            FieldRegion::Pointer { .. } => panic!(),
        }
        assert!(matches!(
            field_region(ValueLayout::pointer_cell(), "handle field").unwrap(),
            FieldRegion::Pointer { word: false }
        ));
        // A `Var` word cell is the pointer region, verbatim-stored.
        assert!(matches!(
            field_region(ValueLayout::word_cell(), "var field").unwrap(),
            FieldRegion::Pointer { word: true }
        ));

        // A mixed pointer/scalar layout is still a hard error (no first-class
        // value packing yet); an unknown layout too.
        let mixed = ValueLayout {
            pointer_cells: 1,
            scalar_cells: 1,
            byte_width: 16,
            align: 8,
            known: true,
            word: false,
        };
        assert!(field_region(mixed, "mixed value")
            .unwrap_err()
            .contains("unsupported mixed layout p1 s1"));
        assert!(
            field_region(ValueLayout::unknown_scalar_cell(), "poly value")
                .unwrap_err()
                .contains("unknown storage layout")
        );
    }
}
