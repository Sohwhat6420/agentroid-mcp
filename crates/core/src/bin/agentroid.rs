//! `agentroid`: the command-line tools that need no daemon (D3, 10 Nov 2026).
//!
//! ```text
//! agentroid audit verify <log.jsonl> [--head SEQ:HASH] [--anchor PATH]
//! ```
//!
//! `verify` spans the rotated parts (`<stem>.NNNNNN.jsonl` beside the log) and the active file, then
//! checks the `.head` file and, with `--anchor`, the off-machine head copy (D3b).
//!
//! Exit codes: 0 the log verified, 1 it is broken (the first bad line is named), 2 usage.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, code) = match args.first().map(String::as_str) {
        Some("audit") if args.get(1).map(String::as_str) == Some("verify") => {
            agentroid_core::audit::verify_command(&args[2..])
        }
        _ => (
            "usage: agentroid audit verify <log.jsonl> [--head SEQ:HASH] [--anchor PATH]"
                .to_string(),
            2,
        ),
    };
    if code == 0 {
        println!("{out}");
    } else {
        eprintln!("{out}");
    }
    std::process::exit(code);
}
