# The Phantom of the Handler

*Effects, handlers, and continuations in Locus — and how they leave no trace.*

> *Part I of a diptych. A zero-cost abstraction is a phantom: it haunts the
> compile — the types, the rows — and is gone from the runtime. This is the
> monad's ghost; [Part II](the-phantom-of-the-stage.md) is the comonad's.*

There is a particular magic trick at the heart of algebraic effects. You write a
program that **performs** operations — `perform console`, `perform get` — as if
some ambient context will service them. Then a **handler** wraps the program and
decides what those operations *mean*. The program never names its handler; the
handler never appears in the program's type once it's discharged. And if you do it
right, the handler isn't in the *machine code* either.

It's there while you reason. It's gone when you run. A phantom.

## The disappearing handler

Here is a handler that intercepts a `console` operation, resumes it, and returns a
constant:

```
handle perform console "x" with {
  console(s) => resume () ;
  return(y)  => 42
}
```

The `perform console "x"` is real. The handler is real. `resume` threads control
back. The type checker sees all of it — and then watches the handler **discharge**
the effect, so the whole expression's row shrinks to `! {}`: pure. Now compile it
and read the assembly (`locusc asm`):

```asm
__locus_main:
    movl  $42, %eax
    retq
```

That's the entire program. No `perform`. No handler. No `resume`. No string. The
`console` operation, the interception, the continuation — all of it evaporated,
leaving a single `mov`. The handler did its work at compile time and vanished.

This is the zero-cost promise made literal: *the cost of the abstraction is the
cost of not having it.* The handler is a phantom — present in the reasoning,
absent from the `.text`.

## When the phantom takes an encore

Not every handler can disappear so cleanly, and the reason is the most interesting
thing about effects: `resume` is a **continuation**, and you can call it more than
once.

```
effect choose : Unit -> Int in
handle perform choose () with {
  choose(x) => resume 1 + resume 2 ;
  return(y) => y * 10
}
```

`choose` is performed *once*, but the handler resumes it **twice** — with `1` and
with `2`. Each `resume` runs the continuation (here, the `return` clause, which
multiplies by ten), so the answer is `(1·10) + (2·10) = 30`. No amount of inlining
can express "run the rest of the program twice"; this needs a real, reified
continuation. In the assembly you can see it become a heap closure that gets
called twice:

```asm
    callq  locus_alloc        ; allocate the continuation closure
    callq  *%rax              ; resume 1  — run the continuation
    callq  *(%rsi)            ; resume 2  — run it again
```

No special runtime, no stack copying — the continuation is just a closure, and
calling it twice runs it twice. Pay-as-you-go: the phantom that needs to encore
pays for a closure; the one that doesn't pays nothing.

## Mutable state from a pure handler

The trick that still feels like sleight of hand: **mutable state with no mutable
cell.** State is just a handler that threads a value through `resume`. The handled
computation becomes a function of the state, and `get`/`put` pass it along:

```
effect State { get : Unit -> Int ; put : Int -> Unit } in
(handle (let a = perform get () in
         let r = perform put (a + 1) in
         perform get ()) with {
  get(u)    => fn s: Int => resume s s ;
  put(s2)   => fn s: Int => resume () s2 ;
  return(v) => fn s: Int => v
}) 0
```

`get` returns the current state; `put` replaces it; the whole thing is applied to
the initial state `0`. Run it: `get → 0`, `put(1)`, `get → 1`. The result is **1**
— and the program's type is `! {}`, **pure**. There is no heap cell, no `ref`, no
hidden mutation. The "state" exists only as a value flowing through continuations
that the compiler threads for you. A mutable abstraction with an immutable
implementation. Another phantom.

## The point

Algebraic effects let you write code against operations whose meaning is supplied
later, by a handler the code never sees. In Locus that handler is tracked in the
type — the effect **row** — so you always know what a function can do. And then,
when the handler is in force, it **discharges** the effect and, wherever it can,
vanishes from the generated code:

- a tail-resumptive handler folds into the control flow — `mov $42`;
- a multi-shot handler reifies its continuation only when it must — one heap
  closure, called as many times as `resume` is;
- a state handler threads a value through continuations — no cell at all.

You reason with the full cast on stage. The machine runs an empty one.

That's the monad's ghost. Its twin — code that writes code at compile time and
disappears just the same — is in [**The Phantom of the Stage**](the-phantom-of-the-stage.md).

---

*Reproduce it:*

```
locusc asm examples/handler.locus     # __locus_main: mov $42 ; ret
locusc run examples/multishot.locus   # 30 — resume twice
locusc run examples/state.locus       # 1  — state from a pure handler
```
