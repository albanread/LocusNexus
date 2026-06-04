# locus-libc

Linux libc/libm boundary metadata for Locus.

The first slice is intentionally a small seed resolver:

- `crt.locus` already writes explicit floating-point signatures.
- `locus-libc` maps those known math symbols to `libm.so.6`.
- The Linux sidecar uses `dlopen`/`dlsym` to register absolute symbols with ORC.

That keeps the Linux port moving without pretending the seed table is the final
ABI oracle.

## Generated Oracle Plan

The long-term Linux oracle should be generated, not hand-maintained. The likely
pipeline is:

1. Read the target sysroot headers with Clang.
2. Extract exported C function declarations, parameter/return types, variadic
   status, and header/module provenance.
3. Normalize C types into Locus FFI leaves (`I32`, `U32`, `Ptr`, `Float`,
   `Unit`, etc.).
4. Attach link objects (`libc.so.6`, `libm.so.6`, `libdl.so.2`, pthread where
   relevant) from a curated symbol-to-library map.
5. Emit a compact checked-in snapshot, similar in spirit to `locus-winapi`'s
   projected Win32 metadata blob.

Useful open-source inputs are local Linux headers under `/usr/include`, glibc or
musl source headers, Linux UAPI headers, and the Linux man-pages project. The
generator should prefer the configured target sysroot as the source of truth so
the sidecar matches the distro/toolchain it is compiling against.
