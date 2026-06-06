# Bindings and functions

Everything in Locus is built from two constructs: naming a value (`let`) and
abstracting over one (`fn`). This page covers both, plus the `do` block that
makes a sequence of them read like statements.

## `let … in`

`let name = expr in body` binds `name` to the value of `expr`, then evaluates
`body` with that name in scope. It is an expression — it has a value (the
`body`'s).

```locus
let x = 40 in x + 2               -- => 42
```

Chain them to build up a computation. Each `let` sees the ones before it:

```locus
let width  = 4 in
let height = 5 in
let area   = width * height in
area + 1                          -- => 21
```

This chaining is how a "single top-level expression" becomes a whole program:
it is just a long ladder of `let … in`.

## Sequencing effects with `let _ =`

When an expression is run only for its **effect** — it returns `Unit` and you
don't need the value — bind it to `_`:

```locus
let _ = console_writeln "first" in
let _ = console_writeln "second" in
0
```

The two writes happen in order; the program's value is `0`. This is the
fundamental sequencing move, and the `do` block below is sugar for exactly it.

## `do` blocks

A `do` block reads as statements separated by `;`. Each `let` binds; each bare
expression is run for its effect; the **last** expression is the block's value.

```locus
do {
  let x = 20;
  let y = x + 22;
  y
}                                 -- => 42
```

It desugars to the `let … in` ladder above — every statement still contributes
its real effects to the row — but it is easier to read when there are several
steps. The standard library uses `do` heavily inside handlers:

```locus
do {
  let _ = win_write_console s;
  let _ = win_write_unit 13;
  let _ = win_write_unit 10;
}
```

## Functions

A function is written `fn param: Type => body`. It is a value, like any other:

```locus
let double = fn x: Int => x + x in
double 21                         -- => 42
```

Application is **juxtaposition** — `double 21`, no parentheses around the
argument. Parentheses are only for grouping (`double (a + b)`).

### Currying — multiple arguments

A function takes exactly one argument. "Multiple arguments" is several functions
nested, each returning the next — *currying*:

```locus
let add = fn a: Int => fn b: Int => a + b in
add 20 22                         -- => 42
```

`add 20 22` is `(add 20) 22`: `add 20` is itself a function (it has captured
`a = 20`) waiting for `b`. The type of `add` is `Int -> Int -> Int`, which
associates to the right: `Int -> (Int -> Int)`.

### Partial application

Because of currying, you get partial application for free — apply some arguments
now, the rest later:

```locus
let add     = fn a: Int => fn b: Int => a + b in
let add_ten = add 10 in           -- a function Int -> Int
add_ten 32                        -- => 42
```

### Closures

A function captures the bindings in scope where it is written. `add_ten` above
closed over `a = 10`. Closures are ordinary heap values; capturing is why a
returned function can outlive the `let` that built it.

## Recursion: `let rec`

A function that calls itself uses `let rec`, and must be given its **full type**
— including the effect row — so the recursive call can be checked before the
body is finished:

```locus
let rec factorial : Int -> Int = fn n: Int =>
  if n <= 1 then 1 else n * factorial (n - 1)
in factorial 5                    -- => 120
```

When the recursion touches managed data — matching or building a sum type, for
instance — the type's row says so:

```locus
let rec list_len : List[a] -> Int ! {gc} =
  fn xs: List[a] =>
    match xs with
    | Nil       => 0
    | Cons(h, t) => 1 + list_len t
in …
```

The `! {gc}` is not decoration: it is the honest statement that walking the list
allocates. Leave it off and the checker rejects the definition.

> **Tail recursion lowers to a loop.** A recursive call in tail position (the
> last thing the function does) compiles to a jump, not a stack frame — so the
> accumulator-passing style used throughout the stdlib (`list_rev_onto`,
> `list_fold`) runs in constant stack. For counted iteration there is also a
> dedicated `loop` form; see [Expressions and control
> flow](expressions-and-control.md).

## Functions are values, and their type carries a row

A function value's type is `A -> B ! E`: argument `A`, result `B`, and the
**latent effect row** `E` that performing the call will unleash. A pure function
has an empty row (often left implicit); an effectful one names what it does:

```
console_writeln : String -> Unit ! {mem, winapi, gc}
factorial       : Int -> Int
```

You can read a function's whole contract — what it takes, what it returns, and
every power it exercises — from that one line. That is the property the rest of
the guide builds on.

— **[Next: Expressions and control flow →](expressions-and-control.md)**
