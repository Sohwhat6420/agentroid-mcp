// Entry point for the Streamable HTTP listener (task 2.6):
//   node --experimental-strip-types src/http-main.ts --config <file.json>
// The config file is JSON: {"host": "<tailscale ip -4>", "port": 8765, ...} (see HttpConfig in
// http-wiring.ts). Tokens come from Windows Credential Manager through the credstore helper:
// AGENTROID_CREDSTORE names the helper binary, else target/release then target/debug of the repo.
// Nothing here prints a token.

import { readFileSync } from "node:fs";
import { DaemonClient } from "./daemon.ts";
import { parseConfig, startHttpServer, stderrLogger } from "./http-wiring.ts";
import { Supervisor, defaultTopicFile, ntfyPost, readTopic } from "./supervise.ts";
import { CredStore, TokenTable, credstorePath } from "./tokens.ts";

async function main(): Promise<void> {
  const i = process.argv.indexOf("--config");
  const file = i >= 0 ? process.argv[i + 1] : process.env.AGENTROID_HTTP_CONFIG;
  if (file === undefined) throw new Error("usage: http-main.ts --config <file.json>");
  const config = parseConfig(JSON.parse(readFileSync(file, "utf8")));
  const tokens = new TokenTable(await new CredStore(credstorePath()).list());
  const daemon = new DaemonClient({ session: `http-${process.pid}`, ...(process.env.AGENTROID_PIPE ? { path: process.env.AGENTROID_PIPE } : {}) });
  // 2.11a: one supervisor for every token, so a job seen through two tokens still pushes once. The
  // HTTP side does not watch jobs in the background (it outlives sessions): a stuck / done state is
  // pushed only when a session's own call shows it.
  const topicFile = defaultTopicFile();
  const supervisor = new Supervisor({
    poster: ntfyPost,
    topic: () => (process.env.AGENTROID_NO_PUSH === "1" ? null : readTopic(topicFile)),
    log: (line) => process.stderr.write(`agentroid: ${line}\n`),
  });
  const running = await startHttpServer({ config, tokens, daemon, log: stderrLogger, supervisor });
  const shutdown = (): void => {
    void running.close().finally(() => {
      daemon.close();
      process.exit(0);
    });
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

main().catch((e: unknown) => {
  process.stderr.write(`agentroid HTTP listener not started: ${e instanceof Error ? e.message : String(e)}\n`);
  process.exit(1);
});
