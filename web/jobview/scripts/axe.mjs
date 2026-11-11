// axe-core (via @axe-core/playwright, installed offline from the local npm cache) on the built page:
// 390 / 820 / 1440 px, light and dark, every state that has distinct markup, plus the command palette
// open. Headless bundled Chromium only (scripts/pw.mjs): never Chrome, Edge or a real profile.
//   exit 0 = 0 serious and 0 critical violations everywhere
import { CHECKS } from './paths.mjs';
import { createRequire } from 'node:module';
import { mkdirSync, writeFileSync } from 'node:fs';
import { launch } from './pw.mjs';
import { serve } from './serve.mjs';

const require = createRequire(import.meta.url);
let AxeBuilder;
try { ({ default: AxeBuilder } = await import('@axe-core/playwright')); } catch {
  console.log('SKIPPED: @axe-core/playwright is not installed (it needs a download yes); nothing was measured');
  process.exit(4);
}
const axeVersion = require('axe-core/package.json').version;

const L = await launch();
if (!L) { console.log('UNMEASURED: no local playwright-core with an installed Chromium'); process.exit(3); }
const srv = await serve();
const STATES = ['running', 'cooling', 'waiting', 'stuck', 'stopped', 'failed', 'done', 'secure', 'stale', 'invalid-events', 'empty', 'invalid-job'];
const lines = [];
let bad = 0, runs = 0, controlOk = false;
const minor = new Map();
try {
  for (const w of [390, 820, 1440]) for (const theme of ['light', 'dark']) for (const st of [...STATES, 'running+palette']) {
    const [fixture, extra] = st.split('+');
    const ctx = await L.browser.newContext({ viewport: { width: w, height: 900 }, colorScheme: theme, reducedMotion: 'reduce' });
    const page = await ctx.newPage();
    await page.route('**/*', r => (r.request().url().startsWith(srv.url) ? r.continue() : r.abort()));
    await page.goto(`${srv.url}?fixture=${fixture}&freeze=1&theme=${theme}`, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => document.getElementById('main')?.getAttribute('aria-busy') !== 'true', null, { timeout: 5000 }).catch(() => {});
    if (extra === 'palette') { await page.keyboard.press('Control+k'); await page.waitForSelector('#pal[open]'); }
    const res = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa', 'best-practice']).analyze();
    runs++;
    const serious = res.violations.filter(v => v.impact === 'serious' || v.impact === 'critical');
    for (const v of res.violations.filter(v => !serious.includes(v))) minor.set(`${v.impact} ${v.id}`, (minor.get(`${v.impact} ${v.id}`) ?? 0) + 1);
    if (serious.length) bad++;
    const line = `${serious.length ? 'FAIL' : 'ok  '} ${st} ${theme} ${w}: ${res.passes.length} rules pass, ${serious.length} serious/critical${serious.length ? ': ' + serious.map(v => `${v.impact} ${v.id} (${v.nodes.length}: ${v.nodes.slice(0, 2).map(n => n.target.join(' ')).join(' | ')})`).join('; ') : ''}`;
    lines.push(line); console.log(line);
    await ctx.close();
  }
  // positive control: the same harness on the same page with planted violations must report them,
  // or a clean result above proves nothing
  {
    const ctx = await L.browser.newContext({ viewport: { width: 1440, height: 900 } });
    const page = await ctx.newPage();
    await page.route('**/*', r => (r.request().url().startsWith(srv.url) ? r.continue() : r.abort()));
    await page.goto(`${srv.url}?fixture=running&freeze=1`, { waitUntil: 'networkidle' });
    await page.evaluate(() => { const b = document.createElement('button'); b.className = 'btn'; document.querySelector('#controls .ctl-row').append(b); const p = document.createElement('p'); p.textContent = 'low contrast'; p.style.color = '#f4f4f4'; p.style.background = '#ffffff'; document.getElementById('state-sentence').after(p); });
    const res = await new AxeBuilder({ page }).analyze();
    const ids = res.violations.filter(v => v.impact === 'serious' || v.impact === 'critical').map(v => v.id);
    controlOk = ids.includes('button-name') && ids.includes('color-contrast');
    const line = `${controlOk ? 'ok  ' : 'FAIL'} positive control (planted unnamed button + low-contrast text): axe reported ${ids.join(', ') || 'nothing'}`;
    lines.push(line); console.log(line);
    await ctx.close();
  }
} finally { await L.browser.close(); await srv.close(); }
if (!controlOk) bad++;
const summary = `axe-core ${axeVersion}, playwright-core ${L.version} headless Chromium: ${runs} runs, ${bad} with serious/critical violations; positive control ${controlOk ? 'caught both planted violations' : 'FAILED'}` + (minor.size ? `\nminor/moderate seen (not gating): ${[...minor].map(([k, n]) => `${k} x${n}`).join(', ')}` : '\nno minor or moderate violations either');
const dir = CHECKS;
writeFileSync(new URL('axe.txt', dir), lines.join('\n') + '\n' + summary + '\n');
console.log(summary);
process.exit(bad ? 1 : 0);
