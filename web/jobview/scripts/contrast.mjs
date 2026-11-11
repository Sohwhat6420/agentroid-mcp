// WCAG 2.x contrast for every token pair the page uses, in both themes, computed from the shipped CSS
// (src/styles/v2.css + app.css), not from the design file. It re-checks every pair in the design's
// contrast report and adds the pairs this build introduced (stale notice on each state tint, the
// secure-screen well, the owner line, the error text, pending/failure notes).
//   exit 0 = every pair meets its threshold
import { CHECKS } from './paths.mjs';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const css = ['v2.css', 'app.css'].map(f => readFileSync(new URL(`../src/styles/${f}`, import.meta.url), 'utf8')).join('\n');
function block(selectorRe) {
  const out = {};
  for (const m of css.matchAll(new RegExp(selectorRe.source + '\\s*\\{([^}]*)\\}', 'g'))) {
    for (const d of m[1].matchAll(/--([\w-]+)\s*:\s*(oklch\([^)]*\)|var\(--[\w-]+\))/g)) out[d[1]] = d[2];
  }
  return out;
}
const light = block(/(?:^|\n|\})\s*:root/);
const dark = { ...light, ...block(/:root\[data-theme="dark"\]/) };
const THEMES = { light, dark };

function oklchToLin(str) {
  const m = /oklch\(\s*([\d.]+)\s+([\d.]+)\s+([\d.]+)\s*(?:\/\s*([\d.]+))?\)/.exec(str);
  if (!m) throw new Error('cannot parse ' + str);
  if (m[4] != null && +m[4] < 1) return null; // translucent: not a text/background token
  const L = +m[1], C = +m[2], h = (+m[3] * Math.PI) / 180, a = C * Math.cos(h), b = C * Math.sin(h);
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3, mm = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3, s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const rgb = [4.0767416621 * l - 3.3077115913 * mm + 0.2309699292 * s, -1.2684380046 * l + 2.6097574011 * mm - 0.3413193965 * s, -0.0041960863 * l - 0.7034186147 * mm + 1.707614701 * s];
  return rgb.map(v => Math.min(1, Math.max(0, v)));
}
const lum = ([r, g, b]) => 0.2126 * r + 0.7152 * g + 0.0722 * b;
function ratio(theme, fg, bg) {
  const t = THEMES[theme];
  const val = (k, depth = 0) => { const v = t[k]; if (!v) throw new Error(`${theme}: token --${k} not found in the shipped CSS`); const a = /^var\(--([\w-]+)\)$/.exec(v); return a && depth < 5 ? val(a[1], depth + 1) : v; };
  const a = lum(oklchToLin(val(fg))), b = lum(oklchToLin(val(bg)));
  return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
}

// the design's pairs, read from its report (first table only)
const report = readFileSync(new URL('./design-contrast-pairs.md', import.meta.url), 'utf8').split(/\n\s*\n/)[0];
const pairs = [];
for (const m of report.matchAll(/^\| (light|dark) \| ([\w-]+) on ([\w-]+) \| ([\d.]+) \| ([\d.]+) \|/gm)) pairs.push({ theme: m[1], fg: m[2], bg: m[3], needs: +m[5], design: +m[4], src: 'design' });
// pairs this build added
const tints = ['run', 'cool', 'wait', 'stuck', 'fail', 'done', 'idle'];
for (const theme of ['light', 'dark']) {
  for (const s of tints) pairs.push({ theme, fg: 'fail', bg: `${s}-tint`, needs: 4.5, src: 'build: stale/invalid notice in the band' });
  pairs.push({ theme, fg: 'screen-ink', bg: 'screen-bg', needs: 4.5, src: 'build: secure-screen well' });
  pairs.push({ theme, fg: 'ink-2', bg: 'surface', needs: 4.5, src: 'build: no-frame text, notes, other jobs' });
  pairs.push({ theme, fg: 'ink-3', bg: 'surface', needs: 4.5, src: 'build: change owner line, crop caption' });
  pairs.push({ theme, fg: 'fail', bg: 'surface', needs: 4.5, src: 'build: control failure note, stale tags' });
  pairs.push({ theme, fg: 'ink', bg: 'surface-2', needs: 4.5, src: 'build: last-error text' });
  pairs.push({ theme, fg: 'line-strong', bg: 'bg', needs: 3, src: 'build: pending (dashed) button border' });
}
let fails = 0, drift = 0;
const rows = pairs.map(p => {
  const r = ratio(p.theme, p.fg, p.bg); const ok = r >= p.needs; if (!ok) fails++;
  const d = p.design != null && Math.abs(r - p.design) > 0.05; if (d) drift++;
  return `| ${p.theme} | ${p.fg} on ${p.bg} | ${r.toFixed(2)} | ${p.needs} | ${ok ? 'pass' : 'FAIL'} | ${p.design != null ? (d ? `design said ${p.design}` : 'matches design') : p.src} |`;
});
const out = `# Contrast of the shipped tokens (WCAG 2.x)\n\n| theme | pair | ratio | needs | result | note |\n|---|---|---|---|---|---|\n${rows.join('\n')}\n\n${pairs.length} checks, ${fails} fail, ${drift} differ from the design report by more than 0.05\n`;
const dir = CHECKS;
writeFileSync(new URL('contrast.md', dir), out);
console.log(`${pairs.length} checks, ${fails} fail, ${drift} drift from design; table in ${fileURLToPath(new URL('contrast.md', dir))}`);
for (const r of rows.filter(r => /FAIL|design said/.test(r))) console.log(r);
process.exit(fails ? 1 : 0);
