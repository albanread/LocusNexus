# Expressions and control flow

Locus has no statements in the imperative sense — everything is an expression
that produces a value. This page covers the branching forms (`if`, `cond`,
`case`, `match`), the `loop`, and the array operations that loops usually drive.

## Branching

### `if … then … else`

The two-armed choice. Both branches must have the **same type**, because the
whole `if` is an expression with one type:

```locus
let classify = fn n: Int =>
  if n < 0 then 0 - 1 else if n == 0 then 0 else 1
in classify 7                     -- => 1
```

There is no one-armed `if` (an `else` is mandatory) — a missing branch would
leave the expression without a value.

### `cond` — ordered guards

When you have a cascade of conditions, `cond` is tidier than nested `if`. Each
arm is `| test => result`, tried top to bottom; a final `| _ =>` is the
catch-all:

```locus
let sign = fn x: Int =>
  cond
  | x < 0 => 0 - 1
  | x > 0 => 1
  | _     => 0
in sign (0 - 5)                   -- => -1
```

### `case` — match on literal values

`case` dispatches on a value compared against literals:

```locus
let edges = fn shape: Int =>
  case shape of
  | 3 => 3
  | 4 => 4
  | _ => 0
in edges 4                        -- => 4
```

### `match` — destructure a sum

`match` is the important one: it takes a sum-type value apart, binding the
payload of whichever constructor matched. Arms are `| Constructor(bindings) =>
body`:

```locus
match Some(7) with
| None    => 0
| Some(x) => x                    -- => 7
```

A `match` must be **exhaustive** — cover every constructor — unless it ends in a
`| _ =>` wildcard. The checker rejects a match that could fall through. This is
how the standard library's combinators stay total:

```locus
let option_with_default = fn opt: Option[a] => fn default: a =>
  match opt with
  | None    => default
  | Some(x) => x
in …
```

## Loops

For counted iteration Locus has a `loop` form built around **accumulators that
update in lock-step**. It is not a general `while` with mutable variables — each
loop variable has an initializer and exactly one step expression, and the loop
is itself an expression with a value.

The shape:

```
loop v1 = init1, v2 = init2 while cond do step1, step2 return result
```

| Part | Meaning |
|------|---------|
| `v = init` | each loop variable and its starting value (comma-separated) |
| `while cond` | continue while this is true |
| `do step1, step2` | the next value of each variable, in order |
| `return result` | the expression yielding the loop's value when `cond` fails |

Summing an array — the canonical example, which lowers to a tight LLVM loop with
scalar accumulators (no stack, no allocation per iteration):

```locus
let a = [10, 20, 30, 40] in
loop i = 0, acc = 0 while i < len a do i + 1, acc + a[i] return acc
                                  -- => 100
```

Read it as: start `i = 0, acc = 0`; while `i < len a`, step to `i + 1` and
`acc + a[i]`; when the condition fails, return `acc`.

### `endloop` — a loop run only for effect

When the loop exists only for its side effects and has no meaningful result, end
it with `endloop` instead of `return`; its value is `Unit`:

```locus
let a = [0, 0, 0, 0] in
loop i = 0 while i < len a do (let _ = a[i] <- i * i in i + 1) endloop
```

Here the body stores `i*i` into each slot; the step is `i + 1`; the loop yields
`Unit`. This is the array-fill idiom from the stdlib's `array_fill_int`.

## Arrays

Arrays pair naturally with loops. The four operations:

| Operation | Syntax | Effect |
|-----------|--------|--------|
| literal | `[1, 2, 3]` | `gc` (allocates) |
| length | `len a` | pure |
| index | `a[i]` | bounds-checked |
| store | `a[i] <- v` | in-place update |

```locus
let a = array_make_int 3 0 in     -- a length-3 array of zeros
let _ = a[0] <- 10 in
let _ = a[1] <- 20 in
let _ = a[2] <- 30 in
loop i = 0, acc = 0 while i < len a do i + 1, acc + a[i] return acc
                                  -- => 60
```

`array_make_int n v` (from the Array service) builds an `n`-element array filled
with `v` — use it when the length is only known at runtime. Indexing is always
bounds-checked; an out-of-range access is a runtime error, not silent
corruption.

> **Why a dedicated `loop` and not just `while`?** The lock-step accumulator
> shape is exactly what lowers to LLVM's phi-node loop form, so it is both the
> readable way to write iteration *and* the fast one — the managed values in
> accumulators are kept rooted for the GC automatically. Tail recursion
> ([previous page](bindings-and-functions.md)) is the other route, and lowers
> the same way.

— **[Next: Effects and handlers →](effects-and-handlers.md)**
