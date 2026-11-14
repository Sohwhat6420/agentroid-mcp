// Writes the count_tokens request bodies for one recorded phone screen, shown five ways.
// The counts in token-counts.json (and so in the README) come from POSTing these files to
// https://api.anthropic.com/v1/messages/count_tokens, which is free and makes no model call.
// This script only writes files: no network, no key.
//
//   cargo run -p agentroid-core --example readme_tree_sizes > tree_sizes.txt
//   node docs/media/src/make_token_bodies.mjs tree_sizes.txt crates/core/tests/redteam/recorded/poco-launcher-2026-10-20.xml bodies claude-opus-5-5
//
// Each body is the screen text alone, as one user message. screen-empty.json holds a single full stop:
// it counts 9 tokens (8 for the message wrapper, 1 for the full stop), so a text's own size is its count minus 8.
import fs from "node:fs";
import path from "node:path";

const [sizesFile, rawFile, outDir, model] = process.argv.slice(2);
if (!sizesFile || !rawFile || !outDir || !model) {
  console.error("usage: node make_token_bodies.mjs <tree_sizes.txt> <raw dump.xml> <out dir> <model>");
  process.exit(2);
}
const text = fs.readFileSync(sizesFile, "utf8").replace(/\r\n/g, "\n");
const sections = {};
let cur = null;
for (const line of text.split("\n")) {
  const m = line.match(/^== (.+) ==$/);
  if (m) { cur = m[1]; sections[cur] = []; continue; }
  if (cur) sections[cur].push(line);
}
const pick = (k) => {
  if (!sections[k]) throw new Error(`no "== ${k} ==" section in ${sizesFile}`);
  return sections[k].join("\n").replace(/\n+$/, "");
};
const views = {
  raw: fs.readFileSync(rawFile, "utf8"),
  full: pick("ui_tree, level full"),
  interactive: pick("ui_tree, interactive_only"),
  digest: pick("ui_tree, level digest (the default)"),
  since: pick("ui_tree since=s1, same screen"),
  empty: "",
};
fs.mkdirSync(outDir, { recursive: true });
const chars = {};
for (const [k, v] of Object.entries(views)) {
  const body = { model, messages: [{ role: "user", content: v === "" ? "." : v }] };
  fs.writeFileSync(path.join(outDir, `screen-${k}.json`), JSON.stringify(body));
  chars[k] = v.length;
}
console.log(JSON.stringify({ chars }));
