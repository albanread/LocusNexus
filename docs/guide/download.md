# Download — Locus research preview (Windows)

> **Research pre-release.** This is an early development build. The language,
> its type checker, the JIT, and the tools work — you can write programs, see
> their effect rows, and run graphical demos — but testing is ongoing and some
> corners of the language are still being hardened. Treat it as a research kit,
> not a production tool.

**[⬇ Download locus-windows-preview.zip](../../locus-windows-preview.zip)**
*(53 MB · Windows x64 · no installer required)*

---

## What is in the zip

Unzip anywhere; all three binaries are self-contained — the language, its
standard library, the JIT, the GC, and the SQLite service plugin are linked in.
No install, no runtime to fetch, no PATH changes required.

```
locus-ide.exe          The IDE — also present as locus.exe (same binary)
locusc.exe             The compiler driver (CLI)
locusc-mcp.exe         The MCP server for agents

examples\              Sample programs
gui_runit\             Graphical programs (canvas, Julia, Mandelbrot, Othello …)
help\                  The built-in manual (also open with F1 inside the IDE)
```

### `locus-ide.exe` — the IDE

A Windows-native Direct2D editor with a JIT-backed live runner. Open a `.locus`
file, press **F5**, and see the result in a pane. The program's **effect row** —
every power it uses — is shown alongside the output.

| Key | Does |
|-----|------|
| **F5** | Run the current buffer (JIT) |
| **F6** | Analyze without running — full effect/capability report |
| **F7** | Type-check + squiggles |
| **F1** | Built-in manual |
| Locus menu | Show ANF IR · LLVM IR · Assembly |

Start with `examples\ide_demo.locus` — it opens a pane, draws a grid, and
responds to mouse clicks. Its effect row reads roughly `{ graphics, event, gc,
mem }`: confined to the IDE world, no filesystem, no network. The type proves it.

### `locusc.exe` — the compiler driver

```
locusc run     FILE            JIT-compile and run (exit code = result)
locusc build   FILE [-o EXE]   AOT compile to a standalone .exe
locusc asm     FILE            Dump the x86-64 assembly
locusc effects FILE [--json]   Print the effect manifest
```

`build` needs Visual Studio Build Tools (`link.exe`) on PATH. `run`, `asm`, and
`effects` do not.

### `locusc-mcp.exe` — the MCP server

An MCP (Model Context Protocol) server over stdio — exposes the compiler to a
coding agent: check, run, build, IR/asm, and the structured effect manifest.
Point your MCP host at `locusc-mcp.exe` as a stdio server. The capability
reports are the same ones the CLI and IDE produce, so an agent reviews exactly
what a human would.

---

## Getting started

1. Unzip to any folder (e.g. `C:\locus`).
2. Double-click **`locus-ide.exe`**.
3. Use **File → Open** to open `examples\ide_demo.locus`, then press **F5**.
   A pane opens; click anywhere and a dot follows your mouse.
4. Press **F6** to open the effect report — the manifest of every power the
   program uses, tagged by layer.
5. Press **F1** to browse the built-in manual.

For the command line: open a terminal in the unzip folder and try

```
locusc run     examples\compute.locus
locusc effects examples\effect.locus
```

The `examples\` folder contains pure-functional demos, effect-system examples,
and console programs. The `gui_runit\` folder contains graphical programs
(animated Julia set, Mandelbrot, canvas stress test, Othello). Open any of them
in the IDE and press **F5**.

---

## What to expect

- **The language and type checker** are stable for the programs in `examples\`
  and `gui_runit\`. Effect inference, generics, traits, staging, and the
  algebraic effect system all work.
- **The SQLite service plugin** is built in — `db_open_memory`, `db_prepare`,
  `db_bind_*`, credential vault. See `examples\db_layer.locus`.
- **The MCP server** speaks the full effect-manifest protocol; it is the same
  binary agents use in automated workflows.
- **AOT (`locusc build`)** produces standalone Windows executables via MSVC
  `link.exe`; it needs Visual Studio Build Tools on PATH.
- **macOS / Linux** — the compiler and MCP server build on both; the IDE is
  Windows-only (Direct2D). Cross-platform IDE work is on the roadmap.

---

## Ongoing testing

This release is a snapshot of active research. Specific things still being
hardened:

- **Scope-by-default combinators** (`with_db`, `with_query`) — deferred pending
  a compiler fix for phantom-typed arguments across row-polymorphic boundaries.
- **BLOB columns** in the SQLite plugin — currently returned as a placeholder
  string; a bytes ABI is needed.
- **Error recovery** in the parser — some malformed programs produce a generic
  diagnostic rather than a precise span.
- **The soundness metatheorem** — the effect system is implemented and checked;
  the formal proof that the row can never lie is a work in progress. See the
  [formal proof work](../../formal/README.md) and the
  [design articles](../articles/safety-through-transparency.md).

Feedback, bug reports, and questions are welcome via GitHub issues.

---

*Back to [The IDE](ide.md) · [Guide index](index.md)*
