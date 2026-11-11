// First-paint budget on the built page, headless bundled Chromium,
// served from 127.0.0.1. Measures, over 7 cold loads each (median reported):
//   FCP          first-contentful-paint (the skeleton)
//   data         first paint with real data (main loses aria-busy), fixture "running"
// with no throttling and again with 4x CPU throttling (a slow laptop), and the dist/ sizes.
//   exit 0 = median FCP and median data paint are both <= 1000 ms in both runs
import { readdirSync, statSync, readFileSync } from 'node:fs';
import { gzipSync } from 'node:zlib';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { launch } from './pw.mjs';
import { serve } from './serve.mjs';

const dist = fileURLToPath(new URL('../dist', import.meta.url));
function walk(d) { return readdirSync(d).flatMap(f => { const p = join(d, f); return statSync(p).isDirectory() ? walk(p) : [p]; }); }
const files = walk(dist).filter(p => !p.includes(`${join('dist', 'fixtures')}`));
let raw = 0, gz = 0;
const sizes = files.map(p => { const b = readFileSync(p); const g = /\.(woff2|jpg)$/.test(p) ? b.length : gzipSync(b).length; raw += b.length; gz += g; return `${p.slice(dist.length + 1)}: ${b.length} B (${g} B gzip)`; });
console.log('dist (without fixtures/):'); for (const s of sizes) console.log('  ' + s);
console.log(`  total ${raw} B, ${gz} B over the wire with gzip`);

const L = await launch();
if (!L) { console.log('UNMEASURED: no local playwright-core with an installed Chromium; first paint not measured'); process.exit(3); }
const srv = await serve();
const median = xs => { const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length / 2)]; };
let ok = true;
try {
  for (const cpu of [1, 4]) {
    const fcp = [], data = [];
    for (let i = 0; i < 7; i++) {
      const ctx = await L.browser.newContext({ viewport: { width: 1440, height: 900 } });
      const page = await ctx.newPage();
      await page.route('**/*', r => (r.request().url().startsWith(srv.url) ? r.continue() : r.abort()));
      if (cpu > 1) { const cdp = await ctx.newCDPSession(page); await cdp.send('Emulation.setCPUThrottlingRate', { rate: cpu }); }
      await page.goto(`${srv.url}?fixture=running&freeze=1`, { waitUntil: 'load' });
      await page.waitForFunction(() => document.getElementById('main')?.getAttribute('aria-busy') !== 'true', null, { timeout: 10000 });
      const m = await page.evaluate(() => new Promise(res => requestAnimationFrame(() => requestAnimationFrame(() => {
        const p = performance.getEntriesByName('first-contentful-paint')[0];
        res({ fcp: p ? p.startTime : null, data: performance.now() });
      }))));
      fcp.push(m.fcp); data.push(m.data);
      await ctx.close();
    }
    const f = median(fcp), d = median(data);
    const pass = f != null && f <= 1000 && d <= 1000; if (!pass) ok = false;
    console.log(`cpu x${cpu}: FCP median ${f?.toFixed(0)} ms (runs ${fcp.map(x => x?.toFixed(0)).join(', ')}); data painted by ${d.toFixed(0)} ms (upper bound; runs ${data.map(x => x.toFixed(0)).join(', ')}) -> ${pass ? 'within 1 s' : 'OVER 1 s'}`);
  }
} finally { await L.browser.close(); await srv.close(); }
console.log('note: served from 127.0.0.1 with no network latency, as bridged serves it on the same laptop; over the tailnet (hub mode) add one round trip per request');
process.exit(ok ? 0 : 1);
