# Reference

Everything on one page. For depth, follow the links to the chapter that explains
each item. For the agent-facing version of these tables (MCP tools plus the
`locusc` CLI), see [Locus for agents](../locus_for_agents.md).

## The judgment

```
Γ ⊢ e : A ! E @ s
```

In context `Γ`, expression `e` has type `A`, may perform effects `E`, at stage
`s`. Types and effects are inferred; you read them with `locus check` and
`locusc effects`.

## Keywords

| Keyword | Example | See |
|---------|---------|-----|
| `let … in` | `let x = 40 in x + 2` | [Bindings](bindings-and-functions.md) |
| `let rec` | `let rec fib : Int -> Int = fn n: Int => …` | [Bindings](bindings-and-functions.md) |
| `fn` | `fn a: Int => fn b: Int => a + b` | [Bindings](bindings-and-functions.md) |
| `do { … }` | `do { let x = 20; x + 22 }` | [Bindings](bindings-and-functions.md) |
| `if … then … else` | `if x < 0 then 0 else x` | [Control flow](expressions-and-control.md) |
| `cond` | `cond \| x < 0 => 0 \| _ => x` | [Control flow](expressions-and-control.md) |
| `case … of` | `case n of \| 3 => 3 \| _ => 0` | [Control flow](expressions-and-control.md) |
| `match … with` | `match opt with \| None => 0 \| Some(x) => x` | [Control flow](expressions-and-control.md) |
| `loop … while … do … return` | `loop i = 0, acc = 0 while i < n do i + 1, acc + i return acc` | [Control flow](expressions-and-control.md) |
| `endloop` | `loop i = 0 while i < n do i + 1 endloop` | [Control flow](expressions-and-control.md) |
| `type … in` | `type Cell = Empty \| Black \| White in …` | [Types](values-and-types.md) |
| `len`, `[ ]`, `<-` | `len a`, `a[i]`, `a[i] <- v` | [Types](values-and-types.md) |
| `effect` | `effect ask : Unit -> Int in …` | [Effects](effects-and-handlers.md) |
| `perform` | `perform ask ()` | [Effects](effects-and-handlers.md) |
| `handle … with` | `handle e with { ask(x) => resume 21 }` | [Effects](effects-and-handlers.md) |
| `resume` | `choose(x) => resume 1 + resume 2` | [Effects](effects-and-handlers.md) |
| `quote`, `${ }` | `base + ${ quote(10) }` | [Staging](staging.md) |
| `genlet` | `genlet(quote(e))` | [Staging](staging.md) |
| `trait` / `instance` / `requires` | `trait StringEq a { … } in` | [Traits](traits.md) |
| `module … at …` | `module Util at app exposing (double) = …` | [Capabilities](modules-and-capabilities.md) |
| `mints` / `seals` / `exposing` | `module Os at boundary mints (winapi) = …` | [Capabilities](modules-and-capabilities.md) |
| `extern` | `extern "GetStdHandle" : U32 -> Int` | [Capabilities](modules-and-capabilities.md) |

## Operators

| Group | Operators | Notes |
|-------|-----------|-------|
| Arithmetic | `+` `-` `*` `/` `%` | plain |
| Arithmetic (wrapping) | `+%` `-%` `*%` | two's-complement wrap on overflow |
| Arithmetic (checked) | `+?` `-?` `*?` | checked overflow |
| Comparison | `==` `!=` `<` `<=` `>` `>=` | `Int` ordering, ordered `Float` |
| Boolean | `&&` `\|\|` `~` | short-circuit and/or, unary not |
| Bitwise | `&` `\|` `^` `<<` `>>` | on `Int` |
| Application | `f x` | juxtaposition, left-associative |
| Type arrow | `->` | `A -> B ! E` |
| Effect row | `! { }` | `String -> Unit ! {agent}` |
| Body / arm | `=>` | `fn`, `match`, `cond`, full handler clause |
| Tail-resume | `->` | handler clause sugar for `=> resume …` |
| Sum / match alt | `\|` | `None \| Some(a)` |
| Field access | `.` | `r.x` |
| Array store | `<-` | `a[i] <- v` |
| Mutable assign | `:=` | `x := v` |
| Comment | `--` | to end of line |

## Handler arrows

| Clause | Means |
|--------|-------|
| `op(x) => body` | full form — call `resume` yourself (abort, multi-shot, state) |
| `op(x) -> body` | sugar for `op(x) => resume body` — tail-resumptive |
| `return(v) => body` | post-process the final value |

## Effect labels

| Label | Means |
|-------|-------|
| `gc` | managed-heap allocation |
| `mem` | raw memory — peek / poke / fill / copy |
| `winapi` | raw Win32 FFI (layer-0 boundary) |
| `libc` / `libm` / `crt` | C runtime / math boundaries |
| `asm` | inline assembly |
| `agent` | the MCP / agent ask-and-tell channel |
| `insert` | compile-time let-insertion (`genlet`) |

An empty row `{ }` means **pure**. A tail variable `{label | r}` means "this,
plus whatever `r` is". See [Effects](effects-and-handlers.md).

## Types

| Type | Notes |
|------|-------|
| `Int` | unboxed 64-bit signed |
| `Float` | unboxed 64-bit IEEE |
| `Bool` | `true` / `false` |
| `String` | managed UTF-16 array |
| `Unit` | `()` |
| `(A, B, …)` | tuple |
| `{ x : A, … }` | record (built `{ x = v }`, accessed `r.x`) |
| `Array[a]` | dense mutable scalar array |
| `Code[a]` | staged code yielding `a` |
| `Option[a]` | `None \| Some(a)` |
| `Result[a, b]` | `Ok(a) \| Err(b)` |
| `List[a]` | `Nil \| Cons(a, List[a])` |
| `Ordering` | `Lt \| Eq \| Gt` |

## CLI — `locusc` (the driver)

| Command | Does |
|---------|------|
| `locusc run FILE` | JIT-compile and run |
| `locusc build FILE [-o EXE]` | build a standalone `.exe` |
| `locusc asm FILE [-o OUT.s]` | dump x86-64 assembly |
| `locusc effects FILE [--json]` | print the effect manifest |
| `locusc help [TOPIC] [--human]` | the built-in help index |
| `locusc help search QUERY` | search help |
| `locusc help service NAME` | one service's surface |
| `locusc help services` | list services |
| `locusc republish [DIR]` | write the embedded stdlib out for review |
| `locusc mcp` | serve the agent-facing MCP protocol over stdio |
| `locusc mcp-call TOOL [JSON]` | call one MCP tool and print JSON |

## CLI — `locus` (the front end)

| Command | Does |
|---------|------|
| `locus check FILE` | type-and-effect check; report type and row |
| `locus sema FILE` | dump the checked semantic model |
| `locus ir FILE` | dump the ANF intermediate representation |
| `locus ast FILE` | dump the parse tree |
| `locus help …` | the same help index as `locusc help` |

## Services at a glance

`Agent`, `Array`, `Bool`, `Console`, `Db`, `DocsFs`, `Fun`, `List`, `LocusEnv`,
`Math`, `Num`, `Option`, `Order`, `Random`, `Result`, `String`, `Time`. Full
tour in [The standard library](standard-library.md); exhaustive signatures via
`locusc help service NAME`.

---

*Back to [the guide index](index.md).*
