// Renders the two banner SVGs to PNG with Playwright's bundled Chromium (headless, no window).
//
//   node docs/media/src/render_banner.mjs
//
// banner-1280x640.svg -> assets/banner-1280x640.png           (GitHub social preview)
// banner-1200x627.svg -> assets/banner-linkedin-1200x627.png  (LinkedIn project media)
// Rendered at device scale 1, so each PNG is exactly the size in its name.

import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..", "..", "..");
const out = join(root, "assets");
mkdirSync(out, { recursive: true });

const jobs = [
  ["banner-1280x640.svg", "banner-1280x640.png", 1280, 640],
  ["banner-1200x627.svg", "banner-linkedin-1200x627.png", 1200, 627],
];

const browser = await chromium.launch();
try {
  for (const [src, dst, width, height] of jobs) {
    const page = await browser.newPage({ viewport: { width, height }, deviceScaleFactor: 1 });
    await page.goto(pathToFileURL(join(here, src)).href);
    await page.screenshot({ path: join(out, dst), clip: { x: 0, y: 0, width, height } });
    await page.close();
    console.log(`${dst} ${width}x${height}`);
  }
} finally {
  await browser.close();
}
