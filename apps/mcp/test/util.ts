import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { DaemonClient } from "../src/daemon.ts";
import { createServer } from "../src/server-core.ts";
import type { Server, ToolResult } from "../src/server-core.ts";
import type { Profile } from "../src/tools.ts";
import { FakeDaemon } from "./fake-daemon.ts";
import type { FakeDevice } from "./fake-daemon.ts";

export const here = dirname(fileURLToPath(import.meta.url));
export const mcpRoot = join(here, "..");

/** The repo's schemas/ folder: at the repo root, or under scaffold/ in a source checkout. */
export function schemasDir(): string {
  for (const rel of ["../../../schemas", "../../../scaffold/schemas"]) {
    const p = join(here, rel);
    if (existsSync(join(p, "receipt.json"))) return p;
  }
  throw new Error("schemas/ not found");
}

export const readJson = (p: string): unknown => JSON.parse(readFileSync(p, "utf8"));

export function fixtureFiles(kind: string): { name: string; value: unknown }[] {
  const dir = join(schemasDir(), "fixtures", kind);
  return readdirSync(dir).filter((f) => f.endsWith(".json")).sort().map((f) => ({ name: f, value: readJson(join(dir, f)) }));
}

export interface Rig {
  fake: FakeDaemon;
  client: DaemonClient;
  server: Server;
  call(name: string, args?: unknown): Promise<ToolResult>;
  stop(): Promise<void>;
}

/** A real DaemonClient talking over a real pipe to the in-process fake. */
export async function rig(devices?: FakeDevice[], opts: { profile?: Profile; session?: string } = {}): Promise<Rig> {
  const fake = new FakeDaemon(devices);
  await fake.start();
  const client = new DaemonClient({ path: fake.path, session: opts.session ?? "test-session", defaultTimeoutMs: 5000 });
  const server = createServer({ daemon: client, session: opts.session ?? "test-session", ...(opts.profile ? { profile: opts.profile } : {}) });
  return {
    fake,
    client,
    server,
    call: (name, args) => server.callTool(name, args),
    stop: async () => {
      server.close();
      client.close();
      await fake.stop();
    },
  };
}
