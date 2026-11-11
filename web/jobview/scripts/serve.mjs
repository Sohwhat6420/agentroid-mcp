// A tiny static server for the built dist/, bound to 127.0.0.1 on an ephemeral port. Used only by the
// headless checks (budget, shots, a11y, axe). It sends the same CSP bridged is asked to send (WIRING.md).
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname, join, normalize, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const CSP = "default-src 'self'; script-src 'self'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self'; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
const TYPES = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.ndjson': 'application/x-ndjson', '.woff2': 'font/woff2', '.jpg': 'image/jpeg', '.svg': 'image/svg+xml', '.txt': 'text/plain; charset=utf-8' };

export async function serve(root = fileURLToPath(new URL('../dist', import.meta.url)), { delayMs = 0 } = {}) {
  const base = resolve(root);
  const requests = [];
  const server = createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    requests.push(url.pathname);
    let p = normalize(join(base, decodeURIComponent(url.pathname)));
    if (!p.startsWith(base)) { res.writeHead(403).end(); return; }
    try { if ((await stat(p)).isDirectory()) p = join(p, 'index.html'); } catch { /* 404 below */ }
    try {
      const body = await readFile(p);
      if (delayMs) await new Promise(r => setTimeout(r, delayMs));
      res.writeHead(200, { 'Content-Type': TYPES[extname(p)] ?? 'application/octet-stream', 'Content-Security-Policy': CSP, 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', Date: new Date().toUTCString() });
      res.end(body);
    } catch { res.writeHead(404, { 'Content-Type': 'text/plain' }).end('not found'); }
  });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const { port } = server.address();
  return { url: `http://127.0.0.1:${port}/`, requests, close: () => new Promise(r => server.close(r)) };
}

if (import.meta.url === `file:///${process.argv[1]?.replace(/\\/g, '/')}`) {
  const s = await serve();
  console.log(s.url);
}
