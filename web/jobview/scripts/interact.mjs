// Behaviour checks in headless bundled Chromium (127.0.0.1 only):
//  fixture mode: pause/resume and stop-with-confirm, the filter in the URL (CRITIQUE), the tooltip's
//  quadrant flip (CRITIQUE), keys and the palette;
//  live mode against scripts/mock-bridged.mjs: the page reads the WIRING endpoints, takes SSE events,
//  loads the frame, and shows a pending then failure state for unwired (501) and refused (403) controls.
//   exit 0 = every check passed
import { CHECKS } from './paths.mjs';
import { mkdirSync, writeFileSync } from 'node:fs';
import { launch } from './pw.mjs';
import { serve } from './serve.mjs';
import { mockBridged } from './mock-bridged.mjs';

const L = await launch();
if (!L) { console.log('UNMEASURED: no local playwright-core with an installed Chromium'); process.exit(3); }
const lines = []; let fails = 0;
const check = (ok, msg) => { if (!ok) fails++; lines.push(`${ok ? 'ok  ' : 'FAIL'} ${msg}`); console.log(lines.at(-1)); };
const word = (page) => page.textContent('#state-word');
async function newPage(url, w = 1440) {
  const ctx = await L.browser.newContext({ viewport: { width: w, height: 900 }, reducedMotion: 'reduce' });
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  const origin = new URL(url).origin;
  await page.route('**/*', r => (r.request().url().startsWith(origin) ? r.continue() : r.abort()));
  await page.goto(url, { waitUntil: 'networkidle' });
  await page.waitForFunction(() => document.getElementById('main')?.getAttribute('aria-busy') !== 'true', null, { timeout: 8000 }).catch(() => {});
  return { ctx, page, errors };
}

const srv = await serve();
try {
  // ---------------- fixture mode
  {
    const { ctx, page, errors } = await newPage(`${srv.url}?fixture=running&freeze=1`);
    check(await word(page) === 'Running', 'fixture running loads as Running');
    await page.click('[data-act="pause"]');
    await page.waitForFunction(() => document.getElementById('state-word').textContent === 'Paused by you', null, { timeout: 3000 }).catch(() => {});
    check(await word(page) === 'Paused by you', `Pause -> "${await word(page)}"`);
    check(await page.isVisible('[data-act="resume"]'), 'Resume is offered while paused');
    await page.click('[data-act="resume"]');
    await page.waitForTimeout(100);
    check(await word(page) === 'Running', `Resume -> "${await word(page)}"`);
    await page.click('[data-act="stop"]');
    const focused = await page.evaluate(() => document.activeElement?.dataset.act);
    check(await page.isVisible('.confirm') && focused === 'stop-no', `Stop asks first, focus on the safe choice (${focused})`);
    await page.keyboard.press('Escape');
    check(!(await page.isVisible('.confirm')), 'Esc cancels the stop');
    await page.click('[data-act="stop"]'); await page.click('[data-act="stop-no"]');
    check(!(await page.isVisible('.confirm')) && await word(page) === 'Running', '"Keep running" cancels the stop');
    await page.click('[data-act="stop"]'); await page.click('[data-act="stop-yes"]');
    await page.waitForTimeout(100);
    check(await word(page) === 'Stopped', `Stop and restore -> "${await word(page)}"`);
    check(errors.length === 0, `no page errors (${errors.join('; ')})`);
    await ctx.close();
  }
  {
    const { ctx, page } = await newPage(`${srv.url}?fixture=running&freeze=1`);
    await page.click('.chip[data-f="problems"]');
    check(new URL(page.url()).searchParams.get('filter') === 'problems', `filter goes into the URL (${page.url().split('?')[1]})`);
    const rows = await page.$$eval('#list .row[data-i]', r => r.length);
    check(rows >= 1 && rows <= 4, `"Retries and problems" lists only those (${rows} rows)`);
    await page.reload({ waitUntil: 'networkidle' });
    check(await page.getAttribute('.chip[data-f="problems"]', 'aria-pressed') === 'true', 'a shared ?filter= link opens with that filter on');
    await page.click('.chip[data-f="all"]');
    check(!new URL(page.url()).searchParams.has('filter'), '"All steps" removes the filter from the URL');
    // tooltip quadrant flip
    const box = await page.locator('#chart-wrap').boundingBox();
    const q = async (fx, fy) => { await page.mouse.move(box.x + box.width * fx, box.y + box.height * fy); await page.waitForTimeout(50); return page.evaluate(() => { const t = document.getElementById('tip'); const r = t.getBoundingClientRect(); return { q: t.dataset.q, on: t.classList.contains('on'), left: r.left, right: r.right, top: r.top, bottom: r.bottom }; }); };
    const left = await q(0.15, 0.6), right = await q(0.8, 0.6);
    const cursorRight = box.x + box.width * 0.8;
    check(left.on && left.q?.endsWith('l'), `tooltip on the left half sits to the right of the point (${left.q})`);
    check(right.on && right.q?.endsWith('r') && right.right <= cursorRight + 1, `tooltip on the right half flips to the left of the point (${right.q})`);
    check(right.right <= box.x + box.width + 1 && right.left >= box.x - 1, 'the tooltip stays inside the chart');
    // keys and palette
    await page.mouse.move(0, 0);
    await page.keyboard.press('f');
    const afterF = await page.textContent('#detail h3');
    check(/^Step \d+/.test(afterF) && await page.isVisible('[data-act="live"]'), `f jumps to the last problem (${afterF})`);
    await page.keyboard.press('j');
    check(!(await page.isVisible('[data-act="live"]')), 'j goes back to live');
    await page.keyboard.press('Control+k'); await page.keyboard.type('12'); await page.keyboard.press('Enter');
    check(/^Step 12 /.test(await page.textContent('#detail h3')), `palette "12" opens step 12 (${await page.textContent('#detail h3')})`);
    await ctx.close();
  }
} finally { await srv.close(); }

// ---------------- live mode against the mock bridged
for (const control of ['unwired', 'refused', 'accept']) {
  const mock = await mockBridged({ fixture: 'running', control: control === 'refused' ? 'accept' : control });
  try {
    const { ctx, page, errors } = await newPage(mock.url);
    if (control === 'refused') await page.evaluate(() => document.querySelector('meta[name="agentroid-token"]').remove());
    check(await word(page) === 'Running', `live (${control}): loads as Running from GET /jobs/:id`);
    check(!(await page.isVisible('#fix')), `live (${control}): no FIXTURE picker`);
    check((await page.textContent('#source-label')).includes('Live'), `live (${control}): footer says live`);
    if (control === 'unwired') {
      const before = await page.evaluate(() => document.getElementById('raw').textContent);
      mock.emit({ kind: 'progress', step_id: 's128', msg: 'streamed-by-the-test' });
      await page.click('#raw-toggle');
      await page.waitForFunction(() => document.getElementById('raw').textContent.includes('streamed-by-the-test'), null, { timeout: 3000 }).catch(() => {});
      check((await page.textContent('#raw')).includes('streamed-by-the-test'), 'live: an SSE event reaches the page (raw view)');
      const img = await page.evaluate(() => { const i = document.querySelector('#screen img.shot'); return i ? { w: i.naturalWidth, src: i.getAttribute('src') } : null; });
      check(img && img.w > 0 && img.src.startsWith('/jobs/7f3a/frame.jpg?ts='), `live: the frame loads from frame.jpg?ts= (${img?.src?.slice(0, 40)})`);
      check(mock.log.some(l => l.startsWith('GET /jobs/7f3a/events')) && mock.log.some(l => l === 'GET /jobs'), 'live: events backlog and the job list were read');
    }
    await page.click('[data-act="pause"]');
    if (control === 'accept') {
      await page.waitForFunction(() => document.getElementById('state-word').textContent === 'Paused by you', null, { timeout: 8000 }).catch(() => {});
      check(await word(page) === 'Paused by you', `live (accept): 202, then the job shows "${await word(page)}"`);
    } else {
      await page.waitForSelector('.controls .note.err', { timeout: 5000 }).catch(() => {});
      const msg = (await page.textContent('#controls .note').catch(() => '')) ?? '';
      check(control === 'unwired' ? /not wired/.test(msg) : /Refused/.test(msg), `live (${control}): the failure is said in words: "${msg.trim()}"`);
      check(await page.isEnabled('[data-act="pause"]'), `live (${control}): controls are usable again after the failure`);
      check(await word(page) === 'Running', `live (${control}): state unchanged`);
    }
    check(errors.length === 0, `live (${control}): no page errors (${errors.join('; ')})`);
    await ctx.close();
  } finally { await mock.close(); }
}
await L.browser.close();
const dir = CHECKS;
writeFileSync(new URL('interact.txt', dir), lines.join('\n') + `\n${fails} failed\n`);
console.log(`${fails} failed`);
process.exit(fails ? 1 : 0);
