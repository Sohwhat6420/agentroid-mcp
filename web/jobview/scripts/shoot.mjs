// Screenshots of the built page in fixture mode, at 390 / 820 / 1440 px, light and dark, in headless
// bundled Chromium. Output: $JOBVIEW_SHOTS (default .shots/)/<scenario>-<theme>-<width>.png.
// Every page is also checked for console errors and for any request that leaves 127.0.0.1.
//   node scripts/shoot.mjs [scenario ...]
import { SHOTS } from './paths.mjs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { launch } from './pw.mjs';
import { serve } from './serve.mjs';

const OUT = fileURLToPath(SHOTS);
const MATRIX = ['running', 'cooling', 'stuck', 'done'];
const SINGLE = ['live-no-bridged', 'loading', 'waiting', 'stopped', 'failed', 'secure', 'stale', 'invalid-job', 'invalid-events', 'repo-stopped', 'empty'];
const WIDTHS = [390, 820, 1440], THEMES = ['light', 'dark'];

const only = process.argv.slice(2);
const jobs = [];
for (const s of MATRIX) for (const w of WIDTHS) for (const t of THEMES) jobs.push({ s, w, t });
for (const s of SINGLE) for (const t of THEMES) jobs.push({ s, w: 1440, t });
const todo = only.length ? jobs.filter(j => only.includes(j.s)) : jobs;

const L = await launch();
if (!L) { console.log('UNMEASURED: no local playwright-core with an installed Chromium'); process.exit(3); }
const srv = await serve();
await mkdir(OUT, { recursive: true });
let problems = 0;
try {
  for (const { s, w, t } of todo) {
    const ctx = await L.browser.newContext({ viewport: { width: w, height: w < 600 ? 844 : 900 }, colorScheme: t, reducedMotion: 'reduce', deviceScaleFactor: 1 });
    const page = await ctx.newPage();
    const errors = [], external = [];
    // with no bridged behind the static server, GET /jobs is a 404 by design of this test
    page.on('console', m => { if (m.type() === 'error' && !(s === 'live-no-bridged' && /404/.test(m.text()))) errors.push(m.text()); });
    page.on('pageerror', e => errors.push(String(e)));
    await page.route('**/*', r => { const u = r.request().url(); if (u.startsWith(srv.url)) return r.continue(); external.push(u); return r.abort(); });
    await page.goto(s === 'live-no-bridged' ? `${srv.url}?theme=${t}` : `${srv.url}?fixture=${s}&freeze=1&theme=${t}`, { waitUntil: 'networkidle' });
    if (s !== 'loading') await page.waitForFunction(() => document.getElementById('main')?.getAttribute('aria-busy') !== 'true', null, { timeout: 5000 }).catch(() => errors.push('never left the loading state'));
    await page.evaluate(() => document.fonts.ready);
    await page.waitForTimeout(150);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    const file = `${OUT}${s}-${t}-${w}.png`;
    // phones: the viewport, as the design's 390 shots are (the control bar is fixed to the bottom, so a
    // full-page capture shows it mid-page); the full page is kept beside it
    if (w < 600) { await page.screenshot({ path: file }); await page.screenshot({ path: file.replace(/\.png$/, '-full.png'), fullPage: true }); }
    else await page.screenshot({ path: file, fullPage: true });
    const bad = errors.length || external.length || overflow > 0;
    if (bad) problems++;
    console.log(`${bad ? 'PROBLEM' : 'ok'} ${s} ${t} ${w}${overflow > 0 ? ` hscroll=${overflow}px` : ''}${errors.length ? ` errors=${JSON.stringify(errors)}` : ''}${external.length ? ` external=${JSON.stringify(external)}` : ''}`);
    await ctx.close();
  }
} finally {
  await L.browser.close();
  await srv.close();
}
console.log(`${todo.length} shots, ${problems} with problems, in ${OUT}`);
process.exit(problems ? 1 : 0);
