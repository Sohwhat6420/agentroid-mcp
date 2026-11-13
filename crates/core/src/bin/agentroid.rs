//! `agentroid`: the command-line tools that need no daemon (D3, 10 Nov 2026).
//!
//! ```text
//! agentroid audit verify <log.jsonl> [--head SEQ:HASH] [--anchor PATH]
//! agentroid ask-screen --device <serial|ip:port> --minutes N --why "TEXT" [--route FILE] ...
//! ```
//!
//! `ask-screen` (INTERIM, 13 Nov 2026): run it before taking over a phone's screen. It reads
//! whether the phone is in use and, if so, asks its owner with a Yes / No push; see
//! `agentroid_core::askscreen`. Exit codes: 0 allowed, 1 refused, 2 usage, 3 phone unreadable.
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
        Some("ask-screen") => {
            let (out, code) = agentroid_core::askscreen::command(&args[1..]);
            // the decision is a JSON line on stdout whatever it is; only usage errors go to stderr
            if code == 2 {
                eprintln!("{out}");
            } else {
                println!("{out}");
            }
            std::process::exit(code);
        }
        _ => (
            format!(
                "usage: agentroid audit verify <log.jsonl> [--head SEQ:HASH] [--anchor PATH]
{}",
                agentroid_core::askscreen::USAGE
            ),
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
