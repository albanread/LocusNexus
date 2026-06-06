# Values and types

Locus is statically typed with inference: you rarely write a type, but one is
always there, and the compiler will tell you what it inferred. This page is the
catalogue of what values exist — the scalars, the ways to combine them, and the
handful of types the standard library defines.

## Scalar types

| Type | Values | Runtime |
|------|--------|---------|
| `Int` | `42`, `0x80`, `0 - 7` | unboxed 64-bit signed integer |
| `Float` | `3.14`, `1.0e-3` | unboxed 64-bit IEEE double |
| `Bool` | `true`, `false` | unboxed |
| `String` | `"hello"` | managed array of UTF-16 code units |
| `Unit` | `()` | zero-size; the result of effectful statements |

`Int`, `Float`, and `Bool` are **unboxed scalars** — they live in registers and
never touch the heap, so arithmetic on them allocates nothing and reports no
`gc` effect. `String` is a *managed* value (it lives on the GC heap), which is
why building one shows `gc` in the effect row.

There is no implicit conversion between `Int` and `Float`. Negate an integer by
subtracting from zero (`0 - n`); the unary `~` is Boolean negation only.

## Tuples

A tuple packs several values of possibly-different types into one. Build with
parentheses, take apart with a `let` pattern:

```locus
-- a function returning two results at once
let point = fn x: Int => fn y: Int => (x, y) in
let (x1, y1) = point 3 4 in
let (x2, y2) = point 5 6 in
(x1 + x2) * 10 + (y1 + y2)        -- => 90
```

The type of `point 3 4` is `(Int, Int)`. Tuples nest freely, return from
functions, and mix element types (`(Int, Bool, String)`). A destructure with
the wrong arity or shape is a compile error. At runtime a tuple is a small heap
struct, so constructing one is a `gc` effect.

## Records

Records are products with **named** fields. Build with `{ field = value, … }`,
project with `.`:

```locus
let point = fn x: Int => fn y: Int => { x = x, y = y } in
let a = point 3 4 in
let b = point 5 6 in
(a.x + b.x) * 10 + (a.y + b.y)    -- => 90
```

Fields are kept sorted by name, so `{x=1, y=2}` and `{y=2, x=1}` are the *same*
record type. Names are resolved to slots at compile time — `a.x` is a plain
load, no dictionary, no overhead — and access chains (`pt.center.x`). At
runtime a record is exactly a tuple of its sorted field values.

## Sum types

A sum type (a.k.a. tagged union, variant, enum) is a value that is *one of*
several shapes. Declare it with `type`, where `|` separates the constructors and
a constructor may carry a payload:

```locus
type Cell = Empty | Black | White in
match Black with
| Black => 1
| _     => 0
```

Constructors with a payload are written `Name(types)`:

```locus
type Shape = Dot | Circle(Int) | Rect(Int, Int) in
let area = fn s: Shape =>
  match s with
  | Dot         => 0
  | Circle(r)   => 3 * r * r
  | Rect(w, h)  => w * h
in area (Rect(4, 5))              -- => 20
```

`match` consumes a sum (see [Expressions and control
flow](expressions-and-control.md)). Constructing a payload-carrying value
allocates, so it carries `gc`.

### Type parameters

A lowercase name in a type is a **type variable** — the type is generic over it.
The standard library's containers are defined this way. `Option[a]` works for
any `a`:

```locus
type Option[a] = None | Some(a) in …
```

Write the parameter in square brackets at the use site too: `Option[Int]`,
`Array[Float]`, `List[String]`.

## The standard library's types

Four sum types are predefined and in scope everywhere. You will use them
constantly.

| Type | Definition | Use |
|------|------------|-----|
| `Option[a]` | `None \| Some(a)` | a value that might be absent |
| `Result[a, b]` | `Ok(a) \| Err(b)` | a success or an error |
| `List[a]` | `Nil \| Cons(a, List[a])` | a singly-linked list |
| `Ordering` | `Lt \| Eq \| Gt` | the result of a comparison |

Each comes with a family of combinators — `option_map`, `option_with_default`,
`list_map`, `list_fold`, and so on — covered in [The standard
library](standard-library.md). Here is `Option` in action:

```locus
let safe_div = fn a: Int => fn b: Int =>
  if b == 0 then None else Some(a / b)
in
match safe_div 84 2 with
| None    => 0
| Some(q) => q                    -- => 42
```

## Arrays

`Array[a]` is a dense, bounds-checked, mutable block of scalars. Build a literal
with `[ … ]`, ask its length with `len`, read with `a[i]`, and store in place
with `a[i] <- v`:

```locus
let a = [10, 20, 30, 40] in
let _ = a[1] <- 99 in
len a + a[1]                      -- => 4 + 99 = 103
```

Arrays are managed values (so they carry `gc`), and indexing is bounds-checked.
`String` is, under the hood, a managed array of 16-bit code units, which is why
the two share so much vocabulary. See [Expressions and control
flow](expressions-and-control.md) for the loop idioms that drive them.

## `Code[a]`

One more type completes the picture: `Code[a]` is *a piece of program that, when
run, produces an `a`*. It is what `quote` builds and `splice` consumes — the
currency of compile-time code generation. You can ignore it until you reach
[Staging](staging.md), where it is the whole story.

## Inference and annotations

You almost never annotate a type. The exceptions are:

- A **function parameter** in a `fn`: `fn x: Int => …`. (This anchors
  inference and documents intent.)
- A **`let rec`** binding, which needs its full type written out so the
  recursive call can be checked:

  ```locus
  let rec list_len : List[a] -> Int ! {gc} =
    fn xs: List[a] => match xs with | Nil => 0 | Cons(h, t) => 1 + list_len t
  in …
  ```

Everywhere else, the type — *and the effect row* — is inferred and reported by
`locus check` and `locusc effects`.

— **[Next: Bindings and functions →](bindings-and-functions.md)**
