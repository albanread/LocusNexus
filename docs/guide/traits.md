# Traits

A **trait** is a named set of operations that a type can implement — the way
Locus expresses "this works for any type that knows how to compare / show /
build itself". Traits are one-parameter and resolved at compile time, so they
add abstraction without adding runtime dispatch cost.

## Declaring a trait

`trait Name a { method : signature ; … } in body` introduces a trait over a
single type variable `a`. Each method has a type — and, like any function, may
carry an effect row:

```locus
trait StringShow a { string_show : a -> String } in
trait StringEq   a { string_eq   : a -> a -> Bool ! {gc} } in …
```

`string_eq`'s `! {gc}` is part of the contract: comparing two of these may
allocate, and every instance must honour that.

## Providing an instance

`instance Name Type { method = impl ; … } in body` implements a trait for a
concrete type. It must supply **exactly** the declared methods:

```locus
instance StringShow String { string_show = fn s => s } in
instance StringEq   String { string_eq = fn a: String => fn b: String => string_equals a b } in …
```

Now `string_show` and `string_eq` resolve, for `String` arguments, to these
implementations — chosen by the type at the call site, with no dictionary passed
at runtime.

## Trait dependencies: `requires`

A trait can require another, meaning "you can only implement me if you've also
implemented that one". Ordering requires equality:

```locus
trait StringOrd a requires StringEq a { string_ordering : a -> a -> Int ! {gc} } in
```

To write `instance StringOrd String`, a `StringEq String` instance must already
exist. This lets ordered operations lean on equality without re-declaring it.

## The standard library's traits

The stdlib ships a small, practical set:

| Trait | Method(s) | For |
|-------|-----------|-----|
| `StringEq a` | `string_eq : a -> a -> Bool ! {gc}` | equality |
| `StringOrd a` *(requires `StringEq`)* | `string_ordering : a -> a -> Int ! {gc}` | ordering |
| `StringShow a` | `string_show : a -> String` | rendering to text |
| `ArrayMake a` | `array_make : Int -> a -> Array[a] ! {gc}` | building a filled array |

`ArrayMake` is how `array_make` works for more than one element type while the
underlying storage stays on the compiler's unboxed scalar-array path:

```locus
trait ArrayMake a { array_make : Int -> a -> Array[a] ! {gc} } in
instance ArrayMake Int { array_make = array_make_int } in …
```

## When to reach for a trait

Traits are deliberately modest here — they exist for genuinely
type-parameterised operations (equality, ordering, show, construction), not as a
general object system. If a piece of behaviour is specific to one type, a plain
`let`-bound function is simpler and clearer. Reach for a trait when the *same*
operation must work across several types and you want the type, not a runtime
tag, to pick the implementation.

— **[Next: The standard library →](standard-library.md)**
