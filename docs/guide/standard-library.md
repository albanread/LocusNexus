# The standard library

The standard library is a set of **services** — modules at the `services` layer
that expose safe, typed APIs and seal away the raw powers underneath. It is
itself written in Locus (`locus/src/stdlib/*.locus`) and linked into every
program, so its names are in scope without any import. Reading it is one of the
best ways to learn the language.

This page is a map. For the exhaustive, always-current signature list of any
service, ask the built-in help:

```sh
$ locusc help services            # list every published service
$ locusc help service String      # full surface of one service
```

## Text and console

| Service | Representative functions | Notes |
|---------|--------------------------|-------|
| `Console` | `console_writeln`, `console_write`, `console_write_char`, `console_read_line`, `console_read_char`, `console_clear_screen`, `console_set_cursor`, `console_write_at`, `console_screen_size`, `console_write_float` | Seals `winapi`, `mem`. Linux exposes `console_writeln` and `console_write_float`. |
| `String` | `string_len`, `string_append`, `string_concat`, `string_slice`, `string_take`, `string_drop`, `string_equals`, `string_compare`, `string_find`, `string_contains`, `string_count`, `string_starts_with`, `string_ends_with` | UTF-16 managed strings; trait instances `StringEq` / `StringOrd` / `StringShow`. |

```locus
if string_equals (string_append "a" "b") "ab" then 1 else 0    -- => 1
```

## Data and combinators

| Service | Representative functions | Notes |
|---------|--------------------------|-------|
| `Option` | `option_is_some`, `option_is_none`, `option_with_default`, `option_map`, `option_bind`, `option_to_result` | combinators over `Option[a]`. |
| `Result` | `result_is_ok`, `result_is_err`, `result_with_default`, `result_map`, `result_map_err`, `result_bind` | combinators over `Result[a, b]`. |
| `List` | `list_len`, `list_append`, `list_reverse`, `list_map`, `list_fold`, `list_filter`, `list_all`, `list_any` | effect-polymorphic: callback rows pass through `{| r}`. |
| `Array` | `array_make_int`, `array_sum_int`, `array_sum_float`, `array_fill_int`, `array_fill_float`, `array_copy_range_int`, `array_dot_float`, `array_scale_float` | dense unboxed scalar arrays; `ArrayMake` trait. |
| `Order` | `min_by`, `max_by` | order a value by a key function. |
| `Fun` | `id`, `compose`, `const`, `flip` | the basic function combinators. |

```locus
let doubled = option_map (Some(21)) (fn x: Int => x + x) in
option_with_default doubled 0                                  -- => 42
```

## Numeric

| Service | Representative functions | Notes |
|---------|--------------------------|-------|
| `Num` | `abs`, `min`, `max`, `clamp`, `compare`, `fmin`, `fmax`, `fclamp` | integer and float min/max/clamp helpers. |
| `Math` | `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `exp`, `ln`, `log10`, `log2`, `ceil`, `fabs`, `pow`, `fmod`, `hypot` | floating-point math over CRT (Windows) / libm (Linux). `sqrt`, `floor`, `round` are language-level forms. |
| `Random` | `random_seed`, `random_next_seed`, `random_next`, `random_between`, `random_bool`, `random_chance` | deterministic, **seed-threaded** PRNG — you pass a seed in and get the next seed back. No ambient entropy. |

```locus
let (roll, seed2) = random_between 1 6 12345 in roll           -- a die roll in 1..6
```

## System and world

| Service | Representative functions | Notes |
|---------|--------------------------|-------|
| `Time` | `clock_ticks`, `clock_frequency`, `clock_millis`, `elapsed_ticks`, `elapsed_millis` | monotonic, high-resolution timing. |
| `DocsFs` | `docs_read_text`, `docs_write_text`, `docs_append_text`, `docs_exists` | filesystem **pinned to the user's Documents folder**; rejects paths with navigation. `docs_read_text` returns `Option[String]`. |
| `LocusEnv` | `locus_env_get` | read-only access to specific `LOCUS_*` variables (`LocusHome`, `LocusCache`, …) — not arbitrary `getenv`. |
| `Db` | `db_mock_connect`, `db_mock_health_check` | mock DB that consumes a *named* Windows credential and returns only connection state — never the secret. Windows only. |
| `Bool` | `bool_not`, `bool_and`, `bool_or`, `bool_xor` | the Boolean connectives as ordinary functions. |

## Agent

| Service | Representative functions | Notes |
|---------|--------------------------|-------|
| `Agent` | `agent_ask_text`, `agent_tell_text` | the constrained MCP/agent text channel — ask the host a question, tell the host something. Carries the `agent` effect. |

The Agent service has its own page, because it is how you write programs an AI
colleague drives turn by turn — see [Programs for agents](agents.md).

## A note on what the services *don't* give you

Every one of these is a deliberately **narrow** surface. `DocsFs` is a
filesystem with no path traversal and one root. `LocusEnv` reads four named
variables, not the environment. `Db` consumes a credential without revealing it.
`Random` is reproducible, not entropic. That narrowness is the design: a service
is a *locus*, a bundle of exactly the verbs a task needs. When you need a new
capability, you add a service that seals its raw power — you don't hand app code
the raw power. See [Modules and capabilities](modules-and-capabilities.md).

— **[Next: Programs for agents →](agents.md)**
