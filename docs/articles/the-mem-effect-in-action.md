# The `mem` effect in action: a UTF-16 → UTF-8 transcoder, down to the assembly

Most languages make you choose. You get a *high-level* language — garbage
collection, closures, a rich type system, maybe algebraic effects — *or* you get
a *systems* language where you can read a `u16` out of a buffer and write a byte
back, and the compiler gets out of your way.

Locus is both at once, and it doesn't need an `unsafe` keyword to do it. The trick
is that the low-level power is **an effect**. Reading and writing raw memory is
real, and it is tracked: any code that does it has `mem` in its type. Up high, a
`String` is an immutable value you can't poke at. Down low, you can take it apart
a code unit at a time — and the instant you do, your function's type says so.

This article walks one program all the way down: a real UTF-16 → UTF-8 transcoder,
written in Locus, using nothing but the `mem` capability — no `WideCharToMultiByte`,
no runtime helper. We'll read its type, then read the x86-64 it compiles to.

## The two worlds, and the seam between them

```
        High-level world  (pure, GC, closures, effects, handlers)
                  |
                  |   ! {mem}
                  v
        Low-level world   (raw memory, pointers, FFI buffers)
```

That arrow is a *type*, not a wall. Here is the whole positioning in one
signature — the transcoder's worker function:

```
go : String -> Int -> Int -> Int -> Int ! {mem}
```

That `! {mem}` is the seam. It's an ordinary effect row — the same machinery that
tracks `console`, `winapi`, or a user-declared effect — and it says: *calling this
function touches raw memory.* You cannot call `go` from a function claiming to be
pure and have it type-check. The capability isn't a back door carved out of the
type system (Rust's `unsafe`); it's a first-class fact *in* the type system.

**This is the safety `unsafe` can't give you.** An `unsafe` block is invisible in
a function's signature — `fn parse(&[u8]) -> T` looks identical whether or not it
dereferences a raw pointer inside, so safety rests on the author's audit. In Locus
the access is in the *type*, and the `(bind)` rule propagates it: every caller,
transitively, carries `mem` until something explicitly discharges it. "Safe-looking
code that secretly touches memory" is not expressible.

None of this is special-cased for memory. `mem` is an ordinary **world label**
([calculus §1.1](../calculus.md)) — the same kind of label as `console` or `gc`
("touches the managed heap") — and it enters and propagates by the two core effect
rules ([calculus §2.1](../calculus.md)): `(perform)` adds the label to the row when
a primitive runs, and `(bind)` takes the row **union** across a sequence. That
union is the whole story of why `mem` is contagious — it flows up through every
caller until a handler or a seal removes it.

So we get the honest version of the old systems-programming bargain. The power to
deconstruct a string into bytes is right here — but it can't hide.

## The accessor

The surface for that power is a subscript:

- `s[i]` — read element `i` of `s`.
- `out[j] <- v` — write `v` to element `j` of `out`.

The element *width* comes from the base's type. A `String` is UTF-16, so `s[i]` is
a 16-bit code **unit** (no manual `* 2` — the stride is implicit). A raw `Int`
address — like a buffer from `VirtualAlloc` — is byte-addressed, so `out[j]` is a
single byte. Both forms desugar to the `mem` primitives (`peek`/`poke`) and both
carry `! {mem}`.

(Note what `s[i]` is *not*: it is not "the i-th character." UTF-16 has surrogate
pairs; Unicode has grapheme clusters. `s[i]` is the i-th raw *unit*, honestly
low-level, honestly effectful. The high-level "string as text" view stays
separate — and immutable.)

## The transcoder

UTF-8 encoding is a 1/2/3/4-byte split, pure bit-twiddling. `emit` writes the
encoding of one code point and returns the new write cursor:

```
let emit = fn out: Int => fn j: Int => fn cp: Int =>
  if cp < 0x80 then
    (let _ = out[j] <- cp in
     j + 1)
  else if cp < 0x800 then
    (let _ = out[j]     <- (0xC0 | (cp >> 6)) in
     let _ = out[j + 1] <- (0x80 | (cp & 0x3F)) in
     j + 2)
  else if cp < 0x10000 then
    (let _ = out[j]     <- (0xE0 | (cp >> 12)) in
     let _ = out[j + 1] <- (0x80 | ((cp >> 6) & 0x3F)) in
     let _ = out[j + 2] <- (0x80 | (cp & 0x3F)) in
     j + 3)
  else
    (let _ = out[j]     <- (0xF0 | (cp >> 18)) in
     let _ = out[j + 1] <- (0x80 | ((cp >> 12) & 0x3F)) in
     let _ = out[j + 2] <- (0x80 | ((cp >> 6) & 0x3F)) in
     let _ = out[j + 3] <- (0x80 | (cp & 0x3F)) in
     j + 4)
in
```

`go` walks the wide string from unit `i`, writing UTF-8 from byte `j`, until the
NUL terminator. The interesting case is the astral planes: a *high surrogate* is
combined with the following *low surrogate* (a one-unit lookahead, `s[i + 1]`)
into a single code point.

```
let rec go : String -> Int -> Int -> Int -> Int ! {mem} =
  fn s: String => fn out: Int => fn i: Int => fn j: Int =>
    let unit = s[i] in
    if unit == 0 then
      j
    else if unit < 0xD800 then
      go s out (i + 1) (emit out j unit)
    else if unit < 0xDC00 then
      (let lo = s[i + 1] in
       let cp = 0x10000 + ((unit - 0xD800) << 10) + (lo - 0xDC00) in
       go s out (i + 2) (emit out j cp))
    else
      go s out (i + 1) (emit out j unit)
in
```

That `let rec go : … ! {mem}` is worth a pause. Recursion needs a type annotation,
and `go` is effectful — so we have to be able to *write down* an effectful function
type. We can: `! {mem}` on the arrow. (This is the kind of thing the calculus
demands of the surface — if effects live in the type, the type syntax must be able
to spell them.)

The driver allocates a buffer and transcodes:

```
let alloc = extern "VirtualAlloc" : Int -> Int -> I32 -> I32 -> Int in
let out = alloc 0 256 0x3000 0x04 in
go "Hello, 世界 🎉" out 0 0
```

`"Hello, 世界 🎉"` is 7 ASCII bytes + two CJK ideographs (3 bytes each) + a space +
a surrogate-pair emoji (4 bytes) = **18** UTF-8 bytes, which is the program's
result. It is verified byte-for-byte in the test suite: `é` → `C3 A9`, `世` →
`E4 B8 96`, `🎉` → `F0 9F 8E 89`.

## What the type says

```
$ locus check examples/utf16_to_utf8.locus
ok
  type  Int
  row   {mem, winapi}
  stage 0
```

`Int ! {mem, winapi}`. The `winapi` is the `VirtualAlloc`; the `mem` is every
`s[i]` and `out[j] <- …`. There is nowhere a raw memory access could hide: it
would show up in this row. That's the whole pitch — a systems program whose
systems-ness is on the label.

## What it compiles to

`locusc asm` dumps the x86-64 — the same code the `.exe` carries. The high-level
constructs leave *no* abstraction tax. Here is `go`'s loop:

```asm
    movzwl  (%rdx,%r14,2), %esi      ; unit = s[i]   — the ×2 stride is the ',2' scale
    testq   %rsi, %rsi               ; if unit == 0
    je      .LBB7_6                  ;   → return j
    cmpl    $55296, %esi             ; unit < 0xD800 ?
    jge     .LBB7_3
    ...                              ; BMP: emit, i + 1
.LBB7_3:
    cmpl    $56319, %esi             ; high surrogate? (≤ 0xDBFF)
    jg      .LBB7_2
    movzwl  2(%rdx,%r14,2), %eax     ; lo = s[i + 1]
    shll    $10, %esi                ; unit << 10
    addq    %rax, %rsi               ;   + lo
    addq    $-56613888, %rsi         ;   + (0x10000 − (0xD800<<10) − 0xDC00), folded
```

Read that and the source line up one-to-one. `s[i]` is a **single** `movzwl` with
the wide stride expressed as the `,2` in the addressing mode — the accessor's
implicit `* 2` cost *nothing*. The surrogate decode,
`0x10000 + ((unit - 0xD800) << 10) + (lo - 0xDC00)`, three terms in the source,
became `shll $10; addq %rax; addq $-56613888` — LLVM folded the three constants
into one.

And `emit`'s 4-byte path (the `🎉` case):

```asm
    shrl    $18, %r8d                ; cp >> 18
    orb     $-16, %r8b               ; 0xF0 | …            (0xF0 = -16 as a byte)
    movb    %r8b, (%rcx,%rax)        ; out[j]     <- …
    movl    %edx, %r8d
    shrl    $12, %r8d                ; cp >> 12
    andb    $63, %r8b                ; … & 0x3F
    orb     $-128, %r8b              ; 0x80 | …
    movb    %r8b, 1(%rcx,%rax)       ; out[j + 1] <- …
    ...                              ; j + 2, j + 3
    addq    $4, %rax                 ; return j + 4
```

`0xF0 | (cp >> 18)` is `shrl $18; orb $-16`. `0x80 | ((cp >> 12) & 0x3F)` is
`shrl $12; andb $63; orb $-128`. The byte stores `out[j + k] <- v` are single
`movb`s into `k(%rcx,%rax)` — the buffer base plus the cursor plus the constant
offset, all in one address mode. This is the code you'd write by hand in C, or in
assembly.

## Why this matters

Plenty of languages have *one* of these. Effect systems (Koka, Eff, Frank,
OCaml 5, Unison) track effects in types. Systems languages (C, Rust, Zig) give you
raw pointers and zero-overhead loads and stores. Managed languages give you GC,
closures, and a rich type system. What's new here — and, as far as we know,
genuinely new — is having all three *at once*, with no seam torn open between them:

1. **Raw, pointer-level memory access is tracked as an effect.** Not the safe,
   typed-reference kind some effect systems already model — the *systems* kind:
   arbitrary addresses, FFI buffers, `peek`/`poke`. And not a block you enter
   (`unsafe { … }`) or a separate dialect, but an ordinary label in the same row
   that tracks IO and user effects. A dereference is `perform`-shaped, and it
   shows in the type like anything else.

2. **It compiles to zero-overhead code.** The tracking is entirely compile-time and
   type-level. `s[i]` is one `movzwl` with the stride folded into the address mode;
   `out[j] <- v` is one `movb`. The effect leaves *no* runtime trace — no boxing, no
   dynamic check, no indirection. You pay for the memory access, and nothing at all
   for the fact that it was tracked.

3. **The high-level model stays intact.** The same program has closures, recursion,
   a real type system, and — elsewhere in the same compiler — algebraic effects and
   handlers that fold to a single instruction when discharged. Dropping to raw
   memory doesn't drop you out of the language.

The usual move is to bolt a `mem`-like capability on as an escape hatch — a place
where the rules stop applying. Locus does the opposite: the low level lives *inside*
the type system, described by the same algebra as everything else. The systems
programmer loses no power, and gains a type that cannot lie about what their code
touches.

## The point

One program, read two ways. Its **type** says `Int ! {mem, winapi}` — a
high-level value computed by code that, somewhere inside, touches raw memory and
the OS, and says so. Its **assembly** is a tight loop of `movzwl`, `shll`, and
`movb` — exactly what you'd write by hand. Nothing was lost going down, and
nothing can be hidden coming back up. That round trip — the signature and the
instructions telling the same true story — is what Locus is for.

---

*Reproduce it:*

```
locus  check examples/utf16_to_utf8.locus   # Int ! {mem, winapi}
locusc run   examples/utf16_to_utf8.locus   # exit code 18
locusc asm   examples/utf16_to_utf8.locus   # the x86-64 above
```
