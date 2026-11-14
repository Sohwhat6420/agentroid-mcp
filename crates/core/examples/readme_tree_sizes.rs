//! The sizes in the README's "Real output" section, measured on a real screen.
//!
//!     cargo run -p agentroid-core --example readme_tree_sizes
//!
//! Input: `tests/redteam/recorded/poco-launcher-2026-10-20.xml`, a `uiautomator dump` of the home screen
//! of a Xiaomi POCO X6 Pro (Android 15, HyperOS 2.0), recorded on 20 Oct 2026. Nothing here talks to a
//! phone or a model: it runs the same parser, refs and listing code that `ui_tree` runs in the daemon.
//!
//! It prints, in characters:
//! * the raw dump (what an agent would read without Agentroid),
//! * the full `ui_tree` listing, and the `interactive_only` listing,
//! * the `digest` level, which the MCP server cuts at 400 characters plus a one-line note
//!   (`TEXT_LIMIT` in `apps/mcp/src/server-core.ts`),
//! * a `since` diff of the same dump read twice (nothing changed on screen).
//!
//! The listing lines are printed first, so the README's snippets can be copied from the output. The
//! one-line header the daemon puts above a listing (backend, read time, snapshot id) is left out, so
//! the counts are for the listing alone.

use agentroid_core::refs::{diff, listing, Snapshot};
use agentroid_core::tree::parse;
use std::path::PathBuf;

/// The MCP server's `digest` cut (`shapeText` in `apps/mcp/src/server-core.ts`).
const DIGEST_LIMIT: usize = 400;

fn digest(text: &str) -> String {
    let n = text.chars().count();
    if n <= DIGEST_LIMIT {
        return text.to_string();
    }
    let cut: String = text.chars().take(DIGEST_LIMIT).collect();
    format!(
        "{cut}\n[cut: {} more characters; ask for level \"default\"]",
        n - DIGEST_LIMIT
    )
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn main() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/redteam/recorded/poco-launcher-2026-10-20.xml");
    let xml = std::fs::read_to_string(&path).expect("read the recorded dump");

    let full_tree = parse(&xml, false).expect("parse the dump");
    let full = Snapshot::of("s1".into(), false, &full_tree, None);
    let full_text = listing(&full);

    let inter_tree = parse(&xml, true).expect("parse the dump, interactive only");
    let inter = Snapshot::of("s1".into(), true, &inter_tree, Some(&full_tree));
    let inter_text = listing(&inter);

    let again = Snapshot::of("s2".into(), false, &parse(&xml, false).unwrap(), None);
    let same = diff(&full, &again);

    let dig = digest(&full_text);

    println!("== ui_tree, level full ==\n{full_text}\n");
    println!("== ui_tree, interactive_only ==\n{inter_text}\n");
    println!("== ui_tree, level digest (the default) ==\n{dig}\n");
    println!("== ui_tree since=s1, same screen ==\n{}\n", same.text);

    let raw = chars(&xml);
    let rows = [
        ("raw uiautomator dump", raw),
        ("ui_tree full listing", chars(&full_text)),
        ("ui_tree interactive_only", chars(&inter_text)),
        ("ui_tree digest (default)", chars(&dig)),
        ("ui_tree since=, nothing changed", chars(&same.text)),
    ];
    println!("== sizes (characters) ==");
    for (what, n) in rows {
        println!(
            "{what:<34} {n:>6}  {:>5.1}% of the raw dump",
            n as f64 / raw as f64 * 100.0
        );
    }
    println!(
        "elements: {} listed, {} interactive",
        full.rows.len(),
        inter.rows.len()
    );
}
