// A stand-in for bridged's HTTP side, exactly as WIRING.md specifies it, for testing live mode before
// 2.11a lands. Binds 127.0.0.1 on an ephemeral port only. Serves dist/ (with the per-start token
// injected into index.html, as bridged is asked to do) and the job endpoints from one generated fixture.
//   control: 'unwired' -> 501, 'accept' -> 202 and the job moves state, anything else -> that status
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { CSP } from './serve.mjs';

const TYPES = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.woff2': 'font/woff2', '.jpg': 'image/jpeg' };

export async function mockBridged({ fixture = 'running', control = 'unwired', token = 'test-token-0123456789' } = {}) {
  const dist = resolve(fileURLToPath(new URL('../dist', import.meta.url)));
  const fx = join(dist, 'fixtures', fixture);
  const job = JSON.parse(await readFile(join(fx, 'job.json'), 'utf8'));
  const jobs = JSON.parse(await readFile(join(fx, 'jobs.json'), 'utf8'));
  const lines = (await readFile(join(fx, 'events.ndjson'), 'utf8')).split('\n').filter(Boolean);
  const id = job.job_id;
  const log = [];
  const streams = new Set();
  const push = (obj) => { const line = JSON.stringify(obj); lines.push(line); for (const res of streams) res.write(`id: ${obj.seq}\ndata: ${line}\n\n`); };
  const nextSeq = () => JSON.parse(lines.at(-1)).seq + 1;
  const hdr = (type, extra = {}) => ({ 'Content-Type': type, 'Content-Security-Policy': CSP, 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', Date: new Date().toUTCString(), ...extra });

  const server = createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    const p = url.pathname;
    log.push(`${req.method} ${p}${url.search}`);
    // bind rule: only loopback clients (bridged also allows its tailnet address in hub mode, never the LAN)
    if (!['127.0.0.1', '::ffff:127.0.0.1', '::1'].includes(req.socket.remoteAddress)) { res.writeHead(403).end(); return; }
    try {
      if (p === '/jobs' && req.method === 'GET') { res.writeHead(200, hdr('application/json')).end(JSON.stringify(jobs)); return; }
      if (p === `/jobs/${id}` && req.method === 'GET') { res.writeHead(200, hdr('application/json')).end(JSON.stringify(job)); return; }
      if (p === `/jobs/${id}/events` && req.method === 'GET') {
        const after = Number(req.headers['last-event-id'] ?? url.searchParams.get('after') ?? -1);
        const backlog = lines.filter(l => JSON.parse(l).seq > after);
        if ((req.headers.accept ?? '').includes('text/event-stream')) {
          res.writeHead(200, hdr('text/event-stream'));
          for (const l of backlog) res.write(`id: ${JSON.parse(l).seq}\ndata: ${l}\n\n`);
          streams.add(res); req.on('close', () => streams.delete(res));
          return;
        }
        res.writeHead(200, hdr('application/x-ndjson')).end(backlog.join('\n') + (backlog.length ? '\n' : '')); return;
      }
      if (p === `/jobs/${id}/frame.json`) { const b = await readFile(join(fx, 'frame.json')).catch(() => null); if (!b) { res.writeHead(404, hdr('text/plain')).end(); return; } res.writeHead(200, hdr('application/json')).end(b); return; }
      if (p === `/jobs/${id}/frame.jpg`) { res.writeHead(200, hdr('image/jpeg')).end(await readFile(join(fx, 'frame.jpg'))); return; }
      if (p === `/jobs/${id}/recorder.json`) { const b = await readFile(join(fx, 'recorder.json')).catch(() => null); if (!b) { res.writeHead(404, hdr('text/plain')).end(); return; } res.writeHead(200, hdr('application/json')).end(b); return; }
      if (p === `/jobs/${id}/recorder/crop.jpg`) { res.writeHead(200, hdr('image/jpeg')).end(await readFile(join(fx, 'crop.jpg'))); return; }
      if (p === `/jobs/${id}/control` && req.method === 'POST') {
        let body = ''; for await (const c of req) body += c;
        const host = req.headers.host, origin = req.headers.origin;
        if (req.headers['x-agentroid-token'] !== token || (origin && origin !== `http://${host}`)) { res.writeHead(403, hdr('application/json')).end('{"error":"refused"}'); return; }
        if (control === 'unwired') { res.writeHead(501, hdr('application/json')).end('{"error":"not implemented"}'); return; }
        if (control !== 'accept') { res.writeHead(Number(control), hdr('application/json')).end('{}'); return; }
        const { action } = JSON.parse(body);
        if (action === 'pause') { job.status = 'paused'; push({ v: 1, ts: new Date().toISOString(), run_id: job.run_id, seq: nextSeq(), kind: 'pause', msg: 'paused: paused_by_user; lease kept' }); }
        if (action === 'resume') { job.status = 'running'; push({ v: 1, ts: new Date().toISOString(), run_id: job.run_id, seq: nextSeq(), kind: 'resume', msg: 'resumed' }); }
        res.writeHead(202, hdr('application/json')).end('{"accepted":true}'); return;
      }
      // static: the built page, with the token bridged would inject
      let f = normalize(join(dist, decodeURIComponent(p === '/' ? '/index.html' : p)));
      if (!f.startsWith(dist)) { res.writeHead(403).end(); return; }
      let b = await readFile(f);
      if (f.endsWith('index.html')) b = Buffer.from(b.toString('utf8').replace('<head>', `<head>\n<meta name="agentroid-token" content="${token}">`));
      res.writeHead(200, hdr(TYPES[extname(f)] ?? 'application/octet-stream')).end(b);
    } catch { if (!res.headersSent) res.writeHead(404, hdr('text/plain')).end('not found'); }
  });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const { port } = server.address();
  return {
    url: `http://127.0.0.1:${port}/`, log, job,
    /** stream one more event to every open SSE connection, as the engine would */
    emit: (o) => push({ v: 1, ts: new Date().toISOString(), run_id: job.run_id, seq: nextSeq(), ...o }),
    close: () => { for (const s of streams) s.end(); return new Promise(r => server.close(r)); },
  };
}
