//! `locusc` — the Locus **compiler driver**.
//!
//! The std-only `locus` CLI does the front-end views (check / sema / ir /
//! evidence); this is the LLVM-backed half:
//!
//! - `locusc run FILE`            — JIT and execute (side effects happen; the
//!                                 program's `i64` becomes the exit code).
//! - `locusc build FILE [-o EXE]` — AOT-compile to a standalone `.exe`.
//! - `locusc asm FILE [-o OUT.s]` — dump the generated **x86-64 assembly** (the
//!                                 systems-level view of the high-level program).
//!
//! All run the same front end (`locus::parse` → `elaborate` → `lower`) to the
//! ANF IR, then hand it to the backend.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::OnceLock;

const USAGE: &str = "\
locusc — the Locus compiler (JIT + AOT)

USAGE:
  locusc run     FILE            JIT-compile and run FILE
  locusc build   FILE [-o EXE]   compile FILE to a standalone .exe
                   [--always-gc] link the collector even if FILE doesn't allocate
  locusc asm     FILE [-o OUT.s] dump the generated x86-64 assembly
  locusc effects FILE [--json]   print FILE's effect manifest (what it touches);
                                 --json emits a stable, diffable manifest for CI
  locusc republish [DIR]         write the embedded stdlib to DIR for review
  locusc --help

`run`'s exit code is the program's i64 result; effects print as they execute.
`asm` writes to stdout unless `-o` is given — the same code the .exe contains.

Minting (`extern`, raw memory) is a `boundary`-only capability: app code may not
name it — every command rejects an app-level mint (`RN-E0402`). The platform team
mints in `boundary` modules (baked in) and authorizes user ones via `locus.toml
[boundary]`; the app team's compiler is locked.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("locusc: {msg}");
            2
        }
    };
    process::exit(code);
}

fn dispatch(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("asm") => cmd_asm(&args[1..]),
        Some("effects") => cmd_effects(&args[1..]),
        Some("republish") => cmd_republish(&args[1..]),
        None | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            Ok(0)
        }
        Some(other) => Err(format!(
            "unknown command `{other}` (try `run`, `build`, `asm`, `effects`, `republish`, `--help`)"
        )),
    }
}

/// Discover and read the project's **boundary manifest** — the `locus.toml`
/// `[boundary] modules = […]` trust root (S2b). Walks up from the source file's
/// directory to the filesystem root; returns the authorized boundary-module names
/// (empty if no `locus.toml` / no `[boundary]` section is found, i.e. the secure
/// default — no *user* module may mint).
fn read_boundary_manifest(source_file: &Path) -> HashSet<String> {
    let mut dir = source_file.parent();
    while let Some(d) = dir {
        let candidate = d.join("locus.toml");
        if candidate.is_file() {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                return parse_boundary_modules(&text);
            }
            break;
        }
        dir = d.parent();
    }
    HashSet::new()
}

/// Extract the names from `[boundary] modules = ["A", "B"]` — a minimal hand-parse
/// (the project keeps a TOML dependency out for this one tiny key). Reads the
/// `[boundary]` table up to the next `[table]`, finds the `modules` array, and
/// collects its quoted strings (the array may span lines).
fn parse_boundary_modules(toml: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(start) = toml.find("[boundary]") else {
        return out;
    };
    let after = &toml[start + "[boundary]".len()..];
    let section = match after.find("\n[") {
        Some(i) => &after[..i],
        None => after,
    };
    let Some(m) = section.find("modules") else {
        return out;
    };
    let rest = &section[m..];
    let Some(lb) = rest.find('[') else {
        return out;
    };
    let Some(rb_off) = rest[lb..].find(']') else {
        return out;
    };
    let array = &rest[lb + 1..lb + rb_off];
    let mut cur = String::new();
    let mut in_str = false;
    for ch in array.chars() {
        match ch {
            '"' if in_str => {
                out.insert(std::mem::take(&mut cur));
                in_str = false;
            }
            '"' => in_str = true,
            _ if in_str => cur.push(ch),
            _ => {}
        }
    }
    out
}

/// The mint-gate (S2): `extern` / `extern asm` / foreign-bind may appear only in
/// an authorized `at boundary` module. App code (the entry) and user modules are
/// checked; the bundled stdlib boundary modules are trusted by construction.
/// `authorized` is the project's `locus.toml [boundary]` set (S2b) — empty by
/// default, so an un-manifested `at boundary` claim is rejected (`RN-E0404`).
fn guard_layer2(src: &str, authorized: &HashSet<String>) -> Result<(), String> {
    // The **mint-gate**, enforced on *every* CLI entry point (`run`/`build`/`asm`/
    // `effects`). Minting (`extern`, the raw memory primitives, and the coming
    // `extern asm` / foreign-bind) is `boundary`-only and manifest-authorized; a
    // violation **blocks**. The platform team mints in boundary modules (baked into
    // this binary) and authorizes any user boundary module via `locus.toml
    // [boundary]`; app code that reaches the raw boundary is rejected outright
    // (`RN-E0402` / `RN-E0404`). This is the "hand the app team a locked compiler"
    // guarantee — see `docs/user-guide-mint-and-seal.md`. (Legitimate FFI is done
    // in a boundary module, or via the library API the test harness uses.)
    let prog = locus::parse_program(src).map_err(|e| e.msg)?;
    locus::mint_gate(&prog.entry, &prog.modules, authorized)
        .map_err(|e| format!("[{}] {e}", e.code()))
}

/// Front end: source → ANF IR + the demanded Win32 DLLs. `program` grafts the
/// prelude (e.g. `writeln`); the resolver fills bare `extern "Sym"` from the
/// Win32 oracle and collects the DLLs the AOT linker will need.
fn to_ir(src: &str) -> Result<(locus::Ir, locus_llvm::winapi_resolve::Demanded), String> {
    let (term, user_modules) = locus::program_with_modules(src).map_err(|e| e.msg)?;
    let (term, demanded) = locus_llvm::winapi_resolve::resolve(term)?;
    let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
        .map_err(|e| e.to_string())?;
    // Enforce each module's `seals (…)` clause over the elaborated exports (S4):
    // no exposed binding may carry a sealed label. Covers the included stdlib
    // services (e.g. Console seals winapi) and the user modules.
    let mut all_modules = locus::stdlib_module_decls();
    all_modules.extend(user_modules);
    locus::check_module_seals(&all_modules, &tree).map_err(|e| e.to_string())?;
    // Run the generators (staging) at compile time, leaving residual object code.
    let tree = locus::stage_reduce(&tree)?;
    if tree.has_unknown_layout() {
        return Err(locus::TypeErr::RepresentationPolymorphicLayout.to_string());
    }
    Ok((locus::lower(&tree), demanded))
}

fn read(file: &str) -> Result<String, String> {
    std::fs::read_to_string(file).map_err(|e| format!("reading `{file}`: {e}"))
}

fn cmd_run(args: &[String]) -> Result<i32, String> {
    let file = args.first().ok_or("usage: locusc run FILE")?;
    let src = read(file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(file)))?;
    let (ir, apis) = to_ir(&src)?;
    // The program runs here — its effects execute and its i64 is the exit code.
    let result = locus_llvm::jit_run_i64(&ir, &apis)?;
    Ok(result as i32)
}

fn cmd_build(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut always_gc = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("`-o` needs a path")?.clone()),
            "--always-gc" => always_gc = true,
            other => file = Some(other.to_string()),
        }
    }
    let file = file.ok_or("usage: locusc build FILE [-o EXE] [--always-gc]")?;
    let src = read(&file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(&file)))?;
    let (ir, apis) = to_ir(&src)?;
    let exe = out
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&file).with_extension("exe"));
    // Each demanded DLL → its import lib, for the linker (kernel32 is free).
    let import_libs = locus_llvm::winapi_resolve::import_libs(&apis);
    locus_llvm::build_exe(&ir, &exe, &import_libs, always_gc)?;
    println!("built {}", exe.display());
    Ok(0)
}

/// `asm FILE [-o OUT.s]` — dump the host x86-64 assembly the backend generates
/// for the program (the same code the `.exe` carries). To stdout by default, or
/// to a file with `-o`. No JIT, no link — just front end → IR → `TargetMachine`.
fn cmd_asm(args: &[String]) -> Result<i32, String> {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("`-o` needs a path")?.clone()),
            other => file = Some(other.to_string()),
        }
    }
    let file = file.ok_or("usage: locusc asm FILE [-o OUT.s]")?;
    let src = read(&file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(&file)))?;
    let (ir, _apis) = to_ir(&src)?;
    let asm = locus_llvm::emit_asm(&ir)?;
    match out {
        Some(path) => {
            std::fs::write(&path, &asm).map_err(|e| format!("writing `{path}`: {e}"))?;
            println!("wrote {path}");
        }
        None => print!("{asm}"),
    }
    Ok(0)
}

/// The embedded effect-catalog source — data the compiler ships, `republish`
/// emits, and a project may edit and rebuild. Single source for both the loader
/// and `republish`, so the emitted copy is byte-identical to the one in use.
const EFFECT_CATALOG: &str = include_str!("effects.catalog");

/// The effect catalog — the category roll-up order, and per-label / per-kind
/// category + gloss. **Data, not hardcoded logic**: parsed from the embedded
/// `effects.catalog` (which `republish` emits), so a project owns its taxonomy.
struct Catalog {
    /// Categories in display order.
    order: Vec<String>,
    /// Explicit `label -> (category, gloss)`.
    by_label: HashMap<String, (String, String)>,
    /// Fallback `kind -> (category, gloss)` for labels not named explicitly.
    by_kind: HashMap<String, (String, String)>,
}

/// Parse the embedded catalog once. Lenient: blank / `#` lines and malformed
/// lines are skipped, so a missing entry degrades to the ultimate default rather
/// than failing the command.
fn load_catalog() -> Catalog {
    let mut order = Vec::new();
    let mut by_label = HashMap::new();
    let mut by_kind = HashMap::new();
    for line in EFFECT_CATALOG.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tok: Vec<&str> = line.split_whitespace().collect();
        if tok[0] == "order" {
            order = tok[1..].iter().map(|s| s.to_string()).collect();
            continue;
        }
        if tok.len() < 3 {
            continue;
        }
        let entry = (tok[1].to_string(), tok[2..].join(" "));
        if let Some(kind) = tok[0].strip_prefix("kind:") {
            by_kind.insert(kind.to_string(), entry);
        } else {
            by_label.insert(tok[0].to_string(), entry);
        }
    }
    Catalog {
        order,
        by_label,
        by_kind,
    }
}

/// The catalog, loaded once, before any lookup (`effects` consults it).
fn catalog() -> &'static Catalog {
    static C: OnceLock<Catalog> = OnceLock::new();
    C.get_or_init(load_catalog)
}

/// A label's KIND — the fallback key when it isn't named explicitly.
fn label_kind(l: &locus::Label) -> &'static str {
    use locus::Label::*;
    match l {
        World(_) => "world",
        User(_) => "user",
        Exn(_) => "exn",
        Gc => "gc",
        // `st` — observable `Ref` mutation (mutability.md §2). Its own kind so the
        // effect manifest groups it distinctly; the catalog's `by_kind`/default
        // fallback supplies the gloss when "state" has no explicit entry.
        St => "state",
        Insert => "staging",
    }
}

/// `(category, gloss)` for a label: explicit entry wins, else the kind fallback,
/// else the ultimate default. Strings live in the `'static` catalog.
fn lookup(l: &locus::Label) -> (&'static str, &'static str) {
    let cat = catalog();
    let name = format!("{l}");
    if let Some((c, g)) = cat.by_label.get(&name) {
        return (c.as_str(), g.as_str());
    }
    if let Some((c, g)) = cat.by_kind.get(label_kind(l)) {
        return (c.as_str(), g.as_str());
    }
    ("user", "effect")
}

/// Which bucket an effect label rolls up into (from the catalog).
fn category(l: &locus::Label) -> &'static str {
    lookup(l).0
}

/// `effects FILE [--json]` — print FILE's effect MANIFEST: its type and the
/// effects it performs, grouped by category and glossed. This is the "review
/// everything" surface — the transparency that makes layer-2 work by interns /
/// sub-agents auditable. A change that grows the row (a stray `{winapi}`, a new
/// `{net}`) shows up here; `--json` emits a stable, sorted manifest you can commit
/// as a golden file and **diff** in CI, so the review signal is the *delta*.
/// Permissive (no capability guard): inspection must work on any code, layer 0
/// included.
fn cmd_effects(args: &[String]) -> Result<i32, String> {
    let mut file: Option<&str> = None;
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            other => file = Some(other),
        }
    }
    let file = file.ok_or("usage: locusc effects FILE [--json]")?;
    let src = read(file)?;
    guard_layer2(&src, &read_boundary_manifest(Path::new(file)))?;
    let term = locus::program(&src).map_err(|e| e.msg)?;
    let (term, _apis) = locus_llvm::winapi_resolve::resolve(term)?;
    let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
        .map_err(|e| e.to_string())?;
    // `labels()` walks a BTreeSet, so the order is sorted and stable — diffable.
    let labels: Vec<&locus::Label> = tree.row.labels().collect();
    let ty = tree.ty.to_string();
    if json {
        print_effects_json(file, &ty, &labels);
    } else {
        print_effects_human(file, &ty, &labels);
    }
    Ok(0)
}

/// Group the labels into the non-empty categories, in `CATEGORY_ORDER`.
fn grouped<'a>(labels: &[&'a locus::Label]) -> Vec<(&'static str, Vec<&'a locus::Label>)> {
    let mut out = Vec::new();
    for cat in &catalog().order {
        let cat = cat.as_str();
        let in_cat: Vec<&locus::Label> = labels
            .iter()
            .copied()
            .filter(|l| category(l) == cat)
            .collect();
        if !in_cat.is_empty() {
            out.push((cat, in_cat));
        }
    }
    out
}

/// The human manifest: type, a one-line summary (explicit set when small, a
/// category roll-up when wide), then the legend grouped by category.
fn print_effects_human(file: &str, ty: &str, labels: &[&locus::Label]) {
    println!("{file}");
    println!("  type    : {ty}");
    if labels.is_empty() {
        println!("  effects : {{}}  — pure (touches nothing outside itself)");
        return;
    }
    let cats = grouped(labels);
    if labels.len() <= 8 {
        let names: Vec<String> = labels.iter().map(|l| format!("{l}")).collect();
        println!("  effects : {{ {} }}", names.join(", "));
    } else {
        let roll: Vec<String> = cats
            .iter()
            .map(|(c, ls)| format!("{c} {}", ls.len()))
            .collect();
        println!(
            "  effects : {} in {} categories  ({})",
            labels.len(),
            cats.len(),
            roll.join(", ")
        );
    }
    for (cat, ls) in &cats {
        println!();
        println!("  {cat} ({})", ls.len());
        for l in ls {
            println!("    {:<10} {}", format!("{l}"), describe(l));
        }
    }
}

/// The machine manifest: stable, sorted JSON for `git diff` / CI policy. Labels
/// are the diff signal (glosses are derived, so they're left out to keep diffs
/// quiet). No serde dependency — the shape is small and fixed.
fn print_effects_json(file: &str, ty: &str, labels: &[&locus::Label]) {
    let cats = grouped(labels);
    println!("{{");
    println!("  \"file\": \"{}\",", json_escape(file));
    println!("  \"type\": \"{}\",", json_escape(ty));
    println!("  \"pure\": {},", labels.is_empty());
    println!("  \"effect_count\": {},", labels.len());
    let counts: Vec<String> = cats
        .iter()
        .map(|(c, ls)| format!("\"{c}\": {}", ls.len()))
        .collect();
    println!("  \"categories\": {{ {} }},", counts.join(", "));
    println!("  \"effects\": [");
    for (i, l) in labels.iter().enumerate() {
        let comma = if i + 1 < labels.len() { "," } else { "" };
        println!(
            "    {{ \"label\": \"{}\", \"category\": \"{}\" }}{comma}",
            json_escape(&format!("{l}")),
            category(l)
        );
    }
    println!("  ]");
    println!("}}");
}

/// Minimal JSON string escaping (no dependency for a fixed, small shape).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A one-line gloss for an effect label — the legend in the `effects` manifest,
/// read from the catalog.
fn describe(l: &locus::Label) -> &'static str {
    lookup(l).1
}

/// `republish [DIR]` — write the compiler's embedded platform to DIR (default
/// `./stdlib-republished`) for review: the stdlib modules (layers 0/1) **and** the
/// effect catalog (the review taxonomy). The binary is the authoritative copy;
/// this emits exactly those bytes (write-out only — the compiler never reads them
/// *back* from disk, so editing the output is inert), plus a MANIFEST with each
/// file's size and FNV-1a so anyone can verify the platform that RUNS equals the
/// one they READ.
fn cmd_republish(args: &[String]) -> Result<i32, String> {
    let dir: PathBuf = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| "stdlib-republished".into());
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating `{}`: {e}", dir.display()))?;
    let mut manifest = String::from(
        "# Locus platform - republished from the compiler's embedded (authoritative) copy.\n\
         # Compare the FNV-1a below against a copy to verify it matches this compiler.\n\
         #\n# layer  module       bytes    fnv1a-64            file\n",
    );
    for (layer, name, src) in locus::stdlib_modules() {
        let fname = format!("{layer}_{name}.locus");
        let path = dir.join(&fname);
        std::fs::write(&path, src).map_err(|e| format!("writing `{}`: {e}", path.display()))?;
        manifest.push_str(&format!(
            "  {layer:<5}  {name:<11}  {:<7}  {:#018x}  {fname}\n",
            src.len(),
            fnv1a(src)
        ));
        println!("republished {}", path.display());
    }
    // The effect catalog — review config, not a code layer, so it gets the `cfg`
    // pseudo-layer in the manifest.
    let cat_path = dir.join("effects.catalog");
    std::fs::write(&cat_path, EFFECT_CATALOG)
        .map_err(|e| format!("writing `{}`: {e}", cat_path.display()))?;
    manifest.push_str(&format!(
        "  {:<5}  {:<11}  {:<7}  {:#018x}  effects.catalog\n",
        "cfg",
        "effects",
        EFFECT_CATALOG.len(),
        fnv1a(EFFECT_CATALOG)
    ));
    println!("republished {}", cat_path.display());
    let mpath = dir.join("MANIFEST.txt");
    std::fs::write(&mpath, &manifest).map_err(|e| format!("writing `{}`: {e}", mpath.display()))?;
    println!("wrote {}", mpath.display());
    Ok(0)
}

/// FNV-1a (64-bit) — a small, dependency-free content hash, so a republished
/// module can be checked byte-for-byte against the compiler's embedded copy.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use locus::Label;

    /// The embedded catalog drives the mapping — the JSON `category` is a stable
    /// contract a CI policy keys on, so the shipped catalog must classify every
    /// label kind into a category it also lists in `order`.
    #[test]
    fn catalog_classifies_every_label_kind() {
        // winapi is the raw FFI edge — its own `boundary` bucket, NOT lumped in
        // with the abstract IO capabilities (`console` / `fs` / `net`).
        assert_eq!(category(&Label::World("winapi".into())), "boundary");
        assert_eq!(category(&Label::World("console".into())), "world");
        assert_eq!(category(&Label::World("fs".into())), "world");
        assert_eq!(category(&Label::World("mem".into())), "memory");
        assert_eq!(category(&Label::Gc), "memory");
        assert_eq!(category(&Label::Exn("Overflow".into())), "control");
        assert_eq!(category(&Label::User("Db".into())), "user");
        assert_eq!(category(&Label::Insert), "staging");
        // every category produced is one the catalog also orders (no orphan bucket)
        let order = &catalog().order;
        for l in [
            Label::World("winapi".into()),
            Label::Gc,
            Label::User("x".into()),
            Label::Insert,
        ] {
            assert!(
                order.iter().any(|c| c == category(&l)),
                "{l} fell outside the roll-up order"
            );
        }
    }

    /// A label not named explicitly falls back to its KIND — so a brand-new world
    /// label or user effect is still categorised (never silently dropped).
    #[test]
    fn unlisted_labels_fall_back_to_their_kind() {
        assert_eq!(category(&Label::World("bluetooth".into())), "world"); // unlisted world
        assert_eq!(category(&Label::User("Telemetry".into())), "user"); // unlisted user
                                                                        // and the gloss comes from the kind fallback, not empty
        assert!(!describe(&Label::User("Telemetry".into())).is_empty());
    }

    /// The hand-rolled JSON escaper covers the cases that would break a parser;
    /// UTF-8 (the em-dash glosses) passes through untouched.
    #[test]
    fn json_escape_handles_specials() {
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(json_escape("tab\tnl\n"), "tab\\tnl\\n");
        assert_eq!(json_escape("plain — unicode"), "plain — unicode");
    }

    #[test]
    fn boundary_manifest_parses_the_module_list() {
        let m = super::parse_boundary_modules(
            "[package]\nname = \"x\"\n\n[boundary]\nmodules = [\"MyFfi\", \"Gpu\"]\n",
        );
        assert!(m.contains("MyFfi") && m.contains("Gpu") && m.len() == 2);
    }

    #[test]
    fn boundary_manifest_tolerates_a_multiline_array_and_no_section() {
        let multiline =
            super::parse_boundary_modules("[boundary]\nmodules = [\n  \"A\",\n  \"B\",\n]\n");
        assert!(multiline.contains("A") && multiline.contains("B"));
        // No `[boundary]` table ⇒ empty (the secure default: nothing authorized).
        assert!(super::parse_boundary_modules("[package]\nname = \"x\"\n").is_empty());
        // A `[boundary]` after the section is not mistaken for its contents.
        let scoped =
            super::parse_boundary_modules("[boundary]\nmodules = [\"A\"]\n[other]\nx = [\"Z\"]\n");
        assert!(scoped.contains("A") && !scoped.contains("Z") && scoped.len() == 1);
    }
}
