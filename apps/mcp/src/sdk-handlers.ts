// The MCP request handlers, shared by the stdio side (sdk-wiring.ts) and the HTTP side
// (http-wiring.ts) so the two cannot drift (task 2.11a). tools/list and tools/call go to
// server-core; when the client put a progressToken in the call's _meta, server-core's progress
// heartbeat becomes notifications/progress on the same request.

import type { Server as SdkServer } from "@modelcontextprotocol/sdk/server/index.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import type { Progress, Server as Core } from "./server-core.ts";

export function installHandlers(sdk: SdkServer, core: Core): void {
  sdk.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: core.listTools() }));
  // core.callTool never throws; every failure is already an isError result with a header and a receipt
  sdk.setRequestHandler(CallToolRequestSchema, async (req, extra) => {
    const token = req.params._meta?.progressToken;
    const progress =
      token === undefined
        ? undefined
        : (p: Progress): void => {
            extra.sendNotification({ method: "notifications/progress", params: { progressToken: token, progress: p.progress, message: p.message } }).catch(() => {});
          };
    const r = await core.callTool(req.params.name, req.params.arguments, progress === undefined ? {} : { progress });
    return { content: r.content, structuredContent: r.structuredContent as unknown as Record<string, unknown>, ...(r.isError ? { isError: true } : {}) };
  });
}

/** Does this JSON-RPC body (one message or a batch) hold a tools/call with a progressToken? */
export function wantsProgress(body: unknown): boolean {
  const one = (m: unknown): boolean => {
    if (m === null || typeof m !== "object") return false;
    const msg = m as { method?: unknown; params?: { _meta?: { progressToken?: unknown } } };
    return msg.method === "tools/call" && msg.params?._meta?.progressToken !== undefined;
  };
  return Array.isArray(body) ? body.some(one) : one(body);
}
