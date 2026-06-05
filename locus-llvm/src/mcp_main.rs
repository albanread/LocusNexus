fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => locus_llvm::mcp::serve_blocking_stdio(),
        Some("worker") if args.len() == 1 => locus_llvm::mcp::worker_blocking_stdio(),
        Some(other) => Err(format!(
            "unknown command `{other}` (try no arguments for MCP stdio)"
        )),
    };
    let code = match result {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("locusc-mcp: {msg}");
            2
        }
    };
    std::process::exit(code);
}
