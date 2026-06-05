//! # Locus — the LLVM/ORC backend
//!
//! The core [`locus`] crate is std-only and stops at the **ANF IR**
//! ([`locus::ir`]); this crate is where that IR becomes machine code. inkwell
//! builds an LLVM module, llvm-sys's ORCv2 LLJIT compiles and runs it.
//!
//! - [`lower`] — ANF IR → LLVM module.
//! - [`jit`] — donate the module to ORC, JIT, look up & call a symbol.
//!
//! - **Slice 15:** the toolchain is wired and the pure fragment runs.
//! - **Slices 16+: effects and FFI.** A foreign call (`extern`, carrying the
//!   `winapi` effect) lowers to a direct `call` that ORC / the linker resolves;
//!   values ride a uniform `i64` model, widths converted at the boundary.
//! - **Slice 17: AOT — a real `.exe`.** [`aot::build_exe`] emits a native object
//!   via an LLVM `TargetMachine`, compiles a tiny C runtime (the CRT `main` plus
//!   the closure allocator `locus_alloc`), and links it with MSVC `link.exe` into
//!   a standalone executable. `42` becomes exit code 42. JIT *and* AOT.
//! - **Output is ordinary Locus:** `console_writeln` (the prelude, [`locus::stdlib`])
//!   over raw Win32 — there is no native console. The only runtime symbol is the
//!   closure allocator [`runtime::locus_alloc`].

#[cfg(feature = "windows-driver")]
pub mod aot;
#[cfg(feature = "windows-driver")]
pub mod asm_runtime;
#[cfg(feature = "windows-driver")]
pub mod jit;
pub mod lower;
#[cfg(all(feature = "windows-driver", feature = "mcp"))]
pub mod mcp;
#[cfg(feature = "windows-driver")]
pub mod runtime;
#[cfg(feature = "windows-driver")]
pub mod winapi_resolve;

#[cfg(feature = "windows-driver")]
pub use aot::{build_exe, emit_asm, emit_asm_opt, emit_library_object, link_program};
#[cfg(feature = "windows-driver")]
pub use jit::jit_run_i64;
pub use lower::{emit_library_module, LibExport};

#[cfg(all(test, feature = "windows-driver"))]
mod tests {
    use super::*;

    /// parse (+ prelude graft) → resolve bare externs (oracle) → elaborate →
    /// ANF → JIT → run, returning the `i64` result. The full driver path, so
    /// tests can call prelude fns (`console_writeln`) and bare Win32 externs.
    fn run(src: &str) -> Result<i64, String> {
        let src = src.to_string();
        std::thread::Builder::new()
            .name("llvm-test-run".into())
            .stack_size(locus::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let term = locus::program(&src).map_err(|e| e.msg)?;
                let (term, apis) = crate::winapi_resolve::resolve(term)?;
                let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
                    .map_err(|e| e.to_string())?;
                let tree = locus::stage_reduce(&tree)?;
                let ir = locus::lower(&tree);
                jit_run_i64(&ir, &apis)
            })
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "llvm test worker panicked".to_string())?
    }

    fn run_agent_text(
        src: &str,
        responses: Vec<String>,
    ) -> Result<(i64, crate::runtime::AgentTranscript), String> {
        let src = src.to_string();
        std::thread::Builder::new()
            .name("llvm-test-run-agent".into())
            .stack_size(locus::PIPELINE_STACK_BYTES)
            .spawn(move || {
                let term = locus::program(&src).map_err(|e| e.msg)?;
                let (term, apis) = crate::winapi_resolve::resolve(term)?;
                let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
                    .map_err(|e| e.to_string())?;
                let tree = locus::stage_reduce(&tree)?;
                let ir = locus::lower(&tree);
                let (run_result, transcript) =
                    crate::runtime::with_agent_text_session(responses, String::new(), || {
                        jit_run_i64(&ir, &apis)
                    });
                run_result.map(|value| (value, transcript))
            })
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "llvm agent test worker panicked".to_string())?
    }

    fn run_f64(src: &str) -> Result<f64, String> {
        run(src).map(|bits| f64::from_bits(bits as u64))
    }

    fn run_f32(src: &str) -> Result<f32, String> {
        run(src).map(|bits| f32::from_bits(bits as u32))
    }

    /// The UTF-16 → UTF-8 transcoder (`examples/utf16_to_utf8.locus`, inlined)
    /// up to allocating `out`; append `let n = go "<input>" out 0 0 in <ret>`.
    const CONVERTER: &str = r#"
let emit = fn out: Int => fn j: Int => fn cp: Int =>
  if cp < 0x80 then (let _ = out[j] <- cp in j + 1)
  else if cp < 0x800 then
    (let _ = out[j] <- (0xC0 | (cp >> 6)) in
     let _ = out[j + 1] <- (0x80 | (cp & 0x3F)) in j + 2)
  else if cp < 0x10000 then
    (let _ = out[j] <- (0xE0 | (cp >> 12)) in
     let _ = out[j + 1] <- (0x80 | ((cp >> 6) & 0x3F)) in
     let _ = out[j + 2] <- (0x80 | (cp & 0x3F)) in j + 3)
  else
    (let _ = out[j] <- (0xF0 | (cp >> 18)) in
     let _ = out[j + 1] <- (0x80 | ((cp >> 12) & 0x3F)) in
     let _ = out[j + 2] <- (0x80 | ((cp >> 6) & 0x3F)) in
     let _ = out[j + 3] <- (0x80 | (cp & 0x3F)) in j + 4)
in
let rec go : String -> Int -> Int -> Int -> Int -> Int ! {mem, gc} =
  fn s: String => fn out: Int => fn limit: Int => fn i: Int => fn j: Int =>
    if i < limit then
      let unit = s[i] in
      if unit < 0xD800 then go s out limit (i + 1) (emit out j unit)
      else if unit < 0xDC00 then
        if (i + 1) < limit then
          (let lo = s[i + 1] in
           let cp = 0x10000 + ((unit - 0xD800) << 10) + (lo - 0xDC00) in
           go s out limit (i + 2) (emit out j cp))
        else
          go s out limit (i + 1) (emit out j unit)
      else go s out limit (i + 1) (emit out j unit)
    else
      j
in
let alloc = extern "VirtualAlloc" : Int -> Int -> I32 -> I32 -> Int in
let out = alloc 0 256 0x3000 0x04 in
"#;

    /// Build a converter program for `input`, returning `ret` (which can read
    /// `n` = the byte count, and `out[k]` = the k-th UTF-8 byte).
    fn convert(input: &str, ret: &str) -> String {
        format!(
            "{CONVERTER} let input = \"{input}\" in let n = go input out (len input) 0 0 in {ret}"
        )
    }

    /// The same front-end path as `run`, stopping at the ANF IR (for the asm
    /// dump, which needs no JIT / no demanded APIs).
    fn ir_of(src: &str) -> locus::Ir {
        let src = src.to_string();
        std::thread::Builder::new()
            .name("llvm-test-ir".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let term = locus::program(&src).unwrap();
                let (term, _apis) = crate::winapi_resolve::resolve(term).unwrap();
                let tree =
                    locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term).unwrap();
                locus::lower(&locus::stage_reduce(&tree).unwrap())
            })
            .expect("spawn LLVM IR worker")
            .join()
            .expect("LLVM IR worker panicked")
    }

    fn llvm_function_body_containing<'a>(text: &'a str, needles: &[&str]) -> &'a str {
        let mut rest = text;
        while let Some(start) = rest.find("define i64 @") {
            let body_start = &rest[start..];
            let end = body_start[1..]
                .find("\ndefine ")
                .map(|idx| idx + 1)
                .unwrap_or(body_start.len());
            let body = &body_start[..end];
            if needles.iter().all(|needle| body.contains(needle)) {
                return body;
            }
            rest = &body_start[end..];
        }
        panic!("LLVM function body containing {needles:?} is present");
    }

    #[test]
    fn jits_an_int_literal() {
        assert_eq!(run("42").unwrap(), 42);
    }

    #[test]
    fn emits_x86_64_assembly() {
        // The asm dump is real host x86-64 text naming the program entry. And a
        // discharged tail-resumptive handler folds to a constant load — the
        // zero-cost guarantee, visible in the machine code itself.
        let asm = emit_asm(&ir_of(
            "handle perform console \"x\" with { console(s) => resume () ; return(y) => 42 }",
        ))
        .unwrap();
        assert!(asm.contains("__locus_main"), "names the program entry");
        assert!(
            asm.contains("$42"),
            "the whole handler is gone — just `mov $42`"
        );
    }

    #[test]
    fn jits_a_let_binding() {
        assert_eq!(run("let x = 7 in x").unwrap(), 7);
    }

    #[test]
    fn jits_a_nested_let() {
        // let a = 1 in let b = 99 in b   →  99
        assert_eq!(run("let a = 1 in let b = 99 in b").unwrap(), 99);
    }

    #[test]
    fn jits_arithmetic() {
        assert_eq!(run("1 + 2 * 3").unwrap(), 7, "precedence: 1 + (2*3)");
        assert_eq!(run("let x = 6 in x * 7").unwrap(), 42);
        assert_eq!(run("10 - 3 - 2").unwrap(), 5, "left-assoc subtraction");
        assert_eq!(
            run("9223372036854775807 +% 1").unwrap(),
            i64::MIN,
            "explicit wrapping addition"
        );
        assert_eq!(run("40 +? 2").unwrap(), 42, "checked add, no overflow");
        assert_eq!(run("50 -? 8").unwrap(), 42, "checked sub, no overflow");
        assert_eq!(run("6 *? 7").unwrap(), 42, "checked mul, no overflow");
        // comparisons widen to i64 (1/0).
        assert_eq!(run("1 < 2").unwrap(), 1);
        assert_eq!(run("3 < 2").unwrap(), 0);
        assert_eq!(run("let n = 7 in n == 7").unwrap(), 1);
    }

    #[test]
    fn checked_integer_arithmetic_emits_overflow_intrinsic() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of("40 +? 2");
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("llvm.sadd.with.overflow.i64"),
            "checked add should call the overflow intrinsic:\n{text}"
        );
        assert!(
            text.contains("llvm.trap"),
            "checked overflow should trap on the overflow edge:\n{text}"
        );
    }

    #[test]
    fn jits_float_arithmetic_as_raw_scalar_bits() {
        assert_eq!(run_f64("1.5 + 2.25").unwrap(), 3.75);
        assert_eq!(run_f64("3.0 / 2.0").unwrap(), 1.5);
        assert_eq!(run_f64("let x = 4.0 in let y = 0.5 in x * y").unwrap(), 2.0);
    }

    #[test]
    fn jits_mutable_local_read_modify_write() {
        // mutability v1 (`let mut` / `:=`): a non-escaping scalar stack slot.
        // Single read-modify-write: x starts 1, becomes 1 + 41 = 42.
        assert_eq!(
            run("let mut x = 1 in let _ = x := x + 41 in x").unwrap(),
            42
        );
        // Multiple assigns reflect the LAST write.
        assert_eq!(
            run("let mut x = 0 in let _ = x := 10 in let _ = x := 20 in x").unwrap(),
            20
        );
        // A read-modify-write chain: 5 -> 10 -> 11.
        assert_eq!(
            run("let mut x = 5 in let _ = x := x * 2 in let _ = x := x + 1 in x").unwrap(),
            11
        );
    }

    #[test]
    fn jits_mutable_local_driven_by_a_loop() {
        // A `let mut` accumulator driven by the native `loop` form: the loop runs a
        // counter `i` (and a dummy `acc`), and each step *assigns* into the mutable
        // local `x` declared in the enclosing function. The slot lives across the
        // loop's preheader/body/exit blocks (one stable entry-block alloca), so the
        // final read sees the imperative sum 0+1+2+3+4 = 10.
        assert_eq!(
            run(
                "let mut x = 0 in \
                 let _ = (loop i = 0, acc = 0 while i < 5 do i + 1, (let _ = x := x + i in acc) else acc) in \
                 x"
            )
            .unwrap(),
            10
        );
    }

    #[test]
    fn jits_mutable_float_local() {
        // A Float mutable local: the cell rides the uniform i64 word model, so the
        // same alloca/store/load lowers Float bits unchanged. 1.5 + 0.5 = 2.0.
        assert_eq!(
            run_f64("let mut x = 1.5 in let _ = x := x + 0.5 in x").unwrap(),
            2.0
        );
    }

    #[test]
    fn jits_the_scalar_ref_counter_gate() {
        // THE Sprint-1 gate (`docs/mutability-ref-sprints.md`): a first-class
        // `Ref[Int]` heap cell — `ref e` allocates it, `r := !r + 41` reads + writes
        // through the handle, `!r` reads it back. Through the REAL collector (alloc
        // + set_scalar/get_scalar on a one-field heap object). NB the sprint plan's
        // literal `ref 0 … !r + 41 ⇒ 42` is an off-by-one (0 + 41 = 41); the gate's
        // *answer* is 42, so seed `ref 1` for the honest read-modify-write 1+41=42.
        assert_eq!(
            run("let r = ref 1 in let _ = (r := !r + 41) in !r").unwrap(),
            42
        );
        // And the off-by-one-faithful `ref 0` form computes the correct 41.
        assert_eq!(
            run("let r = ref 0 in let _ = (r := !r + 41) in !r").unwrap(),
            41
        );
        // The last write wins, like any mutable cell.
        assert_eq!(
            run("let r = ref 5 in let _ = (r := 10) in let _ = (r := 20) in !r").unwrap(),
            20
        );
    }

    #[test]
    fn jits_a_scalar_ref_float_round_trip() {
        // A `Ref[Float]` round-trips through the heap cell: the content cell stores
        // the float bits verbatim (set_scalar/get_scalar are opaque i64 moves), and
        // the `+` is the float path. 1.5 + 2.0 = 3.5.
        assert_eq!(
            run_f64("let r = ref 1.5 in let _ = (r := !r + 2.0) in !r").unwrap(),
            3.5
        );
    }

    #[test]
    fn jits_a_ref_passed_to_a_function() {
        // A `Ref` is a first-class value: pass it to a function that reads AND writes
        // it through the handle it was handed — proving the cell is a real, passable
        // heap object (not a stack slot). `bump` reads !s (41), writes 42, returns 42.
        assert_eq!(
            run("let r = ref 41 in \
                 let bump = fn s => (let _ = (s := !s + 1) in !s) in \
                 bump r")
            .unwrap(),
            42
        );
    }

    #[test]
    fn runs_the_numeric_array_helpers() {
        // Execution (not just type-check) of the new stdlib helpers, end to end
        // through the JIT. Dot product: 1*4 + 2*5 + 3*6 = 32.
        assert_eq!(
            run_f64("array_dot_float ([1.0, 2.0, 3.0]) ([4.0, 5.0, 6.0])").unwrap(),
            32.0
        );
        // In-place scale by 2, then read an element back: [1,2,3]*2 ⇒ a[2] = 6.
        assert_eq!(
            run_f64("let a = [1.0, 2.0, 3.0] in let _ = array_scale_float a 2.0 in a[2]").unwrap(),
            6.0
        );
    }

    #[test]
    fn runs_the_num_clamp_helper() {
        // clamp x lo hi — above, below, and within the range.
        assert_eq!(run("clamp 15 0 10").unwrap(), 10);
        assert_eq!(run("clamp (0 - 5) 0 10").unwrap(), 0);
        assert_eq!(run("clamp 5 0 10").unwrap(), 5);
    }

    #[test]
    fn runs_the_float_num_helpers() {
        // fmin/fmax/fclamp on Float — proves Float comparison threads through, and
        // (with the bigger pipeline stack) the extra stdlib bindings graft cleanly.
        assert_eq!(run_f64("fmin 2.0 5.0").unwrap(), 2.0);
        assert_eq!(run_f64("fmax 2.0 5.0").unwrap(), 5.0);
        assert_eq!(run_f64("fclamp 7.5 0.0 1.0").unwrap(), 1.0);
        assert_eq!(run_f64("fclamp (0.0 - 1.0) 0.0 1.0").unwrap(), 0.0);
    }

    #[test]
    fn jits_local_simd_vector_arithmetic() {
        assert_eq!(
            run_f64(
                "let a = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
                 let b = splatQuad (toFloat32 10.0) in \
                 let c = a + b in \
                 fromFloat32 (c.x) + fromFloat32 (c.y) + fromFloat32 (c.z) + fromFloat32 (c.w)"
            )
            .unwrap(),
            50.0
        );
        assert_eq!(
            run_f64(
                "let a = Pair(1.25, 2.5) in \
                 let b = splatPair 0.5 in \
                 let c = a * b in c.lane0 + c.lane1"
            )
            .unwrap(),
            1.875
        );
        assert_eq!(
            run_f64(
                "let a = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
                 let c = a * 4.0 in \
                 fromFloat32 (c.x) + fromFloat32 (c.y) + fromFloat32 (c.z) + fromFloat32 (c.w)"
            )
            .unwrap(),
            40.0
        );
        assert_eq!(
            run_f64("let a = Pair(1.25, 2.5) in let c = 4.0 * a in c.lane0 + c.lane1").unwrap(),
            15.0
        );
    }

    #[test]
    fn jits_vector_field_in_tuple_round_trips() {
        // SIMD Sprint 1: a `Quad[Float32]` (16 B = 2 scalar cells) stored in a
        // tuple and projected back — the lane equals the value stored. Exercises
        // the multi-cell scalar store/load in `lower_tuple`/`lower_proj`.
        assert_eq!(
            run_f64(
                "let q = Quad(toFloat32 1.5, toFloat32 2.5, toFloat32 3.5, toFloat32 4.5) in \
                 let t = (q, 0) in \
                 let (r, n) = t in \
                 fromFloat32 (r.x) + fromFloat32 (r.y) + fromFloat32 (r.z) + fromFloat32 (r.w)"
            )
            .unwrap(),
            12.0
        );
        // Mixed tuple `(1, splatQuad 2.0, 3)` — the multi-cell vector sits BETWEEN
        // two scalar fields, so projecting each exercises cumulative scalar-cell
        // offset accumulation (Int@0, Quad@1..2, Int@3).
        assert_eq!(
            run("let t = (1, splatQuad (toFloat32 2.0), 3) in \
                 let (a, q, b) = t in \
                 a * 100 + round (fromFloat32 (q.x) + fromFloat32 (q.w)) + b")
            .unwrap(),
            107
        );
    }

    #[test]
    fn jits_vector_field_in_record_round_trips() {
        // The same multi-cell round-trip through a RECORD field (named, sorted).
        // `v` is a `Quad[Float32]`, `n` an Int after it — the record lays out the
        // 2-cell vector then the scalar, and each reads back correctly.
        assert_eq!(
            run_f64(
                "let p = { v = Quad(toFloat32 10.0, toFloat32 20.0, toFloat32 30.0, toFloat32 40.0), n = 7 } in \
                 fromFloat32 (p.v.x) + fromFloat32 (p.v.w) + toFloat (p.n)"
            )
            .unwrap(),
            57.0
        );
        // A `Pair[Float]` (also 16 B = 2 cells, but f64 lanes) in a record —
        // confirms the reassembly picks the right LLVM vector type from `ty`.
        assert_eq!(
            run_f64(
                "let p = { hi = 1, d = Pair(1.25, 2.5) } in \
                 p.d.lane0 + p.d.lane1 + toFloat p.hi"
            )
            .unwrap(),
            4.75
        );
    }

    #[test]
    fn jits_vector_array_round_trips() {
        // SIMD Sprint 1: an `Array[Quad[Float32]]` — build it, overwrite an
        // element, read it back, extract lanes. Each element is 2 contiguous
        // scalar cells at `i * elem_bytes`.
        assert_eq!(
            run_f64(
                "let a = [splatQuad (toFloat32 0.0), splatQuad (toFloat32 0.0)] in \
                 let _ = a[1] <- Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
                 let q = a[1] in \
                 fromFloat32 (q.x) + fromFloat32 (q.y) + fromFloat32 (q.z) + fromFloat32 (q.w)"
            )
            .unwrap(),
            10.0
        );
        // A literal-built `Array[Quad[Float32]]` indexed directly — element 0 is
        // untouched by the element-1 write, so its lanes are the literal's.
        assert_eq!(
            run_f64(
                "let a = [Quad(toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0), \
                          splatQuad (toFloat32 0.0)] in \
                 let q = a[0] in fromFloat32 (q.x) + fromFloat32 (q.w)"
            )
            .unwrap(),
            13.0
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn quad_float32_add_emits_packed_simd_instruction() {
        // The runtime Win32 value keeps the vector addition from folding to a
        // constant before the TargetMachine gets to emit assembly.
        let asm = emit_asm(&ir_of(
            r#"let now = extern "GetTickCount64" : Unit -> Int in
               let x = toFloat32 (toFloat (now ())) in
               let a = Quad(x, x, x, x) in
               let b = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in
               let c = a + b in
               round (fromFloat32 (c.x) + fromFloat32 (c.y) + fromFloat32 (c.z) + fromFloat32 (c.w))"#,
        ))
        .unwrap();

        assert!(
            asm.contains("addps") || asm.contains("vaddps"),
            "Quad[Float32] addition should lower to packed SIMD add:\n{asm}"
        );
    }

    #[test]
    fn jits_vector_array_load_add_store_kernel() {
        // SIMD Sprint 2 — the kernel primitive: stride a loop by the lane count,
        // `loadQuad` a contiguous chunk from two `Array[Float32]` inputs, packed-
        // add, `storeQuad` into an output array; then read the output elements
        // and confirm they equal the elementwise sum. Length 8 = 2*4 lanes (an
        // exact multiple — tail handling is out of Sprint-2 scope). The single
        // packed load/op/store is the point; here we check the *numbers*.
        assert_eq!(
            run_f64(
                "let a = [toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0, \
                          toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0] in \
                 let b = [toFloat32 10.0, toFloat32 20.0, toFloat32 30.0, toFloat32 40.0, \
                          toFloat32 50.0, toFloat32 60.0, toFloat32 70.0, toFloat32 80.0] in \
                 let out = [toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, \
                            toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0] in \
                 let _ = (loop i = 0, acc = 0 while i < len out \
                          do i + 4, \
                             (let _ = storeQuad(out, i, loadQuad(a, i) + loadQuad(b, i)) in acc) \
                          else acc) in \
                 fromFloat32 (out[0]) + fromFloat32 (out[3]) + fromFloat32 (out[4]) + fromFloat32 (out[7])"
            )
            .unwrap(),
            // out[0]=11, out[3]=44, out[4]=55, out[7]=88 → 198.
            198.0
        );
        // A `Pair[Float]` (f64-lane) kernel — a single chunk, the load/add/store
        // over a length-2 array, to exercise the 8-byte lane stride path too.
        assert_eq!(
            run_f64(
                "let a = [1.5, 2.5] in \
                 let b = [10.0, 20.0] in \
                 let out = [0.0, 0.0] in \
                 let _ = storePair(out, 0, loadPair(a, 0) + loadPair(b, 0)) in \
                 out[0] + out[1]"
            )
            .unwrap(),
            // (1.5+10) + (2.5+20) = 11.5 + 22.5 = 34.
            34.0
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn vector_array_load_store_emits_packed_simd_memory_ops() {
        // The kernel's load must lower to ONE packed `<4 x float>` vector load and
        // the store to ONE packed vector store — not 4 scalar loads/stores. A
        // runtime Win32 value keeps the optimizer from folding the chunk away.
        let kernel = r#"let now = extern "GetTickCount64" : Unit -> Int in
               let seed = toFloat32 (toFloat (now ())) in
               let a = [seed, seed, seed, seed] in
               let b = [toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0] in
               let out = [toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0] in
               let _ = storeQuad(out, 0, loadQuad(a, 0) + loadQuad(b, 0)) in
               round (fromFloat32 (out[0]))"#;

        // LLVM IR: a typed `<4 x float>` load and store (element-aligned, align 4).
        let ctx = inkwell::context::Context::create();
        let module = crate::lower::emit_module(&ctx, &ir_of(kernel), false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("load <4 x float>"),
            "loadQuad should be one packed vector load:\n{text}"
        );
        assert!(
            text.contains("store <4 x float>"),
            "storeQuad should be one packed vector store:\n{text}"
        );

        // And the emitted machine code uses packed SSE moves (movups for the
        // element-aligned load/store) plus a packed add — genuinely one SIMD
        // load/op/store, not a scalar loop.
        let asm = emit_asm(&ir_of(kernel)).unwrap();
        assert!(
            asm.contains("movups") || asm.contains("movaps") || asm.contains("vmovups"),
            "vector load/store should be packed SSE moves:\n{asm}"
        );
        assert!(
            asm.contains("addps") || asm.contains("vaddps"),
            "the kernel add should be a packed SIMD add:\n{asm}"
        );
    }

    #[test]
    fn vector_kernel_loop_hoists_the_array_handle_deref_out_of_the_loop() {
        // SIMD Sprint 3, Part A — the raw-pointer fast path for vector load/store.
        // The kernel loop `loadQuad`s from two input arrays (`a`, `b`), packed-adds,
        // and `storeQuad`s into `out`. Sprint 2 already borrowed `out` (it is touched
        // by `len out` + `storeQuad`), but the *inputs* `a`/`b`, reached ONLY through
        // `loadQuad`, were not in the borrow set — so each iteration re-derefed their
        // handle via `locus_gc_scalar_fields_ptr`. Extending `collect_raw_array_uses`
        // to `Comp::VectorLoad`/`VectorStore` borrows all three once before the loop;
        // `vector_array_elem_ptr` then reuses the cached base every iteration.
        //
        // Evidence: every `scalar_fields_ptr` call sits in the loop PREHEADER (before
        // `br label %loop`); the loop body emits ZERO of them. The whole-vector bounds
        // check (`idx + lanes <= len`) is still emitted per access — the deref is
        // hoisted, the bounds check is not elided.
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0, \
                      toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0] in \
             let b = [toFloat32 10.0, toFloat32 20.0, toFloat32 30.0, toFloat32 40.0, \
                      toFloat32 50.0, toFloat32 60.0, toFloat32 70.0, toFloat32 80.0] in \
             let out = [toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, \
                        toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0] in \
             let _ = (loop i = 0, acc = 0 while i < len out \
                      do i + 4, \
                         (let _ = storeQuad(out, i, loadQuad(a, i) + loadQuad(b, i)) in acc) \
                      else acc) in \
             round (fromFloat32 (out[0]))",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();

        // The deref is a `call ptr @locus_gc_scalar_fields_ptr(...)`; the function
        // declaration (`declare ptr @...`) is not a call and must not be counted.
        let calls: Vec<usize> = text
            .match_indices("call ptr @locus_gc_scalar_fields_ptr")
            .map(|(i, _)| i)
            .collect();
        // Three borrowed arrays (a, b, out) ⇒ exactly three hoisted derefs.
        assert_eq!(
            calls.len(),
            3,
            "the loop's three arrays are each derefed once (hoisted):\n{text}"
        );

        // The loop is entered with `br label %loop`; everything before it is the
        // preheader, everything after is the header/body/exit. Every deref must be
        // in the preheader, none in the loop region.
        let loop_entry = text
            .find("br label %loop")
            .expect("the kernel emits a loop with a preheader branch");
        for &at in &calls {
            assert!(
                at < loop_entry,
                "every array handle deref must be hoisted into the preheader, not the loop body:\n{text}"
            );
        }

        // And the loop body still does the packed load + the per-access bounds check
        // — the hoist caches the base, it does NOT elide the whole-vector bound.
        assert!(
            text.contains("load <4 x float>"),
            "the body still does the packed vector load over the cached base:\n{text}"
        );
        assert!(
            text.contains("vec.idx.past") && text.contains("vector.index.trap"),
            "the whole-vector bounds check (idx + lanes <= len) is still emitted per access:\n{text}"
        );
    }

    #[test]
    fn vector_load_bounds_check_emits_a_trap() {
        // `loadQuad(a, i)` bounds-checks the WHOLE vector (`i + 4 <= len`): an
        // out-of-bounds chunk must trap, never read past the array. We assert the
        // emitted IR carries the trap edge (executing the JIT trap would abort the
        // test process; the overflow-trap tests check the IR the same way) — and
        // that it guards a real `<4 x float>` load (the load is downstream, not
        // skipped). The kernel below loads at index 2 of a length-4 array, which
        // needs elements 2..6 — past the end, so the trap edge is taken at runtime.
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0] in \
             let q = loadQuad(a, 2) in round (fromFloat32 (q.x))",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("vector.index.trap"),
            "a vector load must emit a bounds-check trap block:\n{text}"
        );
        assert!(
            text.contains("llvm.trap"),
            "the out-of-bounds edge must reach llvm.trap:\n{text}"
        );
        assert!(
            text.contains("load <4 x float>"),
            "the in-bounds edge still does one packed vector load:\n{text}"
        );
    }

    #[test]
    fn vector_load_store_element_type_mismatch_is_an_elaboration_error() {
        // Loading an `Array[Int]` as a float vector is a clean elaboration error
        // (Int is not a vector lane type) — no codegen, a typed message.
        assert!(run("let a = [1, 2, 3, 4] in let q = loadQuad(a, 0) in q.x")
            .unwrap_err()
            .contains("type"));
        // A lane/element mismatch: storing a `Quad[Float]` (f64 lanes) into an
        // `Array[Float32]` is rejected — the stored vector type must match the
        // array's element type exactly.
        assert!(run(
            "let a = [toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0] in \
             let _ = storeQuad(a, 0, Quad(1.0, 2.0, 3.0, 4.0)) in 0"
        )
        .unwrap_err()
        .contains("type"));
    }

    #[test]
    fn jits_explicit_sqrt_and_fma() {
        assert_eq!(run_f64("sqrt 9.0").unwrap(), 3.0);
        assert_eq!(run_f64("fma(1.5, 2.0, 0.25)").unwrap(), 3.25);
        assert_eq!(run_f64("fromFloat32 (sqrt (toFloat32 9.0))").unwrap(), 3.0);
        assert_eq!(
            run_f64(
                "let a = Quad(toFloat32 4.0, toFloat32 9.0, toFloat32 16.0, toFloat32 25.0) in \
                 let r = sqrt a in \
                 fromFloat32 (r.x) + fromFloat32 (r.y) + fromFloat32 (r.z) + fromFloat32 (r.w)"
            )
            .unwrap(),
            14.0
        );
        assert_eq!(
            run_f64(
                "let a = splatQuad (toFloat32 2.0) in \
                 let b = splatQuad (toFloat32 3.0) in \
                 let c = splatQuad (toFloat32 4.0) in \
                 let r = fma(a, b, c) in fromFloat32 (r.x) + fromFloat32 (r.w)"
            )
            .unwrap(),
            20.0
        );
    }

    #[test]
    fn explicit_vector_math_emits_llvm_intrinsics() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = splatQuad (toFloat32 4.0) in \
             let b = splatQuad (toFloat32 2.0) in \
             let c = fma(a, b, sqrt a) in fromFloat32 (c.x)",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("llvm.sqrt.v4f32"), "{text}");
        assert!(text.contains("llvm.fma.v4f32"), "{text}");
    }

    #[test]
    fn jits_vector_reductions() {
        assert_eq!(
            run_f64(
                "fromFloat32 (sum (Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0)))"
            )
            .unwrap(),
            10.0
        );
        assert_eq!(
            run_f64("dot(Pair(1.0, 2.0), Pair(3.0, 4.0))").unwrap(),
            11.0
        );
        assert_eq!(
            run_f64(
                "fromFloat32 (length (Quad(toFloat32 3.0, toFloat32 4.0, toFloat32 0.0, toFloat32 0.0)))"
            )
            .unwrap(),
            5.0
        );
    }

    #[test]
    fn vector_reductions_emit_llvm_intrinsics() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
             fromFloat32 (sum a)",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("llvm.vector.reduce.fadd.v4f32"), "{text}");
    }

    #[test]
    fn jits_vector_masks_select_and_reductions() {
        assert_eq!(
            run_f64(
                "let a = Quad(toFloat32 1.0, toFloat32 5.0, toFloat32 2.0, toFloat32 8.0) in \
                 let b = splatQuad (toFloat32 4.0) in \
                 let c = select(a < b, b, a) in \
                 fromFloat32 (c.x) + fromFloat32 (c.y) + fromFloat32 (c.z) + fromFloat32 (c.w)"
            )
            .unwrap(),
            21.0
        );
        assert_eq!(
            run("let a = Pair(1.0, 2.0) in if any (a < 1.5) then 1 else 0").unwrap(),
            1
        );
        assert_eq!(
            run("let a = Pair(1.0, 2.0) in if all (a < 1.5) then 1 else 0").unwrap(),
            0
        );
        assert_eq!(
            run("let a = Pair(1.0, 2.0) in if all (a < 3.0) then 1 else 0").unwrap(),
            1
        );
        assert_eq!(
            run("let a = Pair(1.0, 2.0) in if any (2.0 == a) then 1 else 0").unwrap(),
            1
        );
    }

    #[test]
    fn vector_masks_emit_fcmp_and_select() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            r#"let now = extern "GetTickCount64" : Unit -> Int in
               let x = toFloat32 (toFloat (now ())) in
               let a = Quad(x, x, x, x) in
               let b = splatQuad (toFloat32 4.0) in
               let c = select(a < b, b, a) in fromFloat32 (c.x)"#,
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("fcmp olt <4 x float>"), "{text}");
        assert!(text.contains("select <4 x i1>"), "{text}");
    }

    #[test]
    fn simd_values_cross_function_abi() {
        assert!(run("Pair(toFloat32 1.0, toFloat32 2.0)")
            .unwrap_err()
            .contains("program result requires a scalar cell"));

        assert_eq!(
            run_f64(
                "let f = fn v: Pair[Float32] => fromFloat32 (v.x) + fromFloat32 (v.y) in \
                 f (Pair(toFloat32 1.0, toFloat32 2.0))"
            )
            .unwrap(),
            3.0
        );
        assert_eq!(
            run_f64(
                "let make = fn x: Float => Pair(x, x + 1.0) in \
                 let r = make 2.0 in r.x + r.y"
            )
            .unwrap(),
            5.0
        );
        assert_eq!(
            run_f64(
                "let id = fn v: Quad[Float] => v in \
                 let r = id (Quad(1.0, 2.0, 3.0, 4.0)) in sum r"
            )
            .unwrap(),
            10.0
        );
        assert_eq!(
            run_f64(
                "let id = fn v: Oct[Float32] => v in \
                 let r = id (Oct(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0, \
                                 toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0)) in \
                 fromFloat32 (sum r)"
            )
            .unwrap(),
            36.0
        );
        assert_eq!(
            run_f64(
                "let make = fn n: Int => Oct(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0, \
                                            toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0) in \
                 let r = make 0 in fromFloat32 (sum r)"
            )
            .unwrap(),
            36.0
        );
        assert_eq!(
            run_f64(
                "let make = fn n: Int => Oct(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0, \
                                            toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0) in \
                 let twice = fn v: Oct[Float32] => v + v in \
                 let r = twice (make 0) in fromFloat32 (sum r)"
            )
            .unwrap(),
            72.0
        );
        assert_eq!(
            run_f64(
                "let a = Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0) in \
                 let f = fn n: Int => fromFloat32 (sum a) + toFloat n in \
                 f 1"
            )
            .unwrap(),
            11.0
        );
        assert_eq!(
            run_f64(
                "let a = Pair(1.0, 2.0) in \
                 let f = fn n: Int => a in \
                 let t = (1, 2) in \
                 let r = f 0 in r.x + r.y"
            )
            .unwrap(),
            3.0
        );
        assert_eq!(
            run("let f = fn m: Mask[Pair] => if any m then 1 else 0 in \
                 f (Pair(1.0, 2.0) < 1.5)")
            .unwrap(),
            1
        );
        assert_eq!(
            run("let lt = fn a: Pair[Float] => a < 2.0 in \
                 let m = lt (Pair(1.0, 3.0)) in if any m then 1 else 0")
            .unwrap(),
            1
        );
        assert_eq!(
            run("let m = Pair(1.0, 2.0) < 1.5 in \
                 let f = fn n: Int => if any m then 1 else 0 in f 0")
            .unwrap(),
            1
        );

        assert!(run("let p = (Pair(toFloat32 1.0, toFloat32 2.0), 3) in 0")
            .unwrap_err()
            .contains("managed object field requires a scalar cell"));
        assert!(run("let a = Pair(1.0, 2.0) in a < 1.5")
            .unwrap_err()
            .contains("reduce a mask with `any`/`all`"));
    }

    #[test]
    fn vector_function_abi_emits_typed_llvm_vectors() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let make = fn n: Int => Oct(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0, \
                                         toFloat32 5.0, toFloat32 6.0, toFloat32 7.0, toFloat32 8.0) in \
             let r = make 0 in \
             fromFloat32 (sum r)",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("define <8 x float> @__locus_lam_"), "{text}");
        assert!(text.contains("call <8 x float>"), "{text}");
    }

    #[test]
    fn a_function_returning_a_quad_has_its_result_stored_and_read_back() {
        // SIMD Sprint 3, Part B — a named `fn (…) => <Quad expr>` is called and its
        // `Quad` result is *consumed*: `storeQuad`d into an output array, then a lane
        // is read back and the numbers are asserted. This is the kernel-relevant use
        // of a vector function result (the value crosses the closure return ABI as a
        // typed `<4 x float>`, then lands in array memory via a packed store).
        assert_eq!(
            run_f64(
                "let make = fn n: Int => \
                     Quad(toFloat32 10.0, toFloat32 11.0, toFloat32 12.0, toFloat32 13.0) in \
                 let out = [toFloat32 0.0, toFloat32 0.0, toFloat32 0.0, toFloat32 0.0] in \
                 let _ = storeQuad(out, 0, make 0) in \
                 fromFloat32 (out[0]) + fromFloat32 (out[1]) + fromFloat32 (out[2]) + fromFloat32 (out[3])"
            )
            .unwrap(),
            // make ⇒ (10, 11, 12, 13); sum = 46.
            46.0
        );
        // And the same returned `Quad` consumed by lane extraction (`.z`) — the
        // other honest way to use a vector result.
        assert_eq!(
            run_f64(
                "let make = fn n: Int => \
                     Quad(toFloat32 4.0, toFloat32 5.0, toFloat32 6.0, toFloat32 7.0) in \
                 let r = make 0 in fromFloat32 (r.z)"
            )
            .unwrap(),
            // make ⇒ (4, 5, 6, 7); .z = 6.
            6.0
        );
    }

    #[test]
    fn a_top_level_vector_result_is_a_clean_actionable_diagnostic() {
        // SIMD Sprint 3, Part B — the `__locus_main` top-level edge. A program whose
        // final result is a vector has no meaningful i64 process exit code, so this
        // is a clean diagnostic, NOT a forced reduction (a silent surprise) and NOT a
        // vector-as-exit-code miscompile. The message names the honest fixes: project
        // a lane, reduce a mask, sum the lanes, or storeQuad into an out-array.
        let err =
            run("Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0)").unwrap_err();
        assert!(
            err.contains("program result requires a scalar cell"),
            "the top-level vector result is rejected, not miscompiled: {err}"
        );
        assert!(
            err.contains("not a valid process exit code"),
            "the message explains WHY (a vector is not an exit code): {err}"
        );
        assert!(
            err.contains("project a lane") && err.contains("storeQuad") && err.contains("sum v"),
            "the message is actionable (names the honest reductions): {err}"
        );
        // A top-level `Pair` result is the same clean error (any vector shape).
        assert!(run("Pair(1.0, 2.0)")
            .unwrap_err()
            .contains("program result requires a scalar cell"));
    }

    #[test]
    fn jits_ordered_float_comparisons() {
        assert_eq!(run("1.0 < 2.0").unwrap(), 1);
        assert_eq!(run("2.0 < 1.0").unwrap(), 0);
        assert_eq!(run("1.0 == 1.0").unwrap(), 1);
        assert_eq!(run("let z = 0.0 / 0.0 in z == z").unwrap(), 0);
        assert_eq!(run("let z = 0.0 / 0.0 in z < 1.0").unwrap(), 0);
    }

    #[test]
    fn jits_integer_division_with_checked_codegen_path() {
        assert_eq!(run("6 / 3").unwrap(), 2);
        assert_eq!(run("(0 - 7) / 2").unwrap(), -3);
        assert_eq!(run("7 / (0 - 2)").unwrap(), -3);
    }

    #[test]
    fn jits_integer_remainder_with_checked_codegen_path() {
        assert_eq!(run("7 % 3").unwrap(), 1);
        assert_eq!(run("(0 - 7) % 2").unwrap(), -1);
        assert_eq!(run("7 % (0 - 2)").unwrap(), 1);
        assert_eq!(run("let n = 17 in n % 5").unwrap(), 2);
    }

    #[test]
    fn jits_short_circuit_bool_connectives() {
        assert_eq!(run("if true && true then 1 else 0").unwrap(), 1);
        assert_eq!(run("if true && false then 0 else 1").unwrap(), 1);
        assert_eq!(run("if false || true then 1 else 0").unwrap(), 1);
        assert_eq!(run("if ~false then 1 else 0").unwrap(), 1);
        assert_eq!(run("if ~true then 0 else 1").unwrap(), 1);
        assert_eq!(run("if ~(3 < 5) then 0 else 1").unwrap(), 1);
        assert_eq!(run("if false && ((1 / 0) == 0) then 0 else 1").unwrap(), 1);
        assert_eq!(run("if true || ((1 / 0) == 0) then 1 else 0").unwrap(), 1);
    }

    #[test]
    fn integer_division_emits_zero_and_overflow_trap() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of("let id = fn x: Int => x in (id 6) / (id 3)");
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("sdiv"),
            "division should lower to sdiv:\n{text}"
        );
        assert!(
            text.contains("llvm.trap"),
            "integer division should trap before LLVM UB cases:\n{text}"
        );
        assert!(
            text.contains("div.zero") && text.contains("div.overflow"),
            "division should guard zero and i64::MIN / -1:\n{text}"
        );
    }

    #[test]
    fn integer_remainder_emits_zero_and_overflow_trap() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of("let id = fn x: Int => x in (id 7) % (id 3)");
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("srem"),
            "remainder should lower to srem:\n{text}"
        );
        assert!(
            text.contains("llvm.trap"),
            "integer remainder should trap before LLVM UB cases:\n{text}"
        );
        assert!(
            text.contains("rem.zero") && text.contains("rem.overflow"),
            "remainder should guard zero and i64::MIN % -1:\n{text}"
        );
    }

    #[test]
    fn jits_numeric_conversions() {
        assert_eq!(run_f64("toFloat 42").unwrap(), 42.0);
        assert_eq!(run("floor (0.0 - 2.25)").unwrap(), -3);
        assert_eq!(run("round 2.6").unwrap(), 3);
        assert_eq!(run_f32("toFloat32 1.25").unwrap(), 1.25);
        assert_eq!(run_f64("fromFloat32 (toFloat32 1.25)").unwrap(), 1.25);
    }

    #[test]
    fn float_bits_survive_tuples_and_closure_captures() {
        assert_eq!(run_f64("let (a, b) = (1.5, 2.25) in a + b").unwrap(), 3.75);
        assert_eq!(
            run_f64("let x = 1.5 in let f = fn y: Float => x + y in f 2.25").unwrap(),
            3.75
        );
    }

    #[test]
    fn jits_fixed_signature_float_externs() {
        assert_eq!(
            run_f64(
                r#"let add = extern "locus_fp64_add" : Float -> Float -> Float in add 1.5 2.25"#
            )
            .unwrap(),
            3.75
        );
        assert_eq!(
            run_f64(
                r#"let add = extern "locus_fp64_add_i64" : Float -> Int -> Float in add 1.5 2"#
            )
            .unwrap(),
            3.5
        );
    }

    #[test]
    fn jits_fixed_signature_float32_externs() {
        assert_eq!(
            run_f32(
                r#"let id = extern "locus_fp32_id" : Float32 -> Float32 in id (toFloat32 1.25)"#
            )
            .unwrap(),
            1.25
        );
    }

    #[test]
    fn jits_conditionals() {
        assert_eq!(run("if 1 < 2 then 10 else 20").unwrap(), 10);
        assert_eq!(run("if 2 < 1 then 10 else 20").unwrap(), 20);
        assert_eq!(run("let x = 5 in if x == 5 then x * 2 else 0").unwrap(), 10);
        assert_eq!(run("case 2 of | 1 => 10 | 2 => 20 | _ => 30").unwrap(), 20);
        assert_eq!(run("case 9 of | 1 => 10 | 2 => 20 | _ => 30").unwrap(), 30);
        assert_eq!(
            run("let x = 2 in cond | x < 0 => 1 | x == 2 => 22 | _ => 3").unwrap(),
            22
        );
        // the else branch must see the OUTER x, not the then-branch's shadow.
        assert_eq!(
            run("let x = 99 in if 2 < 1 then (let x = 1 in x) else x").unwrap(),
            99,
            "branch-local binding must not leak across branches"
        );
    }

    #[test]
    fn jits_do_block_sugar() {
        assert_eq!(run("do { let x = 20; let y = x + 22; y }").unwrap(), 42);
        assert_eq!(run("do { 1 + 2; 40 + 2 }").unwrap(), 42);
    }

    #[test]
    fn jits_accumulator_loops() {
        assert_eq!(
            run("loop i = 0, acc = 0 while i < 100 do i + 1, acc + i else acc").unwrap(),
            4950
        );
        assert_eq!(
            run("let n = 6 in loop i = 1, acc = 1 while i < n do i + 1, acc * (i + 1) else acc")
                .unwrap(),
            720
        );
        assert_eq!(
            run("loop i = 0, acc = 0 while i < 10 do i + 1, acc + i return acc").unwrap(),
            45
        );
        assert_eq!(
            run("do { loop i = 0 while i < 3 do i + 1 endloop; 42 }").unwrap(),
            42
        );
    }

    #[test]
    fn jits_accumulator_loops_over_scalar_arrays() {
        assert_eq!(
            run("let a = [10, 20, 30, 40] in \
                 loop i = 0, acc = 0 while i < len a do i + 1, acc + a[i] else acc")
            .unwrap(),
            100
        );
        assert_eq!(
            run("let a = [1.25, 2.5, 3.75] in \
                 loop i = 0, acc = 0.0 while i < len a do i + 1, acc + a[i] else floor acc")
            .unwrap(),
            7
        );
    }

    #[test]
    fn jits_a_seal_region_runtime_transparently() {
        // `seal` / `nogc` is a *static* boundary (sealing-solution.md §5): it is
        // erased after type-checking, so a sealed program runs identically to its
        // body. The region allocates an array internally — the collector still
        // links because the inner `gc` survives the erasure — and returns a
        // scalar, so the seal removes `gc` from the outward row only.
        assert_eq!(
            run("nogc { let a = [10, 20, 30] in a[1] + a[2] }").unwrap(),
            50
        );
        // `nogc { e }` ≝ `seal gc { e }` — byte-identical runtime behaviour.
        assert_eq!(
            run("seal gc { let a = [10, 20, 30] in a[1] + a[2] }").unwrap(),
            50
        );
        // Sealing a native power (`mem`) over an allocating + mutating body runs
        // the body unchanged and returns the stored value.
        assert_eq!(
            run("seal mem { let a = [0, 0] in let _ = (a[0] <- 7) in a[0] }").unwrap(),
            7
        );
    }

    #[test]
    fn jits_loop_backed_array_stdlib_helpers() {
        assert_eq!(
            run("let a = array_make_int 4 7 in array_sum_int a").unwrap(),
            28
        );
        assert_eq!(
            run("let a = array_make 4 7 in array_sum_int a").unwrap(),
            28
        );
        assert_eq!(run("array_sum_int ([10, 20, 30, 40])").unwrap(), 100);
        assert_eq!(
            run("let a = [0, 0, 0, 0] in \
                 let _ = array_fill_int a 7 in \
                 array_sum_int a")
            .unwrap(),
            28
        );
        assert_eq!(
            run("let a = [1.25, 2.5, 3.75] in floor (array_sum_float a)").unwrap(),
            7
        );
        assert_eq!(
            run("let a = [0.0, 0.0] in \
                 let _ = array_fill_float a 1.5 in \
                 floor (array_sum_float a)")
            .unwrap(),
            3
        );
        assert_eq!(
            run("let src = [10, 20, 30, 40] in \
                 let dst = [0, 0, 0, 0, 0] in \
                 let _ = array_copy_range_int src 1 dst 2 3 in \
                 array_sum_int dst")
            .unwrap(),
            90
        );
        assert_eq!(
            run("let src = [1.25, 2.5, 3.75] in \
                 let dst = [0.0, 0.0, 0.0, 0.0] in \
                 let _ = array_copy_range_float src 0 dst 1 3 in \
                 floor (array_sum_float dst)")
            .unwrap(),
            7
        );
    }

    #[test]
    fn accumulator_loop_emits_llvm_phi_blocks() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of("loop i = 0, acc = 0 while i < 10 do i + 1, acc + i else acc");
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("loop.body"), "{text}");
        assert!(text.contains("phi i64"), "{text}");
        assert!(
            !text.contains("__locus_lam_"),
            "structured loops should not lower via lifted recursion:\n{text}"
        );
    }

    #[test]
    fn accumulator_loop_uses_raw_scalar_array_loads() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [10, 20, 30, 40] in \
             loop i = 0, acc = 0 while i < len a do i + 1, acc + a[i] else acc",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("locus_gc_scalar_fields_ptr"), "{text}");
        assert!(
            !text.contains("locus_gc_len"),
            "loop len should use the borrowed scalar-field base:\n{text}"
        );
        assert!(
            !text.contains("locus_gc_array_get_scalar_bytes"),
            "scalar array loop loads should be lowered directly:\n{text}"
        );
    }

    #[test]
    fn array_stdlib_sum_uses_raw_scalar_array_loads() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of("array_sum_int ([10, 20, 30, 40])");
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        let sum_body = llvm_function_body_containing(
            &text,
            &[
                "locus_gc_scalar_fields_ptr",
                "%loop.acc",
                "add i64 %loop.acc",
            ],
        );
        assert!(sum_body.contains("locus_gc_scalar_fields_ptr"), "{text}");
        assert!(
            !sum_body.contains("locus_gc_array_get_scalar_bytes"),
            "stdlib array_sum_int should lower through the same scalar loop fast path:\n{text}"
        );
        assert!(
            !sum_body.contains("array.index.trap"),
            "stdlib array_sum_int loop guard should cover its body access:\n{text}"
        );
    }

    #[test]
    fn array_stdlib_copy_range_uses_cached_scalar_arrays() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let src = [10, 20, 30, 40] in \
             let dst = [0, 0, 0, 0] in \
             let _ = array_copy_range_int src 1 dst 0 3 in \
             array_sum_int dst",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("locus_gc_scalar_fields_ptr"), "{text}");
        assert!(
            !text.contains("locus_gc_array_get_scalar_bytes"),
            "stdlib array_copy_range_int should use borrowed scalar payloads:\n{text}"
        );
        assert!(
            text.contains("array.index.trap"),
            "offset copy ranges are still locally bounds-checked until range proofs land:\n{text}"
        );
    }

    #[test]
    fn guarded_accumulator_loop_elides_redundant_array_bounds_check() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [10, 20, 30, 40] in \
             loop i = 0, acc = 0 while i < len a do i + 1, acc + a[i] else acc",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            !text.contains("array.index.trap"),
            "loop guard should cover the body array access:\n{text}"
        );
        assert!(
            !text.contains("llvm.trap"),
            "no body-local bounds trap expected for the guarded access:\n{text}"
        );
    }

    #[test]
    fn unproved_accumulator_loop_keeps_array_bounds_check() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [10, 20, 30, 40] in \
             let start = 0 - 1 in \
             loop i = start, acc = 0 while i < len a do i + 1, acc + a[i] else acc",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("array.index.trap"),
            "negative initial index must keep the body bounds trap:\n{text}"
        );
    }

    #[test]
    fn shadowed_loop_index_keeps_array_bounds_check() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [10, 20, 30, 40] in \
             loop i = 0, acc = 0 while i < len a do \
               i + 1, (let i = 0 - 1 in acc + a[i]) \
             else acc",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("array.index.trap"),
            "a locally shadowed index is not the proven induction variable:\n{text}"
        );
    }

    #[test]
    fn accumulator_loop_uses_raw_scalar_array_stores() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [0, 0, 0, 0] in \
             loop i = 0 while i < len a do \
               (let _ = a[i] <- i * 10 in i + 1) \
             else ()",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("locus_gc_scalar_fields_ptr"), "{text}");
        assert!(
            text.contains("array.elem.ptr"),
            "scalar array stores in guarded loops should use the borrowed payload:\n{text}"
        );
        assert!(
            !text.contains("array.index.trap"),
            "loop guard should cover the body array store:\n{text}"
        );
    }

    #[test]
    fn unproved_accumulator_loop_store_keeps_array_bounds_check() {
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let a = [0, 0, 0, 0] in \
             let start = 0 - 1 in \
             loop i = start while i < len a do \
               (let _ = a[i] <- i in i + 1) \
             else ()",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(
            text.contains("array.index.trap"),
            "unproved array stores must retain the local bounds trap:\n{text}"
        );
    }

    #[test]
    fn accumulator_loop_stores_scalar_arrays() {
        assert_eq!(
            run("let a = [0, 0, 0, 0] in \
                 let _ = loop i = 0 while i < len a do \
                   (let _ = a[i] <- i * 10 in i + 1) \
                 else () in \
                 a[0] + a[1] + a[2] + a[3]")
            .unwrap(),
            60
        );
    }

    #[test]
    fn accumulator_loop_stores_packed_float32_arrays() {
        assert_eq!(
            run("let a = [toFloat32 0.0, toFloat32 0.0] in \
                 let _ = loop i = 0 while i < len a do \
                   (let _ = a[i] <- toFloat32 1.5 in i + 1) \
                 else () in \
                 floor (fromFloat32 a[0] + fromFloat32 a[1])")
            .unwrap(),
            3
        );
    }

    #[test]
    fn accumulator_loop_allows_allocation_in_scalar_steps() {
        assert_eq!(
            run("loop i = 0 while i < 2 do (let t = (i, i) in i + 1) else i").unwrap(),
            2
        );
        assert_eq!(
            run("loop i = 0 while (let t = (i, i) in i < 2) do i + 1 else i").unwrap(),
            2
        );
    }

    #[test]
    fn accumulator_loop_allows_temporary_handles_in_scalar_steps() {
        assert_eq!(
            run("let p = (1, (2, 3)) in \
                 loop i = 0 while i < 2 do (let (x, q) = p in i + x) else i")
            .unwrap(),
            2
        );
        assert_eq!(
            run(r#"loop i = 0, total = 0 while i < 3 do
                     i + 1,
                     (let s = string_repeat "x" (i + 1) in total + string_len s)
                   else total"#)
            .unwrap(),
            6
        );
    }

    #[test]
    fn accumulator_loop_allows_handle_accumulators() {
        assert_eq!(
            run("loop xs = [1], i = 0 while i < 3 do [1, 2], i + 1 else len xs").unwrap(),
            2
        );
        assert_eq!(
            run(
                r#"loop s = "", i = 0 while i < 4 do string_append s "x", i + 1 else string_len s"#
            )
            .unwrap(),
            4
        );
        assert_eq!(
            run(r#"let s = loop s = "", i = 0 while i < 3 do
                     string_append s "x",
                     i + 1
                   else s in string_len s"#)
            .unwrap(),
            3
        );
        assert_eq!(
            run(r#"let idloop = fn s: String =>
                     loop t = s, i = 0 while i < 1 do t, i + 1 else t
                   in string_len (idloop "abc")"#)
            .unwrap(),
            3
        );
    }

    #[test]
    fn accumulator_loop_handle_steps_remain_parallel() {
        assert_eq!(
            run(r#"loop s = "a", seen = 0 while seen < 1 do
                     string_append s "b",
                     string_len s
                   else seen"#)
            .unwrap(),
            1
        );
    }

    #[test]
    fn jits_functions_and_closures() {
        // identity, and a function that computes
        assert_eq!(run("let id = fn x: Int => x in id 5").unwrap(), 5);
        assert_eq!(run("let sq = fn x: Int => x * x in sq 9").unwrap(), 81);
        // capture: addy closes over y
        assert_eq!(
            run("let y = 10 in let addy = fn x: Int => x + y in addy 5").unwrap(),
            15
        );
        // composition: twice closes over inc (another closure)
        assert_eq!(
            run("let inc = fn x: Int => x + 1 in let twice = fn n: Int => inc (inc n) in twice 40")
                .unwrap(),
            42,
        );
    }

    #[test]
    fn jits_recursion() {
        // factorial — a closure that captures itself
        let fact = "let rec fact : Int -> Int = \
                    fn n: Int => if n < 1 then 1 else n * fact (n - 1) in fact 5";
        assert_eq!(run(fact).unwrap(), 120);
        // fibonacci — two recursive calls
        let fib = "let rec fib : Int -> Int = \
                   fn n: Int => if n < 2 then n else fib (n - 1) + fib (n - 2) in fib 10";
        assert_eq!(run(fib).unwrap(), 55);
    }

    #[test]
    fn jits_direct_self_tail_recursion_without_stack_growth() {
        let countdown = "let rec go : Int -> Int = \
                         fn n: Int => if n < 1 then 7 else go (n - 1) in go 1000000";
        assert_eq!(run(countdown).unwrap(), 7);

        let ctx = inkwell::context::Context::create();
        let ir = ir_of(
            "let rec go : Int -> Int = \
             fn n: Int => if n < 1 then 7 else go (n - 1) in go 5",
        );
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains("tail.loop"), "{text}");
        assert!(text.contains("br label %tail.loop"), "{text}");
    }

    #[test]
    fn jits_a_win32_call() {
        // GetCurrentProcess() → the pseudo-handle (HANDLE)-1, deterministic.
        let h = r#"let h = extern "GetCurrentProcess" : Unit -> Int in h ()"#;
        assert_eq!(run(h).unwrap(), -1, "current-process pseudo-handle");
        // GetTickCount64() → ms since boot, always positive once running.
        let t = r#"let now = extern "GetTickCount64" : Unit -> Int in now ()"#;
        assert!(run(t).unwrap() > 0, "tick count should be positive");
    }

    #[test]
    fn jits_a_multi_arg_win32_call() {
        // MulDiv(a, b, c) = (a*b)/c, rounded — a three-argument kernel32 call
        // with 32-bit (`int`) parameters and return. Proves the spine reaches
        // the call as N args, AND that the sized-int boundary is correct: args
        // truncate to i32, and the i32 result sign-extends back to i64 — so the
        // value is exact with no `as i32` masking.
        let m = r#"let muldiv = extern "MulDiv" : I32 -> I32 -> I32 -> I32 in muldiv 10 7 2"#;
        assert_eq!(run(m).unwrap(), 35, "MulDiv(10, 7, 2) = 70 / 2");
        // a negative result exercises the sign-extension specifically.
        let n = r#"let muldiv = extern "MulDiv" : I32 -> I32 -> I32 -> I32 in muldiv (0 - 6) 4 3"#;
        assert_eq!(
            run(n).unwrap(),
            -8,
            "MulDiv(-6, 4, 3) = -24/3; i32 result sign-extends"
        );
    }

    #[test]
    fn managed_strings_reach_stdlib_through_closures() {
        // A String flows through a closure unchanged, then out to a Win32 …W API
        // (there is no console runtime now — output is the `console_writeln` prelude).
        // lstrlenW counts UTF-16 code units.
        let through = r#"let id = fn x: String => x in string_len (id "world")"#;
        assert_eq!(
            run(through).unwrap(),
            5,
            "\"world\" through a closure = 5 units"
        );
        // 🎉 (U+1F389) is a surrogate PAIR — 2 UTF-16 units; proves the wide
        // representation spans the full 21-bit range, not just ASCII/BMP.
        let emoji = "string_len \"\u{1f389}\"";
        assert_eq!(run(emoji).unwrap(), 2, "surrogate pair = 2 units");
    }

    #[test]
    fn managed_string_search_helpers_run() {
        assert_eq!(
            run(r#"if string_starts_with "hello" "he" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_ends_with "hello" "lo" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(run(r#"string_find_from "banana" "na" 3"#).unwrap(), 4);
        assert_eq!(run(r#"string_last_find "banana" "na""#).unwrap(), 4);
        assert_eq!(run(r#"string_count "aaaa" "aa""#).unwrap(), 2);
        assert_eq!(run(r#"string_find_from "abc" "" 9"#).unwrap(), 3);
        assert_eq!(run(r#"string_last_find "abc" """#).unwrap(), 3);
        assert_eq!(
            run(r#"if string_contains_at "banana" "nan" 2 then 1 else 0"#).unwrap(),
            1
        );
    }

    #[test]
    fn managed_string_allocating_helpers_run() {
        assert_eq!(run("string_len (string_empty ())").unwrap(), 0);
        assert_eq!(
            run(r#"if string_equals (string_singleton 65) "A" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_equals (string_slice "abcdef" 2 3) "cde" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_equals (string_slice "abc" (0 - 2) 2) "ab" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_equals (string_take "abcdef" 2) "ab" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_equals (string_drop "abcdef" 4) "ef" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_equals (string_append "ab" "cd") "abcd" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_equals (string_repeat "ab" 3) "ababab" then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(run(r#"string_len (string_repeat "x" (0 - 1))"#).unwrap(), 0);
    }

    #[test]
    fn agent_text_channel_runs_through_runtime_session() {
        let src = r#"let answer = agent_ask_text "move?" in
                    let _ = agent_tell_text answer in
                    if string_equals answer "d3" then string_len answer else 0"#;
        let (value, transcript) = run_agent_text(src, vec!["d3".into()]).unwrap();
        assert_eq!(value, 2);
        assert_eq!(transcript.remaining_responses, 0);
        assert_eq!(
            transcript.events,
            vec![
                crate::runtime::AgentEvent::Ask {
                    prompt: "move?".into(),
                    response: "d3".into(),
                    used_default: false,
                },
                crate::runtime::AgentEvent::Tell { text: "d3".into() },
            ]
        );
    }

    #[test]
    fn hard_othello_agent_replay_survives_two_black_replies() {
        let src = include_str!("../../examples/othello_for_agents_hard.locus");
        let (value, transcript) = run_agent_text(src, vec!["2,3".into(), "2,1".into()]).unwrap();
        assert!(
            transcript.events.len() > 50,
            "expected a multi-turn transcript, got {:?}",
            transcript.events
        );
        assert!(value > 0, "encoded score should be positive");
    }

    #[test]
    fn minesweeper_agent_replay_survives_safe_reveals() {
        let src = include_str!("../../examples/minesweeper_for_agents.locus");
        let (value, transcript) = run_agent_text(
            src,
            vec!["5,5".into(), "5,3".into(), "0,0".into(), "flag 0,1".into()],
        )
        .unwrap();
        assert!(
            transcript.events.len() > 40,
            "expected a multi-turn transcript, got {:?}",
            transcript.events
        );
        assert!(
            transcript
                .events
                .iter()
                .any(|event| matches!(event, crate::runtime::AgentEvent::Ask { .. })),
            "expected at least one agent ask"
        );
        assert!(
            transcript.events.iter().any(|event| matches!(
                event,
                crate::runtime::AgentEvent::Tell { text }
                    if text.contains("status: mines=")
                        && text.contains("unflagged_hidden=")
                        && text.contains("safe_remaining=")
            )),
            "expected status text to include agent-useful mine/hidden/safe counts"
        );
        assert!(value > 0, "safe replay should not hit a mine");
    }

    #[test]
    fn mastermind_agent_replay_solves_with_duplicate_feedback() {
        let src = include_str!("../../examples/mastermind_for_agents.locus");
        let (value, transcript) = run_agent_text(src, vec!["1234".into(), "5325".into()]).unwrap();
        assert_eq!(value, 1002, "expected solve on turn two");
        assert_eq!(transcript.remaining_responses, 0);
        assert!(
            transcript.events.iter().any(|event| matches!(
                event,
                crate::runtime::AgentEvent::Tell { text }
                    if text == "feedback: guess=1234 exact=0 misplaced=2 remaining=7"
            )),
            "expected duplicate-aware feedback for the exploratory guess"
        );
        assert!(
            transcript.events.iter().any(|event| matches!(
                event,
                crate::runtime::AgentEvent::Tell { text }
                    if text == "solved in 2 turns. secret=5325"
            )),
            "expected final solved tell with deterministic secret"
        );
    }

    #[test]
    fn managed_string_traits_run() {
        assert_eq!(run(r#"if string_eq "a" "a" then 1 else 0"#).unwrap(), 1);
        assert_eq!(
            run(r#"if string_ordering "a" "b" < 0 then 1 else 0"#).unwrap(),
            1
        );
        assert_eq!(
            run(r#"if string_equals (string_show "hi") "hi" then 1 else 0"#).unwrap(),
            1
        );
    }

    #[test]
    fn managed_strings_do_not_pass_as_raw_ptrs() {
        // A Locus `String` is a managed UTF-16 array handle, not an LPCWSTR.
        // Boundary code must copy it into a raw buffer before passing `Ptr`.
        let s = r#"let wlen = extern "lstrlenW" : Ptr -> I32 in wlen "hello""#;
        assert!(run(s).is_err(), "String should not coerce to Ptr");
    }

    #[test]
    fn jits_console_writeln_from_the_prelude() {
        // Output is now ordinary Locus — `console_writeln` (the prelude, readable over
        // Win32), grafted in by `program`. No native console: it JITs and runs,
        // writing to stdout, and yields Unit.
        assert_eq!(
            run(r#"console_writeln "hello from JIT""#).unwrap(),
            0,
            "console_writeln : String -> Unit"
        );
    }

    #[test]
    fn jits_console_write_float_from_the_prelude() {
        assert_eq!(
            run("console_write_float (1.5 + 2.25)").unwrap(),
            0,
            "console_write_float : Float -> Unit"
        );
    }

    #[test]
    fn jits_monotonic_clock_service() {
        let src = "let start = clock_ticks () in \
                   let freq = clock_frequency () in \
                   let elapsed = elapsed_ticks start in \
                   if 0 < freq then (if elapsed < 0 then 0 else 1) else 0";
        assert_eq!(run(src).unwrap(), 1, "Time service resolves through Win32");
    }

    #[test]
    fn jits_a_tail_resumptive_handler() {
        // A handler INTERCEPTS `console`: it resumes with unit (so the perform
        // yields nothing) and the `return` clause produces the value. The effect
        // is discharged — the program is pure, prints nothing, and is 42.
        let src = "handle perform console \"x\" with { \
                   console(s) => resume () ; return(y) => 42 }";
        assert_eq!(
            run(src).unwrap(),
            42,
            "the return clause runs after a tail resume"
        );
    }

    #[test]
    fn jits_an_abort_handler() {
        // Abort = exception: `perform fail ()` jumps out with 7, skipping the
        // `z + 100` continuation AND the return clause. The effect is discharged.
        let src = "effect fail : Unit -> Int in \
                   handle (let z = perform fail () in z + 100) with { \
                   fail(x) => 7 ; return(y) => y }";
        assert_eq!(run(src).unwrap(), 7, "abort skips the continuation");
    }

    #[test]
    fn jits_bitwise_ops() {
        // The five kernel bitwise primitives, on the uniform i64 value.
        assert_eq!(run("0xF0 & 0x3F").unwrap(), 0x30, "and");
        assert_eq!(run("0xF0 | 0x0F").unwrap(), 0xFF, "or");
        assert_eq!(run("0xFF ^ 0x0F").unwrap(), 0xF0, "xor");
        assert_eq!(run("1 << 4").unwrap(), 16, "left shift");
        assert_eq!(run("0x100 >> 4").unwrap(), 16, "right shift");
        // `>>` is ARITHMETIC (sign-preserving), as for any signed i64: -8>>1=-4.
        assert_eq!(
            run("(0 - 8) >> 1").unwrap(),
            -4,
            "arithmetic >> preserves sign"
        );
    }

    #[test]
    fn jits_utf8_byte_math() {
        // The arithmetic core of the UTF-16→UTF-8 converter, PURE (! {}): the
        // third continuation byte of 🎉 (U+1F389) is 0x80 | ((cp >> 6) & 0x3F).
        // Exercises >>, &, |, and hex literals together. = 0x8E.
        let b2 = "let cp = 0x1F389 in 0x80 | ((cp >> 6) & 0x3F)";
        assert_eq!(run(b2).unwrap(), 0x8E, "third UTF-8 byte of 🎉");
        // The lead byte of a 2-byte sequence: é (U+00E9) → 0xC0 | (cp >> 6) = 0xC3.
        let lead = "let cp = 0xE9 in 0xC0 | (cp >> 6)";
        assert_eq!(run(lead).unwrap(), 0xC3, "lead byte of é");
    }

    #[test]
    fn jits_peek_and_poke() {
        // The `mem` capability, scalar half: allocate a writable page (the
        // address arrives as an `Int`), poke two bytes, peek them back. Proves
        // inttoptr + store/load round-trips through real memory. 65 + 66 = 131.
        let src = "let alloc = extern \"VirtualAlloc\" : Int -> Int -> I32 -> I32 -> Int in \
                   let buf = alloc 0 64 0x3000 0x04 in \
                   let _ = poke8 buf 65 in \
                   let _ = poke8 (buf + 1) 66 in \
                   (peek8 buf) + (peek8 (buf + 1))";
        assert_eq!(
            run(src).unwrap(),
            131,
            "poke then peek round-trips through memory"
        );
    }

    #[test]
    fn jits_fill_and_copy() {
        // The bulk half: fill 4 bytes with 'A', memmove them up 4, overwrite one
        // with 'B', read two back. Exercises memset + memmove. 65 + 66 = 131.
        let src = "let alloc = extern \"VirtualAlloc\" : Int -> Int -> I32 -> I32 -> Int in \
                   let buf = alloc 0 64 0x3000 0x04 in \
                   let _ = fill buf 65 4 in \
                   let _ = copy (buf + 4) buf 4 in \
                   let _ = poke8 (buf + 5) 66 in \
                   (peek8 (buf + 4)) + (peek8 (buf + 5))";
        assert_eq!(
            run(src).unwrap(),
            131,
            "fill + copy move bytes through memory"
        );
    }

    #[test]
    fn peek16_reads_a_wide_unit() {
        // `peek16` reads one UTF-16 code unit. A wide `Str` literal is laid out
        // as u16s; its first unit is the address's low half. We can't take a
        // Str's address arithmetically yet (next slice), but we CAN allocate,
        // poke a 16-bit value, and read it back at width 16 — proving the width
        // dispatch (i16 load, zero-extended).
        let src = "let alloc = extern \"VirtualAlloc\" : Int -> Int -> I32 -> I32 -> Int in \
                   let buf = alloc 0 64 0x3000 0x04 in \
                   let _ = poke16 buf 0x1F389 in \
                   peek16 buf";
        // 0x1F389 truncates to 16 bits on the way in (0xF389), reads back zero-
        // extended → 0xF389 = 62345.
        assert_eq!(
            run(src).unwrap(),
            0xF389,
            "poke16/peek16 round-trip, truncated + zero-extended"
        );
    }

    #[test]
    fn array_read_deconstructs_a_wide_string() {
        // `s[i]` reads the i-th UTF-16 unit of a String — deconstructing it at
        // the mem level (the high-level value stays immutable; the access is
        // `! {mem}`). `Str` indexes as 16-bit units, so no manual `* 2`.
        assert_eq!(run(r#""hello"[0]"#).unwrap(), 104, "'h'");
        assert_eq!(run(r#""hello"[1]"#).unwrap(), 101, "'e'");
        // `s[i]` yields raw UNITS, not characters: 🎉 (U+1F389) is a surrogate
        // PAIR — proof the accessor is honestly low-level.
        assert_eq!(run("\"\u{1f389}\"[0]").unwrap(), 0xD83C, "high surrogate");
        assert_eq!(run("\"\u{1f389}\"[1]").unwrap(), 0xDF89, "low surrogate");
    }

    #[test]
    fn array_store_constructs_a_byte_buffer() {
        // `buf[j] <- v` writes the j-th byte of an Int buffer (byte-indexed), and
        // `buf[j]` reads it back — constructing then deconstructing at mem level.
        let src = "let alloc = extern \"VirtualAlloc\" : Int -> Int -> I32 -> I32 -> Int in \
                   let buf = alloc 0 64 0x3000 0x04 in \
                   let _ = buf[0] <- 72 in \
                   let _ = buf[1] <- 73 in \
                   buf[0] + buf[1]";
        assert_eq!(run(src).unwrap(), 145, "'H' + 'I' written then read back");
    }

    #[test]
    fn array_accessor_is_just_peek_poke() {
        // The subscript desugars to the SAME memory the raw primitives touch:
        // poke a byte, read it via the accessor and vice-versa — they agree.
        let src = "let alloc = extern \"VirtualAlloc\" : Int -> Int -> I32 -> I32 -> Int in \
                   let buf = alloc 0 64 0x3000 0x04 in \
                   let _ = poke8 (buf + 3) 99 in \
                   buf[3]";
        assert_eq!(
            run(src).unwrap(),
            99,
            "buf[3] reads what poke8 (buf+3) wrote"
        );
    }

    #[test]
    fn converts_utf16_to_utf8() {
        // The whole point — a real Unicode transcoder written in Locus, verified
        // BYTE for byte against the UTF-8 standard. mem accessor + bitwise +
        // effectful recursion, nothing else.
        // "é" (U+00E9) → C3 A9 — a 2-byte sequence.
        assert_eq!(
            run(&convert("é", "out[0] * 256 + out[1]")).unwrap(),
            0xC3A9,
            "é = C3 A9"
        );
        // "世" (U+4E16) → E4 B8 96 — a 3-byte sequence.
        let three = "(out[0] * 256 + out[1]) * 256 + out[2]";
        assert_eq!(
            run(&convert("世", three)).unwrap(),
            0xE4B896,
            "世 = E4 B8 96"
        );
        // "🎉" (U+1F389) → F0 9F 8E 89 — surrogate DECODE then 4-byte encode. The
        // hard path: two UTF-16 units combined into one astral code point.
        assert_eq!(run(&convert("🎉", "n")).unwrap(), 4, "🎉 is 4 UTF-8 bytes");
        let four = "((out[0] * 256 + out[1]) * 256 + out[2]) * 256 + out[3]";
        assert_eq!(
            run(&convert("🎉", four)).unwrap(),
            0xF09F8E89,
            "🎉 = F0 9F 8E 89"
        );
        // a mixed string: the example's exact byte count (7 ASCII + 3 + 3 + 1 + 4).
        assert_eq!(
            run(&convert("Hello, 世界 🎉", "n")).unwrap(),
            18,
            "mixed-script byte count"
        );
    }

    #[test]
    fn jits_a_oneshot_continuation() {
        // `resume` called ONCE, but not in tail position — its result is used.
        // The continuation is the return clause (identity here): resume 41 runs
        // it to get 41, then `+ 1` = 42. A reified (captured) continuation.
        let src = "effect ask : Unit -> Int in \
                   handle perform ask () with { ask(x) => resume 41 + 1 ; return(y) => y }";
        assert_eq!(
            run(src).unwrap(),
            42,
            "one-shot: resume, then compute with its result"
        );
    }

    #[test]
    fn jits_a_multishot_continuation() {
        // `resume` called TWICE — the continuation runs once per call. `choose`
        // resumes with 1 and with 2; the return clause multiplies each by 10:
        // (1*10) + (2*10) = 30. Multi-shot, pay-as-you-go (a heap closure called
        // twice). This is the case no inlining can express.
        let src = "effect choose : Unit -> Int in \
                   handle perform choose () with { \
                   choose(x) => resume 1 + resume 2 ; return(y) => y * 10 }";
        assert_eq!(
            run(src).unwrap(),
            30,
            "multi-shot: resume twice, return runs each time"
        );
    }

    #[test]
    fn reified_continuations_handle_capture_and_repeat() {
        // Adversarial checks on the reified path — easy to get subtly wrong.
        // (1) the continuation (the return clause) CLOSES OVER an outer variable.
        let cap = "let n = 5 in effect choose : Unit -> Int in \
                   handle perform choose () with { \
                   choose(x) => resume 1 + resume 2 ; return(y) => y + n }";
        assert_eq!(
            run(cap).unwrap(),
            13,
            "continuation closes over n=5: (1+5)+(2+5)"
        );
        // (2) resume called THREE times — the closure runs three times.
        let three = "effect choose : Unit -> Int in \
                     handle perform choose () with { \
                     choose(x) => resume 1 + resume 2 + resume 3 ; return(y) => y }";
        assert_eq!(run(three).unwrap(), 6, "resume x3: 1+2+3");
        // (3) the op ARGUMENT is bound, and reused as the resume value.
        let arg = "effect dbl : Int -> Int in \
                   handle perform dbl 7 with { dbl(x) => resume x + resume x ; return(y) => y }";
        assert_eq!(
            run(arg).unwrap(),
            14,
            "x=7 is the perform arg; resume x + resume x"
        );
        // (4) resume results combined multiplicatively, with a non-trivial return.
        let prod = "effect choose : Unit -> Int in \
                    handle perform choose () with { \
                    choose(x) => resume 2 * resume 3 ; return(y) => y + 1 }";
        assert_eq!(run(prod).unwrap(), 12, "(2+1)*(3+1)");
    }

    #[test]
    fn reified_continuations_span_a_larger_computation() {
        // Selective CPS: the continuation now captures work AFTER the perform, not
        // just the return clause. `let x = perform choose () in x * 10`, resumed
        // with 1 and 2 → (1*10) + (2*10) = 30. (Was a "single perform" error.)
        let span = "effect choose : Unit -> Int in \
                    handle (let x = perform choose () in x * 10) with { \
                    choose(p) => resume 1 + resume 2 ; return(z) => z }";
        assert_eq!(
            run(span).unwrap(),
            30,
            "continuation captures `x * 10` after the perform"
        );
        // one-shot, with work both after the perform AND after the resume:
        // continuation = λx. x + 100; resume 5 = 105; then * 2 = 210.
        let one = "effect a : Unit -> Int in \
                   handle (let x = perform a () in x + 100) with { \
                   a(p) => resume 5 * 2 ; return(z) => z }";
        assert_eq!(run(one).unwrap(), 210, "(5 + 100) * 2");
        // a PURE conditional in the continuation passes through: resume 1 → 70,
        // resume 2 → 80, sum 150.
        let cond = "effect a : Unit -> Int in \
                    handle (let x = perform a () in if x == 1 then 70 else 80) with { \
                    a(p) => resume 1 + resume 2 ; return(z) => z }";
        assert_eq!(
            run(cond).unwrap(),
            150,
            "pure branch inside the continuation"
        );
        // TWO sequential performs, each resumed once: continuation chains.
        let two = "effect ab { a : Unit -> Int ; b : Unit -> Int } in \
                   handle (let x = perform a () in let y = perform b () in x + y) with { \
                   a(p) => resume 10 + resume 20 ; b(q) => resume 100 ; return(z) => z }";
        // a resumes twice: each runs `let y = perform b () in x+y` (b → 100), so
        // resume 10 = 10+100 = 110, resume 20 = 20+100 = 120; 110 + 120 = 230.
        assert_eq!(
            run(two).unwrap(),
            230,
            "multi-shot a, each continuation performs b"
        );
    }

    /// Wrap `body` in the parameterized (state-passing) `State` handler, applied
    /// to the initial state 0. The handled computation becomes `Int -> Int`.
    fn state_prog(body: &str) -> String {
        "effect State { get : Unit -> Int ; put : Int -> Unit } in \
         (handle (BODY) with { \
            get(u)  => fn s: Int => resume s s ; \
            put(s2) => fn s: Int => resume () s2 ; \
            return(v) => fn s: Int => v }) 0"
            .replace("BODY", body)
    }

    #[test]
    fn jits_state_from_pure_handlers() {
        // Mutable STATE with no mutable cell — the canonical effect-handlers
        // payoff. Each `get`/`put` threads state through `resume`; the whole
        // machinery is multi-perform reified continuations with the answer type
        // modified to `Int -> Int`. The fact this runs (and is correct) exercises
        // selective CPS, continuation closures, and capture, end to end.
        let one = "let a = perform get () in let r = perform put (a + 1) in perform get ()";
        assert_eq!(run(&state_prog(one)).unwrap(), 1, "get→0, put(1), get→1");
        let twice = "let a = perform get () in let r1 = perform put (a + 1) in \
                     let b = perform get () in let r2 = perform put (b + 1) in perform get ()";
        assert_eq!(run(&state_prog(twice)).unwrap(), 2, "two increments from 0");
        let readadd = "let a = perform get () in let r = perform put (a + 100) in \
                       let b = perform get () in a + b";
        assert_eq!(run(&state_prog(readadd)).unwrap(), 100, "a(0) + b(100)");
    }

    #[test]
    fn a_continuation_across_a_closure_is_a_clear_error() {
        // The effect performed INSIDE a closure (called later) — not a syntactic
        // perform in the scrutinee, so the continuation can't be captured. A clear
        // error, not a crash or a miscompile.
        let src = "effect a : Unit -> Int in \
                   handle (let f = fn u: Unit => perform a () in f ()) with { \
                   a(p) => resume 1 + resume 2 ; return(y) => y }";
        assert!(run(src).unwrap_err().contains("later slice"));
    }

    #[test]
    fn jits_tuples() {
        // Products: the first compound data type. Construction, destructuring,
        // n-ary, nested, heterogeneous, and through functions.
        assert_eq!(
            run("let (a, b) = (3, 4) in a * 10 + b").unwrap(),
            34,
            "destructure"
        );
        assert_eq!(
            run("let (a, b) = (1, 2) in let (c, d) = (b, a) in c * 10 + d").unwrap(),
            21,
            "swap"
        );
        assert_eq!(
            run("let (x, y, z) = (5, 6, 7) in x + y + z").unwrap(),
            18,
            "3-tuple"
        );
        assert_eq!(
            run("let (p, c) = ((1, 2), 3) in let (a, b) = p in a + b + c").unwrap(),
            6,
            "nested"
        );
        // a function returns two values at once.
        assert_eq!(
            run("let pair = fn n: Int => (n, n + 1) in let (a, b) = pair 10 in a * 100 + b")
                .unwrap(),
            1011,
            "fn returns a tuple"
        );
        // heterogeneous elements: (Int, Bool).
        assert_eq!(
            run("let (n, flag) = (7, true) in if flag then n else 0").unwrap(),
            7,
            "mixed types"
        );
    }

    #[test]
    fn jits_arrays() {
        // Dynamic heap objects — what the GC is for. Construction, indexing, len.
        assert_eq!(
            run("let a = [10, 20, 30] in a[0] + a[2] + len a").unwrap(),
            43,
            "read + len"
        );
        assert_eq!(run("let a = [5, 6, 7, 8] in len a").unwrap(), 4, "length");
        // A computed index expression.
        assert_eq!(
            run("let a = [1, 2, 3, 4, 5] in a[2 + 1]").unwrap(),
            4,
            "computed index"
        );
        // MUTATION — the headline difference from tuples. `a[1] <- 99` updates in
        // place; the store yields Unit, so we sequence it with a `let`.
        assert_eq!(
            run("let a = [1, 2, 3] in let done = a[1] <- 99 in a[0] + a[1] + a[2]").unwrap(),
            103,
            "mutate in place",
        );
        // An array of arrays — reference elements, traced through the collector.
        assert_eq!(
            run("let a = [[1, 2], [3, 4]] in let r = a[1] in r[0] + r[1]").unwrap(),
            7,
            "nested arrays"
        );
        // An array built and returned by a function (escapes as a handle).
        assert_eq!(
            run("let f = fn n: Int => [n, n * 2, n * 3] in let a = f 5 in a[0] + a[1] + a[2]")
                .unwrap(),
            30,
            "fn returns an array",
        );
    }

    #[test]
    fn jits_float_arrays() {
        assert_eq!(
            run("let a = [1.25, 2.5] in let _ = a[0] <- 3.5 in floor (a[0] + a[1]) + len a")
                .unwrap(),
            8,
            "Float arrays use logical length plus scalar payload"
        );
    }

    #[test]
    fn jits_float32_arrays_with_packed_stride() {
        assert_eq!(
            run_f64(
                "let a = [toFloat32 1.25, toFloat32 2.5, toFloat32 3.75] in \
                 let _ = a[1] <- toFloat32 4.5 in \
                 fromFloat32 a[0] + fromFloat32 a[1] + fromFloat32 a[2]"
            )
            .unwrap(),
            9.5
        );
    }

    #[test]
    fn jits_sum_types() {
        // Constructors + match + field binding — the building blocks for List/Tree/Option.
        assert_eq!(
            run("type T = A | B(Int) in match B(5) with | A => 0 | B(x) => x + 100").unwrap(),
            105
        );
        // Option, both arms reached.
        assert_eq!(
            run("type Opt = None | Some(Int) in match Some(7) with | None => 0 | Some(x) => x")
                .unwrap(),
            7
        );
        assert_eq!(
            run("type Opt = None | Some(Int) in match None with | None => 42 | Some(x) => x")
                .unwrap(),
            42
        );
        // A multi-field constructor binds positionally.
        assert_eq!(
            run("type Shape = Circle(Int) | Rect(Int, Int) in \
                 match Rect(3, 4) with | Circle(r) => r | Rect(w, h) => w * h")
            .unwrap(),
            12,
        );
        // RECURSIVE: a linked list, summed with a recursive match.
        assert_eq!(
            run("type List = Nil | Cons(Int, List) in \
                 let rec sum : List -> Int ! {gc} = fn l: List => \
                   match l with | Nil => 0 | Cons(h, t) => h + sum t in \
                 sum (Cons(10, Cons(20, Cons(30, Nil))))")
            .unwrap(),
            60,
        );
        // A wildcard catch-all.
        assert_eq!(
            run("type C = R | G | B in match G with | R => 1 | _ => 99").unwrap(),
            99
        );
    }

    #[test]
    fn a_non_exhaustive_match_is_an_error() {
        assert!(
            run("type Opt = None | Some(Int) in match Some(1) with | Some(x) => x").is_err(),
            "omitting None must be rejected",
        );
    }

    #[test]
    fn gc_closure_captures_a_tuple() {
        // A closure capturing a TUPLE (a GC handle, not a scalar) — the
        // hard case for Regime 1. Typed capture layout stores the tuple in a
        // pointer field and loads it back as a fresh handle inside the closure.
        assert_eq!(
            run("let t = (10, 20) in let f = fn x: Int => let (a, b) = t in a + b + x in f 5")
                .unwrap(),
            35,
            "closure reads a captured tuple's fields",
        );
        // Two closures capturing two different tuples, both reached at call time.
        assert_eq!(
            run("let p = (3, 4) in let q = (5, 6) in \
                 let f = fn z: Int => let (a, b) = p in let (c, d) = q in a + b + c + d + z in \
                 f 0")
            .unwrap(),
            18,
            "two captured tuples",
        );
        // A captured tuple AND a captured scalar in the same closure (mixed cells).
        assert_eq!(
            run("let t = (100, 20) in let k = 3 in \
                 let f = fn _u: Int => let (a, b) = t in a + b + k in f 0")
            .unwrap(),
            123,
            "mixed handle + scalar captures",
        );
    }

    #[test]
    fn gc_closure_captures_full_width_scalar_bits() {
        // The tuple forces the managed-heap closure path. The captured scalar
        // uses a top-bit pattern that the old fixnum capture encoding lost.
        let src = "let anchor = (0, 0) in \
                   let hi = 1 << 62 in \
                   let f = fn _u: Int => hi in \
                   let (a, b) = anchor in f 0";
        assert_eq!(run(src).unwrap(), 1_i64 << 62);
    }

    #[test]
    fn gc_closure_captures_mixed_pointer_and_full_width_scalar() {
        let src = "let t = (10, 20) in \
                   let hi = 1 << 62 in \
                   let f = fn z: Int => \
                     let (a, b) = t in \
                     if hi == (1 << 62) then a + b + z else 999 \
                   in f 12";
        assert_eq!(run(src).unwrap(), 42);
    }

    #[test]
    fn a_bad_tuple_destructure_is_a_clear_error() {
        // arity mismatch and a non-tuple both fail to type-check (RN-E0203).
        assert!(
            run("let (x, y) = (1, 2, 3) in x").is_err(),
            "arity mismatch"
        );
        assert!(run("let (x, y) = 5 in x").is_err(), "not a tuple");
    }

    #[test]
    fn jits_records() {
        // Records: named-field products. Access, field-order independence (sorted
        // by name), through functions, nested, chained access.
        assert_eq!(
            run("let p = { x = 3, y = 4 } in p.x * 10 + p.y").unwrap(),
            34,
            "access"
        );
        assert_eq!(
            run("let p = { y = 4, x = 3 } in p.x * 10 + p.y").unwrap(),
            34,
            "field order irrelevant"
        );
        assert_eq!(
            run("let mk = fn a: Int => { lo = a, hi = a + 1 } in let r = mk 7 in r.hi * 10 + r.lo")
                .unwrap(),
            87,
            "fn returns a record"
        );
        assert_eq!(
            run("let pt = { x = 1, y = { a = 2, b = 3 } } in pt.y.a * 10 + pt.x").unwrap(),
            21,
            "nested + chained"
        );
    }

    #[test]
    fn a_bad_record_access_is_a_clear_error() {
        assert!(
            run("let r = { x = 1 } in r.z")
                .unwrap_err()
                .contains("no field"),
            "missing field"
        );
        assert!(
            run("(5).x").unwrap_err().contains("needs a record"),
            "not a record"
        );
    }

    #[test]
    fn jits_a_multi_op_effect() {
        // One effect, TWO operations with different result types (Int and Bool),
        // one handler discharging both — the Reader pattern. muted() is false, so
        // we ask volume() = 80; the effect is fully handled, so it's pure and 80.
        let src = "effect Settings { volume : Unit -> Int ; muted : Unit -> Bool } in \
                   handle (if perform muted () then 0 else perform volume ()) with { \
                   muted(x) => resume false ; volume(x) => resume 80 ; return(y) => y }";
        assert_eq!(
            run(src).unwrap(),
            80,
            "two ops of one effect, both tail-resumed"
        );
    }

    #[test]
    fn stages_compile_time_code_generation() {
        // The comonadic half. A `splice` runs its generator at compile time; the
        // residual stage-0 code takes its place.
        // β / cancellation + residualize: ${ quote(2+3) } = 2+3 = 5 (runtime add).
        assert_eq!(run("${ quote(2 + 3) }").unwrap(), 5, "splice/quote cancel");
        // a STATIC `if` selects which code to emit — real specialization. The
        // unchosen arm never exists at runtime (asm is a single `mov`).
        assert_eq!(
            run("${ if 1 < 2 then quote(10) else quote(20) }").unwrap(),
            10,
            "then"
        );
        assert_eq!(
            run("${ if 5 < 2 then quote(10) else quote(20) }").unwrap(),
            20,
            "else"
        );
        // static arithmetic in the condition is evaluated at generation time.
        assert_eq!(
            run("${ if 2 * 3 == 6 then quote(1) else quote(0) }").unwrap(),
            1,
            "arith cond"
        );
        // the residual is ordinary stage-0 code: it composes with runtime values.
        assert_eq!(
            run("let n = 7 in n + ${ quote(100) }").unwrap(),
            107,
            "residual composes"
        );
    }

    #[test]
    fn stages_with_generator_bindings() {
        // `let` + variables in the generator, and CROSS-STAGE constants — static
        // values baked into the generated code.
        // a generation-stage value lifted into code as a literal:
        assert_eq!(
            run("${ let m = 7 in quote(m) }").unwrap(),
            7,
            "cross-stage constant"
        );
        // the generator computes, the static `if` picks, and `n` parameterizes the
        // residual `3 * 10` (= 30):
        assert_eq!(
            run("${ let n = 3 in if n < 5 then quote(n * 10) else quote(0) }").unwrap(),
            30,
            "code parameterized by a compile-time value"
        );
        // a code-valued generator var, spliced into more code: `5 + 1` = 6.
        assert_eq!(
            run("${ let c = quote(5) in quote(${c} + 1) }").unwrap(),
            6,
            "code composition"
        );
        // a generator `let` binding a code value, returned directly (was an error
        // in slice 1): `${ let m = quote(5) in m }` = 5.
        assert_eq!(
            run("${ let m = quote(5) in m }").unwrap(),
            5,
            "let-bound code value"
        );
    }

    /// `power n input`: a recursive generation-stage code-builder specializing
    /// `input ^ n` to straight-line multiplies, applied to `input`.
    fn power_prog(n: i32, input: i32) -> String {
        "let f = fn y: Int => \
           ${ let rec power : Int -> Code[Int] -> Code[Int] = fn k: Int => fn x: Code[Int] => \
                if k == 0 then quote(1) else quote(${x} * ${power (k - 1) x}) \
              in power #N# (quote(y)) } \
         in f #IN#"
            .replace("#N#", &n.to_string())
            .replace("#IN#", &input.to_string())
    }

    #[test]
    fn stages_a_recursive_code_builder() {
        // `power` — the canonical multi-stage example. A RECURSIVE generation-stage
        // function specializes x^n into straight-line multiplies; the whole
        // recursion runs at compile time, leaving only the arithmetic (the asm has
        // no loop and no `power`). Functions + recursion in a generator.
        assert_eq!(run(&power_prog(3, 4)).unwrap(), 64, "4^3");
        assert_eq!(run(&power_prog(5, 2)).unwrap(), 32, "2^5");
        assert_eq!(run(&power_prog(4, 3)).unwrap(), 81, "3^4");
        assert_eq!(run(&power_prog(1, 7)).unwrap(), 7, "7^1");
        assert_eq!(run(&power_prog(0, 9)).unwrap(), 1, "9^0 = 1");
    }

    #[test]
    fn staging_composes_with_effects() {
        // δ's object direction: a `quote` of an effectful computation keeps the
        // effect (winapi, from console_writeln) INSIDE the Code — `Code[Unit ! {winapi}]`
        // — so the generated code performs it at runtime. (Both print as a side
        // effect and yield console_writeln's Unit = 0.)
        assert_eq!(
            run(r#"${ quote(console_writeln "generated") }"#).unwrap(),
            0,
            "effect rides in staged code"
        );
        // staging chooses WHICH effectful code at compile time (branches share the
        // row, so the Code types match).
        let choose = r#"${ let level = 2 in
                           if level == 2 then quote(console_writeln "verbose") else quote(console_writeln "quiet") }"#;
        assert_eq!(
            run(choose).unwrap(),
            0,
            "compile-time choice among effectful variants"
        );
    }

    #[test]
    fn genlet_shares_a_computation() {
        // δ's generative direction: `genlet` hoists a binding so a computation
        // used many times runs ONCE. `${ genlet(quote(41 + 1)) }` hoists `41 + 1`
        // to a shared `let` and references it = 42.
        assert_eq!(
            run("${ genlet(quote(41 + 1)) }").unwrap(),
            42,
            "genlet hoists then references"
        );
        // an EFFECTFUL base shared: 5 + 5 = 10, and the base (and its print) runs
        // ONCE — the difference LLVM can't optimize away (no dedup of side effects).
        let shared =
            r#"${ let r = genlet(quote(let u = console_writeln "x" in 5)) in quote(${r} + ${r}) }"#;
        assert_eq!(run(shared).unwrap(), 10, "shared effectful base, 5 + 5");
    }

    #[test]
    fn an_unreducible_generator_is_a_clear_error() {
        // The generator fragment (let / if / vars / arithmetic / quote / recursive
        // functions / genlet / letloc) reduces. A cross-stage STRING constant (a
        // String as a generation-stage value) is a later slice: a clear error.
        let src = r#"${ let msg = "tag" in quote(msg) }"#;
        assert!(run(src).unwrap_err().contains("not reducible"));
    }

    #[test]
    fn jits_a_user_declared_effect() {
        // `effect` gives a USER op a signature; `perform` + a tail-resumptive
        // handler then run it — the canonical "provide a value" example. The
        // handler supplies 21, the return clause doubles it, the effect is
        // discharged (pure), and it is 42.
        let src = "effect ask : Unit -> Int in \
                   handle perform ask () with { ask(x) => resume 21 ; return(y) => y + y }";
        assert_eq!(
            run(src).unwrap(),
            42,
            "declare + perform + handle a user effect"
        );
    }

    #[test]
    fn jits_effect_and_handler_sugar() {
        let src = "effect ask : Unit -> Int in \
                   handle (ask() + 1) with { ask(_) -> 41 }";
        assert_eq!(
            run(src).unwrap(),
            42,
            "operation-call sugar performs, handler-arm sugar resumes"
        );
    }

    // ── repr-poly tag slice: generic `List[Int]` runs end to end (in-process) ──

    #[test]
    fn generic_list_len_over_int_runs() {
        // THE SLICE, executed: a generic `list_len` over a concrete `List[Int]`
        // built with tagged scalars in `Var` (word) cells. Used to be unlowerable
        // ("cannot lower representation-polymorphic layout yet"). Now it lowers and
        // the JIT'd code walks the word-cell rest pointers and counts, returning 3.
        // This exercises the tag store (`value<<2` into a word cell), the word-cell
        // rest pointer (`set_ptr`/`get_ptr`), and the GC, all in-process.
        assert_eq!(
            run("list_len (Cons(1, Cons(2, Cons(3, Nil))))").unwrap(),
            3,
            "generic list_len over Int counts the elements"
        );
        assert_eq!(run("list_len Nil").unwrap(), 0, "empty list has length 0");
    }

    #[test]
    fn list_take_and_drop_run() {
        // Executed end to end through the GC: take/drop slice a List[Int], and
        // list_len on the result confirms the count (take 2 of 3 ⇒ 2; drop 1 ⇒ 2).
        let l = "(Cons(1, Cons(2, Cons(3, Nil))))";
        assert_eq!(run(&format!("list_len (list_take {l} 2)")).unwrap(), 2);
        assert_eq!(run(&format!("list_len (list_drop {l} 1)")).unwrap(), 2);
        // Boundaries: take more than length ⇒ whole list; drop all ⇒ empty.
        assert_eq!(run(&format!("list_len (list_take {l} 9)")).unwrap(), 3);
        assert_eq!(run(&format!("list_len (list_drop {l} 9)")).unwrap(), 0);
    }

    #[test]
    fn list_find_bridges_to_option_and_runs() {
        // list_find returns Option (cross-module: option grafts outer of list).
        // option_with_default extracts: found ⇒ the value, not-found ⇒ the default.
        let l = "(Cons(1, Cons(2, Cons(3, Nil))))";
        assert_eq!(
            run(&format!(
                "option_with_default (list_find {l} (fn x: Int => 1 < x)) 99"
            ))
            .unwrap(),
            2,
            "first element > 1 is 2"
        );
        assert_eq!(
            run(&format!(
                "option_with_default (list_find {l} (fn x: Int => x < 0)) 99"
            ))
            .unwrap(),
            99,
            "no element < 0 ⇒ the default"
        );
    }

    #[test]
    fn runs_expected_comparison_operators() {
        assert_eq!(run("if 2 <= 2 then 1 else 0").unwrap(), 1);
        assert_eq!(run("if 3 > 2 then 1 else 0").unwrap(), 1);
        assert_eq!(run("if 3 >= 3 then 1 else 0").unwrap(), 1);
        assert_eq!(run("if 2 != 3 then 1 else 0").unwrap(), 1);
        assert_eq!(run("if 2 >= 3 then 0 else 1").unwrap(), 1);
        assert_eq!(run("if 1.5 <= 2.0 then 1 else 0").unwrap(), 1);
    }

    #[test]
    fn runs_array_and_num_helpers() {
        // array_make_int allocates a runtime-sized Int array with all slots initialized.
        assert_eq!(
            run("let a = array_make_int 3 5 in let _ = a[1] <- 9 in a[0] + a[1] + a[2]").unwrap(),
            19
        );
        // array_fill writes every cell; read one back.
        assert_eq!(
            run("let a = [0, 0, 0] in let _ = array_fill_int a 7 in a[1]").unwrap(),
            7
        );
        // array_copy_range copies src→dst; read the copied tail.
        assert_eq!(
            run("let src = [1, 2, 3] in let dst = [0, 0, 0] in \
                 let _ = array_copy_range_int src 0 dst 0 3 in dst[2]")
            .unwrap(),
            3
        );
        // num scalar helpers.
        assert_eq!(run("abs (0 - 5)").unwrap(), 5);
        assert_eq!(run("abs 5").unwrap(), 5);
        assert_eq!(run("max 3 7").unwrap(), 7);
        assert_eq!(run("min 3 7").unwrap(), 3);
    }

    #[test]
    fn runs_random_helpers() {
        assert_eq!(run("random_next_seed 12345").unwrap(), 595905495);
        assert_eq!(
            run("let (roll, seed2) = random_between 1 6 12345 in roll * 1000 + (seed2 % 1000)")
                .unwrap(),
            4495
        );
        assert_eq!(
            run("let (roll, _seed2) = random_between 6 1 12345 in roll").unwrap(),
            4
        );
        assert_eq!(
            run("let (flag, seed2) = random_bool 12345 in if flag then 0 else seed2 % 1000")
                .unwrap(),
            495
        );
        assert_eq!(
            run("let (ok, _seed2) = random_chance 1 2 12345 in if ok then 1 else 0").unwrap(),
            0
        );
        assert_eq!(
            run("let (ok, _seed2) = random_chance 2 2 12345 in if ok then 1 else 0").unwrap(),
            1
        );
    }

    #[test]
    fn runs_more_list_combinators() {
        // list_all / list_any (predicates ⇒ 1/0) and list_append (length checks).
        assert_eq!(
            run("list_all (Cons(2, Cons(4, Nil))) (fn x: Int => 1 < x)").unwrap(),
            1,
            "all > 1"
        );
        assert_eq!(
            run("list_all (Cons(2, Cons(0, Nil))) (fn x: Int => 1 < x)").unwrap(),
            0,
            "0 is not > 1"
        );
        assert_eq!(
            run("list_any (Cons(0, Cons(5, Nil))) (fn x: Int => 1 < x)").unwrap(),
            1,
            "5 > 1"
        );
        assert_eq!(
            run("list_any (Cons(0, Cons(1, Nil))) (fn x: Int => 1 < x)").unwrap(),
            0,
            "none > 1"
        );
        assert_eq!(
            run("list_len (list_append (Cons(1, Nil)) (Cons(2, Cons(3, Nil))))").unwrap(),
            3,
            "1 + 2 elements"
        );
    }

    #[test]
    fn runs_the_core_combinators() {
        // Execution coverage for the core higher-order stdlib combinators — the
        // order-module lesson (type-check passes, run fails) means these deserve
        // end-to-end checks, not just `ty_of`. Results are reduced to an Int so the
        // JIT exit code can carry them.
        let l3 = "(Cons(1, Cons(2, Cons(3, Nil))))";
        // list_fold: sum 1+2+3 = 6.
        assert_eq!(
            run(&format!(
                "list_fold {l3} 0 (fn acc: Int => fn x: Int => acc + x)"
            ))
            .unwrap(),
            6
        );
        // list_map then list_len: maps to 3 elements.
        assert_eq!(
            run(&format!("list_len (list_map {l3} (fn x: Int => x + 10))")).unwrap(),
            3
        );
        // list_filter: keep > 1 ⇒ 2 elements.
        assert_eq!(
            run(&format!("list_len (list_filter {l3} (fn x: Int => 1 < x))")).unwrap(),
            2
        );
        // option_map + extract.
        assert_eq!(
            run("option_with_default (option_map (Some(5)) (fn x: Int => x + 1)) 0").unwrap(),
            6
        );
        // option_bind: chain to Some/None.
        assert_eq!(
            run("option_with_default (option_bind (Some(5)) (fn x: Int => Some(x + 2))) 0")
                .unwrap(),
            7
        );
        // result_map then unwrap-or.
        assert_eq!(
            run("result_with_default (result_map (Ok(5)) (fn x: Int => x * 2)) 0").unwrap(),
            10
        );
    }

    #[test]
    fn runs_the_order_helpers() {
        // min_by/max_by with num's `compare` as the Ordering comparator.
        assert_eq!(run("min_by 3 5 compare").unwrap(), 3);
        assert_eq!(run("max_by 3 5 compare").unwrap(), 5);
        assert_eq!(run("min_by 5 3 compare").unwrap(), 3, "order-independent");
        assert_eq!(run("max_by 5 3 compare").unwrap(), 5);
    }

    #[test]
    fn runs_the_bool_combinators() {
        // Boolean logic functions (Bool ⇒ 1/0 as the exit code).
        assert_eq!(run("bool_not false").unwrap(), 1);
        assert_eq!(run("bool_not true").unwrap(), 0);
        assert_eq!(run("bool_and true false").unwrap(), 0);
        assert_eq!(run("bool_or false true").unwrap(), 1);
        assert_eq!(run("bool_xor true true").unwrap(), 0);
        assert_eq!(run("bool_xor true false").unwrap(), 1);
    }

    #[test]
    fn list_index_safe_access_runs() {
        // 0-indexed safe access via Option: in range ⇒ the element, out of range
        // (or negative) ⇒ the default.
        let l = "(Cons(10, Cons(20, Cons(30, Nil))))";
        assert_eq!(
            run(&format!("option_with_default (list_index {l} 1) 0")).unwrap(),
            20
        );
        assert_eq!(
            run(&format!("option_with_default (list_index {l} 5) 0")).unwrap(),
            0
        );
        assert_eq!(
            run(&format!("option_with_default (list_index {l} (0 - 1)) 0")).unwrap(),
            0
        );
    }

    #[test]
    fn option_to_result_bridges_and_runs() {
        // Option → Result (result grafts outer of option). result_is_ok confirms
        // the branch: Some ⇒ Ok (true), None ⇒ Err (false).
        assert_eq!(
            run("result_is_ok (option_to_result (Some(1)) 0)").unwrap(),
            1,
            "Some ⇒ Ok"
        );
        assert_eq!(
            run("result_is_ok (option_to_result None 0)").unwrap(),
            0,
            "None ⇒ Err"
        );
    }

    #[test]
    fn generic_list_reverse_then_len_over_int_runs() {
        // `list_reverse` recurses `Cons(h, acc)` — the **passthrough**: `h` is read
        // from a word cell and re-stored verbatim into a word cell (no re-tag, no
        // untag). Reversing a 3-element `List[Int]` then measuring it must still be
        // 3 — proving the passthrough preserves the elements through the word cells.
        assert_eq!(
            run("list_len (list_reverse (Cons(1, Cons(2, Cons(3, Nil)))))").unwrap(),
            3,
            "reverse preserves length through word-cell passthrough"
        );
    }

    #[test]
    fn generic_list_map_over_int_runs() {
        // `list_map xs f` rebuilds `Cons(f h, …)` — the mapped element (a `b`-typed
        // value, a `Var`) is stored verbatim into the new node's word cell. Mapping
        // over a 2-element `List[Int]` and measuring the result is still 2, proving
        // the mapped-element word store keeps the list well-formed.
        assert_eq!(
            run("list_len (list_map (Cons(1, Cons(2, Nil))) (fn x: Int => x + 10))").unwrap(),
            2,
            "map preserves length through the mapped-element word store"
        );
    }

    #[test]
    fn nested_handle_in_var_cell_runs() {
        // A concrete managed handle (an inner `List`) stored into the outer node's
        // `Var` (word) cell must be resolved to an `addr|10` interior pointer
        // (ToPtr) on store, and interned back to a handle (FromPtr) on read — its
        // `0xABCD` index bits are not a valid word-cell word. Used to crash at
        // `set_word`'s handle-magic assert.
        assert_eq!(
            run("list_len (Cons(Cons(1, Nil), Nil))").unwrap(),
            1,
            "ToPtr: an inner list lands in the outer Var cell as an interior pointer"
        );
        assert_eq!(
            run(
                "match Cons(Cons(7, Cons(8, Nil)), Nil) with | Nil => 0 | Cons(h, t) => list_len h"
            )
            .unwrap(),
            2,
            "FromPtr: the inner-list word is interned back to a usable List handle"
        );
    }

    #[test]
    fn hof_callback_returning_a_handle_runs() {
        // A callback `fn x: Int => Cons(x, Nil)` returns a managed handle; the HOF
        // wrapper must ToPtr that result into the mapped list's word cell (covariant
        // codomain), and a later read must FromPtr it.
        assert_eq!(
            run("list_len (list_map (Cons(1, Cons(2, Nil))) (fn x: Int => Cons(x, Nil)))").unwrap(),
            2,
            "wrapper ToPtr: a handle-returning callback maps into a List[List[Int]]"
        );
        assert_eq!(
            run(
                "match list_map (Cons(5, Nil)) (fn x: Int => Cons(x, Nil)) with \
                 | Nil => 0 | Cons(h, t) => list_len h"
            )
            .unwrap(),
            1,
            "the mapped inner handle reads back via FromPtr"
        );
    }

    #[test]
    fn nested_handles_survive_a_collection() {
        // GC-criticality: a managed handle laid into a `Var` cell as `addr|10` must
        // be rewritten in place when its object is evacuated. Build thousands of
        // nested handles (forcing G0 collections), then read them back — a stale
        // (un-rewritten) interior pointer would panic in `resolve` or count garbage.
        let range = "let rec range : Int -> List[Int] ! {gc} = \
                     fn n: Int => if n == 0 then Nil else Cons(n, range (n - 1)) in ";
        // ~10k allocations evacuate the live nested nodes; the outer spine survives.
        assert_eq!(
            run(&format!(
                "{range} list_len (list_map (range 5000) (fn x: Int => Cons(x, Nil)))"
            ))
            .unwrap(),
            5000,
            "every nested addr|10 interior word is rewritten across collections"
        );
        // Read an inner list's length AFTER the collections that built the outer.
        assert_eq!(
            run(&format!(
                "{range} match list_map (range 5000) (fn x: Int => Cons(x, Cons(x, Nil))) with \
                 | Nil => 0 | Cons(h, t) => list_len h"
            ))
            .unwrap(),
            2,
            "an inner list read after a collection follows the rewritten pointer"
        );
    }

    #[test]
    fn generic_list_fold_over_int_runs() {
        // A CURRIED callback (`b -> a -> b`) flowing into list_fold's generic arrow
        // param: the wrapper recurses the spine (untag each arg, tag the result),
        // and the App-result Untag un-coerces the `b`-word result at the pinned
        // concrete `Int`. Before T7 this returned `Tag(sum)` (e.g. 24, not 6).
        assert_eq!(
            run("list_fold (Cons(1, Cons(2, Cons(3, Nil)))) 0 (fn acc: Int => fn x: Int => acc + x)")
                .unwrap(),
            6,
            "curried fold sum 1+2+3"
        );
        assert_eq!(
            run("list_fold (Cons(10, Cons(20, Cons(30, Nil)))) 0 (fn acc: Int => fn x: Int => acc + x)")
                .unwrap(),
            60,
            "curried fold sum 10+20+30"
        );
        assert_eq!(
            run("list_fold Nil 42 (fn acc: Int => fn x: Int => acc + x)").unwrap(),
            42,
            "empty fold returns the init, untagged at the result"
        );
    }

    #[test]
    fn nested_generic_destructure_runs() {
        // Matching a managed handle pulled THROUGH a Var cell, then DESTRUCTURED
        // directly (not just passed to a fn), used to fail `NotASum`: Term::Match
        // read the binder's refined `Var` type without resolving it through the
        // store. Now resolved at the Match/LetTuple/Field destructure sites.
        // List[Option[Int]]: read the inner Some's payload.
        assert_eq!(
            run("match Cons(Some(5), Nil) with | Nil => 0 | Cons(h, t) => \
                 match h with | None => 0 | Some(y) => y")
            .unwrap(),
            5,
            "nested Option destructure through a Var cell"
        );
        // List[List[Int]]: destructure the inner list directly (head of [7]).
        assert_eq!(
            run(
                "match Cons(Cons(7, Nil), Nil) with | Nil => 0 | Cons(h, t) => \
                 match h with | Nil => 0 | Cons(a, b) => a"
            )
            .unwrap(),
            7,
            "nested List destructure through a Var cell"
        );
    }

    #[test]
    fn handle_accumulator_fold_runs() {
        // A curried callback whose ACCUMULATOR is a managed handle (a List): the
        // wrapper's inner lambda must capture the interned HANDLE (a FromPtr hoisted
        // into the outer lambda), not the raw `addr|10` word -- capturing the word
        // set_ptr'd a magic-less pointer and crashed `resolve`. Reverse [1,2,3] onto
        // Nil via Cons -> [3,2,1].
        assert_eq!(
            run(
                "list_len (list_fold (Cons(1, Cons(2, Cons(3, Nil)))) (Nil) \
                 (fn acc: List[Int] => fn x: Int => Cons(x, acc)))"
            )
            .unwrap(),
            3,
            "reverse via a handle-accumulator fold: length 3"
        );
        assert_eq!(
            run("match list_fold (Cons(1, Cons(2, Cons(3, Nil)))) (Nil) \
                 (fn acc: List[Int] => fn x: Int => Cons(x, acc)) with \
                 | Nil => 0 | Cons(h, t) => h")
            .unwrap(),
            3,
            "head of the reversed list"
        );
        // Non-empty init: fold [1,2] onto [99] -> [2,1,99].
        assert_eq!(
            run(
                "list_len (list_fold (Cons(1, Cons(2, Nil))) (Cons(99, Nil)) \
                 (fn acc: List[Int] => fn x: Int => Cons(x, acc)))"
            )
            .unwrap(),
            3,
            "handle-accumulator fold onto a non-empty init"
        );
    }

    #[test]
    fn crt_math_services_run() {
        // The math.* layer-1 services alias raw crt_* externs backed by
        // ucrtbase.dll. The IR collapses the `let pow = crt_pow` alias so `pow a b`
        // is ONE foreign "pow"(a,b); the resolver records pow -> ucrtbase.dll; the
        // JIT LoadLibrary's ucrtbase.dll + GetProcAddress's `pow`. App names no extern.
        assert_eq!(run_f64("pow 2.0 3.0").unwrap(), 8.0, "pow 2 3 via the CRT");
        assert_eq!(run_f64("pow 5.0 2.0").unwrap(), 25.0);
        assert_eq!(run_f64("exp 0.0").unwrap(), 1.0, "exp 0 = 1");
        assert_eq!(run_f64("cos 0.0").unwrap(), 1.0, "cos 0 = 1");
        assert_eq!(run_f64("sin 0.0").unwrap(), 0.0, "sin 0 = 0");
        assert_eq!(
            run_f64("ln 1.0").unwrap(),
            0.0,
            "ln 1 = 0 (CRT log is natural log)"
        );
        assert_eq!(run_f64("atan2 0.0 1.0").unwrap(), 0.0, "atan2 0 1 = 0");
    }

    // ── traits / qualified types v1 — Sprint 3: dictionary-passing RUNS ──────
    //
    // The trait constraint is discharged to a runtime **dictionary** (a record of
    // method closures, `object-system-design.md` §4) that the JIT actually calls.
    // These exercise both lowering paths: a *monomorphic* call site (instance
    // known — the dictionary literal) and a *polymorphic* generic (the hidden
    // dictionary parameter threaded onward). Return type `Int` keeps the result
    // `run`-checkable without a String runtime.

    /// A user `trait Show a { show : a -> Int }` + `instance Show Int` and a
    /// program `show 5` ⇒ **5**: a monomorphic resolved call running *through* the
    /// dictionary (`Field(dict_Show_Int, show) 5`, the instance literal built from
    /// `instance Show Int { show = fn x => x }`).
    #[test]
    fn trait_show_int_runs_through_a_dictionary() {
        assert_eq!(
            run("trait Show a { show : a -> Int } in \
                 instance Show Int { show = fn x => x } in \
                 show 5")
            .unwrap(),
            5
        );
    }

    /// A trait method that actually **distinguishes instances**: `toInt true` ⇒ 1,
    /// `toInt false` ⇒ 0 (the `instance ToInt Bool` body runs through the dict).
    #[test]
    fn trait_method_distinguishes_instances() {
        let prog = |b: &str| {
            format!(
                "trait ToInt a {{ toInt : a -> Int }} in \
                 instance ToInt Bool {{ toInt = fn b => if b then 1 else 0 }} in \
                 toInt {b}"
            )
        };
        assert_eq!(run(&prog("true")).unwrap(), 1);
        assert_eq!(run(&prog("false")).unwrap(), 0);
    }

    /// The §1.4 worked example **runs**: `min2 3 7` ⇒ 3 with a user `instance Ord
    /// Int` (superclass `Eq Int`) whose `compare` uses the int comparison ops.
    /// `min2 : Ord a => a -> a -> a` is a constrained generic — it takes a hidden
    /// `dict_Ord` parameter, projects `dict_Ord.compare`, and the call site supplies
    /// the `Ord Int` dictionary literal (with its embedded `super_Eq`).
    #[test]
    fn trait_min2_ord_int_runs() {
        let prog = "type Ordering = LT | EQ | GT in \
             trait Eq a { eq : a -> a -> Bool } in \
             trait Ord a requires Eq a { compare : a -> a -> Ordering ! {gc} } in \
             instance Eq Int { eq = fn x => fn y => x == y } in \
             instance Ord Int { compare = fn x => fn y => if x < y then LT else if x == y then EQ else GT } in \
             let min2 = fn x => fn y => match compare x y with | LT => x | EQ => x | GT => y in \
             min2 3 7";
        assert_eq!(run(prog).unwrap(), 3);
        // The symmetric case (so we know `compare` truly drives the choice).
        let prog2 = prog.replace("min2 3 7", "min2 7 3");
        assert_eq!(run(&prog2).unwrap(), 3);
    }

    /// A **polymorphic** generic that **threads** a dictionary: `describe x = toInt
    /// x` is `ToInt a => a -> Int`; its constraint rides its scheme (the hidden dict
    /// parameter), and a use at a concrete type supplies the literal. Proves the
    /// dict is threaded into the generic, not just inlined at a monomorphic site.
    #[test]
    fn trait_polymorphic_generic_threads_a_dictionary() {
        let prog = "trait ToInt a { toInt : a -> Int } in \
             instance ToInt Bool { toInt = fn b => if b then 10 else 20 } in \
             instance ToInt Int { toInt = fn n => n + 1 } in \
             let describe = fn x => toInt x in \
             describe true + describe 5";
        // describe true => 10 ; describe 5 => 6 ; sum = 16. Two different instances
        // reach the same generic, so the dictionary must be threaded per call.
        assert_eq!(run(prog).unwrap(), 16);
    }

    /// **R2 stage-0 devirtualization — the zero-cost evidence**
    /// (`trait-resolution.md` §1.3; `docs/traits-devirt-sprint.md`). A *direct
    /// monomorphic* trait-method call resolves to a concrete `DictEvidence::Instance`
    /// at elaboration, so `sema::dict_pass` β-inlines the instance's method body at
    /// the call site (the pipeline does **not** β-reduce an immediately-applied
    /// lambda — it would GC-allocate a closure and indirect-call it — so the inline
    /// happens at compile time). The dictionary record, the field load, and the
    /// indirect call are all **erased**; the value still computes.
    ///
    /// ## IR before vs after (inspected via `emit_module(...).print_to_string()`)
    ///
    /// **Before** this sprint, `toInt 5` lowered like the *polymorphic* path still
    /// does (see `trait_polymorphic_generic_threads_a_dictionary` — and the
    /// `describe 5` baseline asserted below): the dictionary is a GC record, the
    /// method a field load, the call indirect —
    /// ```llvm
    ///   %d   = call i64 @locus_gc_alloc(i64 0, i64 1)        ; the dict record
    ///   call void @locus_gc_set_scalar(i64 %d, i64 0, ptr @__locus_lam_… )
    ///   %fpw = call i64 @locus_gc_get_scalar(i64 %d, i64 0)  ; load the method field
    ///   %fp  = inttoptr i64 %fpw to ptr
    ///   %r   = call i64 %fp(i64 %d, i64 %tagged5)            ; the INDIRECT call
    /// ```
    ///
    /// **After**, the whole dictionary indirection is gone — the method body
    /// `fn x => x` is inlined and folded to the literal:
    /// ```llvm
    ///   define i64 @__locus_main() { entry: ret i64 5 }
    /// ```
    ///
    /// The assertions below pin exactly that erasure at the direct site, and
    /// (for contrast) confirm the polymorphic baseline still has the record +
    /// indirect call — so the test proves the difference is real, not absence of
    /// the construct everywhere.
    #[test]
    fn trait_direct_monomorphic_call_is_devirtualized_to_zero_cost() {
        let direct = "trait ToInt a { toInt : a -> Int } in \
                      instance ToInt Int { toInt = fn x => x } in \
                      toInt 5";
        // Correctness — the inlined body still computes the value.
        assert_eq!(run(direct).unwrap(), 5);

        // Zero-cost evidence — inspect the lowered LLVM IR for the direct site.
        let ctx = inkwell::context::Context::create();
        let ir = ir_of(direct);
        let module = crate::lower::emit_module(&ctx, &ir, false).unwrap();
        let text = module.print_to_string().to_string();

        // No dictionary record / closure is GC-allocated at the direct site.
        assert!(
            !text.contains("locus_gc_alloc"),
            "devirt should build NO dictionary record (no GC alloc):\n{text}"
        );
        // No indirect call: the method is not loaded from a field and called
        // through a function pointer (`inttoptr` + `call i64 %fp(...)`).
        assert!(
            !text.contains("inttoptr"),
            "devirt should leave NO function-pointer load (no indirect call):\n{text}"
        );
        assert!(
            !text.contains("locus_gc_get_scalar") && !text.contains("locus_gc_get_ptr"),
            "devirt should leave NO dictionary field load:\n{text}"
        );
        // The body inlined and folded to the literal it computes.
        assert!(
            text.contains("ret i64 5"),
            "the inlined `fn x => x` applied to 5 should fold to `ret i64 5`:\n{text}"
        );

        // Contrast: the *polymorphic* path (a generic threading a runtime dict)
        // DOES still build the record and call through a pointer — so the erasure
        // above is a real difference, not a property of every trait program.
        let poly = "trait ToInt a { toInt : a -> Int } in \
                    instance ToInt Bool { toInt = fn b => if b then 10 else 20 } in \
                    instance ToInt Int { toInt = fn n => n + 1 } in \
                    let describe = fn x => toInt x in \
                    describe 5";
        let ir_poly = ir_of(poly);
        let module_poly = crate::lower::emit_module(&ctx, &ir_poly, false).unwrap();
        let text_poly = module_poly.print_to_string().to_string();
        assert!(
            text_poly.contains("locus_gc_alloc"),
            "the polymorphic baseline still builds a dictionary record:\n{text_poly}"
        );
        assert!(
            text_poly.contains("inttoptr"),
            "the polymorphic baseline still calls the method indirectly:\n{text_poly}"
        );
    }
}
