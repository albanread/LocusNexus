//! Agent-facing help index.
//!
//! This is deliberately data, not prose scattered through drivers. The CLI and
//! MCP server both read these cards so an agent can discover syntax, operation,
//! services, and reminders through the same compiler contract.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelpCard {
    pub id: &'static str,
    pub kind: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub syntax: &'static str,
    pub example: &'static str,
    pub details: &'static [&'static str],
    pub related: &'static [&'static str],
    pub keywords: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelpHit {
    pub card: &'static HelpCard,
    pub score: i32,
}

pub const TOPICS: &[HelpCard] = &[
    HelpCard {
        id: "agent.start",
        kind: "topic",
        title: "Agent start",
        summary: "Minimal loop for an agent writing Locus through CLI or MCP.",
        syntax: "help search QUERY -> check -> run/run_agent_text -> inspect effects -> repeat",
        example: "locusc help search \"agent ask text\"",
        details: &[
            "Use help search when you do not remember a construct or service.",
            "Use check before run; the checker reports the type and effect row.",
            "Use effects --json to review capabilities before execution.",
            "MCP agents should call help_overview first, then help_search as needed.",
            "Agent-channel programs use queued replay: run, inspect asks/tells, rerun with a longer responses array.",
        ],
        related: &[
            "operation.cli",
            "operation.mcp",
            "syntax.effects",
            "service.Agent.agent_ask_text",
        ],
        keywords: &["agent", "start", "workflow", "discover", "mcp", "reminder"],
    },
    HelpCard {
        id: "operation.cli",
        kind: "operation",
        title: "CLI operation",
        summary: "The command-line workflow for checking, inspecting, running, and building.",
        syntax: "locusc check/run/build/asm/effects/help ...",
        example: "locusc effects examples/othello.locus --json",
        details: &[
            "locus check FILE type-checks with the core checker.",
            "locus ir FILE prints ANF IR.",
            "locusc run FILE JIT-runs a program and returns its i64 result as the exit code.",
            "locusc effects FILE --json prints the effect manifest for review.",
            "locusc mcp serves the MCP protocol over stdio when built with the mcp feature.",
        ],
        related: &["agent.start", "operation.mcp", "syntax.modules", "effects.rows"],
        keywords: &["cli", "run", "build", "asm", "effects", "check", "ir", "help"],
    },
    HelpCard {
        id: "operation.mcp",
        kind: "operation",
        title: "MCP operation",
        summary: "Structured compiler tools for agents without parsing terminal prose.",
        syntax: "check, emit_ir, emit_asm, build, run, run_agent_text, agent_session_*, effects, help_*",
        example: "run_agent_text({file: \"examples/othello_for_agents.locus\", responses: [\"2,3\"], default_response: \"pass\"})",
        details: &[
            "MCP tools accept either file or source.",
            "run_agent_text provides a scoped queued Agent ask/tell text channel.",
            "It is not live mid-call interaction: inspect agent_transcript.asks/tells and rerun with a longer responses array.",
            "Ask entries include used_default=true when the queued responses were exhausted.",
            "For live turn-by-turn Agent I/O, use agent_session_start, agent_session_reply, agent_session_status, and agent_session_close.",
            "Live session replies/status accept since_event_index to return only new transcript events; latest_score/latest_ask stay available as compact fields.",
            "help_overview reports workspace_cwd; relative file paths resolve from that server directory.",
            "The MCP server supervises per-request workers; compiler/help/service facts come from the worker binary that handles the request.",
            "help_overview reports worker path and stdlib hashes; set LOCUS_WORKER_EXE to point the supervisor at a fresh compatible worker.",
            "Tool results are structured JSON with type, effects, demanded APIs, and run result.",
            "Use help_search over MCP for iterative recall.",
        ],
        related: &["agent.start", "service.Agent", "syntax.effects"],
        keywords: &[
            "mcp",
            "tool",
            "json",
            "agent",
            "run_agent_text",
            "agent_session",
            "live",
            "structured",
            "workspace_cwd",
            "cwd",
            "relative",
            "path",
            "paths",
            "file",
            "directory",
        ],
    },
    HelpCard {
        id: "syntax.let",
        kind: "syntax",
        title: "let bindings",
        summary: "Name a value, then use it in the body.",
        syntax: "let name = expr in body",
        example: "let x = 40 in x + 2",
        details: &[
            "Use let rec with an explicit function type for recursion.",
            "Use do blocks when several lets read more naturally as statements.",
        ],
        related: &["syntax.do", "syntax.functions", "syntax.loops"],
        keywords: &["let", "binding", "variable", "name", "rec"],
    },
    HelpCard {
        id: "syntax.functions",
        kind: "syntax",
        title: "functions",
        summary: "Functions are explicit lambdas; multiple arguments are curried.",
        syntax: "fn x: T => expr",
        example: "let add = fn a: Int => fn b: Int => a + b in add 20 22",
        details: &[
            "Recursive functions use let rec and a type annotation.",
            "A function type may include a latent row: Int -> Unit ! {agent}.",
        ],
        related: &["syntax.let", "syntax.effects", "syntax.traits"],
        keywords: &["fn", "function", "lambda", "arrow", "recursive", "let rec"],
    },
    HelpCard {
        id: "syntax.do",
        kind: "syntax",
        title: "do blocks",
        summary: "Statement-like sequencing sugar that still desugars to expressions.",
        syntax: "do { let x = expr; expr; final }",
        example: "do { let x = 20; let y = x + 22; y }",
        details: &[
            "Every item still contributes its real effects to the row.",
            "Expression statements are sequenced for effects; the last expression is the result.",
        ],
        related: &["syntax.let", "syntax.effects", "syntax.handlers"],
        keywords: &["do", "block", "sequence", "statements", "semicolon"],
    },
    HelpCard {
        id: "syntax.conditionals",
        kind: "syntax",
        title: "conditionals",
        summary: "Branch with if/then/else, cond, or case sugar.",
        syntax: "if cond then a else b",
        example: "cond | x < 0 => 0 | x < 10 => x | _ => 10",
        details: &[
            "Both branches must have the same type.",
            "case is expression matching over literal-like alternatives.",
            "cond is ordered guard sugar.",
        ],
        related: &["syntax.match", "syntax.loops", "syntax.operators"],
        keywords: &["if", "then", "else", "cond", "case", "branch"],
    },
    HelpCard {
        id: "syntax.operators",
        kind: "syntax",
        title: "operators and comparisons",
        summary: "Arithmetic, equality, and ordering operators are ordinary expression syntax.",
        syntax: "+ - * / %, == != < <= > >=, && ||, ~",
        example: "if amount <= target then ways[amount] else 0",
        details: &[
            "Use == and != for equality checks.",
            "Use <, <=, >, and >= for signed Int ordering and ordered Float comparisons.",
            "Use % for signed Int remainder; floating-point remainder is the Math service's fmod.",
            "Use && and || for short-circuit Bool connectives.",
            "Use ~ for unary Bool negation; Locus avoids ! here because ! belongs to effect rows and Ref dereference.",
            "The Bool service still provides bool_and, bool_or, bool_xor, and bool_not as ordinary functions.",
        ],
        related: &["syntax.conditionals", "syntax.loops", "service.Bool", "service.Num"],
        keywords: &[
            "operator",
            "operators",
            "comparison",
            "compare",
            "<=",
            ">=",
            "!=",
            "<",
            "==",
            ">",
            "&&",
            "||",
            "~",
            "boolean",
            "bool",
            "not",
            "negation",
            "arithmetic",
            "%",
            "mod",
            "modulo",
            "remainder",
            "plus",
            "minus",
        ],
    },
    HelpCard {
        id: "syntax.match",
        kind: "syntax",
        title: "match",
        summary: "Pattern match sum constructors and bind constructor fields.",
        syntax: "match expr with | C(x) => body | _ => fallback",
        example: "match Some(7) with | None => 0 | Some(x) => x",
        details: &[
            "Matches must be exhaustive unless a wildcard arm is present.",
            "Constructors come from type declarations or the stdlib.",
        ],
        related: &["syntax.types", "service.Option", "service.Result", "service.List"],
        keywords: &["match", "pattern", "constructor", "Some", "None", "Ok", "Err"],
    },
    HelpCard {
        id: "syntax.loops",
        kind: "syntax",
        title: "loops",
        summary: "Loop accumulators update in lock-step until the condition fails.",
        syntax: "loop i = init, acc = init while cond do next_i, next_acc return result; loop i = init while cond do next_i endloop",
        example: "loop i = 0, acc = 0 while i < 10 do i + 1, acc + i return acc",
        details: &[
            "Each loop variable has an initializer and one step expression.",
            "The return expression produces the loop result; older else result spelling is still accepted.",
            "Use endloop for statement-style loops whose result is intentionally Unit.",
            "Managed values in loop accumulators are rooted by the runtime when necessary.",
        ],
        related: &["syntax.arrays", "syntax.do", "syntax.operators", "service.Array"],
        keywords: &[
            "loop",
            "while",
            "do",
            "return",
            "else",
            "endloop",
            "accumulator",
            "iteration",
            "statement",
        ],
    },
    HelpCard {
        id: "syntax.arrays",
        kind: "syntax",
        title: "arrays",
        summary: "Arrays are managed values with bounds-checked indexing and update.",
        syntax: "[a, b, c], len arr, arr[i], arr[i] <- value, array_make len value",
        example: "let a = array_make 3 0 in let _ = a[1] <- 99 in a[1]",
        details: &[
            "String is represented as a managed array of UTF-16 code units.",
            "Indexing and update carry memory/GC effects as inferred by the compiler.",
            "Use array_make for mutable tables whose length is only known at runtime; Int is supported today.",
        ],
        related: &["service.Array", "service.String", "syntax.loops"],
        keywords: &["array", "index", "len", "set", "update", "literal", "allocate", "fill", "make"],
    },
    HelpCard {
        id: "syntax.types",
        kind: "syntax",
        title: "types",
        summary: "Declare sum types with constructors and use match to consume them.",
        syntax: "type Name = A | B(Int, String) in body",
        example: "type Cell = Empty | Black | White in match Black with | Black => 1 | _ => 0",
        details: &[
            "Constructors allocate managed values when they carry payloads.",
            "The stdlib defines Option, Result, List, and Ordering.",
        ],
        related: &["syntax.match", "service.Option", "service.Result", "service.List"],
        keywords: &["type", "sum", "constructor", "Option", "Result", "List"],
    },
    HelpCard {
        id: "syntax.effects",
        kind: "syntax",
        title: "effects",
        summary: "Every capability a computation may use appears in its effect row.",
        syntax: "effect op : A -> B in perform op value",
        example: "effect ask : String -> String in perform ask \"move?\"",
        details: &[
            "Rows print like Int ! {gc, agent}.",
            "Service functions expose constrained effects instead of hidden ambient power.",
            "Native boundary labels include gc, mem, winapi, libc, libm, asm, and agent.",
            "Use locusc effects FILE --json before running generated code.",
        ],
        related: &["effects.rows", "syntax.handlers", "syntax.modules", "service.Agent"],
        keywords: &["effect", "perform", "row", "capability", "agent", "gc", "mem"],
    },
    HelpCard {
        id: "syntax.handlers",
        kind: "syntax",
        title: "handlers",
        summary: "Handle an effect by providing operation clauses and an optional return clause.",
        syntax: "handle expr with { op(x) -> body ; return(y) -> body }",
        example: "handle perform ask 1 with { ask(x) -> x + 1 }",
        details: &[
            "A handled effect disappears from the surrounding row.",
            "Use resume in resumptive handlers when the operation should continue.",
            "Services often use handlers to discharge abstract operations into boundary calls.",
        ],
        related: &["syntax.effects", "syntax.modules"],
        keywords: &["handle", "with", "return", "resume", "operation", "discharge"],
    },
    HelpCard {
        id: "syntax.modules",
        kind: "syntax",
        title: "modules and capabilities",
        summary: "Modules declare their layer and export boundary-safe services.",
        syntax: "module Name at app|services|boundary [mints (...)] [seals (...)] [exposing (...)] = body",
        example: "module Util at app exposing (double) = let double = fn x: Int => x + x in ()",
        details: &[
            "Only boundary modules may mint extern/raw powers.",
            "Services build controlled APIs over boundary modules and can seal raw labels.",
            "App modules cannot use extern directly.",
        ],
        related: &["syntax.effects", "operation.cli", "service.Console"],
        keywords: &["module", "boundary", "services", "app", "mints", "seals", "exposing"],
    },
    HelpCard {
        id: "syntax.traits",
        kind: "syntax",
        title: "traits",
        summary: "Declare one-parameter traits and instances for typed generic operations.",
        syntax: "trait Name a { method : a -> Int } in instance Name Int { method = fn x: Int => x } in body",
        example: "string_eq \"a\" \"a\"",
        details: &[
            "Trait method signatures include effect rows like any function.",
            "Instances must provide exactly the declared methods.",
            "The stdlib has string traits for equality, ordering, and show.",
        ],
        related: &["service.String", "syntax.functions", "syntax.effects"],
        keywords: &["trait", "instance", "method", "StringEq", "StringOrd", "generic"],
    },
    HelpCard {
        id: "effects.rows",
        kind: "topic",
        title: "effect rows",
        summary: "Rows are the readable capability manifest attached to a type.",
        syntax: "Type ! {effect1, effect2}",
        example: "String -> String ! {agent, gc}",
        details: &[
            "Empty row means pure.",
            "gc means managed heap allocation.",
            "agent means the program can ask/tell its MCP host.",
            "winapi/libc/libm/crt/asm are raw boundary labels and should usually appear only below services.",
        ],
        related: &["syntax.effects", "operation.cli", "service.Agent"],
        keywords: &["row", "effects", "pure", "gc", "agent", "winapi", "libc"],
    },
];

pub const SERVICES: &[HelpCard] = &[
    HelpCard {
        id: "service.Agent",
        kind: "service",
        title: "Agent service",
        summary: "Constrained MCP/agent text channel for generated programs.",
        syntax: "agent_ask_text : String -> String ! {agent, gc}; agent_tell_text : String -> Unit ! {agent}",
        example: "let move = agent_ask_text \"black move?\" in agent_tell_text move",
        details: &[
            "Shared on Windows and Linux.",
            "Use run_agent_text over MCP to provide queued responses and receive a transcript.",
            "For interactive games/tools, include the valid choices in each prompt and expect the host to rerun after inspecting asks.",
            "For live interaction, use the MCP agent_session_* tools so each agent_ask_text suspends until agent_session_reply provides one answer.",
            "For long live sessions, use since_event_index and latest_score/latest_move_result instead of rereading the full transcript every turn.",
        ],
        related: &[
            "service.Agent.agent_ask_text",
            "service.Agent.agent_tell_text",
            "operation.mcp",
        ],
        keywords: &["Agent", "agent_ask_text", "agent_tell_text", "mcp"],
    },
    HelpCard {
        id: "service.Agent.agent_ask_text",
        kind: "service",
        title: "agent_ask_text",
        summary: "Ask the MCP/agent host for a text response.",
        syntax: "agent_ask_text : String -> String ! {agent, gc}",
        example: "let answer = agent_ask_text \"move?\" in string_len answer",
        details: &[
            "The prompt and response are Locus String values.",
            "The answer materializes as a managed String, so gc appears in the row.",
            "Under MCP, responses are consumed from the run_agent_text responses array; exhausted queues use default_response.",
        ],
        related: &["service.Agent", "service.String", "operation.mcp"],
        keywords: &["ask", "agent", "prompt", "response", "text", "mcp"],
    },
    HelpCard {
        id: "service.Agent.agent_tell_text",
        kind: "service",
        title: "agent_tell_text",
        summary: "Send text to the MCP/agent host transcript.",
        syntax: "agent_tell_text : String -> Unit ! {agent}",
        example: "agent_tell_text \"board updated\"",
        details: &["Use this for traces, board states, and constrained program output."],
        related: &["service.Agent", "operation.mcp"],
        keywords: &["tell", "agent", "transcript", "trace", "text"],
    },
    HelpCard {
        id: "service.Console",
        kind: "service",
        title: "Console service",
        summary: "Terminal output and, on Windows, character/line/screen helpers.",
        syntax: "console_writeln : String -> Unit; console_write_float : Float -> Unit",
        example: "console_writeln \"hello\"",
        details: &[
            "Windows also exposes console_write, console_write_char, console_read_char, console_read_line, console_clear_screen, console_set_cursor, console_write_at, and console_screen_size.",
            "Linux currently exposes console_writeln and console_write_float.",
        ],
        related: &["service.String", "syntax.effects"],
        keywords: &["console", "console_writeln", "write", "console_read_line", "screen", "terminal"],
    },
    HelpCard {
        id: "service.DocsFs",
        kind: "service",
        title: "DocsFs service",
        summary: "Documents-folder-only filesystem service.",
        syntax: "docs_read_text, docs_write_text, docs_append_text, docs_exists",
        example: "docs_write_text \"note.txt\" \"hello\"",
        details: &[
            "All paths are pinned to the user's Documents folder.",
            "Names with navigation are rejected by the boundary layer.",
            "docs_read_text returns Option[String].",
        ],
        related: &["service.Option", "service.String", "syntax.effects"],
        keywords: &["fs", "file", "documents", "read", "write", "append", "exists"],
    },
    HelpCard {
        id: "service.LocusEnv",
        kind: "service",
        title: "LocusEnv service",
        summary: "Read-only access to specific LOCUS_* environment variables.",
        syntax: "locus_env_get : LocusEnvKey -> Option[String]",
        example: "match locus_env_get LocusHome with | None => \"\" | Some(s) => s",
        details: &[
            "Known keys include LocusHome, LocusCache, LocusConfig, and LocusTrace.",
            "The service does not expose arbitrary environment lookup.",
        ],
        related: &["service.Option", "service.String"],
        keywords: &["env", "environment", "LOCUS", "LocusHome", "read only"],
    },
    HelpCard {
        id: "service.Db",
        kind: "service",
        title: "Db service",
        summary: "Mock database service that consumes named credentials without returning secrets.",
        syntax: "db_mock_connect : String -> Bool; db_mock_health_check : String -> Int",
        example: "if db_mock_connect \"test.api.key\" then 1 else 0",
        details: &[
            "Windows only today.",
            "The app supplies a credential profile name such as test.api.key.",
            "The service consumes the matching Windows Generic Credential internally and returns only mock connection state.",
            "Credential material is not exposed as a Locus String to app code.",
            "The underlying Credential Manager lookup is pinned to secure/credentials and is not list/write/delete capable.",
            "This is the placeholder for a future MySQL-backed service; the public shape should stay named-operation, not raw secret read.",
        ],
        related: &["service.String", "syntax.effects", "effects.rows"],
        keywords: &[
            "db",
            "database",
            "mysql",
            "mock",
            "connect",
            "health",
            "wincred",
            "credential",
            "credentials",
            "Credential Manager",
            "Generic Credential",
            "secure/credentials",
            "secret",
            "api key",
            "test.api.key",
            "windows",
        ],
    },
    HelpCard {
        id: "service.Time",
        kind: "service",
        title: "Time service",
        summary: "Monotonic timing helpers for measurement.",
        syntax: "clock_ticks, clock_frequency, clock_millis, elapsed_ticks, elapsed_millis",
        example: "let start = clock_ticks () in elapsed_millis start",
        details: &[
            "clock_ticks is high-resolution and monotonic.",
            "clock_frequency gives ticks per second.",
            "ticks_to_millis and ticks_to_micros convert raw ticks.",
        ],
        related: &["operation.cli", "syntax.effects"],
        keywords: &["time", "clock", "ticks", "elapsed", "performance", "duration"],
    },
    HelpCard {
        id: "service.String",
        kind: "service",
        title: "String service",
        summary: "UTF-16 string helpers over managed String values.",
        syntax: "string_len, string_slice, string_append, string_equals, string_find, string_count",
        example: "if string_equals (string_append \"a\" \"b\") \"ab\" then 1 else 0",
        details: &[
            "String is a variable-length managed array of 16-bit code units.",
            "Important helpers: string_len, string_is_empty, string_unit_at, string_empty, string_singleton, string_slice, string_take, string_drop, string_concat, string_append, string_repeat, string_equals, string_compare, string_starts_with, string_ends_with, string_contains_at, string_find_from, string_find, string_last_find, string_contains, string_count.",
            "String traits include StringEq, StringOrd, and StringShow.",
        ],
        related: &["syntax.arrays", "syntax.traits", "service.Agent"],
        keywords: &["string", "UTF-16", "text", "append", "find", "slice", "equals"],
    },
    HelpCard {
        id: "service.Math",
        kind: "service",
        title: "Math service",
        summary: "Floating-point math over CRT/libm.",
        syntax: "sin, cos, tan, asin, acos, atan, atan2, exp, ln, log10, log2, ceil, fabs, pow, fmod, hypot",
        example: "pow 2.0 3.0",
        details: &[
            "Windows uses CRT; Linux uses libm.",
            "sqrt, floor, and round are language-level numeric forms.",
        ],
        related: &["service.Num", "syntax.effects"],
        keywords: &["math", "sin", "cos", "pow", "log", "float", "libm", "crt"],
    },
    HelpCard {
        id: "service.Random",
        kind: "service",
        title: "Random service",
        summary: "Deterministic seed-threaded pseudo-random helpers for games, examples, and tests.",
        syntax: "random_seed, random_next_seed, random_next, random_between, random_bool, random_chance",
        example: "let (roll, seed2) = random_between 1 6 12345 in roll",
        details: &[
            "Shared on Windows and Linux.",
            "Pass an Int seed in and thread the returned next seed through later draws.",
            "random_next_seed advances a seed without allocation.",
            "Pair-returning helpers allocate tuples, so gc appears in their effect rows.",
            "random_between lo hi seed is inclusive and accepts the bounds in either order.",
            "This service is deterministic; it does not read ambient operating-system entropy.",
        ],
        related: &["service.Num", "syntax.operators", "syntax.effects"],
        keywords: &[
            "random",
            "rng",
            "prng",
            "seed",
            "dice",
            "roll",
            "between",
            "bool",
            "chance",
            "stochastic",
            "game",
            "games",
            "deterministic",
        ],
    },
    HelpCard {
        id: "service.Array",
        kind: "service",
        title: "Array service",
        summary: "Loop-backed helpers for dense numeric arrays.",
        syntax: "array_make, array_make_int, array_sum_int, array_sum_float, array_fill_int, array_copy_range_int, array_dot_float, array_scale_float",
        example: "let ways = array_make 201 0 in let _ = ways[0] <- 1 in ways[0]",
        details: &[
            "Helpers are intentionally monomorphic today so layout remains explicit.",
            "array_make is the generic constructor surface; ArrayMake Int is supported today.",
            "array_make_int len value allocates an Array[Int] and initializes every slot.",
            "array_fill_int mutates an existing Array[Int]; use it when you already have the array.",
            "Use loops directly for custom array traversals.",
        ],
        related: &["syntax.arrays", "syntax.loops", "syntax.traits", "service.Array.array_make"],
        keywords: &["array", "make", "new", "allocate", "sum", "fill", "copy", "dot", "scale", "loop", "dynamic programming"],
    },
    HelpCard {
        id: "service.Array.array_make",
        kind: "service",
        title: "array_make",
        summary: "Generic-facing array constructor: allocate an Array of a supported element type and initialize every slot.",
        syntax: "array_make len initial_value",
        example: "let table = array_make 201 0 in let _ = table[0] <- 1 in table[200]",
        details: &[
            "This is a trait method from ArrayMake a; the initial value chooses the element type.",
            "ArrayMake Int is supported today and performs gc because it materializes a managed array.",
            "Use array_make_int when you want the concrete Int constructor explicitly.",
        ],
        related: &["service.Array", "syntax.arrays", "syntax.traits"],
        keywords: &["array_make", "ArrayMake", "array", "make", "generic", "allocate", "fill", "initialize", "dp", "table", "mutable"],
    },
    HelpCard {
        id: "service.Array.array_make_int",
        kind: "service",
        title: "array_make_int",
        summary: "Concrete Int array constructor: allocate an Array[Int] and initialize every element.",
        syntax: "array_make_int len initial_value",
        example: "let table = array_make_int 201 0 in let _ = table[0] <- 1 in table[200]",
        details: &[
            "Prefer array_make when the generic trait-facing constructor reads better.",
            "The result is mutable through normal arr[i] <- value updates.",
            "This helper performs gc because it materializes a managed array.",
            "Useful for dynamic programming tables and generated numeric workloads.",
        ],
        related: &["service.Array", "service.Array.array_make", "syntax.arrays", "syntax.loops"],
        keywords: &["array_make_int", "array", "make", "allocate", "fill", "initialize", "dp", "table", "mutable"],
    },
    HelpCard {
        id: "service.List",
        kind: "service",
        title: "List service",
        summary: "Generic List plus common functional combinators.",
        syntax: "Nil, Cons, list_len, list_map, list_fold, list_filter, list_reverse",
        example: "list_len (Cons(1, Cons(2, Nil)))",
        details: &[
            "List constructors allocate managed values.",
            "Callback effects thread through map, fold, filter, and for_each.",
            "Also includes list_take, list_drop, list_append, list_find, list_all, list_any, list_index, and list_for_each.",
        ],
        related: &["syntax.types", "syntax.match", "service.Option"],
        keywords: &["list", "Nil", "Cons", "map", "fold", "filter", "reverse"],
    },
    HelpCard {
        id: "service.Option",
        kind: "service",
        title: "Option service",
        summary: "Optional values and combinators.",
        syntax: "None, Some, option_map, option_bind, option_with_default",
        example: "option_with_default (Some(7)) 0",
        details: &[
            "Use Option for expected absence.",
            "Also includes option_is_some, option_is_none, and option_to_result.",
        ],
        related: &["syntax.match", "service.Result", "service.List"],
        keywords: &["Option", "Some", "None", "maybe", "optional", "bind", "map"],
    },
    HelpCard {
        id: "service.Result",
        kind: "service",
        title: "Result service",
        summary: "Expected success/error values and combinators.",
        syntax: "Ok, Err, result_map, result_bind, result_with_default",
        example: "result_with_default (Ok(5)) 0",
        details: &[
            "Use Result for expected errors that are ordinary data.",
            "Also includes result_is_ok, result_is_err, and result_map_err.",
        ],
        related: &["syntax.match", "service.Option"],
        keywords: &["Result", "Ok", "Err", "error", "success", "bind", "map"],
    },
    HelpCard {
        id: "service.Num",
        kind: "service",
        title: "Num service",
        summary: "Small integer and ordering helpers.",
        syntax: "abs, min, max, clamp, compare",
        example: "clamp 15 0 10",
        details: &["compare returns Ordering: Lt, Eq, or Gt."],
        related: &["service.Order", "service.Math"],
        keywords: &["num", "abs", "min", "max", "clamp", "compare", "Ordering"],
    },
    HelpCard {
        id: "service.Bool",
        kind: "service",
        title: "Bool service",
        summary: "Boolean combinators.",
        syntax: "bool_not, bool_and, bool_or, bool_xor",
        example: "bool_xor true false",
        details: &["These are pure helpers over Bool."],
        related: &["syntax.conditionals"],
        keywords: &["bool", "true", "false", "and", "or", "xor", "not"],
    },
    HelpCard {
        id: "service.Order",
        kind: "service",
        title: "Order service",
        summary: "Choose values with a comparator.",
        syntax: "min_by, max_by",
        example: "min_by 3 5 compare",
        details: &["Comparators return Ordering values from Num.compare or custom functions."],
        related: &["service.Num", "syntax.functions"],
        keywords: &["order", "min_by", "max_by", "compare", "Ordering"],
    },
    HelpCard {
        id: "service.Fun",
        kind: "service",
        title: "Fun service",
        summary: "Small higher-order pure helpers.",
        syntax: "id, compose, const, flip",
        example: "compose (fn x: Int => x + 1) (fn y: Int => y * 2) 10",
        details: &["These helpers are parametric; callback effects flow through composition."],
        related: &["syntax.functions", "syntax.effects"],
        keywords: &["function", "id", "compose", "const", "flip", "higher order"],
    },
];

pub fn all_cards() -> impl Iterator<Item = &'static HelpCard> {
    TOPICS.iter().chain(SERVICES.iter())
}

pub fn find(id_or_title: &str) -> Option<&'static HelpCard> {
    let q = id_or_title.trim();
    all_cards().find(|card| {
        card.id.eq_ignore_ascii_case(q)
            || card.title.eq_ignore_ascii_case(q)
            || card
                .id
                .rsplit('.')
                .next()
                .is_some_and(|last| last.eq_ignore_ascii_case(q))
    })
}

pub fn service(name: &str) -> Option<&'static HelpCard> {
    let q = name.trim();
    SERVICES.iter().find(|card| {
        card.id.eq_ignore_ascii_case(q)
            || card.title.eq_ignore_ascii_case(q)
            || card
                .id
                .rsplit('.')
                .next()
                .is_some_and(|last| last.eq_ignore_ascii_case(q))
    })
}

pub fn search(query: &str, limit: usize) -> Vec<HelpHit> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<HelpHit> = all_cards()
        .filter_map(|card| {
            let score = score_card(card, &words);
            (score > 0).then_some(HelpHit { card, score })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.card.id.cmp(b.card.id)));
    hits.truncate(limit.max(1));
    hits
}

fn score_card(card: &HelpCard, words: &[String]) -> i32 {
    let id = card.id.to_ascii_lowercase();
    let title = card.title.to_ascii_lowercase();
    let summary = card.summary.to_ascii_lowercase();
    let syntax = card.syntax.to_ascii_lowercase();
    let example = card.example.to_ascii_lowercase();
    let details = card.details.join(" ").to_ascii_lowercase();
    let keywords = card.keywords.join(" ").to_ascii_lowercase();
    let mut score = 0;
    for word in words {
        if id == *word || title == *word {
            score += 100;
        }
        if id.contains(word) {
            score += 40;
        }
        if title.contains(word) {
            score += 35;
        }
        if keywords.contains(word) {
            score += 30;
        }
        if summary.contains(word) {
            score += 18;
        }
        if syntax.contains(word) {
            score += 12;
        }
        if example.contains(word) {
            score += 10;
        }
        if details.contains(word) {
            score += 6;
        }
    }
    score
}

pub fn overview_text() -> String {
    let mut out = String::new();
    out.push_str("Locus help\n\n");
    out.push_str("Start here if you are an agent:\n");
    out.push_str("  locusc help agent\n");
    out.push_str("  locusc help search \"what you need\"\n");
    out.push_str("  locusc help topic syntax.loops\n");
    out.push_str("  locusc help service Agent\n");
    out.push_str("  locusc help remind effects\n");
    out.push_str("JSON is the default; add --human when a human-in-the-loop wants prose.\n\n");
    out.push_str("Useful topic ids:\n");
    for card in TOPICS {
        out.push_str(&format!("  {:<24} {}\n", card.id, card.summary));
    }
    out.push_str("\nPublished service modules:\n");
    for card in SERVICES
        .iter()
        .filter(|c| !c.id.matches('.').nth(1).is_some())
    {
        out.push_str(&format!("  {:<24} {}\n", card.id, card.summary));
    }
    out
}

pub fn card_text(card: &HelpCard) -> String {
    let mut out = String::new();
    out.push_str(&format!("{} ({})\n", card.title, card.id));
    out.push_str(&format!("kind: {}\n\n", card.kind));
    out.push_str(card.summary);
    out.push_str("\n\nsyntax/signature:\n  ");
    out.push_str(card.syntax);
    if !card.example.is_empty() {
        out.push_str("\n\nexample:\n  ");
        out.push_str(card.example);
    }
    if !card.details.is_empty() {
        out.push_str("\n\nnotes:\n");
        for detail in card.details {
            out.push_str("  - ");
            out.push_str(detail);
            out.push('\n');
        }
    }
    if !card.related.is_empty() {
        out.push_str("\nrelated:\n");
        for id in card.related {
            out.push_str("  ");
            out.push_str(id);
            out.push('\n');
        }
    }
    out
}

pub fn remind_text(topic: &str) -> String {
    if let Some(card) = find(topic).or_else(|| search(topic, 1).first().map(|h| h.card)) {
        let mut out = String::new();
        out.push_str(&format!("Reminder: {} ({})\n", card.title, card.id));
        out.push_str(card.summary);
        out.push_str("\n");
        if !card.syntax.is_empty() {
            out.push_str("syntax: ");
            out.push_str(card.syntax);
            out.push('\n');
        }
        if !card.example.is_empty() {
            out.push_str("example: ");
            out.push_str(card.example);
            out.push('\n');
        }
        if !card.related.is_empty() {
            out.push_str("next: ");
            out.push_str(&card.related.join(", "));
            out.push('\n');
        }
        out
    } else {
        format!("no reminder found for `{topic}`; try `help search {topic}`")
    }
}

pub fn search_text(query: &str, hits: &[HelpHit]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Help search: {query}\n"));
    if hits.is_empty() {
        out.push_str(
            "No matches. Try broader words like `loop`, `agent`, `string`, `effect`, or `file`.\n",
        );
        return out;
    }
    for hit in hits {
        out.push_str(&format!(
            "  {:<32} {:<9} score={:<3} {}\n",
            hit.card.id, hit.card.kind, hit.score, hit.card.summary
        ));
    }
    out
}

pub fn services_text() -> String {
    let mut out = String::new();
    out.push_str("Published Locus services\n\n");
    for card in SERVICES {
        out.push_str(&format!(
            "{:<32} {:<9} {}\n",
            card.id, card.kind, card.summary
        ));
    }
    out
}

pub fn overview_json() -> String {
    let topics: Vec<&HelpCard> = TOPICS.iter().collect();
    let services: Vec<&HelpCard> = SERVICES
        .iter()
        .filter(|c| !c.id.matches('.').nth(1).is_some())
        .collect();
    format!(
        "{{\"schema\":\"locus-help/1\",\"kind\":\"overview\",\"default_format\":\"json\",\"human_flag\":\"--human\",\"agent_start\":{},\"topics\":{},\"services\":{}}}",
        json_string("locusc help agent"),
        cards_json_array(&topics),
        cards_json_array(&services)
    )
}

pub fn card_json(card: &HelpCard) -> String {
    format!(
        "{{\"id\":{},\"kind\":{},\"title\":{},\"summary\":{},\"syntax\":{},\"example\":{},\"details\":{},\"related\":{},\"keywords\":{}}}",
        json_string(card.id),
        json_string(card.kind),
        json_string(card.title),
        json_string(card.summary),
        json_string(card.syntax),
        json_string(card.example),
        string_array_json(card.details),
        string_array_json(card.related),
        string_array_json(card.keywords)
    )
}

pub fn search_json(query: &str, hits: &[HelpHit]) -> String {
    let items: Vec<String> = hits
        .iter()
        .map(|hit| {
            format!(
                "{{\"score\":{},\"card\":{}}}",
                hit.score,
                card_json(hit.card)
            )
        })
        .collect();
    format!(
        "{{\"schema\":\"locus-help/1\",\"kind\":\"search\",\"query\":{},\"results\":[{}]}}",
        json_string(query),
        items.join(",")
    )
}

pub fn services_json() -> String {
    let services: Vec<&HelpCard> = SERVICES.iter().collect();
    format!(
        "{{\"schema\":\"locus-help/1\",\"kind\":\"services\",\"services\":{}}}",
        cards_json_array(&services)
    )
}

pub fn remind_json(topic: &str) -> String {
    if let Some(card) = find(topic).or_else(|| search(topic, 1).first().map(|h| h.card)) {
        format!(
            "{{\"schema\":\"locus-help/1\",\"kind\":\"reminder\",\"query\":{},\"card\":{}}}",
            json_string(topic),
            card_json(card)
        )
    } else {
        format!(
            "{{\"schema\":\"locus-help/1\",\"kind\":\"reminder\",\"query\":{},\"error\":{}}}",
            json_string(topic),
            json_string("no matching help card")
        )
    }
}

fn cards_json_array(cards: &[&HelpCard]) -> String {
    let items: Vec<String> = cards.iter().map(|card| card_json(card)).collect();
    format!("[{}]", items.join(","))
}

fn string_array_json(items: &[&str]) -> String {
    let items: Vec<String> = items.iter().map(|s| json_string(s)).collect();
    format!("[{}]", items.join(","))
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn search_finds_agent_ask_text() {
        let hits = search("ask agent for move text", 3);
        assert!(
            hits.iter()
                .any(|hit| hit.card.id == "service.Agent.agent_ask_text"),
            "expected agent_ask_text in hits: {hits:?}"
        );
    }

    #[test]
    fn search_finds_array_make() {
        let hits = search("mutable dynamic programming int table", 5);
        assert!(
            hits.iter()
                .any(|hit| hit.card.id == "service.Array.array_make"),
            "expected array_make in hits: {hits:?}"
        );
    }

    #[test]
    fn search_finds_db_for_credentials() {
        let hits = search("windows credential manager api key secret", 5);
        assert!(
            hits.iter().any(|hit| hit.card.id == "service.Db"),
            "expected Db in hits: {hits:?}"
        );
    }

    #[test]
    fn search_finds_operator_comparison_help() {
        let hits = search("<= >= != comparison operators", 5);
        assert!(
            hits.iter().any(|hit| hit.card.id == "syntax.operators"),
            "expected operators card in hits: {hits:?}"
        );
    }

    #[test]
    fn search_finds_random_helpers() {
        let hits = search("random dice seed game", 5);
        assert!(
            hits.iter().any(|hit| hit.card.id == "service.Random"),
            "expected Random service in hits: {hits:?}"
        );
    }

    #[test]
    fn search_finds_endloop_help() {
        let hits = search("statement loop endloop ignored result", 5);
        assert!(
            hits.iter().any(|hit| hit.card.id == "syntax.loops"),
            "expected loops card in hits: {hits:?}"
        );
    }

    #[test]
    fn search_finds_mcp_workspace_cwd_help() {
        let hits = search("relative file paths workspace_cwd", 5);
        assert!(
            hits.iter().any(|hit| hit.card.id == "operation.mcp"),
            "expected MCP operation card in hits: {hits:?}"
        );
    }

    #[test]
    fn exact_topic_and_service_lookup_work() {
        assert_eq!(find("syntax.loops").unwrap().title, "loops");
        assert_eq!(
            find("syntax.operators").unwrap().title,
            "operators and comparisons"
        );
        assert_eq!(
            service("agent_ask_text").unwrap().id,
            "service.Agent.agent_ask_text"
        );
        assert_eq!(
            service("array_make_int").unwrap().id,
            "service.Array.array_make_int"
        );
        assert_eq!(
            service("array_make").unwrap().id,
            "service.Array.array_make"
        );
        assert_eq!(service("Random").unwrap().id, "service.Random");
        assert_eq!(service("Db").unwrap().id, "service.Db");
        assert_eq!(service("Agent").unwrap().id, "service.Agent");
    }

    #[test]
    fn related_links_point_to_existing_cards() {
        let ids: HashSet<&str> = all_cards().map(|card| card.id).collect();
        for card in all_cards() {
            for related in card.related {
                assert!(
                    ids.contains(related),
                    "{} links to missing help card {}",
                    card.id,
                    related
                );
            }
        }
    }

    #[test]
    fn json_is_machine_readable_shape() {
        let json = search_json("loop", &search("loop", 2));
        assert!(json.contains("\"schema\":\"locus-help/1\""));
        assert!(json.contains("\"results\""));
    }
}
