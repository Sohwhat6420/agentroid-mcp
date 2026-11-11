// Renders each fixture's phone frame (scenes/<name>.json, written by gen-fixtures.mjs) to a real
// JPEG under 60 KB, so fixture mode exercises the same <img> path as bridged's frame.jpg. The
// picture is the design's camera scene (design/v2.html frameSVG) WITHOUT the ghost cursor: the
// ghost is drawn by the page from frame.json, never baked into the image.
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { launch } from './pw.mjs';

const app = join(dirname(fileURLToPath(import.meta.url)), '..');
const SCENES = join(app, 'scenes'), OUT = join(app, 'public', 'fixtures');
const MAX = 61440;
const ISOS = [100, 200, 400, 800, 1600], ISO_X = { 100: 200, 200: 405, 400: 610, 800: 815, 1600: 1020 };
const hm = t => new Date((t + 8 * 3600) * 1000).toISOString().slice(11, 16);
const esc = s => String(s).replace(/[&<>"]/g, ch => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[ch]));

function scene(sc) {
  const expo = sc.iso === 'AUTO' ? .5 : Math.log2((sc.iso / 100) * ((1 / +sc.sh.split('/')[1]) * 500)) / Math.log2(16 * 125);
  const lum = Math.round(28 + Math.max(0, Math.min(1, expo)) * 150);
  const sky = `rgb(${lum + 10},${lum + 14},${lum + 20})`, hill = `rgb(${Math.round(lum * .55)},${Math.round(lum * .6)},${Math.round(lum * .62)})`, sun = `rgb(${Math.min(255, lum + 70)},${Math.min(255, lum + 68)},${Math.min(255, lum + 60)})`;
  const mode = sc.photoMode ? 'photo' : 'pro';
  let s = `<svg viewBox="0 0 1220 2712" width="1220" height="2712" xmlns="http://www.w3.org/2000/svg">`;
  s += `<rect width="1220" height="2712" fill="#141618"/>`;
  s += `<text x="80" y="110" fill="#d9dde2" font-size="46" font-family="system-ui">${hm(sc.t)}</text><rect x="1040" y="78" width="86" height="40" rx="8" fill="none" stroke="#d9dde2" stroke-width="5"/><rect x="1048" y="86" width="${Math.round(60 * sc.batt / 100)}" height="24" rx="3" fill="#d9dde2"/>`;
  s += `<rect y="260" width="1220" height="1620" fill="${sky}"/><circle cx="880" cy="640" r="120" fill="${sun}"/><path d="M0 1500 L300 1180 L560 1420 L820 1100 L1220 1460 L1220 1880 L0 1880Z" fill="${hill}"/>`;
  s += `<g stroke="rgba(255,255,255,.28)" stroke-width="3"><line x1="407" y1="260" x2="407" y2="1880"/><line x1="813" y1="260" x2="813" y2="1880"/><line x1="0" y1="800" x2="1220" y2="800"/><line x1="0" y1="1340" x2="1220" y2="1340"/></g>`;
  if (mode === 'pro') {
    if (sc.dial) { s += `<rect x="60" y="1730" width="1100" height="170" rx="24" fill="rgba(20,22,24,.86)"/>`; for (const v of ISOS) s += `<text x="${ISO_X[v]}" y="1838" text-anchor="middle" fill="#e8ebee" font-size="54" font-family="ui-monospace,monospace">${v}</text>`; }
    const chips = [['ISO ' + sc.iso, 150, 370], ['S ' + sc.sh, 400, 640], ['WB 5200', 670, 900], ['MF', 930, 1080]];
    chips.forEach(([t, a, b], ci) => { s += `<rect x="${a}" y="1955" width="${b - a}" height="96" rx="20" fill="#2a2e33" ${ci === 0 && sc.dial ? 'stroke="#f1c75b" stroke-width="6"' : ''}/><text x="${(a + b) / 2}" y="2018" text-anchor="middle" fill="#eef0f2" font-size="44" font-family="ui-monospace,monospace">${esc(t)}</text>`; });
    for (let k = 0; k < 25; k++) s += `<line x1="${170 + k * 36}" x2="${170 + k * 36}" y1="2130" y2="${k % 4 ? 2165 : 2185}" stroke="#9aa1a8" stroke-width="5"/>`;
  } else s += `<rect x="470" y="1960" width="280" height="90" rx="45" fill="#2a2e33"/><text x="610" y="2020" text-anchor="middle" fill="#eef0f2" font-size="44" font-family="system-ui">1x  2x</text>`;
  s += `<g font-size="44" font-family="system-ui" text-anchor="middle"><text x="390" y="2330" fill="#aab0b6">VIDEO</text><text x="610" y="2330" fill="${mode === 'photo' ? '#f1c75b' : '#aab0b6'}">PHOTO</text><text x="830" y="2330" fill="${mode === 'pro' ? '#f1c75b' : '#aab0b6'}">PRO</text></g>`;
  s += `<circle cx="610" cy="2500" r="112" fill="none" stroke="#eef0f2" stroke-width="12"/><circle cx="610" cy="2500" r="88" fill="#eef0f2"/>`;
  if (sc.popup) {
    s += `<rect width="1220" height="2712" fill="rgba(0,0,0,.55)"/><rect x="90" y="980" width="1040" height="720" rx="56" fill="#2b2f34"/>`;
    s += `<g font-family="system-ui" fill="#eef0f2" text-anchor="middle"><text x="610" y="1120" font-size="50">Allow Camera to access this</text><text x="610" y="1190" font-size="50">device's location?</text>`;
    s += `<text x="610" y="1380" font-size="44" fill="#a8c7fa">While using the app</text><text x="610" y="1490" font-size="44" fill="#a8c7fa">Only this time</text><text x="610" y="1600" font-size="44" fill="#a8c7fa">Don't allow</text></g>`;
  }
  return s + '</svg>';
}

const pw = await launch();
if (!pw) { console.log('UNMEASURED: no local playwright-core with an installed Chromium; frames not rendered'); process.exit(2); }
const { browser } = pw;
console.log(`playwright-core ${pw.version} from ${pw.dir}`);
try {
  const page = await browser.newPage({ viewport: { width: 1220, height: 2712 }, deviceScaleFactor: 0.5 });
  await page.route('**/*', r => r.request().url().startsWith('data:') || r.request().url() === 'about:blank' ? r.continue() : r.abort());
  for (const f of readdirSync(SCENES).filter(f => f.endsWith('.json')).sort()) {
    const sc = JSON.parse(readFileSync(join(SCENES, f), 'utf8'));
    await page.setContent(`<!doctype html><html><body style="margin:0;background:#000">${scene(sc)}</body></html>`);
    const clip = sc.kind === 'crop' ? { x: sc.crop.x, y: sc.crop.y, width: sc.crop.w, height: sc.crop.h } : { x: 0, y: 0, width: 1220, height: 2712 };
    let q = 70, buf;
    do { buf = await page.screenshot({ type: 'jpeg', quality: q, clip }); q -= 10; } while (buf.length > MAX && q > 10);
    if (buf.length > MAX) throw new Error(`${f}: still ${buf.length} bytes at quality 10`);
    const dir = join(OUT, sc.name);
    if (sc.kind === 'crop') {
      writeFileSync(join(dir, 'crop.jpg'), buf);
      const meta = JSON.parse(readFileSync(join(dir, 'recorder.json'), 'utf8')); meta.crop.bytes = buf.length;
      writeFileSync(join(dir, 'recorder.json'), JSON.stringify(meta, null, 1) + '\n');
    } else {
      writeFileSync(join(dir, 'frame.jpg'), buf);
      const meta = JSON.parse(readFileSync(join(dir, 'frame.json'), 'utf8')); meta.bytes = buf.length;
      writeFileSync(join(dir, 'frame.json'), JSON.stringify(meta, null, 1) + '\n');
    }
    console.log(`${f.padEnd(22)} ${buf.length} bytes (quality ${q + 10})`);
  }
} finally { await browser.close(); }
