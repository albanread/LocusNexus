# Locus for agents

A compact field guide to driving **Locus** from an MCP client or the
`locusc` command line. Start at the MCP table, fall back to the CLI, and
keep the operator and keyword tables handy while you write code.

## What Locus is

Locus is an **ML-flavoured** language: programs are built from `let`
bindings, curried `fn` lambdas, sum types, and `match`, and the compiler
infers the types for you. What sets it apart is **effects**. Every
capability a computation can use — heap allocation, talking to its agent
host, calling the OS — is tracked by the type system and printed as an
**effect row** beside the value's type:

```locus
-- ask the agent host for a move, then echo it back
let move = agent_ask_text "your move?" in
agent_tell_text move
-- inferred type:  Unit ! {agent, gc}
```

The `! {agent, gc}` *is* the row: this code may talk to the agent
(`agent`) and allocate (`gc`), and nothing else. `handle` discharges an
effect and removes it from the row; `module`s at the `boundary` layer
mint raw powers (`winapi`, `libc`, …) and `seal` them behind safe
services. Run `locusc effects FILE` to see the manifest before you run
anything.

**Help is built in.** Both the CLI and the MCP server ship an
agent-oriented help index — start with `locusc help agent` (or the
`help_overview` MCP tool), then `help_search`, `help_topic`,
`help_service`, and `help_remind` to drill in. You never have to guess
syntax: ask the help index.

## MCP commands

The MCP server (`locusc mcp`) exposes these tools. Discovery first,
then check, then run.

| Tool | What it does |
|------|--------------|
| `help_overview` | Agent-start overview of syntax, operations, and services — read this first |
| `help_search` | Search the embedded help index by query |
| `help_topic` | Fetch one help topic by id |
| `help_service` | Help for a stdlib service / module / function |
| `help_remind` | Compact reminder card for one topic |
| `check` | Type-check and lower a program **without** running it |
| `effects` | Return the effect manifest (what the program may touch) |
| `emit_ir` | Emit the Locus ANF IR text |
| `emit_asm` | Emit host x86-64 assembly |
| `list_stdlib_services` | List the embedded stdlib services and boundary modules |
| `explain_diagnostic` | Explain a stable Locus diagnostic code |
| `run` | JIT-compile and run; returns the `i64` result |
| `build` | Build a standalone Windows executable |
| `materialize_target` | Materialize a target artifact |
| `run_agent_text` | JIT-run with a queued ask/response channel and a transcript |
| `agent_session_start` | Start a live session; runs until the first `agent_ask_text` |
| `agent_session_reply` | Reply to the current prompt and wait for the next ask |
| `agent_session_status` | Long-poll the live session: transcript, latest fields, result |
| `agent_session_close` | Close a live session and release it |

Call from the shell without a long-lived server via
`locusc mcp-call <tool> [JSON_ARGS]`, e.g.
`locusc mcp-call run --source "let x = 40 in x + 2"`.

## locusc commands

| Command | What it does |
|---------|--------------|
| `locusc run FILE` | JIT-compile and run FILE (side effects happen) |
| `locusc build FILE [-o EXE]` | AOT-compile FILE to a standalone `.exe` |
| `locusc asm FILE [-o OUT.s]` | Dump the generated x86-64 assembly |
| `locusc effects FILE [--json]` | Print FILE's effect manifest |
| `locusc help [TOPIC] [--human]` | Discover syntax, operations, and services |
| `locusc help search QUERY` | Search the help index |
| `locusc help service NAME` | Help for one published service |
| `locusc help services` | List published services |
| `locusc help remind TOPIC` | Compact reminder card |
| `locusc republish [DIR]` | Write the embedded stdlib to DIR for review |
| `locusc mcp` | Serve the agent-facing MCP protocol over stdio |
| `locusc mcp-call COMMAND …` | Call the MCP server once and print JSON |

The front-end `locus` binary adds source-inspection commands:
`locus check`, `locus sema`, `locus ir`, and `locus ast`.

## Operators

| Operator | Means | Example |
|----------|-------|---------|
| `+ - * /` | arithmetic | `total * 2 + 1` |
| `%` | signed Int remainder | `n % 2 == 0` |
| `== !=` | equality | `move == "pass"` |
| `< <= > >=` | ordering | `amount <= target` |
| `&& \|\|` | short-circuit Bool | `ready && ~busy` |
| `~` | unary Bool negation | `~ done` |
| `f x` | function application (juxtaposition) | `add 20 22` |
| `->` | function type arrow | `Int -> Int` |
| `! { }` | effect row on a type | `String -> Unit ! {agent}` |
| `=>` | `fn` body / match arm | `fn x: Int => x + 1` |
| `\|` | sum-type / match alternative | `None \| Some(a)` |
| `[ ]` | type parameter / array literal | `Option[a]`, `[1, 2, 3]` |
| `a[i]` | array index | `ways[amount]` |
| `a[i] <- v` | array update | `cells[0] <- White` |
| `--` | line comment | `-- explain a step` |

## Keywords

| Keyword | Role | Example |
|---------|------|---------|
| `let … in` | bind a value | `let x = 40 in x + 2` |
| `let rec` | recursive binding (needs a type) | `let rec fib : Int -> Int = fn n: Int => …` |
| `fn` | curried lambda | `fn a: Int => fn b: Int => a + b` |
| `do { … }` | sequence statements | `do { let x = 20; x + 22 }` |
| `if … then … else` | two-armed choice | `if x < 0 then 0 else x` |
| `cond` | ordered guard sugar | `cond \| x < 0 => 0 \| _ => x` |
| `match … with` | pattern match | `match opt with \| None => 0 \| Some(x) => x` |
| `type … in` | declare a sum type | `type Cell = Empty \| Black \| White in …` |
| `loop … while … do … return` | accumulator loop | `loop i = 0, acc = 0 while i < 10 do i + 1, acc + i return acc` |
| `endloop` | statement loop (Unit result) | `loop i = 0 while i < n do i + 1 endloop` |
| `effect` | declare an operation | `effect ask : String -> String in …` |
| `perform` | invoke an effect op | `perform ask "move?"` |
| `handle … with` | discharge an effect | `handle e with { ask(x) -> x + 1 }` |
| `resume` | continue a handled op | `ask(x) -> resume (x + 1)` |
| `module … at … =` | module and its layer | `module Util at app exposing (double) = …` |
| `mints / seals / exposing` | capability control | `module Os at boundary mints (winapi) = …` |
| `trait / instance` | typed generic operations | `trait Show a { show : a -> String } in …` |
