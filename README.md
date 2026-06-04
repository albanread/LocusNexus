# LocusNexus

*A language for people and AI colleagues — a place for every power, and a controlled crossing between them.*

---

## The idea in one paragraph

Software is made of **boundaries**: between the raw machine and the program that
drives it, between what a computation *does* and what it *needs*, between the
foreign world outside the language and the typed, legible code inside it. Most
languages leave those boundaries unmarked — power leaks across them silently,
ambiently, from anywhere. LocusNexus does the opposite. It gives every power a
**locus**: a written position in the type, so you read what code does straight
from its signature, down to whether it allocates. And it routes all authority
through a small number of **nexuses**: named crossing-points where the world binds
to the language, and where forward-effect binds to backward-need. The name is the
thesis — *locus* is the place, *nexus* is the crossing, and the project is the
binding and controlling force between them.

## The two boundaries the name is built on

**The world boundary — where authority enters.** Raw power (calling the OS,
touching memory, driving the collector) comes in through exactly one act —
**minting**, at one named site, the boundary layer. Everything above it trades in
**sealed** abstractions: the dangerous label is unnameable, so app code cannot
utter it, cannot invoke it, cannot build an adapter to it — while every line that
*implements* it stays readable. A seal removes authority, not visibility. This is
the nexus through which all real-world reach must pass, and it is one-directional
and auditable by construction.

**The calculus boundary — where effect meets need.** Effects (what a computation
does as it runs forward) and coeffects (what it demands of its context) are two
graded structures, and a **distributive law** is the crossing where they commute
coherently — the still point where doing and needing pass through each other as a
single fabric, machine-checked for coherence. This is the formal nexus: not a
feature bolted on, but the join that makes the two systems one language.

A locus is *where*; a nexus is *what crosses there*. LocusNexus has a locus for
every power precisely so it can have a small, controlled set of crossings.

## The corner nobody else offers

Today an AI colleague — an agent, an intern — gets one of two deals, as if power
and reach were a single dial. A fixed set of canned commands: safe, but rigid, and
every new move needs a human. Or a full runtime — all of Python, all of Node, the
kitchen sink: expressive, but one import from anything. Those are two axes, not
one. LocusNexus sits in the corner the dial hides:

```
                    world reach
                  narrow        wide
                ┌───────────┬───────────┐
   expressivity │  canned   │  the      │
        high    │  + a real │  kitchen  │  ← LocusNexus is upper-left:
                │  language │  sink     │    a full language over a
                │ (locusc) │           │    bounded, curated world surface
                ├───────────┼───────────┤
        low     │  fixed    │  raw      │
                │  tool set │  shell    │
                └───────────┴───────────┘
```

A team mints and seals a curated set of world-verbs — `readProjectFile`,
`writeLog`, `query` — each scoped to exactly what it should touch. The colleague agent
gets the **whole language** to compose them: loops, abstraction, real computation.
What it can *reach* is exactly that verb set, and everything else is not forbidden
so much as **unsayable**. The mistake "it touched the network" isn't caught after
the fact — there is no name for it to write. Real leverage, without the keys to the
floor.

## What this buys you

- **Transparency** — every effect a computation can have is in its type, including
  `gc`; nothing is hidden, ambient, or implicit. You audit the signature, not the
  source.
- **Confinement without opacity** — sealed powers are un-wieldable above the
  boundary, yet the code that implements them stays fully readable. You get the
  abstraction *and* an auditable, shrinkable floor under it.
- **A legible surface for AI colleagues** — small kernel, errors that are local
  type-checker messages at the call site rather than distant runtime surprises,
  and a curated world surface a team controls.

---

> **Status.** The mint/seal core is implemented and tested — the layer lattice
> (`boundary < services < app`), manifest-gated minting, region and module seals,
> per-provider labels.  The calculus
> drives all of it. The compiler binary is `locusc`.
