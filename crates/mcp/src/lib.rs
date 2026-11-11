//! `mcp`: task 2.4 (MCP server, Phase 2 tools, receipts, health header).
//!
//! Stub. The design puts the real MCP server in TypeScript under
//! `apps/mcp`; this crate only reserves the Rust-side slot requested by HF-20.

/// Crate name, used by tests and diagnostics.
pub fn name() -> &'static str {
    "mcp"
}

#[cfg(test)]
mod tests {
    #[test]
    fn name_is_stable() {
        assert_eq!(super::name(), "mcp");
    }
}
