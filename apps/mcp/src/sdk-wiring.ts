// The ONLY file that imports @modelcontextprotocol/sdk. It adapts server-core to the SDK over stdio.
// Written against the SDK's public API: low-level `Server`, `StdioServerTransport`,
// `ListToolsRequestSchema`, `CallToolRequestSchema`. Needs an SDK version with tool outputSchema
// and structuredContent (the 1.x releases from mid-2025 on).
// stdout belongs to the protocol: anything for humans goes to stderr.

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { DaemonClient } from "./daemon.ts";
import { installHandlers } from "./sdk-handlers.ts";
import { createServer } from "./server-core.ts";
import { Supervisor, defaultTopicFile, ntfyPost, readTopic } from "./supervise.ts";

const profile = process.env.AGENTROID_PROFILE === "minimal" ? "minimal" : "full";
const session = process.env.AGENTROID_SESSION ?? `claude-${process.pid}`;
const daemon = new DaemonClient({ session, ...(process.env.AGENTROID_PIPE ? { path: process.env.AGENTROID_PIPE } : {}) });
// 2.11a: this process lives exactly as long as the Claude Code session, so it watches the jobs it
// starts and pushes stuck / done (by design: only while a session is open). AGENTROID_NO_PUSH=1
// turns pushes off; the topic file is read at push time, so creating it later works too.
const topicFile = defaultTopicFile();
const supervisor = new Supervisor({
  poster: ntfyPost,
  topic: () => (process.env.AGENTROID_NO_PUSH === "1" ? null : readTopic(topicFile)),
  log: (line) => console.error(`agentroid: ${line}`),
});
const core = createServer({ daemon, profile, session, supervisor, watch: true });

const server = new Server({ name: "agentroid", version: "0.1.0" }, { capabilities: { tools: {} }, instructions: core.instructions });

installHandlers(server, core);

const shutdown = (): void => {
  core.close();
  daemon.close(); // drops the pipe, so bridged frees this session's leases
  process.exit(0);
};
process.stdin.on("end", shutdown);
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

await server.connect(new StdioServerTransport());
console.error(`agentroid MCP server ready (profile ${profile}, session ${session})`);
