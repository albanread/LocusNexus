# Lexical structure

This page covers the surface: how source text is tokenized — comments,
literals, identifiers, keywords, and the full operator set. It is reference
material; skim it now and come back when a symbol puzzles you.

## Source files

A Locus source file is UTF-8 text. A leading byte-order mark is skipped, so
files saved by Windows editors load cleanly. The file conventionally ends in
`.locus` and contains a single top-level expression (see
[Bindings and functions](bindings-and-functions.md) for how `let ... in`
chains keep that one expression arbitrarily large).

## Comments

One form: `--` runs to the end of the line, ML/Dylan style.

```locus
-- this is a comment
let x = 40 in   -- and so is this
x + 2
```

There is no block-comment form; comment out a region line by line.

## Literals

| Kind | Examples | Notes |
|------|----------|-------|
| `Int` | `42`, `0`, `2147483647` | 64-bit signed. |
| `Int` (hex) | `0x80`, `0xEF`, `0x40000000` | `0x` / `0X` prefix; handy for byte masks. |
| `Float` | `1.25`, `0.0`, `3.0`, `1.0e-3` | 64-bit IEEE; scientific notation allowed. |
| `Bool` | `true`, `false` | |
| `String` | `"hello"`, `"line\n"` | UTF-16 at runtime; escapes `\n`, `\t`, `\"`, `\\`. |
| `Unit` | `()` | the empty tuple; the "no useful value" value. |

`Int` and `Float` are distinct types with no implicit coercion. The arithmetic
operators are shared and resolved by type: `2 + 3` is integer arithmetic, and
`2.0 + 3.0` is floating-point — you do not write a separate `+.`. The standard
library relies on this; for example `array_sum_float` accumulates with the same
`+` you'd use on `Int`.

## Identifiers

Identifiers start with a letter and continue with letters, digits, and
underscores. The standard library uses `snake_case` for values
(`array_sum_int`, `console_writeln`) and `PascalCase` for types, constructors,
and modules (`Option`, `Some`, `Console`). A single `_` is the conventional
"don't care" name, used constantly to sequence effects:

```locus
let _ = console_writeln "side effect" in 0
```

## Keywords

These words are reserved by the grammar:

```
let   rec   in   fn   do   if   then   else   cond   case   of
match   with   loop   while   return   endloop   type   effect
perform   handle   resume   trait   instance   requires   module
at   mints   seals   exposing   extern   quote   splice   genlet   len
```

Type and constructor names (`Int`, `Float`, `Bool`, `String`, `Unit`, `Array`,
`Code`, `Option`, `Some`, `None`, `Ok`, `Err`, …) are ordinary identifiers
provided by the language and standard library, not reserved keywords.

## Operators

Locus has a conventional operator set, with one twist worth calling out:
integer arithmetic comes in **three flavours**.

### Arithmetic — plain, wrapping, checked

| Plain | Wrapping | Checked | Meaning |
|-------|----------|---------|---------|
| `+` | `+%` | `+?` | addition |
| `-` | `-%` | `-?` | subtraction |
| `*` | `*%` | `*?` | multiplication |
| `/` | | | division |
| `%` | | | signed remainder |

Plain `+ - *` are the default. The `%`-suffixed forms **wrap** on overflow
(two's-complement, no check); the `?`-suffixed forms are **checked**. Pick the
one whose overflow behaviour you mean. Division `/` and remainder `%` are signed
integer operations; for floating-point remainder use the Math service's `fmod`.

```locus
let a = 1000000 * 1000000 in   -- plain
let b = 200 +% 100 in          -- wrapping
a - b
```

### Comparison and Boolean

| Operator | Meaning |
|----------|---------|
| `==` `!=` | equality / inequality |
| `<` `<=` `>` `>=` | signed `Int` ordering, and ordered `Float` comparison |
| `&&` `\|\|` | short-circuit Boolean *and* / *or* |
| `~` | unary Boolean negation |

Locus deliberately uses `~` for "not", *not* `!` — because `!` already means a
type's effect row (`A -> B ! {mem}`). The Bool service also offers `bool_and`,
`bool_or`, `bool_xor`, and `bool_not` as ordinary functions when you want them.

### Bitwise

| Operator | Meaning |
|----------|---------|
| `&` | bitwise AND |
| `\|` | bitwise OR |
| `^` | bitwise XOR |
| `<<` `>>` | left / right shift |

These operate on `Int`. Note `\|` is overloaded: between expressions it is
bitwise OR; in a `type` declaration or a `match` it separates alternatives.
Context disambiguates.

### Structural and special

| Token | Meaning |
|-------|---------|
| `=` | binds a name in `let` / `instance` / record fields |
| `:` | type annotation (`fn x: Int => …`) |
| `:=` | assignment to a mutable binding |
| `->` | function-type arrow, and the tail-resumptive handler arm |
| `=>` | `fn` body, `match` / `cond` / `case` arm, full handler arm |
| `\|` | sum-type and match alternative separator |
| `!` | a type's latent effect row: `A -> B ! {mem}` |
| `.` | record field access: `r.x` |
| `,` | tuple / argument / effect-row separator |
| `[ ]` | type parameters `Array[Int]`, array literals `[1, 2, 3]`, indexing `a[i]` |
| `<-` | in-place array store: `a[i] <- v` |
| `${ }` | splice — run a generator at compile time (see [Staging](staging.md)) |

The two arrows are the single most common source of confusion, so fix them
early: **`=>` is for ordinary bodies** (a `fn`, a `match` arm), and **`->` is a
type arrow** *and* a special tail-resumptive shorthand inside handlers. The
[Effects](effects-and-handlers.md) page explains the handler case in full.

— **[Next: Values and types →](values-and-types.md)**
