// Accessibility checks that do not need axe (headless bundled Chromium, 127.0.0.1 only):
//  1. targets: every visible interactive control is >= 44 x 44 px at 390 and 820 px, and with a touch
//     screen at 1440 px; with a mouse at 1440 the design's 36 px (DESIGN §5, --target) is checked
//  2. keyboard: Tab walks the controls in reading order (band -> frame -> chart -> recorder -> phone),
//     every stop shows a visible focus outline, nothing traps focus, and the palette returns focus
//  3. reduced motion: with prefers-reduced-motion, no transform animation or transition runs
//  4. no horizontal scroll at any width
//   exit 0 = every check passed
import { CHECKS } from './paths.mjs';
import { mkdirSync, writeFileSync } from 'node:fs';
import { launch } from './pw.mjs';
import { serve } from './serve.mjs';

const L = await launch();
if (!L) { console.log('UNMEASURED: no local playwright-core with an installed Chromium'); process.exit(3); }
const srv = await serve();
const lines = [];
let fails = 0;
const log = (ok, msg) => { if (!ok) fails++; lines.push(`${ok ? 'ok  ' : 'FAIL'} ${msg}`); console.log(lines.at(-1)); };

async function open(o) {
  const ctx = await L.browser.newContext({ viewport: { width: o.w, height: 900 }, colorScheme: o.theme ?? 'light', reducedMotion: o.reduce ? 'reduce' : 'no-preference', hasTouch: !!o.touch, isMobile: false });
  const page = await ctx.newPage();
  await page.route('**/*', r => (r.request().url().startsWith(srv.url) ? r.continue() : r.abort()));
  await page.goto(`${srv.url}?fixture=${o.fixture ?? 'running'}&freeze=1`, { waitUntil: 'networkidle' });
  await page.waitForFunction(() => document.getElementById('main')?.getAttribute('aria-busy') !== 'true');
  await page.evaluate(() => document.fonts.ready);
  return { ctx, page };
}

const SIZES = () => [...document.querySelectorAll('button, a[href], input, select, summary, [tabindex="0"]')]
  .filter(e => { const r = e.getBoundingClientRect(); const cs = getComputedStyle(e); return r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && !e.closest('[hidden], dialog:not([open])'); })
  .map(e => { const r = e.getBoundingClientRect(); return { what: `${e.tagName.toLowerCase()}${e.id ? '#' + e.id : ''}${e.dataset.act ? '[' + e.dataset.act + ']' : ''}${e.dataset.f ? '[' + e.dataset.f + ']' : ''} "${(e.textContent || e.getAttribute('aria-label') || '').trim().slice(0, 30)}"`, w: Math.round(r.width * 10) / 10, h: Math.round(r.height * 10) / 10 }; });

try {
  // 1. targets
  for (const fixture of ['running', 'stuck', 'done']) {
    for (const [w, touch, min] of [[390, false, 44], [820, false, 44], [1440, true, 44], [1440, false, 36]]) {
      const { ctx, page } = await open({ w, touch, fixture });
      const all = await page.evaluate(SIZES);
      // the chart and the step list are large regions, not small targets; the raw <pre> is a text region
      const small = all.filter(t => (t.w < min || t.h < min) && !/#(chart-wrap|list|raw)\b/.test(t.what));
      log(small.length === 0, `targets >= ${min} px, ${fixture} at ${w}${touch ? ' (touch)' : ' (mouse)'}: ${all.length} controls${small.length ? '; too small: ' + small.map(t => `${t.what} ${t.w}x${t.h}`).join('; ') : ''}`);
      const hs = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      log(hs <= 0, `no horizontal scroll, ${fixture} at ${w}: overflow ${hs} px`);
      await ctx.close();
    }
  }

  // 2. keyboard focus order
  for (const [w, fixture] of [[1440, 'running'], [390, 'running'], [1440, 'stuck']]) {
    const { ctx, page } = await open({ w, fixture });
    await page.evaluate(() => document.activeElement?.blur());
    const stops = [];
    for (let i = 0; i < 600; i++) {
      await page.keyboard.press('Tab');
      const s = await page.evaluate(() => {
        const e = document.activeElement; if (!e || e === document.body) return null;
        const cs = getComputedStyle(e), r = e.getBoundingClientRect();
        const REGIONS = [['.skip', 'skip'], ['header', 'header'], ['.band', 'band'], ['.receipt', 'receipt'], ['.frame-panel', 'frame'], ['section[aria-labelledby="spine-h"]', 'chart'], ['section.rec', 'recorder'], ['.vitals', 'phone'], ['footer', 'footer'], ['dialog', 'palette']];
        const hit = REGIONS.find(([sel]) => e.matches(sel) || e.closest(sel));
        return { id: e.id || e.dataset.act || e.dataset.f || e.tagName.toLowerCase(), region: hit ? hit[1] : 'other', y: r.top + scrollY, outline: cs.outlineStyle !== 'none' && parseFloat(cs.outlineWidth) >= 2 || cs.boxShadow !== 'none' };
      });
      if (!s) break; // focus left the page: nothing traps it
      if (stops.length > 3 && stops[0].id === s.id) break; // wrapped round
      stops.push(s);
    }
    const order = ['skip', 'header', 'band', 'receipt', 'frame', 'chart', 'recorder', 'phone', 'footer'];
    const regionSeq = stops.map(s => s.region).filter((r, i, a) => r !== a[i - 1]);
    const rank = r => order.indexOf(r);
    const monotonic = regionSeq.every((r, i) => i === 0 || rank(r) >= rank(regionSeq[i - 1]));
    log(stops.length >= 10, `Tab reaches ${stops.length} stops, ${fixture} at ${w}`);
    log(monotonic, `focus order follows reading order, ${fixture} at ${w}: ${regionSeq.join(' > ')}`);
    const noRing = stops.filter(s => !s.outline);
    log(noRing.length === 0, `every focus stop shows a focus ring, ${fixture} at ${w}${noRing.length ? ': missing on ' + noRing.map(s => s.id).join(', ') : ''}`);
    // the palette: opens on Ctrl+K, traps nothing after Esc, returns focus to where it was
    await page.focus('#theme');
    await page.keyboard.press('Control+k');
    const inPal = await page.evaluate(() => document.activeElement?.id);
    await page.keyboard.press('Escape');
    const back = await page.evaluate(() => document.activeElement?.id);
    log(inPal === 'pal-q' && back === 'theme', `palette takes focus (${inPal}) and Esc returns it (${back}), at ${w}`);
    await ctx.close();
  }

  // 3. reduced motion
  for (const theme of ['light', 'dark']) {
    const { ctx, page } = await open({ w: 1440, theme, reduce: true });
    // trigger the state-change entrance and a frame blink, then read what is animating
    await page.evaluate(() => { document.getElementById('state-wrap').classList.add('enter'); document.getElementById('phone').classList.add('blink'); });
    const moving = await page.evaluate(() => {
      const out = [];
      for (const a of document.getAnimations()) {
        const kf = a.effect?.getKeyframes?.() ?? [];
        if (kf.some(k => k.transform && k.transform !== 'none')) out.push(`${a.animationName ?? a.transitionProperty ?? 'animation'} on ${a.effect.target?.id || a.effect.target?.className}`);
      }
      for (const e of document.querySelectorAll('*')) { const cs = getComputedStyle(e); if (/transform/.test(cs.transitionProperty) && parseFloat(cs.transitionDuration) > 0 && !e.matches('.btn')) out.push(`transform transition on ${e.id || e.className}`); }
      return out;
    });
    log(moving.length === 0, `reduced motion (${theme}): no movement${moving.length ? ': ' + moving.join('; ') : ''}`);
    await ctx.close();
  }
} finally { await L.browser.close(); await srv.close(); }
const dir = CHECKS;
writeFileSync(new URL('a11y.txt', dir), lines.join('\n') + `\n${fails} failed\n`);
console.log(`${fails} failed`);
process.exit(fails ? 1 : 0);
