//! Driver smoke test for `locus emit-interface` (separate-compilation v1,
//! Sprint 1). Runs the compiled `locus` binary on a small single-module program
//! and asserts it emits a **non-empty, parseable** `.locusi` whose round-trip
//! (`emit → parse`) recovers an equal interface structure.

use std::process::Command;

use locus::iface;

/// A small services library: a `Box[a]` sum + two functions, one (`mapBox`)
/// carrying a `{gc}` effect row (it allocates).
const BOX_LIB: &str = "module Data.Box at services exposing (Box, unbox, mapBox) =\n\
     type Box[a] = Box(a) in\n\
     let unbox = fn b: Box[Int] => match b with | Box(x) => x in\n\
     let mapBox = fn b: Box[Int] => Box(b) in\n\
     ()\n\
     ()\n";

#[test]
fn emit_interface_produces_a_parseable_locusi() {
    // Locate the just-built `locus` binary (cargo sets CARGO_BIN_EXE_<name>).
    let bin = env!("CARGO_BIN_EXE_locus");

    let dir = std::env::temp_dir();
    let src = dir.join("locus_emit_smoke.locus");
    std::fs::write(&src, BOX_LIB).expect("write sample source");

    let out = Command::new(bin)
        .arg("emit-interface")
        .arg(&src)
        .output()
        .expect("run `locus emit-interface`");

    assert!(
        out.status.success(),
        "emit-interface exited {:?}; stderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let text = String::from_utf8(out.stdout).expect("interface text is UTF-8");
    assert!(!text.trim().is_empty(), "emitted interface is non-empty");
    assert!(
        text.contains("module Data.Box at services"),
        "header present"
    );
    assert!(text.contains("{gc}"), "mapBox publishes its {{gc}} row");

    // The emitted text re-parses to a structure (the producer/consumer bridge).
    let parsed = iface::parse(&text).expect("emitted .locusi re-parses");
    assert_eq!(parsed.name, "Data.Box");
    assert!(parsed.vals.iter().any(|v| v.name == "mapBox"));
    assert!(parsed.types.iter().any(|t| t.name == "Box"));

    // Re-serializing the parsed interface reproduces the same text (idempotent).
    assert_eq!(
        iface::serialize(&parsed),
        text,
        "serialize ∘ parse is stable"
    );

    let _ = std::fs::remove_file(&src);
}
