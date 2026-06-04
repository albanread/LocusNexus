# Locus Linux x86-64 Compiler

This is the Linux x86-64 compiler surface for Locus. It is intentionally a
standalone Cargo project, not a root workspace member, so early Linux work can
move without forcing Windows-focused colleagues to absorb new build/link/runtime
constraints. The agent-facing command surface tracks `locusc`.

Current scope:

- `locusc run FILE`
- `locusc build FILE [-o EXE] [--always-gc]`
- `locusc asm FILE [-o OUT.s]`
- `locusc effects FILE [--json]`
- `locusc republish [DIR]`
- Shared front end from `locus`
- Shared LLVM lowering from `locus-llvm`
- Shared runtime/GC shims from `locus-rt`
- Linux libc/libm extern resolution from `locus-libc`
- Bare known libc/libm extern materialization in authorized boundary modules
- Linux-local ORC JIT setup
- SysV x86-64 `extern asm` dev-twin symbols for the sidecar JIT
- Internal ELF object emission with embedded Linux `.masm` runtime symbols
- ELF executable linking through `cc`
- Opportunistic `liblocus_rt.a` linking for real GC in AOT binaries
- Linux stdout service over libc `write`, including in-Locus UTF-16 to UTF-8 conversion
- Pure, managed-array/GC, console, and math/libm smoke coverage
- Manifest-authorized boundary-module coverage for Layer-0 asm calls

Not ported yet:

- Generated POSIX/libc extern oracle beyond the seed libc/libm ABI map
- Automatic production of `liblocus_rt.a` from the sidecar build; run
  `cargo build -p locus-rt` at the repo root when real-GC AOT linking is needed

Build and test in WSL Ubuntu:

```sh
cd /mnt/c/projects/locus/ports/linux-x86_64
CARGO_TARGET_DIR=/home/oberon/.cache/locus-linux-sidecar \
  RUSTFLAGS=-Awarnings \
  cargo test
```

Run a non-printing example:

```sh
CARGO_TARGET_DIR=/home/oberon/.cache/locus-linux-sidecar \
  RUSTFLAGS=-Awarnings \
  cargo run -- run ../../examples/iteration.locus
```

Emit and link an ELF executable:

```sh
CARGO_TARGET_DIR=/home/oberon/.cache/locus-linux-sidecar \
  RUSTFLAGS=-Awarnings \
  cargo run -- build ../../examples/iteration.locus -o /tmp/locus-iteration
```
