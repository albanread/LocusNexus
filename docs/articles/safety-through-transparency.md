# Safety through transparency: a lead, not a leash

There are two ways to keep code from doing something you didn't intend. You can
build a **wall** — a sandbox, a permission prompt, a syscall filter — that assumes
the code on the other side might be hostile and stops it by force. Or you can make
everything the code does **visible**, so that an unintended reach is *seen* — in
review, in a diff, in the type — and corrected before it matters.

The first is a *leash*. It's for an adversary: it restrains, it yanks, and it has
to anticipate every move. The second is a *lead*. It's for a companion you trust
but who can wander — a junior teammate, an AI colleague — and it works not by
restraint but by **direction and daylight**. This article is about the second one,
which is the kind Locus is built for.

Two things first, because they matter.

**This is not a sandbox.** It does not contain malice. A determined adversary, a
compromised dependency, a memory-corruption exploit — none of that is what this
addresses, and nothing here is a claim about it.

**Real sandboxes are still your friend.** Most serious deployments run inside
genuine isolation at several levels — OS accounts, containers, VMs, seccomp,
hypervisors — and you should keep all of it. Locus's transparency operates at a
level those can't reach: the level of *code you are reading* and *changes you are
reviewing*. A container can stop a process from opening a socket; it can't tell
you, in a pull request, that a function which used to be pure now touches the
network. The type can. They stack; they don't compete.

So we won't talk about "security." We'll talk about two ordinary, valuable things:
**auditability** — effects you can review — and **mistake prevention** — a
guardrail a team builds for its own work, that warns in development and holds the
line in production.

## Everything the code does is in its type

Locus tracks effects in the type. Touching the terminal is `console`; raw memory is
`mem`; the managed heap is `gc`; the OS boundary is `winapi`. Any code that does one
of these has the label in its row, and the row flows up through every caller until
something explicitly discharges it ([the `mem` article](the-mem-effect-in-action.md)
walks this end to end; [`capabilities.md`](../capabilities.md) covers the layering).

The consequence is a property worth saying plainly: **a program's complete
interaction with the world is printed in its type.** There is no off-the-books I/O,
no `unsafe` block that's invisible in a signature. So you can ask the compiler what
a program touches and get the whole truth:

```
$ locusc effects hello.locus
hello.locus
  type    : Unit
  effects : { winapi }

  boundary (1)
    winapi     raw Win32 FFI — the OS boundary (layer-0 only)
```

```
$ locusc effects pure.locus
pure.locus
  type    : Int
  effects : {}  — pure (touches nothing outside itself)
```

(`hello.locus` is `writeln "review me"`; `pure.locus` is `1 + 2 * 3`.) That
manifest is the foundation. Everything below is what you *do* with it.

Note the roll-up category: `winapi` reports under **`boundary`**, not under the
ordinary IO capabilities (`console`, `fs`, `net`). The raw OS edge is the one reach
a review most wants to *see*, so it gets its own bucket and sits first — a
`boundary` line in layer-2 code is a signal, not noise blended into the rest.

## Auditability: review the row, not every line

You don't have to read every line an intern or an agent wrote to know what it can
do. You read the **row**. A function that claims `Int -> Int ! {}` is pure — provably,
not by promise — and you can skip it in review. A function whose row grew a `{net}`
since last week is the one to look at, and a diff of `locusc effects` output puts
that growth right in front of you. Hand an agent a task, get back a change, and the
first question — *what did it reach for?* — is answered by the type, at a glance.

This is **open-ended** in a way a blocklist of forbidden calls can never be. A
blocklist catches only what you thought to forbid; the row surfaces effects you
never anticipated — next year's network library, the syscall nobody wrote a rule
for, the mistake no one foresaw. You can't forget to audit a footprint that prints
itself. That's the strength: not that bad reaches are *forbidden*, but that **no
reach is invisible**.

## The guardrail a team builds for itself

Now the production benefit. In Locus, **the compiler is the platform.** A team
builds out the layer-0 floor its project needs — the raw OS calls, the collector,
the FFI — and the layer-1 services on top of it (a `console`, a logger, whatever the
project wants), embeds them, and hands *that compiler* to the people doing the
application work. Including interns. Including sub-agents.

The raw FFI — `extern`, the power to name a Win32 entry point directly — lives only
in layer 0. Application code reaches the OS through the team's **capabilities**
(`writeln`, the `console` effect), never the raw boundary. This isn't a cage imposed
from outside; it's the team **writing down its own intent** — *our app code goes
through our front door, not the raw syscall* — and having the project's compiler hold
everyone to it. The lead is one the team clips on itself.

And it has two tensions, because development and shipping want different things. A
**dev** build *warns* and keeps going:

```
$ locusc run scratch.locus        # dev compiler
locusc: warning [dev build]: `extern "GetTickCount64"` names the FFI boundary
  directly — a layer-0 capability that layer-2 code does not hold. Use a stdlib
  capability instead (e.g. `writeln`, the `console` effect); run `locusc republish`
  to see the surface the app may use.
  (a prod compiler — built `--features sealed` — rejects this; `locusc effects`
   shows the effect either way.)
… and the program runs.
```

(`scratch.locus` is `let now = extern "GetTickCount64" : Unit -> Int in now ()`.)
An intern exploring, an agent trying things — they aren't blocked, they're *told*,
and pointed at the right tool. The lead has slack.

A **prod** build of the same compiler — the team built it `--features sealed` —
*blocks*:

```
$ locusc run scratch.locus        # prod compiler (--features sealed)
locusc: `extern "GetTickCount64"` names the FFI boundary directly — a layer-0
  capability that layer-2 code does not hold. …
$ echo $?
2
```

Same code, same detector; the team's two builds of its own compiler differ only in
**severity**. The lead shortens for the street. A stray `extern` that slipped
through a dev session *cannot ship* — that's the production benefit, stated plainly:
a whole class of mistake, *reaching past the team's intended surface*, is caught
before release, by construction.

And the everyday case sails through both builds untouched — `writeln "review me"`
compiles, runs, and prints in dev and prod alike. The guardrail is on the raw reach,
not on getting work done.

## Sealed is not hidden — and you can prove it

A lead doesn't hide what it's attached to. The platform an app is built on isn't a
black box bundled into the compiler; it's readable Locus, and the compiler will
**emit its own authoritative copy on demand**:

```
$ locusc republish ./review
republished ./review/0_winapi.locus
republished ./review/1_console.locus
republished ./review/1_num.locus
republished ./review/effects.catalog
wrote ./review/MANIFEST.txt
```

Every line of the layer-0/1 code beneath an app, written out for reading — *and the
review taxonomy itself* (`effects.catalog`, the data that decides how the manifest
groups and glosses effects, including which labels count as `boundary`). The
manifest carries a content hash for each, so you can confirm the platform you're
reviewing is byte-for-byte the one the compiler runs:

```
# layer  module       bytes    fnv1a-64            file
  0      winapi       779      0xc24c1cd801b000e8  0_winapi.locus
  1      console      1480     0xdefdcb1266d262c4  1_console.locus
  1      num          644      0xd98c08ffa4ecdc62  1_num.locus
  cfg    effects      2217     0x1687c09470baf4e7  effects.catalog
```

It's **write-out only** — the compiler never reads this back, so editing the copy on
disk changes nothing it compiles. The point was never to lock the files; it's that
the truth is *emitted on demand and verifiable*. Transparency goes all the way down:
not just what the app touches, but what the platform under it is made of.

## The lead, not the leash

A leash assumes the worst and restrains by force. It has to win every contest, and
the thing on the end of it resents the pull. A lead assumes a capable companion who
can take a wrong turn, and it works by guidance and daylight: you both want to get
there, and the lead just keeps the walk legible and the wrong turns short.

Locus's effect transparency is a lead. It doesn't assume your interns or your agents
are hostile — it assumes they're **capable and occasionally wrong**, which is the
truth — and it makes their work *legible*: every effect in the type, every change in
the diff, every reach past the team's surface warned in development and held back
from production. The strength is auditability; the benefit is fewer mistakes
shipped.

For genuine adversaries, bring a sandbox — several, at several levels. That's a
different tool for a different job, and Locus is happy to run inside all of them. But
for the everyday reality of building software with fallible collaborators, human and
otherwise, the thing you want most of the time isn't a tighter cage. It's a clearer
view and a gentle pull in the right direction.

A lead, not a leash.

---

*Reproducible against this repo: `locusc effects FILE`, `locusc republish [DIR]`, and
the dev-warn / prod-block split (`--features sealed`) are all in `locusc`. Companions:
[the `mem` effect in action](the-mem-effect-in-action.md) (effects you can read down
to the assembly), [`capabilities.md`](../capabilities.md) (the layered platform),
[`../MANIFESTO.md`](../MANIFESTO.md) (the transparency commitment this rests on).*
