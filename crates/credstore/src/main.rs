//! credstore: one JSON request on stdin, one JSON answer on stdout (see lib.rs for the protocol).
//! Spawned hidden by the MCP server (apps/mcp/src/tokens.ts). Secrets go over the pipes only,
//! never the command line, and never to stderr.

use std::io::{Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut input = String::new();
    let out = match std::io::stdin().take(64 * 1024).read_to_string(&mut input) {
        Ok(_) => credstore::handle(&input),
        Err(_) => serde_json::json!({ "ok": false, "error": "cannot read the request from stdin" }),
    };
    let ok = out.get("ok") == Some(&serde_json::Value::Bool(true));
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{out}").is_err() {
        return ExitCode::from(2);
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
