// Checks that the stated Phase 2 order in the build plan document is a valid topological order
// of the "Depends on:" lines in the Phase 2 task blocks. Node built-ins only; no child processes.
import { readFileSync } from "node:fs";

const file = process.argv[2];
if (!file) {
  console.error("usage: node scripts/toposort.mjs <build-plan.md>");
  process.exit(2);
}

const lines = readFileSync(file, "utf8").split(/\r?\n/);
const ID = "\\d+\\.\\d+[ab]?";

// 1. The stated order: the line containing "Phase 2 order:".
const orderLine = lines.find((l) => l.includes("Phase 2 order:"));
if (!orderLine) {
  console.error("no 'Phase 2 order:' line found");
  process.exit(2);
}
const afterMarker = orderLine.split("Phase 2 order:")[1].split(". The blocks")[0];
const order = afterMarker
  .replace(/\([^)]*\)/g, "")
  .split("→")
  .map((s) => s.replace(/[*\s]/g, ""))
  .filter(Boolean);

// 2. The blocks: from "Phase 2 task blocks" up to the Phase 3 heading.
const start = lines.findIndex((l) => l.startsWith("**Phase 2 task blocks"));
const end = lines.findIndex((l, i) => i > start && l.startsWith("**Phase 3"));
if (start < 0 || end < 0) {
  console.error("could not find the Phase 2 task blocks region");
  process.exit(2);
}

const deps = new Map();
let current = null;
for (const line of lines.slice(start + 1, end)) {
  const h = line.match(new RegExp("^\\*\\*(" + ID + ")\\s"));
  if (h) {
    current = h[1];
    deps.set(current, null);
    continue;
  }
  const d = line.match(/^Depends on:\s*(.*)$/);
  if (d && current && deps.get(current) === null) {
    // Drop parenthetical notes, then take the leading id of each comma-separated item.
    const text = d[1].replace(/\([^)]*\)/g, "");
    const ids = text
      .split(",")
      .map((s) => s.trim().split(/\s/)[0])
      .filter(Boolean);
    deps.set(current, ids);
  }
}

const violations = [];
const phase2 = new Set(deps.keys());

for (const [id, ds] of deps) {
  if (ds === null) violations.push("block " + id + " has no 'Depends on:' line");
}
for (const id of order) {
  if (!phase2.has(id)) violations.push("ordered id " + id + " has no Phase 2 block");
}
for (const id of phase2) {
  if (!order.includes(id)) violations.push("block " + id + " is missing from the stated order");
}
const dup = order.filter((id, i) => order.indexOf(id) !== i);
for (const id of new Set(dup)) violations.push("id " + id + " appears twice in the stated order");

// Ids outside Phase 2 (1.4, S4, S5, S1 ...) are external prerequisites. Accept the known
// shapes, flag anything else as unknown.
const external = new Set();
for (const [id, ds] of deps) {
  for (const dep of ds ?? []) {
    if (phase2.has(dep)) continue;
    if (/^S\d+$/.test(dep) || new RegExp("^" + ID + "$").test(dep)) external.add(dep);
    else violations.push(id + " depends on unknown id '" + dep + "'");
  }
}

// Order check: every Phase 2 dependency must come earlier.
const pos = new Map(order.map((id, i) => [id, i]));
for (const id of order) {
  for (const dep of deps.get(id) ?? []) {
    if (!phase2.has(dep)) continue;
    if (!pos.has(dep)) continue;
    if (pos.get(dep) > pos.get(id)) {
      violations.push(id + " (position " + (pos.get(id) + 1) + ") depends on " + dep + " (position " + (pos.get(dep) + 1) + "), which comes later");
    }
  }
}

// Cycle check (Kahn) over Phase 2 edges.
const indeg = new Map([...phase2].map((id) => [id, 0]));
for (const [id, ds] of deps) {
  for (const dep of ds ?? []) if (phase2.has(dep)) indeg.set(id, indeg.get(id) + 1);
}
const queue = [...indeg].filter(([, n]) => n === 0).map(([id]) => id);
let seen = 0;
while (queue.length) {
  const n = queue.shift();
  seen++;
  for (const [id, ds] of deps) {
    if ((ds ?? []).includes(n)) {
      indeg.set(id, indeg.get(id) - 1);
      if (indeg.get(id) === 0) queue.push(id);
    }
  }
}
if (seen !== phase2.size) violations.push("dependency cycle among Phase 2 blocks");

console.log("Phase 2 order (" + order.length + "): " + order.join(" -> "));
console.log("Blocks parsed: " + phase2.size + "; external prerequisites: " + [...external].sort().join(", "));
if (violations.length) {
  for (const v of violations) console.log("VIOLATION: " + v);
  process.exit(1);
}
console.log("OK: the stated order is a valid topological order.");
