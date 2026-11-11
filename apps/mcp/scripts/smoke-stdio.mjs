// Smoke test: start the real SDK server over stdio, send initialize + tools/list, print what comes back.
// No daemon is needed for tools/list. Run from apps/mcp: node scripts/smoke-stdio.mjs
import { spawn } from 'node:child_process';

const child = spawn(process.execPath, ['--experimental-strip-types', '--no-warnings', 'src/sdk-wiring.ts'], {
  stdio: ['pipe', 'pipe', 'pipe'],
  windowsHide: true,
});
let out = '';
let err = '';
child.stdout.on('data', (d) => { out += d; });
child.stderr.on('data', (d) => { err += d; });
const send = (m) => child.stdin.write(JSON.stringify(m) + '\n');
send({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'smoke', version: '0' } } });
setTimeout(() => {
  send({ jsonrpc: '2.0', method: 'notifications/initialized' });
  send({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} });
}, 800);
setTimeout(() => {
  child.kill();
  const lines = out.split('\n').filter(Boolean).map((l) => JSON.parse(l));
  const init = lines.find((l) => l.id === 1);
  const list = lines.find((l) => l.id === 2);
  console.log('server:', JSON.stringify(init?.result?.serverInfo), 'instructions chars:', init?.result?.instructions?.length);
  const tools = list?.result?.tools ?? [];
  console.log('tools:', tools.length, tools.map((t) => t.name).join(','));
  console.log('tools/list bytes:', JSON.stringify(tools).length);
  if (err.trim()) console.log('stderr:', err.trim().slice(0, 600));
  process.exit(init && tools.length > 0 ? 0 : 1);
}, 3000);
