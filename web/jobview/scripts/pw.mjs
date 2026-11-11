// Finds an ALREADY INSTALLED playwright-core on this machine and launches its bundled headless
// Chromium. It never downloads a browser or a package, never uses the installed Chrome or Edge,
// never touches a real browser profile, and never shows a window (headless; Playwright spawns its
// children with windowsHide: true).
//
// Set JOBVIEW_PLAYWRIGHT_CORE to a playwright-core folder to choose one; otherwise known local
// copies are tried. If none is usable, launch() returns null and callers record "unmeasured".
import { createRequire } from 'node:module';
import { existsSync } from 'node:fs';
import { join } from 'node:path';

const CANDIDATES = [
  process.env.JOBVIEW_PLAYWRIGHT_CORE,
  join(process.cwd(), 'node_modules', 'playwright-core'),
].filter(Boolean);

export function findPlaywright() {
  for (const dir of CANDIDATES) {
    if (!existsSync(join(dir, 'package.json'))) continue;
    try {
      const req = createRequire(join(dir, 'package.json'));
      const pw = req(dir);
      const exe = pw.chromium.executablePath();
      // headless shell sits next to the full build; prefer it (no window can exist)
      if (exe && existsSync(exe)) return { pw, dir, version: req(join(dir, 'package.json')).version, exe };
    } catch { /* try the next one */ }
  }
  return null;
}

export async function launch() {
  const found = findPlaywright();
  if (!found) return null;
  const browser = await found.pw.chromium.launch({
    headless: true,
    // no extensions, no first-run, no network beyond 127.0.0.1 (pages are routed by the scripts)
    args: ['--disable-extensions', '--no-first-run', '--disable-background-networking', '--disable-component-update'],
  });
  return { browser, ...found };
}
